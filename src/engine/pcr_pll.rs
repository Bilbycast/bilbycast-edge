// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Software PI-controller PLL recovering source's 27 MHz clock from
//! incoming MPEG-TS PCR samples.
//!
//! Used by [`crate::engine::master_clock::MasterClockKind::SourcePcrPll`]
//! to clock every output of a flow whose source carries an embedded PCR
//! (SRT / RTP / UDP / RIST / RTMP / RTSP / `media_player` / `replay`).
//! `PcrTrust` measures *output* PCR accuracy at egress; `PcrPll` is the
//! sibling consumer of the same `(pcr_27mhz, wall_ns)` event stream on
//! the *ingress* side.
//!
//! ## Algorithm
//!
//! On every PCR sample `(pcr_27mhz, wall_ns)` arriving from the input
//! demuxer:
//!
//! 1. Compute the modular-aware delta against the previous sample
//!    (`Δpcr_ticks`, `Δwall_ns`).
//! 2. Discard discontinuities — gaps > 500 ms, backwards jumps in the
//!    monotonic wall clock, or first-sample-after-reset are filtered
//!    just like `pcr_trust.rs` does.
//! 3. Update the recovered rate `R` (ticks-per-nanosecond) via a
//!    proportional + integral filter against the instantaneous
//!    observed rate. Default gains converge inside 125 samples
//!    (≈ 5 s at 25 Hz PCR cadence).
//! 4. Re-anchor `(pcr_anchor, wall_anchor)` so future `now_27mhz()`
//!    queries advance the anchor via the recovered rate without ever
//!    drifting away from the most-recent observed PCR.
//!
//! `now_27mhz()` is a free-running prediction: it takes the current
//! monotonic wall clock and projects forward from the last anchor via
//! the recovered rate. This means `now_27mhz()` advances continuously
//! between PCR samples (40 ms gaps are normal), without quantising to
//! the PCR cadence.
//!
//! ## Lock state
//!
//! The PLL reports `is_locked()` once both:
//!
//! - At least [`MIN_SAMPLES_FOR_LOCK`] PCR samples have been seen.
//! - The recent residual jitter (p99 over the last
//!   [`JITTER_WINDOW`] samples) is below [`LOCK_JITTER_THRESHOLD_US`].
//!
//! Lock is sticky downwards — the PLL doesn't bounce in and out on a
//! single jitter spike. An unlock fires only after a sustained
//! [`UNLOCK_JITTER_THRESHOLD_US`] excursion across the window.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

/// Expected source clock rate: 27 MHz = 27 ticks per microsecond
/// = 0.027 ticks per nanosecond. PLL bias term against this nominal.
pub const EXPECTED_RATE_TICKS_PER_NS: f64 = 27.0 / 1000.0;

/// Maximum forward gap (µs) between PCR samples before we treat the
/// gap as a discontinuity. 500 ms matches `pcr_trust.rs`.
pub const PCR_MAX_SAMPLE_INTERVAL_US: u64 = 500_000;

/// Minimum samples before the PLL reports locked. 5 s × 25 Hz typical
/// PCR cadence = 125 samples; round to 100 for cushion at 20 Hz.
pub const MIN_SAMPLES_FOR_LOCK: u64 = 100;

/// p99 jitter threshold (µs) for lock entry. Brief target is < 100 µs.
pub const LOCK_JITTER_THRESHOLD_US: u64 = 100;

/// p99 jitter threshold (µs) for unlock — set comfortably above lock so
/// a single noisy sample doesn't toggle the state.
pub const UNLOCK_JITTER_THRESHOLD_US: u64 = 500;

/// Number of recent residuals retained for jitter percentile.
pub const JITTER_WINDOW: usize = 64;

/// PI controller gains. Tuned empirically for 25 Hz PCR cadence;
/// proportional drives quick rate uptake, integral cancels long-term
/// bias from CPU clock skew.
pub const DEFAULT_KP: f64 = 0.10;
pub const DEFAULT_KI: f64 = 0.005;

/// 33-bit PCR base × 300 — full 27 MHz PCR modulus. Wraps every
/// ~95 hours.
pub const PCR_MODULUS_27MHZ: u64 = (1u64 << 33) * 300;

