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
//! Where each frame goes on the encoder's input — re-anchored at a seam, a
//! gap covered with zeros under `silent_fallback`, a frame behind the input
//! held back, silence laid to the picture less its lead — is the rule the
//! RTMP output shares (`audio_encode::EncoderInputClock`).
//!
//! # Video re-encoder
//!
//! Source H.264/HEVC access unit → Annex-B framing → `VideoDecoder`
//! → `VideoEncoder` (x264 / x265 / NVENC, feature-gated) → target
//! H.264/HEVC NAL units. The encoder opens at the source's measured frame
//! rate (`EncoderRateLock`) unless the operator pins one. The operator's
//! `gop_size` is honoured; unset, the GOP tiles the CMAF segment at that
//! rate (`cmaf_default_gop`), and an IDR is forced on every GOP boundary of
//! the encoded-frame count, so every segment boundary lands on an IDR
//! whatever scene cuts the encoder finds in between.
//! Every output frame carries the source PTS of its own picture, as the
//! decoder propagated it.

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;

use crate::config::models::{AudioEncodeConfig, VideoEncodeConfig};
use crate::engine::aac_config::{AacConfig, ProgramConfig, pce_7_1_back};
use crate::engine::audio_au::adts_access_unit;
use crate::engine::audio_decode::AacDecoder;
use crate::engine::audio_encode::{
    AudioCodec, AudioEncoder, EncoderInputClock, EncoderParams, Placement, SOURCE_PTS_SLACK_TICKS,
};
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
    /// Where the encoder's input is on the source's timeline, and the
    /// picture's lead over the audio silence stops short by — the placement
    /// rule the RTMP output shares (`audio_encode::EncoderInputClock`).
    input: EncoderInputClock,
    /// What the encoder emits, read off its first frame's ADTS header
    /// ([`Self::encoder_track`]): for HE-AAC the core rate, SBR / PS
    /// implicit; for 7.1 the program_config_element. `None` until a frame
    /// is out.
    emitted: Option<AacConfig>,
    /// What an encoder retired for another input rate still held, run out
    /// on its own timeline (`ensure_encoder_rate`), for the next frames
    /// handed back — all but its last frame, which waits in `splice`.
    retired: Vec<(Vec<u8>, u64)>,
    /// The junction between that run-out and the new encoder's frames, until
    /// the new encoder's first frame carrying the new audio settles it
    /// ([`Self::splice_fresh`]).
    splice: Option<Splice>,
}

/// The junction of a rebuild for another source rate: the retired
/// encoder's run-out, then the new encoder's frames
/// ([`AudioReencoder::splice_fresh`]).
///
/// Two encoders cannot be spliced on one timeline without overlap: the new
/// one's priming frames (its delay line, 2048 samples of AAC-LC, plus its
/// resampler's) sit before the input position the retired one's run-out
/// reaches. Its last priming frame is still needed — the MDCT overlap-add
/// partner of its first frame carrying the new audio, which decodes
/// garbled after any other frame — and decodes, after the retired run-out,
/// to silence: the run-out ends in zeros, and so does that partner's first
/// half. So the partner is kept, stamped just before the first content
/// frame, and every priming frame before it dropped. The retired run-out's
/// last frame is held back until then: when the content frame starts no
/// later than it (the retired encoder held fewer samples than the new
/// resampler's delay, ~34 at 44.1 to 48 kHz) it is dropped, with the few
/// real samples it carries, rather than stepping the stamps back or
/// dropping the content frame. Dropping every new frame at or before the
/// run-out's last stamp, as the first version did, dropped the partner
/// (the first ~21 ms of the new audio decoded garbled) or, in that case,
/// the content frame itself.
struct Splice {
    /// The retired encoder's last frame, and the stamp of the one before
    /// it.
    last: Option<(Vec<u8>, u64)>,
    before: Option<u64>,
    /// Where the new encoder's input starts (90 kHz): set at its first
    /// submit.
    content_from: Option<u64>,
    /// One frame of the new encoder (90 kHz).
    frame_90k: u64,
    /// The new encoder's latest frame carrying none of the new audio.
    partner: Option<(Vec<u8>, u64)>,
}

/// The least step between two stamps at a rebuild's junction: a sample at
/// 8 kHz, so the segmenter's conversion to the track's timescale (its sample
/// rate, 8 kHz at least) never gives two frames one decode time — a
/// zero-duration sample.
const SPLICE_STEP_90K: u64 = 12;
/// Rounding the 90 kHz stamps carry, below which a frame ending at the new
/// encoder's input position still counts as ending there.
const SPLICE_ROUNDING_90K: u64 = 2;

struct LazyAudioInit {
    /// The source's configuration as the demuxer caches it — a
    /// PCE-signalled source's with its program_config_element. `None` until
    /// the first AAC frame (and, for such a source, its layout) observed.
    source_config: Option<AacConfig>,
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
            lazy_init_params: LazyAudioInit { source_config: None },
            flow_id: flow_id.to_string(),
            output_id: output_id.to_string(),
            cancel: cancel.clone(),
            out_stats,
            silence,
            input: EncoderInputClock::default(),
            emitted: None,
            retired: Vec::new(),
            splice: None,
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

    /// The configuration of what the encoder emits — its target rate and
    /// layout, which a source at any other rate or layout is converted to —
    /// as its first frame's ADTS header names it: for HE-AAC the core (24
    /// kHz for a 48 kHz target, v2's mono), SBR / PS signalled implicitly,
    /// as a passed-through HE-AAC source's track says; for 7.1, channel
    /// configuration 0 and the program_config_element. `None` until a frame
    /// is out. It used to be the target rate and channels under AAC-LC, so
    /// an HE-AAC track's init announced 48 kHz LC over SBR frames.
    pub fn encoder_track(&self) -> Option<AacConfig> {
        self.emitted.clone()
    }

    /// The configuration of the silent-fallback track, so the caller can
    /// build the CMAF `AudioSegmenter` before any source audio — or any
    /// encoded frame — exists: what the eager encoder will emit
    /// ([`expected_adts_config`] of its codec, rate and layout). Its rate and
    /// layout are the encoder's own, which the declared ones are clamped to
    /// (stereo at most), and every later rebuild keeps them. `None` unless
    /// `silent_fallback` is active, or for a rate with no ADTS index.
    pub fn silent_fallback_track(&self) -> Option<AacConfig> {
        self.silence.as_ref()?;
        let p = self.encoder.as_ref()?.params();
        expected_adts_config(p.codec, p.target_sample_rate, p.target_channels)
    }

    /// Where on the source's timeline the encoder's next input sample
    /// belongs, if it has been anchored.
    fn input_position(&self) -> Option<u64> {
        self.input.position()
    }

    /// Start the encoder's input timeline afresh at `pts`.
    fn anchor_input(&mut self, pts: u64) {
        match self.encoder.as_mut() {
            Some(enc) => {
                enc.reanchor_pts();
                self.input.anchor(pts, enc.params().sample_rate);
            }
            None => self.input.anchor(pts, self.input.rate()),
        }
    }

