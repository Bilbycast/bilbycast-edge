// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Epoch-locked egress: deriving a wall-clock release instant from a PCR
//! value alone, with no peer coordination.
//!
//! # The idea
//!
//! When a flow's master clock is epoch-locked, output PCR is a pure
//! deterministic function of the host's UNIX-epoch wall clock —
//! [`crate::engine::master_clock::PtpMasterClock::now_27mhz`] is literally
//! `(unix_ns × 27 / 1000) mod PCR_MODULUS`. Two nodes whose clocks agree
//! therefore stamp the same PCR value onto the same instant *without ever
//! talking to each other*.
//!
//! That function is invertible. Given a PCR value we can recover the wall
//! instant that produced it, and schedule the datagram's release at
//! `that instant + egress_offset`. Every node computes the identical
//! target, so their egress aligns by arithmetic rather than by a control
//! loop. This is the pattern the broadcast industry calls **epoch
//! locking** — synchronising to a hypothetical encoder that started at
//! 00:00:00 UTC on 1 Jan 1970 and has been running ever since.
//!
//! # Why absolute correctness does not matter
//!
//! The generator applies a constant pre-roll
//! ([`crate::engine::av_sync_mux`]'s `PCR_PREROLL_27MHZ`) and the mux path
//! adds its own fixed depth. This module deliberately does **not** try to
//! model either. Any constant term is *common-mode*: it shifts every node
//! by the same amount and cancels out of the inter-node difference, which
//! is the only quantity this feature promises. Folding those constants
//! into the operator's `egress_offset_ms` is both simpler and more robust
//! than trying to track them.
//!
//! What *must* be identical across nodes is the arithmetic here — hence a
//! single shared implementation with exact integer maths and no
//! configuration knobs beyond the offset itself.
//!
//! # Clock domains
//!
//! PCR is generated from `SystemTime::now()`, i.e. **CLOCK_REALTIME**.
//! `wire_emit` schedules against **CLOCK_TAI** (see
//! [`crate::engine::wire_emit`]'s `monotonic_now_ns`), and the two differ
//! by the TAI−UTC offset — 37 s as of 2026, and 0 on a host that never
//! loaded leap-second data. Rather than carry a leap-second table, we
//! measure the offset directly by sampling both clocks at the same moment
//! ([`tai_minus_realtime_ns`]). That is self-correcting, needs no
//! configuration, and stays right across a leap second.
//!
//! Getting this wrong is not subtle: a 37 s error would be immediately and
//! obviously visible, not a quiet drift.

/// PCR wrap period in 27 MHz ticks. Same value as
/// [`crate::engine::pcr_pll::PCR_MODULUS_27MHZ`] and
/// [`crate::stats::pcr_trust::PCR_MODULUS_27MHZ`]; re-stated here so this
/// module stays a leaf with no engine dependencies (it is pure integer
/// maths and is unit-tested as such).
pub const PCR_MODULUS_27MHZ: u64 = (1u64 << 33) * 300;

/// The modulus expressed in nanoseconds — the ambiguity period of a PCR
/// value. `(1<<33) × 300 / 27e6` s ≈ 95_443 s ≈ 26.5 hours.
///
/// This is what makes the inversion safe: the ambiguity window is five
/// orders of magnitude larger than any plausible clock disagreement or
/// egress offset, so "pick the candidate nearest to now" can never choose
/// the wrong wrap.
///
/// **Do not use this for modular arithmetic.** The true period is
/// `95_443_717_688_888.89 ns` and this constant truncates it. Reducing a
/// 2026-era timestamp modulo the truncated value accumulates the missing
/// 0.89 ns once per elapsed period — ~18_700 periods since the UNIX
/// epoch, i.e. a ~16 µs error, growing by ~0.9 ns/day. [`unix_ns_from_pcr_27mhz`]
/// therefore does its reduction in exact 27 MHz tick space and converts
/// to nanoseconds only at the end. This constant is for human-facing
/// reasoning and for locating test points, nothing more.
///
/// Test-only, and gated rather than merely allowed: the warning above is
/// only enforceable if the production path cannot reach it at all.
#[cfg(test)]
pub const PCR_MODULUS_NS: u128 = (PCR_MODULUS_27MHZ as u128) * 1000 / 27;

/// Worst-case round-trip error of [`pcr_27mhz_from_unix_ns`] followed by
/// [`unix_ns_from_pcr_27mhz`], in nanoseconds.
///
/// Both directions truncate, and each truncation loses at most one 27 MHz
/// tick (`1000/27` ≈ 37.04 ns), so the round trip lands at most ~75 ns
/// *below* the original instant and never above it. That is five orders
/// of magnitude inside the ±100 ms this feature targets, and — because
/// every node runs the identical arithmetic — it is common-mode anyway.
///
/// Test-only: it exists to give the round-trip tests a single named bound
/// to assert against, not to be consulted at runtime.
#[cfg(test)]
pub const ROUND_TRIP_TOLERANCE_NS: u128 = 75;