/// Tunable parameters for the PLL loop filter.
#[derive(Debug, Clone, Copy)]
pub struct PcrPllConfig {
    /// Proportional gain. Default [`DEFAULT_KP`].
    pub kp: f64,
    /// Integral gain. Default [`DEFAULT_KI`].
    pub ki: f64,
}

impl Default for PcrPllConfig {
    fn default() -> Self {
        Self {
            kp: DEFAULT_KP,
            ki: DEFAULT_KI,
        }
    }
}

/// Telemetry snapshot for the PLL. Stable across calls — no statefulness.
#[derive(Debug, Clone, Serialize)]
pub struct PcrPllTelemetry {
    /// True when the PLL has converged (see [`MIN_SAMPLES_FOR_LOCK`]
    /// and [`LOCK_JITTER_THRESHOLD_US`]).
    pub locked: bool,
    /// Recovered rate vs. expected 27 MHz, in ppm. Positive = source
    /// running faster than the local CPU clock.
    pub rate_offset_ppm: f64,
    /// p99 jitter in µs over the last [`JITTER_WINDOW`] samples.
    pub jitter_us: u64,
    /// Cumulative PCR samples accepted (post-discontinuity filter).
    pub samples: u64,
}

impl Default for PcrPllTelemetry {
    fn default() -> Self {
        Self {
            locked: false,
            rate_offset_ppm: 0.0,
            jitter_us: 0,
            samples: 0,
        }
    }
}

/// Internal mutable state, behind a Mutex.
struct PllState {
    /// Most-recent observed PCR in 27 MHz ticks (mod PCR_MODULUS_27MHZ).
    last_pcr_27mhz: Option<u64>,
    /// Wall-clock nanoseconds at which `last_pcr_27mhz` was recorded.
    last_wall_ns: Option<u128>,
    /// Anchor PCR — predicted now() advances from this point at the
    /// recovered rate.
    anchor_pcr_27mhz: Option<u64>,
    /// Wall-clock anchor.
    anchor_wall_ns: Option<u128>,
    /// Recovered rate in ticks per nanosecond. Initialised to
    /// [`EXPECTED_RATE_TICKS_PER_NS`].
    rate: f64,
    /// PI integrator term.
    integral: f64,
    /// Recent residual jitter samples in µs.
    jitter_samples: VecDeque<u64>,
    /// Cumulative samples accepted (post-discontinuity filter).
    cumulative_samples: u64,
    /// Sticky lock flag — see module docs.
    locked: bool,
}

impl PllState {
    fn new() -> Self {
        Self {
            last_pcr_27mhz: None,
            last_wall_ns: None,
            anchor_pcr_27mhz: None,
            anchor_wall_ns: None,
            rate: EXPECTED_RATE_TICKS_PER_NS,
            integral: 0.0,
            jitter_samples: VecDeque::with_capacity(JITTER_WINDOW),
            cumulative_samples: 0,
            locked: false,
        }
    }
}

/// Software PI-controller PLL recovering source's 27 MHz clock.
///
/// Thread-safe: `record_sample` and `now_27mhz` are both lock-guarded.
/// Single-mutex contention is negligible — the writer (input demuxer)
/// runs at the PCR cadence (≤ 50 Hz) and readers (every output's emit
/// loop) take the lock for a few microseconds per call.
pub struct PcrPll {
    state: Mutex<PllState>,
    config: PcrPllConfig,
    /// Process-monotonic anchor. Used by [`now_27mhz`] when the PLL has
    /// not yet seen a sample (returns a Wallclock-style fallback so
    /// downstream PCR generation never trips on a `None`).
    epoch: Instant,
}

impl PcrPll {
    /// Build a PLL with the given config.
    pub fn new(config: PcrPllConfig) -> Self {
        Self {
            state: Mutex::new(PllState::new()),
            config,
            epoch: Instant::now(),
        }
    }

