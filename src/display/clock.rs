// Measured audio-playout clock shared between the display task and the
// audio task on a `display` output.
//
// **This is a MEASURED clock, not an estimate.** The audio task calls
// [`AudioClock::set_position`] after every successful ALSA write with the
// PTS of the sample the DAC is *actually about to play* — derived from
// `snd_pcm_delay()` (frames written but not yet played). The display task
// reads [`AudioClock::current_pts_90k_smoothed`] per video frame and paces
// each frame to that playout position.
//
// History: an earlier revision advanced this clock open-loop by
// `samples_written / nominal_rate` plus two heuristic "catch-up" tiers
// that snapped the clock forward/back by ±100 ms when accumulated PES-PTS
// gaps crossed a threshold. That produced (a) a large constant lip-sync
// offset, because the audio clock never knew the true DAC playout point,
// and (b) a slow sawtooth as the catch-up hunted around the soundcard-vs-
// source rate gap. Driving the clock from the measured hardware delay
// eliminates both: the clock IS the playout point, so there is nothing to
// estimate and nothing to snap. Rate-matching between the soundcard and
// the source is handled in `audio.rs` by an adaptive resampler, not here.
//
// Implementation: a base PTS (90 kHz, = the measured playout point at the
// last write) plus a wall-clock stamp of when that measurement was taken.
// Reads interpolate the base forward by wall-clock elapsed since the
// stamp (the DAC plays in real time), capped at `INTERP_CAP_US` so a
// stalled audio task — xrun recovery, codec error, channel close — stops
// extrapolating into the future and lets the display task notice the
// stall instead.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Beyond this many µs since the last `set_position`, stop interpolating
/// the playout forward — a brief gap (a slow write, one missed period)
/// shouldn't extrapolate unbounded. Steady-state writes land every ALSA
/// period (~20–32 ms), so this cap is only reached on a hiccup.
const INTERP_CAP_US: u64 = 120_000;

/// Beyond this many µs since the last `set_position`, treat the clock as
/// **unavailable** (reads return `None`). At this point the audio task is
/// genuinely wedged (codec death, ALSA device hang, channel closed without
/// teardown) and the display loop should fall back to free-running video on
/// wall-clock rather than pacing to a frozen clock forever.
const STALE_US: u64 = 2_000_000;

/// Measured audio-playout clock. Constructed empty; the audio task calls
/// [`AudioClock::set_position`] on the first successful ALSA write and
/// after every write thereafter. Until the first call, all reads return
/// `None`.
#[derive(Debug, Default)]
pub struct AudioClock {
    /// Set true by the first `set_position`. Until then, reads return `None`.
    armed: AtomicBool,
    /// Reference `Instant` (the receiver-side `program_start`), recorded on
    /// the first `set_position` and reused so `base_wall_us` and the reader
    /// share one timeline.
    program_start: OnceLock<Instant>,
    /// The measured DAC playout PTS at the most recent `set_position`, in
    /// 90 kHz units (matching `TsDemuxer`).
    base_pts_90k: AtomicU64,
    /// Microseconds-since-`program_start` at the most recent `set_position`.
    /// The reader interpolates `base_pts_90k` forward by `now − base_wall_us`.
    base_wall_us: AtomicU64,
}

