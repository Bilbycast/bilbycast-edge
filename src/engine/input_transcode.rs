// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ingress-side audio + video transcoding composer for TS-carrying inputs.
//!
//! Wraps [`super::ts_audio_replace::TsAudioReplacer`] and
//! [`super::ts_video_replace::TsVideoReplacer`] in the same audio-first-then-video
//! order already used by the TS outputs (see `output_srt.rs`), so every Group A
//! input (RTP, UDP, SRT, RIST, RTMP, RTSP, WebRTC/WHIP, WebRTC/WHEP) can
//! normalise its feed *once* before the flow's broadcast channel and amortise
//! codec work across all attached outputs.
//!
//! The design goals come straight from the root CLAUDE.md:
//!
//! * **Never block the data path** — codec calls are synchronous and take
//!   single-digit milliseconds per frame. Callers wrap [`InputTranscoder::process`]
//!   in `tokio::task::block_in_place` exactly like the output-side does.
//! * **Lock-free on hot paths** — no mutexes, no allocations once the scratch
//!   buffers reach steady state. The composer owns two `Vec<u8>` scratches and
//!   reuses them across calls via `.clear() + extend_from_slice`.
//! * **Drop rather than stall** — if the underlying replacer produces no bytes
//!   for a tick (audio bootstrap, PMT not yet seen), `process` returns an
//!   empty slice; the input task just skips the `broadcast_tx.send` for that
//!   chunk and keeps going.
//!
//! See `docs/transcoding.md` for the licensing / feature-flag matrix.

use std::sync::Arc;

use crate::config::models::{AudioEncodeConfig, VideoEncodeConfig};

use super::audio_transcode::TranscodeJson;
use super::ts_audio_replace::{GapFill, TsAudioReplaceError, TsAudioReplacer};
use super::ts_pcr_remux::TsPcrRemux;
use super::ts_video_replace::{TsVideoReplaceError, TsVideoReplacer, VideoEncodeStats};

/// Errors raised when constructing an [`InputTranscoder`].
#[derive(Debug)]
#[allow(dead_code)]
pub enum InputTranscoderError {
    /// The audio replacer rejected its config (unknown codec, unsupported
    /// codec-for-TS, feature flag missing).
    Audio(TsAudioReplaceError),
    /// The video replacer rejected its config (unknown backend, backend
    /// feature not compiled in, video-engine missing).
    Video(TsVideoReplaceError),
}

impl std::fmt::Display for InputTranscoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Audio(e) => write!(f, "input audio transcode: {e}"),
            Self::Video(e) => write!(f, "input video transcode: {e}"),
        }
    }
}

impl std::error::Error for InputTranscoderError {}

impl From<TsAudioReplaceError> for InputTranscoderError {
    fn from(e: TsAudioReplaceError) -> Self {
        Self::Audio(e)
    }
}

impl From<TsVideoReplaceError> for InputTranscoderError {
    fn from(e: TsVideoReplaceError) -> Self {
        Self::Video(e)
    }
}

/// Composite ingress transcoder. Routes raw 188-byte-aligned MPEG-TS through
/// optional audio and video ES replacers in order.
///
/// Not `Sync` (the inner codecs hold raw C state), but `Send` so the whole
/// instance can be moved to the input task at spawn time.
pub struct InputTranscoder {
    audio: Option<TsAudioReplacer>,
    video: Option<TsVideoReplacer>,
    /// Scratch buffer that holds the audio-replacer output. Reused across
    /// ticks to avoid steady-state allocations.
    scratch_a: Vec<u8>,
    /// Scratch buffer that holds the video-replacer output (input to the
    /// PCR stage).
    scratch_b: Vec<u8>,
    /// Scratch buffer that holds the PCR stage's output (input to the A/V
    /// realign stage, or the final output when no realigner is active).
    scratch_p: Vec<u8>,
    /// Scratch buffer that holds the final output after A/V realignment.
    scratch_c: Vec<u8>,
    /// Trailing PCR stage: the output PCR is the input's, delayed by one
    /// measured transcode allowance (`ts_pcr_remux`).
    pcr: TsPcrRemux,
    /// A/V emission realigner. `Some` only when the **video** ES is
    /// transcoded — the deep video encode pipeline is what pushes audio
    /// ahead of video, so there's nothing to realign on an audio-only
    /// transcode (video passthrough isn't delayed). Holds audio gated on
    /// video PTS so the two leave together. See `ts_av_realign`.
    realign: Option<crate::engine::ts_av_realign::TsAvRealigner>,
}