    /// Produce silent AAC frames `(data, pts)` if the drop watchdog says real
    /// audio is absent or stalled — as many as it takes to bring the silence
    /// up to where the audio would be, given where the picture is
    /// (`video_pts_90k`, the newest video PTS). Idempotent and cheap when
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
    /// video's lead over the audio (`EncoderInputClock::silence_target`) —
    /// where the audio itself would be — so real audio returning lands where
    /// the silence ends rather than behind it.
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
        let (Some(sg), Some(enc)) = (self.silence.as_mut(), self.encoder.as_mut()) else {
            return Ok(Vec::new());
        };
        let target = self.input.silence_target(video);
        // Anchored at the target when nothing is, and when the picture moved
        // further than any gap this would fill — a splice or a wrap: the
        // silence follows it rather than chasing it, and the encoder is told
        // the same way a source jump tells it.
        if self.input.silence_start(target, enc.params().sample_rate) {
            enc.reanchor_pts();
        }
        let chunk_samples = sg.chunk_samples() as u64;
        let mut fresh = Vec::new();
        while let Some(at) = self.input.next_silence(chunk_samples, target) {
            // Submitted at the input position, which is what a freshly
            // (re-)anchored encoder takes its output timeline from.
            let (planar, _) = sg.next_chunk();
            note_splice_start(&mut self.splice, at);
            if enc.submit_planar(planar, at) {
                self.input.took(chunk_samples);
            } else {
                self.input.refused();
                break;
            }
            self.input.mark_silence();
            take_encoded(enc, &mut self.emitted, &self.output_id, &mut fresh);
        }
        // Through a rebuild's junction like the real audio's frames: they
        // went out ahead of the retired encoder's run-out, priming and all.
        let mut out = Vec::new();
        self.splice_fresh(fresh, &mut out);
        Ok(out)
    }

    /// What to do with a real submit at `pts`, given where the encoder's
    /// input is (`EncoderInputClock::place`), done: the encoder re-anchored
    /// on it, or a forward gap covered with zeros — whose frames go to `out`.
    /// A source behind the input comes back `Skip`: going backwards would
    /// stamp it before the silence already handed to the segmenter — a
    /// zero-duration sample, and a fragment presented late.
    fn place_source(&mut self, pts: u64, out: &mut Vec<(Vec<u8>, u64)>) -> Placement {
        let placement = self.input.place(pts, self.silence.is_some());
        match placement {
            Placement::Submit { reanchor: true } => {
                if let Some(at) = self.input.position().filter(|at| pts > at.saturating_add(SOURCE_PTS_SLACK_TICKS)) {
                    // A gap: the source moved on without the encoder.
                    tracing::debug!(
                        "CMAF output '{}': audio source PTS {pts} is {} ticks past the encoder's \
                         input; re-anchoring",
                        self.output_id,
                        pts - at,
                    );
                }
                self.anchor_input(pts);
            }
            Placement::FillThenSubmit { samples } => {
                // A gap the silent fallback exists to cover: the fill stops
                // one chunk short of the picture, and a return lands past
                // that residual — up to a chunk at the source's rate, most
                // of a frame at 16 kHz. Covered with zeros and continued,
                // rather than re-anchored and the last silent sample
                // stretched over it.
                let at = self.input.position().unwrap_or(pts);
                note_splice_start(&mut self.splice, at);
                if let Some(enc) = self.encoder.as_mut() {
                    let zeros = vec![vec![0.0f32; samples as usize]; enc.params().channels as usize];
                    if enc.submit_planar(&zeros, at) {
                        self.input.took(samples);
                    } else {
                        self.input.refused();
                    }
                    self.collect_encoded(out);
                }
            }
            Placement::Submit { reanchor: false } | Placement::Skip => {}
        }
        placement
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
        if let Some(mut old) = self.encoder.take() {
            tracing::info!(
                "CMAF output '{}': audio source is {source_sr} Hz / {source_ch} ch, the \
                 encoder was built for {} Hz; rebuilding it for the source's rate",
                self.output_id,
                old.params().sample_rate,
            );
            // What it still holds — the partial frame and its delay line,
            // the end of the audio (or silence) before this frame — goes out
            // on its own timeline: dropped with it, it left that much of a
            // hole, filled by the new encoder's priming.
            old.reanchor_pts();
            // As raw access units, as every frame handed back is
            // (`take_encoded`).
            let mut tail: Vec<(Vec<u8>, u64)> = old
                .drain()
                .into_iter()
                .filter_map(|f| adts_access_unit(&f.data).map(|(au, _)| (au.to_vec(), f.pts)))
                .collect();
            // A junction still waiting from a rebuild before this one: its
            // held frame goes out ahead of this run-out; its partner is no
            // longer anyone's.
            if let Some(prev) = self.splice.take() {
                self.retired.extend(prev.last);
            }
            // The run-out's last frame waits for the new encoder's first
            // content frame (`Splice`).
            let last = tail.pop();
            let before = tail.last().map(|&(_, pts)| pts);
            self.retired.extend(tail);
            if last.is_some() {
                let frame_samples: u64 = match self.target_codec {
                    AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => 2_048,
                    _ => 1_024,
                };
                self.splice = Some(Splice {
                    last,
                    before,
                    content_from: None,
                    frame_90k: frame_samples * 90_000 / u64::from(target_sample_rate.max(1)),
                    partner: None,
                });
            }
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
        self.input.rebase(source_sr);
        Ok(())
    }

    /// Reset the drop watchdog on a real source AAC frame so silence goes
    /// quiet for the grace window after it, and measure the picture's lead
    /// over the audio — `video_pts_90k` is the newest video PTS — for the
    /// next fill to stop short by.
    pub fn mark_real_audio(&mut self, pts: u64, video_pts_90k: Option<u64>) {
        if let Some(sg) = self.silence.as_mut() {
            sg.mark_real_audio(pts);
        }
        self.input.mark_real_audio(pts, video_pts_90k);
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
        // come from the demuxer-cached configuration. The caller passes it
        // via `set_source_config` before the first frame.
        if self.decoder.is_none() {
            let source = self
                .lazy_init_params
                .source_config
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("audio re-encoder: AAC config not set"))?;
            let dec = AacDecoder::from_config(source)
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
        let mut out = std::mem::take(&mut self.retired);
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
        note_splice_start(&mut self.splice, anchor);
        let enc = self.encoder.as_mut().expect("ensured by the caller");
        if enc.submit_planar(&planar, anchor) {
            self.input.took(n);
        } else {
            self.input.refused();
        }
        self.collect_encoded(&mut out);
        Ok(out)
    }

    /// Hand back what the encoder has encoded, through the junction of a
    /// rebuild while one is open ([`Self::splice_fresh`]).
    fn collect_encoded(&mut self, out: &mut Vec<(Vec<u8>, u64)>) {
        let Some(enc) = self.encoder.as_mut() else {
            return;
        };
        let mut fresh = Vec::new();
        take_encoded(enc, &mut self.emitted, &self.output_id, &mut fresh);
        self.splice_fresh(fresh, out);
    }

    /// Hand back the new encoder's `fresh` frames across a rebuild's
    /// junction ([`Splice`]), after whatever the retired encoder's run-out
    /// still has to hand back: priming frames dropped but the last, which is
    /// stamped one step before the first frame carrying the new audio; the
    /// run-out's last frame ahead of them, or dropped when there is no room
    /// for it. From then on every frame goes out as it came.
    fn splice_fresh(&mut self, fresh: Vec<(Vec<u8>, u64)>, out: &mut Vec<(Vec<u8>, u64)>) {
        out.append(&mut self.retired);
        for (au, pts) in fresh {
            let Some(s) = self.splice.as_mut() else {
                out.push((au, pts));
                continue;
            };
            if s.content_from.is_some_and(|c| pts + s.frame_90k <= c + SPLICE_ROUNDING_90K) {
                s.partner = Some((au, pts));
                continue;
            }
            let s = self.splice.take().expect("matched above");
            let last_pts = s.last.as_ref().map(|&(_, pts)| pts);
            let kept = match s.last {
                Some(last) if pts >= last.1 + 2 * SPLICE_STEP_90K => {
                    out.push(last);
                    true
                }
                Some(_) => {
                    tracing::debug!(
                        "CMAF output '{}': the retired encoder's last frame overlaps the new \
                         audio's first; dropped",
                        self.output_id
                    );
                    false
                }
                None => false,
            };
            if let Some((partner, _)) = s.partner {
                let at = pts - SPLICE_STEP_90K;
                // After the retired run-out's last frame handed out, never
                // at or before it (a step back the segmenter would turn
                // into a zero or wrapped duration).
                let after = if kept { last_pts } else { s.before };
                if after.is_none_or(|b| at > b) {
                    out.push((partner, at));
                }
            }
            out.push((au, pts));
        }
    }

    /// Tell the re-encoder the source configuration observed by the
    /// demuxer so the lazy AacDecoder construction can succeed — for a
    /// PCE-signalled source (channel configuration 0) the configuration
    /// with its program_config_element, which the decoder reads the layout
    /// from. A configuration other than the running decoder's (a switch to
    /// another feed) retires that decoder, and the next frame builds one for
    /// it: a decoder set up for the old configuration misdecodes or refuses
    /// the new one's frames, and it used to be kept for the life of the
    /// output. The encoder, and so the track, stay as they are: the source
    /// is converted to them.
    pub fn set_source_config(&mut self, source: AacConfig) {
        if self.decoder.as_ref().is_some_and(|d| d.config() != &source) {
            tracing::info!(
                "CMAF output '{}': the source's AAC configuration changed; rebuilding its decoder",
                self.output_id
            );
            self.decoder = None;
        }
        self.lazy_init_params.source_config = Some(source);
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

/// Note where a rebuilt encoder's input starts, at its first submit
/// (`at`, the input position), for its junction ([`Splice`]).
fn note_splice_start(splice: &mut Option<Splice>, at: u64) {
    if let Some(s) = splice.as_mut()
        && s.content_from.is_none()
    {
        s.content_from = Some(at);
    }
}

/// Move what `enc` has encoded into `out` as `(access unit, pts)`: each
/// frame's raw access unit, its ADTS header taken off
/// ([`adts_access_unit`]), as an MP4 sample carries it, and the first
/// frame's configuration recorded in `emitted` ([`AudioReencoder::encoder_track`]).
/// Each sample used to keep the encoder's ADTS header inside the `mdat`,
/// which a decoder configured from the `esds` reads as the start of the raw
/// data block — the RTMP output's edge#210 defect, on CMAF. A frame that is
/// not one whole ADTS frame has no access unit to carry and is dropped, as
/// the RTMP output drops it (said once: a property of the build's encoder).
fn take_encoded(enc: &mut AudioEncoder, emitted: &mut Option<AacConfig>, output_id: &str, out: &mut Vec<(Vec<u8>, u64)>) {
    while let Some(frame) = enc.try_recv() {
        // fdk may hand back an empty frame while it primes: nothing to carry.
        if frame.data.is_empty() {
            continue;
        }
        let Some((au, (profile, sr_idx, ch_cfg))) = adts_access_unit(&frame.data) else {
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!("CMAF output '{output_id}': an encoded AAC frame is not one whole ADTS frame, dropped");
            }
            continue;
        };
        if emitted.is_none() {
            // A 7.1 frame opens with its PCE (`audio_encode` puts it there),
            // so the first frame names the whole configuration.
            *emitted = AacConfig::from_adts(profile, sr_idx, ch_cfg, au);
        }
        out.push((au.to_vec(), frame.pts));
    }
}

/// The configuration an in-process AAC encoder of `codec` at `sample_rate`
/// and `channels` writes in its ADTS headers, before it has written any —
/// what a track built ahead of the first frame must announce. ADTS signals
/// SBR / PS implicitly, so HE-AAC names AAC-LC at its core, half the rate
/// (fdk runs SBR dual-rate), and v2 its mono core; 7.1 is channel
/// configuration 0 with the program_config_element `audio_encode` puts in
/// each frame (`aac_config::pce_7_1_back`). `None` for a codec that is not
/// AAC or a rate or layout ADTS cannot name. Pinned to fdk's own headers by
/// a test.
pub fn expected_adts_config(codec: AudioCodec, sample_rate: u32, channels: u8) -> Option<AacConfig> {
    let (core_rate, core_channels) = match codec {
        AudioCodec::AacLc => (sample_rate, channels),
        AudioCodec::HeAacV1 => (sample_rate / 2, channels),
        AudioCodec::HeAacV2 => (sample_rate / 2, 1),
        _ => return None,
    };
    let sr_idx = crate::engine::audio_decode::sr_index_from_hz(core_rate)?;
    match core_channels {
        1..=6 => AacConfig::new(1, sr_idx, core_channels),
        8 => ProgramConfig::from_raw_data_block(&pce_7_1_back(1, sr_idx)).map(|pce| AacConfig::with_pce(1, sr_idx, pce)),
        _ => None,
    }
}

// ────────────────────────────────────────────────────────────────────────
//  Video re-encoder
// ────────────────────────────────────────────────────────────────────────

#[cfg(feature = "media-codecs")]
pub struct VideoReencoder {
    decoder: Option<video_engine::VideoDecoder>,
    /// The encoder rate, measured from the decoded frames unless pinned.
    rate: crate::engine::video_encode_util::EncoderRateLock,
    /// Segment duration the default GOP tiles (`cmaf_default_gop`); `None`
    /// leaves the encoder's own default.
    segment_secs: Option<f64>,
    /// Frame counter the encoder is stamped with — its time base is
    /// 1 / fps, and 90 kHz ticks against it are the VBV hazard
    /// `ScaledVideoEncoder::set_pts_90k` documents.
    next_frame: i64,
    /// How an encoded frame finds its picture's source PTS.
    in_flight: crate::engine::video_encode_util::EncodedPtsMap,
    /// The source PTS of each decoded picture (see `FramePtsStamper`).
    stamper: crate::engine::video_encode_util::FramePtsStamper,
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
    source_codec: Option<video_codec::VideoCodec>,
    /// Holds an H.264 decoder open back until an access unit carries the
    /// SPS, which seeds its reorder depth (`SpsOpenGate`).
    sps_gate: crate::engine::video_encode_util::SpsOpenGate,
    /// Where `video_encode_interlace_unavailable` goes (the live output);
    /// `None` (clip export) leaves it to the log line.
    events: Option<crate::manager::events::EventSender>,
}

#[cfg(not(feature = "media-codecs"))]
pub struct VideoReencoder {
    _phantom: (),
    output_id: String,
}

pub struct VideoOutFrame {
    pub nalus: Vec<Vec<u8>>,
    pub is_keyframe: bool,
    /// Source PTS (90 kHz) of the picture this frame codes — the one the
    /// decoder propagated through its reorder queue — or `None` when the
    /// decoder lost it. Not the PTS of the access unit that was being fed
    /// when the frame came out: a decoder hands pictures back in display
    /// order, as many or as few per access unit as it has, and drops the
    /// ones it cannot decode (an open-GOP clip's leading pictures).
    pub pts: Option<u64>,
}

#[cfg(feature = "media-codecs")]
/// Longest default GOP of a CMAF re-encode, in seconds: IDRs every ≤ 2 s,
/// as Apple's HLS authoring spec asks and as the 60-frame default this
/// replaced intended (2 s at 30 fps).
const CMAF_MAX_DEFAULT_GOP_SECS: f64 = 2.0;

/// The default GOP (frames) of a CMAF re-encode at `fps_num / fps_den` with
/// `segment_secs` segments: the fewest GOPs of at most
/// [`CMAF_MAX_DEFAULT_GOP_SECS`] that tile a segment, each rounded **up** to
/// a whole frame. The segmenter cuts on the first IDR at or after the
/// target, so a segment's last GOP ending at or just past it closes the
/// segment on time; one ending short of it by a frame would run the
/// segment on to the next IDR. A 2 s segment is one GOP: 50 frames at 25,
/// 60 at 29.97 and 30, 48 at 23.976, 100 at 50. The flat 60 it replaced was
/// 2.4 s at 25 fps, so a 2 s target cut 2.4 s segments.
#[cfg(feature = "media-codecs")]
pub fn cmaf_default_gop(segment_secs: f64, fps_num: u32, fps_den: u32) -> u32 {
    let fps = fps_num as f64 / fps_den.max(1) as f64;
    let gops = (segment_secs / CMAF_MAX_DEFAULT_GOP_SECS - 1e-9).ceil().max(1.0);
    ((segment_secs * fps / gops - 1e-6).ceil() as u32).clamp(1, 600)
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
    /// `segment_secs`: the CMAF segment duration an unset `gop_size` is
    /// sized to (`None` — clip export — keeps the encoder's default).
    pub fn new(cfg: &VideoEncodeConfig, output_id: &str, segment_secs: Option<f64>) -> Result<Self> {
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
        // Unpinned, the encoder opens at the source's measured rate (the
        // placeholder below is replaced at the lock); it used to open at a
        // flat 30/1 — a 25 fps source's VUI said 30 fps and CBR ran 5/6 of
        // the configured bitrate.
        let (pinned, (fps_num, fps_den)) = match (cfg.fps_num, cfg.fps_den) {
            (Some(n), Some(d)) => (true, (n, d)),
            _ => (false, crate::engine::video_encode_util::RATE_LOCK_FALLBACK),
        };
        // B-frames are pinned off, as on RTMP: this path hands the segmenter
        // each frame's presentation stamp and no decode time, so it writes
        // the frame at that stamp with no composition offset. An encoder
        // that reordered (x264 without `zerolatency`, or any hardware
        // encoder asked for `bframes`) would give it display-order stamps in
        // decode order — a timeline stepping back. (Passthrough carries the
        // source's DTS and its offsets since edge#156; the re-encoder does
        // not surface its packets' DTS.)
        let mut cfg = cfg.clone();
        if cfg.bframes.unwrap_or(0) != 0 {
            tracing::warn!(
                error_code = "cmaf_bframes_unsupported",
                requested = cfg.bframes.unwrap_or(0),
                "CMAF output '{output_id}': video_encode.bframes is not supported on the CMAF \
                 re-encode (its samples carry no composition offsets) — encoding with bframes = 0"
            );
            cfg.bframes = Some(0);
        }
        // CMAF-LL segments are self-contained (DASH/HLS tune-in); SPS/PPS
        // rides in-band on every IDR, so `global_header = false`. An unset
        // GOP is sized to the segment once the rate is known
        // (`cmaf_default_gop`).
        let pipeline = crate::engine::video_encode_util::ScaledVideoEncoder::new(
            cfg.clone(),
            target_codec,
            fps_num,
            fps_den,
            false,
            format!("CMAF output '{}'", output_id),
        );
        Ok(Self {
            decoder: None,
            rate: crate::engine::video_encode_util::EncoderRateLock::new(pinned),
            segment_secs,
            next_frame: 0,
            in_flight: Default::default(),
            stamper: Default::default(),
            pipeline,
            output_id: output_id.to_string(),
            annex_b_scratch: Vec::with_capacity(256 * 1024),
            source_codec: None,
            sps_gate: crate::engine::video_encode_util::SpsOpenGate::new(),
            events: None,
        })
    }

    /// Raise `video_encode_interlace_unavailable` (an explicit `scan:
    /// interlaced` coding progressive) on `events`, scoped to this output,
    /// once per encoder open — as the TS and RTMP paths do.
    pub fn set_event_sender(&mut self, events: crate::manager::events::EventSender) {
        self.events = Some(events);
    }

    /// Encode one access unit (decode order), stamped `pts` (90 kHz).
    /// Returns every frame the encoder handed back — none while the decoder
    /// and encoder fill, and while the rate is still being measured — each
    /// carrying its own picture's source PTS.
    pub fn encode_frame(
        &mut self,
        nalus: &[Vec<u8>],
        pts: Option<u64>,
        _is_keyframe: bool,
        codec: CmafVideoCodec,
    ) -> Result<Vec<VideoOutFrame>> {
        // Assemble Annex-B bitstream for the decoder. Each NAL unit in
        // `nalus` has start codes already stripped, so we re-prepend
        // 0x00000001.
        self.annex_b_scratch.clear();
        for nalu in nalus {
            self.annex_b_scratch.extend_from_slice(&[0, 0, 0, 1]);
            self.annex_b_scratch.extend_from_slice(nalu);
        }
        let src_codec = match codec {
            CmafVideoCodec::H264 => video_codec::VideoCodec::H264,
            CmafVideoCodec::H265 => video_codec::VideoCodec::Hevc,
        };
        self.encode_scratch(src_codec, pts)
    }

    /// Encode one MPEG-2 video access unit — its ES, which the decoder takes
    /// as it is — stamped `pts` (90 kHz), as [`Self::encode_frame`]. CMAF
    /// carries H.264 / HEVC, so an MPEG-2 source reaches it only this way;
    /// it used to be dropped whatever `video_encode` said.
    pub fn encode_mpeg2(&mut self, es: &[u8], pts: Option<u64>) -> Result<Vec<VideoOutFrame>> {
        self.annex_b_scratch.clear();
        self.annex_b_scratch.extend_from_slice(es);
        self.encode_scratch(video_codec::VideoCodec::Mpeg2, pts)
    }

    /// The access unit in `annex_b_scratch`, of `src_codec`, through the
    /// decoder and the encoder.
    fn encode_scratch(
        &mut self,
        src_codec: video_codec::VideoCodec,
        pts: Option<u64>,
    ) -> Result<Vec<VideoOutFrame>> {
        match self.source_codec {
            None => self.source_codec = Some(src_codec),
            Some(prev) if prev != src_codec => {
                bail!("video re-encoder: source codec changed mid-flow");
            }
            _ => {}
        }

        if self.decoder.is_none() {
            // Opened on the first access unit that carries the SPS (nothing
            // decodes before one), and seeded from it: an H.264 decoder's
            // reorder depth comes from its SPS (`ReorderSeed`). Opened on a
            // P picture at a mid-GOP join, a source that declares no
            // reordering would hold a frame for good.
            if !self.sps_gate.admits(src_codec, &self.annex_b_scratch) {
                return Ok(Vec::new());
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
        let Some(dec) = self.decoder.as_mut() else {
            return Ok(Vec::new());
        };
        // The PTS goes in with the access unit so the decoder hands each
        // picture back with its own (display order); an access unit whose
        // PES carried none goes in without (fed as 0, its picture came back
        // stamped 0), and its picture is stamped from the ones before it.
        match pts {
            Some(pts) => dec.send_packet_with_pts(&self.annex_b_scratch, (pts & PTS_MASK_33B) as i64),
            None => dec.send_packet(&self.annex_b_scratch),
        }
        .map_err(|e| anyhow::anyhow!("VideoDecoder send_packet failed: {e}"))?;
        let mut decoded = Vec::new();
        while let Ok(frame) = dec.receive_frame() {
            decoded.push(frame);
        }
        let mut out = Vec::new();
        for frame in &decoded {
            self.encode_decoded(frame, &mut out)?;
        }
        Ok(out)
    }

    /// One decoded picture through the rate lock and the encoder.
    fn encode_decoded(
        &mut self,
        decoded: &video_engine::DecodedFrame,
        out: &mut Vec<VideoOutFrame>,
    ) -> Result<()> {
        let admitted = self.rate.admit(decoded.pts(), &mut self.pipeline);
        let interval = crate::engine::video_encode_util::frame_interval_90k(self.pipeline.fps());
        let src_pts = self.stamper.stamp(decoded.pts(), interval);
        if !admitted {
            return Ok(());
        }
        if let Some(segment_secs) = self.segment_secs
            && !self.pipeline.is_open()
        {
            let (n, d) = self.pipeline.fps();
            self.pipeline.set_default_gop_if_unopened(cmaf_default_gop(segment_secs, n, d));
        }
        let counter = self.next_frame;
        self.next_frame += 1;
        // Live CMAF: an IDR on every GOP boundary of the encoded-frame count.
        // The encoder's own GOP drifts off the tiling at the first scene
        // cut: x264 codes a cut as an IDR and restarts its GOP count there,
        // so the next natural IDR — and the segment boundary the segmenter
        // cuts on — lands a partial GOP late (3.2 s segments where 2 s were
        // asked, measured on Sky Sports). Scene-cut IDRs in between cost
        // bits, never a boundary.
        if self.segment_secs.is_some()
            && let Some(gop) = self.pipeline.gop_size().filter(|g| *g > 1)
            && counter > 0
            && counter % i64::from(gop) == 0
        {
            self.pipeline.force_next_keyframe();
        }
        self.in_flight.push(counter, src_pts);
        let was_open = self.pipeline.is_open();
        let encoded = match self.pipeline.encode(decoded, Some(counter)) {
            Ok(encoded) => encoded,
            Err(e) => {
                self.in_flight.cancel_last();
                bail!("VideoEncoder encode_frame failed: {e}");
            }
        };
        if let Some(why) = self.pipeline.take_interlace_notice()
            && let (Some(events), Some(source)) = (self.events.as_ref(), self.source_codec)
        {
            crate::engine::video_encode_util::emit_output_interlace_unavailable(
                events,
                "CMAF",
                &self.output_id,
                &why,
                source,
            );
        }
        if !was_open && self.pipeline.is_open() {
            let (w, h) = self.pipeline.dst_dimensions();
            let (n, d) = self.pipeline.fps();
            tracing::info!(
                "CMAF output '{}': video re-encoder opened {}x{} at {n}/{d} fps, GOP {}",
                self.output_id,
                w,
                h,
                self.pipeline
                    .gop_size()
                    .map_or_else(|| "encoder default".to_string(), |g| format!("{g} frames")),
            );
        }
        for frame in encoded {
            self.push_encoded(frame, out);
        }
        Ok(())
    }

    /// Hand an encoded frame back with its picture's source PTS, found by
    /// the counter the encoder echoes.
    fn push_encoded(&mut self, frame: video_codec::EncodedVideoFrame, out: &mut Vec<VideoOutFrame>) {
        let pts = self.in_flight.take(frame.pts);
        // Convert Annex-B bitstream back to start-code-stripped NAL
        // units (CMAF samples carry length-prefixed NALs; the
        // segmenter re-applies length prefixes before packing).
        let mut nalus = Vec::new();
        split_annex_b_to_nalus(&frame.data, &mut nalus);
        if !nalus.is_empty() {
            out.push(VideoOutFrame { nalus, is_keyframe: frame.keyframe, pts });
        }
    }

    /// Drain whatever the decoder and then the encoder are still holding.
    ///
    /// Both buffer, so the last frames handed in are not the last frames
    /// out. A live output never notices — it runs until it is stopped — but a
    /// clip has an end, and without this the tail of every export is short by
    /// however deep the decoder reorders (its B-frame depth, or the one frame
    /// an H.264 decoder seeded for a join holds on an IPPP source) plus
    /// however deep the encoder buffers. The decoder is drained first, its
    /// frames encoded, and only then the encoder flushed. Each frame keeps
    /// its own picture's PTS, as from [`Self::encode_frame`].
    pub fn flush(&mut self) -> Result<Vec<VideoOutFrame>> {
        let mut decoded = Vec::new();
        if let Some(dec) = self.decoder.as_mut()
            && dec.send_flush().is_ok()
        {
            while let Ok(frame) = dec.receive_frame() {
                decoded.push(frame);
            }
        }
        let mut out = Vec::new();
        for frame in &decoded {
            self.encode_decoded(frame, &mut out)?;
        }
        for frame in self.pipeline.flush().map_err(|e| anyhow::anyhow!("{e}"))? {
            self.push_encoded(frame, &mut out);
        }
        Ok(out)
    }
}

/// 33-bit MPEG-TS timestamp mask.
#[cfg(feature = "media-codecs")]
const PTS_MASK_33B: u64 = (1 << 33) - 1;

#[cfg(not(feature = "media-codecs"))]
impl VideoReencoder {
    pub fn new(_cfg: &VideoEncodeConfig, output_id: &str, _segment_secs: Option<f64>) -> Result<Self> {
        bail!("video_encode requires the `media-codecs` feature (and a `video-encoder-*` backend) at build time")
    }

    pub fn encode_frame(
        &mut self,
        _nalus: &[Vec<u8>],
        _pts: Option<u64>,
        _is_keyframe: bool,
        _codec: CmafVideoCodec,
    ) -> Result<Vec<VideoOutFrame>> {
        bail!("video_encode disabled at build time")
    }

    pub fn encode_mpeg2(&mut self, _es: &[u8], _pts: Option<u64>) -> Result<Vec<VideoOutFrame>> {
        bail!("video_encode disabled at build time")
    }

    pub fn set_event_sender(&mut self, _events: crate::manager::events::EventSender) {}
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

    /// What an in-process AAC encoder writes in its ADTS headers is what a
    /// track built before its first frame announces
    /// ([`expected_adts_config`], the silent-fallback track's source): for
    /// HE-AAC the core, half the rate, v2's mono core; 7.1 as channel
    /// configuration 0 with its PCE. Read off fdk's own first frame, for
    /// each codec at the rates and layouts it opens at.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn the_expected_adts_config_is_what_the_encoder_writes() {
        use crate::stats::collector::OutputStatsAccumulator;
        let cases: [(AudioCodec, &[u32], &[u8]); 3] = [
            (AudioCodec::AacLc, &[48_000, 44_100, 32_000, 24_000, 16_000], &[1, 2, 6, 8]),
            (AudioCodec::HeAacV1, &[48_000, 44_100, 32_000], &[1, 2]),
            (AudioCodec::HeAacV2, &[48_000, 44_100, 32_000], &[2]),
        ];
        for (codec, rates, layouts) in cases {
            for &rate in rates {
                for &channels in layouts {
                    let what = format!("{} {rate} Hz {channels} ch", codec.as_str());
                    let mut enc = AudioEncoder::spawn(
                        EncoderParams {
                            codec,
                            sample_rate: rate,
                            channels,
                            target_bitrate_kbps: codec.default_bitrate_kbps() * u32::from(channels.max(2)) / 2,
                            target_sample_rate: rate,
                            target_channels: channels,
                            opus_vbr_mode: None,
                            opus_fec: false,
                            opus_dtx: false,
                            opus_frame_duration_ms: None,
                        },
                        CancellationToken::new(),
                        "f".into(),
                        "o".into(),
                        Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "cmaf".into())),
                        None,
                    )
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                    let silence = vec![vec![0.0f32; 1024]; channels as usize];
                    let mut first = None;
                    for k in 0..16u64 {
                        enc.submit_planar(&silence, k * 1_920);
                        if let Some(f) = std::iter::from_fn(|| enc.try_recv()).find(|f| !f.data.is_empty()) {
                            first = Some(f);
                            break;
                        }
                    }
                    let frame = first.unwrap_or_else(|| panic!("{what}: no frame"));
                    let (au, (profile, sr_idx, ch_cfg)) = adts_access_unit(&frame.data).expect("one whole ADTS frame");
                    assert_eq!(
                        AacConfig::from_adts(profile, sr_idx, ch_cfg, au),
                        expected_adts_config(codec, rate, channels),
                        "{what}"
                    );
                }
            }
        }
    }

    /// The silent-fallback track is the eager encoder's format, which the
    /// declared one is clamped to (stereo at most): a declared 5.1 used to
    /// build a six-channel track over the stereo the encoder emits. HE-AAC
    /// announces its core.
    #[test]
    #[cfg(feature = "fdk-aac")]
    fn the_silent_fallback_track_is_the_eager_encoders_format() {
        let cancel = CancellationToken::new();
        let mut six = ae("aac_lc", true);
        six.channels = Some(6);
        let r = AudioReencoder::new(&six, None, &cancel, "o", "f").expect("eager encoder");
        assert_eq!(r.silent_fallback_track(), AacConfig::new(1, 3, 2), "stereo, as the encoder");
        let r = AudioReencoder::new(&ae("he_aac_v1", true), None, &cancel, "o", "f").expect("eager encoder");
        assert_eq!(r.silent_fallback_track(), AacConfig::new(1, 6, 2), "HE-AAC: the 24 kHz core");
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
        assert_eq!(track, AacConfig::new(1, 3, 2).unwrap());
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
        // The re-anchor first runs out what the encoder still held of the
        // silence, on the silence's timeline, then the real audio follows
        // from its PTS less the codec delay — less the first frame of the
        // zeros that run-out left in the delay line, which is not sent.
        let (flushed, out): (Vec<_>, Vec<_>) = out.into_iter().partition(|(_, p)| *p < real - delay);
        let last_silent = more[more.len() - 1].1;
        assert!(!flushed.is_empty() && flushed[0].1 == last_silent + frame, "the silence runs out on its timeline");
        assert!(flushed.windows(2).all(|w| w[1].1 == w[0].1 + frame));
        assert!(!out.is_empty());
        assert!(
            out[0].1.abs_diff(real - delay + frame) <= 1,
            "the first real frame must sit at the source's PTS minus codec delay, plus the frame not sent, \
             got {} for {real}",
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
        assert_eq!(laid[0].1, out[0].1 + 5 * frame, "the silence continues the real audio exactly");

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

    /// A source that changes rate mid-stream (a 48 kHz programme, then a
    /// 44.1 kHz break) rebuilds the encoder: what the retired one still
    /// held — the end of the 48 kHz audio, its delay line and partial frame
    /// — goes out on its own timeline first, then the new encoder's frames
    /// after it, none at or before the last of those. Dropped with the old
    /// encoder, that audio left a hole the new encoder's priming filled:
    /// the burst at the programme's end was never sent.
    #[test]
    #[cfg(feature = "fdk-aac")]
    fn a_rebuild_for_another_rate_runs_the_old_encoder_out_on_its_timeline() {
        let cancel = CancellationToken::new();
        let mut r = AudioReencoder::new(&ae("aac_lc", false), None, &cancel, "out5", "flow5")
            .expect("fdk-aac in-process encoder");
        const P0: u64 = 900_000;
        let burst = |rate: u32| -> Vec<f32> {
            let n = (rate / 100) as usize;
            (0..n)
                .map(|k| {
                    let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / (n - 1) as f32).cos();
                    0.5 * w * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / rate as f32).sin()
                })
                .collect()
        };
        // A second of 48 kHz ending in a burst 300 samples from its end,
        // then a second of 44.1 kHz silence, contiguous.
        let old_len = 48_000usize;
        let at = old_len - 480 - 300;
        let mut old = vec![0.0f32; old_len];
        old[at..at + 480].copy_from_slice(&burst(48_000));
        let mut out = Vec::new();
        let mut fed = 0u64;
        for chunk in old.chunks(1_024) {
            let pts = P0 + fed * 90_000 / 48_000;
            out.extend(r.encode_planar(&[chunk.to_vec(), chunk.to_vec()], pts, 48_000, 2).unwrap());
            fed += chunk.len() as u64;
        }
        let p1 = P0 + 90_000;
        let new = vec![0.0f32; 44_100];
        let mut fed = 0u64;
        for chunk in new.chunks(1_024) {
            let pts = p1 + fed * 90_000 / 44_100;
            out.extend(r.encode_planar(&[chunk.to_vec(), chunk.to_vec()], pts, 44_100, 2).unwrap());
            fed += chunk.len() as u64;
        }
        assert_eq!(r.encoder.as_ref().unwrap().params().sample_rate, 44_100, "rebuilt");
        assert!(out.windows(2).all(|w| w[1].1 > w[0].1), "the stamps never step back");
        // Frames come back as bare access units: decode them against the
        // configuration the track announces.
        let asc = r.encoder_track().expect("a frame is out").audio_specific_config();
        let mut d = aac_audio::AacDecoder::open_raw(&asc).unwrap();
        let (mut times, mut pcm) = (Vec::new(), Vec::new());
        for (data, pts) in &out {
            let p = d.decode_frame(data).unwrap().planar[0].clone();
            times.extend((0..p.len()).map(|i| *pts as f64 + i as f64 * 90_000.0 / 48_000.0));
            pcm.extend(p);
        }
        let b = burst(48_000);
        let best = (0..pcm.len() - b.len())
            .max_by(|&x, &y| {
                let s = |k: usize| -> f32 { b.iter().zip(&pcm[k..]).map(|(p, q)| p * q).sum() };
                s(x).total_cmp(&s(y))
            })
            .unwrap();
        let energy: f32 = b.iter().zip(&pcm[best..]).map(|(p, q)| p * q).sum();
        let expected = P0 as f64 + at as f64 * 90_000.0 / 48_000.0;
        assert!(energy > 10.0, "the burst before the rebuild is sent: {energy}");
        assert!((times[best] - expected).abs() <= 2.0 * 90_000.0 / 48_000.0, "at {} for {expected}", times[best]);
    }

    /// The other side of a rebuild: the new audio's first frame decodes
    /// whole and lands at its source time, whatever the retired encoder
    /// held. Every new frame at or before the retired run-out's last stamp
    /// used to be dropped: that took the new encoder's last priming frame —
    /// the overlap-add partner of its first content frame, which then
    /// decoded garbled (held 200 / 600: the burst at 1.6 of its 22.4, 286
    /// ticks off) — and, when the retired encoder held fewer samples than
    /// the new resampler's delay (~34), the content frame itself (held 10 /
    /// 31 / 33: the burst lost). The partner is now kept, one step before
    /// the content frame, and the run-out's last frame, when the content
    /// frame starts no later than it, dropped. The stamps never step back,
    /// nor do two share a decode time in the track's timescale.
    #[test]
    #[cfg(feature = "fdk-aac")]
    fn a_rebuild_keeps_the_new_audios_first_frame_whole() {
        const P0: u64 = 900_000;
        let burst = |rate: u32| -> Vec<f32> {
            let n = (rate / 100) as usize;
            (0..n)
                .map(|k| {
                    let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / (n - 1) as f32).cos();
                    0.5 * w * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / rate as f32).sin()
                })
                .collect()
        };
        for held in [0usize, 10, 31, 33, 34, 35, 200, 600, 1_000] {
            let cancel = CancellationToken::new();
            let mut r = AudioReencoder::new(&ae("aac_lc", false), None, &cancel, "out6", "flow6")
                .expect("fdk-aac in-process encoder");
            // 48 kHz silence leaving `held` samples in the encoder's
            // accumulator, then 44.1 kHz audio with a burst 100 samples in.
            let old_len = 46 * 1_024 + held;
            let old = vec![0.0f32; old_len];
            let mut out = Vec::new();
            let mut fed = 0u64;
            for chunk in old.chunks(1_024) {
                let pts = P0 + fed * 90_000 / 48_000;
                out.extend(r.encode_planar(&[chunk.to_vec(), chunk.to_vec()], pts, 48_000, 2).unwrap());
                fed += chunk.len() as u64;
            }
            let p1 = P0 as f64 + old_len as f64 * 90_000.0 / 48_000.0;
            let mut new = vec![0.0f32; 44_100];
            new[100..100 + 441].copy_from_slice(&burst(44_100));
            let mut fed = 0u64;
            for chunk in new.chunks(1_024) {
                let pts = p1.round() as u64 + fed * 90_000 / 44_100;
                out.extend(r.encode_planar(&[chunk.to_vec(), chunk.to_vec()], pts, 44_100, 2).unwrap());
                fed += chunk.len() as u64;
            }
            assert_eq!(r.encoder.as_ref().unwrap().params().sample_rate, 44_100, "held {held}: rebuilt");
            for w in out.windows(2) {
                assert!(w[1].1 >= w[0].1 + SPLICE_STEP_90K, "held {held}: {} then {}", w[0].1, w[1].1);
            }
            let asc = r.encoder_track().expect("a frame is out").audio_specific_config();
            let mut d = aac_audio::AacDecoder::open_raw(&asc).unwrap();
            let (mut times, mut pcm) = (Vec::new(), Vec::new());
            for (data, pts) in &out {
                let p = d.decode_frame(data).unwrap().planar[0].clone();
                times.extend((0..p.len()).map(|i| *pts as f64 + i as f64 * 90_000.0 / 48_000.0));
                pcm.extend(p);
            }
            let b = burst(48_000);
            let best = (0..pcm.len() - b.len())
                .max_by(|&x, &y| {
                    let s = |k: usize| -> f32 { b.iter().zip(&pcm[k..]).map(|(p, q)| p * q).sum() };
                    s(x).total_cmp(&s(y))
                })
                .unwrap();
            let energy: f32 = b.iter().zip(&pcm[best..]).map(|(p, q)| p * q).sum();
            let expected = p1 + 100.0 * 90_000.0 / 44_100.0;
            assert!(energy > 20.0, "held {held}: the burst decodes whole: {energy} of ~22.4");
            assert!(
                (times[best] - expected).abs() <= 4.0,
                "held {held}: the burst at {} for {expected}",
                times[best]
            );
        }
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

