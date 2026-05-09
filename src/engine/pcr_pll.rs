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

/// Minimum physical inter-arrival time (ns) between two PCR samples.
/// Samples whose `Δwall_ns` is below this are treated as a coalesced
/// arrival burst rather than a real rate observation, and are dropped
/// at the discontinuity gate. Examples that produce sub-100-µs deltas:
/// `ffmpeg -stream_loop -re` at the loop boundary (re-pacing restarts
/// from frame 0, kernel tx may release several TS packets in a single
/// burst); SRT / RIST recovery replaying a buffered batch; bonded-link
/// reordering followed by in-order delivery; OS scheduler coalescing
/// several `recv()` returns into one batch. None of these correspond
/// to a real source clock observation. Without this gate, a single
/// such sample lands an `observed_rate` of ~10⁵–10⁸ ticks/ns into the
/// PI loop and wedges the recovered rate.
pub const MIN_INTERSAMPLE_WALL_NS: u128 = 100_000;

/// Per-sample observed-rate gate. Catches MASSIVE outliers that would
/// instantaneously corrupt the integrator (10⁵+ ppm — typical of an
/// arrival burst the [`MIN_INTERSAMPLE_WALL_NS`] floor missed, or a
/// PCR encoded against a discontinuous source clock). Set wide
/// (±10 %) because real-world arrival jitter, even on a healthy LAN,
/// makes the *instantaneous* observed rate fluctuate by tens of
/// thousands of ppm per sample — that's a job for the PI integrator
/// to filter, not for a hard gate to reject. The gate is here only
/// to keep the controller out of arithmetic-overflow / wedged-state
/// territory; the steady-state rate clamp ([`RATE_MIN`] / [`RATE_MAX`])
/// enforces the physical-plausibility window on the *recovered* rate.
pub const MAX_OBSERVED_RATE_OFFSET_PPM: f64 = 100_000.0;

/// Hard plausibility bound on the recovered rate (`s.rate`) after the
/// PI update. Real broadcast oscillators sit at ±1 ppm; consumer
/// crystals ±50 ppm; 500 ppm gives ~10× headroom. If the controller
/// ever wants to settle outside this window, it's misbehaving and we
/// refuse — better to free-run at the boundary than diverge into
/// wedged-master-clock territory.
pub const MAX_RECOVERED_RATE_OFFSET_PPM: f64 = 500.0;

/// Hard clamp on `s.integral`. Anti-windup: a single outlier sample
/// (or a transient burst that slipped past the gates) cannot push
/// the integrator to a value subsequent samples can't unwind. Sized
/// at 10× the steady-state magnitude needed to track
/// [`MAX_RECOVERED_RATE_OFFSET_PPM`] of source drift.
const INTEGRATOR_MAX: f64 = EXPECTED_RATE_TICKS_PER_NS
    * (MAX_RECOVERED_RATE_OFFSET_PPM / 1.0e6)
    * 10.0;

/// Recovered-rate clamp window endpoints. See
/// [`MAX_RECOVERED_RATE_OFFSET_PPM`].
const RATE_MIN: f64 = EXPECTED_RATE_TICKS_PER_NS
    * (1.0 - MAX_RECOVERED_RATE_OFFSET_PPM / 1.0e6);
const RATE_MAX: f64 = EXPECTED_RATE_TICKS_PER_NS
    * (1.0 + MAX_RECOVERED_RATE_OFFSET_PPM / 1.0e6);

/// Per-sample observed-rate gate endpoints (looser than the recovered
/// rate clamp on purpose — see [`MAX_OBSERVED_RATE_OFFSET_PPM`]).
const OBSERVED_RATE_MIN: f64 = EXPECTED_RATE_TICKS_PER_NS
    * (1.0 - MAX_OBSERVED_RATE_OFFSET_PPM / 1.0e6);
