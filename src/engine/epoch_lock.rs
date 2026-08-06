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
    let dist = |a: u128| if a > near_ticks { a - near_ticks } else { near_ticks - a };

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
