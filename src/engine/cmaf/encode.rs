// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Phase 3 audio & video re-encoders for CMAF outputs.
//!
//! Both are callable from the hot path via `block_in_place` so the
//! broadcast subscriber task does not lose ordering with other outputs.
//!
//! # Audio re-encoder
//!
//! Source AAC frame → `AacDecoder` → channel stage (the `transcode`
//! block's routing, or the standard downmix `audio_encode.channels` asks
//! for; `audio_transcode::encoder_stage`) → `AudioEncoder`, which converts
//! the rate (the block's `sample_rate`, else `audio_encode.sample_rate`)
//! in its own resampler → target AAC frame(s).
//!
//! # Video re-encoder
//!
//! Source H.264/HEVC access unit → Annex-B framing → `VideoDecoder`
//! → `VideoEncoder` (x264 / x265 / NVENC, feature-gated) → target
//! H.264/HEVC NAL units. The operator's `gop_size` is honoured (60 when
//! unset); CMAF segment boundaries land on that GOP's IDRs.

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;

use crate::config::models::{AudioEncodeConfig, VideoEncodeConfig};
use crate::engine::audio_decode::AacDecoder;
use crate::engine::audio_encode::{AudioCodec, AudioEncoder, EncoderParams};
use crate::engine::audio_silence::SilenceGenerator;
use crate::stats::collector::OutputStatsAccumulator;

use super::fmp4::VideoCodec as CmafVideoCodec;

// ────────────────────────────────────────────────────────────────────────
//  Audio re-encoder
// ────────────────────────────────────────────────────────────────────────

pub struct AudioReencoder {
    decoder: Option<AacDecoder>,
    encoder: Option<AudioEncoder>,
    target_codec: AudioCodec,
    target_bitrate_kbps: u32,
    /// The output rate and channel count asked for: the `transcode`
    /// block's, else `audio_encode`'s (`None` = the source's).
    target_sample_rate: Option<u32>,
    target_channels: Option<u8>,
    /// The `transcode` block without its `sample_rate` (the encoder's
    /// resampler converts the rate): what the channel stage routes by.
    channel_block: Option<crate::engine::audio_transcode::TranscodeJson>,
    /// The channel stage between decoder and encoder, rate-neutral, and the
    /// decoded format it was built for. It replaced a mix that took L / R
    /// of a 5.1 source (losing the centre) and averaged stereo to mono,
    /// whatever the `transcode` block said — which was ignored.
    layout: Option<crate::engine::audio_transcode::PlanarAudioTranscoder>,
    layout_in: (u32, u8),
    lazy_init_params: LazyAudioInit,
    flow_id: String,
    output_id: String,
    cancel: CancellationToken,
    out_stats: Arc<OutputStatsAccumulator>,
    /// Silent-PCM generator + audio-drop watchdog. `Some` iff
    /// `audio_encode.silent_fallback = true` on the CMAF output's
    /// config — the encoder is then built eagerly (see [`Self::new`])
    /// so silent AAC frames can start flowing before any source
    /// audio arrives.
    silence: Option<SilenceGenerator>,
    /// Where the encoder's input is on the source's timeline: the PTS it was
    /// (re-)anchored at, and how many input samples — real and silent — it
    /// has taken since, at `input_sr`. The encoder free-runs from one anchor
    /// and ignores every later PTS, so this is the only way to know when
    /// the source and the encoder have parted company; and it is kept as a
    /// running count divided once, not a sum of per-submit steps, so a
    /// 44.1 kHz rounding does not drift it.
    input_anchor_pts: Option<u64>,
    input_since_anchor: u64,
    input_sr: u32,
    /// How far the newest video DTS runs ahead of the newest audio PTS, the
    /// largest seen over the current run of real audio. A hardware encoder
    /// sends video ahead of its DTS by its VBV delay and audio close to its
    /// PTS, so filling silence to the picture's position would run it past
    /// where the audio is, and every return of real audio would then step
    /// backwards — a zero-duration sample and a fragment presented late.
    /// Silence is filled to the picture less this.
    video_lead_90k: u64,
    /// True while silence is being laid down, so the first real frame after
    /// it resets the lead measurement.
    in_silence: bool,
}

/// How far a submit's PTS may sit from where the encoder's input is before
/// the encoder's output timeline is re-anchored on it, or the submit held
/// back.
///
/// The in-process encoder free-runs from one anchor (see
/// `AudioEncoder::reanchor_pts`), so a splice, a lost PES, a wrap the
/// unwrapper did not absorb, or silence inserted between two contiguous
/// real frames would otherwise leave the audio on a timeline the picture is
/// no longer on — for good. Compared against where the input *is* rather
/// than against the previous submit, so a source that packs half a second
/// of audio into one PES is not a jump, and a 200 ms silence burst between
/// two contiguous frames is. One 48 kHz frame plus the rounding a
/// PES-to-frame PTS carries; anything a seam lets through here is an
/// offset the picture never shares, so it has to sit inside the ±40 ms the
/// A/V gate allows.
const SOURCE_PTS_SLACK_TICKS: u64 = 2_000;

/// How much silence one fill may emit before it is treated as a jump in the
/// picture rather than a gap to cover: a splice or a wrap on the video moves
/// the target by hours, and nobody wants hours of silence encoded to reach it.
const SILENCE_FILL_CAP_TICKS: u64 = 4 * 90_000;

struct LazyAudioInit {
    /// ADTS config tuple cached by the demuxer. `None` until first
    /// AAC frame observed.
    adts_config: Option<(u8, u8, u8)>,
}

