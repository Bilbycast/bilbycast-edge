// ALSA blocking-PCM audio backend for the local-display output.
//
// Used by the audio child task on a `display` output. Blocking
// `snd_pcm_writei()` *is* the master clock — every successful write of
// N samples advances the shared `AudioClock` by `N / sample_rate`
// seconds. When ALSA returns `EPIPE` (xrun), we `prepare()` and keep
// going without nudging the anchor — the clock simply pauses for the
// recovery window, which is psychoacoustically less harmful than a
// fast-forward.

use std::sync::Arc;
use std::time::Instant;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result};

use super::clock::AudioClock;

/// One ALSA writer wrapping a blocking PCM device. Lazily configures
/// sample rate + channels on the first frame so we don't need to know
/// the upstream codec's SR up-front (the caller passes both via
/// `write`).
pub struct AudioBackend {
    device_name: String,
    pcm: Option<PCM>,
    /// SR the PCM was opened at. Mid-stream SR change forces a reopen.
    sample_rate: u32,
    /// Channel count we opened the PCM for. Same — reopen on change.
    channels: u8,
    /// `set_anchor` hasn't fired yet. Used to seed the AudioClock with
    /// the first frame's PTS.
    pending_anchor: bool,
    /// Running sum of per-write gap-PTS values that fell *below* the
    /// per-frame `PTS_GAP_CATCHUP_MS` threshold. Catches the slow-drift
    /// failure mode where many small (~21 ms) silent audio drops in
    /// the upstream `atx` mpsc never individually trip the catch-up
    /// but cumulatively pull `audio_pts` seconds behind real time.
    /// When the running sum crosses `PTS_CUMULATIVE_CATCHUP_MS`, we
    /// snap the `AudioClock` forward by the cumulative amount and
    /// reset. Reset on re-anchor too (a stream switch invalidates
    /// any drift accumulated against the previous source).
    cumulative_gap_pts: i64,
}

