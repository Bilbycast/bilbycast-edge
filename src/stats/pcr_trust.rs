// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-output PCR accuracy trust metric — Phase 8 of the PID-bus feature.
//!
//! Measures `|observed_Δ_us − expected_Δ_us|` at the egress point for every
//! PCR-bearing packet the output forwards. Expected Δ is derived from the
//! 27 MHz PCR timestamp difference between consecutive PCRs; observed Δ is
//! the wall-clock difference between the send timestamps. A healthy
//! assembler carries PCR forward byte-for-byte, so the drift should stay
//! within a small fraction of the PCR-to-PCR interval (broadcast typical:
//! 40 ms intervals, p95 of |drift| well under 1 ms).
//!
//! **Why a rotating reservoir, not streaming quantiles**: pro broadcast
//! operations care about *recent*, *exact* percentiles. A PCR spike from
//! 20 minutes ago is noise; what matters is "in the last N samples, is p99
//! still under budget?". A fixed-size rotating reservoir gives exact
//! percentiles over the last 4096 PCR pairs (~2.7 minutes at 25 Hz) and
//! naturally ages out old data. Streaming P² (Jain/Chlamtac) saves a few
//! KB but introduces tail-quantile approximation error that makes incident
//! postmortems harder — operators and dashboards disagreeing on the
//! numbers has burned us before.
//!
//! **Performance invariant**: the sampler runs only on PCR-bearing TS
//! packets (≤ 50 Hz on worst-case MPEG-TS; 25 Hz typical). Each update is
//! one Mutex lock + one VecDeque push-back (O(1) amortised when full —
//! pop_front evicts the oldest). No allocation on the hot path once the
//! reservoir is initialised. Snapshot takes one lock, clones into a Vec
//! (<= 32 KB), sorts, and computes percentiles — bounded work.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::stats::models::PcrTrustStats;

/// Maximum samples retained. 4096 PCR pairs ≈ 2.7 minutes at 25 Hz
/// (typical broadcast 40 ms PCR interval), or ≈ 1.3 minutes at 50 Hz
/// (aggressive 20 ms PCR interval). Sized to cover the operator's
/// typical "check the last minute or two of trust" window without
/// retaining stale history through config changes.
pub const PCR_TRUST_RESERVOIR_SIZE: usize = 4096;

/// Window used for the "recent" percentile field in the snapshot. The
/// snapshot exposes both a long-window p95 (over the full reservoir) and
/// a short-window p95 (over the last `WINDOW_SAMPLES`) so operators can
/// tell whether a spike is recent or baseline. 256 samples ≈ 10 s at
/// 25 Hz.
pub const PCR_TRUST_WINDOW_SAMPLES: usize = 256;

/// 33-bit PCR base × 300 — the full 27 MHz PCR modulus. Wraps every
/// ~95 hours. Used to reduce inputs before computing drift deltas.
pub const PCR_MODULUS_27MHZ: u64 = (1u64 << 33) * 300;

/// Samples with apparent ΔPCR or Δwall larger than this are treated as
/// straddling a PCR discontinuity and discarded — not "drift". Set to
/// 500 ms, comfortably above the 100 ms maximum normal PCR interval
/// (ETSI TR 101 290 Priority 2.3). Bigger gaps happen across keyframe
/// boundaries on some encoders, stream restarts, or sample-rate
/// transitions — none of which are trust-metric territory.
pub const PCR_MAX_SAMPLE_INTERVAL_US: u64 = 500_000;

/// Per-output PCR trust sampler. Holds the rotating reservoir and the
/// state needed to compute |observed − expected| on each PCR sample.
///
/// Thread-safety: the sampler's state is behind a single `Mutex`. The
/// only writer is the output's send task (one task per output), and
/// the only reader is the 1 Hz stats snapshot path — contention is
/// negligible.
pub struct PcrTrustSampler {
    state: Mutex<PcrTrustState>,
}

struct PcrTrustState {
    /// Rotating reservoir of absolute drifts in microseconds.
    /// The oldest sample is evicted when a new one pushes past capacity.
    samples: VecDeque<u64>,
    /// Last observed PCR in 27 MHz ticks; `None` until first sample.
    last_pcr_27mhz: Option<u64>,
    /// Wall-clock microseconds at which `last_pcr_27mhz` was recorded.
    last_sample_wall_us: Option<u64>,
    /// Cumulative total drift in microseconds — gives operators a "trend"
    /// signal that the reservoir alone can't expose (since it rolls over).
    cumulative_drift_us: u64,
    /// Cumulative sample count over the flow lifetime.
    cumulative_samples: u64,
}