impl AudioReencoder {
    pub fn new(
        cfg: &AudioEncodeConfig,
        transcode: Option<&crate::engine::audio_transcode::TranscodeJson>,
        cancel: &CancellationToken,
        output_id: &str,
        flow_id: &str,
    ) -> Result<Self> {
        // The block wins over audio_encode's fields, as on every output.
        let target_sample_rate = transcode.and_then(|t| t.sample_rate).or(cfg.sample_rate);
        let target_channels = transcode.and_then(|t| t.channels).or(cfg.channels);
        let target_codec = AudioCodec::parse(&cfg.codec)
            .ok_or_else(|| anyhow::anyhow!("unknown audio codec: {}", cfg.codec))?;
        // CMAF audio is AAC only in Phase 3 — reject exotic codecs
        // early so operators see the error at startup, not at runtime.
        if !matches!(target_codec, AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2) {
            bail!(
                "CMAF audio_encode rejects codec '{}' — allowed: aac_lc, he_aac_v1, he_aac_v2",
                cfg.codec
            );
        }
        let target_bitrate_kbps = cfg
            .bitrate_kbps
            .unwrap_or_else(|| target_codec.default_bitrate_kbps());
        let out_stats = Arc::new(OutputStatsAccumulator::new(
            "cmaf-audio-encode".to_string(),
            "cmaf-audio-encode".to_string(),
            "cmaf-audio-encode".to_string(),
        ));

        // Silent-fallback path: build the encoder eagerly using the
        // declared target sample_rate / channels (defaulting to 48 kHz
        // stereo) so the caller can start pushing silent PCM before
        // any source audio ever shows up. The source decoder stays
        // `None` until the first real AAC frame arrives.
        let (encoder, silence) = if cfg.silent_fallback {
            let sr = target_sample_rate.unwrap_or(48_000);
            let ch = target_channels.unwrap_or(2).clamp(1, 2);
            let params = EncoderParams {
                codec: target_codec,
                sample_rate: sr,
                channels: ch,
                target_bitrate_kbps,
                target_sample_rate: sr,
                target_channels: ch,
                opus_vbr_mode: cfg.opus_vbr_mode.clone(),
                opus_fec: cfg.opus_fec,
                opus_dtx: cfg.opus_dtx,
                opus_frame_duration_ms: cfg.opus_frame_duration_ms,
            };
            // A child, so retiring this encoder later cancels it alone.
            let enc = AudioEncoder::spawn(
                params,
                cancel.child_token(),
                flow_id.to_string(),
                output_id.to_string(),
                out_stats.clone(),
                None,
            )
            .map_err(|e| anyhow::anyhow!(
                "CMAF audio_encode(silent_fallback) spawn failed: {e}"
            ))?;
            (Some(enc), Some(SilenceGenerator::new(sr, ch, 0)))
        } else {
            (None, None)
        };

        Ok(Self {
            decoder: None,
            encoder,
            target_codec,
            target_bitrate_kbps,
            target_sample_rate,
            target_channels,
            channel_block: transcode.map(|t| crate::engine::audio_transcode::TranscodeJson {
                sample_rate: None,
                ..t.clone()
            }),
            layout: None,
            layout_in: (0, 0),
            lazy_init_params: LazyAudioInit { adts_config: None },
            flow_id: flow_id.to_string(),
            output_id: output_id.to_string(),
            cancel: cancel.clone(),
            out_stats,
            silence,
            input_anchor_pts: None,
            input_since_anchor: 0,
            input_sr: 0,
            video_lead_90k: 0,
            in_silence: false,
        })
    }

    /// Whether this re-encoder was built with `silent_fallback = true`.
    /// When true, the caller should drive a silence tick at
    /// [`Self::silence_chunk_duration`] and call
    /// [`Self::encode_silence_if_needed`] each tick.
    pub fn has_silent_fallback(&self) -> bool {
        self.silence.is_some()
    }

    /// Tokio-interval period for the silence watchdog tick. `None`
    /// unless `silent_fallback` is active.
    pub fn silence_chunk_duration(&self) -> Option<std::time::Duration> {
        self.silence.as_ref().map(|sg| sg.chunk_duration())
    }

    /// AudioSpecificConfig tuple `(profile, sr_index, ch_cfg)` describing
    /// what the encoder emits, once it exists: its target rate and layout,
    /// which a source at any other rate or layout is converted to. `None`
    /// before the first frame has built it, or for a target rate with no
    /// ADTS index.
    pub fn encoder_track(&self) -> Option<(u8, u8, u8)> {
        let p = self.encoder.as_ref()?.params();
        let sr_idx = crate::engine::audio_decode::sr_index_from_hz(p.target_sample_rate)?;
        Some((1, sr_idx, p.target_channels))
    }

    /// AudioSpecificConfig tuple `(profile, sr_index, ch_cfg)` for the
    /// silent-fallback track, so the caller can eagerly build the
    /// CMAF `AudioSegmenter` before any source audio arrives. `None`
    /// unless `silent_fallback` is active OR the declared sample_rate
    /// isn't a standard ADTS-indexed rate.
    pub fn silent_fallback_track(&self) -> Option<(u8, u8, u8)> {
        let sr = self.target_sample_rate.unwrap_or(48_000);
        let ch = self.target_channels.unwrap_or(2);
        let sr_idx = crate::engine::audio_decode::sr_index_from_hz(sr)?;
        self.silence.as_ref()?;
        Some((1, sr_idx, ch))
    }

    /// Where on the source's timeline the encoder's next input sample
    /// belongs, if it has been anchored.
    fn input_position(&self) -> Option<u64> {
        let anchor = self.input_anchor_pts?;
        Some(anchor.saturating_add(self.input_since_anchor * 90_000 / self.input_sr.max(1) as u64))
    }

    /// Start the encoder's input timeline afresh at `pts`.
    fn anchor_input(&mut self, pts: u64) {
        if let Some(enc) = self.encoder.as_mut() {
            enc.reanchor_pts();
            self.input_sr = enc.params().sample_rate;
        }
        self.input_anchor_pts = Some(pts);
        self.input_since_anchor = 0;
    }

    /// Produce silent AAC frames `(data, pts)` if the drop watchdog says real
    /// audio is absent or stalled — as many as it takes to bring the silence
    /// up to where the audio would be, given where the picture is
    /// (`video_pts_90k`, the newest video DTS). Idempotent and cheap when
    /// audio is flowing: an empty vec when `silent_fallback` is off, when
    /// real audio arrived within the grace window, or when the silence has
    /// already reached the picture. The PTS on each frame is the encoder's
    /// output PTS, the same timeline the real-audio path stamps.
    ///
    /// Filled to the picture, not ticked by the clock. The tick is the
    /// watchdog; it used to also be the meter — one chunk per tick, and every
    /// tick the output loop observed late (a segment PUT, a re-encode in
    /// `block_in_place`) was a chunk the audio never made up, so a video-only
    /// feed's silence fell a second or two a minute behind its picture until
    /// MSE ran out of buffered audio at the playhead. And a delivery stall
    /// ticked silence into the encoder while the picture stood still, so the
    /// real audio that followed was stamped that much late for the life of
    /// the flow. Measured against the picture, neither happens: a stall
    /// advances neither track, and lost ticks are made up on the next.
    ///
    /// Filled from where the encoder's input is, and to the picture less the
    /// video's lead over the audio (`video_lead_90k`) — where the audio
    /// itself would be — so real audio returning lands where the silence
    /// ends rather than behind it.
    ///
    /// Nothing is emitted until the caller has a picture to measure against.
    /// The generator is built before any media has arrived; the first silent
    /// chunk used to be submitted at zero, the encoder anchored its output
    /// counter there once and for all, and every silent frame from startup
    /// sat hours below the video's DTS.
    pub fn encode_silence_if_needed(
        &mut self,
        video_pts_90k: Option<u64>,
    ) -> Result<Vec<(Vec<u8>, u64)>> {
        let Some(sg) = self.silence.as_ref() else {
            return Ok(Vec::new());
        };
        if !sg.should_emit() || self.encoder.is_none() {
            return Ok(Vec::new());
        }
        let Some(video) = video_pts_90k else {
            return Ok(Vec::new());
        };
        let target = video.saturating_sub(self.video_lead_90k);
        match self.input_position() {
            None => self.anchor_input(target),
            // A picture that moved further than any gap this would fill is a
            // splice or a wrap: the silence follows it rather than chasing
            // it, and the encoder is told the same way a source jump tells it.
            Some(at) if target.saturating_sub(at) > SILENCE_FILL_CAP_TICKS => {
                self.anchor_input(target)
            }
            Some(_) => {}
        }
        let chunk_samples = self
            .silence
            .as_ref()
            .map(|sg| sg.chunk_samples() as u64)
            .unwrap_or(0);
        let chunk_ticks = chunk_samples * 90_000 / self.input_sr.max(1) as u64;
        let mut out = Vec::new();
        while let Some(at) = self.input_position().filter(|at| at + chunk_ticks <= target) {
            let (sg, enc) = match (self.silence.as_mut(), self.encoder.as_mut()) {
                (Some(sg), Some(enc)) => (sg, enc),
                _ => break,
            };
            // Submitted at the input position, which is what a freshly
            // (re-)anchored encoder takes its output timeline from.
            let (planar, _) = sg.next_chunk();
            enc.submit_planar(planar, at);
            self.input_since_anchor += chunk_samples;
            self.in_silence = true;
            while let Some(f) = enc.try_recv() {
                out.push((f.data.to_vec(), f.pts));
            }
        }
        Ok(out)
    }

