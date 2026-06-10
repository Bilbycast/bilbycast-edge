// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Lock-free throughput estimator using atomic operations.
///
/// Computes bits-per-second by sampling a monotonically increasing byte counter.
/// Caches the last computed value and only re-samples after a minimum interval
/// (1 second) to avoid incorrect readings when polled by multiple consumers
/// (dashboard + API + bandwidth monitor).
///
/// All state is stored in atomics — no Mutex required. The `sample()` method
/// takes `&self` and uses a CAS loop to update state, making it safe to call
/// from multiple tasks without blocking the Tokio runtime.
#[derive(Debug)]
pub struct ThroughputEstimator {
    last_bytes: AtomicU64,
    /// Monotonic timestamp in microseconds (from `Instant` baseline).
    last_sample_time_us: AtomicU64,
    cached_bps: AtomicU64,
    baseline: Instant,
    min_interval_us: u64,
}

impl ThroughputEstimator {
    pub fn new() -> Self {
        Self {
            last_bytes: AtomicU64::new(0),
            last_sample_time_us: AtomicU64::new(0),
            cached_bps: AtomicU64::new(0),
            baseline: Instant::now(),
            min_interval_us: 1_000_000, // 1 second
        }
    }

    /// Sample the current byte counter, return estimated bits per second.
    /// If called within the minimum interval, returns the cached value.
    ///
    /// Lock-free: uses atomic loads and a single CAS to avoid races between
    /// concurrent callers (stats snapshot + bandwidth monitor). At worst,
    /// two concurrent callers both compute and one's write is lost — the
    /// next 1-second sample corrects it.
    pub fn sample(&self, current_bytes: u64) -> u64 {
        let now_us = self.baseline.elapsed().as_micros() as u64;
        let prev_time_us = self.last_sample_time_us.load(Ordering::Relaxed);
        let elapsed_us = now_us.saturating_sub(prev_time_us);

        if elapsed_us < self.min_interval_us {
            return self.cached_bps.load(Ordering::Relaxed);
        }

        // Try to claim this sample window via CAS. If another thread wins,
        // we just return the cached value — no blocking, no retry loop.
        if self
            .last_sample_time_us
            .compare_exchange(prev_time_us, now_us, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return self.cached_bps.load(Ordering::Relaxed);
        }

        let prev_bytes = self.last_bytes.swap(current_bytes, Ordering::Relaxed);
        let delta_bytes = current_bytes.saturating_sub(prev_bytes);
        let elapsed_secs = elapsed_us as f64 / 1_000_000.0;
        let bps = ((delta_bytes as f64 / elapsed_secs) * 8.0) as u64;

        self.cached_bps.store(bps, Ordering::Relaxed);
        bps
    }
}

/// Lock-free staleness tracker for a monotonically increasing counter.
///
/// Sibling of [`ThroughputEstimator`]: same prev-sample idiom, but instead
/// of a rate it answers "has the counter advanced within the last `window`?".
/// Used by the stats snapshot to decay a display output's derived state from
/// `active` to `idle` when the renderer stops flipping frames (frozen panel)
/// — network outputs get the same decay for free from the byte-delta bitrate
/// estimator, display outputs have no byte counter to estimate from.
///
/// All state is atomics; safe for concurrent snapshot callers (dashboard +
/// API + Prometheus). A lost race on `last_advance_us` is benign — both
/// writers store a "now" from the same poll interval.
#[derive(Debug)]
pub struct CounterFreshness {
    last_value: AtomicU64,
    /// Monotonic micros (vs `baseline`) at which the counter was last
    /// observed to advance. Starts at 0 (= baseline), so a counter that
    /// never advances goes stale `window` after construction.
    last_advance_us: AtomicU64,
    baseline: Instant,
}

impl CounterFreshness {
    pub fn new() -> Self {
        Self {
            last_value: AtomicU64::new(0),
            last_advance_us: AtomicU64::new(0),
            baseline: Instant::now(),
        }
    }

