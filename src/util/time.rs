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