    /// What to do with a real submit at `pts`, given where the encoder's
    /// input is. See [`SOURCE_PTS_SLACK_TICKS`]. Frames a forward gap is
    /// covered with go to `out`.
    fn place_source(&mut self, pts: u64, out: &mut Vec<(Vec<u8>, u64)>) -> Placement {
        let Some(at) = self.input_position() else {
            self.anchor_input(pts);
            return Placement::Submit;
        };
        if pts > at + SOURCE_PTS_SLACK_TICKS {
            let gap = pts - at;
            if self.silence.is_some() && gap <= SILENCE_FILL_CAP_TICKS {
                // A gap the silent fallback exists to cover: the fill stops
                // one chunk short of the picture, and a return lands past
                // that residual — up to a chunk at the source's rate, most
                // of a frame at 16 kHz. Covered with zeros and continued,
                // rather than re-anchored and the last silent sample
                // stretched over it.
                let samples = gap * self.input_sr.max(1) as u64 / 90_000;
                let channels = self
                    .encoder
                    .as_ref()
                    .map_or(2, |e| e.params().channels as usize);
                let zeros = vec![vec![0.0f32; samples as usize]; channels];
                if let Some(enc) = self.encoder.as_mut() {
                    enc.submit_planar(&zeros, at);
                    self.input_since_anchor += samples;
                    while let Some(f) = enc.try_recv() {
                        out.push((f.data.to_vec(), f.pts));
                    }
                }
                return Placement::Submit;
            }
            // A gap: the source moved on without the encoder. Re-anchor.
            tracing::debug!(
                "CMAF output '{}': audio source PTS {pts} is {gap} ticks past the encoder's \
                 input; re-anchoring",
                self.output_id,
            );
            self.anchor_input(pts);
            Placement::Submit
        } else if pts + SOURCE_PTS_SLACK_TICKS < at {
            // The source is behind the encoder's input: silence overshot the
            // audio's return, by the part of the video lead the estimate
            // missed. Going backwards would stamp this frame before the
            // silence already handed to the segmenter — a zero-duration
            // sample, and a fragment presented late. Hold the source back
            // until it catches up; what is lost is the overlap, once.
            if at - pts > SILENCE_FILL_CAP_TICKS {
                self.anchor_input(pts);
                Placement::Submit
            } else {
                Placement::Skip
            }
        } else {
            Placement::Submit
        }
    }

    /// Make sure the encoder takes input at the source's rate and channel
    /// count. The eager silent-fallback encoder is built at the declared
    /// output rate before any source has been seen, and consumed a 44.1 kHz
    /// source as though it were 48 kHz: 8.8 % slow, a third of a semitone
    /// flat, and a drift no PTS comparison could see. Rebuilt on the first
    /// real frame that differs; the silence already emitted was at the
    /// track's rate and is unaffected.
    ///
    /// The encoder's input layout is always its output layout — the
    /// in-process backends take no other, and refuse it at spawn — so a
    /// source at another channel count goes through the channel stage on
    /// the way in (`ensure_layout`), and only its rate decides whether to
    /// rebuild.
    fn ensure_encoder_for(&mut self, source_sr: u32, source_ch: u8, pts: u64) -> Result<()> {
        self.ensure_encoder_rate(source_sr, source_ch, pts)?;
        self.ensure_layout(source_sr, source_ch)
    }

    /// The channel stage for a decoded `(source_sr, source_ch)`, to the
    /// encoder's input layout, at the source's rate.
    fn ensure_layout(&mut self, source_sr: u32, source_ch: u8) -> Result<()> {
        if self.layout_in == (source_sr, source_ch) {
            return Ok(());
        }
        let enc_ch = self
            .encoder
            .as_ref()
            .map_or(source_ch, |e| e.params().channels);
        self.layout = crate::engine::audio_transcode::encoder_stage(
            self.channel_block.as_ref(),
            None,
            self.target_channels,
            source_sr,
            source_ch,
            Some((source_sr, enc_ch)),
        )
        .map_err(|e| anyhow::anyhow!("channel stage: {e}"))?;
        self.layout_in = (source_sr, source_ch);
        Ok(())
    }

    fn ensure_encoder_rate(&mut self, source_sr: u32, source_ch: u8, pts: u64) -> Result<()> {
        let matches = self
            .encoder
            .as_ref()
            .is_some_and(|e| e.params().sample_rate == source_sr);
        if matches {
            return Ok(());
        }
        let target_sample_rate = self
            .encoder
            .as_ref()
            .map(|e| e.params().target_sample_rate)
            .unwrap_or_else(|| self.target_sample_rate.unwrap_or(source_sr));
        // The layout the channel stage produces for this source: the
        // block's routing or the standard downmix to the asked-for count.
        let target_channels = match self.encoder.as_ref() {
            Some(e) => e.params().target_channels,
            None => crate::engine::audio_transcode::encoder_stage_format(
                self.channel_block.as_ref(),
                None,
                self.target_channels,
                source_sr,
                source_ch,
            )
            .map_err(|e| anyhow::anyhow!("channel stage: {e}"))?
            .1,
        };
        let at = self.input_position();
        let params = EncoderParams {
            codec: self.target_codec,
            sample_rate: source_sr,
            channels: target_channels,
            target_bitrate_kbps: self.target_bitrate_kbps,
            target_sample_rate,
            target_channels,
            // CMAF rejects Opus at construction (see `Self::new`), so the
            // Opus knobs are unreachable on this path. Using None / false
            // keeps the field defaults explicit at the construction site.
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
        };
        if let Some(old) = self.encoder.take() {
            tracing::info!(
                "CMAF output '{}': audio source is {source_sr} Hz / {source_ch} ch, the \
                 encoder was built for {} Hz; rebuilding it for the source's rate",
                self.output_id,
                old.params().sample_rate,
            );
            old.cancel();
        }
        // On a child of the output's token: `old.cancel()` above reached
        // only the encoder being retired, and this one will be retired the
        // same way. Cancelling the output's own token here ended the output
        // on the first frame of a 44.1 kHz source.
        let enc = AudioEncoder::spawn(
            params,
            self.cancel.child_token(),
            self.flow_id.clone(),
            self.output_id.clone(),
            self.out_stats.clone(),
            None,
        )
        .map_err(|e| anyhow::anyhow!("AudioEncoder spawn failed: {e}"))?;
        self.encoder = Some(enc);
        // The silence has to be at the encoder's input rate and layout too,
        // and its watchdog carries over: this runs on a real frame, so the
        // new generator has just heard one.
        if self.silence.is_some() {
            let mut sg = SilenceGenerator::new(source_sr, target_channels.clamp(1, 2), 0);
            sg.mark_real_audio(pts);
            self.silence = Some(sg);
        }
        // The input position is a property of what has been handed to the
        // segmenter, not of the encoder: it carries over, and the new encoder
        // is anchored there by the next submit.
        self.input_anchor_pts = at;
        self.input_since_anchor = 0;
        self.input_sr = source_sr;
        Ok(())
    }