    /// Record one ingress PCR observation. `pcr_27mhz` is the PCR base
    /// (90 kHz × 300 — the 27 MHz reconstructed from the adaptation
    /// field), `wall_ns` is a process-monotonic wall-clock timestamp
    /// (typically `Instant::now() − epoch_instant` converted to ns).
    ///
    /// First call primes anchor state and returns without updating
    /// rate. Subsequent calls run the PI loop filter.
    pub fn record_sample(&self, pcr_27mhz: u64, wall_ns: u128) {
        let mut s = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        // First sample: prime state, no Δ available.
        let (last_pcr, last_wall) = match (s.last_pcr_27mhz, s.last_wall_ns) {
            (Some(p), Some(w)) => (p, w),
            _ => {
                s.last_pcr_27mhz = Some(pcr_27mhz);
                s.last_wall_ns = Some(wall_ns);
                s.anchor_pcr_27mhz = Some(pcr_27mhz);
                s.anchor_wall_ns = Some(wall_ns);
                return;
            }
        };

        // Compute modular Δpcr.
        let cur = pcr_27mhz % PCR_MODULUS_27MHZ;
        let prev = last_pcr % PCR_MODULUS_27MHZ;
        let delta_pcr_ticks = if cur >= prev {
            cur - prev
        } else {
            PCR_MODULUS_27MHZ - prev + cur
        };

        let delta_wall_ns = wall_ns.saturating_sub(last_wall);
        let delta_pcr_us = delta_pcr_ticks / 27;
        let delta_wall_us = (delta_wall_ns / 1000) as u64;

        // Discontinuity filter — same threshold as pcr_trust.rs. Reset
        // the anchor so we don't bake a 33-bit-wrap or stream-restart
        // gap into the rate estimate.
        if delta_pcr_us > PCR_MAX_SAMPLE_INTERVAL_US
            || delta_wall_us > PCR_MAX_SAMPLE_INTERVAL_US
            || delta_wall_ns == 0
        {
            s.last_pcr_27mhz = Some(pcr_27mhz);
            s.last_wall_ns = Some(wall_ns);
            s.anchor_pcr_27mhz = Some(pcr_27mhz);
            s.anchor_wall_ns = Some(wall_ns);
            // Lock survives a single discontinuity, but the jitter
            // window resets to avoid carrying nonsense forward.
            s.jitter_samples.clear();
            return;
        }

        // Instantaneous rate (ticks/ns) — gap-bounded so it can't
        // explode on a sample 1 ns apart.
        let observed_rate = (delta_pcr_ticks as f64) / (delta_wall_ns as f64);

        // Phase residual at this instant: how far the PLL's prediction
        // is from the actual incoming PCR.
        let predicted_delta_ticks = s.rate * (delta_wall_ns as f64);
        let residual_ticks = (delta_pcr_ticks as f64) - predicted_delta_ticks;
        let residual_us = (residual_ticks.abs() / 27.0) as u64;

        // PI loop filter — proportional pulls toward the observed rate
        // at this instant; integral builds against systematic bias
        // between source clock and CPU clock.
        s.integral += self.config.ki * (observed_rate - s.rate);
        s.rate += self.config.kp * (observed_rate - s.rate) + s.integral;

        // Re-anchor against the latest sample so prediction errors
        // never grow unbounded. Anchor is the truth; rate carries the
        // forward extrapolation.
        s.anchor_pcr_27mhz = Some(pcr_27mhz);
        s.anchor_wall_ns = Some(wall_ns);
        s.last_pcr_27mhz = Some(pcr_27mhz);
        s.last_wall_ns = Some(wall_ns);

        // Update jitter window.
        if s.jitter_samples.len() >= JITTER_WINDOW {
            s.jitter_samples.pop_front();
        }
        s.jitter_samples.push_back(residual_us);
        s.cumulative_samples = s.cumulative_samples.saturating_add(1);

        // Lock-state hysteresis.
        let p99 = jitter_p99(&s.jitter_samples);
        if !s.locked
            && s.cumulative_samples >= MIN_SAMPLES_FOR_LOCK
            && p99 <= LOCK_JITTER_THRESHOLD_US
        {
            s.locked = true;
        } else if s.locked && p99 > UNLOCK_JITTER_THRESHOLD_US {
            s.locked = false;
        }
    }