impl InputTranscoder {
    /// Build a transcoder from optional `audio_encode`, `transcode`, and
    /// `video_encode` blocks.
    ///
    /// Returns `Ok(None)` when all three are `None` — the caller should skip
    /// the stage entirely in that case (zero cost, no scratch buffers
    /// allocated, no codec state opened).
    ///
    /// PID rewriting (the operator's `pid_overrides` map) is handled by the
    /// `InputPostProcess` stage that runs after this transcoder in every
    /// input task — see `input_post_process.rs`.
    pub fn new(
        audio_encode: Option<&AudioEncodeConfig>,
        transcode: Option<&TranscodeJson>,
        video_encode: Option<&VideoEncodeConfig>,
        force_idr: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Option<Self>, InputTranscoderError> {
        if audio_encode.is_none() && transcode.is_none() && video_encode.is_none() {
            return Ok(None);
        }

        let audio = match audio_encode {
            Some(ae) => Some(TsAudioReplacer::new(ae, transcode.cloned())?),
            None => None,
        };
        let video = match video_encode {
            Some(ve) => Some(TsVideoReplacer::new(ve, force_idr)?),
            None => None,
        };

        // If `transcode` is set but `audio_encode` is not, the transcode block
        // has no encoder to feed — ignore silently to match the output-side
        // contract (transcode is a sub-block of audio_encode in practice).
        // We still return `Some` if video_encode is set.
        if audio.is_none() && video.is_none() {
            return Ok(None);
        }

        // A/V emission realigner — DISABLED pending a redesign.
        //
        // The intent was to hold audio and release it gated on video PTS so it
        // stops running ~0.5 s ahead of its (deep-HW-pipeline-delayed) video.
        // The broadcast-readiness codec matrix (2026-05-30) showed the
        // byte-stream batch-release approach is too source-dependent: it helped
        // some sources (sky-witness: audio_pts−PCR 598→208 ms) but OVER-HELD on
        // others (sync-test 1080p25→hevc_qsv: −133 ms ⇒ audio LATE, dropped by a
        // strict T-STD decoder). Neither per-frame draining nor a PCR floor
        // fixed sync-test — the interleaving fundamentally fights the bursty,
        // pipelined transcode output. An audio-EARLY stream is benign (PTS are
        // correct; receivers buffer it; never late), so we keep that safe state
        // rather than risk audio drops. The module + tests are retained for a
        // future PTS-ordered-merge implementation. See ts_av_realign.rs.
        let realign: Option<crate::engine::ts_av_realign::TsAvRealigner> = None;

        let mut pcr = TsPcrRemux::new();
        // The input's muxer-mode rewriter is told of this stage's own PCR
        // steps (`take_pcr_steps`, handed over in
        // `process_input_packet_with_post`).
        pcr.report_pcr_steps();
        let mut video = video;
        if let Some(v) = video.as_mut() {
            v.set_pcr_remux_stats(pcr.stats_handle());
        }
        Ok(Some(Self {
            audio,
            video,
            scratch_a: Vec::with_capacity(32 * 1024),
            scratch_b: Vec::with_capacity(32 * 1024),
            scratch_p: Vec::with_capacity(32 * 1024),
            scratch_c: Vec::with_capacity(32 * 1024),
            pcr,
            realign,
        }))
    }

    /// How the audio replacer fills a forward gap in the source audio
    /// timeline (no-op without an audio stage). Only `media_player` asks
    /// for [`GapFill::Relabel`]: its file splices step the PTS over
    /// continuous content. Neither replacer takes the flow's A/V sync
    /// pacer: they stamp source-relative PTS, the `ts_pcr_remux` stage
    /// re-stamps the input's PCR, and the input's `ts_pts_rewriter` then
    /// anchors the whole stream to the master clock in muxer mode.
    pub fn set_audio_gap_fill(&mut self, fill: GapFill) {
        if let Some(a) = self.audio.as_mut() {
            a.set_gap_fill(fill);
        }
    }

    /// Wire the per-input edge-added A/V skew reporter onto both
    /// replacer stages (`stats::av_skew`). Called by
    /// `register_ingress_stats` so every input module gets it without
    /// per-module wiring.
    pub fn set_av_skew_reporter(
        &mut self,
        reporter: Arc<crate::stats::av_skew::AvSkewReporter>,
    ) {
        if let Some(a) = self.audio.as_mut() {
            a.set_av_skew_reporter(reporter.clone());
        }
        if let Some(v) = self.video.as_mut() {
            v.set_av_skew_reporter(reporter);
        }
    }

    /// Wire the event watchdogs (all input-scoped) onto both stages:
    ///
    /// - the video replacer's decode-stall watchdog, so an ingress video
    ///   transcode whose decoder consumes input but produces no frames
    ///   surfaces a one-shot `video_transcode_decode_stalled` Warning instead
    ///   of silently shipping audio-only TS — the field report's
    ///   `media_player` input + `video_encode` QSV-decode-fails case — and
    ///   its engage watchdog (`video_transcode_source_not_found` / `_found`,
    ///   `video_source_pid_not_found`);
    /// - the audio replacer's engage watchdog
    ///   (`audio_transcode_source_not_found` / `_found`,
    ///   `audio_source_pid_not_found`);
    /// - the PCR stage's `transcode_pcr_late`,
    ///   `transcode_pcr_residency_exceeded` and `transcode_pcr_synthesized`.
    ///
    /// Each is a no-op when the stage is absent. Mirrors the output-side
    /// wiring in `transcode_chain::build_for_output`.
    pub fn set_decode_stall_watchdog(
        &mut self,
        event_sender: crate::manager::events::EventSender,
        input_id: impl Into<String>,
    ) {
        let input_id = input_id.into();
        self.pcr.set_event_sink(event_sender.clone(), input_id.clone(), true);
        if let Some(a) = self.audio.as_mut() {
            a.set_event_watchdog(event_sender.clone(), input_id.clone(), true);
        }
        if let Some(v) = self.video.as_mut() {
            v.set_decode_stall_watchdog_input(event_sender, input_id);
        }
    }

    /// Pass one chunk of raw 188-byte-aligned TS through the audio stage, then
    /// the video stage. Returns the transformed output as a borrowed slice of
    /// the internal scratch buffer. The slice is valid until the next call to
    /// `process` / `flush`, so the caller must copy or clone before the next
    /// invocation.
    ///
    /// An empty return slice means "nothing to emit this tick" — either input
    /// was empty, or the replacers buffered the input into their internal PES
    /// accumulators waiting for more bytes.
    pub fn process<'a>(&'a mut self, input_ts: &'a [u8]) -> &'a [u8] {
        // Reset scratch buffers without giving up capacity.
        self.scratch_a.clear();
        self.scratch_b.clear();
        self.scratch_c.clear();

        // The PCR stage sees the input's own PES ahead of the replacers
        // (`TsPcrRemux::observe_input`): a PCR step they carry straight on
        // across makes nothing stale.
        self.pcr.observe_input(input_ts);

        // Stage 1: audio. When absent, pass the input through unchanged.
        let after_audio: &[u8] = match self.audio.as_mut() {
            Some(a) => {
                a.process(input_ts, &mut self.scratch_a);
                &self.scratch_a
            }
            None => input_ts,
        };

        // Stage 2: video. When absent, the audio-stage output is final.
        let after_video: &[u8] = match self.video.as_mut() {
            Some(v) => {
                v.process(after_audio, &mut self.scratch_b);
                &self.scratch_b
            }
            None => after_audio,
        };

        // Stage 3: PCR re-stamp — the input's PCR delayed by the measured
        // transcode allowance, checked against the re-encoded PES.
        self.pcr.set_replaced_pids(
            self.audio.as_ref().and_then(TsAudioReplacer::replaced_pid),
            self.video.as_ref().and_then(TsVideoReplacer::replaced_pid),
        );
        if let Some(v) = self.video.as_mut() {
            self.pcr.set_input_pcr_starved(v.input_pcr_starved());
            self.pcr.note_source_holds(v.take_source_holds());
        }
        self.scratch_p.clear();
        self.pcr.process(after_video, &mut self.scratch_p);

        // Stage 4: A/V emission realign (only present when video is
        // transcoded). Holds audio until the video PID's PTS catches up so
        // they leave together; PTS/CC untouched. Absent ⇒ return stage-3 out.
        match self.realign.as_mut() {
            Some(r) => {
                r.process(&self.scratch_p, &mut self.scratch_c);
                &self.scratch_c
            }
            None => &self.scratch_p,
        }
    }

    /// Drain buffered PES / encoder state into `output`. Call on graceful
    /// shutdown. No-op for unset stages.
    #[allow(dead_code)]
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        let mut tail = Vec::new();
        if let Some(a) = self.audio.as_mut() {
            let mut tmp = Vec::new();
            a.flush(&mut tmp);
            // Feed the drained audio bytes through the video stage too.
            if let Some(v) = self.video.as_mut() {
                v.process(&tmp, &mut tail);
            } else {
                tail.extend_from_slice(&tmp);
            }
        }
        if let Some(v) = self.video.as_mut() {
            v.flush(&mut tail);
        }
        self.pcr.process(&tail, output);
        // Emit any audio the realigner is still holding so shutdown loses
        // no samples (ordering relative to the just-flushed tail is moot at
        // teardown — the stream is ending).
        if let Some(r) = self.realign.as_mut() {
            r.flush(output);
        }
    }

