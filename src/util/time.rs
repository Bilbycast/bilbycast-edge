// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::OnceLock;
use std::time::Instant;

/// Monotonic clock reference. Set once at startup.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Initialize the monotonic epoch. Call once at startup.
pub fn init_epoch() {
    EPOCH.get_or_init(Instant::now);
}

/// Get the current monotonic time in microseconds since epoch.
pub fn now_us() -> u64 {
    match EPOCH.get() {
        Some(epoch) => epoch.elapsed().as_micros() as u64,
        None => 0,
    }
}

/// An instant already read, in microseconds since epoch, as [`now_us`] read
/// then would have given it — without reading the clock again. 0 for one
/// before the epoch, or before [`init_epoch`].
pub fn us_at(at: Instant) -> u64 {
    match EPOCH.get() {
        Some(epoch) => at.saturating_duration_since(*epoch).as_micros() as u64,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instant read between two `now_us()` reads lands between them.
    #[test]
    fn an_instant_reads_as_now_us_did_then() {
        init_epoch();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let before = now_us();
        let at = Instant::now();
        let after = now_us();
        assert!((before..=after).contains(&us_at(at)));
        assert_eq!(us_at(*EPOCH.get().unwrap() - std::time::Duration::from_secs(1)), 0);
    }
}