/// Forward direction: the 27 MHz PCR value an epoch-locked master clock
/// generates for a given UNIX-epoch instant.
///
/// Mirrors `PtpMasterClock::now_27mhz` exactly, including doing the ×27 in
/// `u128` — absolute epoch nanoseconds are ~1.8e18 in 2026, and
/// `1.8e18 × 27 ≈ 4.9e19` overflows `u64::MAX` (1.8e19). A `u64` multiply
/// here would pin the result to a constant and freeze the derived anchor.
///
/// Test-only. Edges never run this direction — a PCR arrives on the wire
/// already stamped, and the only thing the emitter needs is the inverse.
/// It exists so the tests can generate a PCR for a chosen instant and
/// assert the round trip.
#[cfg(test)]
#[inline]
pub fn pcr_27mhz_from_unix_ns(unix_ns: u128) -> u64 {
    ((unix_ns * 27 / 1000) % (PCR_MODULUS_27MHZ as u128)) as u64
}

/// Inverse direction: recover the UNIX-epoch instant that an epoch-locked
/// master clock would have stamped with `pcr_27mhz`.
///
/// `pcr_27mhz` is modular, so the true instant is only determined up to a
/// multiple of `PCR_MODULUS_NS`. `near_unix_ns` resolves that: we return
/// whichever candidate lies closest to it. Because the ambiguity period is
/// ~26.5 h and the inputs disagree by milliseconds at worst, the choice is
/// never marginal.
///
/// Resolution is one 27 MHz tick (~37 ns) — five orders of magnitude finer
/// than the ±100 ms this feature targets. See `ROUND_TRIP_TOLERANCE_NS`.
///
/// # Why the reduction happens in tick space
///
/// The obvious implementation reduces `near_unix_ns` modulo
/// `PCR_MODULUS_NS` and adds the PCR's nanosecond phase. That is subtly
/// wrong: `PCR_MODULUS_NS` truncates a repeating fraction, so each elapsed
/// period contributes ~0.89 ns of error and a 2026 timestamp lands ~16 µs
/// off. Ticks are the domain where the modulus is exact, so the reduction
/// is done there and the conversion to nanoseconds happens once, at the
/// end.
pub fn unix_ns_from_pcr_27mhz(pcr_27mhz: u64, near_unix_ns: u128) -> u128 {
    let modulus = PCR_MODULUS_27MHZ as u128;
    // Reduce in exact 27 MHz tick space — see the note above.
    let near_ticks = near_unix_ns * 27 / 1000;
    // Start of the wrap period `near_ticks` currently sits in.
    let period_start = near_ticks - (near_ticks % modulus);
    let candidate = period_start + (pcr_27mhz as u128 % modulus);

    // Three candidates cover every case: the current period, and one
    // period either side (the PCR may have wrapped just before or just
    // after `near_ticks` did). Pick the nearest.
    let prev = candidate.saturating_sub(modulus);
    let next = candidate + modulus;
    let dist = |a: u128| a.abs_diff(near_ticks);

    let mut best = candidate;
    let mut best_dist = dist(candidate);
    for c in [prev, next] {
        let d = dist(c);
        if d < best_dist {
            best = c;
            best_dist = d;
        }
    }
    // Single tick→ns conversion, applied identically on every node.
    best * 1000 / 27
}

/// Measured TAI − CLOCK_REALTIME offset in nanoseconds.
///
/// `realtime_unix_ns` and `tai_ns` must be sampled from the same moment.
/// Returns a signed delta so callers can convert a CLOCK_REALTIME-derived
/// instant onto the CLOCK_TAI timeline `wire_emit` schedules against.
///
/// On a host with leap-second data loaded this is +37 s (2026); on one
/// without, CLOCK_TAI aliases CLOCK_REALTIME and this is 0. Both are
/// self-consistent — what matters is that the same measurement is applied
/// on the way in and the way out.
#[inline]
pub fn tai_minus_realtime_ns(tai_ns: u64, realtime_unix_ns: u128) -> i64 {
    (tai_ns as i128 - realtime_unix_ns as i128) as i64
}

