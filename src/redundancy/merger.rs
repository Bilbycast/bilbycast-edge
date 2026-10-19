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
//!    has rolled out of the window, or the stream has moved: drop it,
//!    unless the stream has visibly moved (see "Re-anchoring" below).
//!
//! ## Re-anchoring
//!
//! A forward jump of a whole window or more re-anchors the window on the
//! new seq at once. A jump of half the sequence space or more reads as
//! "too far behind" instead, as does a source restarting on an ISN behind
//! the old one, so without more the merger would discard the new stream
//! until its seqs wrapped back into the window: up to 32 767 packets, about
//! 7 s at 100 Mbps after a 3.5 s outage both legs lost, 34 s at 10 Mbps
//! after an encoder restart. A too-far-behind packet re-anchors the window
//! on itself when either
//!
//! - nothing has been accepted for its stale time
//!   ([`HitlessMerger::with_stale_after_us`], [`DEFAULT_STALE_AFTER_US`] by
//!   default; the caller's arrival clock), or
//! - the too-far-behind packet just before it had the same seq and came
//!   from the other leg, with nothing accepted in between: both legs carry
//!   the stream it belongs to.
//!
//! Neither can follow a leg that is only lagging. A packet behind the
//! window on a leg that lags the other by `d` arrives at most `d` after the
//! newest seq the merger holds first arrived, which reset the clock, so it
//! re-anchors nothing while the stale time exceeds the legs' path
//! differential. Two copies of one old seq, one from each leg and back to
//! back, need the leg the stream is on to repeat a seq it delivered long
//! ago at the moment the lagging leg delivers it. A leg lagging by less
//! than the window never sends a packet behind it at all.
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
//! window is still well above the inter-leg jitter floor. That is the
//! default: any multiple of 64 up to 16384 is accepted, which is how ST
//! 2110-20 ingest widens it and how a PID-bus Hitless slot's
//! `reorder_window` sets it.

/// Identifies which network leg a packet arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveLeg {
    Leg1,
    Leg2,
    None,
}

/// Default number of past sequence numbers to track in the gap-fill
/// bitmap. Sized for TS-class packet rates (≈0.4 s at 2400 pps — see
/// the module doc). High-rate consumers (ST 2110-20 uncompressed video
/// at 146–580 kpps per leg, where 1024 slots is only 1.8–7 ms of
/// wallclock — less than 2022-7 Class A's 10 ms path-differential
/// budget) construct via [`HitlessMerger::with_window`] instead.
pub const REORDER_WINDOW: u16 = 1024;

/// How long a merger accepts nothing before a packet it would drop as too
/// far behind re-anchors its window instead ([`HitlessMerger::with_stale_after_us`]),
/// in microseconds of the caller's arrival clock. Above any path
/// differential a transport-layer pair is run with, so a leg that is only
/// lagging cannot trigger it (see "Re-anchoring" in the module doc), and
/// short beside the 7–34 s a moved stream was discarded for without it.
pub const DEFAULT_STALE_AFTER_US: u64 = 1_000_000;

/// Hitless merger state. Tracks emitted RTP sequence numbers in a sliding
/// bitmap window so the merger can dedup duplicates *and* fill gaps left
/// by single-leg loss.
pub struct HitlessMerger {
    /// Highest RTP sequence number we have ever observed across both legs
    /// (wrapping u16). Anchor for the sliding bitmap window.
    highest_seq: u16,
    /// The bitmap slot holding `highest_seq`. A seq `n` behind it sits `n`
    /// slots behind this one, wrapping inside the window. Placing seqs
    /// relative to the highest one rather than at `seq % window` is what
    /// lets the window be any multiple of 64: `seq % window` stays
    /// consistent across the u16 wrap only for a window that divides 65536.
    highest_slot: u16,
    /// Bitmap of recently-emitted seq numbers, one bit per window slot. A
    /// `1` bit means "this seq has been emitted at least once and any
    /// further arrival is a duplicate". A `0` bit in the gap behind
    /// `highest_seq` means "this seq has not been emitted yet — if it
    /// arrives, fill the gap".
    ///
    /// Sized once to `window / 64` words. An advance clears the slots it
    /// uncovers rather than shifting.
    emitted: Box<[u64]>,
    /// How many slots, counting back from `highest_seq`, hold a seq of the
    /// stream: the rest have held nothing since the anchor, so a clear bit
    /// there is no loss.
    covered: u16,
    /// How many of the oldest covered slots hold seqs a re-anchor skipped:
    /// the seqs just behind the new anchor, which a lagging leg can still
    /// fill. One that leaves the window unfilled is counted in
    /// [`Self::skipped`], not [`Self::lost`].
    reset_tail: u16,
    /// Seqs that left the window behind `highest_seq` without either leg
    /// delivering them.
    lost: u64,
    /// Seqs a re-anchor jumped over that never reached the window filled.
    skipped: u64,
    /// Times the window re-anchored.
    resets: u64,
    /// Arrival time, on the caller's microsecond clock, of the last packet
    /// accepted.
    last_accept_us: u64,
    /// See [`Self::with_stale_after_us`].
    stale_after_us: u64,
    /// The last packet dropped as too far behind since a packet was
    /// accepted: its seq and its leg.
    last_stale: Option<(u16, ActiveLeg)>,
    /// Which leg last provided the forwarded packet.
    active_leg: ActiveLeg,
    /// Whether we've forwarded at least one packet.
    initialized: bool,
    /// Gap-fill window size in slots: a multiple of 64 in `[64, 16384]`.
    window: u16,
}

impl HitlessMerger {
    /// Create a new merger in the un-initialized state with the default
    /// TS-class window ([`REORDER_WINDOW`]).
    pub fn new() -> Self {
        Self::with_window(REORDER_WINDOW)
    }

    /// Create a merger with an explicit gap-fill window.
    ///
    /// `window` must be a multiple of 64 in `[64, 16384]`: 64 is the
    /// bitmap word granularity, and 16384 preserves a sane band between
    /// "behind the window" and "forward stream-reset" (both bounds are
    /// asserted). Used by ST 2110-20 ingest where 2022-7 Class A allows
    /// 10 ms of inter-leg path differential — at 580 kpps (2160p50)
    /// the 1024-slot default covers only ~1.8 ms — and by a PID-bus
    /// Hitless slot's configured `reorder_window`.
    pub fn with_window(window: u16) -> Self {
        assert!(
            (64..=16384).contains(&window) && window.is_multiple_of(64),
            "HitlessMerger window must be a multiple of 64 in [64, 16384], got {window}"
        );
        Self {
            highest_seq: 0,
            highest_slot: 0,
            emitted: vec![0u64; (window / 64) as usize].into_boxed_slice(),
            covered: 0,
            reset_tail: 0,
            lost: 0,
            skipped: 0,
            resets: 0,
            last_accept_us: 0,
            stale_after_us: DEFAULT_STALE_AFTER_US,
            last_stale: None,
            active_leg: ActiveLeg::None,
            initialized: false,
            window,
        }
    }

    /// Set how long, in microseconds of the arrival clock the caller passes
    /// to [`Self::try_merge`], the merger accepts nothing before a packet it
    /// would drop as too far behind re-anchors the window on itself
    /// ([`DEFAULT_STALE_AFTER_US`] unless set; see "Re-anchoring" in the
    /// module doc). A caller whose legs may lag each other by more than the
    /// default — a buffered merger with a long path differential — sets it
    /// above that differential.
    pub fn with_stale_after_us(mut self, stale_after_us: u64) -> Self {
        self.stale_after_us = stale_after_us;
        self
    }

    /// Seqs that left the gap-fill window without either leg delivering
    /// them, since the merger was created: the packets this pair lost.
    /// A packet that arrives after its seq has left the window is dropped
    /// as too late, so it is counted too. So are the seqs still unfilled in
    /// the window when it re-anchors.
    ///
    /// The seqs a re-anchor jumps over are not: from the sequence numbers
    /// alone a burst both legs lost cannot be told from a source restarting
    /// on a new ISN. [`Self::skipped`] counts them instead, for a caller
    /// that can tell the two apart from elsewhere — an SRT pair from its
    /// legs' own drop counts.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Seqs re-anchors jumped over that no leg filled, since the merger was
    /// created: a burst both legs lost, or nothing at all when the source
    /// restarted on a new ISN. Of a jump of `n` seqs, those that fall more
    /// than a window behind the new anchor are counted at the jump; the
    /// last window's worth sit in the window behind it, where a lagging leg
    /// can still fill them, and are counted as they leave it unfilled. See
    /// [`Self::lost`].
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Re-anchors since the merger was created: forward jumps of a whole
    /// window or more, and a moved stream's too-far-behind packets (see
    /// "Re-anchoring" in the module doc). See [`Self::lost`].
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// The slot `ahead` seqs past `highest_seq` (`ahead < window`).
    #[inline]
    fn slot_ahead(&self, ahead: u16) -> usize {
        self.wrap(self.highest_slot as usize + ahead as usize)
    }

