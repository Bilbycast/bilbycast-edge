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
}