/// Single-shot convenience form: given a PCR value, produce the CLOCK_TAI
/// instant at which the carrying datagram should hit the wire, measuring
/// the TAI−realtime skew from the two clock readings passed in.
///
/// - `pcr_27mhz` — PCR parsed from the datagram.
/// - `now_tai_ns` — the emitter's current CLOCK_TAI reading.
/// - `realtime_unix_ns` — CLOCK_REALTIME sampled at the same moment.
/// - `egress_offset_ns` — the operator's group-wide headroom constant.
///
/// The returned instant is on the same timeline as `now_tai_ns`, so the
/// caller can compare and sleep against it directly. It may legitimately
/// be in the past when the node cannot meet the configured offset — the
/// caller treats that as the deficit signal rather than clamping silently.
///
/// **Test-only, deliberately.** It is correct only when the two clock
/// readings are simultaneous, which is exactly what the emitter cannot
/// promise: `wire_emit` reads the clock once and reuses it across a whole
/// batch, so the skew derived here would absorb the batch latency and
/// shift every target by a load-dependent amount. Production therefore
/// calls [`target_tai_ns`] with a skew sampled separately on a slow timer
/// (see `wire_emit::refresh_epoch_skew`). Gating this to tests keeps that
/// distinction structural instead of advisory.
#[cfg(test)]
pub fn target_tai_ns_for_pcr(
    pcr_27mhz: u64,
    now_tai_ns: u64,
    realtime_unix_ns: u128,
    egress_offset_ns: u64,
) -> i128 {
    target_tai_ns(
        pcr_27mhz,
        now_tai_ns,
        tai_minus_realtime_ns(now_tai_ns, realtime_unix_ns),
        egress_offset_ns,
    )
}

/// The core derivation, taking the TAI−realtime skew as an explicit
/// parameter rather than re-measuring it.
///
/// # Why the skew is a parameter
///
/// Measuring it from the caller's `now_tai_ns` against a freshly-sampled
/// `SystemTime::now()` only works if the two readings are simultaneous.
/// They are not: `wire_emit` reads the clock once and then processes a
/// whole batch of datagrams against that reading, so `now_tai_ns` is
/// routinely stale by however long the batch takes. A stale reading would
/// be mistaken for clock skew and shifted straight into the target —
/// silently, and by an amount that varies with load. Callers therefore
/// sample both clocks together, once, and pass the resulting skew in.
///
/// Given a fixed skew the result is **independent of `now_tai_ns`**, which
/// is the whole point: the target must be a function of the PCR alone.
/// `now_tai_ns` is used only to pick the right wrap period, where being
/// milliseconds stale is irrelevant against a ~26.5 h ambiguity window.
pub fn target_tai_ns(
    pcr_27mhz: u64,
    now_tai_ns: u64,
    tai_minus_realtime_ns: i64,
    egress_offset_ns: u64,
) -> i128 {
    let skew = tai_minus_realtime_ns as i128;
    // Disambiguate against *realtime* — that is the clock the PCR was
    // generated from, so it is the one whose wrap phase matches. Derived
    // from the caller's TAI reading rather than sampled separately, so
    // the two can never disagree.
    let realtime_near = (now_tai_ns as i128 - skew).max(0) as u128;
    let generated_at = unix_ns_from_pcr_27mhz(pcr_27mhz, realtime_near) as i128;
    generated_at + skew + egress_offset_ns as i128
}

/// A shared label for the source's PCR timeline: "source PCR value
/// `pcr_27mhz` denotes wall instant `unix_ns`".
///
/// # Why this exists
///
/// The closed form above ([`unix_ns_from_pcr_27mhz`]) assumes the PCR it
/// is handed was generated *from* the UNIX epoch. That is true only when
/// the stream's originator is itself epoch-locked. For an ordinary
/// contribution feed the source PCR is free-running with an arbitrary
/// phase, so there is no self-derivable mapping onto wall time — and
/// critically, **any mapping a node derives from its own observations is
/// node-variant**, because every observation is timestamped on arrival
/// and therefore carries that node's ingest latency.
///
/// The phase must therefore come from outside the node: one scalar pair,
/// minted once and distributed byte-identically to every member of an
/// alignment group. Nothing requires `unix_ns` to be the instant the
/// encoder actually originated that content — it is only a *label*. The
/// manager mints it from the **slowest** member's observed arrival plus a
/// margin, which makes the required egress dwell equal to the inter-node
/// latency *spread* rather than the absolute end-to-end latency. That
/// distinction is what keeps the dwell clear of the wire-emit residence
/// cap on a WAN contribution path; see `docs/clocking.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceAnchor {
    /// Reference point on the source's 27 MHz PCR timeline.
    pub pcr_27mhz: u64,
    /// Wall instant (UNIX epoch nanoseconds, CLOCK_REALTIME domain) that
    /// the group agrees `pcr_27mhz` denotes.
    pub unix_ns: i128,
    /// Monotonically increasing mint counter. Carried on telemetry so an
    /// operator can confirm every member is on the same generation — two
    /// members on different generations are misaligned by the difference
    /// between the two anchors.
    pub generation: u32,
    /// Source-PCR value at (or after) which this anchor takes effect.
    ///
    /// Gating the swap on a **content** coordinate rather than a wall
    /// instant is what makes a re-mint hitless: both nodes switch on the
    /// same *packet*, not at the same moment on their own clocks. Without
    /// it a re-mint opens a divergence window equal to the full anchor
    /// step. `None` = effective immediately (first arm).
    pub effective_from_pcr: Option<u64>,
}

