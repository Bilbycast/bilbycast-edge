use std::time::Instant;

/// Estimates throughput (bits per second) by sampling a monotonically increasing byte counter.
///
/// Usage: call `sample(current_bytes)` once per second. Returns estimated bps.
pub struct ThroughputEstimator {
    last_bytes: u64,
    last_sample_time: Instant,
}

impl ThroughputEstimator {
    pub fn new() -> Self {
        Self {
            last_bytes: 0,
            last_sample_time: Instant::now(),
        }
    }

    /// Sample the current byte counter, return estimated bits per second.
    pub fn sample(&mut self, current_bytes: u64) -> u64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample_time).as_secs_f64();
        if elapsed < 0.001 {
            return 0;
        }
        let delta_bytes = current_bytes.saturating_sub(self.last_bytes);
        let bps = ((delta_bytes as f64 / elapsed) * 8.0) as u64;
        self.last_bytes = current_bytes;
        self.last_sample_time = now;
        bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_initial_sample_returns_zero_or_reasonable() {
        let mut est = ThroughputEstimator::new();
        // First sample should return 0 or something reasonable (tiny elapsed)
        let bps = est.sample(0);
        // With 0 bytes delta, should be 0
        assert_eq!(bps, 0);
    }

    #[test]
    fn test_throughput_calculation() {
        let mut est = ThroughputEstimator::new();
        // Simulate initial state
        est.sample(0);

        // Sleep briefly to create measurable elapsed time
        thread::sleep(Duration::from_millis(100));

        // 125,000 bytes in ~0.1s = ~10 Mbps
        let bps = est.sample(125_000);
        // Should be roughly 10 Mbps (8 * 125000 / 0.1 = 10,000,000)
        // Allow wide range due to sleep imprecision
        assert!(bps > 5_000_000, "Expected > 5 Mbps, got {bps}");
        assert!(bps < 20_000_000, "Expected < 20 Mbps, got {bps}");
    }

    #[test]
    fn test_zero_elapsed_returns_zero() {
        let mut est = ThroughputEstimator::new();
        // Two samples in rapid succession
        let _ = est.sample(1000);
        // Override sample time to now so elapsed is ~0
        est.last_sample_time = Instant::now();
        let bps = est.sample(2000);
        // With essentially zero elapsed, should return 0
        assert_eq!(bps, 0);
    }
}
