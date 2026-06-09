// ALSA blocking-PCM audio backend for the local-display output.
//
// Used by the audio child task on a `display` output. The design has three
// pieces that together give clean, drift-free lip-sync:
//
//  1. **Measured playout clock.** After every successful `snd_pcm_writei`
//     we read `snd_pcm_delay()` (frames written but not yet played) and
//     publish the PTS of the sample the DAC is *actually about to play* to
//     the shared [`AudioClock`]. The display task paces video to that
//     measured point — so there is no estimated clock to drift and no
//     heuristic catch-up to oscillate (both of which the previous
//     open-loop `samples_written / nominal_rate` + ±100 ms snap design
//     suffered from).
//
//  2. **Adaptive resampler.** Two independent crystals (the soundcard DAC
//     vs the source/stream clock) feeding a fixed-size ring *must* drift to
//     empty (xrun) or full (overflow) without sample-rate conversion. A
//     `rubato` cubic resampler runs at a ratio near 1.0, nudged a few ppm
//     per write by a PI loop, so the DAC consumes exactly as fast as the
//     source supplies. The pitch change at sub-percent ratios is inaudible.
//     This replaces the old PES-PTS "catch-up" tiers entirely.
//
//  3. **Clock reference.** In audio-master mode (no master clock attached)
//     the PI loop holds the ALSA buffer at a target fill. In genlock mode
//     (a flow [`MasterClockHandle`] is attached) the PI loop instead drives
//     the measured playout PTS toward `master.now_90khz()`, so the HDMI
//     panel stays coherent with the flow's wire outputs.
//
// On `EPIPE` (xrun) we `prepare()` and re-write; the clock simply
// re-anchors on the next measured position, which is psychoacoustically
// gentler than a fast-forward.

use std::sync::Arc;
use std::time::Instant;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};

use super::clock::AudioClock;
use crate::engine::master_clock::MasterClockHandle;

/// One ALSA writer wrapping a blocking PCM device. Lazily configures
/// sample rate + channels on the first frame so we don't need to know the
/// upstream codec's SR up-front (the caller passes both via `write`).
pub struct AudioBackend {
    device_name: String,
    pcm: Option<PCM>,
    /// SR the PCM was opened at (the source nominal rate). Mid-stream SR
    /// change forces a reopen + resampler rebuild.
    sample_rate: u32,
    /// Channel count of the decoded source we last saw. Reopen on change.
    channels: u8,
    /// True until the first successful write publishes a clock position.
    pending_anchor: bool,

    // ── adaptive resampler ──────────────────────────────────────────
    /// Stereo (post-downmix) variable-ratio resampler. Built lazily on the
    /// first write and rebuilt when the input chunk size changes.
    resampler: Option<Async<f32>>,
    /// Input chunk size (frames/channel) the resampler was built for.
    resampler_chunk_in: usize,
    /// Pre-downmix L/R scratch fed to the resampler (2 rows).
    in_lr: Vec<Vec<f32>>,
    /// Resampler output scratch (2 rows, sized to `output_frames_max`).
    out_lr: Vec<Vec<f32>>,
    /// Heavily-smoothed ALSA buffer fill (frames) — the control input. The
    /// raw `snd_pcm_delay()` is noisy/quantised at the period level;
    /// smoothing it (then slew-limiting the ratio) is what keeps ratio
    /// adjustments at the inaudible ppm scale instead of the per-block
    /// percent-scale swings that sound like tape wow/flutter.
    delay_ema: f64,
    /// Current resample-ratio offset in **ppm**, persisted across writes and
    /// slew-limited per write. In steady state this sits at roughly the
    /// soundcard-vs-source crystal offset (a few hundred ppm = inaudible) and
    /// barely moves.
    ratio_ppm: f64,
    /// Target ALSA buffer fill (frames) the audio-master loop holds.
    /// Set from the actual configured buffer size at open time.
    target_delay_frames: i64,

    // ── genlock (Stage 2) ───────────────────────────────────────────
    /// When `Some`, the PI loop drives the measured playout toward the flow
    /// master clock instead of holding a fixed buffer fill, so the display
    /// stays rate-coherent with the flow's wire outputs.
    master: Option<MasterClockHandle>,
    /// Genlock rate-lock anchor: `(playout_pts_90k, master_now_90k)` captured
    /// once the clock has stabilised on the current stream. The loop drives the
    /// *relative* drift since this anchor to zero, so the absolute PTS-vs-master
    /// scales need not align (the source PTS epoch and the wallclock master
    /// epoch differ).
    genlock_anchor: Option<(i64, i64)>,
    /// Writes remaining before genlock may (re)capture its anchor. Set at open
    /// and after a stream-switch discontinuity so the anchor is taken only once
    /// the ALSA ring has drained the previous stream and the measured playout
    /// reflects the NEW stream — otherwise the anchor catches a mid-drain value
    /// and genlock locks the panel to a wrong (~buffer-depth) phase offset.
    genlock_settle: u32,
}