impl PcrTrustSampler {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PcrTrustState {
                samples: VecDeque::with_capacity(PCR_TRUST_RESERVOIR_SIZE),
                last_pcr_27mhz: None,
                last_sample_wall_us: None,
                cumulative_drift_us: 0,
                cumulative_samples: 0,
            }),
        }
    }

    /// Record one PCR observation at egress. `pcr_27mhz` is the PCR base
    /// (90 kHz × 300 — the 27 MHz reconstructed from the adaptation
    /// field), `now_us` is the egress wall-clock timestamp.
    ///
    /// First call primes `last_pcr_27mhz` + `last_sample_wall_us` and
    /// produces no drift sample (no Δ available). Subsequent calls
    /// compute `|ΔPCR_µs − Δwall_µs|` and push the absolute drift into
    /// the reservoir.
    ///
    /// **Sample-skip rules** (important for clean percentiles):
    ///   - First call after startup just primes state.
    ///   - If the apparent ΔPCR or Δwall exceeds
    ///     [`PCR_MAX_SAMPLE_INTERVAL_US`], the sample is discarded and
    ///     state is reset to the current PCR/wall. This filters out
    ///     PCR discontinuities (keyframe gaps > threshold, stream
    ///     restarts, 33-bit wrap, or samples straddling flow idle).
    ///     The trust metric is meaningful only for adjacent
    ///     PCR-bearing packets within a normal PCR cadence (≤ 100 ms
    ///     per broadcast standard).
    ///
    /// PCR wrap-around (33-bit base × 300 → ≈ 95 hours) is handled via
    /// proper modular subtraction in the PCR space: a genuine wrap
    /// produces a small forward delta; an actual backward jump (a
    /// discontinuity) produces an implausibly large forward delta in
    /// the modular space and is filtered by the interval guard above.
    pub fn record(&self, pcr_27mhz: u64, now_us: u64) {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (prev_pcr, prev_wall) = match (state.last_pcr_27mhz, state.last_sample_wall_us) {
            (Some(p), Some(w)) => (p, w),
            _ => {
                state.last_pcr_27mhz = Some(pcr_27mhz);
                state.last_sample_wall_us = Some(now_us);
                return;
            }
        };

        // Reduce both values into the PCR modulus space before
        // subtracting. u64::wrapping_sub treats the full 64-bit range
        // as its modulus, which produces garbage when PCR-space wraps
        // or goes backwards — do the subtraction in PCR-space instead.
        let cur = pcr_27mhz % PCR_MODULUS_27MHZ;
        let prev = prev_pcr % PCR_MODULUS_27MHZ;
        let delta_pcr_ticks = if cur >= prev {
            cur - prev
        } else {
            PCR_MODULUS_27MHZ - prev + cur
        };
        // Convert ticks → µs: 27 MHz → 1 tick = 1/27 µs.
        let delta_pcr_us = delta_pcr_ticks / 27;
        let delta_wall_us = now_us.saturating_sub(prev_wall);

        // Guard against sample-straddling-discontinuity: skip and reset.
        if delta_pcr_us > PCR_MAX_SAMPLE_INTERVAL_US
            || delta_wall_us > PCR_MAX_SAMPLE_INTERVAL_US
        {
            state.last_pcr_27mhz = Some(pcr_27mhz);
            state.last_sample_wall_us = Some(now_us);
            return;
        }

        let drift_us = delta_pcr_us.abs_diff(delta_wall_us);

        if state.samples.len() >= PCR_TRUST_RESERVOIR_SIZE {
            state.samples.pop_front();
        }
        state.samples.push_back(drift_us);
        state.cumulative_drift_us = state.cumulative_drift_us.saturating_add(drift_us);
        state.cumulative_samples = state.cumulative_samples.saturating_add(1);
        state.last_pcr_27mhz = Some(pcr_27mhz);
        state.last_sample_wall_us = Some(now_us);
    }

    /// Take a point-in-time snapshot. Produces `None` when the reservoir
    /// is empty (the first PCR observation only primes state — no drift
    /// Δ available yet). The caller should omit `pcr_trust` from the
    /// output's wire shape in that case.
    pub fn snapshot(&self) -> Option<PcrTrustStats> {
        let state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.samples.is_empty() {
            return None;
        }
        let mut full: Vec<u64> = state.samples.iter().copied().collect();
        full.sort_unstable();
        let p50_us = percentile(&full, 50.0);
        let p95_us = percentile(&full, 95.0);
        let p99_us = percentile(&full, 99.0);
        let max_us = *full.last().unwrap_or(&0);

        // Short-window p95 over the last WINDOW_SAMPLES. Iterate the
        // deque's tail directly — avoids a second full clone.
        let window_len = full.len().min(PCR_TRUST_WINDOW_SAMPLES);
        let window_p95_us = if window_len >= 2 {
            let mut window: Vec<u64> = state
                .samples
                .iter()
                .rev()
                .take(window_len)
                .copied()
                .collect();
            window.sort_unstable();
            percentile(&window, 95.0)
        } else {
            p95_us
        };

        let avg_us = if state.cumulative_samples > 0 {
            state.cumulative_drift_us / state.cumulative_samples
        } else {
            0
        };

        Some(PcrTrustStats {
            samples: full.len() as u64,
            cumulative_samples: state.cumulative_samples,
            avg_us,
            p50_us,
            p95_us,
            p99_us,
            max_us,
            window_samples: window_len as u64,
            window_p95_us,
        })
    }
}