impl SourceAnchor {
    /// Is this anchor in force for a datagram carrying `pcr_27mhz`?
    ///
    /// Compared with [`signed_pcr_delta`] rather than `>=` so a PCR that
    /// has wrapped past the trigger still counts as "at or after" it.
    #[inline]
    pub fn is_effective_for(&self, pcr_27mhz: u64) -> bool {
        match self.effective_from_pcr {
            None => true,
            Some(from) => signed_pcr_delta(pcr_27mhz, from) >= 0,
        }
    }
}

/// Shortest signed distance from `b` to `a` on the modular PCR timeline,
/// in 27 MHz ticks — i.e. the representative of `a - b` nearest zero.
///
/// # Why not `a.wrapping_sub(b) as i64`
///
/// The PCR modulus is `(1<<33) × 300 ≈ 2.58e12`, which is **not** a power
/// of two, so the natural `u64` wrap is not congruent to it. Reducing
/// with `wrapping_sub` and casting silently returns a value that is wrong
/// by `2^64 mod MODULUS` once the arithmetic crosses a `u64` boundary,
/// and near the PCR wrap it produces a ~4 h 35 m error — large enough to
/// look like a plausible timeline and small enough to be missed.
/// `rem_euclid` against the real modulus is correct at every input.
#[inline]
pub fn signed_pcr_delta(a: u64, b: u64) -> i64 {
    let m = PCR_MODULUS_27MHZ as i128;
    let d = (a as i128 - b as i128).rem_euclid(m);
    if d > m / 2 { (d - m) as i64 } else { d as i64 }
}

/// Anchored inversion: the wall instant a source PCR denotes, under a
/// group-shared [`SourceAnchor`].
///
/// Unlike [`unix_ns_from_pcr_27mhz`] this reads **no local clock at all** —
/// not even to disambiguate the wrap, because the anchor already pins the
/// phase. The result is a pure function of `(pcr_27mhz, anchor)`, both of
/// which are identical on every member. That is the whole alignment
/// property, and stating it this way makes it checkable by inspection.
#[inline]
pub fn unix_ns_from_pcr_anchored(pcr_27mhz: u64, anchor: &SourceAnchor) -> i128 {
    let delta_ticks = signed_pcr_delta(pcr_27mhz, anchor.pcr_27mhz) as i128;
    // Truncating division, applied identically on every node — so the
    // sub-tick residue is common-mode and cancels between members.
    anchor.unix_ns + delta_ticks * 1000 / 27
}

/// Release instant on the emitter's CLOCK_TAI timeline for a datagram
/// carrying `pcr_27mhz`, under a group-shared anchor.
///
/// # Heterogeneous leap-second data is safe
///
/// `tai_minus_realtime_ns` is a *local* quantity: 37 s on a host with
/// leap-second data loaded, 0 on one without. It cancels rather than
/// diverging — each node converts the same shared CLOCK_REALTIME instant
/// into its own CLOCK_TAI domain, so both fire at the same real moment.
/// A group may therefore mix hosts with and without leap data.
#[inline]
pub fn target_tai_ns_anchored(
    pcr_27mhz: u64,
    tai_minus_realtime_ns: i64,
    egress_offset_ns: u64,
    anchor: &SourceAnchor,
) -> i128 {
    unix_ns_from_pcr_anchored(pcr_27mhz, anchor)
        + tai_minus_realtime_ns as i128
        + egress_offset_ns as i128
}

/// Lock-free, hot-swappable home for the group anchor.
///
/// A re-mint must reach a running wire thread without restarting the
/// output — restarting drops the dwell and glitches the feed, which is a
/// poor answer to "the manager refreshed a scalar". The wire thread runs
/// SCHED_FIFO, so a `Mutex` is the wrong tool even at 25-50 reads/s: if the
/// writer is preempted while holding it, the real-time thread blocks on a
/// lower-priority task.
///
/// A seqlock fits exactly: writes are rare and single-writer, reads are
/// frequent and wait-free. The writer bumps `seq` to odd, publishes, bumps
/// to even; a reader that sees an odd or changed `seq` retries.
#[derive(Debug, Default)]
pub struct EpochAnchorCell {
    /// Serialises writers. **Readers never take it** — [`EpochAnchorCell::load`]
    /// stays wait-free, which is the whole point of the seqlock.
    ///
    /// The doc above used to say this cell was single-writer (the manager
    /// command handler). It is not, and has not been for as long as three
    /// separate writers have existed:
    ///
    /// - `set_epoch_anchor` → `store` (`manager/client.rs:5484`)
    /// - the `{"clear": true}` withdrawal → `store_disarm` (`:5424`)
    /// - the spawn-time seed in `wire_emit::spawn_wire_emitter` (`:606`)
    ///
    /// and the WS reader `tokio::spawn`s a task **per message**
    /// (`manager/client.rs:739`), so two commands for one output run
    /// concurrently. Without serialisation `seq.load` + `seq.store` is a
    /// non-atomic read-modify-write: two writers can each read `s`, each
    /// write `s + 1`, then each write `s + 2`, leaving `seq` *even* while
    /// their payload stores interleaved — so a reader validates a torn
    /// anchor instead of retrying. No memory barrier can fix that; it needs
    /// mutual exclusion.
    ///
    /// Writes are rare (an alignment-group mint or withdrawal) and the
    /// critical section is a handful of atomic stores with no `.await`, so
    /// holding a `std::sync::Mutex` here costs nothing and cannot park the
    /// SCHED_FIFO wire thread — that thread only ever reads.
    write_lock: std::sync::Mutex<()>,
    seq: std::sync::atomic::AtomicU64,
    pcr_27mhz: std::sync::atomic::AtomicU64,
    unix_ns: std::sync::atomic::AtomicI64,
    generation: std::sync::atomic::AtomicU32,
    /// `u64::MAX` encodes `None` — a real trigger can never be that value,
    /// since PCR is bounded by [`PCR_MODULUS_27MHZ`].
    effective_from_pcr: std::sync::atomic::AtomicU64,
    /// False until the first anchor is published.
    armed: std::sync::atomic::AtomicBool,
}