    /// Reset the drop watchdog on a real source AAC frame so silence goes
    /// quiet for the grace window after it, and measure the picture's lead
    /// over the audio — `video_pts_90k` is the newest video DTS — for the
    /// next fill to stop short by.
    pub fn mark_real_audio(&mut self, pts: u64, video_pts_90k: Option<u64>) {
        if let Some(sg) = self.silence.as_mut() {
            sg.mark_real_audio(pts);
        }
        if std::mem::take(&mut self.in_silence) {
            // A new run of real audio: measure its lead afresh.
            self.video_lead_90k = 0;
        }
        if let Some(video) = video_pts_90k {
            self.video_lead_90k = self.video_lead_90k.max(video.saturating_sub(pts));
        }
    }

    /// Encode one source AAC frame. Returns zero or more re-encoded AAC
    /// frames, each with its own PTS (the encoder may buffer one input
    /// before emitting depending on the codec's frame size).
    ///
    /// The PTS is the encoder's, not the source's. The encoder emits a frame
    /// per `frame_size` samples, and a source frame is that size only when
    /// the source is AAC-LC too: an AC-3 source (1536 samples) yields two
    /// output frames on every second submit, an MP2 one (1152) every eighth,
    /// an HE-AAC element over LATM (2048) on every one. Stamping every frame
    /// of a submit with the source PTS gave the second a duration of zero,
    /// and Chrome's MSE treats the frame after a zero-duration one as a
    /// discontinuity — need-RAP on the video track, picture dropped to the
    /// next IDR — on every such pair. The frozen-picture symptom, from the
    /// re-encode path this time.
    pub fn encode_aac_frame(&mut self, frame: &[u8], pts: u64) -> Result<Vec<(Vec<u8>, u64)>> {
        // Lazy decoder / encoder construction — we can't initialise
        // either until we know the source sample rate + channels, which
        // come from the demuxer-cached ADTS config. The caller passes
        // the tuple via `set_adts_config` before the first frame.
        if self.decoder.is_none() {
            let (profile, sr_idx, ch_cfg) = self
                .lazy_init_params
                .adts_config
                .ok_or_else(|| anyhow::anyhow!("audio re-encoder: ADTS config not set"))?;
            let dec = AacDecoder::from_adts_config(profile, sr_idx, ch_cfg)
                .map_err(|e| anyhow::anyhow!("AacDecoder open failed: {e}"))?;
            self.decoder = Some(dec);
        }
        let dec = self.decoder.as_mut().unwrap();
        let planar = dec
            .decode_frame(frame)
            .map_err(|e| anyhow::anyhow!("AAC decode failed: {e}"))?;
        if planar.is_empty() {
            return Ok(Vec::new());
        }
        let source_sr = dec.sample_rate();
        let source_ch = dec.channels();
        self.ensure_encoder_for(source_sr, source_ch, pts)?;
        self.submit_real(&planar, pts)
    }

    /// Submit real PCM at `pts`, placed on the encoder's input timeline.
    fn submit_real(&mut self, planar: &[Vec<f32>], pts: u64) -> Result<Vec<(Vec<u8>, u64)>> {
        let mut out = Vec::new();
        if self.place_source(pts, &mut out) == Placement::Skip {
            return Ok(out);
        }
        let n = planar.first().map_or(0, |c| c.len() as u64);
        let planar: std::borrow::Cow<'_, [Vec<f32>]> = match self.layout.as_mut() {
            Some(l) => std::borrow::Cow::Owned(
                l.process(planar).map_err(|e| anyhow::anyhow!("channel stage: {e}"))?,
            ),
            None => std::borrow::Cow::Borrowed(planar),
        };
        // At the input position, not the frame's PTS: equal on a first
        // anchor or a re-anchor, ignored once anchored, and after a rebuild
        // it is what puts the new encoder where the retired one left off
        // rather than up to a slack away.
        let anchor = self.input_position().unwrap_or(pts);
        let enc = self.encoder.as_mut().expect("ensured by the caller");
        enc.submit_planar(&planar, anchor);
        self.input_since_anchor += n;
        while let Some(frame) = enc.try_recv() {
            out.push((frame.data.to_vec(), frame.pts));
        }
        Ok(out)
    }

    /// Tell the re-encoder the ADTS parameters observed by the demuxer
    /// so the lazy AacDecoder construction can succeed.
    pub fn set_adts_config(&mut self, profile: u8, sr_idx: u8, ch_cfg: u8) {
        self.lazy_init_params.adts_config = Some((profile, sr_idx, ch_cfg));
    }

    /// Encode a planar f32 PCM frame that the caller has already decoded
    /// out-of-band (e.g. an FFmpeg-backed decode of a non-AAC source like
    /// MP2 / AC-3 / E-AC-3). Lazy-builds the AAC encoder against the
    /// supplied source sample-rate / channel count on first call, then
    /// hands the planar frame straight to the encoder. Mirrors the back
    /// half of [`Self::encode_aac_frame`] without touching the AAC
    /// decoder slot. Frames carry the encoder's PTS, as in
    /// [`Self::encode_aac_frame`], and for the same reason.
    pub fn encode_planar(
        &mut self,
        planar: &[Vec<f32>],
        pts: u64,
        source_sr: u32,
        source_ch: u8,
    ) -> Result<Vec<(Vec<u8>, u64)>> {
        if planar.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_encoder_for(source_sr, source_ch, pts)?;
        self.submit_real(planar, pts)
    }
}

/// Where a real submit lands relative to the encoder's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// On the timeline (anchored, re-anchored, or contiguous): submit it.
    Submit,
    /// Behind the encoder's input by less than a fill: hold it back.
    Skip,
}

// ────────────────────────────────────────────────────────────────────────
//  Video re-encoder
// ────────────────────────────────────────────────────────────────────────

#[cfg(feature = "media-codecs")]
pub struct VideoReencoder {
    decoder: Option<video_engine::VideoDecoder>,
    /// Shared encoder pipeline — wraps `VideoEncoder` + optional
    /// `VideoScaler`. CMAF carries SPS/PPS inline on every IDR
    /// (segments are self-contained for DASH/HLS tune-in) so the
    /// pipeline is opened with `global_header = false`. When the
    /// operator sets `video_encode.width` / `.height`, the scaler
    /// Lanczos-resizes the decoded frame instead of letting libavcodec
    /// silently crop.
    pipeline: crate::engine::video_encode_util::ScaledVideoEncoder,
    /// Output_id for tracing.
    output_id: String,
    /// Annex-B scratch buffer reused across frames to avoid allocs.
    annex_b_scratch: Vec<u8>,
    /// Source codec pinned at first observed frame; changing codec is
    /// rejected (operator must restart the flow).
    source_codec: Option<CmafVideoCodec>,
    /// The last pts handed to the encoder — the step-off point for the
    /// frames [`Self::flush`] drains from the decoder.
    last_pts: Option<i64>,
    /// Holds an H.264 decoder open back until an access unit carries the
    /// SPS, which seeds its reorder depth (`SpsOpenGate`).
    sps_gate: crate::engine::video_encode_util::SpsOpenGate,
}