/// Genlock anchor-settle window (writes). ~0.75 s at the AC-3 block rate —
/// comfortably longer than the ALSA ring depth so the previous stream has
/// fully drained before the anchor is captured.
const GENLOCK_SETTLE_WRITES: u32 = 24;

// Resample-ratio control. The cardinal rule for an audio clock-recovery
// resampler is that the ratio must be a *near-constant* offset that moves
// only in tiny, slow steps — otherwise the listener hears wow/flutter. So we
// (a) heavily smooth the buffer-fill measurement, (b) clamp how far the ratio
// may move per write (SLEW_PPM), and (c) keep the steady-state deviation tiny.
//
/// EMA weight for the buffer-fill measurement (~1.6 s time constant at the
/// ~31 blocks/s AC-3 rate) — removes block-rate noise so the ratio is steady.
const DELAY_EMA_ALPHA: f64 = 0.02;
/// ppm of ratio per unit of normalised buffer error. Proportional control:
/// the buffer settles a little off target (crystal_ppm / this), holding the
/// ratio at the crystal offset with no drift. Small enough that a full-scale
/// error still maps well inside the clamp.
const BUF_GAIN_PPM: f64 = 3_000.0;
/// Genlock: ppm of ratio per ms of measured-playout-vs-master phase error.
/// Gentle — the slew limit below damps any phase-loop overshoot to inaudible.
const GENLOCK_GAIN_PPM_PER_MS: f64 = 20.0;
/// Max ratio change per write, in ppm. ~3 ppm/write ≈ 90 ppm/s — a pitch
/// glide far below the audible-modulation threshold, so even a transient is
/// inaudible and steady state (target reached) is a flat constant ratio.
const SLEW_PPM: f64 = 3.0;
/// Hard clamp on the ratio offset (±0.8 %). Real crystals are ≪ this; the
/// clamp is pure runaway protection.
const MAX_PPM: f64 = 8_000.0;
/// Headroom the resampler is *built* with (output-buffer sizing). Larger than
/// MAX_PPM so the scratch is always big enough.
const MAX_RATIO_DEV: f64 = 0.03;