const NO_TRIGGER: u64 = u64::MAX;

/// Generation reserved to mean "the group withdrew its anchor".
///
/// The manager mints from 1 (`previous.generation + 1`, starting at 0+1), so
/// 0 can never name a real anchor and is free to carry the disarm.
///
/// A disarm has to be its own signal rather than "stop publishing": the cell
/// is level-triggered, so a member that simply stopped hearing from the
/// manager would hold its last anchor forever. That is right for a control-
/// plane outage — a running group must not fall apart because the manager
/// restarted — and wrong for a deliberate withdrawal, which is why the two
/// are distinguished here rather than by a timeout.
///
/// # Why a sentinel generation and not `armed = false`
///
/// Clearing `armed` looks like the obvious encoding, and it is the wrong
/// one: [`EpochAnchorCell::load`] already returns `None` for a torn read
/// (the bounded retry giving up), so "withdrawn" would be indistinguishable
/// from "the writer was preempted mid-update". The reader would then drop a
/// live anchor — taking a healthy member off the group timeline mid-air —
/// on a transient it is specifically designed to tolerate. A published
/// sentinel keeps `None` meaning exactly one thing: *no information this
/// datagram, keep what you have*.
pub const DISARM_GENERATION: u32 = 0;

impl EpochAnchorCell {
    pub fn new() -> Self {
        Self::default()
    }

    /// Withdraw the group anchor.
    ///
    /// The emitter drops back to the closed-form inversion (still covered by
    /// the plausibility gate) and — the point of the exercise — resumes
    /// publishing mint observations, so the manager can re-mint from fresh
    /// arrivals. Without this a re-mint re-derives the *same* anchor from
    /// each member's frozen first-engagement pair and egress phase never
    /// moves, which is exactly the drift it was invoked to correct.
    pub fn store_disarm(&self) {
        self.store(SourceAnchor {
            pcr_27mhz: 0,
            unix_ns: 0,
            generation: DISARM_GENERATION,
            effective_from_pcr: None,
        });
    }

    /// Publish a new anchor.
    ///
    /// Writers are serialised by `write_lock` (see the field for why the
    /// former single-writer claim was wrong). Readers are unaffected.
    pub fn store(&self, a: SourceAnchor) {
        use std::sync::atomic::Ordering::{Relaxed, Release};
        use std::sync::atomic::fence;
        // Poisoning carries no meaning here: the guarded data is the atomics
        // below, and a panic between the odd and even bump leaves `seq` odd,
        // which readers already treat as "write in progress" and retry past.
        let _writer = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let s = self.seq.load(Relaxed);
        self.seq.store(s.wrapping_add(1), Relaxed); // odd: write in progress
        // The odd bump must be visible BEFORE any payload store. A `Release`
        // *store* on `seq` would not give that: release ordering constrains
        // operations sequenced *before* the store, and says nothing about the
        // ones after it, so the payload stores below were free to be hoisted
        // above the bump — letting a reader observe an even `seq` alongside
        // half-updated fields and accept it. A `Release` fence here is the
        // constraint that was actually wanted.
        fence(Release);
        self.pcr_27mhz.store(a.pcr_27mhz, Relaxed);
        self.unix_ns.store(a.unix_ns as i64, Relaxed);
        self.generation.store(a.generation, Relaxed);
        self.effective_from_pcr
            .store(a.effective_from_pcr.unwrap_or(NO_TRIGGER), Relaxed);
        self.armed.store(true, Relaxed);
        // Release: every payload store above happens-before a reader's
        // matching acquire of this value.
        self.seq.store(s.wrapping_add(2), Release); // even: consistent
    }