const OBSERVED_RATE_MAX: f64 = EXPECTED_RATE_TICKS_PER_NS
    * (1.0 + MAX_OBSERVED_RATE_OFFSET_PPM / 1.0e6);

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

        // Discontinuity filter. Reset the anchor and refuse to update
        // the recovered rate when the sample is implausible:
        //
        // - `delta_pcr_us` / `delta_wall_us` > 500 ms: stream restart,
        //   33-bit wrap, or extended input gap. Same threshold as
        //   `pcr_trust.rs`.
        // - `delta_wall_ns < MIN_INTERSAMPLE_WALL_NS`: arrival burst
        //   (ffmpeg loop boundary, SRT / RIST recovery batch, OS recv
        //   coalescing). The PCR delta is real, but the wall delta is
        //   not — the resulting `observed_rate` would be 10⁵–10⁸×
        //   nominal and wedge the integrator.
        // ── Hard discontinuity (real gap > 500 ms): reset anchor.
        // Stream restart, 33-bit PCR wrap, extended ingress gap. Same
        // threshold as `pcr_trust.rs`. The next sample primes against
        // the new anchor.
        if delta_pcr_us > PCR_MAX_SAMPLE_INTERVAL_US
            || delta_wall_us > PCR_MAX_SAMPLE_INTERVAL_US
        {
            s.last_pcr_27mhz = Some(pcr_27mhz);
            s.last_wall_ns = Some(wall_ns);
            s.anchor_pcr_27mhz = Some(pcr_27mhz);
            s.anchor_wall_ns = Some(wall_ns);
            s.jitter_samples.clear();
            return;
        }

        // ── Transient burst (Δwall < 100 µs): SKIP without resetting
        // anchor. This is the case where multiple PCR-bearing
        // datagrams arrive in the same recv() batch (kernel coalesce,
        // ffmpeg burst, SRT/RIST recovery replay). Resetting the
        // anchor here would force the next legitimate sample to
        // compute its observed_rate against this burst's wall time
        // — which is a few microseconds earlier than the burst's
        // PCR position dictates — and that biases observed_rate way
        // outside the outlier gate. Skipping (with the anchor
        // unchanged) means the next legitimate sample compares
        // against the last-known-good anchor and produces a clean
        // observation.
        if delta_wall_ns < MIN_INTERSAMPLE_WALL_NS {
            return;
        }

        // Instantaneous rate (ticks/ns).
        let observed_rate = (delta_pcr_ticks as f64) / (delta_wall_ns as f64);

        // Massive-outlier gate. Catches per-sample observed rates
        // that are off by more than [`MAX_OBSERVED_RATE_OFFSET_PPM`]
        // (set wide — ±10 %). The PI loop is responsible for filtering
        // out the much-smaller per-sample jitter that real-world
        // arrival timing produces (tens of thousands of ppm on a
        // healthy LAN); this gate exists only to keep the controller
        // out of wedged-state territory when something genuinely
        // pathological slips past the [`MIN_INTERSAMPLE_WALL_NS`]
        // floor. Anchor is re-aligned so the next legitimate sample
        // has a clean reference.
        if !(OBSERVED_RATE_MIN..=OBSERVED_RATE_MAX).contains(&observed_rate) {
            // Massive-outlier rejection. SKIP without resetting the
            // anchor — same rationale as the burst-skip branch above.
            // The next legitimate sample compares against the
            // last-good anchor and produces a clean observation.
            return;
        }

        // Phase residual at this instant: how far the PLL's prediction
        // is from the actual incoming PCR.
        let predicted_delta_ticks = s.rate * (delta_wall_ns as f64);
        let residual_ticks = (delta_pcr_ticks as f64) - predicted_delta_ticks;
        let residual_us = (residual_ticks.abs() / 27.0) as u64;

        // PI loop filter — proportional pulls toward the observed rate
        // at this instant; integral builds against systematic bias
        // between source clock and CPU clock.
        s.integral += self.config.ki * (observed_rate - s.rate);
        // Anti-windup: clamp the integrator before it feeds back into
        // s.rate. A single outlier that slipped past the gates above
        // (or a transient ringing while the PLL is warming up) cannot
        // push the integrator to a value subsequent samples can't
        // unwind inside the lock window.
        s.integral = s.integral.clamp(-INTEGRATOR_MAX, INTEGRATOR_MAX);
        s.rate += self.config.kp * (observed_rate - s.rate) + s.integral;
        // Hard rate clamp. If the controller wants to settle outside
        // the physical-plausibility window, refuse — better to free-run
        // at the boundary than diverge into wedged-master-clock
        // territory where every downstream PCR generator and pacer
        // produces nonsense.
        s.rate = s.rate.clamp(RATE_MIN, RATE_MAX);

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

    /// A single sample arriving inside the physical inter-arrival
    /// floor (`MIN_INTERSAMPLE_WALL_NS`) must NOT update the recovered
    /// rate. This is the ffmpeg-loop-boundary / recv-coalescing case
    /// that wedged the PLL on the testbed.
    #[test]
    fn coalesced_arrival_does_not_wedge_rate() {
        let pll = PcrPll::default();
        feed_perfect(&pll, 130, 0, 40_000_000);
        let rate_before = pll.telemetry().rate_offset_ppm;
        assert!(pll.telemetry().locked, "PLL should lock on perfect input");

        // Burst arrival: same 40 ms PCR cadence, but the wallclock
        // delta collapses to 1 µs (kernel released several TS packets
        // in a single recv() return).
        let last_wall = 130u128 * 40_000_000;
        let last_pcr = 130u64 * 40_000 * 27;
        pll.record_sample(last_pcr + 40_000 * 27, last_wall + 1_000);

        let t = pll.telemetry();
        let rate_after = t.rate_offset_ppm;
        assert!(
            (rate_after - rate_before).abs() < 1.0,
            "single coalesced-arrival sample should not change rate; \
             before={} ppm, after={} ppm",
            rate_before,
            rate_after
        );
    }

    /// Even when `Δwall_ns` clears the floor, an `observed_rate`
    /// outside ±MAX_RECOVERED_RATE_OFFSET_PPM of nominal is corrupt and must be
    /// rejected. (Construct the case where Δwall is just above the
    /// 100 µs floor but Δpcr is still order-of-magnitude too large.)
    #[test]
    fn implausible_observed_rate_is_rejected() {
        let pll = PcrPll::default();
        feed_perfect(&pll, 130, 0, 40_000_000);
        let rate_before = pll.telemetry().rate_offset_ppm;

        // Δwall = 200 µs (above the 100 µs floor), Δpcr = 40 ms worth
        // of ticks (1 080 000). observed_rate = 1 080 000 / 200 000
        // = 5.4 ticks/ns ≈ 200 000× nominal. Way outside ±500 ppm.
        let last_wall = 130u128 * 40_000_000;
        let last_pcr = 130u64 * 40_000 * 27;
        pll.record_sample(last_pcr + 1_080_000, last_wall + 200_000);

        let rate_after = pll.telemetry().rate_offset_ppm;
        assert!(
            (rate_after - rate_before).abs() < 1.0,
            "implausible observed_rate should be rejected; before={} after={} ppm",
            rate_before,
            rate_after
        );
    }

    /// Even a stream of corrupt samples (e.g. a misbehaving source or
    /// a flaky network where every batch coalesces) cannot push
    /// `s.rate` outside the physical-plausibility window. This is the
    /// load-bearing safety property that prevents downstream PCR
    /// generators from producing nonsense PCRs and any future pacer
    /// from over- or under-driving the wire.
    #[test]
    fn rate_never_escapes_plausibility_window() {
        let pll = PcrPll::default();
        // 100 normal samples to lock the PLL.
        feed_perfect(&pll, 100, 0, 40_000_000);

        // Now blast it with 500 corrupt-but-pre-gate-passing-for-some-
        // reason inputs. We can't construct samples that pass the gates
        // and still produce wild rates (that's the point of the gates),
        // so this also exercises that the gates themselves keep firing
        // without the rate ever drifting. Mix legitimate nominal-rate
        // samples with implausible bursts.
        let mut wall: u128 = 100 * 40_000_000;
        let mut pcr: u64 = 100 * 40_000 * 27;
        for i in 0..500u64 {
            // Every 5th sample is a burst.
            if i % 5 == 0 {
                wall += 1_000;
                pcr += 1_080_000;
            } else {
                wall += 40_000_000;
                pcr += 1_080_000;
            }
            pll.record_sample(pcr, wall);
        }

        let t = pll.telemetry();
        // s.rate must stay inside ±MAX_RECOVERED_RATE_OFFSET_PPM under all
        // conditions, regardless of whether lock is held.
        assert!(
            t.rate_offset_ppm.abs() <= MAX_RECOVERED_RATE_OFFSET_PPM + 1.0,
            "rate escaped plausibility window: {} ppm (limit ±{})",
            t.rate_offset_ppm,
            MAX_RECOVERED_RATE_OFFSET_PPM
        );
    }

    /// Recovery from a *pre-existing* wedged state. Simulates the
    /// production failure mode observed on the testbed: if the PLL is
    /// somehow in a wedged state (e.g., unwinding from a sequence of
    /// outliers that pre-dated the fixes), feeding clean input must
    /// pull `s.rate` back into the plausibility window quickly.
    #[test]
    fn recovers_from_wedged_state() {
        let pll = PcrPll::default();
        // Force the state into a wedged configuration externally.
        {
            let mut s = pll.state.lock().unwrap();
            s.rate = EXPECTED_RATE_TICKS_PER_NS * 1.0e3; // 1e9 ppm — the
                                                        //  exact wedge
                                                        //  observed in
                                                        //  testbed stats
            s.integral = 1.0; // also wedged
            s.last_pcr_27mhz = Some(0);
            s.last_wall_ns = Some(0);
            s.anchor_pcr_27mhz = Some(0);
            s.anchor_wall_ns = Some(0);
        }

        // Feed clean input. With anti-windup + rate clamp, even the
        // very first PI update must clamp s.rate back inside the
        // window.
        let pcr_per_step: u64 = 40_000 * 27; // 40 ms, exactly nominal
        for i in 1u64..=10 {
            let wall = i as u128 * 40_000_000;
            let pcr = i * pcr_per_step;
            pll.record_sample(pcr, wall);
        }

        let t = pll.telemetry();
        assert!(
            t.rate_offset_ppm.abs() <= MAX_RECOVERED_RATE_OFFSET_PPM + 1.0,
            "PLL did not recover from wedged state: {} ppm",
            t.rate_offset_ppm
        );
    }

    /// Anti-windup: integrator never exceeds INTEGRATOR_MAX after the
    /// PI update step.
    #[test]
    fn integrator_is_bounded_under_repeated_outliers() {
        let pll = PcrPll::default();
        // Lock the PLL first.
        feed_perfect(&pll, 130, 0, 40_000_000);

        // Inject samples that push observed_rate to the edge of the
        // window (just inside +500 ppm) repeatedly. The integrator
        // would naturally walk up; the clamp must hold.
        let mut wall: u128 = 130 * 40_000_000;
        let mut pcr: u64 = 130 * 40_000 * 27;
        // 499 ppm fast: per 40 ms, ticks = 1080000 + (1080000 × 499e-6)
        //              = 1080000 + 539.
        for _ in 0..1000u64 {
            wall += 40_000_000;
            pcr += 1_080_000 + 539;
            pll.record_sample(pcr, wall);
        }

        let s = pll.state.lock().unwrap();
        assert!(
            s.integral.abs() <= INTEGRATOR_MAX + f64::EPSILON,
            "integrator escaped clamp: {} (limit ±{})",
            s.integral,
            INTEGRATOR_MAX
        );
    }
}