    /// Shared handle to the PCR stage's counters (always present).
    pub fn pcr_stats(&self) -> Arc<crate::engine::ts_pcr_remux::PcrRemuxStats> {
        self.pcr.stats_handle()
    }

    /// The PCR stage's own steps in the chunks processed since the last
    /// call (`TsPcrRemux::take_pcr_steps`).
    pub fn take_pcr_steps(&mut self) -> Vec<(u64, i64)> {
        self.pcr.take_pcr_steps()
    }

    /// Returns a shared handle to the video-encode stats counters if a video
    /// stage is active. The input task registers this with the flow
    /// accumulator at startup so the manager dashboard can surface ingress
    /// codec health.
    pub fn video_stats(&self) -> Option<Arc<VideoEncodeStats>> {
        self.video.as_ref().map(|v| v.stats_handle())
    }

    /// Shared handle to the audio replacer's internal decode counters, if
    /// the audio stage is active. Registered via
    /// [`crate::stats::collector::FlowStatsAccumulator::set_input_decode_stats`]
    /// so the input-side `audio_decode` tag pairs with `audio_encode` in the
    /// inputs-live snapshot and the UI renders `Audio Transcode` instead of
    /// the encode-only fallback.
    pub fn audio_decode_stats(
        &self,
    ) -> Option<Arc<crate::engine::audio_decode::DecodeStats>> {
        self.audio.as_ref().map(|a| a.decode_stats_handle())
    }

    /// Shared handle to the audio replacer's internal encode counters, if
    /// the audio stage is active.
    pub fn audio_encode_stats(
        &self,
    ) -> Option<Arc<crate::engine::audio_encode::EncodeStats>> {
        self.audio.as_ref().map(|a| a.encode_stats_handle())
    }

    /// The audio stage's target bitrate in kbps — the configured one, or
    /// the codec's default when `audio_encode.bitrate_kbps` is unset — if
    /// the audio stage is active. What the output side registers too
    /// (`TsAudioReplacer::bitrate_kbps`), so an unset bitrate reads as the
    /// rate actually encoded rather than 0.
    pub fn audio_bitrate_kbps(&self) -> Option<u32> {
        self.audio.as_ref().map(|a| a.bitrate_kbps())
    }

    /// Shared handle to the audio replacer's own counters (source PID and
    /// stream type, the pre-PMT drops and the timeline corrections, silence
    /// inserted and samples dropped), if the audio stage is active.
    pub fn audio_replacer_stats(
        &self,
    ) -> Option<Arc<crate::engine::ts_audio_replace::TsAudioReplacerStats>> {
        self.audio.as_ref().map(|a| a.stats_handle())
    }

    /// Shared handle to the video replacer's internal decode counters, if
    /// the video stage is active.
    pub fn video_decode_stats(
        &self,
    ) -> Option<Arc<crate::engine::video_decode_stats::VideoDecodeStats>> {
        self.video.as_ref().map(|v| v.decode_stats_handle())
    }

    /// Attach the registered input-side `AudioDecodeStatsHandle` to the
    /// audio replacer so it can refresh the source-codec label whenever
    /// the PMT learns a new stream_type. No-op when no audio stage is
    /// active.
    pub fn attach_input_audio_decode_handle(
        &mut self,
        handle: Arc<crate::stats::collector::AudioDecodeStatsHandle>,
    ) {
        if let Some(a) = self.audio.as_mut() {
            a.with_input_decode_handle(handle);
        }
    }

    /// Attach the registered input-side `AudioEncodeStatsHandle` to the
    /// audio replacer, which publishes its resolved output format there
    /// (the configured `sample_rate` / `channels` are 0 when they follow
    /// the source).
    pub fn attach_input_audio_encode_handle(
        &mut self,
        handle: Arc<crate::stats::collector::AudioEncodeStatsHandle>,
    ) {
        if let Some(a) = self.audio.as_mut() {
            a.with_input_encode_handle(handle);
        }
    }

    /// Attach the registered input-side `VideoDecodeStatsHandle` to the
    /// video replacer so it can refresh the source-codec label whenever
    /// the PMT learns a new stream_type. No-op when no video stage is
    /// active.
    pub fn attach_input_video_decode_handle(
        &mut self,
        handle: Arc<crate::stats::collector::VideoDecodeStatsHandle>,
    ) {
        if let Some(v) = self.video.as_mut() {
            v.with_input_decode_handle(handle);
        }
    }

    /// One-shot IDR request handle for the ingress video transcoder, if a
    /// video stage is active. The input forwarder normally passes its
    /// external flag into [`Self::new`] instead; this accessor is for
    /// tests and diagnostic tooling. Returns `None` for passthrough
    /// inputs (no video encoder).
    #[allow(dead_code)]
    pub fn force_idr_handle(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        self.video.as_ref().map(|v| v.force_idr_handle())
    }

    /// Whether the composer has an active audio-encode stage.
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Whether the composer has an active video-encode stage.
    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// Human-readable description of what stages are active, for logging.
    pub fn describe(&self) -> String {
        let a = self
            .audio
            .as_ref()
            .map(|a| format!("audio→{}", a.target_description()))
            .unwrap_or_default();
        let v = self
            .video
            .as_ref()
            .map(|v| format!("video→{}", v.target_description()))
            .unwrap_or_default();
        match (a.is_empty(), v.is_empty()) {
            (false, false) => format!("{a}, {v}"),
            (false, true) => a,
            (true, false) => v,
            (true, true) => "<idle>".to_string(),
        }
    }
}