#[cfg(not(feature = "media-codecs"))]
pub struct VideoReencoder {
    _phantom: (),
    output_id: String,
}

pub struct VideoOutFrame {
    pub nalus: Vec<Vec<u8>>,
    pub is_keyframe: bool,
}

/// The codec family a `video_encode.codec` string produces.
///
/// The re-encoder's output family is not necessarily the source's: an H.264
/// source re-encoded with `x265` emits HEVC. Anything reading the encoder's
/// NAL units — the CMAF track builder, above all — has to know which set of
/// NAL header semantics applies, and the demuxed frame's codec is the wrong
/// answer there.
///
/// `None` for a string [`VideoReencoder::new`] would reject anyway.
pub fn encoded_codec_family(codec: &str) -> Option<CmafVideoCodec> {
    match codec {
        "x264" | "h264_nvenc" | "h264_qsv" | "h264_vaapi" | "h264_rkmpp" => {
            Some(CmafVideoCodec::H264)
        }
        "x265" | "hevc_nvenc" | "hevc_qsv" | "hevc_vaapi" | "hevc_rkmpp" => {
            Some(CmafVideoCodec::H265)
        }
        _ => None,
    }
}

#[cfg(feature = "media-codecs")]
impl VideoReencoder {
    pub fn new(cfg: &VideoEncodeConfig, output_id: &str) -> Result<Self> {
        let target_codec = match cfg.codec.as_str() {
            "x264" => video_codec::VideoEncoderCodec::X264,
            "x265" => video_codec::VideoEncoderCodec::X265,
            "h264_nvenc" => video_codec::VideoEncoderCodec::H264Nvenc,
            "hevc_nvenc" => video_codec::VideoEncoderCodec::HevcNvenc,
            "h264_qsv" => video_codec::VideoEncoderCodec::H264Qsv,
            "hevc_qsv" => video_codec::VideoEncoderCodec::HevcQsv,
            "h264_vaapi" => video_codec::VideoEncoderCodec::H264Vaapi,
            "hevc_vaapi" => video_codec::VideoEncoderCodec::HevcVaapi,
            "h264_rkmpp" => video_codec::VideoEncoderCodec::H264Rkmpp,
            "hevc_rkmpp" => video_codec::VideoEncoderCodec::HevcRkmpp,
            other => bail!("unknown video codec: {other}"),
        };
        let (fps_num, fps_den) = match (cfg.fps_num, cfg.fps_den) {
            (Some(n), Some(d)) => (n, d),
            _ => (30, 1),
        };
        // CMAF-LL segments are self-contained (DASH/HLS tune-in); SPS/PPS
        // rides in-band on every IDR, so `global_header = false`. GOP
        // size defaults to 60 (2s at 30 fps) when the operator didn't
        // pick one — the pipeline's `build_encoder_config` applies
        // `2 * fps` by default, but CMAF segmenters are happier with a
        // steady 60-frame GoP regardless of fps.
        let mut pipeline_cfg = cfg.clone();
        if pipeline_cfg.gop_size.is_none() {
            pipeline_cfg.gop_size = Some(60);
        }
        let pipeline = crate::engine::video_encode_util::ScaledVideoEncoder::new(
            pipeline_cfg,
            target_codec,
            fps_num,
            fps_den,
            false,
            format!("CMAF output '{}'", output_id),
        );
        Ok(Self {
            decoder: None,
            pipeline,
            output_id: output_id.to_string(),
            annex_b_scratch: Vec::with_capacity(256 * 1024),
            source_codec: None,
            last_pts: None,
            sps_gate: crate::engine::video_encode_util::SpsOpenGate::new(),
        })
    }

    /// Encode one access unit. Returns the re-encoded NAL list +
    /// keyframe flag, or `None` if the encoder buffered the frame.
    pub fn encode_frame(
        &mut self,
        nalus: &[Vec<u8>],
        pts: u64,
        _is_keyframe: bool,
        codec: CmafVideoCodec,
    ) -> Result<Option<VideoOutFrame>> {
        match self.source_codec {
            None => self.source_codec = Some(codec),
            Some(prev) if prev != codec => {
                bail!("video re-encoder: source codec changed mid-flow");
            }
            _ => {}
        }

        // Assemble Annex-B bitstream for the decoder. Each NAL unit in
        // `nalus` has start codes already stripped, so we re-prepend
        // 0x00000001.
        self.annex_b_scratch.clear();
        for nalu in nalus {
            self.annex_b_scratch.extend_from_slice(&[0, 0, 0, 1]);
            self.annex_b_scratch.extend_from_slice(nalu);
        }

        if self.decoder.is_none() {
            let src_codec = match codec {
                CmafVideoCodec::H264 => video_codec::VideoCodec::H264,
                CmafVideoCodec::H265 => video_codec::VideoCodec::Hevc,
            };
            // Opened on the first access unit that carries the SPS (nothing
            // decodes before one), and seeded from it: an H.264 decoder's
            // reorder depth comes from its SPS (`ReorderSeed`). Opened on a
            // P picture at a mid-GOP join, a source that declares no
            // reordering would hold a frame for good.
            if !self.sps_gate.admits(src_codec, &self.annex_b_scratch) {
                return Ok(None);
            }
            let dec = video_engine::VideoDecoder::open_opts(
                src_codec,
                video_engine::DecoderOptions {
                    reorder_seed: video_engine::ReorderSeed::FromAccessUnit(&self.annex_b_scratch),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("VideoDecoder open failed: {e}"))?;
            self.decoder = Some(dec);
            // Woven (H.264) or single-field (HEVC) interlaced frames — what
            // an explicit `scan: interlaced` needs to know.
            self.pipeline.set_source_codec(src_codec);
        }
        let dec = self.decoder.as_mut().unwrap();
        dec.send_packet(&self.annex_b_scratch)
            .map_err(|e| anyhow::anyhow!("VideoDecoder send_packet failed: {e}"))?;
        let decoded = match dec.receive_frame() {
            Ok(f) => f,
            Err(_e) => return Ok(None), // encoder buffered
        };

        self.last_pts = Some(pts as i64);
        let was_open = self.pipeline.is_open();
        let encoded = self
            .pipeline
            .encode(&decoded, Some(pts as i64))
            .map_err(|e| anyhow::anyhow!("VideoEncoder encode_frame failed: {e}"))?;
        if !was_open && self.pipeline.is_open() {
            let (w, h) = self.pipeline.dst_dimensions();
            tracing::info!(
                "CMAF output '{}': video re-encoder opened {}x{}",
                self.output_id, w, h,
            );
        }
        if encoded.is_empty() {
            return Ok(None);
        }

        // Convert Annex-B bitstream back to start-code-stripped NAL
        // units (CMAF samples carry length-prefixed NALs; the
        // segmenter re-applies length prefixes before packing).
        let mut out_nalus = Vec::new();
        let mut is_keyframe = false;
        for frame in &encoded {
            if frame.keyframe {
                is_keyframe = true;
            }
            split_annex_b_to_nalus(&frame.data, &mut out_nalus);
        }
        if out_nalus.is_empty() {
            return Ok(None);
        }
        Ok(Some(VideoOutFrame {
            nalus: out_nalus,
            is_keyframe,
        }))
    }

    /// Drain whatever the decoder and then the encoder are still holding.
    ///
    /// Both buffer, so the last frames handed in are not the last frames
    /// out. A live output never notices — it runs until it is stopped — but a
    /// clip has an end, and without this the tail of every export is short by
    /// however deep the decoder reorders (its B-frame depth, or the one frame
    /// an H.264 decoder seeded for a join holds on an IPPP source) plus
    /// however deep the encoder buffers. The decoder is drained first, its
    /// frames encoded, and only then the encoder flushed.
    pub fn flush(&mut self) -> Result<Vec<VideoOutFrame>> {
        let mut encoded = Vec::new();
        if let Some(dec) = self.decoder.as_mut()
            && dec.send_flush().is_ok()
        {
            while let Ok(decoded) = dec.receive_frame() {
                // Held-back frames carry no label of their own (the caller
                // stamps flushed frames itself); the encoder only needs a
                // monotonic one.
                let pts = self.last_pts.map_or(0, |p| p + 1);
                self.last_pts = Some(pts);
                encoded.extend(
                    self.pipeline
                        .encode(&decoded, Some(pts))
                        .map_err(|e| anyhow::anyhow!("VideoEncoder encode_frame failed: {e}"))?,
                );
            }
        }
        encoded.extend(self.pipeline.flush().map_err(|e| anyhow::anyhow!("{e}"))?);
        let mut out = Vec::new();
        for frame in &encoded {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(&frame.data, &mut nalus);
            if !nalus.is_empty() {
                out.push(VideoOutFrame { nalus, is_keyframe: frame.keyframe });
            }
        }
        Ok(out)
    }
}

#[cfg(not(feature = "media-codecs"))]
impl VideoReencoder {
    pub fn new(_cfg: &VideoEncodeConfig, output_id: &str) -> Result<Self> {
        bail!("video_encode requires the `media-codecs` feature (and a `video-encoder-*` backend) at build time")
    }