    /// The slot `behind` seqs before `highest_seq` (`behind < window`).
    #[inline]
    fn slot_behind(&self, behind: u16) -> usize {
        self.wrap(self.highest_slot as usize + (self.window - behind) as usize)
    }

    /// Fold a slot index below `2 * window` back into the window.
    #[inline]
    fn wrap(&self, slot: usize) -> usize {
        let window = self.window as usize;
        if slot >= window { slot - window } else { slot }
    }

    #[inline]
    fn is_emitted(&self, slot: usize) -> bool {
        (self.emitted[slot / 64] >> (slot % 64)) & 1 == 1
    }

    #[inline]
    fn set_emitted(&mut self, slot: usize, emitted: bool) {
        let bit = 1u64 << (slot % 64);
        if emitted {
            self.emitted[slot / 64] |= bit;
        } else {
            self.emitted[slot / 64] &= !bit;
        }
    }

    /// Re-anchor the window on `seq`, `forward` seqs past `highest_seq`
    /// (wrapping, `forward >= window`).
    ///
    /// Each covered slot still clear holds a seq neither leg delivered,
    /// which leaves the window now: a seq the stream had is lost, one a
    /// previous re-anchor skipped is skipped. Of the `forward - 1` seqs the
    /// jump passes over, the newest `window - 1` take the slots behind
    /// `seq`, fillable by a lagging leg like any gap; the rest never reach
    /// the window and are skipped now. Counted a bitmap word at a time, on
    /// a jump only, so a stream of jumps costs no more per packet than the
    /// re-anchors it already made.
    fn reanchor(&mut self, seq: u16, forward: u16) {
        let tail = self.reset_tail;
        let unfilled_tail = tail - self.emitted_behind(self.covered - tail, tail);
        let ordinary = self.covered - tail;
        let unfilled = ordinary - self.emitted_behind(0, ordinary);
        self.lost += u64::from(unfilled);
        self.skipped += u64::from(unfilled_tail);
        let jumped = forward - 1;
        let behind = jumped.min(self.window - 1);
        self.skipped += u64::from(jumped - behind);
        self.resets += 1;
        self.anchor(seq);
        self.covered = 1 + behind;
        self.reset_tail = behind;
    }

    /// Emitted seqs among the `count` slots that start `from` slots behind
    /// `highest_seq` and run back from there (`from + count <= window`).
    fn emitted_behind(&self, from: u16, count: u16) -> u16 {
        if count == 0 {
            return 0;
        }
        // The oldest of them, and the slots forward from it.
        let start = self.slot_behind(from + count - 1);
        let end = start + usize::from(count);
        let window = usize::from(self.window);
        let set = if end <= window {
            self.emitted_in(start, end)
        } else {
            self.emitted_in(start, window) + self.emitted_in(0, end - window)
        };
        set as u16
    }

    /// Set bits in slots `from..to` of the bitmap.
    fn emitted_in(&self, from: usize, to: usize) -> u32 {
        let mut count = 0;
        let mut slot = from;
        while slot < to {
            let bit = slot % 64;
            let span = (64 - bit).min(to - slot);
            let mask = if span == 64 { u64::MAX } else { ((1u64 << span) - 1) << bit };
            count += (self.emitted[slot / 64] & mask).count_ones();
            slot += span;
        }
        count
    }

    /// Anchor the window on `seq`, as the only seq seen so far.
    fn anchor(&mut self, seq: u16) {
        self.emitted.fill(0);
        self.highest_seq = seq;
        self.highest_slot = 0;
        self.set_emitted(0, true);
        self.covered = 1;
        self.reset_tail = 0;
    }

    /// Advance `highest_seq` by `forward` (`0 < forward < window`) to
    /// `seq`, emitted. The `forward - 1` seqs skipped are left clear so the
    /// other leg can still fill them. Each slot reused held the seq one
    /// window earlier, which leaves the window now: if no leg delivered
    /// it, it is lost — or skipped, when a re-anchor had jumped over it.
    fn advance(&mut self, seq: u16, forward: u16) {
        // Slots `ahead` of the old highest hold seqs of the stream only once
        // `ahead` reaches past the slots not yet covered; the first
        // `reset_tail` of those hold seqs a re-anchor skipped.
        let uncovered = self.window - self.covered;
        let tail_end = uncovered + self.reset_tail;
        for ahead in 1..=forward {
            let slot = self.slot_ahead(ahead);
            if ahead > uncovered && !self.is_emitted(slot) {
                if ahead <= tail_end {
                    self.skipped += 1;
                } else {
                    self.lost += 1;
                }
            }
            self.set_emitted(slot, ahead == forward);
        }
        self.reset_tail -= forward.saturating_sub(uncovered).min(self.reset_tail);
        self.highest_slot = self.slot_ahead(forward) as u16;
        self.highest_seq = seq;
        self.covered = self.covered.saturating_add(forward).min(self.window);
    }

    /// Record that a packet arriving at `now_us` from `leg` is forwarded.
    #[inline]
    fn accept(&mut self, leg: ActiveLeg, now_us: u64) -> Option<ActiveLeg> {
        self.active_leg = leg;
        self.last_accept_us = now_us;
        self.last_stale = None;
        Some(leg)
    }

    /// Whether a packet too far behind the window, of seq `seq` from `leg`
    /// at `now_us`, belongs to a stream that has moved (see "Re-anchoring"
    /// in the module doc). Remembered for the next one either way.
    fn stream_moved(&mut self, seq: u16, leg: ActiveLeg, now_us: u64) -> bool {
        let quiet = now_us.saturating_sub(self.last_accept_us) >= self.stale_after_us;
        let both_legs = matches!(self.last_stale, Some((s, l)) if s == seq && l != leg);
        self.last_stale = Some((seq, leg));
        quiet || both_legs
    }

    /// Attempt to merge an incoming packet from the given leg, arriving at
    /// `now_us` on any monotonic microsecond clock the caller keeps for
    /// both legs (only differences between its values are read).
    ///
    /// Returns `Some(leg)` if this packet should be forwarded downstream,
    /// or `None` if it's a duplicate / too-old packet that should be
    /// silently dropped.
    pub fn try_merge(&mut self, seq: u16, leg: ActiveLeg, now_us: u64) -> Option<ActiveLeg> {
        if !self.initialized {
            // Anchor on the very first packet. Mark only this seq as
            // emitted; the rest of the bitmap stays zero so that the
            // following ~`window` seqs can be filled in any order by
            // either leg.
            self.anchor(seq);
            self.initialized = true;
            return self.accept(leg, now_us);
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
            // A gap of a whole window or more: a burst both legs lost, or a
            // stream reset (e.g. publisher restarted with a new ISN).
            if forward >= self.window {
                self.reanchor(seq, forward);
            } else {
                self.advance(seq, forward);
            }
            self.accept(leg, now_us)
        } else if backward < self.window {
            // ── Within the gap-fill window ──
            //
            // Either a duplicate (bit already set — drop) or a missing
            // packet that the other leg has just delivered (bit clear —
            // fill the gap and emit).
            let slot = self.slot_behind(backward);
            if self.is_emitted(slot) {
                None
            } else {
                self.set_emitted(slot, true);
                self.accept(leg, now_us)
            }
        } else if self.stream_moved(seq, leg, now_us) {
            // ── Too far behind, but the stream has moved: a jump of half
            //    the sequence space or more, or a new ISN behind the old ──
            self.reanchor(seq, forward);
            self.accept(leg, now_us)
        } else {
            // ── Too far behind: rolled out of the window ──
            None
        }
    }

    /// The slot holding `seq`, when `seq` is at or behind `highest_seq`
    /// inside the window: where a [`HitlessRunMerger`] keeps its state for
    /// the seq beside the emitted bit.
    #[inline]
    fn slot_of(&self, seq: u16) -> Option<usize> {
        let behind = self.highest_seq.wrapping_sub(seq);
        (self.initialized && behind < self.window).then(|| self.slot_behind(behind))
    }
}