/// Register an ingress transcoder's stats handles + static summary descriptor
/// with the flow stats accumulator, keyed by `input_id`. Called at flow start
/// by each Group A input right after `InputTranscoder::new` succeeds (and
/// always — even when the transcoder is `None` — so the UI can show the
/// "passthrough" pipeline tag when this input is the active one). Per-input
/// keying means a multi-input flow reports only the active input's pipeline
/// after a seamless switch; the passive inputs' registrations stay in place
/// for later reactivation but never leak into the snapshot.
pub fn register_ingress_stats(
    flow_stats: &crate::stats::collector::FlowStatsAccumulator,
    input_id: &str,
    transcoder: Option<&mut InputTranscoder>,
    audio_encode: Option<&AudioEncodeConfig>,
    video_encode: Option<&VideoEncodeConfig>,
    event_sender: &crate::manager::events::EventSender,
) {
    use crate::stats::collector::EgressMediaSummaryStatic;

    // Edge-added A/V skew: get-or-create this input's reporter and hand
    // it to the replacer stages. Idempotent (DashMap entry per input id),
    // so the InputPostProcess construction in each input module can call
    // `av_skew_reporter_for_input` again for the rewriter side.
    let av_skew = flow_stats.av_skew_reporter_for_input(input_id);

    // Track which stages we registered so the static descriptor below can
    // be set from a single moved-out `transcoder` reference.
    let has_audio_stage_via_transcoder;
    let has_video_stage_via_transcoder;

    if let Some(t) = transcoder {
        has_audio_stage_via_transcoder = t.has_audio();
        has_video_stage_via_transcoder = t.has_video();
        t.set_av_skew_reporter(av_skew.clone());
        flow_stats.set_input_transcode_pcr_stats(input_id, t.pcr_stats());
        flow_stats.set_input_audio_replacer_stats(input_id, t.audio_replacer_stats());
        // Wire the decode-stall watchdog (input-scoped) so a silent ingress
        // video-decode failure raises a `video_transcode_decode_stalled`
        // Warning instead of shipping audio only. No-op without a video stage.
        t.set_decode_stall_watchdog(event_sender.clone(), input_id);

        // ── Audio: register decode + encode handles when an audio stage
        // is active so the manager UI's inputs-live snapshot carries both
        // halves of the transcode and renders `Audio Transcode` instead of
        // the encode-only fallback. The decode-side codec label starts
        // empty and is refreshed by the replacer on the first PMT scan.
        if let (Some(ae), Some(decode_handle_stats), Some(encode_handle_stats)) = (
            audio_encode,
            t.audio_decode_stats(),
            t.audio_encode_stats(),
        ) {
            let target_codec = ae.codec.as_str();
            let target_sr = ae.sample_rate.unwrap_or(0);
            let target_ch = ae.channels.unwrap_or(0);
            // The replacer's resolved bitrate (the codec default when
            // unset), as the output side registers it. `bitrate_kbps` is
            // not published later, so an unset one registered as 0 read
            // "@ 0 kbps" for the life of the input.
            let target_br = t
                .audio_bitrate_kbps()
                .unwrap_or_else(|| ae.bitrate_kbps.unwrap_or(0));
            let audio_handle = flow_stats.set_input_decode_stats(
                input_id,
                decode_handle_stats,
                "",
                0,
                0,
            );
            t.attach_input_audio_decode_handle(audio_handle);
            let encode_handle = flow_stats.set_input_encode_stats(
                input_id,
                encode_handle_stats,
                target_codec.to_string(),
                target_sr,
                target_ch,
                target_br,
            );
            t.attach_input_audio_encode_handle(encode_handle);
        }

        // ── Video: register decode + encode handles when a video stage
        // is active. Encode side has been wired since this function was
        // first written; decode side is the bit that flips the label
        // from `Video Encode` to `Video Transcode`.
        if let (Some(enc), Some(vs)) = (video_encode, t.video_stats()) {
            let backend = match enc.codec.as_str() {
                "x264" | "x265" => enc.codec.clone(),
                "h264_nvenc" | "hevc_nvenc" => "nvenc".to_string(),
                other => other.to_string(),
            };
            let target_codec = match enc.codec.as_str() {
                "x264" | "h264_nvenc" => "h264",
                "x265" | "hevc_nvenc" => "hevc",
                other => other,
            };
            flow_stats.set_input_video_encode_stats(
                input_id,
                vs,
                String::new(),
                target_codec.to_string(),
                enc.width.unwrap_or(0),
                enc.height.unwrap_or(0),
                match (enc.fps_num, enc.fps_den) {
                    (Some(n), Some(d)) if d > 0 => n as f32 / d as f32,
                    _ => 0.0,
                },
                enc.bitrate_kbps.unwrap_or(0),
                backend,
            );

            if let Some(vd_stats) = t.video_decode_stats() {
                let video_handle = flow_stats.set_input_video_decode_stats(
                    input_id,
                    vd_stats,
                    "",
                    0,
                    0,
                    0.0,
                );
                t.attach_input_video_decode_handle(video_handle);
            }
        }
    } else {
        has_audio_stage_via_transcoder = false;
        has_video_stage_via_transcoder = false;
    }

    // Static summary descriptor. Every ingress pipeline ends in raw TS on
    // the flow broadcast channel, hence `transport_mode = "ts"`. Passthrough
    // flags mirror the output-side convention: `true` means the stage leaves
    // that essence untouched.
    let has_audio_stage = audio_encode.is_some() || has_audio_stage_via_transcoder;
    let has_video_stage = video_encode.is_some() || has_video_stage_via_transcoder;
    flow_stats.set_ingress_static(
        input_id,
        EgressMediaSummaryStatic {
            transport_mode: Some("ts".to_string()),
            video_passthrough: !has_video_stage,
            audio_passthrough: !has_audio_stage,
            audio_only: false,
            ..Default::default()
        },
    );
}

/// Parse the length of an RTP header including any CSRC list and extension
/// header. Returns `None` if the input is too short or malformed. Does not
/// validate the packet is RTP — callers must check that separately (e.g. via
/// [`crate::util::rtp_parse::is_likely_rtp`]).
pub(crate) fn rtp_header_length(data: &[u8]) -> Option<usize> {
    if data.len() < 12 {
        return None;
    }
    let cc = (data[0] & 0x0F) as usize;
    let has_ext = (data[0] & 0x10) != 0;
    let mut len = 12 + cc * 4;
    if has_ext {
        if data.len() < len + 4 {
            return None;
        }
        let ext_len = u16::from_be_bytes([data[len + 2], data[len + 3]]) as usize;
        len += 4 + ext_len * 4;
    }
    if data.len() < len {
        return None;
    }
    Some(len)
}

/// Helper that every Group A input uses at the point of publishing a freshly
/// received packet onto the flow's broadcast channel. When `transcoder` is
/// `None` the packet is sent verbatim; otherwise the TS payload (after
/// stripping an optional RTP header) is run through the transcoder inside
/// `tokio::task::block_in_place` and republished as raw TS.
///
/// Returns `true` when something was sent, `false` when the packet was
/// swallowed (e.g. the replacer is still bootstrapping its PMT discovery).
/// Legacy two-arg variant kept for any external caller that hasn't
/// migrated to `publish_input_packet_with_post`. Internal callers now
/// all use the `_with_post` form so this is currently unused — left in
/// place so an out-of-tree caller doesn't break, and so removing it is
/// a deliberate API change rather than an accidental refactor.
#[allow(dead_code)]
pub fn publish_input_packet(
    transcoder: &mut Option<InputTranscoder>,
    broadcast_tx: &tokio::sync::broadcast::Sender<super::packet::RtpPacket>,
    packet: super::packet::RtpPacket,
) -> bool {
    publish_input_packet_with_post(transcoder, &mut None, broadcast_tx, packet)
}