    /// Sample the counter; returns `true` when it advanced since the
    /// previous sample or within the last `window`.
    ///
    /// Detection latency is bounded by the caller's poll cadence: an
    /// advancing counter is always fresh regardless of how rarely it is
    /// sampled (the advance itself is observed on the next poll), and a
    /// frozen one reads stale at the first poll past `window`.
    pub fn advanced_within(&self, current: u64, window: std::time::Duration) -> bool {
        let now_us = self.baseline.elapsed().as_micros() as u64;
        let prev = self.last_value.swap(current, Ordering::Relaxed);
        if current > prev {
            self.last_advance_us.store(now_us, Ordering::Relaxed);
            return true;
        }
        // Unchanged (or reset backwards — treat as stale until it advances
        // again): fresh only while inside the window of the last advance.
        now_us.saturating_sub(self.last_advance_us.load(Ordering::Relaxed))
            < window.as_micros() as u64
    }
}

impl Default for CounterFreshness {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_initial_sample_returns_zero() {
        let est = ThroughputEstimator::new();
        let bps = est.sample(0);
        assert_eq!(bps, 0);
    }

    #[test]
    fn test_throughput_calculation() {
        let est = ThroughputEstimator {
            min_interval_us: 50_000, // 50 ms for testing
            ..ThroughputEstimator::new()
        };
        est.sample(0);

        thread::sleep(Duration::from_millis(100));

        // 125,000 bytes in ~0.1s = ~10 Mbps
        let bps = est.sample(125_000);
        assert!(bps > 5_000_000, "Expected > 5 Mbps, got {bps}");
        assert!(bps < 20_000_000, "Expected < 20 Mbps, got {bps}");
    }

    #[test]
    fn test_cached_value_returned_within_interval() {
        let est = ThroughputEstimator {
            min_interval_us: 50_000,
            ..ThroughputEstimator::new()
        };
        est.sample(0);

        thread::sleep(Duration::from_millis(100));
        let bps1 = est.sample(125_000);
        assert!(bps1 > 0);

        // Immediate second call should return cached value, not recompute
        let bps2 = est.sample(125_000);
        assert_eq!(bps1, bps2);
    }

    #[test]
    fn freshness_advance_is_fresh() {
        let f = CounterFreshness::new();
        // First advance is always fresh, regardless of window.
        assert!(f.advanced_within(1, Duration::from_millis(0)));
        // Subsequent advances stay fresh.
        assert!(f.advanced_within(2, Duration::from_millis(0)));
    }

    #[test]
    fn freshness_frozen_counter_goes_stale_after_window() {
        let f = CounterFreshness::new();
        assert!(f.advanced_within(10, Duration::from_millis(40)));
        // Unchanged but still inside the window — fresh.
        assert!(f.advanced_within(10, Duration::from_millis(40)));
        thread::sleep(Duration::from_millis(60));
        // Unchanged past the window — stale.
        assert!(!f.advanced_within(10, Duration::from_millis(40)));
        // Counter moves again — fresh immediately.
        assert!(f.advanced_within(11, Duration::from_millis(40)));
    }

    #[test]
    fn freshness_never_advanced_goes_stale() {
        let f = CounterFreshness::new();
        thread::sleep(Duration::from_millis(60));
        assert!(!f.advanced_within(0, Duration::from_millis(40)));
    }

    #[test]
    fn freshness_backwards_reset_is_stale_until_next_advance() {
        let f = CounterFreshness::new();
        assert!(f.advanced_within(100, Duration::from_millis(40)));
        thread::sleep(Duration::from_millis(60));
        // Counter went backwards (restart) — stale, not a phantom advance.
        assert!(!f.advanced_within(5, Duration::from_millis(40)));
        // …until it moves forward from the new base.
        assert!(f.advanced_within(6, Duration::from_millis(40)));
    }
}