impl AudioBackend {
    /// Construct without opening — defers `snd_pcm_open` until the first
    /// frame so SR/channels are known. Pass an empty / `None` device name to
    /// disable audio (the caller skips the audio child task).
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            device_name: device_name.into(),
            pcm: None,
            sample_rate: 0,
            channels: 0,
            pending_anchor: true,
            resampler: None,
            resampler_chunk_in: 0,
            in_lr: vec![Vec::new(), Vec::new()],
            out_lr: vec![Vec::new(), Vec::new()],
            delay_ema: 0.0,
            ratio_ppm: 0.0,
            target_delay_frames: 0,
            master: None,
            genlock_anchor: None,
            genlock_settle: 0,
        }
    }

    /// Returns `true` when audio is muted (empty device name); the caller
    /// can skip spawning the audio task in that case.
    pub fn is_muted(&self) -> bool {
        self.device_name.is_empty()
    }

    /// Attach the flow master clock to switch the resampler PI loop into
    /// genlock mode (Stage 2). With no master attached the backend runs in
    /// audio-master mode (holds buffer fill).
    pub fn set_master_clock(&mut self, master: MasterClockHandle) {
        self.master = Some(master);
    }

    /// Reset internal state so the next `write` reopens the PCM and rebuilds
    /// the resampler. Called when stream parameters change or after a fatal
    /// error.
    pub fn reset(&mut self) {
        self.pcm = None;
        self.sample_rate = 0;
        self.channels = 0;
        self.pending_anchor = true;
        self.resampler = None;
        self.resampler_chunk_in = 0;
        self.delay_ema = 0.0;
        self.ratio_ppm = 0.0;
        self.genlock_anchor = None;
        self.genlock_settle = 0;
    }

    /// Write one decoded audio frame's worth of planar f32 samples. Downmixes
    /// to the configured stereo `channel_pair`, resamples by the current
    /// adaptive ratio, writes to ALSA (blocking), then measures the DAC
    /// playout point and publishes it to `clock`.
    ///
    /// Returns `Ok(input_frames_consumed)`.
    pub fn write(
        &mut self,
        planar: &[Vec<f32>],
        pts_90k: u64,
        sample_rate: u32,
        channels: u8,
        clock: &Arc<AudioClock>,
        channel_pair: [u8; 2],
    ) -> Result<usize> {
        if self.is_muted() || planar.is_empty() {
            return Ok(0);
        }
        if sample_rate == 0 || channels == 0 {
            return Ok(0);
        }
        let n_in = planar[0].len();
        if n_in == 0 {
            return Ok(0);
        }

        // Reopen on SR / channel-count drift (rebuilds the resampler too).
        if self.pcm.is_none() || self.sample_rate != sample_rate || self.channels != channels {
            self.open_pcm(sample_rate, channels)?;
        }

        // ── 1. Downmix multichannel → stereo by selecting the pair. ──
        let safe_idx = |idx: usize| -> usize {
            if idx < planar.len() {
                idx
            } else {
                0 // fall back to channel 0 if the operator picked a missing index
            }
        };
        let l_src = &planar[safe_idx(channel_pair[0] as usize)];
        let r_src = &planar[safe_idx(channel_pair[1] as usize)];
        self.in_lr[0].clear();
        self.in_lr[0].extend_from_slice(l_src);
        self.in_lr[1].clear();
        self.in_lr[1].extend_from_slice(r_src);
        // Guard against ragged channel lengths from the decoder.
        let r_len = self.in_lr[1].len();
        if r_len != n_in {
            self.in_lr[1].resize(n_in, 0.0);
        }

        // ── 2. (Re)build the resampler for this input chunk size. ──
        if self.resampler.is_none() || self.resampler_chunk_in != n_in {
            // Nominal ratio 1.0 (we open ALSA at the source rate); the PI
            // loop trims it ±a few ppm. Cubic poly is cheap and clean for
            // sub-percent ratio changes.
            let r = Async::<f32>::new_poly(
                1.0,
                1.0 + MAX_RATIO_DEV + 0.01,
                PolynomialDegree::Cubic,
                n_in,
                2,
                FixedAsync::Input,
            )
            .map_err(|e| anyhow::anyhow!("display audio resampler init: {e}"))?;
            let max_out = r.output_frames_max();
            self.out_lr = vec![vec![0.0f32; max_out], vec![0.0f32; max_out]];
            self.resampler = Some(r);
            self.resampler_chunk_in = n_in;
        }

        // Re-anchor on a large source-PTS discontinuity (operator input
        // switch to a stream with an unrelated PTS epoch). The measured clock
        // tracks playout to within a buffer depth in steady state, so a jump
        // beyond 0.5 s is a stream re-base, not jitter. Drop the genlock
        // anchor so genlock re-locks to the new stream instead of carrying the
        // old epoch's offset (which would saturate the ratio and leave audio
        // permanently off-pitch). The `ratio_ppm` crystal estimate is for the
        // same soundcard so it stays. Mirrors the old explicit re-anchor.
        if let Some(now_pts) = clock.current_pts_90k() {
            if (pts_90k as i64 - now_pts as i64).unsigned_abs() > 45_000 {
                self.genlock_anchor = None;
                self.genlock_settle = GENLOCK_SETTLE_WRITES;
            }
        }

        // ── 3. Compute + apply the adaptive resample ratio. ──
        let rel_ratio = self.compute_ratio(clock);
        let max_out;
        let m_out;
        {
            let r = self.resampler.as_mut().expect("resampler built above");
            r.set_resample_ratio_relative(rel_ratio, true)
                .map_err(|e| anyhow::anyhow!("display audio set ratio: {e}"))?;
            max_out = self.out_lr[0].len();
            let in_adapter = SequentialSliceOfVecs::new(self.in_lr.as_slice(), 2, n_in)
                .map_err(|e| anyhow::anyhow!("display audio in adapter: {e}"))?;
            let mut out_adapter =
                SequentialSliceOfVecs::new_mut(self.out_lr.as_mut_slice(), 2, max_out)
                    .map_err(|e| anyhow::anyhow!("display audio out adapter: {e}"))?;
            let (_used, written) = r
                .process_into_buffer(&in_adapter, &mut out_adapter, None)
                .map_err(|e| anyhow::anyhow!("display audio resample: {e}"))?;
            m_out = written;
        }

        // ── 4. Interleave the resampled stereo to S16 and write. ──
        let mut interleaved: Vec<i16> = Vec::with_capacity(m_out * 2);
        for i in 0..m_out {
            interleaved.push(f32_to_s16(self.out_lr[0][i]));
            interleaved.push(f32_to_s16(self.out_lr[1][i]));
        }

        let pcm = self.pcm.as_ref().expect("pcm opened above");
        let io = pcm.io_i16().context("alsa io_i16")?;
        let mut hit_xrun = false;
        let _written_frames = match io.writei(&interleaved) {
            Ok(n) => n,
            Err(e) if e.errno() == libc::EPIPE => {
                let _ = pcm.prepare();
                // `prepare()` empties the ring, so the post-recovery
                // `snd_pcm_delay()` reflects a freshly-(re)filled buffer — a
                // bogus forward/backward step relative to the pre-xrun
                // playout. Reset the control state and skip publishing this
                // write's position so the clock doesn't lurch; the next clean
                // write re-establishes it.
                hit_xrun = true;
                // Ring just emptied — reseed the smoothed fill at target so the
                // refill transient doesn't kick the ratio. Keep `ratio_ppm`
                // (the crystal estimate is unchanged); drop the genlock anchor.
                self.delay_ema = self.target_delay_frames as f64;
                self.genlock_anchor = None;
                io.writei(&interleaved)
                    .context("alsa writei after prepare")?
            }
            Err(e) => {
                return Err(anyhow::anyhow!("alsa writei failed: {e}"));
            }
        };

        // ── 5. Measure the DAC playout point and publish it. ──
        // delay = frames written but not yet played. The last sample we just
        // queued has source-PTS `pts_90k + n_in/source_rate`; the sample the
        // DAC is about to play is `delay` output-frames behind that. We skip
        // the publish on two degenerate cases so the clock never anchors to a
        // bogus value (the display falls back to wall-clock pacing until the
        // next clean write re-arms it):
        //  - just recovered from an xrun (ring reset, see above);
        //  - startup buffer-fill where the queued delay still exceeds the
        //    stream's elapsed PTS (would publish a spurious ~0 for a source
        //    whose PTS epoch starts near zero).
        if !hit_xrun {
            let delay_frames = pcm.delay().unwrap_or(0).max(0);
            let block_end_src_pts =
                pts_90k.wrapping_add(((n_in as u128) * 90_000 / (sample_rate as u128)) as u64);
            let delay_pts = ((delay_frames as u128) * 90_000 / (sample_rate as u128)) as u64;
            if block_end_src_pts > delay_pts {
                clock.set_position(Instant::now(), block_end_src_pts - delay_pts);
                self.pending_anchor = false;
            }
        }

        Ok(n_in)
    }

    /// Compute the relative resample ratio for the current write.
    ///
    /// Returns a value within `1.0 ± MAX_PPM`, but — crucially — moves it by at
    /// most `SLEW_PPM` per call, so the ratio is effectively a constant offset
    /// that glides imperceptibly. The control target is a *heavily smoothed*
    /// buffer-fill error (audio-master) or measured-playout-vs-master phase
    /// (genlock); smoothing + slew-limiting are what keep this inaudible
    /// (an earlier per-block PI loop swung the ratio ±%-scale = audible wow).
    fn compute_ratio(&mut self, clock: &Arc<AudioClock>) -> f64 {
        let pcm = match self.pcm.as_ref() {
            Some(p) => p,
            None => return 1.0,
        };
        if self.target_delay_frames <= 0 {
            return 1.0;
        }
        // Smooth the (noisy, period-quantised) buffer fill heavily.
        let delay_frames = pcm.delay().unwrap_or(self.target_delay_frames).max(0) as f64;
        self.delay_ema += (delay_frames - self.delay_ema) * DELAY_EMA_ALPHA;
        // +ve when the ring is too EMPTY → output MORE (ratio>1) to refill.
        let buf_err = (self.target_delay_frames as f64 - self.delay_ema)
            / self.target_delay_frames as f64;

        // Copy master "now" out before touching `genlock_anchor` (avoids
        // holding a borrow of `self.master` across the `get_or_insert`).
        let master_now: Option<i64> = self.master.as_ref().map(|m| m.now_90khz() as i64);

        // Target ratio offset (ppm) for this write. Proportional control: the
        // buffer/phase settles a little off target, holding the ratio at the
        // crystal/source-vs-master offset with no long-term drift.
        let target_ppm = match master_now {
            // ── Genlock: lock measured playout to the master rate. ──
            // Only once the clock is armed (a real measured playout exists);
            // until then hold buffer fill and defer capturing the anchor
            // (anchoring against the source pts would seed a bogus phase error).
            Some(master_now) if self.genlock_settle == 0 => {
                match clock.current_pts_90k_smoothed() {
                    Some(playout) => {
                        let playout = playout as i64;
                        let (pa, ma) =
                            *self.genlock_anchor.get_or_insert((playout, master_now));
                        // Relative drift since the anchor, in ms. +ve = playout
                        // advanced MORE than master (audio fast) → stretch (ratio>1).
                        let phase_ms = (((playout - pa) - (master_now - ma)) as f64) / 90.0;
                        GENLOCK_GAIN_PPM_PER_MS * phase_ms + BUF_GAIN_PPM * 0.3 * buf_err
                    }
                    None => BUF_GAIN_PPM * buf_err,
                }
            }
            // Still settling after open / a switch (or no master): hold buffer
            // fill and defer the anchor so it's captured on the new stream.
            Some(_) => {
                self.genlock_settle = self.genlock_settle.saturating_sub(1);
                BUF_GAIN_PPM * buf_err
            }
            // ── Audio-master: hold the buffer at target fill. ──
            None => BUF_GAIN_PPM * buf_err,
        }
        .clamp(-MAX_PPM, MAX_PPM);

        // Slew-limit the persistent ratio toward the target: a couple of ppm
        // per write at most, so any correction is a slow inaudible glide and
        // steady state (target reached) is a flat constant ratio.
        let step = (target_ppm - self.ratio_ppm).clamp(-SLEW_PPM, SLEW_PPM);
        self.ratio_ppm = (self.ratio_ppm + step).clamp(-MAX_PPM, MAX_PPM);
        1.0 + self.ratio_ppm * 1e-6
    }

    fn open_pcm(&mut self, sample_rate: u32, channels: u8) -> Result<()> {
        let pcm = PCM::new(&self.device_name, Direction::Playback, false).with_context(|| {
            format!(
                "display_audio_open_failed: snd_pcm_open '{}'",
                self.device_name
            )
        })?;
        let (buffer_frames, _period_frames) = {
            let hwp = HwParams::any(&pcm).context("alsa HwParams::any")?;
            hwp.set_channels(2).context("alsa set_channels(2)")?;
            hwp.set_rate(sample_rate, ValueOr::Nearest)
                .context("alsa set_rate")?;
            hwp.set_format(Format::s16()).context("alsa set_format(S16)")?;
            hwp.set_access(Access::RWInterleaved)
                .context("alsa set_access(RWInterleaved)")?;
            // Period ≈ 20 ms at 48 kHz = 960 frames; buffer = 4 periods.
            let period = (sample_rate as i64 / 50).max(64) as alsa::pcm::Frames;
            let _ = hwp.set_period_size_near(period, ValueOr::Nearest);
            let _ = hwp.set_buffer_size_near((period * 4).max(256));
            pcm.hw_params(&hwp).context("alsa hw_params")?;
            let buf = hwp.get_buffer_size().unwrap_or((period * 4) as alsa::pcm::Frames);
            let per = hwp.get_period_size().unwrap_or(period);
            (buf as i64, per as i64)
        };
        pcm.prepare().context("alsa prepare")?;
        self.pcm = Some(pcm);
        self.sample_rate = sample_rate;
        self.channels = channels;
        // Hold the ring at roughly half-full: deep enough to ride period
        // jitter, shallow enough to keep end-to-end audio latency low.
        self.target_delay_frames = (buffer_frames / 2).max(1);
        // Seed the smoothed fill at target so the initial buffer-fill transient
        // doesn't kick the ratio; start the ratio flat.
        self.delay_ema = self.target_delay_frames as f64;
        self.ratio_ppm = 0.0;
        // Force a resampler rebuild for the (possibly) new chunk size.
        self.resampler = None;
        self.resampler_chunk_in = 0;
        self.genlock_anchor = None;
        self.genlock_settle = GENLOCK_SETTLE_WRITES;
        Ok(())
    }
}

/// Saturating planar-f32 → s16 conversion. Clipping below -1.0 / above 1.0
/// is bounded; the operator can monitor `audio_underruns` for real
/// distortion via `DisplayStats`.
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

    #[test]
    fn ratio_stays_bounded_audio_master() {
        // With no PCM open compute_ratio short-circuits to 1.0; this guards
        // the clamp arithmetic against pathological target/delay values.
        let mut b = AudioBackend::new("hw:0,0");
        b.target_delay_frames = 1920;
        b.ratio_ppm = 1e9; // pathological
        // No pcm → returns 1.0 (can't measure delay without a device).
        let clock = Arc::new(AudioClock::new());
        let r = b.compute_ratio(&clock);
        assert_eq!(r, 1.0);
    }
}