    pub fn encode_frame(
        &mut self,
        _nalus: &[Vec<u8>],
        _pts: u64,
        _is_keyframe: bool,
        _codec: CmafVideoCodec,
    ) -> Result<Option<VideoOutFrame>> {
        bail!("video_encode disabled at build time")
    }
}

/// Split an Annex-B byte stream into NALU vectors with the start
/// codes stripped. Handles both 3-byte (0x000001) and 4-byte
/// (0x00000001) start codes.
#[cfg(feature = "media-codecs")]
fn split_annex_b_to_nalus(data: &[u8], out: &mut Vec<Vec<u8>>) {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
                continue;
            }
            if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                starts.push(i + 4);
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    for (k, &s) in starts.iter().enumerate() {
        let end = if k + 1 < starts.len() {
            // Back up over the start code preamble of the next NAL.
            let next_start = starts[k + 1];
            if next_start >= 4 && data[next_start - 4..next_start] == [0, 0, 0, 1] {
                next_start - 4
            } else if next_start >= 3 && data[next_start - 3..next_start] == [0, 0, 1] {
                next_start - 3
            } else {
                next_start
            }
        } else {
            data.len()
        };
        if end > s {
            out.push(data[s..end].to_vec());
        }
    }
}

#[cfg(test)]
mod reencoder_tests {
    use super::*;
    use crate::config::models::AudioEncodeConfig;