impl AudioClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the measured playout position. `playout_pts_90k` is the PTS of
    /// the sample the DAC is about to play (the source-PTS of the last
    /// queued sample minus the `snd_pcm_delay()` queue depth). `now` is the
    /// wall-clock instant the measurement was taken; the first call's value
    /// is kept as the timeline origin and reused for every later call.
    pub fn set_position(&self, now: Instant, playout_pts_90k: u64) {
        let _ = self.program_start.set(now);
        let now_us = self
            .program_start
            .get()
            .map(|s| s.elapsed().as_micros() as u64)
            .unwrap_or(0);
        self.base_pts_90k.store(playout_pts_90k, Ordering::Relaxed);
        self.base_wall_us.store(now_us, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    /// Raw last-measured playout PTS in 90 kHz units (no interpolation).
    /// `None` until the audio task has set a position. Kept for stats /
    /// debug paths and unit tests; the display loop uses
    /// [`current_pts_90k_smoothed`].
    #[allow(dead_code)]
    pub fn current_pts_90k(&self) -> Option<u64> {
        if !self.armed.load(Ordering::Acquire) {
            return None;
        }
        Some(self.base_pts_90k.load(Ordering::Relaxed))
    }

    /// Current audio playout PTS in 90 kHz units, interpolated forward by
    /// wall-clock since the last [`set_position`]. The DAC plays in real
    /// time, so between ALSA-period writes the true playout advances at
    /// wall-clock rate; interpolating gives the display loop a smooth
    /// per-frame reference instead of a value that steps once per period.
    /// Forward interpolation is capped at [`INTERP_CAP_US`]. If no position
    /// has been published for longer than [`STALE_US`] the audio task is
    /// considered wedged and this returns `None` so the display loop reverts
    /// to wall-clock pacing instead of tracking a frozen clock indefinitely.
    pub fn current_pts_90k_smoothed(&self) -> Option<u64> {
        if !self.armed.load(Ordering::Acquire) {
            return None;
        }
        let base = self.base_pts_90k.load(Ordering::Relaxed);
        let Some(start) = self.program_start.get() else {
            return Some(base);
        };
        let now_us = start.elapsed().as_micros() as u64;
        let last_us = self.base_wall_us.load(Ordering::Relaxed);
        let gap_us = now_us.saturating_sub(last_us);
        if gap_us > STALE_US {
            return None;
        }
        let elapsed_pts = gap_us.min(INTERP_CAP_US).saturating_mul(90) / 1000;
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
    fn position_is_reported_raw() {
        let c = AudioClock::new();
        c.set_position(Instant::now(), 90_000);
        assert_eq!(c.current_pts_90k(), Some(90_000));
        // A later measurement simply overwrites the base (re-anchor on a
        // stream switch is free — the next write's playout PTS wins).
        c.set_position(Instant::now(), 270_000);
        assert_eq!(c.current_pts_90k(), Some(270_000));
    }

    #[test]
    fn smoothed_clock_interpolates_forward_at_wallclock_rate() {
        let c = AudioClock::new();
        c.set_position(Instant::now(), 90_000);
        let base = c.current_pts_90k().unwrap();
        thread::sleep(Duration::from_millis(20));
        let smoothed = c.current_pts_90k_smoothed().unwrap();
        // ~20 ms of wall-clock = ~1800 PTS units; wide tolerance for
        // scheduler jitter.
        let delta = smoothed.wrapping_sub(base);
        assert!(
            (1_200..=3_000).contains(&delta),
            "expected ~1800 PTS interpolation, got {delta}",
        );
    }

    #[test]
    fn interpolation_is_capped_on_stall() {
        let c = AudioClock::new();
        c.set_position(Instant::now(), 1_000_000);
        // Simulate a long stall well past the cap.
        thread::sleep(Duration::from_millis(200));
        let smoothed = c.current_pts_90k_smoothed().unwrap();
        let delta = smoothed.wrapping_sub(1_000_000);
        // Capped at INTERP_CAP_US (120 ms) = 10_800 PTS; never the full 200 ms.
        assert!(
            delta <= 10_800 + 900,
            "interpolation should cap near 120 ms, got {delta} PTS",
        );
    }

    #[test]
    fn shared_arc_is_lock_free_and_monotonic_under_forward_positions() {
        let c = Arc::new(AudioClock::new());
        let start = Instant::now();
        c.set_position(start, 0);
        let writer = {
            let c = Arc::clone(&c);
            thread::spawn(move || {
                let mut pts = 0u64;
                for _ in 0..100 {
                    pts += 960; // ~20 ms @ 48 kHz worth of playout
                    c.set_position(Instant::now(), pts * 90_000 / 48_000);
                    thread::sleep(Duration::from_micros(50));
                }
            })
        };
        let reader = {
            let c = Arc::clone(&c);
            thread::spawn(move || {
                let mut last = 0;
                for _ in 0..100 {
                    if let Some(pts) = c.current_pts_90k() {
                        // Positions only ever move forward in this test.
                        assert!(pts >= last);
                        last = pts;
                    }
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
        assert!(c.current_pts_90k().unwrap() > 0);
    }
}