    /// Wait-free read. `None` until an anchor has been published.
    ///
    /// Bounded retry rather than an unbounded spin: the wire thread must
    /// never be parked by a writer that was preempted mid-update. Falling
    /// back to "no anchor for this datagram" is safe — the caller then uses
    /// the closed form for one packet and the plausibility gate still
    /// covers it.
    pub fn load(&self) -> Option<SourceAnchor> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        if !self.armed.load(Relaxed) {
            return None;
        }
        for _ in 0..4 {
            let before = self.seq.load(Acquire);
            if !before.is_multiple_of(2) {
                continue;
            }
            let a = SourceAnchor {
                pcr_27mhz: self.pcr_27mhz.load(Relaxed),
                unix_ns: self.unix_ns.load(Relaxed) as i128,
                generation: self.generation.load(Relaxed),
                effective_from_pcr: match self.effective_from_pcr.load(Relaxed) {
                    NO_TRIGGER => None,
                    v => Some(v),
                },
            };
            // Mirror image of the writer's fence, and needed for the mirror
            // reason: the payload loads above must not sink BELOW the
            // validating re-read of `seq`. An `Acquire` *load* orders what
            // follows it, not what precedes it, so `seq.load(Acquire)` alone
            // left the payload reads free to move after it — and a validation
            // that can be reordered after the data it validates is not a
            // validation. With the fence, the re-read only needs `Relaxed`.
            std::sync::atomic::fence(Acquire);
            if self.seq.load(Relaxed) == before {
                return Some(a);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concurrent writers must never let a reader validate a mixture of two
    /// anchors.
    ///
    /// Every field published here is derived from one generation number, so
    /// any blend of two writers' anchors is detectable by arithmetic rather
    /// than by timing. The assertion is an invariant, not a race outcome, so
    /// this test cannot fail spuriously when the primitive is correct — it
    /// simply stops catching regressions if the window closes.
    ///
    /// It targets the defect the `Release`/`Acquire` fences alone do NOT
    /// address: `seq.load` + `seq.store` is a non-atomic read-modify-write,
    /// so two unserialised writers can both leave `seq` even while their
    /// payload stores interleave.
    #[test]
    fn concurrent_writers_never_publish_a_torn_anchor() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let cell = Arc::new(EpochAnchorCell::new());
        let stop = Arc::new(AtomicBool::new(false));

        let writers: Vec<_> = (0..3u32)
            .map(|w| {
                let cell = Arc::clone(&cell);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    // Disjoint per-writer ranges, and never DISARM_GENERATION.
                    let mut g = w * 1_000_000 + 1;
                    while !stop.load(Ordering::Relaxed) {
                        cell.store(SourceAnchor {
                            pcr_27mhz: u64::from(g),
                            unix_ns: i128::from(g) * 1_000,
                            generation: g,
                            effective_from_pcr: Some(u64::from(g) * 7),
                        });
                        g += 1;
                    }
                })
            })
            .collect();

