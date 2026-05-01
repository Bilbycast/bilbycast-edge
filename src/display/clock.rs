// Lock-free audio clock shared between the display task and the audio
// task on a `display` output.
//
// The audio task is the master: every successful `snd_pcm_writei` of N
// samples advances the clock by `N / sample_rate` seconds. The display
// task reads the clock per-frame to compute the current PTS-equivalent
// for dup/drop decisions against decoded video frames.
//
// Implementation: three `AtomicU64` (anchor wall-clock μs, anchor PTS
// 90 kHz, sample-rate Hz) plus an `AtomicU64` of total samples
// written-since-anchor. Relaxed ordering is fine — drift accumulates
// over thousands of frames, not bytes, and the display task only needs
// a roughly-fresh snapshot.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// Audio-as-master clock. Constructed empty; the audio task calls
/// [`AudioClock::set_anchor`] on the first successful ALSA write and
/// [`AudioClock::advance`] thereafter.
#[derive(Debug, Default)]
pub struct AudioClock {
    /// Set by `set_anchor`. Until then, all reads return `None`.
    armed: std::sync::atomic::AtomicBool,
    /// Wall-clock anchor in microseconds since the program started
    /// (using `Instant`-derived deltas; we don't expose the absolute
    /// `Instant` so nothing in the system needs to share its origin).
    anchor_us: AtomicU64,
    /// PTS at the anchor instant, in 90 kHz units (matching `TsDemuxer`).
    anchor_pts_90k: AtomicU64,
    /// Sample rate driving the clock — set on first write, mutated only
    /// when the upstream stream's SR changes (and the anchor is reset).
    sample_rate: AtomicU32,
    /// Total samples-per-channel written through the audio device since
    /// the anchor. Together with `sample_rate` this lets the display
    /// task compute a fresh PTS without locking.
    samples_since_anchor: AtomicU64,
}

impl AudioClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initial wall-clock anchor. Called by the audio task right before
    /// the first successful `writei` returns. The wall-clock origin is
    /// the receiver-side `Instant` we keep externally.
    pub fn set_anchor(&self, now: Instant, anchor_pts_90k: u64, sample_rate: u32) {
        let anchor_us = now.elapsed().as_micros() as u64;
        self.anchor_us.store(anchor_us, Ordering::Relaxed);
        self.anchor_pts_90k.store(anchor_pts_90k, Ordering::Relaxed);
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        self.samples_since_anchor.store(0, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    /// Advance the clock by `samples` samples-per-channel of audio that
    /// just landed in the ALSA buffer. The audio task calls this after
    /// each successful blocking write.
    pub fn advance(&self, samples: u64) {
        self.samples_since_anchor
            .fetch_add(samples, Ordering::Relaxed);
    }

    /// Read the current "audio now" PTS in 90 kHz units. `None` until
    /// the audio task has set the anchor. Cheap — three relaxed loads
    /// plus a couple of mults.
    pub fn current_pts_90k(&self) -> Option<u64> {
        if !self.armed.load(Ordering::Acquire) {
            return None;
        }
        let anchor_pts = self.anchor_pts_90k.load(Ordering::Relaxed);
        let sr = self.sample_rate.load(Ordering::Relaxed);
        if sr == 0 {
            return Some(anchor_pts);
        }
        let samples = self.samples_since_anchor.load(Ordering::Relaxed);
        // 90 000 / sample_rate * samples — done in u128 to avoid overflow
        // on long playouts (e.g. 48 kHz × hours).
        let delta_pts = (samples as u128).saturating_mul(90_000) / (sr as u128);
        Some(anchor_pts.wrapping_add(delta_pts as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn unarmed_clock_returns_none() {
        let c = AudioClock::new();
        assert!(c.current_pts_90k().is_none());
    }

    #[test]
    fn anchored_clock_advances_in_pts() {
        let c = AudioClock::new();
        let now = Instant::now();
        c.set_anchor(now, 90_000, 48_000);
        assert_eq!(c.current_pts_90k(), Some(90_000));
        // Write 480 samples = 10 ms at 48 kHz = 900 PTS units.
        c.advance(480);
        assert_eq!(c.current_pts_90k(), Some(90_900));
        c.advance(48_000);
        assert_eq!(c.current_pts_90k(), Some(90_000 + 900 + 90_000));
    }

    #[test]
    fn shared_arc_lock_free() {
        let c = Arc::new(AudioClock::new());
        let now = Instant::now();
        c.set_anchor(now, 0, 48_000);
        let writer = {
            let c = Arc::clone(&c);
            thread::spawn(move || {
                for _ in 0..100 {
                    c.advance(48);
                    thread::sleep(Duration::from_micros(10));
                }
            })
        };
        let reader = {
            let c = Arc::clone(&c);
            thread::spawn(move || {
                let mut last = 0;
                for _ in 0..100 {
                    let pts = c.current_pts_90k().unwrap();
                    assert!(pts >= last);
                    last = pts;
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
        assert!(c.current_pts_90k().unwrap() >= 100 * 48 * 90_000 / 48_000);
    }
}