impl AudioBackend {
    /// Construct without opening — defers the `snd_pcm_open` until the
    /// first frame so SR/channels are known. Pass an empty / `None`
    /// device name to disable audio entirely (the caller skips the
    /// audio child task).
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            device_name: device_name.into(),
            pcm: None,
            sample_rate: 0,
            channels: 0,
            pending_anchor: true,
            cumulative_gap_pts: 0,
        }
    }

    /// Returns `true` when audio is muted (empty device name); the
    /// caller can skip spawning the audio task in that case.
    pub fn is_muted(&self) -> bool {
        self.device_name.is_empty()
    }

    /// Reset internal state so the next `write` reopens the PCM. Called
    /// when the upstream stream parameters change or after a fatal
    /// error.
    pub fn reset(&mut self) {
        self.pcm = None;
        self.sample_rate = 0;
        self.channels = 0;
        self.pending_anchor = true;
        self.cumulative_gap_pts = 0;
    }

    /// Write one decoded audio frame's worth of planar f32 samples.
    /// `pts_90k` is the demuxer-side PTS — used to seed the
    /// `AudioClock` anchor on the first successful write. Updates
    /// `clock` after each successful write so the display task can
    /// dup/drop video frames.
    ///
    /// Returns `Ok(samples_per_channel_written)`; partial writes are
    /// possible under xrun recovery and the caller should treat the
    /// next batch's PTS accordingly.
    pub fn write(
        &mut self,
        planar: &[Vec<f32>],
        pts_90k: u64,
        sample_rate: u32,
        channels: u8,
        clock: &Arc<AudioClock>,
        program_start: Instant,
        channel_pair: [u8; 2],
    ) -> Result<usize> {
        if self.is_muted() || planar.is_empty() {
            return Ok(0);
        }
        if sample_rate == 0 || channels == 0 {
            return Ok(0);
        }
        // Reopen on SR / channel-count drift.
        if self.pcm.is_none() || self.sample_rate != sample_rate || self.channels != channels {
            self.open_pcm(sample_rate, channels)?;
        }
        // Downmix multichannel → stereo by selecting the configured
        // pair of channels. ALSA opens with `channels=2` (we always
        // render stereo to the operator's confidence-monitor sink),
        // so the writer always interleaves exactly two channels.
        let l_idx = channel_pair[0] as usize;
        let r_idx = channel_pair[1] as usize;
        let nb = planar[0].len();
        let mut interleaved: Vec<i16> = Vec::with_capacity(nb * 2);
        let safe_idx = |idx: usize| -> usize {
            if idx < planar.len() {
                idx
            } else {
                0 // fall back to channel 0 if the operator picked a
                  // channel index that doesn't exist on this stream
            }
        };
        let l = &planar[safe_idx(l_idx)];
        let r = &planar[safe_idx(r_idx)];
        for i in 0..nb {
            interleaved.push(f32_to_s16(l[i]));
            interleaved.push(f32_to_s16(r[i]));
        }

        let pcm = self
            .pcm
            .as_ref()
            .expect("pcm opened above by open_pcm");
        let io = pcm.io_i16().context("alsa io_i16")?;

        // Blocking write. On EPIPE (xrun) the alsa crate already
        // recovers internally for us via `try_recover`, but we re-attempt
        // once on EPIPE if the recover path didn't.
        let written = match io.writei(&interleaved) {
            Ok(n) => n,
            Err(e) if e.errno() == libc::EPIPE => {
                let _ = pcm.prepare();
                io.writei(&interleaved).context("alsa writei after prepare")?
            }
            Err(e) => {
                return Err(anyhow::anyhow!("alsa writei failed: {e}"));
            }
        };

        // Anchor the AudioClock on the very first successful write, or
        // re-anchor on a large PTS discontinuity (e.g. operator switched
        // the flow's active input — the new stream has an unrelated PTS
        // base, so the clock's drift comparison would otherwise flag every
        // new video frame as multi-second-late and drop them all).
        let needs_re_anchor = if self.pending_anchor {
            true
        } else if let Some(now_pts) = clock.current_pts_90k() {
            // 90 kHz × 0.5 s = 45 000. A real continuous stream tracks the
            // ALSA clock to within a few hundred PTS units; anything past
            // half a second is a stream change.
            let diff = pts_90k.wrapping_sub(now_pts) as i64;
            diff.unsigned_abs() > 45_000
        } else {
            false
        };
        if needs_re_anchor {
            clock.set_anchor(program_start, pts_90k, sample_rate);
            self.pending_anchor = false;
            self.cumulative_gap_pts = 0;
            clock.advance(written as u64);
        } else {
            // Steady-state clock advance. Naively `advance(written)` only
            // counts samples we actually pushed to ALSA — which is wrong
            // when a *significant* run of audio frames fails to decode
            // (mid-stream codec hiccup, splitter resync) or get skipped
            // at the decoder. Each missed frame leaves a hole the clock
            // never sees, the display task reads `audio_pts` as N ms
            // behind reality, computes drift = +N ms (video looks
            // ahead), and back-pressures the bounded video mpsc.
            //
            // Two-tier catch-up:
            //
            // 1. Per-write spike catch-up at PTS_GAP_CATCHUP_MS — fires
            //    on a *single* PES PTS jump bigger than two frame
            //    periods (mid-stream codec hiccup, splitter resync).
            //
            // 2. Cumulative drift catch-up at PTS_CUMULATIVE_CATCHUP_MS
            //    — fires when many sub-threshold gaps add up. This
            //    catches the silent-mpsc-drop failure mode where the
            //    audio `atx` queue fills during a decode burst and
            //    drops single ~21 ms AAC frames; each drop is below
            //    the per-write threshold but the lag accumulates as a
            //    growing `+av_sync_offset_ms` (video drifting "ahead"
            //    of audio). Without this tier the clock could lag real
            //    time by seconds within a few minutes of playout on
            //    bursty streams.
            //
            // We still don't want to chase encoder PTS micro-jitter
            // (1–2 ms / frame, oscillating around zero, well-known on
            // DVB MPEG-2 audio streams). Cumulative summation handles
            // that naturally — symmetric jitter averages out near
            // zero and never crosses the threshold; one-sided drift
            // accumulates and trips the catch-up.
            let now_pts = clock.current_pts_90k().unwrap_or(pts_90k);
            let written_pts = ((written as u128) * 90_000 / (sample_rate as u128)) as u64;
            let expected_clock_after_write = now_pts.wrapping_add(written_pts);
            let target_clock = pts_90k.wrapping_add(written_pts);
            let gap_pts = target_clock.wrapping_sub(expected_clock_after_write) as i64;
            // Two AAC frames = ~43 ms; two AC-3 frames = 64 ms; two MP2
            // frames = 48 ms. 50 ms covers all three with margin —
            // smaller jumps stay treated as benign jitter (handled by
            // the cumulative tier).
            const PTS_GAP_CATCHUP_MS: i64 = 50;
            // 100 ms cumulative drift = ~5 silent AAC drops or ~3 AC-3
            // drops. Big enough that legitimate symmetric encoder
            // jitter (±1–2 ms) won't trip it within thousands of
            // frames; small enough that a real one-sided drift fires
            // catch-up before the operator sees a/v slip on screen.
            const PTS_CUMULATIVE_CATCHUP_MS: i64 = 100;
            let mut extra_samples: u64 = 0;
            if gap_pts > PTS_GAP_CATCHUP_MS * 90 {
                // Big single-frame jump — snap immediately, reset
                // cumulative (it's been absorbed).
                extra_samples =
                    ((gap_pts as u128) * (sample_rate as u128) / 90_000) as u64;
                self.cumulative_gap_pts = 0;
            } else {
                // Small per-frame gap — accumulate. May be jitter (±)
                // or a slow drift (+). Only the latter trips the
                // cumulative threshold.
                self.cumulative_gap_pts =
                    self.cumulative_gap_pts.saturating_add(gap_pts);
                if self.cumulative_gap_pts > PTS_CUMULATIVE_CATCHUP_MS * 90 {
                    extra_samples = ((self.cumulative_gap_pts as u128)
                        * (sample_rate as u128)
                        / 90_000) as u64;
                    self.cumulative_gap_pts = 0;
                } else if self.cumulative_gap_pts < -(PTS_CUMULATIVE_CATCHUP_MS * 90) {
                    // Mirror case — clock running ahead of source by a
                    // sustained margin. Pull it back so the display's
                    // drift estimator doesn't drop video forever.
                    // Subtracting from `written` would wrap underflow,
                    // so we just zero the advance for this write and
                    // reset the running sum; the clock holds on the
                    // previous frame's position for one period.
                    self.cumulative_gap_pts = 0;
                    clock.advance(0);
                    return Ok(written);
                }
            }
            let advance_samples = (written as u64).saturating_add(extra_samples);
            clock.advance(advance_samples);
        }
        Ok(written)
    }

    fn open_pcm(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        let pcm = PCM::new(&self.device_name, Direction::Playback, false)
            .with_context(|| format!("display_audio_open_failed: snd_pcm_open '{}'", self.device_name))?;
        {
            let hwp = HwParams::any(&pcm).context("alsa HwParams::any")?;
            hwp.set_channels(2)
                .context("alsa set_channels(2)")?;
            hwp.set_rate(sample_rate, ValueOr::Nearest)
                .context("alsa set_rate")?;
            hwp.set_format(Format::s16())
                .context("alsa set_format(S16)")?;
            hwp.set_access(Access::RWInterleaved)
                .context("alsa set_access(RWInterleaved)")?;
            // Period ≈ 20 ms at 48 kHz = 960 frames; buffer = 4 periods.
            let _ = hwp.set_period_size_near(
                (sample_rate as i64 / 50).max(64) as alsa::pcm::Frames,
                ValueOr::Nearest,
            );
            let _ = hwp.set_buffer_size_near(
                (sample_rate as i64 / 50 * 4).max(256) as alsa::pcm::Frames,
            );
            pcm.hw_params(&hwp).context("alsa hw_params")?;
        }
        pcm.prepare().context("alsa prepare")?;
        self.pcm = Some(pcm);
        self.sample_rate = sample_rate;
        self.channels = channels;
        Ok(())
    }
}

/// Saturating planar-f32 → s16 conversion. Clipping below -1.0 / above
/// 1.0 is bounded; the operator can monitor `audio_underruns` for
/// real distortion via `DisplayStats`.
fn f32_to_s16(s: f32) -> i16 {
    let scaled = (s.clamp(-1.0, 1.0) * 32767.0).round();
    scaled as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muted_backend_skips_open() {
        let mut b = AudioBackend::new("");
        assert!(b.is_muted());
        let clock = Arc::new(AudioClock::new());
        let n = b
            .write(
                &[vec![0.0_f32; 480], vec![0.0_f32; 480]],
                0,
                48_000,
                2,
                &clock,
                Instant::now(),
                [0, 1],
            )
            .unwrap();
        assert_eq!(n, 0);
        assert!(clock.current_pts_90k().is_none());
    }

    #[test]
    fn f32_to_s16_clips_correctly() {
        assert_eq!(f32_to_s16(0.0), 0);
        assert_eq!(f32_to_s16(1.0), 32767);
        assert_eq!(f32_to_s16(-1.0), -32767);
        assert_eq!(f32_to_s16(2.0), 32767);
        assert_eq!(f32_to_s16(-2.0), -32767);
    }
}