        let mut seen = 0u64;
        let mut torn = 0u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            if let Some(a) = cell.load() {
                seen += 1;
                let g = a.generation;
                if a.pcr_27mhz != u64::from(g)
                    || a.unix_ns != i128::from(g) * 1_000
                    || a.effective_from_pcr != Some(u64::from(g) * 7)
                {
                    torn += 1;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        for w in writers {
            w.join().expect("writer thread");
        }

        assert!(seen > 0, "reader never observed a published anchor");
        assert_eq!(torn, 0, "observed {torn} torn anchors across {seen} reads");
    }

    /// A representative 2026 wall instant, well away from any wrap
    /// boundary.
    const T2026_NS: u128 = 1_785_000_000_000_000_000;

    #[test]
    fn round_trips_at_a_representative_instant() {
        let pcr = pcr_27mhz_from_unix_ns(T2026_NS);
        let back = unix_ns_from_pcr_27mhz(pcr, T2026_NS);
        assert!(
            T2026_NS.abs_diff(back) <= ROUND_TRIP_TOLERANCE_NS,
            "round-trip drifted: {T2026_NS} -> {pcr} -> {back}"
        );
    }

    #[test]
    fn round_trips_across_a_range_of_instants() {
        // Walk a full wrap period in irregular steps so we cross the
        // boundary at an arbitrary phase rather than a convenient one.
        let step = PCR_MODULUS_NS / 97;
        for i in 0..200u128 {
            let t = T2026_NS + i * step;
            let pcr = pcr_27mhz_from_unix_ns(t);
            let back = unix_ns_from_pcr_27mhz(pcr, t);
            assert!(
                t.abs_diff(back) <= ROUND_TRIP_TOLERANCE_NS,
                "failed at i={i}: {t} -> {pcr} -> {back}"
            );
        }
    }

    #[test]
    fn resolves_the_wrap_from_either_side() {
        // Put the reference instant a hair *before* a wrap boundary and
        // the PCR a hair after it (and vice versa). A naive
        // same-period-only inversion picks a value ~26.5 h wrong here.
        let boundary = T2026_NS - (T2026_NS % PCR_MODULUS_NS) + PCR_MODULUS_NS;

        // PCR generated 5 ms after the boundary, asked about 5 ms before.
        let after = boundary + 5_000_000;
        let pcr_after = pcr_27mhz_from_unix_ns(after);
        let got = unix_ns_from_pcr_27mhz(pcr_after, boundary - 5_000_000);
        assert!(
            after.abs_diff(got) <= ROUND_TRIP_TOLERANCE_NS,
            "forward wrap mis-resolved: expected ~{after}, got {got}"
        );

        // PCR generated 5 ms before the boundary, asked about 5 ms after.
        let before = boundary - 5_000_000;
        let pcr_before = pcr_27mhz_from_unix_ns(before);
        let got = unix_ns_from_pcr_27mhz(pcr_before, boundary + 5_000_000);
        assert!(
            before.abs_diff(got) <= ROUND_TRIP_TOLERANCE_NS,
            "backward wrap mis-resolved: expected ~{before}, got {got}"
        );
    }

    /// The property the whole feature rests on: two nodes with different
    /// local reference readings must derive the *same* absolute instant
    /// from the same PCR. Any divergence here is inter-node misalignment.
    #[test]
    fn two_nodes_derive_the_same_instant_from_the_same_pcr() {
        let pcr = pcr_27mhz_from_unix_ns(T2026_NS);
        // Node A is 8 ms ahead of node B — a generous NTP disagreement.
        let a = unix_ns_from_pcr_27mhz(pcr, T2026_NS + 8_000_000);
        let b = unix_ns_from_pcr_27mhz(pcr, T2026_NS - 8_000_000);
        assert_eq!(a, b, "nodes disagreed on the instant a PCR maps to");
    }

    #[test]
    fn tai_offset_is_applied_and_signed_correctly() {
        // Host with leap-second data: CLOCK_TAI runs 37 s ahead.
        const TAI_SKEW_NS: u64 = 37_000_000_000;
        let realtime = T2026_NS;
        let tai = (realtime as u64).wrapping_add(TAI_SKEW_NS);
        assert_eq!(tai_minus_realtime_ns(tai, realtime), TAI_SKEW_NS as i64);

        let pcr = pcr_27mhz_from_unix_ns(realtime);
        let target = target_tai_ns_for_pcr(pcr, tai, realtime, 0);
        // The PCR was generated *now*, so with a zero offset the target
        // should land on the current TAI reading.
        assert!(
            (target - tai as i128).unsigned_abs() <= ROUND_TRIP_TOLERANCE_NS,
            "target ignored the TAI offset: target={target} now_tai={tai}"
        );
    }

    #[test]
    fn egress_offset_shifts_the_target_by_exactly_that_much() {
        let realtime = T2026_NS;
        let tai = realtime as u64;
        let pcr = pcr_27mhz_from_unix_ns(realtime);
        let base = target_tai_ns_for_pcr(pcr, tai, realtime, 0);
        let offset_ns = 250_000_000u64;
        let shifted = target_tai_ns_for_pcr(pcr, tai, realtime, offset_ns);
        assert_eq!(shifted - base, offset_ns as i128);
    }

    /// A node whose processing path is slower produces a target in the
    /// past for the *same* PCR — the deficit signal. Critically, both
    /// nodes still name the same absolute instant; only their ability to
    /// meet it differs.
    #[test]
    fn a_late_node_yields_a_past_target_not_a_shifted_one() {
        let generated = T2026_NS;
        let pcr = pcr_27mhz_from_unix_ns(generated);
        // This node only got the datagram to the emitter 400 ms later.
        let now_realtime = generated + 400_000_000;
        let now_tai = now_realtime as u64;
        // Offset budget is only 100 ms, so it cannot be met.
        let target = target_tai_ns_for_pcr(pcr, now_tai, now_realtime, 100_000_000);
        assert!(
            target < now_tai as i128,
            "expected a past target (deficit), got {target} vs now {now_tai}"
        );
        let deficit_ns = now_tai as i128 - target;
        assert!(
            (deficit_ns - 300_000_000).abs() < 1_000,
            "deficit should be ~300 ms, got {deficit_ns} ns"
        );
    }

    #[test]
    fn modulus_ns_matches_the_tick_modulus() {
        // Guards against someone "simplifying" one constant and not the
        // other. ~95_443 s.
        assert_eq!(PCR_MODULUS_NS, (PCR_MODULUS_27MHZ as u128) * 1000 / 27);
        assert!((95_442_000_000_000..95_444_000_000_000).contains(&(PCR_MODULUS_NS as u64)));
    }

    // ── Anchored (group-shared) inversion ───────────────────────────────

    fn anchor_at(pcr: u64, unix_ns: i128) -> SourceAnchor {
        SourceAnchor { pcr_27mhz: pcr, unix_ns, generation: 1, effective_from_pcr: None }
    }

    #[test]
    fn signed_delta_is_zero_centred_and_wraps_both_ways() {
        assert_eq!(signed_pcr_delta(100, 100), 0);
        assert_eq!(signed_pcr_delta(127, 100), 27);
        assert_eq!(signed_pcr_delta(100, 127), -27);
        // Across the wrap: 10 ticks past the modulus is +20 from 27 below it.
        let m = PCR_MODULUS_27MHZ;
        assert_eq!(signed_pcr_delta(10, m - 10), 20);
        assert_eq!(signed_pcr_delta(m - 10, 10), -20);
    }

    /// The defect this function exists to avoid: `wrapping_sub` is not
    /// congruent to the PCR modulus (which is not a power of two), so the
    /// naive form is wrong by ~4 h 35 m at the wrap antipode.
    #[test]
    fn signed_delta_beats_the_naive_wrapping_sub_at_the_antipode() {
        let m = PCR_MODULUS_27MHZ;
        let a = 5u64;
        let b = m - 5;
        assert_eq!(signed_pcr_delta(a, b), 10, "true distance is 10 ticks");
        let naive = a.wrapping_sub(b) as i64;
        assert_ne!(naive, 10, "the naive form must differ — that is the bug");
    }

    /// The alignment property, stated directly: two nodes holding the same
    /// anchor derive the *same* instant for the same PCR, and nothing
    /// about either node enters the calculation.
    #[test]
    fn anchored_targets_are_identical_across_nodes_and_read_no_local_clock() {
        let a = anchor_at(1_000_000, T2026_NS as i128);
        let pcr = 1_000_000 + 27_000_000; // one second later on the source
        // Node A has leap-second data (TAI-UTC = 37 s), node B does not.
        let t_a = target_tai_ns_anchored(pcr, 37_000_000_000, 200_000_000, &a);
        let t_b = target_tai_ns_anchored(pcr, 0, 200_000_000, &a);
        // Each lands on its own TAI timeline, but both denote the same
        // CLOCK_REALTIME instant — the skew cancels.
        assert_eq!(t_a - 37_000_000_000, t_b);
        // And that instant is anchor + 1 s of source time + the offset.
        assert_eq!(t_b, T2026_NS as i128 + 1_000_000_000 + 200_000_000);
    }

    #[test]
    fn anchored_inversion_round_trips_forwards_and_backwards() {
        let a = anchor_at(500_000_000, T2026_NS as i128);
        for secs in [-3600i64, -60, -1, 0, 1, 60, 3600] {
            let pcr = (500_000_000i64 + secs * 27_000_000)
                .rem_euclid(PCR_MODULUS_27MHZ as i64) as u64;
            let got = unix_ns_from_pcr_anchored(pcr, &a);
            let want = T2026_NS as i128 + secs as i128 * 1_000_000_000;
            assert!(
                (got - want).abs() <= 40,
                "secs={secs}: got {got} want {want}"
            );
        }
    }

    /// A re-mint must switch both nodes on the same *packet*. Gating on a
    /// content coordinate is what makes that true regardless of when each
    /// node received the new anchor.
    #[test]
    fn effective_from_pcr_gates_on_content_not_wall_time() {
        let mut next = anchor_at(2_000_000, T2026_NS as i128);
        next.effective_from_pcr = Some(2_000_000);
        assert!(!next.is_effective_for(1_999_999), "before the trigger");
        assert!(next.is_effective_for(2_000_000), "exactly at the trigger");
        assert!(next.is_effective_for(2_000_001), "after the trigger");
        // Still correct once the PCR has wrapped past the trigger.
        assert!(next.is_effective_for(2_000_000 + 27_000_000));
    }

    /// A withdrawal is a published generation-0 anchor, not an absent one.
    ///
    /// The cell is level-triggered, so "stop publishing" cannot mean
    /// "disarm" — a member must hold its anchor through a manager restart.
    /// The reader therefore has to be able to *see* the withdrawal.
    #[test]
    fn a_withdrawal_is_readable_and_distinct_from_never_armed() {
        let cell = EpochAnchorCell::new();
        assert!(cell.load().is_none(), "never armed reads as absent");

        cell.store(SourceAnchor {
            pcr_27mhz: 4_242_424_242,
            unix_ns: 1_786_000_000_000_000_000,
            generation: 3,
            effective_from_pcr: None,
        });
        assert_eq!(cell.load().expect("armed").generation, 3);

        cell.store_disarm();
        let withdrawn = cell.load().expect("a withdrawal is published, not absent");
        assert_eq!(withdrawn.generation, DISARM_GENERATION);
    }

    /// The sentinel must never collide with a real anchor. The manager
    /// mints `previous + 1` starting from 0, so the first real generation
    /// is 1 and 0 is always free. (Pinned here because the emitter treats 0 as
    /// "drop the anchor" — a real anchor arriving as 0 would be read as a
    /// withdrawal and silently unalign the member.)
    #[test]
    fn disarm_generation_is_below_the_first_minted_generation() {
        let first_minted = 0u32 + 1;
        assert!(DISARM_GENERATION < first_minted);
        assert_eq!(DISARM_GENERATION, 0);
    }
}
