// Lock-free audio clock shared between the display task and the audio
// task on a `display` output.
//
// The audio task is the master: every successful `snd_pcm_writei` of N
// samples advances the clock by `N / sample_rate` seconds. The display
// task reads the clock per-frame to compute the current PTS-equivalent
// for pacing decisions against decoded video frames.
//
// Implementation: an anchor PTS (90 kHz), the active sample rate (Hz),
// and a running counter of total samples-since-anchor — all atomic.
// Plus a wall-clock anchor (`program_start`-relative microseconds) and
// a per-`advance` wall-clock timestamp used to interpolate the clock
// forward between ALSA-period writes (~20 ms). Without that
// interpolation the display task reads a stepped value and drift
// against video PTS swings ±20 ms across each period boundary,
// triggering visible dup/drop stutter.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Audio-as-master clock. Constructed empty; the audio task calls
/// [`AudioClock::set_anchor`] on the first successful ALSA write and
/// [`AudioClock::advance`] thereafter.
#[derive(Debug, Default)]
pub struct AudioClock {
    /// Set by `set_anchor`. Until then, all reads return `None`.
    armed: AtomicBool,
    /// Reference `Instant` against which `last_advance_us` is measured.
    /// Set on the first `set_anchor` call (the supplied `now` is the
    /// receiver-side `program_start` — kept around so the smoothed
    /// reader can compute "wall-clock now in µs since program start").
    program_start: OnceLock<Instant>,
    /// PTS at the anchor instant, in 90 kHz units (matching `TsDemuxer`).
    anchor_pts_90k: AtomicU64,
    /// Sample rate driving the clock — set on first write, mutated only
    /// when the upstream stream's SR changes (and the anchor is reset).
    sample_rate: AtomicU32,
    /// Total samples-per-channel written through the audio device since
    /// the anchor. Together with `sample_rate` this lets the display
    /// task compute a fresh PTS without locking.
    samples_since_anchor: AtomicU64,
    /// Microseconds-since-`program_start` at the most recent `advance`
    /// (or `set_anchor`). The smoothed reader uses
    /// `now_us - last_advance_us` to interpolate the audio PTS forward
    /// between ALSA-period writes — a ~20 ms quantum without it.
    last_advance_us: AtomicU64,
}

impl AudioClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initial wall-clock anchor. Called by the audio task right before
    /// the first successful `writei` returns. The wall-clock origin
    /// `now` (the receiver-side `program_start`) is recorded the first
    /// time this is called and reused for every subsequent call so
    /// `last_advance_us` and the smoothed reader stay on a single
    /// timeline.
    pub fn set_anchor(&self, now: Instant, anchor_pts_90k: u64, sample_rate: u32) {
        let _ = self.program_start.set(now);
        self.anchor_pts_90k.store(anchor_pts_90k, Ordering::Relaxed);
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        self.samples_since_anchor.store(0, Ordering::Relaxed);
        let now_us = self
            .program_start
            .get()
            .map(|s| s.elapsed().as_micros() as u64)
            .unwrap_or(0);
        self.last_advance_us.store(now_us, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    /// Advance the clock by `samples` samples-per-channel of audio that
    /// just landed in the ALSA buffer. The audio task calls this after
    /// each successful blocking write.
    pub fn advance(&self, samples: u64) {
        self.samples_since_anchor
            .fetch_add(samples, Ordering::Relaxed);
        if let Some(start) = self.program_start.get() {
            let now_us = start.elapsed().as_micros() as u64;
            self.last_advance_us.store(now_us, Ordering::Relaxed);
        }
    }

    /// Read the current "audio now" PTS in 90 kHz units. `None` until
    /// the audio task has set the anchor. Cheap — three relaxed loads
    /// plus a couple of mults. **Stepped** — the value only advances
    /// when the audio task calls `advance()`, i.e. once per ALSA period
    /// (~20 ms in our config). Use [`current_pts_90k_smoothed`] from
    /// the display task; this method is kept for stats / debug paths
    /// that want the raw "samples written so far" answer.
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

    /// Same as [`current_pts_90k`] but **interpolated forward** by
    /// wall-clock since the most recent `advance`. Smooths over the
    /// per-ALSA-period quantum (~20 ms in our config) the audio task
    /// writes in. The display loop reads this for per-frame pacing —
    /// without smoothing, drift swings ±20 ms across each period
    /// boundary and tips the dup/drop logic into bursts of repeated /
    /// dropped frames that the operator sees as motion stutter.
    ///
    /// The interpolation is capped at 200 ms — beyond that the audio
    /// task is stalled (xrun recovery, codec error, channel close)
    /// and we shouldn't keep extrapolating; we let drift go negative
    /// instead so the display task knows to drop / re-anchor.
    pub fn current_pts_90k_smoothed(&self) -> Option<u64> {
        let base = self.current_pts_90k()?;
        let Some(start) = self.program_start.get() else {
            return Some(base);
        };
        let sr = self.sample_rate.load(Ordering::Relaxed);
        if sr == 0 {
            return Some(base);
        }
        let now_us = start.elapsed().as_micros() as u64;
        let last_us = self.last_advance_us.load(Ordering::Relaxed);
        let elapsed_us = now_us.saturating_sub(last_us).min(200_000);
        let elapsed_pts = elapsed_us.saturating_mul(90) / 1000;
        Some(base.wrapping_add(elapsed_pts))
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
        assert!(c.current_pts_90k_smoothed().is_none());
    }

    #[test]
    fn anchored_clock_advances_in_pts() {
        let c = AudioClock::new();
        let now = Instant::now();
        c.set_anchor(now, 90_000, 48_000);
        // Raw reader is exact: anchor PTS until samples advance.
        assert_eq!(c.current_pts_90k(), Some(90_000));
        // Write 480 samples = 10 ms at 48 kHz = 900 PTS units.
        c.advance(480);
        assert_eq!(c.current_pts_90k(), Some(90_900));
        c.advance(48_000);
        assert_eq!(c.current_pts_90k(), Some(90_000 + 900 + 90_000));
    }

    #[test]
    fn smoothed_clock_interpolates_between_advances() {
        let c = AudioClock::new();
        c.set_anchor(Instant::now(), 90_000, 48_000);
        c.advance(480); // base = 90_900
        let base = c.current_pts_90k().unwrap();
        thread::sleep(Duration::from_millis(20));
        let smoothed = c.current_pts_90k_smoothed().unwrap();
        // Smoothed should have moved forward by roughly 20 ms = 1800 PTS,
        // give a wide tolerance for OS scheduler jitter.
        let delta = smoothed.wrapping_sub(base);
        assert!(
            (1_500..=3_000).contains(&delta),
            "expected ~1800 PTS interpolation, got {delta}",
        );
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