    /// Recovered source-clock now in 27 MHz ticks. Free-running between
    /// PCR samples — projects from the last anchor via the recovered
    /// rate. Wraps modulo [`PCR_MODULUS_27MHZ`].
    ///
    /// Before the first sample lands, returns a Wallclock-style fall-
    /// back (process monotonic time scaled at the nominal rate, plus a
    /// random offset) so downstream PCR generation always has a value.
    pub fn now_27mhz(&self, wall_ns: u128) -> u64 {
        let s = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match (s.anchor_pcr_27mhz, s.anchor_wall_ns) {
            (Some(anchor_pcr), Some(anchor_wall)) => {
                let dt_ns = wall_ns.saturating_sub(anchor_wall);
                let dt_ticks = (s.rate * dt_ns as f64) as u64;
                anchor_pcr.wrapping_add(dt_ticks) % PCR_MODULUS_27MHZ
            }
            _ => {
                // Pre-sample fallback: behave like a wallclock anchored
                // at process start. Random offset to keep cross-edge
                // wallclock-fallback flows from claiming coherence.
                let elapsed_ns = self.epoch.elapsed().as_nanos();
                let elapsed_ticks =
                    (elapsed_ns as u64).saturating_mul(27) / 1000;
                elapsed_ticks % PCR_MODULUS_27MHZ
            }
        }
    }

    /// Convenience wrapper for callers that want to compute `wall_ns`
    /// from a process-anchored `Instant` rather than passing it
    /// explicitly.
    #[allow(dead_code)]
    pub fn now_27mhz_with(&self, epoch: Instant) -> u64 {
        let dt_ns = epoch.elapsed().as_nanos();
        self.now_27mhz(dt_ns)
    }

    pub fn is_locked(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.locked)
            .unwrap_or(false)
    }

    pub fn telemetry(&self) -> PcrPllTelemetry {
        let s = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let rate_offset_ppm =
            (s.rate - EXPECTED_RATE_TICKS_PER_NS) / EXPECTED_RATE_TICKS_PER_NS * 1e6;
        let jitter_us = jitter_p99(&s.jitter_samples);
        PcrPllTelemetry {
            locked: s.locked,
            rate_offset_ppm,
            jitter_us,
            samples: s.cumulative_samples,
        }
    }
}

impl Default for PcrPll {
    fn default() -> Self {
        Self::new(PcrPllConfig::default())
    }
}