/// Variant of [`publish_input_packet`] that accepts an optional
/// [`super::input_post_process::InputPostProcess`] applied after the
/// transcoder (or as the only stage on passthrough flows). Use this on
/// inputs that surface `program_number` / `pid_overrides` / `pid_map`
/// to operators.
pub fn publish_input_packet_with_post(
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<super::input_post_process::InputPostProcess>,
    broadcast_tx: &tokio::sync::broadcast::Sender<super::packet::RtpPacket>,
    packet: super::packet::RtpPacket,
) -> bool {
    match process_input_packet_with_post(transcoder, post, packet) {
        Some(p) => broadcast_tx.send(p).is_ok(),
        None => false,
    }
}

/// Same as [`publish_input_packet_with_post`] but publishes through an
/// [`super::ingress_publisher::IngressPublisher`] instead of a raw
/// `broadcast::Sender`. When the publisher is configured with
/// `ingress_delay_ms > 0`, packets are queued in a per-input
/// FIFO and released at their scheduled time; otherwise the publisher
/// forwards immediately (zero-overhead pass-through).
///
/// Returns `true` when the packet reached the publisher (either
/// directly or via the smoother's mpsc); `false` when the
/// transcode+post pipeline filtered it out or the smoother's queue
/// was full.
pub fn publish_input_packet_smoothed(
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<super::input_post_process::InputPostProcess>,
    publisher: &super::ingress_publisher::IngressPublisher,
    packet: super::packet::RtpPacket,
) -> bool {
    match process_input_packet_with_post(transcoder, post, packet) {
        Some(p) => publisher.send(p),
        None => false,
    }
}