    fn ae(codec: &str, silent_fallback: bool) -> AudioEncodeConfig {
        AudioEncodeConfig {
            codec: codec.to_string(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
            silent_fallback,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
             source_audio_pid: None,
             ts_signalling: None,
        }
    }

    #[test]
    fn silent_fallback_off_defers_encoder() {
        let cancel = CancellationToken::new();
        let r = AudioReencoder::new(&ae("aac_lc", false), None, &cancel, "out1", "flow1").unwrap();
        assert!(!r.has_silent_fallback());
        assert!(r.silence_chunk_duration().is_none());
        assert!(r.silent_fallback_track().is_none());
    }

    #[test]
    #[cfg_attr(
        not(any(feature = "fdk-aac", feature = "media-codecs")),
        ignore = "audio_encode requires an in-process encoder backend or ffmpeg in PATH"
    )]
    fn silent_fallback_on_builds_eager_encoder() {
        let cancel = CancellationToken::new();
        let r = AudioReencoder::new(&ae("aac_lc", true), None, &cancel, "out2", "flow2");
        // Accept ffmpeg-missing as a skip when no in-process backend is
        // compiled in — the edge surfaces the error at runtime.
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("ffmpeg") || msg.contains("FfmpegNotFound"),
                    "unexpected silent-fallback spawn error: {msg}"
                );
                return;
            }
        };
        assert!(r.has_silent_fallback());
        assert_eq!(
            r.silence_chunk_duration(),
            Some(std::time::Duration::from_nanos(21_333_333))
        );
        let track = r.silent_fallback_track().expect("default sr has ADTS index");
        // profile=1 (AAC-LC), sr_idx=3 (48000 Hz), ch_cfg=2.
        assert_eq!(track, (1, 3, 2));
    }

    /// Silence is laid down to where the audio would be — the picture less
    /// its lead over the audio — and no further; real audio returning
    /// contiguously with the silence is continued, real audio far from it
    /// re-anchors, and a source behind the silence is held back rather
    /// than stamped before it.
    ///
    /// With one chunk per tick the silence fell behind the picture by every
    /// tick the output loop observed late; filled to the picture itself it
    /// ran past the audio by the video's lead, and every return stepped
    /// backwards — a zero-duration sample and a fragment presented late.
    #[test]
    #[cfg(feature = "fdk-aac")]
    fn silence_fills_to_the_audio_and_real_audio_lands_where_it_ends() {
        let cancel = CancellationToken::new();
        let mut r = AudioReencoder::new(&ae("aac_lc", true), None, &cancel, "out3", "flow3")
            .expect("fdk-aac in-process encoder");
        let frame = 1_920u64; // 1024 samples at 48 kHz, in 90 kHz ticks

        // No picture yet: nothing to measure against, nothing emitted.
        assert!(r.encode_silence_if_needed(None).unwrap().is_empty());

        // The picture is at 10 s and nothing is known of the audio's lead:
        // the audio's timeline is placed there, with nothing yet to fill.
        let video = 10 * 90_000u64;
        assert!(r.encode_silence_if_needed(Some(video)).unwrap().is_empty());
        assert_eq!(r.input_position(), Some(video));
        // Two more seconds of picture: two seconds of silence, ~94 frames,
        // laid down in one call however many ticks it took to notice.
        let more = r.encode_silence_if_needed(Some(video + 2 * 90_000)).unwrap();
        assert!((90..=96).contains(&more.len()), "{} frames for 2 s", more.len());
        let delay = video - more[0].1;
        assert!(delay < 9_000, "the first silent frame sits at the picture minus codec delay: {}", more[0].1);
        let at = r.input_position().expect("anchored");
        assert!(video + 2 * 90_000 - at < frame, "filled to the picture, not past it: {at}");
        // Nothing to add while the picture stands still.
        assert!(r.encode_silence_if_needed(Some(video + 2 * 90_000)).unwrap().is_empty());

        // Real audio arrives on its own timeline, an hour away: re-anchored.
        let real = 3_600 * 90_000u64;
        let pcm = vec![vec![0.1f32; 1024]; 2];
        let mut out = Vec::new();
        for i in 0..6u64 {
            let pts = real + i * frame;
            // The picture runs 300 ms ahead of the audio on this source.
            r.mark_real_audio(pts, Some(pts + 27_000));
            out.extend(r.encode_planar(&pcm, pts, 48_000, 2).unwrap());
        }
        assert!(!out.is_empty());
        assert!(
            out[0].1.abs_diff(real - delay) <= 1,
            "the first real frame must sit at the source's PTS minus codec delay, got {} for {real}",
            out[0].1
        );
        for (k, (_, pts)) in out.iter().enumerate() {
            assert_eq!(*pts, out[0].1 + k as u64 * frame, "contiguous source frames stay put: frame {k}");
        }
        let last_real_end = real + 6 * frame;

        // The audio drops but the picture goes on 2 s: the fill stops 300 ms
        // short of the picture — where the audio would be — and starts where
        // the encoder's input is, not one frame early.
        let video_now = last_real_end + 27_000 + 2 * 90_000;
        // The watchdog is wall-clock: nothing until the grace has elapsed.
        assert!(r.encode_silence_if_needed(Some(video_now)).unwrap().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(600));
        let laid = r.encode_silence_if_needed(Some(video_now)).unwrap();
        assert!(!laid.is_empty(), "the grace has elapsed");
        let at = r.input_position().unwrap();
        assert!(video_now - 27_000 - at < frame, "stopped short by the lead: {at} vs {}", video_now - 27_000);
        assert_eq!(laid[0].1, out[0].1 + 6 * frame, "the silence continues the real audio exactly");

        // Real audio returns right where the silence ends: contiguous, no
        // re-anchor, no hole.
        let back = at;
        r.mark_real_audio(back, Some(back + 27_000));
        let resumed = r.encode_planar(&pcm, back, 48_000, 2).unwrap();
        let last_silent = laid[laid.len() - 1].1;
        assert_eq!(resumed[0].1, last_silent + frame, "the real frame follows the last silent one");

        // A source that comes back BEHIND the silence (the lead estimate was
        // short) is held until it catches up, never stamped before the
        // silence already handed out.
        let behind = r.input_position().unwrap() - 4 * frame;
        for i in 0..3u64 {
            let pts = behind + i * frame;
            r.mark_real_audio(pts, Some(pts + 27_000));
            assert!(r.encode_planar(&pcm, pts, 48_000, 2).unwrap().is_empty(), "frame {i} held back");
        }
        // One frame behind is within the slack: taken, and contiguous.
        let near = behind + 3 * frame;
        r.mark_real_audio(near, Some(near + 27_000));
        let again = r.encode_planar(&pcm, near, 48_000, 2).unwrap();
        assert!(!again.is_empty());
        assert_eq!(again[0].1, resumed[resumed.len() - 1].1 + frame);
    }

    /// The eager silent-fallback encoder is built at the declared rate before
    /// any source has been seen; a source at another rate rebuilds it, so a
    /// 44.1 kHz source is not consumed as 48 kHz — 8.8 % slow and flat.
    #[test]
    #[cfg(feature = "fdk-aac")]
    fn a_source_at_another_rate_rebuilds_the_eager_encoder() {
        let cancel = CancellationToken::new();
        let mut r = AudioReencoder::new(&ae("aac_lc", true), None, &cancel, "out4", "flow4")
            .expect("fdk-aac in-process encoder");
        assert_eq!(r.encoder.as_ref().unwrap().params().sample_rate, 48_000);
        let pcm = vec![vec![0.1f32; 1024]; 2];
        let mut out = Vec::new();
        let frame_441 = 1024 * 90_000 / 44_100; // 2089 ticks
        for i in 0..40u64 {
            let pts = 1_000_000 + i * frame_441;
            r.mark_real_audio(pts, None);
            out.extend(r.encode_planar(&pcm, pts, 44_100, 2).unwrap());
        }
        let p = r.encoder.as_ref().unwrap().params();
        assert_eq!((p.sample_rate, p.target_sample_rate), (44_100, 48_000), "input at the source, output at the track");
        assert!(out.len() >= 30, "{} frames", out.len());
        // Resampled to 48 kHz, so 40 × 1024 input samples are ~38 output
        // frames, and the stamps run at real time: the last frame is
        // ~(n-1) × 1920 ticks after the first, not (n-1) × 2089.
        let span = out[out.len() - 1].1 - out[0].1;
        assert_eq!(span, (out.len() as u64 - 1) * 1_920);
        assert!(r.silence.as_ref().unwrap().sample_rate() == 44_100, "silence follows the encoder's input rate");
        // The retired encoder was cancelled — on its own token. The output's
        // token, which the first version cancelled with it, is untouched:
        // that ended the whole output on the first 44.1 kHz frame.
        assert!(!cancel.is_cancelled(), "the output's token survived the rebuild");
        assert!(
            !r.encode_planar(&pcm, 1_000_000 + 40 * frame_441, 44_100, 2).unwrap().is_empty()
                || !r.encode_planar(&pcm, 1_000_000 + 41 * frame_441, 44_100, 2).unwrap().is_empty(),
            "and the rebuilt encoder keeps encoding"
        );
    }

    /// A source at another channel count is mixed to the encoder's layout on
    /// the way in; the in-process backends take no other, and indexing their
    /// accumulators by the source's count was a panic on the first frame.
    #[test]
    #[cfg(feature = "fdk-aac")]
    fn a_mono_source_is_mixed_to_the_tracks_layout() {
        let cancel = CancellationToken::new();
        let mut r = AudioReencoder::new(&ae("aac_lc", true), None, &cancel, "out5", "flow5")
            .expect("fdk-aac in-process encoder");
        let mono = vec![vec![0.25f32; 1024]];
        let mut out = Vec::new();
        for i in 0..8u64 {
            let pts = 5_000_000 + i * 1_920;
            r.mark_real_audio(pts, None);
            out.extend(r.encode_planar(&mono, pts, 48_000, 1).unwrap());
        }
        assert!(!out.is_empty(), "mono frames were encoded, not dropped or panicked on");
        let p = r.encoder.as_ref().unwrap().params();
        assert_eq!((p.channels, p.target_channels), (2, 2), "the encoder stays at the track's layout");
        assert!(!cancel.is_cancelled());

    }

    /// Mix of a decoded source by the channel stage the re-encoder builds:
    /// `planar` (its channel count) at 48 kHz, to the encoder's layout.
    #[cfg(feature = "fdk-aac")]
    fn mix(transcode: Option<crate::engine::audio_transcode::TranscodeJson>, channels: Option<u8>, planar: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let cancel = CancellationToken::new();
        let mut cfg = ae("aac_lc", false);
        cfg.channels = channels;
        let mut r = AudioReencoder::new(&cfg, transcode.as_ref(), &cancel, "out6", "flow6").unwrap();
        r.ensure_encoder_for(48_000, planar.len() as u8, 0).unwrap();
        match r.layout.as_mut() {
            Some(l) => l.process(planar).unwrap(),
            None => planar.to_vec(),
        }
    }

    /// `audio_encode.channels` and a `transcode` block convert exactly as
    /// they do on the TS outputs (`audio_transcode::encoder_stage`): a 5.1
    /// source to stereo is the ITU-R BS.775 downmix — its centre in both
    /// channels at -3 dB, where the old mix took L / R and lost it — and the
    /// block's routing, which CMAF used to ignore, applies.
    #[test]
    #[cfg(feature = "fdk-aac")]
    fn the_channel_stage_downmixes_and_honours_the_transcode_block() {
        let centre: Vec<Vec<f32>> = (0..6).map(|k| vec![if k == 2 { 1.0 } else { 0.0 }; 4]).collect();
        let lr = mix(None, Some(2), &centre);
        assert_eq!(lr.len(), 2);
        for c in &lr {
            assert!((c[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3, "centre at -3 dB: {}", c[0]);
        }
        // The block's own routing: swap L and R.
        let swap = crate::engine::audio_transcode::TranscodeJson {
            channel_map: Some(vec![vec![1], vec![0]]),
            ..Default::default()
        };
        let stereo = vec![vec![1.0f32; 4], vec![0.0f32; 4]];
        let out = mix(Some(swap), None, &stereo);
        assert_eq!((out[0][0], out[1][0]), (0.0, 1.0));
        // Mono to stereo duplicates.
        let up = mix(None, Some(2), &[vec![0.5f32; 4]]);
        assert_eq!(up[0], up[1]);
    }
}

#[cfg(test)]
#[cfg(feature = "media-codecs")]
mod tests {
    /// Every codec string `VideoReencoder::new` accepts must resolve to a
    /// family here, and to the family it actually encodes — the CMAF track is
    /// built by parsing the encoder's NAL headers, and reading HEVC as H.264
    /// finds no parameter sets at all, so the output publishes nothing.
    #[test]
    fn encoded_codec_family_covers_every_accepted_codec() {
        use super::encoded_codec_family;
        for c in ["x264", "h264_nvenc", "h264_qsv", "h264_vaapi", "h264_rkmpp"] {
            assert_eq!(encoded_codec_family(c), Some(CmafVideoCodec::H264), "{c}");
        }
        for c in ["x265", "hevc_nvenc", "hevc_qsv", "hevc_vaapi", "hevc_rkmpp"] {
            assert_eq!(encoded_codec_family(c), Some(CmafVideoCodec::H265), "{c}");
        }
        // Not a codec the re-encoder accepts; CMAF has no `*_auto` resolution.
        assert_eq!(encoded_codec_family("h264_auto"), None);
        assert_eq!(encoded_codec_family(""), None);
    }

    use super::*;

    #[test]
    fn annex_b_split_4byte_prefix() {
        let data = [0u8, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE];
        let mut out = Vec::new();
        split_annex_b_to_nalus(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0x67, 0x42]);
        assert_eq!(out[1], vec![0x68, 0xCE]);
    }

    #[test]
    fn annex_b_split_3byte_prefix() {
        let data = [0u8, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE];
        let mut out = Vec::new();
        split_annex_b_to_nalus(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0x67, 0x42]);
        assert_eq!(out[1], vec![0x68, 0xCE]);
    }
}

