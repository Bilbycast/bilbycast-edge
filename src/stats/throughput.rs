// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use std::time::Instant;

/// Estimates throughput (bits per second) by sampling a monotonically increasing byte counter.
///
/// Caches the last computed value and only re-samples after a minimum interval (1 second)
/// to avoid incorrect readings when polled by multiple consumers (dashboard + API).
pub struct ThroughputEstimator {
    last_bytes: u64,
    last_sample_time: Instant,
    cached_bps: u64,
    min_interval_secs: f64,
}

impl ThroughputEstimator {
    pub fn new() -> Self {
        Self {
            last_bytes: 0,
            last_sample_time: Instant::now(),
            cached_bps: 0,
            min_interval_secs: 1.0,
        }
    }

    /// Sample the current byte counter, return estimated bits per second.
    /// If called within the minimum interval, returns the cached value.
    pub fn sample(&mut self, current_bytes: u64) -> u64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample_time).as_secs_f64();
        if elapsed < self.min_interval_secs {
            return self.cached_bps;
        }
        let delta_bytes = current_bytes.saturating_sub(self.last_bytes);
        let bps = ((delta_bytes as f64 / elapsed) * 8.0) as u64;
        self.last_bytes = current_bytes;
        self.last_sample_time = now;
        self.cached_bps = bps;
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
        let mut est = ThroughputEstimator::new();
        let bps = est.sample(0);
        assert_eq!(bps, 0);
    }

    #[test]
    fn test_throughput_calculation() {
        let mut est = ThroughputEstimator::new();
        est.min_interval_secs = 0.05; // Lower for testing
        est.sample(0);

        thread::sleep(Duration::from_millis(100));

        // 125,000 bytes in ~0.1s = ~10 Mbps
        let bps = est.sample(125_000);
        assert!(bps > 5_000_000, "Expected > 5 Mbps, got {bps}");
        assert!(bps < 20_000_000, "Expected < 20 Mbps, got {bps}");
    }

    #[test]
    fn test_cached_value_returned_within_interval() {
        let mut est = ThroughputEstimator::new();
        est.min_interval_secs = 0.05;
        est.sample(0);

        thread::sleep(Duration::from_millis(100));
        let bps1 = est.sample(125_000);
        assert!(bps1 > 0);

        // Immediate second call should return cached value, not recompute
        let bps2 = est.sample(125_000);
        assert_eq!(bps1, bps2);
    }
}