impl Default for PcrTrustSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Linear-interpolated percentile over a sorted slice. Returns 0 when
/// the slice is empty. `q` is in [0, 100].
fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (q / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = rank - lo as f64;
    let a = sorted[lo] as f64;
    let b = sorted[hi] as f64;
    (a + (b - a) * frac).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_primes_state_but_emits_nothing() {
        let s = PcrTrustSampler::new();
        s.record(10_000_000, 1_000_000);
        assert!(s.snapshot().is_none(), "first sample should not produce a snapshot");
    }

    #[test]
    fn two_aligned_samples_produce_zero_drift() {
        let s = PcrTrustSampler::new();
        // 40 ms in 27 MHz ticks = 40_000 × 27 = 1_080_000 ticks.
        s.record(0, 0);
        s.record(1_080_000, 40_000);
        let snap = s.snapshot().expect("have a sample");
        assert_eq!(snap.samples, 1);
        assert_eq!(snap.p50_us, 0, "aligned PCR + wall-clock → zero drift");
        assert_eq!(snap.max_us, 0);
    }

    #[test]
    fn mismatched_interval_produces_exact_drift() {
        let s = PcrTrustSampler::new();
        // Expected Δ = 40 ms; actual wall Δ = 42 ms → 2000 µs of drift.
        s.record(0, 0);
        s.record(1_080_000, 42_000);
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.p50_us, 2000);
        assert_eq!(snap.max_us, 2000);
    }

    #[test]
    fn reservoir_caps_at_configured_size() {
        let s = PcrTrustSampler::new();
        for i in 0..(PCR_TRUST_RESERVOIR_SIZE as u64 + 500) {
            s.record(i * 1_080_000, i * 40_000);
        }
        let snap = s.snapshot().unwrap();
        assert!(
            snap.samples as usize <= PCR_TRUST_RESERVOIR_SIZE,
            "reservoir grew past cap: {}",
            snap.samples
        );
        assert!(
            snap.cumulative_samples > PCR_TRUST_RESERVOIR_SIZE as u64,
            "cumulative should keep counting past the cap: {}",
            snap.cumulative_samples
        );
    }

    #[test]
    fn percentiles_are_ordered_correctly() {
        let s = PcrTrustSampler::new();
        // Feed 1000 samples with varying drift: 99 with 0 drift, 1 with 100 ms drift.
        // p99 should stay at 0, max should climb to ~100 ms.
        for i in 0..1000u64 {
            let wall = i * 40_000 + if i == 999 { 100_000 } else { 0 };
            s.record(i * 1_080_000, wall);
        }
        let snap = s.snapshot().unwrap();
        assert!(snap.p50_us <= snap.p95_us);
        assert!(snap.p95_us <= snap.p99_us);
        assert!(snap.p99_us <= snap.max_us);
        assert!(snap.max_us >= 90_000, "outlier should be visible at max: {}", snap.max_us);
    }

    #[test]
    fn backward_pcr_jump_does_not_record_garbage_drift() {
        // Regression for a bug where `u64::wrapping_sub` + `% PCR_MODULUS_27MHZ`
        // produced drift in the tens of seconds when PCR apparently went
        // backwards by a few ticks. Backward jumps should now be caught by
        // the 500 ms interval guard and discarded.
        let s = PcrTrustSampler::new();
        // 40 ms forward — normal sample.
        s.record(0, 0);
        s.record(1_080_000, 40_000);
        // Jump backwards by 1 ms (PCR discontinuity). Should be rejected,
        // not recorded as ~23 seconds of drift.
        s.record(1_053_000, 41_000);
        // Another normal sample after the reset.
        s.record(1_053_000 + 1_080_000, 41_000 + 40_000);
        let snap = s.snapshot().unwrap();
        // Only the two healthy samples should count; the backward jump was
        // skipped and reset state.
        assert_eq!(snap.samples, 2);
        assert!(snap.max_us < 500_000, "max drift leaked: {}", snap.max_us);
    }

    #[test]
    fn large_forward_gap_is_discarded() {
        // A 1-second gap (way above typical PCR cadence) is a keyframe /
        // restart artefact, not a drift sample.
        let s = PcrTrustSampler::new();
        s.record(0, 0);
        s.record(27_000_000, 1_000_000); // 1 s forward
        let snap = s.snapshot();
        assert!(snap.is_none(), "large-gap sample should have been skipped");
    }

    #[test]
    fn percentile_helper_linear_interp() {
        let v = [0u64, 10, 20, 30, 40];
        assert_eq!(percentile(&v, 0.0), 0);
        assert_eq!(percentile(&v, 50.0), 20);
        assert_eq!(percentile(&v, 100.0), 40);
        // Exact mid-interpolation between [20, 30] at q=62.5 → 25.
        assert_eq!(percentile(&v, 62.5), 25);
    }
}