/// SMPTE 2022-7 merge where a sequence number names a run of packets rather
/// than one: the TS packets of one PID that one datagram carried, which the
/// PID bus delivers one by one, each with the datagram's seq and its `index`
/// in the run ([`crate::engine::ts_es_bus::UpstreamSeq`]). A video PID has
/// seven to a 1316-byte datagram.
///
/// Both legs of a 2022-7 pair carry identical datagrams, so packet `index` of
/// seq `seq` is the same packet on either leg. A [`HitlessMerger`] over the
/// seqs keeps the window — what is new, what fills a gap, what is too late —
/// and beside each of its slots sits the index of the next packet of that
/// seq to forward. A packet is forwarded when it is that next one, from
/// whichever leg; one already forwarded is a duplicate. One past it is
/// dropped as well: the packets before it in its run have not come, and
/// forwarding it would put the run out of order. The leg that delivers them
/// brings the rest. So a leg that lost the start of a run — its channel
/// lagged, or it subscribed mid-datagram — leaves the run to the other leg
/// from the packet it is missing, and a repeat of a datagram is a duplicate
/// packet by packet. A run neither leg has from its start is dropped from
/// the hole on: unbuffered, nothing can wait for the missing packet. Behind
/// a hold, [`BufferedHitlessMerger::with_runs`] keeps such a run's packets
/// and releases them past the hole once its hold expires ([`HeldRuns`]).
///
/// Each seq's first packet decides the seq as the transport merger decides a
/// datagram, so the window, gap fills, too-late drops and re-anchors work in
/// datagrams as they do there. Cost per packet over
/// [`HitlessMerger::try_merge`]: a slot lookup, a compare and a store, on one
/// `u16` per window slot allocated once.
pub struct HitlessRunMerger {
    seqs: HitlessMerger,
    /// Per window slot, the index of the next packet to forward of the seq
    /// [`Self::seqs`] holds there. Reset when that seq first arrives, and
    /// read only while the slot's emitted bit is set — which only that
    /// arrival sets.
    next: Box<[u16]>,
}

impl HitlessRunMerger {
    /// A run merger whose seqs have a `window`-seq gap-fill window (see
    /// [`HitlessMerger::with_window`]).
    pub fn with_window(window: u16) -> Self {
        Self {
            seqs: HitlessMerger::with_window(window),
            next: vec![0u16; usize::from(window)].into_boxed_slice(),
        }
    }

    /// Merge packet `index` of seq `seq`'s run, from `leg`, arriving at
    /// `now_us` (as for [`HitlessMerger::try_merge`]). Returns `Some(leg)`
    /// to forward it, or `None` to drop it: a duplicate, a packet ahead of
    /// its run, or a seq too late to merge.
    pub fn try_merge(
        &mut self,
        seq: u16,
        index: u16,
        leg: ActiveLeg,
        now_us: u64,
    ) -> Option<ActiveLeg> {
        let first = self.seqs.try_merge(seq, leg, now_us).is_some();
        let slot = self.seqs.slot_of(seq)?;
        if first {
            self.next[slot] = 0;
        }
        if index != self.next[slot] {
            return None;
        }
        self.next[slot] = index.saturating_add(1);
        Some(leg)
    }
}

/// How a [`BufferedHitlessMerger`] tells a new packet from a duplicate.
pub trait SeqDedup {
    /// Whether packet `index` of seq `seq`, from `leg` at `now_us`, may be
    /// held: `Some(leg)` to hold it for release, `None` for a duplicate or
    /// a packet too late.
    fn admit(&mut self, seq: u16, index: u16, leg: ActiveLeg, now_us: u64) -> Option<ActiveLeg>;

    /// Re-anchors so far ([`HitlessMerger::resets`]): the buffered merger
    /// re-projects a seq that re-anchored the window from behind it.
    fn resets(&self) -> u64;
}

/// One packet per seq, so `index` is always 0.
impl SeqDedup for HitlessMerger {
    #[inline]
    fn admit(&mut self, seq: u16, _index: u16, leg: ActiveLeg, now_us: u64) -> Option<ActiveLeg> {
        self.try_merge(seq, leg, now_us)
    }

    fn resets(&self) -> u64 {
        HitlessMerger::resets(self)
    }
}

/// The dedup of a [`BufferedHitlessMerger::with_runs`]: runs of packets per
/// seq, as [`HitlessRunMerger`] merges them, behind a hold.
///
/// The seqs decide the window as there. Within a seq the hold does what the
/// unbuffered merger's next-index rule does: every packet is held under its
/// `(seq, index)`, so a copy of one held is a duplicate and one already
/// released is too late, and packets are released in run order. So a packet
/// past a hole is held rather than dropped — a run whose head neither leg
/// has (both legs lagged into it, the first run after a start or a plan
/// swap, a run cut short with the other leg down) — and released in order
/// past the hole once its hold expires, the missing packet having had the
/// whole hold to arrive.
pub struct HeldRuns(HitlessMerger);

impl SeqDedup for HeldRuns {
    #[inline]
    fn admit(&mut self, seq: u16, _index: u16, leg: ActiveLeg, now_us: u64) -> Option<ActiveLeg> {
        // A seq in the window after this arrival: its first packet just
        // admitted, a later one of its run, or a gap filled.
        self.0.try_merge(seq, leg, now_us);
        self.0.slot_of(seq).map(|_| leg)
    }