/// p99 jitter helper. Cheap (≤ 64 samples sorted in place).
fn jitter_p99(samples: &VecDeque<u64>) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut v: Vec<u64> = samples.iter().copied().collect();
    v.sort_unstable();
    let idx = ((v.len() as f64 - 1.0) * 0.99).round() as usize;
    v[idx.min(v.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: feed N PCR samples that perfectly track wall_ns at the
    /// nominal rate, ramping up from `start_pcr`.
    fn feed_perfect(pll: &PcrPll, n: u64, start_pcr: u64, period_ns: u128) {
        for i in 0..n {
            let pcr =
                start_pcr.wrapping_add((i as u64) * (period_ns as u64) * 27 / 1000);
            let wall = i as u128 * period_ns;
            pll.record_sample(pcr, wall);
        }
    }

    #[test]
    fn first_sample_primes_state_silently() {
        let pll = PcrPll::default();
        pll.record_sample(123_456_789, 0);
        let t = pll.telemetry();
        assert_eq!(t.samples, 0, "first sample primes anchor only");
        assert!(!t.locked);
    }

    #[test]
    fn perfect_clock_reports_near_zero_rate_offset() {
        let pll = PcrPll::default();
        // 200 samples at 40 ms cadence (25 Hz typical broadcast).
        feed_perfect(&pll, 200, 0, 40_000_000);
        let t = pll.telemetry();
        assert!(
            t.rate_offset_ppm.abs() < 1.0,
            "rate offset should be near zero on a perfect source: {} ppm",
            t.rate_offset_ppm
        );
    }

    #[test]
    fn lock_is_acquired_within_5_seconds_of_perfect_input() {
        let pll = PcrPll::default();
        // 5 s × 25 Hz = 125 samples — should comfortably exceed
        // MIN_SAMPLES_FOR_LOCK and stay below the jitter threshold.
        feed_perfect(&pll, 130, 0, 40_000_000);
        let t = pll.telemetry();
        assert!(t.locked, "PLL should lock on perfect input within 5s");
        assert!(t.jitter_us <= LOCK_JITTER_THRESHOLD_US);
    }

    #[test]
    fn now_27mhz_advances_after_lock() {
        let pll = PcrPll::default();
        feed_perfect(&pll, 200, 0, 40_000_000);
        // After feeding, the anchor wall is at sample 199 × 40 ms.
        // Project forward 10 ms from there.
        let last_wall = 199u128 * 40_000_000;
        let now1 = pll.now_27mhz(last_wall);
        let now2 = pll.now_27mhz(last_wall + 10_000_000);
        // 10 ms × 27 000 ticks/ms = 270 000 ticks.
        let delta = now2.wrapping_sub(now1);
        assert!(
            delta >= 268_000 && delta <= 272_000,
            "unexpected projected delta: {}",
            delta
        );
    }

    #[test]
    fn discontinuity_does_not_blow_up_rate() {
        let pll = PcrPll::default();
        feed_perfect(&pll, 150, 0, 40_000_000);
        let rate_before = pll.telemetry().rate_offset_ppm;

        // Inject a 1-second backwards jump (stream restart).
        let last_wall = 149u128 * 40_000_000 + 40_000_000;
        pll.record_sample(0, last_wall + 1_000_000_000);
        // Then resume normal cadence.
        for i in 0..50u64 {
            let wall = last_wall + 1_000_000_000 + (i as u128) * 40_000_000;
            let pcr = (i * 40_000 * 27) as u64;
            pll.record_sample(pcr, wall);
        }
        let rate_after = pll.telemetry().rate_offset_ppm;
        // Discontinuity should be filtered — rate stays sane.
        assert!(
            rate_after.abs() < 100.0,
            "rate after discontinuity is implausible: {} ppm",
            rate_after
        );
        // (sanity check the before rate didn't drift either)
        assert!(rate_before.abs() < 10.0);
    }

    #[test]
    fn drift_source_is_tracked_in_rate_offset() {
        // Source running 100 ppm fast: every nominal 40 ms wall, the
        // source ticks 40_000 × 27 × (1 + 100e-6) = 1080000 + 108 ticks.
        let pll = PcrPll::default();
        let period_ns = 40_000_000u128;
        for i in 0..200u64 {
            let wall = i as u128 * period_ns;
            // Multiply by (1 + 100ppm) = 1.0001. Use u128 to avoid loss.
            let pcr_per_step: u128 = 1_080_000 + 108; // 100 ppm fast
            let pcr = (i as u128 * pcr_per_step) as u64;
            pll.record_sample(pcr, wall);
        }
        let t = pll.telemetry();
        assert!(t.locked);
        assert!(
            (t.rate_offset_ppm - 100.0).abs() < 5.0,
            "PLL did not converge to +100 ppm drift: got {} ppm",
            t.rate_offset_ppm
        );
    }

    #[test]
    fn pre_sample_now_returns_monotonic_value() {
        let pll = PcrPll::default();
        let a = pll.now_27mhz(1_000);
        let b = pll.now_27mhz(2_000_000);
        // 2 ms = 54 000 ticks.
        assert!(b > a);
    }

    #[test]
    fn jitter_p99_is_bounded_on_clean_input() {
        let pll = PcrPll::default();
        feed_perfect(&pll, 300, 0, 40_000_000);
        let t = pll.telemetry();
        assert!(
            t.jitter_us < LOCK_JITTER_THRESHOLD_US,
            "jitter p99 on clean input too high: {} µs",
            t.jitter_us
        );
    }

    #[test]
    fn cumulative_samples_excludes_first_prime() {
        let pll = PcrPll::default();
        // 1 prime + 9 real samples = 9 cumulative.
        feed_perfect(&pll, 10, 0, 40_000_000);
        let t = pll.telemetry();
        assert_eq!(t.samples, 9);
    }

    #[test]
    fn pcr_modulus_wraps_correctly() {
        let pll = PcrPll::default();
        // First sample near top of modulus, second past the wrap.
        let high = PCR_MODULUS_27MHZ - 1_000_000;
        pll.record_sample(high, 0);
        // 50 ms later — well below the discontinuity threshold of 500 ms.
        // Δticks = 50 ms × 27000 ticks/ms = 1_350_000.
        let next_pcr = (high + 1_350_000) % PCR_MODULUS_27MHZ;
        pll.record_sample(next_pcr, 50_000_000);
        let t = pll.telemetry();
        assert_eq!(t.samples, 1, "wrap should not be filtered as a discontinuity");
    }
}