#[cfg(all(test, feature = "video-encoder-x264"))]
mod flush_tests {
    use super::*;
    use video_codec::{VideoEncoderCodec, VideoEncoderConfig, VideoPreset};

    /// A clip re-encode (`export_mp4::reencode_all_intra`) gets every frame
    /// back: the decoder's held-back pictures — its reorder depth on a
    /// B-frame source — are drained and encoded on flush, not only the
    /// encoder's. They used to be lost from the tail of every clip.
    #[test]
    fn flush_returns_the_frames_the_decoder_held_back() {
        let (w, h, n) = (320usize, 240usize, 30usize);
        let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            gop_size: 30,
            max_b_frames: 2,
            // `zerolatency` would turn the B-frames off.
            tune: String::new(),
            preset: VideoPreset::Veryfast,
            global_header: false,
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut aus = Vec::new();
        for i in 0..n {
            let y: Vec<u8> = (0..w * h).map(|k| ((k % w + 5 * i) % 200) as u8 + 20).collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        assert_eq!(aus.len(), n);
        assert!(aus.iter().any(|f| f.pts != f.dts), "the source reorders");

        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "gop_size": 1, "bframes": 0, "preset": "veryfast"
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "clip-test").unwrap();
        let mut out = 0usize;
        for (i, au) in aus.iter().enumerate() {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(&au.data, &mut nalus);
            if re
                .encode_frame(&nalus, i as u64 * 3_600, false, CmafVideoCodec::H264)
                .unwrap()
                .is_some()
            {
                out += 1;
            }
        }
        let held = re.flush().unwrap().len();
        assert!(held >= 1, "the decoder held pictures back");
        assert_eq!(out + held, n, "every source frame comes back");
    }

    /// A live output joining mid-GOP on a source that declares no
    /// reordering (x264 `zerolatency`, as the edge's own encodes are) opens
    /// its decoder on the next access unit that carries the SPS, seeded 0 —
    /// not on the P picture it joined on, seeded 1, which libavcodec never
    /// lowers: a frame held for the life of the output.
    #[test]
    fn a_mid_gop_join_opens_the_decoder_on_the_sps() {
        let (w, h) = (320usize, 240usize);
        let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            gop_size: 25,
            preset: VideoPreset::Veryfast,
            global_header: false,
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut aus = Vec::new();
        for i in 0..40 {
            let y: Vec<u8> = (0..w * h).map(|k| ((k % w + 5 * i) % 200) as u8 + 20).collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        let has_sps = |i: usize| crate::engine::video_encode_util::carries_h264_sps(&aus[i].data);
        let join = 3;
        assert!(!has_sps(join), "joined on a P picture");
        let next_sps = (join..aus.len()).find(|&i| has_sps(i)).expect("a second IDR");

        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast"
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "join-test").unwrap();
        let mut out = 0usize;
        for (i, au) in aus.iter().enumerate().skip(join) {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(&au.data, &mut nalus);
            if re
                .encode_frame(&nalus, i as u64 * 3_600, false, CmafVideoCodec::H264)
                .unwrap()
                .is_some()
            {
                out += 1;
            }
            if i < next_sps {
                assert!(re.decoder.is_none(), "AU {i} carries no SPS: passed over");
            }
        }
        let dec = re.decoder.as_ref().expect("opened on the SPS");
        assert_eq!(dec.reorder_depth(), 0, "a declared IPPP source holds no frame");
        assert!(out >= 10, "{out} frames re-encoded");
    }
}