#[cfg(all(test, feature = "media-codecs"))]
mod gop_tests {
    use super::cmaf_default_gop;

    /// The default GOP tiles the segment at the measured rate, each GOP at
    /// most 2 s and rounded up to a whole frame, so the segmenter's first
    /// IDR at or past the target is the segment's own end.
    #[test]
    fn the_default_gop_tiles_the_segment() {
        assert_eq!(cmaf_default_gop(2.0, 25, 1), 50);
        assert_eq!(cmaf_default_gop(2.0, 30, 1), 60);
        assert_eq!(cmaf_default_gop(2.0, 30_000, 1001), 60, "59.94 frames rounds up");
        assert_eq!(cmaf_default_gop(2.0, 24_000, 1001), 48);
        assert_eq!(cmaf_default_gop(2.0, 50, 1), 100);
        assert_eq!(cmaf_default_gop(1.0, 25, 1), 25);
        // 6 s: three 2 s GOPs; 29.97 fps: 3 x 60 frames (6.006 s).
        assert_eq!(cmaf_default_gop(6.0, 25, 1), 50);
        assert_eq!(cmaf_default_gop(6.0, 30_000, 1001), 60);
        // 5 s: three GOPs of 1 2/3 s.
        assert_eq!(cmaf_default_gop(5.0, 25, 1), 42);
        assert_eq!(cmaf_default_gop(10.0, 60, 1), 120);
        for (seg, num, den) in [(2.0, 25, 1), (5.0, 25, 1), (6.0, 30_000, 1001), (3.0, 24_000, 1001)] {
            let gop = cmaf_default_gop(seg, num, den) as f64;
            let gops = (seg / 2.0_f64).ceil();
            let frame = den as f64 / num as f64;
            assert!(gops * gop * frame >= seg - 1e-9, "the last GOP reaches the target");
            assert!((gops - 1.0) * gop * frame < seg, "no earlier IDR does");
        }
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
            // A texture panning a pixel a frame: x264 codes it with B-frames.
            let y: Vec<u8> = (0..w * h)
                .map(|k| (((k % w + i) * 7919 + (k / w) * 104_729) % 181) as u8 + 30)
                .collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        assert_eq!(aus.len(), n);
        assert!(aus.windows(2).any(|a| a[1].pts < a[0].pts), "the source reorders");

        // Pinned, as the clip export pins it: no frame waits for a rate.
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "gop_size": 1, "bframes": 0, "preset": "veryfast",
            "fps_num": 25, "fps_den": 1
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "clip-test", None).unwrap();
        let mut out = Vec::new();
        for au in &aus {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(&au.data, &mut nalus);
            out.extend(
                re.encode_frame(&nalus, Some(au.pts as u64 * 3_600), false, CmafVideoCodec::H264).unwrap(),
            );
        }
        let before_flush = out.len();
        out.extend(re.flush().unwrap());
        assert!(out.len() > before_flush, "the decoder held pictures back");
        assert_eq!(out.len(), n, "every source frame comes back");
        // Each on its own picture's PTS, in display order — the B-frame
        // source went in I P B B.
        let pts: Vec<Option<u64>> = out.iter().map(|f| f.pts).collect();
        let want: Vec<Option<u64>> = (0..n as u64).map(|i| Some(i * 3_600)).collect();
        assert_eq!(pts, want);
    }

    /// An MPEG-2 video source is re-encoded (CMAF carries H.264 / HEVC): its
    /// ES goes to an MPEG-2 decoder as it is, and every picture comes back as
    /// H.264 on its own PTS, the first an IDR carrying its SPS. MPEG-2 used
    /// to be dropped whatever `video_encode` said.
    ///
    /// The source is a vendored 13 kB ES — 40 pictures of `testsrc` at
    /// 176x144, 25 fps, GOP 12, no B-frames, `-q:v 31` (made with
    /// `ffmpeg -f lavfi -i testsrc=size=176x144:rate=25 -frames:v 40
    /// -c:v mpeg2video -g 12 -bf 0 -q:v 31 -f mpeg2video`). It used to be
    /// made at test time by the `ffmpeg` CLI, and the test passed without
    /// running wherever that was absent — CI's runner among them.
    #[test]
    fn an_mpeg2_source_is_re_encoded() {
        let es: &[u8] = include_bytes!("../testdata/mpeg2_176x144_25p_40f.m2v");
        // One access unit per picture: each picture start code (00 00 01 00),
        // with the sequence / GOP headers before it.
        let mut cuts = vec![0usize];
        let mut seen_picture = false;
        for i in 0..es.len().saturating_sub(4) {
            if es[i..i + 3] == [0, 0, 1] && es[i + 3] == 0x00 {
                if seen_picture {
                    // Start the next unit at any sequence / GOP header
                    // directly ahead of this picture.
                    let back = (cuts[cuts.len() - 1]..i)
                        .rev()
                        .find(|&j| es[j..j + 3] == [0, 0, 1] && matches!(es[j + 3], 0xB3 | 0xB8))
                        .filter(|&j| es[j..i].windows(4).all(|w| !(w[..3] == [0, 0, 1] && w[3] == 0x01)));
                    cuts.push(back.unwrap_or(i));
                }
                seen_picture = true;
            }
        }
        cuts.push(es.len());
        let aus: Vec<&[u8]> = cuts.windows(2).map(|w| &es[w[0]..w[1]]).collect();
        assert_eq!(aus.len(), 40);
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast", "bframes": 0, "fps_num": 25, "fps_den": 1
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "mpeg2-test", None).unwrap();
        let mut out = Vec::new();
        for (i, au) in aus.iter().enumerate() {
            out.extend(re.encode_mpeg2(au, Some(900_000 + i as u64 * 3_600)).unwrap());
        }
        out.extend(re.flush().unwrap());
        assert_eq!(out.len(), 40, "every picture comes back");
        assert!(out[0].is_keyframe);
        assert!(out[0].nalus.iter().any(|n| n[0] & 0x1F == 7), "H.264, SPS on the IDR");
        let pts: Vec<Option<u64>> = out.iter().map(|f| f.pts).collect();
        let want: Vec<Option<u64>> = (0..40u64).map(|i| Some(900_000 + i * 3_600)).collect();
        assert_eq!(pts, want);
    }

    /// Unpinned, the live re-encoder opens its encoder at the source's
    /// measured rate — it used to open at a flat 30/1, so a 25 fps source's
    /// VUI said 30 fps — and sizes the default GOP to the segment at that
    /// rate: 50 frames for a 2 s segment at 25 fps (the flat 60 it replaced
    /// was 2.4 s, so every 2 s segment ran 2.4 s).
    #[test]
    fn an_unpinned_encode_opens_at_the_measured_rate_and_tiles_the_segment() {
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
        for i in 0..130 {
            // A still gradient with a small block moving across it: nothing
            // x264's scene-cut detection would start a GOP early on.
            let y: Vec<u8> = (0..w * h)
                .map(|k| {
                    let (x, row) = (k % w, k / w);
                    if (100..116).contains(&row) && (2 * i..2 * i + 16).contains(&x) {
                        235
                    } else {
                        (40 + x / 2) as u8
                    }
                })
                .collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast"
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "rate-test", Some(2.0)).unwrap();
        let mut out = Vec::new();
        for au in &aus {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(&au.data, &mut nalus);
            out.extend(
                re.encode_frame(&nalus, Some(900_000 + au.pts as u64 * 3_600), false, CmafVideoCodec::H264)
                    .unwrap(),
            );
        }
        assert_eq!(re.pipeline.fps(), (25, 1));
        assert_eq!(re.pipeline.gop_size(), Some(50));
        let first = out.iter().position(|f| f.is_keyframe).unwrap();
        let sps = out[first]
            .nalus
            .iter()
            .find_map(|n| video_engine::find_h264_sps(&[&[0, 0, 0, 1][..], n].concat()))
            .expect("the IDR carries its SPS");
        assert_eq!(sps.timing.map(|(n, t, _)| (n, t)), Some((1, 50)), "VUI 25 fps");
        let idrs: Vec<u64> = out.iter().filter(|f| f.is_keyframe).map(|f| f.pts.unwrap()).collect();
        assert!(idrs.len() >= 2, "{idrs:?}");
        assert!(idrs.windows(2).all(|w| w[1] - w[0] == 180_000), "an IDR every 2 s: {idrs:?}");
        // The frames dropped while the rate was measured are the only ones
        // missing: every later picture came back on its own PTS.
        let pts: Vec<u64> = out.iter().map(|f| f.pts.unwrap()).collect();
        assert!(pts.windows(2).all(|w| w[1] - w[0] == 3_600), "{pts:?}");
        assert!(out.len() + 8 >= aus.len(), "{} of {} frames", out.len(), aus.len());
    }

    /// A source that stamps only every 12th picture (its I pictures, say):
    /// the access units without a PTS go to the decoder without one, so the
    /// rate lock counts their pictures and measures 25 fps, and each such
    /// picture is stamped a frame past the one before it. Fed PTS 0 (the
    /// demuxer's default), their pictures came back stamped 0: the meter
    /// never measured — 60 frames dropped, then 30/1 — and the samples
    /// carried 0.
    #[test]
    fn a_sparse_pts_source_measures_its_rate_and_stamps_every_picture() {
        let aus = crate::engine::output_rtmp::x264_test_source(100, 3_600);
        let cfg: VideoEncodeConfig =
            serde_json::from_value(serde_json::json!({"codec": "x264", "preset": "veryfast"})).unwrap();
        let mut re = VideoReencoder::new(&cfg, "sparse-test", Some(2.0)).unwrap();
        let mut out = Vec::new();
        for (k, (au, pts)) in aus.iter().enumerate() {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(au, &mut nalus);
            let pts = (k % 12 == 0).then_some(*pts);
            out.extend(re.encode_frame(&nalus, pts, false, CmafVideoCodec::H264).unwrap());
        }
        assert_eq!(re.pipeline.fps(), (25, 1), "measured over the unstamped pictures");
        let pts: Vec<u64> = out.iter().map(|f| f.pts.expect("every picture stamped")).collect();
        assert!(pts.len() >= 30, "{} frames", pts.len());
        assert!(pts.windows(2).all(|w| w[1] == w[0] + 3_600), "{pts:?}");
    }

    /// `bframes` on the CMAF re-encode is pinned to 0: with B-frames (x264
    /// with a tune that honours them) the encoder handed frames back in
    /// decode order, each B-frame lost its PTS and was stamped with the
    /// access unit being fed — and the segmenter, which takes a stamp as the
    /// decode time, saw its timeline step back. Every frame comes back on
    /// its own picture's PTS, in order.
    #[test]
    fn b_frames_are_pinned_off_on_the_cmaf_re_encode() {
        // A texture panning a pixel a frame, which x264 codes with B-frames
        // when it may (the plain gradient of `x264_test_source` it does not).
        let (w, h) = (320usize, 240usize);
        let mut src = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            gop_size: 50,
            preset: VideoPreset::Veryfast,
            global_header: false,
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut aus = Vec::new();
        for i in 0..40usize {
            let y: Vec<u8> = (0..w * h)
                .map(|k| (((k % w + i) * 7919 + (k / w) * 104_729) % 181) as u8 + 30)
                .collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(src.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(src.flush().unwrap());
        let aus: Vec<(Vec<u8>, u64)> =
            aus.into_iter().map(|f| (f.data, 900_000 + f.pts as u64 * 3_600)).collect();
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast", "tune": "fastdecode", "bframes": 2,
            "fps_num": 25, "fps_den": 1
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "bframe-test", Some(2.0)).unwrap();
        let mut out = Vec::new();
        for (au, pts) in &aus {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(au, &mut nalus);
            out.extend(re.encode_frame(&nalus, Some(*pts), false, CmafVideoCodec::H264).unwrap());
        }
        out.extend(re.flush().unwrap());
        let pts: Vec<u64> = out.iter().map(|f| f.pts.expect("its own picture's PTS")).collect();
        assert!(pts.len() >= 30, "{} frames", pts.len());
        assert!(pts.windows(2).all(|w| w[1] == w[0] + 3_600), "{pts:?}");
    }

    /// An encoder that buffers (x264 with its lookahead, not `zerolatency`)
    /// hands frames back well after they went in; each still comes back on
    /// its own picture's PTS, matched by the counter the encoder echoes —
    /// not the PTS of whatever was fed last.
    #[test]
    fn a_buffering_encoder_keeps_each_frames_pts() {
        let aus = crate::engine::output_rtmp::x264_test_source(40, 3_600);
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast", "tune": "fastdecode", "fps_num": 25, "fps_den": 1
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "lookahead-test", Some(2.0)).unwrap();
        let mut out = Vec::new();
        for (au, pts) in &aus {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(au, &mut nalus);
            out.extend(re.encode_frame(&nalus, Some(*pts), false, CmafVideoCodec::H264).unwrap());
        }
        let before_flush = out.len();
        out.extend(re.flush().unwrap());
        assert!(before_flush + 5 < out.len(), "the encoder buffered: {before_flush} of {}", out.len());
        let pts: Vec<Option<u64>> = out.iter().map(|f| f.pts).collect();
        let want: Vec<Option<u64>> = aus.iter().map(|(_, p)| Some(*p)).collect();
        assert_eq!(pts, want);
    }

    /// Scene cuts do not move the segment boundaries: x264 codes a cut as
    /// an IDR and restarts its GOP count there, so its next natural IDR —
    /// the boundary the segmenter cuts on — landed a partial GOP late. An
    /// IDR is forced on every GOP boundary of the encoded-frame count.
    #[test]
    fn scene_cuts_keep_the_idrs_on_the_segment_tiling() {
        let (w, h) = (320usize, 240usize);
        let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            gop_size: 250,
            preset: VideoPreset::Veryfast,
            global_header: false,
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut aus = Vec::new();
        for i in 0..160 {
            // A hard cut every 30 frames: flat dark, then flat bright.
            let base = if (i / 30) % 2 == 0 { 40u8 } else { 200u8 };
            let y: Vec<u8> = (0..w * h)
                .map(|k| if (100..116).contains(&(k / w)) && (2 * i..2 * i + 16).contains(&(k % w)) { 235 } else { base })
                .collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        // Pinned, so no frame is dropped while a rate is measured: the
        // encoded-frame count is the source frame index.
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast", "fps_num": 25, "fps_den": 1
        }))
        .unwrap();
        let mut re = VideoReencoder::new(&cfg, "cut-test", Some(2.0)).unwrap();
        let mut out = Vec::new();
        for au in &aus {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(&au.data, &mut nalus);
            out.extend(re.encode_frame(&nalus, Some(au.pts as u64 * 3_600), false, CmafVideoCodec::H264).unwrap());
        }
        assert_eq!(re.pipeline.gop_size(), Some(50));
        let idrs: Vec<u64> = out.iter().filter(|f| f.is_keyframe).map(|f| f.pts.unwrap() / 3_600).collect();
        assert!(idrs.contains(&30), "the fixture has a scene-cut IDR: {idrs:?}");
        for boundary in [0, 50, 100, 150] {
            assert!(idrs.contains(&boundary), "an IDR at frame {boundary}: {idrs:?}");
        }
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
        let mut re = VideoReencoder::new(&cfg, "join-test", Some(2.0)).unwrap();
        let mut out = 0usize;
        for (i, au) in aus.iter().enumerate().skip(join) {
            let mut nalus = Vec::new();
            split_annex_b_to_nalus(&au.data, &mut nalus);
            out += re
                .encode_frame(&nalus, Some(i as u64 * 3_600), false, CmafVideoCodec::H264)
                .unwrap()
                .len();
            if i < next_sps {
                assert!(re.decoder.is_none(), "AU {i} carries no SPS: passed over");
            }
        }
        let dec = re.decoder.as_ref().expect("opened on the SPS");
        assert_eq!(dec.reorder_depth(), 0, "a declared IPPP source holds no frame");
        assert!(out >= 10, "{out} frames re-encoded");
    }
}