/// Process a packet through optional transcode + post-process stages and
/// return the resulting packet, without performing the final broadcast
/// send. Splits [`publish_input_packet_with_post`] in two so a caller can
/// take ownership of the egress timing — used by the media-player input
/// to do the broadcast send from a dedicated SCHED_FIFO pacer thread
/// where `tokio::task::block_in_place` is not available.
///
/// Returns `None` when the stages consumed the input without producing
/// output (e.g. transcoder bootstrapping its PMT discovery, or the post-
/// processor filtering this packet away). Returns `Some(pkt)` ready for
/// broadcast.
pub fn process_input_packet_with_post(
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<super::input_post_process::InputPostProcess>,
    packet: super::packet::RtpPacket,
) -> Option<super::packet::RtpPacket> {
    use bytes::Bytes;

    // Fast path: nothing to do, hand the packet back verbatim.
    if transcoder.is_none() && post.is_none() {
        return Some(packet);
    }

    // Locate the TS bytes. For `is_raw_ts: true` (or SRT's `Unknown` fallback)
    // the whole payload is TS. For RTP-wrapped TS, strip the RTP header.
    let ts_offset = if packet.is_raw_ts {
        0
    } else {
        match rtp_header_length(&packet.data) {
            Some(n) => n,
            // Malformed header — hand the packet back unchanged so the
            // output side (or the next packet) can recover.
            None => return Some(packet),
        }
    };
    let ts_in = &packet.data[ts_offset..];

    let out: Vec<u8> = tokio::task::block_in_place(|| {
        let after_transcode_owned: Option<Vec<u8>> = transcoder.as_mut().map(|t| {
            t.process(ts_in).to_vec()
        });
        // The transcode's own PCR steps go to the muxer-mode rewriter with
        // the chunk that carries them: it passes them through as the PCR
        // steps they are, not as source discontinuities.
        let pcr_steps = transcoder.as_mut().map(InputTranscoder::take_pcr_steps).unwrap_or_default();
        let after_transcode: &[u8] = match after_transcode_owned {
            Some(ref v) => v.as_slice(),
            None => ts_in,
        };
        if after_transcode.is_empty() {
            return Vec::new();
        }
        match post.as_mut() {
            Some(p) => {
                p.note_upstream_pcr_steps(pcr_steps);
                p.process(after_transcode).to_vec()
            }
            None => after_transcode.to_vec(),
        }
    });
    if out.is_empty() {
        return None;
    }

    Some(super::packet::RtpPacket {
        data: Bytes::from(out),
        sequence_number: packet.sequence_number,
        rtp_timestamp: packet.rtp_timestamp,
        recv_time_us: packet.recv_time_us,
        // After the input transcode / post-process stages the flow always
        // carries raw TS.
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_returns_none() {
        let t = InputTranscoder::new(None, None, None, None).expect("construct");
        assert!(t.is_none(), "all-None config must be idle (no stage)");
    }

    #[test]
    fn transcode_without_audio_encode_is_none() {
        // A transcode block without audio_encode has no encoder to feed — the
        // composer treats it as idle (matches the output-side contract where
        // transcode is a sub-block of audio_encode).
        let tj = TranscodeJson {
            sample_rate: Some(48_000),
            ..Default::default()
        };
        let t = InputTranscoder::new(None, Some(&tj), None, None).expect("construct");
        assert!(t.is_none());
    }

    #[test]
    fn audio_only_stage_passes_non_ts_through() {
        let ae = AudioEncodeConfig {
            codec: "aac_lc".to_string(),
            bitrate_kbps: Some(128),
            sample_rate: None,
            channels: None,
            silent_fallback: false,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
             source_audio_pid: None,
             ts_signalling: None,
        };
        let mut t = InputTranscoder::new(Some(&ae), None, None, None)
            .expect("construct")
            .expect("stage");
        // Non-aligned input is passed through verbatim by TsAudioReplacer.
        let input = b"not a ts packet";
        let out = t.process(input);
        assert_eq!(out, input);
    }

    /// An ingress `audio_encode` with no `bitrate_kbps` registers the
    /// bitrate it encodes at — the codec's default — as the output side
    /// does, not 0: the target shape is published later but the bitrate
    /// never is, so a 0 read "@ 0 kbps" for the life of the input.
    #[test]
    fn an_unset_ingress_audio_bitrate_registers_the_codec_default() {
        let ae = AudioEncodeConfig {
            codec: "aac_lc".to_string(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
            silent_fallback: false,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
            source_audio_pid: None,
            ts_signalling: None,
        };
        let mut t = InputTranscoder::new(Some(&ae), None, None, None)
            .expect("construct")
            .expect("stage");
        assert_eq!(t.audio_bitrate_kbps(), Some(128));
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f1".to_string(),
            "flow-1".to_string(),
            "srt".to_string(),
        );
        flow.set_active_input_id("in-1");
        let (tx, _rx) = crate::manager::events::event_channel();
        register_ingress_stats(&flow, "in-1", Some(&mut t), Some(&ae), None, &tx);
        let e = flow.snapshot().input.audio_encode_stats.expect("an ingress audio encode");
        assert_eq!(e.target_bitrate_kbps, 128);
        // Rate and channels still follow the source until the first frame.
        assert_eq!((e.target_sample_rate_hz, e.target_channels), (0, 0));

        // A configured bitrate is registered as configured.
        let ae = AudioEncodeConfig { bitrate_kbps: Some(96), ..ae };
        let mut t = InputTranscoder::new(Some(&ae), None, None, None).unwrap().unwrap();
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f2".to_string(),
            "flow-2".to_string(),
            "srt".to_string(),
        );
        flow.set_active_input_id("in-2");
        register_ingress_stats(&flow, "in-2", Some(&mut t), Some(&ae), None, &tx);
        assert_eq!(flow.snapshot().input.audio_encode_stats.unwrap().target_bitrate_kbps, 96);
    }

    /// End-to-end: `audio_encode: mp2` + `pid_overrides` keyed on a
    /// non-1 program number. The transcoder runs first (the audio
    /// replacer rewrites the PMT's audio stream_type from 0x0F AAC →
    /// 0x03 MP2), then the input post-process rewriter renames the PMT
    /// PID + audio PID. The final PMT must carry **both** changes: the
    /// MP2 stream_type from the replacer AND the target PIDs from the
    /// rewriter.
    ///
    /// This is the contract the bug-fix relies on — the two stages
    /// work on disjoint PMT bytes (replacer touches stream_type, rewriter
    /// touches the PID slots), so they compose without overwriting each
    /// other.
    #[test]
    fn audio_encode_plus_pid_overrides_preserves_both_codec_and_pid() {
        use crate::config::models::{TsPidOverridesEntry, TsPidOverridesMap};
        use crate::engine::input_post_process::{
            InputPostProcess, InputPostProcessConfig,
        };
        use crate::engine::ts_parse::{
            mpeg2_crc32, ts_pid, TS_PACKET_SIZE, TS_SYNC_BYTE,
        };

        // Same shape as testbed/configs/edge1/config.json media-player
        // "Ten.ts" entry: audio_encode = mp2, pid_overrides keyed on
        // program 1591 → pmt_pid 4096, video_pid 256, audio_pid 257.
        let ae = AudioEncodeConfig {
            codec: "mp2".to_string(),
            bitrate_kbps: Some(192),
            sample_rate: None,
            channels: None,
            silent_fallback: false,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
            source_audio_pid: None,
            ts_signalling: None,
        };
        let mut overrides = TsPidOverridesMap::new();
        overrides.insert(
            1591,
            TsPidOverridesEntry {
                pmt_pid: Some(4096),
                video_pid: Some(256),
                audio_pid: Some(257),
                audio_pids: None,
                pcr_pid: None,
            },
        );

        let mut transcoder = InputTranscoder::new(Some(&ae), None, None, None)
            .expect("transcoder construct")
            .expect("audio stage active");
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: None,
            pid_overrides: Some(&overrides),
            pid_map: None,
            passthrough_clock: false,
            av_sync_pacer: None,
            av_skew: None,
        })
        .expect("rewriter active");

        // Build PAT(program 1591 → PMT pid 0x100) + PMT(video 0x101
        // H.264, audio 0x102 AAC). The audio replacer's PMT rewrite
        // walks this packet, finds the audio entry by source PID, and
        // changes its stream_type byte from 0x0F → 0x03. The rewriter
        // then walks the same PMT and renames the PMT TS-header PID +
        // body's video / audio PID slots.

        // PAT.
        let mut pat = [0xFFu8; TS_PACKET_SIZE];
        pat[0] = TS_SYNC_BYTE;
        pat[1] = 0x40; // PUSI=1, PID=0
        pat[2] = 0x00;
        pat[3] = 0x10;
        pat[4] = 0x00; // pointer_field
        let section_length = 5 + 4 + 4; // 13
        pat[5] = 0x00;
        pat[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pat[7] = (section_length & 0xFF) as u8;
        pat[8] = 0x00;
        pat[9] = 0x01;
        pat[10] = 0xC1;
        pat[11] = 0x00;
        pat[12] = 0x00;
        // Program entry: program_number=1591, pmt_pid=0x100.
        pat[13] = (1591u16 >> 8) as u8;
        pat[14] = (1591u16 & 0xFF) as u8;
        pat[15] = 0xE0 | (((0x100u16 >> 8) as u8) & 0x1F);
        pat[16] = 0x00;
        let crc = mpeg2_crc32(&pat[5..17]);
        pat[17] = (crc >> 24) as u8;
        pat[18] = (crc >> 16) as u8;
        pat[19] = (crc >> 8) as u8;
        pat[20] = crc as u8;

        // PMT (on PID 0x100), with H.264 video at 0x101 + AAC audio at 0x102.
        let mut pmt = [0xFFu8; TS_PACKET_SIZE];
        pmt[0] = TS_SYNC_BYTE;
        pmt[1] = 0x40 | (((0x100u16 >> 8) as u8) & 0x1F);
        pmt[2] = 0x00;
        pmt[3] = 0x10;
        pmt[4] = 0x00;
        // table_id PMT + section header (9 bytes header + 2*5 ES + 4 CRC) = 23
        let pmt_section_length: u16 = 9 + 5 + 5 + 4;
        pmt[5] = 0x02;
        pmt[6] = 0xB0 | (((pmt_section_length >> 8) & 0x0F) as u8);
        pmt[7] = (pmt_section_length & 0xFF) as u8;
        pmt[8] = 0x00;
        pmt[9] = 0x01;
        pmt[10] = 0xC1;
        pmt[11] = 0x00;
        pmt[12] = 0x00;
        // PCR_PID = 0x101 (video PID).
        pmt[13] = 0xE0 | (((0x101u16 >> 8) as u8) & 0x1F);
        pmt[14] = 0x01;
        pmt[15] = 0xF0;
        pmt[16] = 0x00;
        // Video ES: stream_type=0x1B (H.264), es_pid=0x101.
        pmt[17] = 0x1B;
        pmt[18] = 0xE0 | (((0x101u16 >> 8) as u8) & 0x1F);
        pmt[19] = 0x01;
        pmt[20] = 0xF0;
        pmt[21] = 0x00;
        // Audio ES: stream_type=0x0F (AAC ADTS), es_pid=0x102.
        pmt[22] = 0x0F;
        pmt[23] = 0xE0 | (((0x102u16 >> 8) as u8) & 0x1F);
        pmt[24] = 0x02;
        pmt[25] = 0xF0;
        pmt[26] = 0x00;
        let pmt_crc = mpeg2_crc32(&pmt[5..(5 + 3 + pmt_section_length as usize - 4)]);
        let crc_off = 5 + 3 + pmt_section_length as usize - 4;
        pmt[crc_off] = (pmt_crc >> 24) as u8;
        pmt[crc_off + 1] = (pmt_crc >> 16) as u8;
        pmt[crc_off + 2] = (pmt_crc >> 8) as u8;
        pmt[crc_off + 3] = pmt_crc as u8;

        // Run PAT → transcoder → post.
        let after_transcoder_pat = transcoder.process(&pat).to_vec();
        // The audio replacer is a passthrough for PAT (it doesn't touch
        // the PAT). The post rewriter rewrites it.
        let rewritten_pat = post.process(&after_transcoder_pat).to_vec();
        assert_eq!(
            rewritten_pat.len(),
            TS_PACKET_SIZE,
            "PAT must be one packet"
        );
        // Program 1591's pmt_pid in the rewritten PAT must be 4096 (the
        // override target).
        let pat_entry_pmt_pid =
            (((rewritten_pat[15] & 0x1F) as u16) << 8) | (rewritten_pat[16] as u16);
        assert_eq!(
            pat_entry_pmt_pid, 4096,
            "rewritten PAT must advertise program 1591 → PMT PID 4096"
        );

        // Run PMT → transcoder → post.
        let after_transcoder_pmt = transcoder.process(&pmt).to_vec();
        // The audio replacer must have rewritten the audio entry's
        // stream_type from 0x0F (AAC) → 0x03 (MP2). The byte sits at
        // ES_info offset 22 in the original packet — but the post-
        // process hasn't run yet, so the audio entry's PID is still
        // the source 0x102.
        let mid_audio_stream_type = after_transcoder_pmt[22];
        assert_eq!(
            mid_audio_stream_type, 0x03,
            "audio replacer must rewrite PMT audio stream_type AAC (0x0F) → MP2 (0x03), got 0x{mid_audio_stream_type:02X}"
        );

        let rewritten_pmt = post.process(&after_transcoder_pmt).to_vec();
        assert_eq!(
            rewritten_pmt.len(),
            TS_PACKET_SIZE,
            "PMT must be one packet"
        );
        // PMT TS-header PID must be 4096 (override).
        assert_eq!(
            ts_pid(&rewritten_pmt[..TS_PACKET_SIZE]),
            4096,
            "rewritten PMT TS-header PID must be 4096"
        );
        // Audio entry: stream_type still 0x03 (MP2) — preserved
        // through the rewriter — AND es_pid 257 (override target).
        let out_audio_st = rewritten_pmt[22];
        let out_audio_pid =
            (((rewritten_pmt[23] & 0x1F) as u16) << 8) | (rewritten_pmt[24] as u16);
        assert_eq!(
            out_audio_st, 0x03,
            "audio stream_type must still be MP2 (0x03) after rewriter; got 0x{out_audio_st:02X} — the rewriter overwrote the replacer's codec change"
        );
        assert_eq!(
            out_audio_pid, 257,
            "rewritten PMT audio entry must carry target PID 257 (got 0x{out_audio_pid:04X})"
        );
        // Video entry: stream_type still 0x1B (H.264 passthrough), pid 256.
        let out_video_st = rewritten_pmt[17];
        let out_video_pid =
            (((rewritten_pmt[18] & 0x1F) as u16) << 8) | (rewritten_pmt[19] as u16);
        assert_eq!(out_video_st, 0x1B, "video stream_type must passthrough");
        assert_eq!(out_video_pid, 256, "video PID must be renamed to 256");
    }

    /// Audio-only transcode (video and its PCR pass through). The replacer
    /// used to emit PES k only when PES k+1 arrived, so against an unshifted
    /// PCR its first frame was late — measured on Sky, 3930 of 4016 PES —
    /// and the trailing PCR stage had to delay the PCR by that lateness. It
    /// now re-encodes each AU as soon as it is complete, so the PES keep
    /// their source lead and the PCR stage never needs more than its
    /// initial 80 ms: no re-encoded PES is ever behind the PCR, and the PCR
    /// keeps the input's positions and cadence.
    #[test]
    fn audio_only_transcode_keeps_every_re_encoded_pes_ahead_of_the_pcr() {
        use crate::engine::ts_parse::{
            extract_pcr, extract_pes_pts, pcr_only_packet, ts_cc, ts_has_payload, ts_pid, ts_pusi,
        };
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let mut frames = Vec::new();
        let mut off = 0usize;
        while off + 7 <= ADTS.len() {
            let len = (((ADTS[off + 3] as usize) & 0x03) << 11)
                | ((ADTS[off + 4] as usize) << 3)
                | ((ADTS[off + 5] as usize) >> 5);
            if len == 0 || off + len > ADTS.len() {
                break;
            }
            frames.push(&ADTS[off..off + len]);
            off += len;
        }
        let ae = AudioEncodeConfig {
            codec: "mp2".to_string(),
            bitrate_kbps: Some(192),
            sample_rate: None,
            channels: None,
            silent_fallback: false,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
            source_audio_pid: None,
            ts_signalling: None,
        };
        let mut t = InputTranscoder::new(Some(&ae), None, None, None).unwrap().unwrap();

        const MS: u64 = 27_000;
        let t0 = 100 * 27_000_000u64;
        // Events in stream order: (time, packets).
        let mut events: Vec<(u64, Vec<[u8; 188]>)> = Vec::new();
        let mut psi = vec![pat_packet(&[(1, 0x1000)], 0, 0)];
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
        psi.extend(packetize_sections(0x1000, &[&pmt], 0));
        events.push((t0, psi));
        let mut pcrs_in = Vec::new();
        for k in 0..100u64 {
            let at = t0 + 1 + k * 30 * MS;
            pcrs_in.push(at);
            events.push((at, vec![pcr_only_packet(0x100, 0, at, false)]));
        }
        // 7 AAC frames per PES (149.3 ms), each PES 100 ms ahead of the PCR:
        // emitted when the next PES arrived, the first frame was ~50 ms late
        // against an unshifted PCR.
        let pes_27 = 7 * 1024 * 27_000_000u64 / 48_000;
        let mut acc = 0u8;
        for j in 0..19u64 {
            let at = t0 + 2 + j * pes_27;
            let mut es = Vec::new();
            for i in 0..7 {
                es.extend_from_slice(frames[(j as usize * 7 + i) % frames.len()]);
            }
            let pes = crate::engine::ts_audio_replace::test_build_audio_pes(&es, (at + 100 * MS) / 300);
            events.push((at, crate::engine::ts_audio_replace::test_packetize(0x101, &pes, &mut acc)));
        }
        events.sort_by_key(|(at, _)| *at);
        let input: Vec<u8> = events
            .iter()
            .flat_map(|(_, p)| p.iter().flat_map(|q| q.iter().copied()))
            .collect();
        let mut out = Vec::new();
        for chunk in input.chunks(7 * 188) {
            out.extend_from_slice(t.process(chunk));
        }

        let d = t.pcr_stats().offset_27mhz.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(d, 80 * MS, "the audio needs no more than the initial 80 ms: {} ms", d / MS);
        let mut last_pcr = None;
        let mut pcrs_out = Vec::new();
        let mut audio_pes = 0;
        let mut last_audio_cc: Option<u8> = None;
        for p in out.chunks(188) {
            if ts_pid(p) == 0x100
                && let Some(v) = extract_pcr(p)
            {
                last_pcr = Some(v);
                pcrs_out.push(v);
            }
            if ts_pid(p) == 0x101 && ts_has_payload(p) {
                if let Some(prev) = last_audio_cc {
                    assert_eq!(ts_cc(p), (prev + 1) & 0x0F, "audio CC continuous");
                }
                last_audio_cc = Some(ts_cc(p));
                if ts_pusi(p) {
                    let pts27 = extract_pes_pts(p).unwrap() * 300;
                    let pcr = last_pcr.expect("a PCR precedes the first re-encoded PES");
                    assert!(
                        pts27 > pcr,
                        "re-encoded PES {audio_pes} is {} ms behind the PCR",
                        (pcr - pts27) / MS
                    );
                    audio_pes += 1;
                }
            }
        }
        assert!(audio_pes >= 80, "re-encoded audio emitted: {audio_pes} PES");
        assert_eq!(pcrs_out.len(), pcrs_in.len(), "one output PCR per input PCR");
        // After the latch every PCR is the input's minus D.
        for (o, i) in pcrs_out.iter().zip(&pcrs_in).skip(20) {
            assert_eq!(*o, i - d);
        }
    }

    /// A source that restarts with its PCR and PTS reset 60 s back (an
    /// encoder restart, an ffmpeg `-stream_loop` wrap): the re-encoded
    /// audio follows it onto the new timeline. Held on the old one, the PCR
    /// stage took every re-encoded PES after the reset for a frame of the
    /// previous epoch and dropped it, for as long as the reset was deep
    /// (a loop's length, for good on a looping source).
    #[test]
    fn re_encoded_audio_follows_a_backward_source_reset() {
        use crate::engine::ts_parse::{extract_pcr, extract_pes_pts, pcr_only_packet, ts_pid, ts_pusi};
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let mut frames = Vec::new();
        let mut off = 0usize;
        while off + 7 <= ADTS.len() {
            let len = (((ADTS[off + 3] as usize) & 0x03) << 11)
                | ((ADTS[off + 4] as usize) << 3)
                | ((ADTS[off + 5] as usize) >> 5);
            if len == 0 || off + len > ADTS.len() {
                break;
            }
            frames.push(&ADTS[off..off + len]);
            off += len;
        }
        let ae = AudioEncodeConfig {
            codec: "mp2".to_string(),
            bitrate_kbps: Some(192),
            sample_rate: None,
            channels: None,
            silent_fallback: false,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
            source_audio_pid: None,
            ts_signalling: None,
        };
        let mut t = InputTranscoder::new(Some(&ae), None, None, None).unwrap().unwrap();

        const MS: u64 = 27_000;
        const RESET: u64 = 60_000 * MS;
        let t0 = 100 * 27_000_000u64;
        let pes_27 = 7 * 1024 * 27_000_000u64 / 48_000;
        // From this stream time on, the source's clock reads 60 s less.
        let t_reset = t0 + 10 * pes_27;
        let clock = |at: u64| if at >= t_reset { at - RESET } else { at };
        let mut events: Vec<(u64, Vec<[u8; 188]>)> = Vec::new();
        let mut psi = vec![pat_packet(&[(1, 0x1000)], 0, 0)];
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
        psi.extend(packetize_sections(0x1000, &[&pmt], 0));
        events.push((t0, psi));
        for k in 0..100u64 {
            let at = t0 + 1 + k * 30 * MS;
            events.push((at, vec![pcr_only_packet(0x100, 0, clock(at), false)]));
        }
        let mut acc = 0u8;
        for j in 0..19u64 {
            let at = t0 + 2 + j * pes_27;
            let mut es = Vec::new();
            for i in 0..7 {
                es.extend_from_slice(frames[(j as usize * 7 + i) % frames.len()]);
            }
            let pts = (clock(at) + 100 * MS) / 300;
            let pes = crate::engine::ts_audio_replace::test_build_audio_pes(&es, pts);
            events.push((at, crate::engine::ts_audio_replace::test_packetize(0x101, &pes, &mut acc)));
        }
        events.sort_by_key(|(at, _)| *at);
        let input: Vec<u8> = events
            .iter()
            .flat_map(|(_, p)| p.iter().flat_map(|q| q.iter().copied()))
            .collect();
        let mut out = Vec::new();
        for chunk in input.chunks(7 * 188) {
            out.extend_from_slice(t.process(chunk));
        }

        let stale = t.pcr_stats().stale_frames_dropped.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(stale, 0, "no re-encoded PES taken for the previous epoch's");
        // Every re-encoded PES after the reset's first PCR is on the new
        // timeline: its PTS within a second of the PCR ahead of it.
        let (mut last_pcr, mut after_reset) = (None, 0);
        for p in out.chunks(188) {
            if ts_pid(p) == 0x100
                && let Some(v) = extract_pcr(p)
            {
                last_pcr = Some(v);
            }
            if ts_pid(p) == 0x101 && ts_pusi(p) {
                let pcr = last_pcr.expect("a PCR first");
                if pcr < t0 {
                    let pts27 = extract_pes_pts(p).unwrap() * 300;
                    assert!(
                        pts27.abs_diff(pcr) < 1_000 * MS,
                        "PES {} ms off the PCR",
                        pts27.abs_diff(pcr) / MS
                    );
                    after_reset += 1;
                }
            }
        }
        // Nine PES of seven AAC frames after the reset: 56 MP2 frames.
        assert!(after_reset >= 50, "re-encoded audio after the reset: {after_reset} PES");
    }
}