    fn resets(&self) -> u64 {
        self.0.resets()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_packet_accepted() {
        let mut merger = HitlessMerger::new();
        let result = merger.try_merge(0, ActiveLeg::Leg1, 0);
        assert_eq!(result, Some(ActiveLeg::Leg1));
    }

    /// A widened window (ST 2110-20 class) gap-fills a slow leg that
    /// lags beyond the 1024-slot TS-class default. With the default
    /// window the same arrival is dropped as too-far-behind — that
    /// contrast is asserted too, so this test pins the behavioural
    /// difference the window parameter exists for.
    #[test]
    fn test_wide_window_gap_fills_beyond_default_window() {
        let lag: u16 = 5000; // > 1024 default, < 16384 wide

        let mut wide = HitlessMerger::with_window(16384);
        assert_eq!(wide.try_merge(1000, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Fast leg races ahead; seq 1001 lost on the fast leg.
        assert_eq!(
            wide.try_merge(1000u16.wrapping_add(lag), ActiveLeg::Leg1, 0),
            Some(ActiveLeg::Leg1)
        );
        // Slow leg delivers the missing seq from far behind: gap-fill.
        assert_eq!(wide.try_merge(1001, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        // ...exactly once.
        assert_eq!(wide.try_merge(1001, ActiveLeg::Leg2, 0), None);

        let mut narrow = HitlessMerger::new();
        assert_eq!(narrow.try_merge(1000, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Beyond the narrow window the same forward jump is a stream
        // reset (>= window), so the late gap-fill is dropped.
        assert_eq!(
            narrow.try_merge(1000u16.wrapping_add(lag), ActiveLeg::Leg1, 0),
            Some(ActiveLeg::Leg1)
        );
        assert_eq!(narrow.try_merge(1001, ActiveLeg::Leg2, 0), None);
    }

    /// Wide window stays consistent across the u16 sequence wrap.
    #[test]
    fn test_wide_window_across_u16_wrap() {
        let mut merger = HitlessMerger::with_window(16384);
        assert_eq!(merger.try_merge(65530, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(2, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1)); // wrap
        assert_eq!(merger.try_merge(65531, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2)); // fill behind wrap
        assert_eq!(merger.try_merge(65531, ActiveLeg::Leg2, 0), None); // dup
        assert_eq!(merger.try_merge(2, ActiveLeg::Leg2, 0), None); // dup of post-wrap
    }

    #[test]
    #[should_panic(expected = "multiple of 64")]
    fn test_window_rejects_a_non_multiple_of_64() {
        let _ = HitlessMerger::with_window(1000);
    }

    /// A window that does not divide 65536 keeps every seq on its own slot
    /// across the u16 wrap. Placed at `seq % 192`, seq 0 shares a bit with
    /// 65472, 64 seqs before it, so leg 2's copy of a 65472 leg 1 lost read
    /// as a duplicate once leg 1 had moved past the wrap.
    #[test]
    fn test_a_non_power_of_two_window_gap_fills_across_the_u16_wrap() {
        let mut merger = HitlessMerger::with_window(192);
        let mut seq: u16 = 65_400;
        while seq != 11 {
            if seq != 65_472 {
                assert_eq!(merger.try_merge(seq, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
            }
            seq = seq.wrapping_add(1);
        }
        assert_eq!(merger.try_merge(65_472, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(65_472, ActiveLeg::Leg2, 0), None);
        assert_eq!(merger.try_merge(0, ActiveLeg::Leg2, 0), None);
        assert_eq!(merger.try_merge(65_535, ActiveLeg::Leg2, 0), None);
        assert_eq!(merger.lost(), 0);
    }

    /// The window decides how far behind a gap can still be filled.
    #[test]
    fn test_the_window_bounds_the_gap_fill() {
        for (window, fillable) in [(64u16, false), (128, true)] {
            let mut merger = HitlessMerger::with_window(window);
            for seq in 0..=100u16 {
                if seq != 20 {
                    merger.try_merge(seq, ActiveLeg::Leg1, 0);
                }
            }
            // 80 behind: inside a 128-slot window, outside a 64-slot one.
            let filled = merger.try_merge(20, ActiveLeg::Leg2, 0).is_some();
            assert_eq!(filled, fillable, "window {window}");
            assert_eq!(merger.lost(), u64::from(!fillable), "window {window}");
        }
    }

    /// A seq is lost when it leaves the window with neither leg having
    /// delivered it — not before, and not when the other leg filled it.
    #[test]
    fn test_lost_counts_only_what_neither_leg_delivered() {
        let mut merger = HitlessMerger::with_window(64);
        for seq in 0..=73u16 {
            if seq != 10 && seq != 20 {
                merger.try_merge(seq, ActiveLeg::Leg1, 0);
            }
        }
        // Leg 2 fills 20; 10 is still inside the window, so not yet lost.
        assert_eq!(merger.try_merge(20, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        assert_eq!(merger.lost(), 0);
        // 74 pushes 10 out of the window.
        merger.try_merge(74, ActiveLeg::Leg1, 0);
        assert_eq!(merger.lost(), 1);
        // Its copy arriving now is too late to forward, and lost once.
        assert_eq!(merger.try_merge(10, ActiveLeg::Leg2, 0), None);
        for seq in 75..=200u16 {
            merger.try_merge(seq, ActiveLeg::Leg1, 0);
        }
        assert_eq!(merger.lost(), 1, "20 was filled by leg 2");
    }

    /// The bitmap's slots before the first packet held no seq: the first
    /// window after the anchor is no loss, nor is a skip counted twice.
    #[test]
    fn test_lost_starts_at_the_anchor() {
        let mut merger = HitlessMerger::with_window(64);
        for seq in 1000..1200u16 {
            merger.try_merge(seq, ActiveLeg::Leg1, 0);
        }
        assert_eq!(merger.lost(), 0);
        // A forward skip of 3 leaves 1200 and 1201 to the other leg …
        merger.try_merge(1202, ActiveLeg::Leg1, 0);
        assert_eq!(merger.lost(), 0);
        // … and once they leave the window unfilled, they are lost.
        for seq in 1203..1300u16 {
            merger.try_merge(seq, ActiveLeg::Leg1, 0);
        }
        assert_eq!(merger.lost(), 2);
    }

    /// A jump of a whole window or more re-anchors the window. The seqs the
    /// old window still lacked leave it then, lost, and the re-anchor is
    /// counted; the seqs the jump skipped are not, a burst or a new ISN.
    #[test]
    fn test_a_reset_counts_the_old_windows_unfilled_seqs() {
        // Partly covered: 51 slots since the anchor, two of them clear.
        let mut merger = HitlessMerger::with_window(64);
        for seq in (0..=50u16).filter(|&s| s != 40 && s != 45) {
            merger.try_merge(seq, ActiveLeg::Leg1, 0);
        }
        assert_eq!((merger.lost(), merger.resets()), (0, 0));
        merger.try_merge(5_000, ActiveLeg::Leg1, 0);
        assert_eq!((merger.lost(), merger.resets()), (2, 1));
        // A copy arriving now is too late, and lost once.
        assert_eq!(merger.try_merge(40, ActiveLeg::Leg2, 0), None);
        for seq in 5_001..5_200u16 {
            merger.try_merge(seq, ActiveLeg::Leg1, 0);
        }
        assert_eq!((merger.lost(), merger.resets()), (2, 1));

        // Fully covered, across the u16 wrap, one slot clear.
        let mut merger = HitlessMerger::with_window(192);
        let mut seq: u16 = 65_000;
        while seq != 300 {
            if seq != 200 {
                merger.try_merge(seq, ActiveLeg::Leg1, 0);
            }
            seq = seq.wrapping_add(1);
        }
        assert_eq!(merger.lost(), 0, "200 is still inside the window");
        merger.try_merge(9_000, ActiveLeg::Leg1, 0);
        assert_eq!((merger.lost(), merger.resets()), (1, 1));
    }

    /// A jump of a whole window or more over a window with nothing unfilled
    /// counts no loss of its own: from the seqs alone a new ISN looks the
    /// same as a burst both legs lost. The re-anchor is counted instead.
    #[test]
    fn test_a_stream_reset_is_not_loss() {
        let mut merger = HitlessMerger::with_window(64);
        for seq in 0..10u16 {
            merger.try_merge(seq, ActiveLeg::Leg1, 0);
        }
        merger.try_merge(5_000, ActiveLeg::Leg1, 0);
        for seq in 5_001..5_200u16 {
            merger.try_merge(seq, ActiveLeg::Leg1, 0);
        }
        assert_eq!((merger.lost(), merger.resets()), (0, 1));
    }

    /// One packet every `period_us`, from `leg`, for each seq of `seqs`
    /// starting at `t0_us`; returns the seqs forwarded and the time after.
    fn stream(
        merger: &mut HitlessMerger,
        seqs: impl IntoIterator<Item = u16>,
        leg: ActiveLeg,
        t0_us: u64,
        period_us: u64,
    ) -> (Vec<u16>, u64) {
        let mut t = t0_us;
        let mut forwarded = Vec::new();
        for seq in seqs {
            if merger.try_merge(seq, leg, t).is_some() {
                forwarded.push(seq);
            }
            t += period_us;
        }
        (forwarded, t)
    }

    /// 3.5 s both legs lost at 100 Mbps (9 500 packets a second) skips
    /// 33 250 seqs: a jump of half the sequence space and more, which reads
    /// as "too far behind". Nothing was accepted for the whole outage, so the
    /// first packet after it re-anchors the window and the stream goes on.
    /// The window used to discard it until its seqs wrapped back: 32 286
    /// packets, 3.4 s more black.
    #[test]
    fn test_a_jump_of_half_the_sequence_space_after_an_outage_reanchors() {
        let mut merger = HitlessMerger::new();
        let (sent, t) = stream(&mut merger, 0..1_000, ActiveLeg::Leg1, 0, 105);
        assert_eq!(sent.len(), 1_000);
        let resume = 999u16.wrapping_add(33_251);
        let after: Vec<u16> = (0..2_000u16).map(|i| resume.wrapping_add(i)).collect();
        let (sent, _) = stream(&mut merger, after.clone(), ActiveLeg::Leg2, t + 3_500_000, 105);
        assert_eq!(sent, after, "the stream after the outage is forwarded whole");
        assert_eq!(merger.resets(), 1);
        // Every seq the outage took is skipped, none lost: from the seqs alone
        // a burst cannot be told from a restart.
        assert_eq!((merger.lost(), merger.skipped()), (0, 33_250));
    }

    /// An encoder restarting onto an ISN in the backward half, with no pause
    /// at all: the new stream is accepted once nothing of the old has been
    /// for the stale time, about 1 s, not when its seqs wrap back into the
    /// window, 34 s at 10 Mbps.
    #[test]
    fn test_a_new_isn_behind_the_old_reanchors_after_the_stale_time() {
        let mut merger = HitlessMerger::new();
        let period = 1_050; // 10 Mbps, 950 packets a second
        let (_, t) = stream(&mut merger, 30_000..31_000, ActiveLeg::Leg1, 0, period);
        let (sent, _) = stream(&mut merger, 5_000..9_000, ActiveLeg::Leg1, t, period);
        assert_eq!(merger.resets(), 1);
        let first = sent[0];
        assert!(
            (5_950..=5_955).contains(&first),
            "the new stream is forwarded from a stale time in, not from seq {first}"
        );
        assert_eq!(sent, (first..9_000).collect::<Vec<_>>());
    }

    /// Both legs moving to a new stream together: the second copy of one of
    /// its seqs, from the other leg, right after the first, re-anchors at once.
    #[test]
    fn test_both_legs_agreeing_on_a_moved_stream_reanchors_at_once() {
        let mut merger = HitlessMerger::new();
        let (_, mut t) = stream(&mut merger, 30_000..31_000, ActiveLeg::Leg1, 0, 1_000);
        let mut sent = Vec::new();
        for seq in 2_000..2_100u16 {
            for leg in [ActiveLeg::Leg1, ActiveLeg::Leg2] {
                if merger.try_merge(seq, leg, t).is_some() {
                    sent.push(seq);
                }
            }
            t += 1_000;
        }
        assert_eq!(sent, (2_000..2_100).collect::<Vec<_>>());
        assert_eq!(merger.resets(), 1);
    }

    /// A leg lagging by more than the window — 2 000 seqs at 9 500 packets a
    /// second, 210 ms — sends nothing but packets too far behind, and the
    /// window never follows it: not while the other leg runs, and not once
    /// the other leg goes, since each of its packets arrives no more than the
    /// lag after its seq first did. Once it passes the newest seq it carries
    /// the stream on, every seq once.
    #[test]
    fn test_a_leg_lagging_past_the_window_never_moves_it() {
        let mut merger = HitlessMerger::new();
        let period = 105;
        let lag = 2_000u64;
        let mut arrivals: Vec<(u64, u16, ActiveLeg)> = Vec::new();
        for k in 0..20_000u64 {
            let seq = k as u16;
            // Leg 1 goes at packet 10 000.
            if k < 10_000 {
                arrivals.push((k * period, seq, ActiveLeg::Leg1));
            }
            arrivals.push(((k + lag) * period, seq, ActiveLeg::Leg2));
        }
        arrivals.sort_by_key(|&(t, seq, _)| (t, seq));
        let mut sent = Vec::new();
        for (t, seq, leg) in arrivals {
            if merger.try_merge(seq, leg, t).is_some() {
                sent.push(seq);
            }
        }
        assert_eq!(merger.resets(), 0);
        assert_eq!(sent, (0..20_000u16).collect::<Vec<_>>());
    }

    #[test]
    fn test_ascending_sequence() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(103, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
    }

    #[test]
    fn test_duplicate_rejected() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Same sequence from other leg should be rejected
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg2, 0), None);
        // Same sequence from same leg also rejected
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1, 0), None);
    }

    #[test]
    fn test_out_of_order_within_window_accepted() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Packet 101 arrives late from leg2 — within the gap-fill window
        // (the slot at 101 was left clear when leg1 advanced 100 → 102),
        // so it must be emitted to provide hitless coverage. This is the
        // exact behaviour Bug #2 residual was missing.
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        // A second 101 (e.g. retransmit) is now a duplicate.
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1, 0), None);
    }

    #[test]
    fn test_gap_accepted() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Gap: 100 -> 105 (diff=5, valid forward gap)
        assert_eq!(merger.try_merge(105, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
    }

    #[test]
    fn test_sequence_wraparound() {
        let mut merger = HitlessMerger::new();
        // Near end of u16 space
        assert_eq!(merger.try_merge(65534, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(65535, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Wraparound to 0
        assert_eq!(merger.try_merge(0, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(1, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
    }

    #[test]
    fn test_far_backward_jump_rejected() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(50000, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // A packet far enough behind to fall outside the gap-fill window
        // (REORDER_WINDOW = 1024). 50000 - 2000 = 48000, which is more
        // than 1024 behind — must be dropped.
        assert_eq!(merger.try_merge(48000, ActiveLeg::Leg2, 0), None);
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
        assert_eq!(merger.try_merge(1000, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(1000, ActiveLeg::Leg2, 0), None); // dup of leg1
        assert_eq!(merger.try_merge(1002, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Critical: leg2 1001 arrives AFTER we've already advanced past
        // 1001 via leg1. The previous merger dropped it; the gap-fill
        // window must accept it.
        assert_eq!(merger.try_merge(1001, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(1002, ActiveLeg::Leg2, 0), None); // dup of leg1
    }

    /// A duplicate of a previously gap-filled packet is still a duplicate.
    #[test]
    fn test_gap_fill_then_dup_dropped() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(1000, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(1002, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(1001, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2)); // gap fill
        // A second arrival of 1001 (e.g. retransmit on leg1) must be a dup.
        assert_eq!(merger.try_merge(1001, ActiveLeg::Leg1, 0), None);
    }

    /// Gap fills must work across the u16 wraparound boundary.
    #[test]
    fn test_gap_fill_across_wraparound() {
        let mut merger = HitlessMerger::new();
        assert_eq!(merger.try_merge(65534, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Skip 65535, jump to 0 (wraparound).
        assert_eq!(merger.try_merge(0, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // leg2 fills 65535 — must be accepted.
        assert_eq!(merger.try_merge(65535, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        // Duplicate 65535 from leg1 must be dropped.
        assert_eq!(merger.try_merge(65535, ActiveLeg::Leg1, 0), None);
        // Forward progress still works after.
        assert_eq!(merger.try_merge(1, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
    }

    /// Gaps that fall behind the reorder window are unrecoverable. The
    /// merger drops them rather than incorrectly emitting them out of
    /// order, since the receiver has already moved on.
    #[test]
    fn test_gap_outside_window_dropped() {
        let mut merger = HitlessMerger::new();
        merger.try_merge(0, ActiveLeg::Leg1, 0);
        // Advance ~1500 packets ahead (well beyond REORDER_WINDOW = 1024).
        for s in 1..=1500u16 {
            merger.try_merge(s, ActiveLeg::Leg1, 0);
        }
        // A late arrival of seq 100 is now ~1400 behind highest. Must be dropped.
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg2, 0), None);
    }

    /// A forward jump larger than the reorder window resets the merger
    /// (treated as a stream reset / publisher restart with a new ISN).
    #[test]
    fn test_huge_forward_jump_resets() {
        let mut merger = HitlessMerger::new();
        merger.try_merge(100, ActiveLeg::Leg1, 0);
        // Forward jump of 5000 (>> REORDER_WINDOW). This is a stream
        // reset, not a real RTP forward jump.
        assert_eq!(merger.try_merge(5100, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // After reset, the bitmap is fresh — duplicate of the new anchor
        // is still a dup, but a different forward seq works.
        assert_eq!(merger.try_merge(5100, ActiveLeg::Leg2, 0), None);
        assert_eq!(merger.try_merge(5101, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
    }

    #[test]
    fn test_interleaved_legs_dedup() {
        let mut merger = HitlessMerger::new();
        // Simulate two legs sending the same packets with slight timing differences
        // Leg1: 100, 101, 102
        // Leg2: 100, 101, 102 (arrives slightly later)
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg2, 0), None); // dup
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1, 0), None); // dup
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg2, 0), None); // dup
    }

    #[test]
    fn test_leg1_fails_leg2_continues() {
        let mut merger = HitlessMerger::new();
        // Start on leg1
        assert_eq!(merger.try_merge(100, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        assert_eq!(merger.try_merge(101, ActiveLeg::Leg1, 0), Some(ActiveLeg::Leg1));
        // Leg1 stops, leg2 continues
        assert_eq!(merger.try_merge(102, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
        assert_eq!(merger.try_merge(103, ActiveLeg::Leg2, 0), Some(ActiveLeg::Leg2));
    }

    /// TS packets of a video PID in a 1316-byte datagram.
    const RUN: u16 = 7;

    /// Feed `leg`'s packets `indices` of seq `seq` to `merger`; returns
    /// the indices it forwards.
    fn run(
        merger: &mut HitlessRunMerger,
        seq: u16,
        indices: std::ops::Range<u16>,
        leg: ActiveLeg,
    ) -> Vec<u16> {
        indices
            .filter(|&i| merger.try_merge(seq, i, leg, 0).is_some())
            .collect()
    }

    /// Every packet of a run is forwarded once, from whichever leg brings
    /// it first: a `HitlessMerger` on the seq alone forwarded the first
    /// packet of each datagram and dropped the other six as duplicates.
    #[test]
    fn test_run_merger_forwards_every_packet_of_a_run_once() {
        let mut merger = HitlessRunMerger::with_window(1024);
        for seq in 100..120u16 {
            let (first, second) = if seq % 2 == 0 {
                (ActiveLeg::Leg1, ActiveLeg::Leg2)
            } else {
                (ActiveLeg::Leg2, ActiveLeg::Leg1)
            };
            assert_eq!(
                run(&mut merger, seq, 0..RUN, first),
                (0..RUN).collect::<Vec<_>>()
            );
            assert!(
                run(&mut merger, seq, 0..RUN, second).is_empty(),
                "seq {seq}"
            );
        }
    }

    /// A datagram one leg lost comes whole from the other, a gap fill like
    /// the transport merger's.
    #[test]
    fn test_run_merger_fills_a_lost_datagram_from_the_other_leg() {
        let mut merger = HitlessRunMerger::with_window(1024);
        for seq in (0..10u16).filter(|&s| s != 4) {
            assert_eq!(run(&mut merger, seq, 0..RUN, ActiveLeg::Leg1).len(), 7);
        }
        for seq in 0..10u16 {
            let filled = run(&mut merger, seq, 0..RUN, ActiveLeg::Leg2);
            let expected: Vec<u16> = if seq == 4 { (0..RUN).collect() } else { vec![] };
            assert_eq!(filled, expected, "seq {seq}");
        }
    }

    /// Leg 2 brings part of a run before leg 1 brings any: leg 1 forwards
    /// the rest, in order, and leg 2's rest is a duplicate.
    #[test]
    fn test_run_merger_takes_a_run_part_from_each_leg() {
        let mut merger = HitlessRunMerger::with_window(1024);
        assert_eq!(run(&mut merger, 7, 0..RUN, ActiveLeg::Leg1).len(), 7);
        assert_eq!(run(&mut merger, 8, 0..3, ActiveLeg::Leg2), vec![0, 1, 2]);
        assert_eq!(
            run(&mut merger, 8, 0..RUN, ActiveLeg::Leg1),
            vec![3, 4, 5, 6]
        );
        assert!(run(&mut merger, 8, 3..RUN, ActiveLeg::Leg2).is_empty());
        assert!(run(&mut merger, 7, 0..RUN, ActiveLeg::Leg2).is_empty());
    }

    /// A leg that missed packets of a run — its channel lagged, or it
    /// subscribed mid-datagram — forwards none past the hole: they would
    /// leave the run out of order. The other leg brings the run from the
    /// hole on, and the next run is merged as usual.
    #[test]
    fn test_run_merger_leaves_the_rest_of_a_holed_run_to_the_other_leg() {
        let mut merger = HitlessRunMerger::with_window(1024);
        // Leg 1's first run starts at its fourth packet.
        assert!(run(&mut merger, 50, 3..RUN, ActiveLeg::Leg1).is_empty());
        // Leg 1's next run lags after its third packet.
        assert_eq!(run(&mut merger, 51, 0..3, ActiveLeg::Leg1), vec![0, 1, 2]);
        assert!(run(&mut merger, 51, 5..RUN, ActiveLeg::Leg1).is_empty());
        assert_eq!(run(&mut merger, 52, 0..RUN, ActiveLeg::Leg1).len(), 7);
        assert_eq!(
            run(&mut merger, 50, 0..RUN, ActiveLeg::Leg2),
            (0..RUN).collect::<Vec<_>>()
        );
        assert_eq!(
            run(&mut merger, 51, 0..RUN, ActiveLeg::Leg2),
            vec![3, 4, 5, 6]
        );
        assert!(run(&mut merger, 52, 0..RUN, ActiveLeg::Leg2).is_empty());
        assert_eq!(run(&mut merger, 53, 0..RUN, ActiveLeg::Leg1).len(), 7);
    }

    /// A repeat of a datagram on the same leg — the network duplicated it
    /// — is numbered from 0 again, so it is a duplicate packet by packet,
    /// back to back or not.
    #[test]
    fn test_run_merger_drops_a_repeated_datagram() {
        let mut merger = HitlessRunMerger::with_window(1024);
        assert_eq!(run(&mut merger, 9, 0..RUN, ActiveLeg::Leg1).len(), 7);
        assert!(run(&mut merger, 9, 0..RUN, ActiveLeg::Leg1).is_empty());
        assert_eq!(run(&mut merger, 10, 0..RUN, ActiveLeg::Leg1).len(), 7);
        assert!(run(&mut merger, 9, 0..RUN, ActiveLeg::Leg1).is_empty());
    }

    /// Runs merge across the u16 wrap, in a window that does not divide
    /// 65536, and a run that has left the window is dropped whole.
    #[test]
    fn test_run_merger_across_the_u16_wrap() {
        let mut merger = HitlessRunMerger::with_window(192);
        let mut seq: u16 = 65_400;
        while seq != 40 {
            if seq != 65_520 && seq != 65_530 {
                assert_eq!(
                    run(&mut merger, seq, 0..RUN, ActiveLeg::Leg1).len(),
                    7,
                    "seq {seq}"
                );
            }
            seq = seq.wrapping_add(1);
        }
        // 45 behind the newest seq: filled, once.
        assert_eq!(run(&mut merger, 65_530, 0..RUN, ActiveLeg::Leg2).len(), 7);
        assert!(run(&mut merger, 65_530, 0..RUN, ActiveLeg::Leg2).is_empty());
        assert!(run(&mut merger, 3, 0..RUN, ActiveLeg::Leg2).is_empty());
        for seq in 40..=300u16 {
            assert_eq!(
                run(&mut merger, seq, 0..RUN, ActiveLeg::Leg1).len(),
                7,
                "seq {seq}"
            );
        }
        // 316 behind: too late, though no leg delivered it.
        assert!(run(&mut merger, 65_520, 0..RUN, ActiveLeg::Leg2).is_empty());
        assert_eq!(merger.seqs.lost(), 1);
    }

    /// With one packet per seq a run merger decides every packet as the
    /// transport merger does: same window, same gap fills, same too-late
    /// drops and re-anchors.
    #[test]
    fn test_run_merger_with_one_packet_per_seq_decides_as_the_seq_merger() {
        for window in [64u16, 192, 1024] {
            let mut runs = HitlessRunMerger::with_window(window);
            let mut seqs = HitlessMerger::with_window(window);
            let mut state: u32 = 0x2545_F491;
            let mut seq: u16 = 65_000;
            for _ in 0..20_000 {
                // xorshift: mostly steps of one, some reordering and
                // repeats, an occasional jump of a window or more.
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let step = match state % 100 {
                    0 => 3 * window,
                    1..=9 => 0u16.wrapping_sub((state % 300) as u16),
                    10..=19 => 0,
                    _ => 1,
                };
                seq = seq.wrapping_add(step);
                let leg = if state & 0x100 == 0 {
                    ActiveLeg::Leg1
                } else {
                    ActiveLeg::Leg2
                };
                assert_eq!(
                    runs.try_merge(seq, 0, leg, 0),
                    seqs.try_merge(seq, leg, 0),
                    "window {window} seq {seq}"
                );
            }
            assert_eq!(
                (runs.seqs.lost(), runs.seqs.resets()),
                (seqs.lost(), seqs.resets())
            );
        }
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
//   4. Insert into a `BTreeMap<(i64, u16), BufferedEntry>` keyed by
//      virtual seq and the packet's index in the seq's run (0 with one
//      packet per seq), stamping `release_at = now + max_path_diff`.
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
///
/// `D` decides what is new. The transport layer's [`HitlessMerger`] (the
/// default) takes one packet per seq, through [`Self::ingest`]; the PID
/// bus's [`HeldRuns`] takes a run of them, each with its index in the run,
/// through [`Self::ingest_run`]. Either way a packet is held under its
/// `(seq, index)` and released in that order.
///
/// What is released comes back as a [`std::vec::Drain`] over a buffer the
/// merger keeps, so releasing allocates nothing once that buffer has grown
/// to the largest release.
pub struct BufferedHitlessMerger<T: Clone, D = HitlessMerger> {
    /// Dedup primitive (rules duplicates / stream resets).
    dedup: D,
    /// Skew-accommodation buffer keyed by virtual (u16-wrap-resolved)
    /// seq number and the packet's index in the seq's run (always 0 with
    /// one packet per seq). Entries drain in key order at `release_at`.
    buffer: BTreeMap<(i64, u16), BufferedEntry<T>>,
    /// Anchors that project `u16` wire seq → `i64` virtual seq.
    anchor_real: u16,
    anchor_virtual: i64,
    anchor_set: bool,
    /// Cursor — just past the key last emitted: that virtual seq, and the
    /// index after the packet's. Every key below it has been passed. None
    /// until the first packet has been emitted.
    cursor: Option<(i64, u16)>,
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
    /// The origin of the microsecond clock handed to the dedup.
    epoch: Instant,
    /// What the last call released, reused from call to call.
    out: Vec<(ActiveLeg, T)>,
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
    /// State machine — Up → Down.
    health: LegHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LegHealth {
    #[default]
    Unknown,
    Up,
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
    /// Tri-state per leg: 0 unknown, 1 up, 3 down.
    pub leg1_health: AtomicU64,
    pub leg2_health: AtomicU64,
    /// Set when the two legs are observed with mismatched SSRCs.
    pub ssrc_mismatch: AtomicU64,
}

/// Hard threshold on max_path_diff. Beyond this the constant emission
/// latency would be larger than typical broadcast end-to-end budgets.
pub const MAX_PATH_DIFF_BOUND: Duration = Duration::from_millis(2000);

/// Conservative upper bound on the seq buffer. At 2400 pkt/s this is
/// ≈ 5 s of pending packets — far above any realistic path
/// differential or burst.
const DEFAULT_MAX_BUFFER_PACKETS: usize = 12_000;

/// The same bound for a run merger's buffer ([`HeldRuns`]), which holds TS
/// packets rather than datagrams: seven times as many packets, as many
/// datagrams as the transport cap for a PID that fills its datagrams, seven
/// 188-byte packets to 1316 bytes — more datagrams for a PID that shares
/// them.
const RUN_MAX_BUFFER_PACKETS: usize = DEFAULT_MAX_BUFFER_PACKETS * 7;

/// "Down" threshold: how long a leg can be silent before we flip its
/// health to Down. Below this, missing packets only count as Degraded.
const LEG_DOWN_THRESHOLD: Duration = Duration::from_millis(1000);

impl<T: Clone> BufferedHitlessMerger<T> {
    /// A merger over the default TS-class dedup window ([`REORDER_WINDOW`]).
    pub fn new(max_path_diff: Duration) -> Self {
        Self::with_window(max_path_diff, REORDER_WINDOW)
    }

    /// A merger whose dedup primitive runs a `window`-slot gap-fill window
    /// (see [`HitlessMerger::with_window`]). The window bounds the path
    /// differential in packets as `max_path_diff` bounds it in time: a
    /// slower leg's copy that arrives more than `window` seqs behind the
    /// faster one is dropped as late, however long the hold.
    pub fn with_window(max_path_diff: Duration, window: u16) -> Self {
        let max_path_diff = bounded_path_diff(max_path_diff);
        Self::build(
            HitlessMerger::with_window(window).with_stale_after_us(stale_after_us(max_path_diff)),
            max_path_diff,
            DEFAULT_MAX_BUFFER_PACKETS,
        )
    }

    /// Ingest a packet. Returns the (leg, payload) pairs that are now
    /// ready to forward, in seq order.
    pub fn ingest(
        &mut self,
        seq: u16,
        leg: ActiveLeg,
        payload: T,
        now: Instant,
    ) -> std::vec::Drain<'_, (ActiveLeg, T)> {
        self.ingest_at(seq, 0, leg, payload, now)
    }
}

impl<T: Clone> BufferedHitlessMerger<T, HeldRuns> {
    /// A merger over runs of packets per seq (see [`HitlessRunMerger`] and
    /// [`HeldRuns`]), its seqs in a `window`-seq gap-fill window as
    /// [`Self::with_window`]'s are.
    pub fn with_runs(max_path_diff: Duration, window: u16) -> Self {
        let max_path_diff = bounded_path_diff(max_path_diff);
        Self::build(
            HeldRuns(
                HitlessMerger::with_window(window)
                    .with_stale_after_us(stale_after_us(max_path_diff)),
            ),
            max_path_diff,
            RUN_MAX_BUFFER_PACKETS,
        )
    }

    /// Ingest packet `index` of seq `seq`'s run. Returns the (leg, payload)
    /// pairs that are now ready to forward, in `(seq, index)` order.
    pub fn ingest_run(
        &mut self,
        seq: u16,
        index: u16,
        leg: ActiveLeg,
        payload: T,
        now: Instant,
    ) -> std::vec::Drain<'_, (ActiveLeg, T)> {
        self.ingest_at(seq, index, leg, payload, now)
    }
}

/// `max_path_diff`, capped at [`MAX_PATH_DIFF_BOUND`].
fn bounded_path_diff(max_path_diff: Duration) -> Duration {
    max_path_diff.min(MAX_PATH_DIFF_BOUND)
}

/// How long a buffered merger's dedup accepts nothing before a packet too
/// far behind re-anchors it ([`HitlessMerger::with_stale_after_us`]): a second
/// past the path differential the legs are allowed, so a leg lagging within
/// it can never move the window.
fn stale_after_us(max_path_diff: Duration) -> u64 {
    max_path_diff.as_micros() as u64 + DEFAULT_STALE_AFTER_US
}

impl<T: Clone, D: SeqDedup> BufferedHitlessMerger<T, D> {
    fn build(dedup: D, max_path_diff: Duration, max_buffer_packets: usize) -> Self {
        let bounded = bounded_path_diff(max_path_diff);
        Self {
            dedup,
            buffer: BTreeMap::new(),
            anchor_real: 0,
            anchor_virtual: 0,
            anchor_set: false,
            cursor: None,
            max_path_diff: bounded,
            max_buffer_packets,
            leg_state: [LegState::default(), LegState::default()],
            stats: BufferedHitlessStats::default(),
            epoch: Instant::now(),
            out: Vec::new(),
        }
    }

    /// Project a `u16` wire seq onto a monotonically-increasing `i64`
    /// virtual seq. Wraparound is preserved by sign-extending the wrapping
    /// `u16` distance from the current anchor.
    ///
    /// The anchor slides forward continuously: whenever a seq is at or
    /// ahead of the anchor (`diff > 0`) it becomes the new anchor. This
    /// keeps `|diff|` bounded by the realistic reorder window on every
    /// call. The previous lazy re-anchor (only past a 1_000_000 threshold)
    /// could never fire because `diff` was first truncated through `as i16`
    /// to `[-32768, 32767]`, so after 32_768 packets the cast wrapped
    /// negative and the virtual seq sawtoothed — the drain cursor then
    /// dropped every later packet as a late gap-fill and emission froze at
    /// exactly 0x8000 (cell16, 2026-05-21).
    fn project(&mut self, seq: u16) -> i64 {
        if !self.anchor_set {
            self.anchor_real = seq;
            self.anchor_virtual = 0;
            self.anchor_set = true;
            return 0;
        }
        let diff = seq.wrapping_sub(self.anchor_real) as i16 as i64;
        let virt = self.anchor_virtual + diff;
        if diff > 0 {
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

    /// Ingest packet `index` of seq `seq` (0 with one packet per seq).
    fn ingest_at(
        &mut self,
        seq: u16,
        index: u16,
        leg: ActiveLeg,
        payload: T,
        now: Instant,
    ) -> std::vec::Drain<'_, (ActiveLeg, T)> {
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
        let mut key = (self.project(seq), index);
        let leg_bit = leg_bit(leg);

        // If we already have an entry at this key, we just observed the
        // slower leg. Update the legs_seen mask + path-diff measurement;
        // drop the duplicate payload.
        if let Some(entry) = self.buffer.get_mut(&key) {
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
        let resets = self.dedup.resets();
        let now_us = now.saturating_duration_since(self.epoch).as_micros() as u64;
        match self.dedup.admit(seq, index, leg, now_us) {
            None => {
                // Outside the dedup window. Late or stale. Drop.
                self.stats.late_dropped.fetch_add(1, Ordering::Relaxed);
                return self.drain_ready(now);
            }
            Some(_) => {
                // A seq the stream moved to re-anchored the dedup's window
                // from behind it — half the sequence space or more past the
                // old seqs, or a new ISN behind them — and projected below
                // everything held. It starts a new stretch of virtual seqs
                // past them instead, so what is held drains first and the
                // new stream follows it rather than being dropped as late.
                if self.dedup.resets() != resets {
                    self.stats.stream_resets.fetch_add(1, Ordering::Relaxed);
                    if key.0 < self.anchor_virtual {
                        self.anchor_real = seq;
                        self.anchor_virtual += 1;
                        key.0 = self.anchor_virtual;
                    }
                }
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
                if let Some(c) = self.cursor
                    && key < c {
                        self.stats.late_dropped.fetch_add(1, Ordering::Relaxed);
                        return self.drain_ready(now);
                    }
                if self.buffer.len() >= self.max_buffer_packets {
                    self.stats
                        .buffer_overflow_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    return self.drain_ready(now);
                }
                self.buffer.insert(
                    key,
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
    pub fn drain_expired(&mut self, now: Instant) -> std::vec::Drain<'_, (ActiveLeg, T)> {
        self.update_leg_health(now);
        self.drain_ready(now)
    }

    fn drain_ready(&mut self, now: Instant) -> std::vec::Drain<'_, (ActiveLeg, T)> {
        // Whatever a caller left of the previous release went when its
        // `Drain` dropped.
        let out = &mut self.out;
        out.clear();
        let mut last_emit_leg: Option<ActiveLeg> = None;
        while let Some((&key, entry)) = self.buffer.iter().next() {
            if now < entry.release_at {
                break;
            }
            let (virt, index) = key;
            // Account for any seq gap relative to the cursor: the seqs
            // between the last one emitted and this one.
            if let Some((last, _)) = self.cursor
                && virt > last + 1 {
                    let gap = (virt - last - 1) as u64;
                    self.stats.gap_lost.fetch_add(gap, Ordering::Relaxed);
                }
            // Emit. Update gap_filled / failover stats based on which
            // leg actually carried this packet first.
            let entry = self.buffer.remove(&key).unwrap();
            self.cursor = Some((virt, index.saturating_add(1)));
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
            if let Some(last) = last_emit_leg
                && last != entry.first_leg && entry.first_leg != ActiveLeg::None {
                    self.stats.failovers.fetch_add(1, Ordering::Relaxed);
                }
            last_emit_leg = Some(entry.first_leg);
            out.push((entry.first_leg, entry.payload));
        }
        self.stats
            .current_buffer_depth
            .store(self.buffer.len() as u64, Ordering::Relaxed);
        self.out.drain(..)
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
        let out: Vec<_> = m.ingest(100, ActiveLeg::Leg1, pkt(100), now).collect();
        assert!(out.is_empty(), "first packet must be held");
        // Drain just before deadline — still nothing.
        let out: Vec<_> = m.drain_expired(now + Duration::from_millis(49)).collect();
        assert!(out.is_empty());
        // Drain at deadline — emitted.
        let out: Vec<_> = m.drain_expired(now + Duration::from_millis(50)).collect();
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
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_millis(70)).collect();
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
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_millis(60)).collect();
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
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_millis(60)).collect();
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
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_millis(60)).collect();
        let seqs: Vec<u16> = out.iter().map(|(_, p)| *p).collect();
        assert_eq!(seqs, vec![101, 102]);
        // leg2's copy of 100 finally lands — well outside the path-diff window.
        let dropped_before = m.stats.late_dropped.load(Ordering::Relaxed);
        let out: Vec<_> = m.ingest(100, ActiveLeg::Leg2, 100, t0 + Duration::from_millis(80)).collect();
        assert!(out.is_empty(), "late gap-fill must NOT emit out of seq order");
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_millis(200)).collect();
        assert!(out.is_empty(), "late gap-fill must not surface on next drain either");
        assert_eq!(
            m.stats.late_dropped.load(Ordering::Relaxed),
            dropped_before + 1,
            "the rejected gap-fill should bump late_dropped"
        );
    }

    /// Regression for the 2026-05-21 cell16 stall: a long-running flow with
    /// a small path differential must keep emitting past the 32 768-packet
    /// (`i16` half-range) mark. The broken `project()` cast the wrapping
    /// `u16` distance through `as i16`, so after 32 768 packets the virtual
    /// seq sawtoothed negative; the drain cursor then dropped every
    /// subsequent packet as a "late gap-fill" and `packets_emitted` froze at
    /// exactly 0x8000. With the continuous re-anchor the virtual seq stays
    /// monotonic and emission never stalls.
    #[test]
    fn long_run_does_not_stall_at_i16_halfrange() {
        // path_diff 0 → each ingest drains immediately (one packet in, one
        // out). Start at an arbitrary ISN so we cross both the u16 wrap and
        // the old 32 768 boundary.
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(0));
        let t0 = Instant::now();
        let isn: u16 = 0xF000;
        let total: usize = 40_000;
        let mut emitted = 0u64;
        for i in 0..total {
            let seq = isn.wrapping_add(i as u16);
            let out: Vec<_> = m.ingest(seq, ActiveLeg::Leg1, pkt(seq), t0).collect();
            emitted += out.len() as u64;
        }
        // Flush anything still held (path_diff 0 means nothing should be).
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_secs(1)).collect();
        emitted += out.len() as u64;
        assert_eq!(
            emitted, total as u64,
            "merger stalled — emitted {emitted} of {total} (froze at the \
             i16 half-range; see cell16 2026-05-21)"
        );
        assert_eq!(
            m.stats.packets_emitted.load(Ordering::Relaxed),
            total as u64
        );
        assert_eq!(
            m.stats.late_dropped.load(Ordering::Relaxed),
            0,
            "no packet should be dropped as a late gap-fill on a clean \
             sequential stream"
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
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_millis(30)).collect();
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].1, 65534);
        assert_eq!(out[1].1, 65535);
        assert_eq!(out[2].1, 0);
        assert_eq!(out[3].1, 1);
    }

    /// A stream that moves half the sequence space or more — 3.5 s both legs
    /// lost at 100 Mbps — re-anchors the dedup from behind its window. Its
    /// seqs would project below everything the hold has passed, and be
    /// dropped as late; they start a new stretch past them instead, and the
    /// stream is released whole and in order.
    #[test]
    fn a_stream_moved_half_the_sequence_space_is_released() {
        let mut m = BufferedHitlessMerger::<u16>::new(Duration::from_millis(50));
        let t0 = Instant::now();
        for seq in 0..100u16 {
            let _ = m.ingest(seq, ActiveLeg::Leg1, seq, t0);
        }
        let old: Vec<u16> = m.drain_expired(t0 + Duration::from_millis(60)).map(|(_, p)| p).collect();
        assert_eq!(old, (0..100).collect::<Vec<_>>());
        let t1 = t0 + Duration::from_millis(3_500);
        let resume = 99u16.wrapping_add(33_251);
        let new: Vec<u16> = (0..200u16).map(|i| resume.wrapping_add(i)).collect();
        for &seq in &new {
            let _ = m.ingest(seq, ActiveLeg::Leg2, seq, t1);
        }
        let released: Vec<u16> =
            m.drain_expired(t1 + Duration::from_millis(60)).map(|(_, p)| p).collect();
        assert_eq!(released, new);
        assert_eq!(m.stats.late_dropped.load(Ordering::Relaxed), 0);
        assert_eq!(m.stats.stream_resets.load(Ordering::Relaxed), 1);
    }

    /// A run merger over seven packets a seq, each payload its `(seq, index)`.
    fn runs() -> BufferedHitlessMerger<(u16, u16), HeldRuns> {
        BufferedHitlessMerger::with_runs(Duration::from_millis(50), REORDER_WINDOW)
    }

    /// Feed `leg`'s packets `indices` of seq `seq` at `now`; returns what
    /// is released.
    fn feed(
        m: &mut BufferedHitlessMerger<(u16, u16), HeldRuns>,
        seq: u16,
        indices: std::ops::Range<u16>,
        leg: ActiveLeg,
        now: Instant,
    ) -> Vec<(u16, u16)> {
        let mut released = Vec::new();
        for i in indices {
            released.extend(m.ingest_run(seq, i, leg, (seq, i), now).map(|(_, p)| p));
        }
        released
    }

    fn every_packet(seqs: std::ops::Range<u16>) -> Vec<(u16, u16)> {
        seqs.flat_map(|s| (0..7).map(move |i| (s, i))).collect()
    }

    /// Every packet of every run is released once and in order, the
    /// datagrams one leg lost filled whole from the other.
    #[test]
    fn runs_release_every_packet_once_in_order() {
        let mut m = runs();
        let t0 = Instant::now();
        for seq in 0..30u16 {
            if seq % 7 != 3 {
                assert!(feed(&mut m, seq, 0..7, ActiveLeg::Leg1, t0).is_empty());
            }
            assert!(feed(&mut m, seq, 0..7, ActiveLeg::Leg2, t0).is_empty());
        }
        assert_eq!(
            m.drain_expired(t0 + Duration::from_millis(60))
                .map(|(_, p)| p)
                .collect::<Vec<_>>(),
            every_packet(0..30)
        );
        assert_eq!(m.stats.packets_emitted.load(Ordering::Relaxed), 210);
        assert_eq!(m.stats.gap_lost.load(Ordering::Relaxed), 0);
    }

    /// Leg 2 brings part of a run before leg 1 brings any, and leg 1 a run
    /// whose start its channel dropped: each run is released whole, in
    /// order, once.
    #[test]
    fn runs_take_a_run_part_from_each_leg() {
        let mut m = runs();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(5);
        feed(&mut m, 8, 0..3, ActiveLeg::Leg2, t0);
        feed(&mut m, 8, 0..7, ActiveLeg::Leg1, t1);
        feed(&mut m, 8, 3..7, ActiveLeg::Leg2, t1);
        feed(&mut m, 9, 4..7, ActiveLeg::Leg1, t1);
        feed(&mut m, 9, 0..7, ActiveLeg::Leg2, t1);
        let out: Vec<_> = m
            .drain_expired(t1 + Duration::from_millis(60))
            .map(|(_, p)| p)
            .collect();
        assert_eq!(out, every_packet(8..10));
    }

    /// A run's rest that arrives once the hold has released its start is
    /// still in order, so it is released; the packets past a hole are not,
    /// and the next run is merged as usual.
    #[test]
    fn runs_released_part_way_take_the_rest_in_order() {
        let mut m = runs();
        let t0 = Instant::now();
        feed(&mut m, 20, 0..3, ActiveLeg::Leg1, t0);
        let t1 = t0 + Duration::from_millis(60);
        assert_eq!(m.drain_expired(t1).len(), 3);
        // Leg 1 lagged past packets 3 and 4.
        assert!(feed(&mut m, 20, 5..7, ActiveLeg::Leg1, t1).is_empty());
        feed(&mut m, 20, 0..7, ActiveLeg::Leg2, t1);
        feed(&mut m, 21, 0..7, ActiveLeg::Leg1, t1);
        let out: Vec<_> = m
            .drain_expired(t1 + Duration::from_millis(60))
            .map(|(_, p)| p)
            .collect();
        let mut expected: Vec<_> = (3..7).map(|i| (20, i)).collect();
        expected.extend(every_packet(21..22));
        assert_eq!(out, expected);
    }

    /// A run neither leg has from its start — both legs' channels lagged
    /// into it, or it is the first after a start — is held from the hole
    /// on, and released in order past the hole once its hold expires: the
    /// missing packets had the whole hold to come. Unbuffered, the merger
    /// can wait for nothing and drops such a run from the hole on; held,
    /// those packets used to be dropped too, the tail of a run lost for a
    /// packet that never came.
    #[test]
    fn runs_release_past_a_hole_neither_leg_fills() {
        let mut m = runs();
        let t0 = Instant::now();
        feed(&mut m, 30, 0..7, ActiveLeg::Leg1, t0);
        // Neither leg has packets 0 and 1 of run 31.
        feed(&mut m, 31, 2..7, ActiveLeg::Leg1, t0);
        feed(&mut m, 31, 2..7, ActiveLeg::Leg2, t0);
        feed(&mut m, 32, 0..7, ActiveLeg::Leg2, t0);
        let out: Vec<_> = m.drain_expired(t0 + Duration::from_millis(60)).map(|(_, p)| p).collect();
        let mut expected = every_packet(30..31);
        expected.extend((2..7).map(|i| (31, i)));
        expected.extend(every_packet(32..33));
        assert_eq!(out, expected);
        // Too late for the hole now.
        assert!(feed(&mut m, 31, 0..2, ActiveLeg::Leg2, t0 + Duration::from_millis(70)).is_empty());
        assert!(m.drain_expired(t0 + Duration::from_millis(200)).next().is_none());
    }
}
