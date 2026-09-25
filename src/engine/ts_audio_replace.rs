// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Streaming MPEG-TS audio elementary-stream replacement.
//!
//! [`TsAudioReplacer`] consumes raw TS packets (188-byte aligned), passes
//! video / PAT / null / other elementary streams through unchanged, and
//! transparently rewrites the audio ES:
//!
//! 1. Every PMT-PID packet goes through the reassembling
//!    `ts_pmt_edit::PsiUnitStage`. From the program's PMT section (found
//!    by program_number, wherever it sits in the unit — VH1 carries a 0xC0
//!    table ahead of it) the replacer learns the audio PID and its source
//!    stream_type, and rebuilds that section: target stream_type, the
//!    target's descriptor policy, a content-tracked version and a valid
//!    CRC, also when the PMT spans packets. See `ts_pmt_edit`.
//! 2. On each audio TS packet, bytes are buffered into a PES. When a new
//!    PES begins (PUSI), the previous PES is decoded (AAC-LC ADTS → PCM),
//!    fed to the target encoder, and the resulting encoded audio frames
//!    are re-packetized into new audio TS packets with the same audio PID.
//! 3. Raw audio TS packets are dropped from the output — they are replaced
//!    by the re-encoded equivalents.
//!
//! This is the streaming variant of the HLS segment-level remuxer in
//! `output_hls.rs`. State is kept across chunks (PES buffer, decoder,
//! encoder, PCM accumulator, PMT identity) so every call to [`process`] is
//! incremental. [`flush`] drains any trailing PES / encoder buffer on
//! shutdown.
//!
//! The replacer is fully synchronous — output tasks that want to use it
//! inside an async context should call it from `tokio::task::block_in_place`
//! or delegate to a dedicated worker thread. AAC / codec operations take
//! single-digit milliseconds per frame and must not run inline on a
//! single-threaded runtime.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use std::sync::Arc;

/// Lock-free per-instance counters surfaced to the manager via the
/// output stats snapshot path.
///
/// Mirrors the shape of `engine::ts_video_replace::VideoEncodeStats` so
/// the manager UI can render an audio-side "transcoding from PID X"
/// badge with the same plumbing as the video badge. Today it carries
/// just the source PID + stream_type — the heavy encode-frame counters
/// remain inside `engine::audio_encode::EncodeStats` on the subprocess
/// path. Extend this struct when adding new audio-replacer telemetry.
#[derive(Debug, Default)]
pub struct TsAudioReplacerStats {
    /// Source audio PID the replacer is currently locked onto
    /// (PMT-discovered or operator-pinned via
    /// `audio_encode.source_audio_pid`). `0` = PMT not yet observed.
    pub source_pid: AtomicU16,
    /// Source audio stream_type byte (`0x0F` AAC, `0x03/0x04` MPEG-1/2,
    /// `0x81` AC-3, `0x06` private). `0` = unknown.
    pub source_stream_type: AtomicU8,
    /// Non-PSI packets dropped before the program's PMT was parsed.
    pub pre_pmt_dropped_packets: std::sync::atomic::AtomicU64,
}

use crate::config::models::AudioEncodeConfig;

use super::audio_encode::AudioCodec;
use super::audio_transcode::{PlanarAudioTranscoder, TranscodeJson};
use super::transcode_engage::{
    EngageEvent, PassthroughCc, PrePmtGate, TranscodeEngageWatch, TranscodeKind,
};
use super::ts_parse::{
    extract_pcr, parse_pat_programs, pcr_only_packet, ts_discontinuity_indicator, ts_has_payload,
    ts_payload_offset, ts_pid, ts_pusi, PAT_PID, TS_PACKET_SIZE, TS_SYNC_BYTE,
};
use super::ts_pmt_edit::{
    detect_flavour, parse_pmt, pmt_index, rebuild_pmt_section_fitting, AudioTarget, EsEdit, OutVersion,
    PmtEdit, PmtView, PsiUnit, PsiUnitStage, TsFlavour,
};

// ────────────────────────── Public surface ──────────────────────────

/// Errors raised when constructing a [`TsAudioReplacer`].
#[derive(Debug)]
#[allow(dead_code)]
pub enum TsAudioReplaceError {
    /// Codec name not recognised.
    UnknownCodec(String),
    /// Codec cannot be carried inside MPEG-TS (e.g. Opus has no standard
    /// TS mapping on the RTP/UDP/SRT outputs we target).
    UnsupportedCodec(String),
    /// Build compiled without the feature required for this codec.
    MissingFeature(&'static str),
}

impl std::fmt::Display for TsAudioReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCodec(c) => write!(f, "unknown codec '{c}'"),
            Self::UnsupportedCodec(c) => write!(f, "codec '{c}' is not supported in MPEG-TS"),
            Self::MissingFeature(feat) => {
                write!(f, "this build was compiled without the '{feat}' feature")
            }
        }
    }
}

impl std::error::Error for TsAudioReplaceError {}

/// MPEG-TS audio elementary-stream replacer.
///
/// Not `Sync` (the in-process codecs hold raw C state), but `Send` so the
/// whole instance can be moved to a blocking worker.
pub struct TsAudioReplacer {
    /// Target codec for the output audio ES.
    codec: AudioCodec,
    /// Resolved output bitrate in kbps.
    bitrate_kbps: u32,
    /// Optional override of the output sample rate. `None` = use source.
    sample_rate_override: Option<u32>,
    /// Optional override of the output channel count. `None` = use source.
    channels_override: Option<u8>,

    /// Discovered PMT PID (the PAT's first program's PMT). `None` before
    /// the PAT has been seen.
    pmt_pid: Option<u16>,
    /// program_number of that program. The PMT section is matched on it,
    /// so a PMT PID shared by several programs, or carrying other tables
    /// ahead of the PMT (VH1's 0xC0 section), resolves to the right one.
    program_number: Option<u16>,
    /// The PAT maps another program to the same PMT PID (then only an
    /// exact program_number match counts — see `ts_pmt_edit::pmt_index`).
    pmt_pid_shared: bool,
    /// Reassembling PMT-PID stage: single-packet, multi-section and
    /// multi-packet PMTs all reach [`Self::handle_pmt_unit`] complete, and
    /// an edited PMT is re-packetised with a valid CRC.
    pmt_stage: PsiUnitStage,
    /// Pre-PMT gate: PSI only until the program's PMT parses (see
    /// [`Self::process`]).
    gate: PrePmtGate,
    /// Last passthrough CC per PID (seeds `out_audio_cc` on takeover).
    passthrough_cc: PassthroughCc,
    /// Discovered audio PID. `None` before the PMT has been parsed. The
    /// replacer drops the original audio TS packets on this PID and
    /// emits re-encoded packets on the same PID; any operator PID rename
    /// is applied downstream by `TsPidOverridesRewriter`.
    audio_pid: Option<u16>,
    /// Optional operator-pinned source audio PID. When set, the replacer
    /// locks onto this PID specifically (useful on MPTS programs with
    /// multiple audio tracks). When unset, falls back to first-matching-
    /// codec discovery. Sourced from `audio_encode.source_audio_pid`.
    source_audio_pid_pin: Option<u16>,
    /// De-duplication key for the `audio_source_pid_not_found` warning:
    /// `Some((pinned_pid, actual_pid))` once we've warned about that
    /// specific mismatch, so we don't spam logs every PMT version bump.
    /// Cleared back to `None` once the pinned PID is found again.
    last_pinned_warn: Option<(u16, u16)>,
    /// Shared lock-free counters surfaced to the manager. The
    /// `OutputStatsAccumulator` holds an `Arc` clone via
    /// `set_audio_replacer_stats`, and the snapshot path reads
    /// `source_pid` + `source_stream_type` from here every tick.
    stats: Arc<TsAudioReplacerStats>,
    /// Lock-free counters for the internal decode stage. Bumped per AAC /
    /// MP2 / AC-3 / E-AC-3 access unit fed to the decoder and per
    /// successfully decoded PCM frame. Surfaced to the manager via
    /// [`crate::stats::collector::OutputStatsAccumulator::set_decode_stats`]
    /// so the egress pipeline emits the `audio_decode` tag — the UI then
    /// collapses `audio_decode + audio_encode` into a single
    /// "Audio Transcode" badge, distinguishing this leg from an
    /// encode-only ST 2110 ingress.
    decode_stats: Arc<crate::engine::audio_decode::DecodeStats>,
    /// Lock-free counters for the internal encode stage. Bumped per PCM
    /// frame submitted to the encoder and per encoded frame produced.
    /// Surfaced to the flow-stats accumulator via
    /// [`crate::stats::collector::FlowStatsAccumulator::set_input_encode_stats`]
    /// so the manager UI's input-side processing block can render
    /// `Audio Transcode` (paired with `decode_stats`) instead of
    /// `Audio Encode` for compressed-source TS ingest.
    encode_stats: Arc<crate::engine::audio_encode::EncodeStats>,
    /// Optional handle on the owning output's stats accumulator, used to
    /// refresh the live `audio_decode_stats` source-codec / output PCM
    /// shape whenever the source codec is rediscovered on a PMT update
    /// (input switch, MPTS program change). `None` on outputs that do
    /// not call [`Self::with_output_stats`] at spawn time — the
    /// `audio_decode` tag still surfaces (the handle is registered up
    /// front), the codec label just stays at its placeholder.
    output_stats: Option<Arc<crate::stats::collector::OutputStatsAccumulator>>,
    /// Optional handle on the input-side `audio_decode_stats` entry the
    /// replacer should refresh whenever the source codec is rediscovered
    /// on a PMT update. Set by ingress-side callers via
    /// [`Self::with_input_decode_handle`] after registering the handle
    /// with the flow stats accumulator. `None` on output-side replacers,
    /// which refresh via `output_stats` instead.
    input_decode_handle: Option<Arc<crate::stats::collector::AudioDecodeStatsHandle>>,
    /// Source audio stream_type (0x0F = AAC-ADTS, etc.). Used to decide
    /// which decoder to instantiate.
    source_stream_type: u8,
    /// Operator's AC-3 carriage choice (`audio_encode.ts_signalling`).
    ts_signalling: crate::config::models::TsAudioSignalling,
    /// AC-3 carriage convention, latched ONCE per replacer lifetime from
    /// the first source PMT (or pinned by `ts_signalling`). Never cleared
    /// on a source reset: output signalling must not flip 0x81 ↔ 0x06
    /// every time the flow switches between a DVB and an ATSC input.
    flavour: Option<TsFlavour>,
    /// "Configured but never engaged" watchdog (`audio_transcode_source_*`).
    engage: TranscodeEngageWatch,
    /// Event sink for the engage watchdog and the pinned-PID warning:
    /// `(sender, entity id, input_scope)`. `None` in tests / callers that
    /// do not wire events.
    event_sink: Option<(crate::manager::events::EventSender, String, bool)>,

    /// PES bytes accumulated for the current audio packet (since the
    /// previous PUSI). Flushed on the next PUSI.
    pes_buffer: Vec<u8>,
    /// Have we started collecting a PES yet? (false until the first PUSI)
    pes_started: bool,

    /// Continuity counter for the output audio PID. Increments per emitted
    /// audio TS packet.
    out_audio_cc: u8,

    /// Output PMT `version_number`, derived from content: bumps whenever
    /// the rebuilt PMT differs from the last one (a source PMT update that
    /// adds an ES, a codec change) and on every `reset_source_state()`,
    /// and otherwise holds. Receivers that cache by version therefore
    /// always re-parse a changed PMT — including after an `A → B → A`
    /// round trip — and never see a flapping version on an unchanged one.
    pmt_version: OutVersion,

    /// Output PTS anchor in 90 kHz ticks — the PTS of the first output
    /// sample emitted since the last anchor reset. Combined with
    /// [`Self::samples_since_anchor`] and [`Self::resolved_sample_rate`]
    /// it yields the exact output PES PTS without accumulating rounding.
    ///
    /// Replaces the previous per-PES FIFO queue, which broke down the
    /// moment input and output frame sizes differed (every
    /// frame-size-mismatched mapping — AC-3 1536 ↔ AAC-LC 1024 ↔
    /// HE-AAC 2048 — used to drift linearly because PTSes were pushed
    /// per source PES but popped per encoded output frame). The
    /// sample-anchor model is codec / frame-size / sample-rate
    /// agnostic: monotonic by construction, exact within ±1 tick
    /// regardless of how the decoder and encoder buffer.
    out_pts_90k: u64,
    /// True after the first source PES anchored [`Self::out_pts_90k`].
    /// Cleared on input switch / source codec change so the next PES
    /// re-establishes the anchor.
    out_pts_anchored: bool,
    /// Output samples emitted since the current anchor was set. Each
    /// emitted PES advances this by `ef.num_samples` (output rate);
    /// the per-PES PTS is recomputed from
    /// `out_pts_90k + samples_since_anchor * 90000 / resolved_sample_rate`
    /// so the division rounds once per PES rather than accumulating.
    samples_since_anchor: u64,
    /// Source-PES countdown before `av_skew` publication resumes after
    /// any anchor (re)establishment. The first few PES after an anchor
    /// land while the codec pipeline refills, producing transient deltas
    /// of hundreds of ms that would latch a scary `worst_abs_ms` on a
    /// healthy path (observed 832 ms at cold start 2026-06-06; steady
    /// state 0 ms).
    av_skew_holdoff: u8,
    /// Edge-added A/V skew reporter (`stats::av_skew`). At each source
    /// PES this replacer publishes `(output PTS its content will carry)
    /// − (source PES PTS)` — the exact audio-path lip-sync shift this
    /// re-encode introduces. This is the number that would have shown
    /// the historical Sky-Witness −27 ms/min loop drift directly on the
    /// dashboard.
    av_skew: Option<Arc<crate::stats::av_skew::AvSkewReporter>>,
    /// Last observed source PES PTS (90 kHz) plus the source-rate
    /// sample count from that PES, used to detect source-PTS
    /// discontinuities > ~500 ms — looping media files, ad splices,
    /// upstream encoder restarts — and re-anchor without waiting for
    /// the operator to bounce the flow. `None` before the first PES
    /// and after a reset.
    expected_next_src_pts_90k: Option<u64>,

    /// **Per-input** signal from the `TsPtsRewriter` on this same
    /// input's pipeline. The rewriter `fetch_add`s a forward PCR jump
    /// magnitude (27 MHz ticks) on every loop wrap; this replacer
    /// reads + zeros it on each PES and applies a coordinated
    /// PTS-leap + zero-PCM silence-pad so output audio PTS catches
    /// up with the (jumped) PCR. Per-input (vs per-flow) by design:
    /// passive inputs run their own pipelines with their own signal
    /// counters; nothing is shared across inputs so cross-input loop
    /// wraps can't pollute the active input's silence pad. `None`
    /// disables the mechanism (audio passthrough or test setup).
    pcr_jump_signal: Option<Arc<std::sync::atomic::AtomicI64>>,

    /// Pending silence-pad duration (27 MHz ticks) drained from
    /// `pcr_jump_signal` on each `consume_pes` and applied inside the
    /// per-decoded-frame loop once `codecs_ready` is true. Survives
    /// the first-PES + decode-failure window where `codecs_ready` is
    /// still false. Capped at 5 s per consume in the apply logic.
    pending_silence_27mhz: i64,

    /// Master clock value (27 MHz) at the first PES anchor — used by
    /// the wallclock-aware catch-up to compute how much master-clock
    /// time has elapsed since this encoder started. `None` until the
    /// first PES; cleared on `reset_source_state` (input switch /
    /// codec change) so the next PES re-anchors. Sampled from
    /// `av_sync_pacer.now_27mhz()`, which abstracts the per-flow
    /// master clock: wallclock (host time, the default), PTP
    /// (grandmaster-synced), or source_pcr_pll (PLL-recovered source
    /// clock). The catch-up below works correctly for all three —
    /// see the docstring on the catch-up site in `consume_pes`.
    first_pes_master_27mhz: Option<u64>,

    /// Source PES PTS (90 kHz) at the first PES anchor. Paired with
    /// `first_pes_master_27mhz` so the catch-up can compute
    /// `effective_out_pts_elapsed = effective_out_pts -
    /// first_pes_src_pts_90k` and compare it against
    /// `master.now_27mhz() - first_pes_master_27mhz`. `None` until
    /// the first PES; cleared on `reset_source_state`.
    first_pes_src_pts_90k: Option<u64>,

    /// Lazily constructed AAC-LC / ADTS decoder. Opened on the first PES
    /// flush once we know the source is AAC.
    #[cfg(feature = "fdk-aac")]
    aac_decoder: Option<aac_audio::AacDecoder>,

    /// Lazily constructed FFmpeg-backed decoder for non-AAC sources
    /// (MP2 / AC-3 / E-AC-3). Opened on the first PES flush once we
    /// know the source codec from the PMT. Mirrors the AAC slot above —
    /// only one is populated at a time per replacer instance.
    #[cfg(feature = "media-codecs")]
    ff_decoder: Option<video_engine::AudioDecoder>,

    /// Lazily constructed AAC encoder (for AAC-family targets). Opened on
    /// the first encode call, once we know the input sample-rate/channels
    /// after the first successful decode.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    aac_encoder: Option<aac_audio::AacEncoder>,

    /// Lazily constructed libavcodec encoder for MP2 / AC-3 targets.
    #[cfg(feature = "media-codecs")]
    av_encoder: Option<video_engine::AudioEncoder>,

    /// Per-channel PCM accumulator (f32, planar). Grown by successful
    /// decodes, drained in `frame_size`-sized chunks into the encoder.
    accumulator: Vec<Vec<f32>>,

    /// Resolved output channel count. Fixed after the first decode.
    resolved_channels: u8,
    /// Resolved output sample rate. Fixed after the first decode.
    resolved_sample_rate: u32,
    /// True after codecs are initialised (decoder + encoder both open).
    codecs_ready: bool,

    /// Optional channel-shuffle / sample-rate transcode block. Applied in
    /// planar PCM form between the AAC decoder and the target encoder.
    /// `None` preserves the pre-transcode behaviour exactly.
    transcode_cfg: Option<TranscodeJson>,
    /// Lazily constructed planar transcoder. Opened on the first decoded
    /// frame, once the input rate + channel count are known.
    transcoder: Option<PlanarAudioTranscoder>,

    /// One-shot "input was switched" request. The flow's per-output
    /// switch watcher flips this to `true` when the active input
    /// changes; the replacer consumes it on the next `process()` entry
    /// and runs the same `reset_source_state()` path that fires on
    /// codec/PID change. Without this, a same-codec same-PID swap leaves
    /// stale decoder + PTS-anchor state from the previous input and the
    /// receiver hears wrong-epoch PTS audio frames against a master-
    /// clock-paced PCR.
    external_reset: Arc<AtomicBool>,

    /// Optional per-flow A/V sync pacer. When set, the anchor target on
    /// first PES and on every discontinuity re-anchor is derived from
    /// `master.now_27mhz()/300 + PCR_PREROLL + lipsync` instead of the
    /// raw source PES PTS. A 10 s safety check falls back to source PTS
    /// when master and source clocks are wildly different (Wallclock
    /// master vs uncorrelated source, or PLL pre-lock garbage) — the
    /// re-anchor only kicks in when the two agree to within 10 s.
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
}

impl TsAudioReplacer {
    /// Build a new replacer from an `audio_encode` block and an optional
    /// `transcode` block.
    ///
    /// This only parses and validates the codec — all heavy codec state is
    /// opened lazily when the first PES is flushed, so a replacer for a
    /// flow that never carries audio costs essentially nothing.
    ///
    /// When `transcode` is `Some`, the decoded PCM is run through a planar
    /// channel-shuffle / sample-rate stage before the target encoder. Unset
    /// fields inside the block fall back to the encoder's own overrides and
    /// then the source format, so an empty block is a no-op.
    ///
    /// PID rewriting (the operator's `pid_overrides` map) is handled by the
    /// downstream `TsPidOverridesRewriter` stage — the replacer re-encodes
    /// audio on the same source PID it learned from the PMT and lets that
    /// stage rename the PID afterwards.
    pub fn new(
        cfg: &AudioEncodeConfig,
        transcode: Option<TranscodeJson>,
    ) -> Result<Self, TsAudioReplaceError> {
        let codec = AudioCodec::parse(&cfg.codec)
            .ok_or_else(|| TsAudioReplaceError::UnknownCodec(cfg.codec.clone()))?;

        // Opus has no standard MPEG-TS mapping on our targeted outputs.
        if matches!(codec, AudioCodec::Opus) {
            return Err(TsAudioReplaceError::UnsupportedCodec(cfg.codec.clone()));
        }

        let bitrate_kbps = cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

        Ok(Self {
            codec,
            bitrate_kbps,
            sample_rate_override: cfg.sample_rate,
            channels_override: cfg.channels,
            pmt_pid: None,
            program_number: None,
            pmt_pid_shared: false,
            pmt_stage: PsiUnitStage::new("ts_audio_replace"),
            gate: PrePmtGate::default(),
            passthrough_cc: PassthroughCc::default(),
            audio_pid: None,
            source_audio_pid_pin: cfg.source_audio_pid,
            last_pinned_warn: None,
            stats: Arc::new(TsAudioReplacerStats::default()),
            decode_stats: Arc::new(crate::engine::audio_decode::DecodeStats::new()),
            encode_stats: Arc::new(crate::engine::audio_encode::EncodeStats::new()),
            output_stats: None,
            input_decode_handle: None,
            source_stream_type: 0,
            ts_signalling: cfg.ts_signalling.unwrap_or_default(),
            flavour: None,
            engage: TranscodeEngageWatch::new(TranscodeKind::Audio, cfg.source_audio_pid),
            event_sink: None,
            pes_buffer: Vec::with_capacity(16 * 1024),
            pes_started: false,
            out_audio_cc: 0,
            pmt_version: OutVersion::new(),
            out_pts_90k: 0,
            out_pts_anchored: false,
            samples_since_anchor: 0,
            av_skew_holdoff: 0,
            av_skew: None,
            expected_next_src_pts_90k: None,
            pcr_jump_signal: None,
            pending_silence_27mhz: 0,
            first_pes_master_27mhz: None,
            first_pes_src_pts_90k: None,
            #[cfg(feature = "fdk-aac")]
            aac_decoder: None,
            #[cfg(feature = "media-codecs")]
            ff_decoder: None,
            #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
            aac_encoder: None,
            #[cfg(feature = "media-codecs")]
            av_encoder: None,
            accumulator: Vec::new(),
            resolved_channels: 0,
            resolved_sample_rate: 0,
            codecs_ready: false,
            transcode_cfg: transcode,
            transcoder: None,
            external_reset: Arc::new(AtomicBool::new(false)),
            av_sync_pacer: None,
        })
    }

    /// Attach a per-flow A/V sync pacer. When set, the anchor target on
    /// first PES and on each `>500 ms` source-PTS discontinuity is
    /// derived from `master.now_27mhz()/300 + PCR_PREROLL + lipsync`
    /// instead of the source PES PTS — the same model as
    /// [`crate::engine::ts_pts_rewriter::TsPtsRewriter`] uses on the
    /// passthrough path. Safe to call zero or one time before
    /// `process()` runs; calling twice silently overwrites. Mirrors
    /// [`crate::engine::ts_video_replace::TsVideoReplacer::set_av_sync_pacer`].
    pub fn set_av_sync_pacer(
        &mut self,
        pacer: Arc<crate::engine::av_sync_mux::AvSyncPacer>,
    ) {
        self.av_sync_pacer = Some(pacer);
    }

    /// Attach the per-input edge-added A/V skew reporter. See the
    /// `av_skew` field doc-comment.
    pub fn set_av_skew_reporter(
        &mut self,
        reporter: Arc<crate::stats::av_skew::AvSkewReporter>,
    ) {
        self.av_skew = Some(reporter);
    }

    /// Attach a per-input PCR forward-jump signal `Arc<AtomicI64>`,
    /// shared with the `TsPtsRewriter` on this SAME input's
    /// pipeline. The rewriter writes the magnitude of every forward
    /// PCR jump > 500 ms; this replacer reads + zeros it on each PES
    /// and applies a coordinated PTS-leap + zero-PCM silence-pad so
    /// output audio PTS catches up with the new PCR. Per-input by
    /// design (vs per-flow) so passive inputs' loop wraps cannot
    /// pollute the active input's audio. See
    /// [`TsAudioReplacer::pcr_jump_signal`] for the full rationale.
    /// Idempotent; calling twice overwrites.
    pub fn set_pcr_jump_signal(
        &mut self,
        signal: Arc<std::sync::atomic::AtomicI64>,
    ) {
        self.pcr_jump_signal = Some(signal);
    }

    /// Wire the manager event sender so the replacer can report that it
    /// is configured but has found nothing to re-encode
    /// (`audio_transcode_source_not_found`, then `_found` on a later lock)
    /// and that an operator-pinned `source_audio_pid` is absent
    /// (`audio_source_pid_not_found`). `input_scope` selects input- vs
    /// output-scoped events, exactly like the video replacer's
    /// decode-stall watchdog. Safe to call zero or one time before
    /// `process()` runs.
    pub fn set_event_watchdog(
        &mut self,
        event_sender: crate::manager::events::EventSender,
        id: impl Into<String>,
        input_scope: bool,
    ) {
        self.event_sink = Some((event_sender, id.into(), input_scope));
    }

    /// Shared handle to the one-shot "input was switched" request flag.
    /// The flow's per-output switch watcher sets this to `true` when the
    /// active input changes; the replacer consumes and clears it on
    /// entry to its next `process()` call and runs the same
    /// `reset_source_state()` path that fires on codec/PID change. The
    /// audio counterpart of `TsVideoReplacer::external_reset_handle()`.
    /// Idempotent under rapid repeated switches — collapses into a
    /// single reset.
    #[allow(dead_code)]
    pub fn external_reset_handle(&self) -> Arc<AtomicBool> {
        self.external_reset.clone()
    }

    /// Shared handle to the source-PID stats counters. Output forward
    /// loops register this with the per-output stats accumulator at
    /// startup so the manager snapshot surfaces "transcoding from PID
    /// 0x0101 (AAC)" on the audio_encode_stats badge.
    pub fn stats_handle(&self) -> Arc<TsAudioReplacerStats> {
        self.stats.clone()
    }

    /// Shared handle to the internal AAC / MP2 / AC-3 / E-AC-3 decoder
    /// counters. Outputs register this via
    /// [`crate::stats::collector::OutputStatsAccumulator::set_decode_stats`]
    /// at spawn time so the manager's pipeline summary emits the
    /// `audio_decode` tag — paired with the encoder's `audio_encode`
    /// the UI then renders a single "Audio Transcode" badge.
    pub fn decode_stats_handle(&self) -> Arc<crate::engine::audio_decode::DecodeStats> {
        self.decode_stats.clone()
    }

    /// Shared handle to the internal encode-stage counters
    /// (`pcm_frames_submitted` / `encoded_frames_out` / etc.). Ingress
    /// callers register this via
    /// [`crate::stats::collector::FlowStatsAccumulator::set_input_encode_stats`]
    /// at spawn time so the manager's inputs-live snapshot carries the
    /// encode side of the transcode — paired with `decode_stats_handle()`
    /// the UI renders an `Audio Transcode` badge instead of the
    /// encode-only fallback.
    pub fn encode_stats_handle(&self) -> Arc<crate::engine::audio_encode::EncodeStats> {
        self.encode_stats.clone()
    }

    /// Attach the owning output's stats accumulator so the replacer can
    /// refresh the live `audio_decode_stats` source-codec / output PCM
    /// shape whenever the source codec is rediscovered on a PMT update.
    /// Optional: omitting it keeps the badge alive (counters tick) but
    /// the source-codec label stays at its placeholder.
    pub fn with_output_stats(
        mut self,
        stats: Arc<crate::stats::collector::OutputStatsAccumulator>,
    ) -> Self {
        self.output_stats = Some(stats);
        self
    }

    /// Attach the input-side `AudioDecodeStatsHandle` the replacer should
    /// keep refreshing as the PMT learns the source codec. Mirrors
    /// [`Self::with_output_stats`] for ingress-side flows registered via
    /// [`crate::engine::input_transcode::register_ingress_stats`].
    pub fn with_input_decode_handle(
        &mut self,
        handle: Arc<crate::stats::collector::AudioDecodeStatsHandle>,
    ) {
        self.input_decode_handle = Some(handle);
        // Push whatever we know right now (placeholder until the PMT is
        // observed) so the UI doesn't lag a tick.
        self.refresh_decode_stats_label();
    }

    /// Best-effort label refresh on the registered audio-decode handle.
    /// Called whenever `self.source_stream_type` / `self.resolved_*`
    /// change. Refreshes both the output-side handle (when
    /// [`Self::with_output_stats`] was attached) and the input-side
    /// handle (when [`Self::with_input_decode_handle`] was attached).
    /// Either or both may be `None`.
    fn refresh_decode_stats_label(&self) {
        let codec_label: &'static str = match self.source_stream_type {
            0x0F | 0x11 => "AAC",
            0x03 | 0x04 => "MP2",
            0x81 | 0x80 | 0xC1 => "AC-3",
            0x87 | 0xC2 => "E-AC-3",
            _ => "",
        };
        // Output-side path: look the handle up through the per-output
        // accumulator. Same lookup as before — kept for backward compat
        // with output-side TS replacers.
        if let Some(stats) = self.output_stats.as_ref()
            && let Some(h) = stats.audio_decode_stats_handle() {
                if !codec_label.is_empty() {
                    h.set_input_codec(codec_label);
                }
                if self.resolved_sample_rate != 0 && self.resolved_channels != 0 {
                    h.set_output_shape(self.resolved_sample_rate, self.resolved_channels);
                }
            }
        // Input-side path: handle held directly. Skips the
        // accumulator-level indirection because ingress-side handles are
        // keyed by `input_id` and the replacer never sees the flow-stats
        // accumulator.
        if let Some(h) = self.input_decode_handle.as_ref() {
            if !codec_label.is_empty() {
                h.set_input_codec(codec_label);
            }
            if self.resolved_sample_rate != 0 && self.resolved_channels != 0 {
                h.set_output_shape(self.resolved_sample_rate, self.resolved_channels);
            }
        }
    }

    /// Human-readable description of the active encoder target.
    pub fn target_description(&self) -> String {
        format!(
            "{} @ {} kbps",
            self.codec.as_str(),
            self.bitrate_kbps,
        )
    }

    /// The audio PID whose PES this replacer re-encodes (`None` while it
    /// passes the source through) — what the chain's trailing PCR stage
    /// checks against its PCR.
    pub fn replaced_pid(&self) -> Option<u16> {
        self.audio_pid.filter(|_| source_replaceable(self.source_stream_type))
    }

    /// Process one chunk of raw MPEG-TS bytes.
    ///
    /// `input_ts` must be 188-byte aligned (caller is responsible for TS
    /// sync recovery). Output TS bytes are appended to `output`. On bad
    /// input (not TS-aligned) the chunk is appended unchanged.
    ///
    /// Until the program's PMT has been parsed only PSI / SI (PIDs ≤ 0x1F,
    /// the PMT PID) and null packets are forwarded: source audio ahead of
    /// the PMT would go out untranscoded, and the replacer's first packet
    /// on the PID would then jump its CC. A PMT that never parses opens the
    /// gate after 5 s (passthrough; the engage watchdog says why).
    pub fn process(&mut self, input_ts: &[u8], output: &mut Vec<u8>) {
        self.process_at(input_ts, output, std::time::Instant::now());
    }

    fn process_at(&mut self, input_ts: &[u8], output: &mut Vec<u8>, now: std::time::Instant) {
        if input_ts.is_empty() {
            return;
        }

        // External "input was switched" trigger — see field doc on
        // `external_reset`. Same-codec same-PID swaps don't fire the
        // codec/PID-change reset path, so without this hook stale
        // decoder + PTS-anchor state leaks across the boundary.
        if self.external_reset.swap(false, Ordering::Relaxed) {
            self.reset_source_state("input switched");
        }

        // Bail out for non-aligned input: passthrough, do nothing clever.
        if !input_ts.len().is_multiple_of(TS_PACKET_SIZE) {
            output.extend_from_slice(input_ts);
            return;
        }

        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= input_ts.len() {
            let pkt = &input_ts[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if pkt[0] != TS_SYNC_BYTE {
                // Lost alignment — emit as-is and move on.
                output.extend_from_slice(pkt);
                continue;
            }

            let pid = ts_pid(pkt);

            // Learn the PMT PID from every PAT. A PMT-PID change means
            // the input switched to an input with a different program
            // layout — reset source-side state so the pipeline
            // re-learns everything from the new program.
            if pid == PAT_PID && ts_pusi(pkt) {
                let mut programs = parse_pat_programs(pkt);
                if !programs.is_empty() {
                    programs.sort_by_key(|(num, _)| *num);
                    let (new_program, new_pmt_pid) = programs[0];
                    self.pmt_pid_shared =
                        programs.iter().filter(|(_, p)| *p == new_pmt_pid).count() > 1;
                    self.engage.note_pat(new_program, new_pmt_pid);
                    if self.pmt_pid != Some(new_pmt_pid)
                        || self.program_number != Some(new_program)
                    {
                        if self.pmt_pid.is_some() {
                            self.audio_pid = None;
                            self.source_stream_type = 0;
                            self.reset_source_state("PMT PID changed");
                            // A new program: gate until its PMT parses.
                            self.gate.rearm(now);
                        }
                        if self.pmt_pid != Some(new_pmt_pid) {
                            self.pmt_stage = PsiUnitStage::new("ts_audio_replace");
                        }
                        self.pmt_pid = Some(new_pmt_pid);
                        self.program_number = Some(new_program);
                    }
                }
            }

            // Every packet on the PMT PID goes through the reassembling
            // stage. A complete unit is inspected (re-reading audio_pid and
            // source_stream_type on every PMT, so input switches between
            // inputs with different audio codecs / PIDs are handled
            // seamlessly) and, when this replacer re-encodes, the program's
            // PMT section is rebuilt; other sections on the PID (a 0xC0
            // table ahead of the PMT, other programs) stay byte-identical.
            if Some(pid) == self.pmt_pid {
                if ts_pusi(pkt) {
                    self.engage.note_pmt_pusi();
                }
                if let Some(unit) = self.pmt_stage.push(pkt, output) {
                    self.handle_pmt_unit(unit, output);
                }
                continue;
            }

            if self.gate.drops(pid, now, "ts_audio_replace") {
                self.stats.pre_pmt_dropped_packets.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // Audio packets: route to the PES accumulator only when the
            // source codec is one we can actually decode. Anything else
            // falls through to the passthrough branch below — losing the
            // re-encode is preferable to dropping audio entirely.
            if Some(pid) == self.audio_pid && source_replaceable(self.source_stream_type) {
                // A PCR on the audio PID (radio services, some SD
                // programmes) keeps its stream position as an
                // adaptation-field-only packet — value and DI unchanged,
                // CC repeating the last payload CC — instead of vanishing
                // with the source payload. The chain's `ts_pcr_remux`
                // stage owns the delay.
                if let Some(pcr) = extract_pcr(pkt) {
                    let cc = self.out_audio_cc.wrapping_sub(1) & 0x0F;
                    output.extend_from_slice(&pcr_only_packet(
                        pid,
                        cc,
                        pcr,
                        ts_discontinuity_indicator(pkt),
                    ));
                }
                self.feed_audio_packet(pkt, output);
                continue;
            }

            // Everything else: passthrough.
            self.passthrough_cc.note(pid, pkt);
            output.extend_from_slice(pkt);
        }

        self.engage.note_packets((input_ts.len() / TS_PACKET_SIZE) as u64);
        self.poll_engage(now);
    }

    /// Advance the engage watchdog and emit whatever it raises. Returns the
    /// event for tests (which drive the clock explicitly).
    fn poll_engage(&mut self, now: std::time::Instant) -> Option<EngageEvent> {
        let ev = self.engage.tick(now)?;
        self.emit_engage(&ev);
        Some(ev)
    }

    fn emit_engage(&self, ev: &EngageEvent) {
        if let Some((sender, id, input_scope)) = self.event_sink.as_ref() {
            self.engage.emit(ev, sender, id, *input_scope);
        }
    }

    /// The configured target as a PMT signalling target. MP2 at 16 / 22.05
    /// / 24 kHz is MPEG-2 LSF (stream_type 0x04); the output rate is the
    /// resolved one once the first frame decoded, the configured override
    /// before that.
    fn audio_target(&self) -> AudioTarget {
        match self.codec {
            AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => AudioTarget::Aac,
            AudioCodec::Mp2 => {
                let rate = if self.resolved_sample_rate != 0 {
                    self.resolved_sample_rate
                } else {
                    self.sample_rate_override.unwrap_or(0)
                };
                AudioTarget::Mp2 { lsf: matches!(rate, 16_000 | 22_050 | 24_000) }
            }
            AudioCodec::Ac3 => AudioTarget::Ac3 {
                flavour: self.flavour.unwrap_or(TsFlavour::Atsc),
            },
            AudioCodec::Opus => unreachable!("rejected in new()"),
        }
    }

    /// Inspect one complete PMT-PID unit, learn the audio ES of our
    /// program, rebuild that program's PMT when re-encoding, and emit.
    fn handle_pmt_unit(&mut self, mut unit: PsiUnit, output: &mut Vec<u8>) {
        self.engage.note_pmt_unit(unit.first_table_id());
        let idx = self
            .program_number
            .and_then(|p| pmt_index(unit.sections(), p, self.pmt_pid_shared));
        let Some(i) = idx else {
            self.pmt_stage.emit(unit, output);
            return;
        };
        let section = unit.sections()[i].clone();
        // Never learn from, or rebuild (and so re-CRC), a damaged PMT.
        let Some(view) = parse_pmt(&section)
            .filter(|_| crate::engine::ts_parse::mpeg2_crc32(&section) == 0)
        else {
            self.pmt_stage.emit(unit, output);
            return;
        };
        // The program's PMT is known: the pre-PMT gate opens.
        self.gate.open();
        let was_replacing = self.replaced_pid();
        if self.flavour.is_none() {
            use crate::config::models::TsAudioSignalling;
            self.flavour = Some(match self.ts_signalling {
                TsAudioSignalling::Dvb => TsFlavour::Dvb,
                TsAudioSignalling::Atsc => TsFlavour::Atsc,
                TsAudioSignalling::Auto => detect_flavour(&view),
            });
        }
        let sel = select_audio_es(&view, self.source_audio_pid_pin);
        self.engage.note_pmt_parsed(sel.es.clone(), sel.unsupported_candidate);
        if let Some((apid, ast)) = sel.chosen {
            // Operator pinned a specific PID but the PMT resolved to a
            // different one — they got the first-match fallback. Warn
            // loudly once per distinct (pinned, actual) pair so log review
            // and the manager's Events page surface the misconfiguration.
            if let Some(pin) = self.source_audio_pid_pin {
                if pin != apid && self.last_pinned_warn != Some((pin, apid)) {
                    tracing::warn!(
                        error_code = "audio_source_pid_not_found",
                        pinned_pid = format!("0x{pin:04X}"),
                        actual_pid = format!("0x{apid:04X}"),
                        actual_stream_type = format!("0x{ast:02X}"),
                        "audio_encode.source_audio_pid pin not present in PMT — falling back to first-matching-codec audio (pinned 0x{pin:04X} → actual 0x{apid:04X})"
                    );
                    if let Some((sender, id, input_scope)) = self.event_sink.as_ref() {
                        crate::engine::transcode_engage::emit_pinned_pid_absent(
                            TranscodeKind::Audio,
                            sender,
                            id,
                            *input_scope,
                            pin,
                            apid,
                            ast,
                        );
                    }
                    self.last_pinned_warn = Some((pin, apid));
                } else if pin == apid && self.last_pinned_warn.is_some() {
                    // Pin re-found (e.g. after an upstream PMT change) —
                    // clear the suppression so a future drop-out warns again.
                    self.last_pinned_warn = None;
                }
            }
            let codec_changed = self.source_stream_type != 0 && self.source_stream_type != ast;
            let pid_changed = self.audio_pid.is_some() && self.audio_pid != Some(apid);
            if codec_changed || pid_changed {
                self.reset_source_state(&format!(
                    "source changed: stream_type {:#04x} -> {:#04x}, pid {:?} -> {}",
                    self.source_stream_type, ast, self.audio_pid, apid
                ));
            }
            self.audio_pid = Some(apid);
            self.source_stream_type = ast;
            // Starting to re-encode a PID (first lock, a PID move, or a
            // switch from a codec that passed through): continue the CC
            // sequence of whatever went out on it before.
            if self.replaced_pid() != was_replacing
                && self.replaced_pid() == Some(apid)
                && let Some(cc) = self.passthrough_cc.take_next_after(apid)
            {
                self.out_audio_cc = cc;
            }
            // Surface for the manager UI's "(from PID 0x0101)" badge.
            // Updated on every PMT discovery so input swaps and PMT-version
            // bumps that change the discovered audio PID are visible
            // immediately.
            self.stats.source_pid.store(apid, Ordering::Relaxed);
            self.stats.source_stream_type.store(ast, Ordering::Relaxed);
            self.refresh_decode_stats_label();
            if let Some(ev) = self.engage.note_locked(apid, ast) {
                self.emit_engage(&ev);
            }
        } else {
            // The program no longer carries decodable audio: stop
            // re-encoding (the ES, if any, passes through) and re-arm the
            // engage watchdog.
            if self.audio_pid.is_some() {
                self.reset_source_state("PMT no longer carries decodable audio");
                self.audio_pid = None;
                self.source_stream_type = 0;
            }
            self.engage.note_unlocked(std::time::Instant::now());
        }

        // Rebuild the PMT whenever the source codec is one we replace
        // (AAC family via fdk-aac, or MP2/AC-3/E-AC-3 via the FFmpeg-backed
        // decoder): new stream_type, the target's descriptor policy, a
        // content-tracked version. Anything else keeps the PMT untouched so
        // downstream decoders see the truth.
        if let Some(apid) = self.audio_pid
            && source_replaceable(self.source_stream_type)
        {
            let target = self.audio_target();
            let edit = [EsEdit::Audio { pid: apid, target }];
            // Growth fallback: the AC-3 additions (registration + 0x6A) are
            // the only edit that grows a section. When the grown unit would
            // need more packets than the source's, or the grown section
            // would overflow 1021 bytes, emit self-identifying 0x81 with no
            // additions instead.
            let rebuilt = rebuild_pmt_section_fitting(
                &unit,
                i,
                &PmtEdit { es: &edit, ..Default::default() },
            );
            if let Some(mut new_section) = rebuilt {
                self.pmt_version.stamp(&mut new_section);
                unit.replace_section(i, new_section);
                self.pmt_stage.emit(unit, output);
                return;
            }
        }
        // Not re-encoding (e.g. an input switch to a DTS-only source). Once
        // this stage has stamped a version, the passthrough PMT is stamped
        // from the same sequence (content unchanged): with its source
        // version it could repeat the version the rebuilt PMT carried, and a
        // receiver caching by version would keep the old stream_type / PID.
        // Before any stamp the PMT stays byte-identical.
        if self.pmt_version.has_stamped() {
            let mut passthrough = section;
            self.pmt_version.stamp(&mut passthrough);
            unit.replace_section(i, passthrough);
        }
        self.pmt_stage.emit(unit, output);
    }

    /// Flush any buffered PES + encoder state. Call once on graceful
    /// shutdown. No-op if codecs were never initialised.
    #[allow(dead_code)]
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        // Flush pending PES.
        if self.pes_started && !self.pes_buffer.is_empty() {
            let pes = std::mem::take(&mut self.pes_buffer);
            let _ = self.consume_pes(&pes, output);
            self.pes_buffer.clear();
            self.pes_started = false;
        }

        // Flush the encoder (last encoded frames live here).
        #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
        {
            if let Some(ref mut enc) = self.aac_encoder {
                // fdk-aac encoder drains by calling encode_frame with empty
                // input — we approximate by skipping, since most broadcast
                // feeds don't need the trailing frames.
                let _ = enc;
            }
        }

        #[cfg(feature = "media-codecs")]
        {
            if let Some(ref mut enc) = self.av_encoder
                && let Ok(frames) = enc.flush() {
                    let pid = match self.audio_pid {
                        Some(p) => p,
                        None => return,
                    };
                    let sr = enc.sample_rate();
                    for ef in frames {
                        let pts = self.next_output_pts_90k(sr);
                        let pes = build_audio_pes(self.codec.ts_pes_stream_id(), &ef.data, pts);
                        let pkts = packetize_ts(pid, &pes, &mut self.out_audio_cc);
                        for pkt in &pkts {
                            output.extend_from_slice(pkt);
                        }
                        self.samples_since_anchor =
                            self.samples_since_anchor.saturating_add(ef.num_samples as u64);
                    }
                }
        }
    }

    /// Compute the PTS for the next output PES from the running anchor
    /// + samples-emitted counter. The division rounds once per call
    /// rather than accumulating, so a long-running encoder at a non-
    /// integer-tick sample rate (e.g. 44.1 kHz) doesn't drift relative
    /// to the source clock. Returns the anchor verbatim when the
    /// resolved output sample rate isn't known yet — the encoder won't
    /// have produced any frames either, so this is the lazy-init path.
    fn next_output_pts_90k(&self, sample_rate: u32) -> u64 {
        if sample_rate == 0 {
            return self.out_pts_90k;
        }
        let advance =
            self.samples_since_anchor.saturating_mul(90_000) / sample_rate as u64;
        self.out_pts_90k.wrapping_add(advance)
    }

    /// Drain `pending_silence_27mhz` into the PCM accumulator as
    /// zero samples — the encoder will produce K silence frames as
    /// it drains, each one advancing `samples_since_anchor` by
    /// `frame_size`. Long-term `effective_out_pts = out_pts_90k +
    /// samples_since_anchor * 90000 / sr` advances by exactly
    /// `silence_samples * 90000 / sr ≈ jump_27mhz / 300` — the
    /// desired catch-up amount, independent of K.
    ///
    /// **`out_pts_90k` is intentionally untouched.** The pre-0.87
    /// implementation also did `out_pts_90k += jump_90k -
    /// frame_step` thinking it was correcting a "natural +1
    /// encoder-frame-step" overshoot. That logic was only correct
    /// when exactly one silence frame was emitted (K=1): for K>1
    /// the formula double-counted, over-advancing effective PTS by
    /// `(K-1) * frame_step` per jump (≈ 235 ms over on a typical
    /// 268 ms loop-splice). Removing the `out_pts_90k += …` line
    /// restores the correct long-term catch-up so audio PTS tracks
    /// source PTS exactly even when the jump spans many frames.
    ///
    /// Cap at 5 s per consume to bound worst-case pathological
    /// inputs (the cap protects against a stuck signal at startup
    /// — operator gets a single fixed-size burst of silence, not
    /// hours of zeroes).
    ///
    /// No-op when `pending_silence_27mhz == 0`, when
    /// `codecs_ready == false` (encoder pipeline not yet open —
    /// the queue persists across decode failures and the first
    /// PES), or when `resolved_sample_rate == 0` (cannot convert
    /// 27 MHz ticks to sample counts).
    fn apply_pending_silence_pad(&mut self) {
        if !self.codecs_ready
            || self.pending_silence_27mhz <= 0
            || self.resolved_sample_rate == 0
        {
            return;
        }
        let capped_27mhz = self
            .pending_silence_27mhz
            .min(5 * 27_000_000) as u64;
        let sr = self.resolved_sample_rate as u64;
        let silence_samples = capped_27mhz
            .saturating_mul(sr)
            / 27_000_000;
        if silence_samples > 0 {
            let silence_ms = capped_27mhz / 27_000;
            tracing::info!(
                silence_samples,
                silence_ms,
                sample_rate = self.resolved_sample_rate,
                "ts_audio_replace: applying silence-pad for source-PTS forward jump \
                 (silence-content-only; out_pts_90k unchanged so encoder frame stamping \
                 advances effective PTS by silence_samples * 90000 / sample_rate)"
            );
            for ch in self.accumulator.iter_mut() {
                ch.extend(
                    std::iter::repeat_n(0.0f32, silence_samples as usize),
                );
            }
        }
        self.pending_silence_27mhz = 0;
    }

    // ── Internal helpers ─────────────────────────────────────────────

    /// Drop every pipeline stage that depends on the current source
    /// stream — decoder, transcoder, encoder, resolved format,
    /// accumulator, PES buffer, PTS anchor. Called when the source
    /// audio codec or PID changes mid-flow (seamless input switching
    /// between inputs with different audio codecs, or a PAT/PMT
    /// program re-layout).
    ///
    /// The target codec itself (`self.codec`)
    /// is preserved — that's the output's configured codec, which
    /// never changes.
    fn reset_source_state(&mut self, reason: &str) {
        tracing::info!(
            "ts_audio_replace: {reason}; reopening audio decoder / encoder"
        );
        self.pes_buffer.clear();
        self.pes_started = false;
        // Re-anchor output PTS to the new input's first PES so the
        // audio stays aligned with the video replacer, which also
        // re-anchors on the video-PID codec swap.
        self.out_pts_anchored = false;
        #[cfg(feature = "fdk-aac")]
        {
            self.aac_decoder = None;
        }
        #[cfg(feature = "media-codecs")]
        {
            self.ff_decoder = None;
        }
        // Encoder and transcoder are both keyed off the input sample
        // rate / channels (resolved from the first decode). The new
        // input may have a different format, so tear them down and
        // let `init_encoder` / transcoder lazy-open rebuild them.
        #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
        {
            self.aac_encoder = None;
        }
        #[cfg(feature = "media-codecs")]
        {
            self.av_encoder = None;
        }
        self.transcoder = None;
        self.accumulator.clear();
        self.samples_since_anchor = 0;
        self.av_skew_holdoff = 32; // suppress transient deltas while the pipeline refills
        self.expected_next_src_pts_90k = None;
        // Drop any unapplied silence-pad and drain the shared signal
        // counter — on input switch / codec change the audio path
        // starts fresh and stale loop-wrap signals are no longer
        // meaningful. `swap(0)` is atomic so we don't race with the
        // rewriter on the same input.
        self.pending_silence_27mhz = 0;
        // Reset the wallclock-aware catch-up anchors. Next first PES
        // re-records both (master clock now + that PES's source PTS).
        self.first_pes_master_27mhz = None;
        self.first_pes_src_pts_90k = None;
        if let Some(s) = self.pcr_jump_signal.as_ref() {
            s.swap(0, std::sync::atomic::Ordering::AcqRel);
        }
        self.resolved_channels = 0;
        self.resolved_sample_rate = 0;
        self.codecs_ready = false;
        // Bump the rewritten-PMT version (mod 32) so receivers see a
        // distinct version on the next PMT and re-parse. Content changes
        // bump on their own (see `pmt_version`); this covers a reset whose
        // PMT happens to be byte-identical to the previous source's.
        self.pmt_version.bump();
        // A new source gets a fresh engage window.
        self.engage.on_reset();
    }

    /// Route one audio TS packet into the PES accumulator, flushing the
    /// previous PES (if any) into `output` when a new PES begins.
    fn feed_audio_packet(&mut self, pkt: &[u8], output: &mut Vec<u8>) {
        if !ts_has_payload(pkt) {
            return;
        }
        let pusi = ts_pusi(pkt);
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            return;
        }
        let payload = &pkt[payload_start..];

        if pusi {
            // Flush previous PES, if any.
            if self.pes_started && !self.pes_buffer.is_empty() {
                let pes = std::mem::take(&mut self.pes_buffer);
                let _ = self.consume_pes(&pes, output);
                self.pes_buffer.clear();
            }
            self.pes_buffer.extend_from_slice(payload);
            self.pes_started = true;
        } else if self.pes_started {
            // DoS guard: a stream that stops emitting PUSI would otherwise grow
            // this audio PES buffer without bound. Drop + resync on next PUSI.
            const PES_CAP: usize = 1024 * 1024;
            if self.pes_buffer.len().saturating_add(payload.len()) > PES_CAP {
                self.pes_buffer.clear();
                self.pes_started = false;
            } else {
                self.pes_buffer.extend_from_slice(payload);
            }
        }
    }

    /// Decode one complete PES worth of audio, feed the PCM into the
    /// encoder, and emit as many encoded-audio TS packets as the encoder
    /// produced.
    fn consume_pes(&mut self, pes: &[u8], output: &mut Vec<u8>) -> Result<(), ()> {
        let (es_data, pts) = match extract_pes_audio(pes) {
            Some(x) => x,
            None => return Err(()),
        };

        // Drain the per-input PCR forward-jump signal from the paired
        // `TsPtsRewriter`. `swap(0)` is atomic so this is race-free
        // even though the rewriter writes from a different task on
        // the same input pipeline. The drained delta accumulates onto
        // `pending_silence_27mhz` and is applied below inside the
        // per-decoded-frame loop once `codecs_ready` is true — that
        // way the silence-pad survives the first-PES + decode-failure
        // window where the encoder isn't initialised yet.
        if let Some(s) = self.pcr_jump_signal.as_ref() {
            let drained = s.swap(0, std::sync::atomic::Ordering::AcqRel);
            if drained > 0 {
                self.pending_silence_27mhz =
                    self.pending_silence_27mhz.saturating_add(drained);
            }
        }

        // Anchor establishment + discontinuity guard. On the first PES
        // we anchor the output PTS to the source's intrinsic PTS so the
        // output stream tracks the source clock from sample 0. After
        // that, every PES is compared against the expected next source
        // PTS (last PTS + last-PES sample-duration). Two outcomes:
        //
        // 1. Forward delta in `(SUB_DISCONTINUITY_THRESHOLD_90K,
        //    DISCONTINUITY_THRESHOLD_90K]` — likely a media_player loop
        //    splice (or any source whose splice gap is `max(audio_max,
        //    pcr_max) + SPLICE_GUARD`, which on real broadcast
        //    captures lands at audio_max + ~268 ms because
        //    pcr_max - audio_max ≈ ~238 ms). Without catch-up, output
        //    audio PES PTS keeps advancing at steady encoder rate
        //    (samples / sr) while video PES PTS (passthrough) carries
        //    the source's per-loop forward jump verbatim, drifting
        //    audio behind video by the loop-jump amount per loop. Route
        //    the forward delta through `pending_silence_27mhz` so the
        //    encoder produces silence frames that consume samples at
        //    the right rate (the silence content alone advances
        //    samples_since_anchor by the right amount; no out_pts_90k
        //    nudge needed). Pre-fix on a Sky Witness 1080i25 looped
        //    capture (30 min × 10 loops): -27 ms/min linear A/V drift
        //    (cell 4 of testbed/full_test_2026-05-21/v2).
        //
        // 2. Absolute delta > DISCONTINUITY_THRESHOLD_90K — catastrophic
        //    jump (upstream restart, SCTE-35 splice across a > 500 ms
        //    boundary). Re-anchor `out_pts_90k` and reset
        //    `samples_since_anchor` so audio doesn't accumulate tens of
        //    seconds of phantom drift against the regenerated output
        //    PCR. Backward jumps in this range hit the monotonicity
        //    guard and are suppressed (see below).
        const DISCONTINUITY_THRESHOLD_90K: u64 = 45_000; // 500 ms
        const SUB_DISCONTINUITY_THRESHOLD_90K: i64 = 7_200; // 80 ms
        // Deadband for the media_player anchor-relabel tracker below: only
        // forward source-PTS steps larger than this are tracked, so normal
        // sub-frame PES-PTS rounding/jitter never accumulates into the
        // anchor. File sources are content-continuous (delta == 0 within a
        // loop), so this only fires on the per-loop splice step.
        const MP_FORWARD_TRACK_DEADBAND_90K: i64 = 450; // 5 ms
        if !self.out_pts_anchored {
            self.out_pts_90k = anchor_target(self.av_sync_pacer.as_ref(), pts);
            self.samples_since_anchor = 0;
            self.av_skew_holdoff = 32;
            self.out_pts_anchored = true;
            // Record the master-clock + source-PTS anchors for the
            // wallclock-aware catch-up below. Sampled here (and only
            // here) so the catch-up's `master_elapsed` and
            // `output_pts_elapsed` reference the same starting point.
            if let Some(pacer) = self.av_sync_pacer.as_ref() {
                self.first_pes_master_27mhz = Some(pacer.now_27mhz());
                self.first_pes_src_pts_90k = Some(pts);
            }
        } else if let Some(expected) = self.expected_next_src_pts_90k {
            let delta = pts.wrapping_sub(expected) as i64;
            // 90 kHz PTS is 33 bits in MPEG-TS; treat the signed delta
            // as bounded — anything outside ±500 ms re-anchors.
            let abs_delta = delta.unsigned_abs();
            if abs_delta > DISCONTINUITY_THRESHOLD_90K {
                // **Monotonicity guard.** Compute where the output
                // is currently sitting (out_pts_90k advanced by
                // samples_since_anchor). If the new anchor candidate
                // (= `pts` per 0.81) would push output PTS *backward*,
                // do NOT re-anchor — leave the existing trajectory
                // alone so output PTS stays monotonically forward.
                // This is the ffmpeg-loop case: the mpegts muxer
                // resets audio PTS at every `-stream_loop` wrap to a
                // value smaller than the last emitted output PTS. The
                // pre-guard behaviour was to slam out_pts_90k backward
                // and start over — receivers saw audio rewind ~3 s
                // per loop and dropped frames. Genuine forward jumps
                // (real source restart, splice insertion) still
                // re-anchor.
                let current_effective_out_pts_90k = if self.resolved_sample_rate > 0 {
                    self.out_pts_90k.wrapping_add(
                        self.samples_since_anchor.saturating_mul(90_000)
                            / self.resolved_sample_rate as u64,
                    )
                } else {
                    self.out_pts_90k
                };
                let candidate = anchor_target(self.av_sync_pacer.as_ref(), pts);
                let forward_delta =
                    (candidate as i64).wrapping_sub(current_effective_out_pts_90k as i64);
                // Suppress ONLY a BACKWARD source jump (`delta < 0`): the
                // ffmpeg `-stream_loop -c copy` muxer resets audio PTS to a
                // smaller value each loop, and re-anchoring there would rewind
                // output PTS by seconds. A FORWARD source jump (`delta > 0` —
                // media_player loop splice / SCTE-35) MUST re-anchor even when
                // the re-encoder has run output slightly ahead of the new
                // source PTS (`forward_delta < 0`): that "ahead" is the
                // silence-pad's per-loop frame-quantization overshoot, and
                // gating the suppress on `forward_delta` (the pre-2026-06-02
                // behaviour) let it accumulate into unbounded A/V drift
                // (~+96 ms/loop measured on a looped AC-3 re-encode; audio
                // crept behind video without bound). Re-anchoring realigns
                // output audio PTS to the source timeline exactly as
                // passthrough does, at the cost of a one-frame backward output
                // step per loop — bounded, vs. the unbounded drift it cures.
                if delta < 0 {
                    tracing::info!(
                        candidate,
                        current_effective_out_pts_90k,
                        backward_delta_90k = forward_delta,
                        delta_90k = delta,
                        "ts_audio_replace: BACKWARD source PTS reset; \
                         suppressing re-anchor to preserve output PTS monotonicity"
                    );
                    // Update expected pointer so subsequent same-
                    // direction PESes don't keep re-triggering the
                    // branch — DON'T touch out_pts_90k / samples.
                    self.expected_next_src_pts_90k = Some(pts);
                } else {
                    tracing::info!(
                        "ts_audio_replace: source PTS discontinuity \
                         (expected={expected}, got={pts}, delta_90k={delta}); \
                         re-anchoring audio output PTS"
                    );
                    self.out_pts_90k = candidate;
                    self.samples_since_anchor = 0;
                    self.av_skew_holdoff = 32; // suppress transient deltas while the pipeline refills
                    // Drop any silence-pad pending for this seam. The
                    // re-anchor already realigns output PTS to the source
                    // timeline; also draining the per-loop gap as silence
                    // frames would double-count it and re-introduce the
                    // overshoot this re-anchor exists to cure.
                    self.pending_silence_27mhz = 0;
                    // Re-anchor the wallclock catch-up too: drop the
                    // accumulated drift from before the discontinuity
                    // so the next lag measurement starts from this
                    // PES. Stale anchors would make the catch-up fire
                    // immediately with a huge lag value derived from
                    // pre-discontinuity history.
                    if let Some(pacer) = self.av_sync_pacer.as_ref() {
                        self.first_pes_master_27mhz = Some(pacer.now_27mhz());
                        self.first_pes_src_pts_90k = Some(pts);
                    }
                }
            } else if self.av_sync_pacer.is_none()
                && delta > MP_FORWARD_TRACK_DEADBAND_90K
            {
                // ── media_player source-PTS forward-step tracking ──
                //
                // The ONLY `audio_encode` path that leaves `av_sync_pacer`
                // unwired is the media_player input transcode (see
                // `input_media_player.rs` — every catch-up config there
                // drifted, so the pacer is intentionally not handed to the
                // transcoder). Its output PTS is therefore a free-running
                // encoder-sample clock that counts only decoded *content*
                // samples. On a looping file whose audio/video content
                // durations differ, the file-side splice advances source
                // PTS by a per-loop step to keep the looped stream
                // PCR-continuous. Passthrough audio carries that step
                // verbatim and stays A/V-locked with the (passthrough or
                // re-encoded) video; the re-encoder's sample clock silently
                // *loses* it, drifting audio against video without bound
                // (measured −26.8 ms/min ≈ −76 ms/loop on the Sky Witness
                // 1080i25 loop; full passthrough of the same file is flat).
                //
                // `delta` here is `pts − (prev_pts + prev_PES_content)`, i.e.
                // the source forward step BEYOND continuous progression —
                // buffer-depth-independent. Track it by advancing the anchor
                // (a pure PTS relabel). We do NOT queue silence: inserting
                // silence would put a >20 ms gap into the decoded PCM
                // (gate 6 violation) and change content; the relabel leaves
                // audio bit-identical and simply reproduces the drift-free
                // passthrough timeline. Forward-only and bounded above by the
                // 500 ms re-anchor branch, so it cannot run away.
                self.out_pts_90k =
                    self.out_pts_90k.wrapping_add(delta as u64) & 0x1_FFFF_FFFF;
                tracing::debug!(
                    expected,
                    got = pts,
                    delta_90k = delta,
                    out_pts_90k = self.out_pts_90k,
                    "ts_audio_replace: media_player source-PTS forward step tracked (anchor relabel)"
                );
            } else if delta > SUB_DISCONTINUITY_THRESHOLD_90K {
                // Sub-500 ms FORWARD jump on a master-clock-paced path
                // (live SRT/RTP/output transcode). Queue the forward delta
                // as silence padding so the encoder produces silence frames
                // that pull `samples_since_anchor` forward by the right
                // amount. PTS catches up in steady state without touching
                // `out_pts_90k`; receivers see continuous output PES at the
                // steady cadence with the gap rendered as silence frames.
                let delta_27mhz = delta.saturating_mul(300);
                self.pending_silence_27mhz = self
                    .pending_silence_27mhz
                    .saturating_add(delta_27mhz);
                tracing::debug!(
                    expected,
                    got = pts,
                    delta_90k = delta,
                    queued_silence_27mhz = delta_27mhz,
                    total_pending_silence_27mhz = self.pending_silence_27mhz,
                    "ts_audio_replace: sub-500ms forward source-PTS jump queued for silence-pad"
                );
            }
        }

        // ── Master-clock-aware audio catch-up ─────────────────────────
        //
        // Some sources (notably `media_player` loops on real broadcast
        // captures) stamp audio PES PTSes continuously across loop
        // boundaries while video PES PTSes carry a per-loop forward
        // jump (because the file-side splice anchor lands at
        // `max(audio_max, pcr_max) + GUARD` and `pcr_max > audio_max`
        // by ~200 ms — audio_max happens to land near the splice
        // anchor while video_max lands further short, leaving a
        // ~268 ms video gap and only ~4.5 ms audio gap per loop). The
        // forward-jump detector above doesn't fire because the audio
        // PES PTS delta stays small; the encoder stamps output PESes
        // at sample-rate cadence; output audio PTS rate ends up
        // slightly below the source/wallclock rate that drives output
        // video PTS. On Sky Witness 170-sec loops the result is
        // ~-27 ms/min audio drift behind passthrough video.
        //
        // The catch-up below compares effective output PTS elapsed
        // since the first PES against master-clock time elapsed
        // since the first PES. When audio falls behind by more than
        // CATCH_UP_THRESHOLD_27M (200 ms), queue the deficit through
        // `pending_silence_27mhz`. The existing silence-pad apply
        // path drains it: silence samples added to the accumulator
        // produce K silence frames at the encoder, each advancing
        // `samples_since_anchor` by `frame_size` — effective PTS
        // climbs back up to wallclock-paced position, lag returns
        // toward zero, the next PES check sees a smaller residual
        // (below threshold) and doesn't re-queue.
        //
        // **Master-clock semantics.** `av_sync_pacer.now_27mhz()`
        // abstracts the per-flow master clock kind:
        // - **wallclock** (default for SRT/RTP/UDP/RIST/RTMP/RTSP/
        //   `media_player`/`replay`): host CLOCK_TAI. The most
        //   common case where this catch-up matters — corrects
        //   per-loop drift caused by source-side splice asymmetry.
        // - **source_pcr_pll** (opt-in via `master_clock.kind =
        //   "contribution"` / `"source_pcr_pll"`): tracks the PLL-
        //   recovered source PCR rate. Source audio samples arrive
        //   at source rate, so `output_pts_elapsed` ≈
        //   `master_elapsed`. Lag stays ~0 and the catch-up is
        //   inert — exactly what we want for PLL-locked
        //   contribution feeds.
        // - **ptp** (auto-selected for ST 2110 + MXL): tracks
        //   `ptp4l` grandmaster. ST 2110 audio is PTP-paced at
        //   source; `output_pts_elapsed` ≈ `master_elapsed`. Lag
        //   stays ~0, inert.
        //
        // Threshold 100 ms — engineering compromise between strict
        // production lip-sync (EBU R37 ±40 ms, ATSC IS-191 -45 ms /
        // +15 ms) and practical silence-pad cost. The catch-up always
        // keeps audio LAGGING (queues silence to advance toward
        // master, never the reverse), so steady-state offset is
        // `[-(threshold + per-loop-step), 0]` — the forgiving side
        // per ATSC IS-191 which permits audio lag up to -45 ms.
        //
        // Why not 40 ms (= strict EBU R37). Each catch-up firing
        // queues silence equal to current lag; the encoder drains in
        // `frame_size`-sample chunks (1024 for AAC-LC, 1152 for MP2),
        // so the IMMEDIATE PTS advance per firing is bounded by one
        // frame (~21 ms at 48 kHz AAC). For the cell 4 case where
        // drift comes in ~30 ms steps at each ~170 sec loop boundary,
        // a 40 ms threshold fires every loop with silence values
        // 42-70 ms, but the encoder catches up ~21 ms per drain
        // round — so the MAX instantaneous drift is bounded by
        // `(threshold + loop_step) ≈ 70 ms` regardless. Tighter
        // threshold (down to ~10 ms) would fire on every PES
        // without improving the practical bound, just multiplying
        // silence-pad artifacts. For strict ±40 ms compliance the
        // architecture needs sub-frame catch-up granularity OR an
        // upstream fix in play_ts_file.
        //
        // Why not 200 ms. Max drift ~230 ms, exceeding common
        // broadcast tolerances (HLS / RTMP / DVB IRD ±100 ms).
        //
        // 100 ms = max drift ~130 ms, firings every ~4 min on Sky
        // Witness, meets HLS/RTMP/DVB tolerances. Production paths
        // using PTP or source_pcr_pll masters don't fire this at
        // all — audio + master + video already track together.
        const CATCH_UP_THRESHOLD_27M: i64 = 2_700_000; // 100 ms
        // Sanity ceiling: a lag > 2 seconds means something is wrong
        // with the master clock or our anchors — the source-PCR PLL
        // may not have locked, the PTP servo may be in transient, or
        // a 33-bit PTS wrap interacted badly with our anchor. Skip
        // catch-up in that case: queueing multi-second silence based
        // on garbage anchor values would mute the output for seconds
        // every PES (which is exactly what an early version of this
        // patch did on cell 6 with source_pcr_pll — observed lag values
        // ran into multi-hour territory during PLL warmup). Once the
        // clocks settle, the next legitimate small-lag catch-up will
        // fire normally.
        const SANITY_CEILING_27M: i64 = 54_000_000; // 2 seconds
        // Gate on `pacer.is_locked()` so the source-PCR PLL has a
        // chance to converge before we trust its `now_27mhz()` values.
        // Wallclock returns `true` immediately; PTP after the servo
        // hits tolerance; source-PCR PLL after lock.
        if let (Some(pacer), Some(first_master_27m), Some(first_src_pts_90k)) = (
            self.av_sync_pacer.as_ref(),
            self.first_pes_master_27mhz,
            self.first_pes_src_pts_90k,
        )
            && self.resolved_sample_rate > 0 && pacer.is_locked() {
                let master_now_27m = pacer.now_27mhz();
                let master_elapsed_27m =
                    master_now_27m.wrapping_sub(first_master_27m) as i64;
                let effective_out_pts_90k = self.out_pts_90k.wrapping_add(
                    self.samples_since_anchor.saturating_mul(90_000)
                        / self.resolved_sample_rate as u64,
                );
                // 33-bit PTS subtraction with wrap.
                let output_pts_elapsed_90k = ((effective_out_pts_90k as i128
                    - first_src_pts_90k as i128)
                    .rem_euclid(1i128 << 33))
                    as u64;
                let output_pts_elapsed_27m = output_pts_elapsed_90k
                    .saturating_mul(300) as i64;
                let lag_27m = master_elapsed_27m
                    .saturating_sub(output_pts_elapsed_27m);
                if lag_27m > CATCH_UP_THRESHOLD_27M
                    && lag_27m < SANITY_CEILING_27M
                {
                    // Per-fire cap = exactly one AC-3 frame duration
                    // (32 ms at 48 kHz, 1536 samples). Larger caps
                    // (e.g. 50 ms) overshoot because the encoder
                    // consumes accumulator in `frame_size` chunks
                    // and will emit 2 frames if silence + real
                    // exceeds 2 × frame_size, advancing
                    // `samples_since_anchor` by 64 ms per fire. By
                    // matching the cap to one frame's worth of
                    // samples, exactly one extra silence frame is
                    // emitted per fire — `samples_since_anchor`
                    // advances by exactly 32 ms above the per-PES
                    // baseline (per fire), which means in
                    // equilibrium with drift rate D ms/sec:
                    //   fire_rate ≈ D / 32 fires/sec
                    //   silence_rate = 32 × fire_rate = D ms/sec
                    // — exactly compensating the drift, so audio
                    // tracks PCR within ±~5 ms after the
                    // `CATCH_UP_THRESHOLD` (100 ms) settling band.
                    // Note: 32 ms is AC-3-specific; for codecs with
                    // different frame_size (Opus 20 ms, MP2 24 ms,
                    // AAC 21.3 ms) a slightly different value would
                    // be optimal, but 32 ms is close enough for the
                    // smaller codecs to still keep audio within
                    // strict broadcast tolerance.
                    const SANITY_CAP_PER_FIRE_27M: i64 = 32 * 27_000;
                    let silence_to_add = lag_27m.min(SANITY_CAP_PER_FIRE_27M);
                    self.pending_silence_27mhz = self
                        .pending_silence_27mhz
                        .saturating_add(silence_to_add);
                    tracing::info!(
                        lag_27m,
                        silence_added_27m = silence_to_add,
                        lag_ms = lag_27m / 27_000,
                        master_elapsed_27m,
                        output_pts_elapsed_27m,
                        threshold_27m = CATCH_UP_THRESHOLD_27M,
                        "ts_audio_replace: master-clock catch-up — output audio PTS has fallen \
                         behind master clock; queueing silence-pad to realign"
                    );
                } else if lag_27m >= SANITY_CEILING_27M {
                    // Log once at debug level — production-quality
                    // installations should never hit this (would
                    // indicate a clock-source pathology); useful for
                    // diagnosing master-clock transients on the
                    // testbed.
                    tracing::debug!(
                        lag_27m,
                        lag_ms = lag_27m / 27_000,
                        master_elapsed_27m,
                        output_pts_elapsed_27m,
                        sanity_ceiling_27m = SANITY_CEILING_27M,
                        "ts_audio_replace: master-clock catch-up suppressed — lag exceeds \
                         sanity ceiling (PLL warmup, PTP servo transient, or PTS wrap)"
                    );
                }
            }

        // ── Phase 1: decode every codec frame in this PES ──
        //
        // AAC (stream_type 0x0F) is decoded in-process via fdk-aac. The
        // ADTS framing lives inside `es_data` itself.
        //
        // MP2 / AC-3 / E-AC-3 (0x03/0x04, 0x80/0x81/0xC1, 0x87/0xC2) are
        // decoded via libavcodec — `engine::audio_decode::split_audio_codec_frames`
        // walks the PES on the codec's sync word, then each access unit
        // is fed to the FFmpeg decoder one at a time
        // (`avcodec_send_packet` decodes only one AU per call).
        struct Decoded {
            planar: Vec<Vec<f32>>,
            sample_rate: u32,
            channels: u8,
        }
        let mut decoded_frames: Vec<Decoded> = Vec::new();

        #[cfg(feature = "fdk-aac")]
        if self.source_stream_type == 0x0F {
            if self.aac_decoder.is_none() {
                self.aac_decoder = Some(
                    aac_audio::AacDecoder::open_adts().map_err(|_| ())?,
                );
            }
            let decoder = self.aac_decoder.as_mut().unwrap();

            let mut pos = 0;
            while pos + 7 <= es_data.len() {
                if es_data[pos] != 0xFF || (es_data[pos + 1] & 0xF0) != 0xF0 {
                    break;
                }
                let protection_absent = (es_data[pos + 1] & 0x01) != 0;
                let header_len = if protection_absent { 7 } else { 9 };
                if pos + header_len > es_data.len() {
                    break;
                }
                let frame_len = (((es_data[pos + 3] & 0x03) as usize) << 11)
                    | ((es_data[pos + 4] as usize) << 3)
                    | ((es_data[pos + 5] as usize) >> 5);
                if frame_len < header_len || pos + frame_len > es_data.len() {
                    break;
                }
                let adts = &es_data[pos..pos + frame_len];
                pos += frame_len;

                self.decode_stats.inc_input();
                match decoder.decode_frame(adts) {
                    Ok(d) => {
                        self.decode_stats.inc_output();
                        decoded_frames.push(Decoded {
                            planar: d.planar,
                            sample_rate: decoder.sample_rate().unwrap_or(48_000),
                            channels: decoder.channels().unwrap_or(2),
                        });
                    }
                    Err(_) => {
                        self.decode_stats.inc_error();
                        continue;
                    }
                }
            }
        }

        #[cfg(feature = "media-codecs")]
        if let Some(ff_codec) = crate::engine::audio_decode::ff_codec_for_stream_type(
            self.source_stream_type,
        ) {
            if self.ff_decoder.is_none() {
                self.ff_decoder = Some(
                    video_engine::AudioDecoder::open(ff_codec).map_err(|_| ())?,
                );
            }
            let decoder = self.ff_decoder.as_mut().unwrap();
            for au in
                crate::engine::audio_decode::split_audio_codec_frames(&es_data, ff_codec)
            {
                self.decode_stats.inc_input();
                if decoder.send_packet(au, pts as i64).is_err() {
                    self.decode_stats.inc_error();
                    continue;
                }
                while let Ok(frame) = decoder.receive_frame() {
                    self.decode_stats.inc_output();
                    decoded_frames.push(Decoded {
                        planar: frame.planar,
                        sample_rate: frame.sample_rate,
                        channels: frame.channels,
                    });
                }
            }
        }

        if decoded_frames.is_empty()
            && self.source_stream_type != 0
            && !source_replaceable(self.source_stream_type)
        {
            // Source codec is something other than the four we decode
            // (e.g. SMPTE 302M `0x06` LPCM). Nothing to re-encode.
            return Err(());
        }

        // Source-rate sample accounting for the discontinuity guard.
        // The first decoded frame in this PES carries the canonical
        // source sample rate; we sum `d.planar[0].len()` across every
        // frame so that the "expected next source PTS" we stamp at end
        // of PES reflects the actual audio time-span covered, even when
        // libavcodec splits one syncframe into multiple decoded frames
        // or when the source PES holds back-to-back AUs.
        let source_sample_rate_hint: u32 = decoded_frames
            .first()
            .map(|d| d.sample_rate)
            .unwrap_or(0);
        let mut source_samples_in_pes: u64 = 0;

        // ── Phase 2: feed PCM into encoder ──
        {
            for d in decoded_frames {
                source_samples_in_pes =
                    source_samples_in_pes.saturating_add(
                        d.planar.first().map(|c| c.len() as u64).unwrap_or(0),
                    );
                if !self.codecs_ready {
                    // Resolve the encoder target. When a transcode block is
                    // present it wins: build the planar transcoder first and
                    // let its output format drive the encoder. Any
                    // audio_encode.sample_rate / channels fields fold in as
                    // fallbacks for fields the transcode block leaves unset.
                    if let Some(ref tj_in) = self.transcode_cfg {
                        let merged = TranscodeJson {
                            sample_rate: tj_in.sample_rate.or(self.sample_rate_override),
                            channels: tj_in.channels.or(self.channels_override),
                            ..tj_in.clone()
                        };
                        match PlanarAudioTranscoder::new(
                            d.sample_rate,
                            d.channels,
                            &merged,
                        ) {
                            Ok(tc) => {
                                self.resolved_sample_rate = tc.out_sample_rate();
                                self.resolved_channels = tc.out_channels();
                                self.transcoder = Some(tc);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "TsAudioReplacer: transcode init failed ({e}); \
                                     dropping this PES"
                                );
                                return Err(());
                            }
                        }
                    } else {
                        self.resolved_sample_rate =
                            self.sample_rate_override.unwrap_or(d.sample_rate);
                        self.resolved_channels =
                            self.channels_override.unwrap_or(d.channels);
                    }
                    self.accumulator =
                        vec![Vec::new(); self.resolved_channels as usize];
                    self.init_encoder()?;
                    self.codecs_ready = true;
                    self.refresh_decode_stats_label();
                }
                // Apply transcode if present; otherwise forward source PCM
                // directly. Both paths produce planar f32 at
                // `resolved_channels` channels.
                let shuffled_planar: Vec<Vec<f32>> =
                    if let Some(ref mut tc) = self.transcoder {
                        match tc.process(&d.planar) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!(
                                    "TsAudioReplacer: transcode process failed ({e}); \
                                     dropping frame"
                                );
                                continue;
                            }
                        }
                    } else {
                        d.planar
                    };
                // Silence-pad in lockstep with the next source samples
                // so the gap appears at the right PTS in the output
                // stream. See `apply_pending_silence_pad` for the
                // full rationale — silence-content-only, no
                // `out_pts_90k` nudge. Cap at 5 s per consume to
                // bound worst-case pathological inputs.
                self.apply_pending_silence_pad();
                for ch in 0..self.resolved_channels as usize {
                    if ch < shuffled_planar.len() {
                        self.accumulator[ch]
                            .extend_from_slice(&shuffled_planar[ch]);
                    } else if !shuffled_planar.is_empty() {
                        self.accumulator[ch].extend(
                            std::iter::repeat_n(0.0f32, shuffled_planar[0].len()),
                        );
                    }
                }
                // No per-PES PTS queue — output PTS is recomputed from
                // the anchor + `samples_since_anchor` for every emitted
                // frame, so encoder buffering and decoder priming can't
                // produce duplicate or shifted PTSes the way the FIFO
                // model used to.
                self.drain_encoder(output)?;
            }
        }

        // Stamp the expected next source PTS so the next PES can detect
        // a > 500 ms discontinuity. Three cases:
        //
        // 1. Decode succeeded — advance by the audio time actually carried
        //    in this PES.
        // 2. First PES failed to decode — leave anchor unset so the next
        //    PES re-anchors instead of treating its PTS as a jump from a
        //    stale value.
        // 3. **Already-anchored PES failed to decode** — advance to *this*
        //    PES's pts so subsequent PESes are measured against the most
        //    recent observed timestamp, not the pre-failure projection.
        //    Without case 3, consecutive decode failures (e.g., when the
        //    ffmpeg `-stream_loop` wrap emits a few malformed AC-3 bytes
        //    that don't pass `split_ac3_frames`) leave `expected_next`
        //    frozen far in the past; the next successful PES then sees a
        //    huge synthetic forward delta and fires a phantom
        //    discontinuity, re-anchoring `out_pts_90k` forward by hundreds
        //    of milliseconds. Stack a few of those across one loop wrap
        //    and the output audio PTS races ahead of the passthrough
        //    video PTS lineage — the AV-drift-after-loop symptom on
        //    `srt-plain-9000` + `-stream_loop -1 -c copy` sources.
        if source_sample_rate_hint > 0 && source_samples_in_pes > 0 {
            let span_90k =
                source_samples_in_pes.saturating_mul(90_000)
                    / source_sample_rate_hint as u64;
            self.expected_next_src_pts_90k =
                Some(pts.wrapping_add(span_90k));
        } else if !self.out_pts_anchored {
            self.expected_next_src_pts_90k = None;
        } else {
            self.expected_next_src_pts_90k = Some(pts);
        }
        // Publish the edge-added audio-path skew. This point is AFTER
        // the PES's content was decoded + encoded + drained, so
        // `samples_since_anchor` sits at the END of this PES's content —
        // compare end-position to end-position: output side
        // `out_pts + samples_since_anchor/rate` vs source side
        // `pts + samples_in_pes/source_rate`. (Comparing against the
        // PES START pts overstated the skew by one PES span — caught
        // live 2026-06-06: reported +96 ms while the decoded flash/beep
        // truth was +9 ms.) Residual bias = decode→encode in-flight +
        // encoder priming, bounded by ~1 encoder frame. Wrap-aware
        // 33-bit signed difference.
        // Output side counts emitted samples PLUS the partial still in
        // the PCM accumulator (without it, mixed source/target frame
        // sizes sawtooth the delta by up to one target frame). PTS-less
        // PES (legal; pts==0 sentinel) are skipped. The holdoff
        // suppresses pipeline-refill transients after (re)anchors.
        if let Some(reporter) = self.av_skew.as_ref() {
            if self.av_skew_holdoff > 0 {
                self.av_skew_holdoff -= 1;
            } else if self.out_pts_anchored
                && self.resolved_sample_rate > 0
                && source_sample_rate_hint > 0
                && pts != 0
            {
                let pending = self
                    .accumulator
                    .first()
                    .map(|c| c.len() as u64)
                    .unwrap_or(0);
                let effective_out = self.out_pts_90k.wrapping_add(
                    self.samples_since_anchor
                        .saturating_add(pending)
                        .saturating_mul(90_000)
                        / self.resolved_sample_rate as u64,
                ) & 0x1_FFFF_FFFF;
                let src_end = pts.wrapping_add(
                    source_samples_in_pes.saturating_mul(90_000)
                        / source_sample_rate_hint as u64,
                ) & 0x1_FFFF_FFFF;
                let mut delta =
                    (effective_out.wrapping_sub(src_end) & 0x1_FFFF_FFFF) as i64;
                if delta >= (1 << 32) {
                    delta -= 1 << 33;
                }
                reporter.set_audio_delta(delta);
            }
        }
        let _ = pts;
        Ok(())
    }

    /// Open the target encoder, using the source sample-rate / channels
    /// (plus overrides) fixed by the first successful decode.
    fn init_encoder(&mut self) -> Result<(), ()> {
        let target_sr = self.resolved_sample_rate;
        let target_ch = self.resolved_channels;

        match self.codec {
            AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => {
                #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
                {
                    let profile = match self.codec {
                        AudioCodec::AacLc => aac_codec::AacProfile::AacLc,
                        AudioCodec::HeAacV1 => aac_codec::AacProfile::HeAacV1,
                        AudioCodec::HeAacV2 => aac_codec::AacProfile::HeAacV2,
                        _ => unreachable!(),
                    };
                    let cfg = aac_codec::EncoderConfig {
                        profile,
                        sample_rate: target_sr,
                        channels: target_ch,
                        bitrate: self.bitrate_kbps * 1000,
                        afterburner: true,
                        sbr_signaling: aac_codec::SbrSignaling::default(),
                        transport: aac_codec::TransportType::Adts,
                    };
                    self.aac_encoder = Some(
                        aac_audio::AacEncoder::open(&cfg).map_err(|_| ())?,
                    );
                    Ok(())
                }
                #[cfg(not(all(feature = "media-codecs", feature = "fdk-aac")))]
                {
                    return Err(());
                }
            }
            AudioCodec::Mp2 | AudioCodec::Ac3 => {
                #[cfg(feature = "media-codecs")]
                {
                    let codec_type = match self.codec {
                        AudioCodec::Mp2 => video_codec::AudioCodecType::Mp2,
                        AudioCodec::Ac3 => video_codec::AudioCodecType::Ac3,
                        _ => unreachable!(),
                    };
                    let cfg = video_codec::AudioEncoderConfig {
                        codec: codec_type,
                        sample_rate: target_sr,
                        channels: target_ch,
                        bitrate_kbps: self.bitrate_kbps,
                    };
                    self.av_encoder =
                        Some(video_engine::AudioEncoder::open(&cfg).map_err(|_| ())?);
                    Ok(())
                }
                #[cfg(not(feature = "media-codecs"))]
                {
                    return Err(());
                }
            }
            AudioCodec::Opus => Err(()),
        }
    }

    /// Pull as many encoded frames as the encoder has ready given the
    /// current PCM accumulator, re-packetize them as TS, and emit.
    fn drain_encoder(&mut self, output: &mut Vec<u8>) -> Result<(), ()> {
        let audio_pid = match self.audio_pid {
            // Encoded packets always ride the source PID. Any operator
            // PID rename lands on the downstream `TsPidOverridesRewriter`
            // stage; the replacer's PMT rewrite advertises the source PID
            // so the rewriter can match and rename consistently.
            Some(p) => p,
            None => return Err(()),
        };

        // AAC branch — fdk-aac encoder.
        #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
        {
            if let Some(ref mut enc) = self.aac_encoder {
                let frame_size = enc.frame_size() as usize;
                while self
                    .accumulator
                    .first()
                    .map(|c| c.len())
                    .unwrap_or(0)
                    >= frame_size
                {
                    let frame: Vec<Vec<f32>> = self
                        .accumulator
                        .iter_mut()
                        .map(|ch| ch.drain(..frame_size).collect())
                        .collect();
                    self.encode_stats.inc_submitted();
                    match enc.encode_frame(&frame) {
                        Ok(encoded) => {
                            let sr = self.resolved_sample_rate;
                            let pts_for_pes = if sr == 0 {
                                self.out_pts_90k
                            } else {
                                self.out_pts_90k.wrapping_add(
                                    self.samples_since_anchor.saturating_mul(90_000)
                                        / sr as u64,
                                )
                            };
                            let pes = build_audio_pes(
                                self.codec.ts_pes_stream_id(),
                                &encoded.bytes,
                                pts_for_pes,
                            );
                            let pkts = packetize_ts(audio_pid, &pes, &mut self.out_audio_cc);
                            for p in &pkts {
                                output.extend_from_slice(p);
                            }
                            self.encode_stats.inc_out(1);
                            self.samples_since_anchor = self
                                .samples_since_anchor
                                .saturating_add(encoded.num_samples as u64);
                        }
                        Err(_) => {
                            self.encode_stats.inc_dropped();
                        }
                    }
                }
                return Ok(());
            }
        }

        // MP2 / AC-3 branch — libavcodec via video-engine.
        #[cfg(feature = "media-codecs")]
        {
            if let Some(ref mut enc) = self.av_encoder {
                let frame_size = enc.frame_size();
                while self
                    .accumulator
                    .first()
                    .map(|c| c.len())
                    .unwrap_or(0)
                    >= frame_size
                {
                    let frame: Vec<Vec<f32>> = self
                        .accumulator
                        .iter_mut()
                        .map(|ch| ch.drain(..frame_size).collect())
                        .collect();
                    self.encode_stats.inc_submitted();
                    match enc.encode_frame(&frame) {
                        Ok(frames) => {
                            let sr = enc.sample_rate();
                            for ef in frames {
                                let pts_for_pes = if sr == 0 {
                                    self.out_pts_90k
                                } else {
                                    self.out_pts_90k.wrapping_add(
                                        self.samples_since_anchor.saturating_mul(90_000)
                                            / sr as u64,
                                    )
                                };
                                let pes = build_audio_pes(
                                    self.codec.ts_pes_stream_id(),
                                    &ef.data,
                                    pts_for_pes,
                                );
                                let pkts =
                                    packetize_ts(audio_pid, &pes, &mut self.out_audio_cc);
                                for p in &pkts {
                                    output.extend_from_slice(p);
                                }
                                self.encode_stats.inc_out(1);
                                self.samples_since_anchor = self
                                    .samples_since_anchor
                                    .saturating_add(ef.num_samples as u64);
                            }
                        }
                        Err(_) => {
                            self.encode_stats.inc_dropped();
                        }
                    }
                }
                return Ok(());
            }
        }

        Err(())
    }
}

// ────────────────────────── TS / PES helpers ──────────────────────────

/// True when the source `stream_type` is one we know how to decode and
/// re-encode: AAC ADTS (0x0F) via fdk-aac, AAC LATM (0x11) /
/// MP2 / AC-3 / E-AC-3 via the FFmpeg-backed audio decoder.
///
/// DVB-style audio with `stream_type = 0x06` (`private_data`) is
/// resolved via [`resolve_private_audio_stream_type`] in
/// [`select_audio_es`] *before* this gate fires, so e.g. a DVB AC-3
/// stream (0x06 + descriptor 0x6A) arrives here as the synthesised
/// 0x81 and lights up exactly like its ATSC sibling.
///
/// Anything else falls through to passthrough (no PMT rewrite, audio
/// bytes preserved).
fn source_replaceable(stream_type: u8) -> bool {
    matches!(
        stream_type,
        0x0F | 0x11 | 0x03 | 0x04 | 0x80 | 0x81 | 0x87 | 0xC1 | 0xC2,
    )
}

/// Walk the ES-info descriptor loop after a `stream_type = 0x06`
/// (private_data) entry and synthesise the ATSC-style codec stream_type
/// that the rest of the replacer already handles. Recognised:
///
/// - DVB AC-3 descriptor (tag `0x6A`, ETSI TS 101 154 § 5.3) → `0x81`
/// - DVB Enhanced AC-3 descriptor (tag `0x7A`) → `0x87`
/// - DVB AAC descriptor (tag `0x7C`) → `0x11` (LATM/LOAS — the
///   broadcast carriage form; ATSC ADTS-via-private is vanishingly
///   rare so we default to LATM and let `split_audio_codec_frames`
///   dispatch through the LOAS path)
/// - `registration_descriptor` (tag `0x05`) with `format_identifier`
///   `"AC-3"` / `"EAC3"`
///
/// Returns `None` for any other private stream (Opus, AC-4, DTS,
/// SMPTE 302M, …) — the caller keeps the raw `0x06` and downstream
/// paths handle it (Opus has its own arm in
/// [`crate::engine::audio_decode::ff_codec_for_stream_type`]; AC-4 /
/// DTS fall through to passthrough). The replacer must only be handed
/// codecs it can actually decode, which is why the broader kinds the
/// shared classifier recognises are deliberately collapsed to `None`
/// here. DVB AC-3 carriage and the ATSC equivalent thus collapse onto
/// one code path — operators see the same transcoding behaviour
/// whether the source comes from Europe / Australia (DVB) or North
/// America (ATSC).
///
/// Descriptor walking itself is the shared
/// [`crate::engine::ts_parse::descriptor_audio_kind`] — the same logic
/// that drives muxer-mode PES re-anchoring (`ts_pts_rewriter`), the
/// singular `audio_pid` override binding, and the A/V drift metric, so
/// all four surfaces agree on what counts as DVB private audio.
fn resolve_private_audio_stream_type(descriptors: &[u8]) -> Option<u8> {
    use crate::engine::ts_parse::{descriptor_audio_kind, PrivateEsAudioKind};
    match descriptor_audio_kind(descriptors)? {
        PrivateEsAudioKind::Ac3 => Some(0x81),
        PrivateEsAudioKind::Eac3 => Some(0x87),
        PrivateEsAudioKind::AacLatm => Some(0x11),
        // Not decodable by the replacer — keep raw 0x06 / passthrough.
        PrivateEsAudioKind::Dts
        | PrivateEsAudioKind::Opus
        | PrivateEsAudioKind::Smpte302m
        | PrivateEsAudioKind::Ac4 => None,
    }
}

/// How one PMT ES entry relates to this replacer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioClass {
    /// Audio we can decode; carries the (DVB-resolved) stream_type the rest
    /// of the replacer keys on.
    Replaceable(u8),
    /// Audio, but a codec the replacer cannot decode (DTS, Opus, AC-4,
    /// SMPTE 302M, MPEG-4 raw audio, TrueHD, …).
    Unsupported,
    NotAudio,
}

fn classify_audio_entry(stream_type: u8, es_info: &[u8]) -> AudioClass {
    match stream_type {
        0x06 => match resolve_private_audio_stream_type(es_info) {
            Some(st) => AudioClass::Replaceable(st),
            None if crate::engine::ts_parse::descriptor_audio_kind(es_info).is_some() => {
                AudioClass::Unsupported
            }
            None => AudioClass::NotAudio,
        },
        0x0F | 0x11 | 0x03 | 0x04 | 0x80 | 0x81 | 0x87 | 0xC1 | 0xC2 => {
            AudioClass::Replaceable(stream_type)
        }
        0x1C | 0x82..=0x85 | 0x88 | 0xA1 | 0xA2 => AudioClass::Unsupported,
        _ => AudioClass::NotAudio,
    }
}

/// The audio ES this replacer should lock onto, plus what the engage
/// watchdog needs to explain a miss.
#[derive(Debug, Default)]
struct AudioSelection {
    /// `(audio_pid, resolved stream_type)`.
    chosen: Option<(u16, u8)>,
    /// The program carries audio, but none of it decodable.
    unsupported_candidate: bool,
    /// Every ES entry: `(pid, stream_type)`.
    es: Vec<(u16, u8)>,
}

/// Select the audio ES to re-encode from a parsed PMT.
///
/// `pinned_pid`:
/// - `Some(pid)` — the operator pinned a specific source PID via
///   `audio_encode.source_audio_pid`. Return it (with its resolved
///   stream_type) when it carries a decodable codec. If the pinned PID is
///   missing OR carries an unsupported codec, fall through to the
///   first-match behaviour; the caller raises `audio_source_pid_not_found`.
/// - `None` — first-match: the first ES that is decodable audio
///   (AAC 0x0F / LATM 0x11, MPEG-1/2 0x03 / 0x04, AC-3 0x80 / 0x81 / 0xC1,
///   E-AC-3 0x87 / 0xC2, or DVB-style 0x06 resolved by its descriptor).
fn select_audio_es(view: &PmtView<'_>, pinned_pid: Option<u16>) -> AudioSelection {
    let mut sel = AudioSelection::default();
    let mut first: Option<(u16, u8)> = None;
    let mut pinned_hit: Option<(u16, u8)> = None;
    for es in &view.es {
        sel.es.push((es.pid, es.stream_type));
        match classify_audio_entry(es.stream_type, view.es_info(es)) {
            AudioClass::Replaceable(st) => {
                if first.is_none() {
                    first = Some((es.pid, st));
                }
                if pinned_pid == Some(es.pid) {
                    pinned_hit = Some((es.pid, st));
                }
            }
            AudioClass::Unsupported => sel.unsupported_candidate = true,
            AudioClass::NotAudio => {}
        }
    }
    sel.chosen = pinned_hit.or(first);
    sel
}

/// Audio output PTS anchor target. **Always returns `src_pts`.**
///
/// The earlier master-clock anchor was REMOVED — it was a layering
/// violation that caused audio to be **double-anchored** when the
/// per-input `ts_pts_rewriter` (default in muxer mode) also runs on
/// the same bytes. The rewriter takes the audio replacer's master-
/// anchored PTS, treats it as `src_pts`, and adds ANOTHER anchor on
/// top — producing audio PTS values wildly different from video,
/// breaking A/V sync at the receiver (measured: ~43 000 second A-V
/// delta in live testbed capture).
///
/// Correct architecture: **one anchor per pipeline.** The
/// `ts_pts_rewriter` (or assembler-side rewriter in PID-bus flows)
/// owns ALL master-clock anchoring at the byte level. The audio
/// replacer just emits PES with source-relative PTS; the rewriter
/// downstream applies the shared master anchor to ALL PIDs uniformly,
/// keeping PCR / video PTS / audio PTS / SCTE-35 pts_time all on the
/// same timeline.
///
/// `pacer` is retained on the signature for API stability; it is no
/// longer dereferenced.
fn anchor_target(
    _pacer: Option<&Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    src_pts: u64,
) -> u64 {
    src_pts
}

/// Extract the elementary-stream payload and PTS (90 kHz) from a complete
/// PES packet. Returns `None` on malformed input.
fn extract_pes_audio(pes: &[u8]) -> Option<(Vec<u8>, u64)> {
    if pes.len() < 9 || pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
        return None;
    }
    let header_data_len = pes[8] as usize;
    let es_start = 9 + header_data_len;
    if es_start >= pes.len() {
        return None;
    }
    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    let pts = if pts_dts_flags >= 2 && pes.len() >= 14 {
        parse_pts(&pes[9..14])
    } else {
        0
    };
    Some((pes[es_start..].to_vec(), pts))
}

/// Decode the 5-byte PTS / DTS in a PES optional header per ISO/IEC
/// 13818-1 §2.4.3.7. See `ts_video_replace::parse_pts` for the bit-by-
/// bit layout — this is the same parser kept in the audio module to
/// avoid a cross-module dependency for one helper.
fn parse_pts(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;
    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) & 0x7F) << 15
        | (b3 << 7)
        | ((b4 >> 1) & 0x7F)
}

/// Wrap an encoded audio frame in a PES packet with a PTS header.
///
/// The five-byte PTS encoding is spec-compliant per ISO/IEC 13818-1
/// §2.4.3.7 — pts bits 32, 30, and 15 land in the right slots. An
/// earlier version of this routine had off-by-one shift bugs that
/// silently dropped pts bits 30 and 15 in the encoded output, which
/// caused standard receivers (Appear, VLC, ffmpeg) to lose audio PTS
/// lock once `pts` exceeded 32 768 ticks (~ 364 ms at 90 kHz).
fn build_audio_pes(stream_id: u8, audio_data: &[u8], pts: u64) -> Vec<u8> {
    let pes_len = 3 + 5 + audio_data.len();
    let mut pes = Vec::with_capacity(14 + audio_data.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    // 0xC0 (MPEG audio) for MP2 / AAC, 0xBD (private_stream_1) for AC-3 —
    // see `AudioCodec::ts_pes_stream_id`.
    pes.push(stream_id);
    pes.extend_from_slice(&(pes_len as u16).to_be_bytes());
    // Marker bits '10' + data_alignment_indicator=1. Each encoded
    // audio frame (one ADTS frame for AAC, one MP2/AC-3 frame) is
    // emitted as its own PES, so the payload starts at an access-unit
    // boundary. ETSI TS 101 154 §C.4 requires this for broadcast.
    pes.push(0x84);
    pes.push(0x80); // PTS present, no DTS
    pes.push(5);    // PES header data length

    let pts = pts & 0x1_FFFF_FFFF;
    // 0x20 = '0010' marker for PTS-only timestamp role; OR in the top
    // 3 bits of pts (pts[32..30] in result bits 3..1) and the trailing
    // marker bit '1' at bit 0.
    pes.push(0x20 | (((pts >> 29) as u8) & 0x0E) | 0x01);
    pes.push(((pts >> 22) & 0xFF) as u8);
    pes.push((((pts >> 14) as u8) & 0xFE) | 0x01);
    pes.push(((pts >> 7) & 0xFF) as u8);
    pes.push((((pts << 1) as u8) & 0xFE) | 0x01);

    pes.extend_from_slice(audio_data);
    pes
}

/// Packetize a PES payload into one or more 188-byte TS packets with the
/// given PID, advancing the caller's continuity-counter.
fn packetize_ts(pid: u16, pes: &[u8], cc: &mut u8) -> Vec<[u8; 188]> {
    let mut packets = Vec::new();
    let mut offset = 0;
    let mut is_first = true;

    while offset < pes.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];

        let pusi: u8 = if is_first { 1 } else { 0 };
        let current_cc = *cc;
        *cc = (*cc + 1) & 0x0F;

        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = (pusi << 6) | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = pid as u8;

        let remaining = pes.len() - offset;
        let payload_capacity = TS_PACKET_SIZE - 4;

        if remaining >= payload_capacity {
            pkt[3] = 0x10 | current_cc;
            pkt[4..TS_PACKET_SIZE]
                .copy_from_slice(&pes[offset..offset + payload_capacity]);
            offset += payload_capacity;
        } else {
            let stuff_len = payload_capacity - remaining;
            if stuff_len == 1 {
                pkt[3] = 0x30 | current_cc;
                pkt[4] = 0;
                pkt[5..5 + remaining].copy_from_slice(&pes[offset..]);
            } else {
                pkt[3] = 0x30 | current_cc;
                pkt[4] = (stuff_len - 1) as u8;
                if stuff_len > 1 {
                    pkt[5] = 0x00;
                    for i in 6..4 + stuff_len {
                        pkt[i] = 0xFF;
                    }
                }
                pkt[4 + stuff_len..4 + stuff_len + remaining]
                    .copy_from_slice(&pes[offset..]);
            }
            offset += remaining;
        }

        is_first = false;
        packets.push(pkt);
    }

    packets
}

/// Test builders for other modules' chain tests: an MPEG audio PES (stream
/// id 0xC0) and its TS packetisation.
#[cfg(test)]
pub(crate) fn test_build_audio_pes(es: &[u8], pts: u64) -> Vec<u8> {
    build_audio_pes(0xC0, es, pts)
}

#[cfg(test)]
pub(crate) fn test_packetize(pid: u16, pes: &[u8], cc: &mut u8) -> Vec<[u8; 188]> {
    packetize_ts(pid, pes, cc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::AudioEncodeConfig;
    use crate::engine::ts_parse::mpeg2_crc32;
    use crate::engine::ts_pmt_edit::is_pmt_for;

    fn enc(codec: &str) -> AudioEncodeConfig {
        AudioEncodeConfig {
            codec: codec.into(),
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
        }
    }

    #[test]
    fn rejects_opus_because_no_ts_mapping() {
        assert!(matches!(
            TsAudioReplacer::new(&enc("opus"), None),
            Err(TsAudioReplaceError::UnsupportedCodec(_))
        ));
    }

    #[test]
    fn rejects_unknown_codec() {
        assert!(matches!(
            TsAudioReplacer::new(&enc("flac"), None),
            Err(TsAudioReplaceError::UnknownCodec(_))
        ));
    }

    #[test]
    fn accepts_aac_lc_and_mp2_and_ac3() {
        assert!(TsAudioReplacer::new(&enc("aac_lc"), None).is_ok());
        assert!(TsAudioReplacer::new(&enc("mp2"), None).is_ok());
        assert!(TsAudioReplacer::new(&enc("ac3"), None).is_ok());
    }

    #[test]
    fn process_empty_input_is_noop() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn process_misaligned_input_is_passthrough() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        let input = vec![0x00u8; 100]; // not 188-aligned
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn process_unknown_pid_passes_through_verbatim() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        // PID 0x1FFF (null) — not the PAT, not a PMT, not audio.
        pkt[1] = 0x1F;
        pkt[2] = 0xFF;
        pkt[3] = 0x10; // payload only, CC=0

        let mut out = Vec::new();
        r.process(&pkt, &mut out);
        assert_eq!(&out[..], &pkt[..]);
    }

    #[test]
    fn packetize_ts_round_trip_single_packet() {
        let pes = vec![0xABu8; 100]; // fits in one TS payload
        let mut cc = 0u8;
        let pkts = packetize_ts(0x100, &pes, &mut cc);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0][0], TS_SYNC_BYTE);
        // PUSI set, PID high bits = 0x01
        assert_eq!(pkts[0][1] & 0x40, 0x40);
        assert_eq!(cc, 1);
    }

    /// Build a single-program PAT TS packet (PMT at `pmt_pid`).
    fn synth_pat(pmt_pid: u16) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, pid hi=0
        pkt[2] = 0x00;
        pkt[3] = 0x10; // payload only, CC=0
        pkt[4] = 0x00; // pointer
        let s = 5;
        pkt[s] = 0x00; // table_id = PAT
        let section_length: u16 = 13; // txid(2)+vsn(1)+sec#(1)+last#(1) + entry(4) + CRC(4)
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00;
        pkt[s + 4] = 0x01;
        pkt[s + 5] = 0xC1;
        pkt[s + 6] = 0x00;
        pkt[s + 7] = 0x00;
        pkt[s + 8] = 0x00;
        pkt[s + 9] = 0x01;
        pkt[s + 10] = 0xE0 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[s + 11] = pmt_pid as u8;
        let crc = mpeg2_crc32(&pkt[s..s + 12]);
        pkt[s + 12] = (crc >> 24) as u8;
        pkt[s + 13] = (crc >> 16) as u8;
        pkt[s + 14] = (crc >> 8) as u8;
        pkt[s + 15] = crc as u8;
        pkt
    }

    /// Build a minimal PMT TS packet with exactly one audio ES entry.
    fn synth_pmt_audio(pmt_pid: u16, audio_pid: u16, stream_type: u8) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = pmt_pid as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let s = 5;
        pkt[s] = 0x02; // table_id = PMT
        let section_length: u16 = 18;
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00;
        pkt[s + 4] = 0x01;
        pkt[s + 5] = 0xC1;
        pkt[s + 6] = 0x00;
        pkt[s + 7] = 0x00;
        pkt[s + 8] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F); // PCR_PID = audio_pid
        pkt[s + 9] = audio_pid as u8;
        pkt[s + 10] = 0xF0;
        pkt[s + 11] = 0x00;
        pkt[s + 12] = stream_type;
        pkt[s + 13] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 14] = audio_pid as u8;
        pkt[s + 15] = 0xF0;
        pkt[s + 16] = 0x00;
        let crc = mpeg2_crc32(&pkt[s..s + 17]);
        pkt[s + 17] = (crc >> 24) as u8;
        pkt[s + 18] = (crc >> 16) as u8;
        pkt[s + 19] = (crc >> 8) as u8;
        pkt[s + 20] = crc as u8;
        pkt
    }

    /// Packet-level wrapper over [`select_audio_es`] for the synth helpers:
    /// locate the PMT section with the shared walker, parse, select.
    fn parse_pmt_audio(pkt: &[u8], pinned: Option<u16>) -> Option<(u16, u8)> {
        let s = crate::engine::ts_parse::find_section_in_packet(pkt, 0x02, None)?;
        let view = parse_pmt(&pkt[s.start..s.end()])?;
        select_audio_es(&view, pinned).chosen
    }

    #[test]
    fn synth_pat_round_trips_through_parser() {
        let pkt = synth_pat(0x1000);
        assert_eq!(parse_pat_programs(&pkt), vec![(1u16, 0x1000u16)]);
    }

    #[test]
    fn synth_pmt_round_trips_through_parser() {
        // AAC (0x0F)
        let pkt = synth_pmt_audio(0x1000, 0x0101, 0x0F);
        assert_eq!(parse_pmt_audio(&pkt, None), Some((0x0101, 0x0F)));
        // AC-3 (0x81)
        let pkt = synth_pmt_audio(0x1000, 0x0101, 0x81);
        assert_eq!(parse_pmt_audio(&pkt, None), Some((0x0101, 0x81)));
    }

    // ── DVB private-stream (0x06) descriptor resolution ──
    //
    // ffmpeg's `mpegts` muxer and every real DVB-T/T2/S/S2/C broadcast
    // ships AC-3 / E-AC-3 / AAC as `stream_type = 0x06` with a codec
    // descriptor (0x6A / 0x7A / 0x7C) in the ES_info loop. ATSC ships
    // direct stream_types (0x81 / 0x87 / 0x0F). Without descriptor
    // resolution the audio replacer hits the `source_replaceable` gate
    // on 0x06, gives up, and emits source audio passthrough — the
    // operator sees an "OK" transcoded flow that didn't actually
    // transcode. These tests lock in that DVB-style PMTs route to the
    // same codec path as their ATSC equivalents.

    /// Build a PMT TS packet with one audio ES at stream_type 0x06 plus
    /// a single descriptor (caller-supplied body bytes already include
    /// tag + length octets). `desc_bytes` is appended verbatim into the
    /// ES_info loop.
    fn synth_pmt_private_audio(
        pmt_pid: u16,
        audio_pid: u16,
        desc_bytes: &[u8],
    ) -> [u8; 188] {
        assert!(desc_bytes.len() <= 0x0F_FF, "descriptor loop too large for the synth helper");
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = pmt_pid as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let s = 5;
        pkt[s] = 0x02; // table_id = PMT
        // 9 (PMT header after section_length) + 5 (ES fixed) + desc_len + 4 (CRC)
        let section_length: u16 = 9 + 5 + desc_bytes.len() as u16 + 4;
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00;
        pkt[s + 4] = 0x01;
        pkt[s + 5] = 0xC1;
        pkt[s + 6] = 0x00;
        pkt[s + 7] = 0x00;
        pkt[s + 8] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 9] = audio_pid as u8;
        pkt[s + 10] = 0xF0;
        pkt[s + 11] = 0x00; // program_info_length = 0
        // ES entry
        pkt[s + 12] = 0x06; // stream_type = private data
        pkt[s + 13] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 14] = audio_pid as u8;
        pkt[s + 15] = 0xF0 | ((desc_bytes.len() >> 8) as u8 & 0x0F);
        pkt[s + 16] = desc_bytes.len() as u8;
        pkt[s + 17..s + 17 + desc_bytes.len()].copy_from_slice(desc_bytes);
        let crc_in_end = s + 17 + desc_bytes.len();
        let crc = mpeg2_crc32(&pkt[s..crc_in_end]);
        pkt[crc_in_end] = (crc >> 24) as u8;
        pkt[crc_in_end + 1] = (crc >> 16) as u8;
        pkt[crc_in_end + 2] = (crc >> 8) as u8;
        pkt[crc_in_end + 3] = crc as u8;
        pkt
    }

    /// DVB AC-3 descriptor (tag 0x6A) on a private-data stream
    /// resolves to ATSC AC-3 (0x81) and feeds into the AC-3 decode
    /// path. This is the Australian / European / Latin American
    /// broadcast contribution case.
    #[test]
    fn dvb_ac3_descriptor_resolves_to_0x81() {
        // 0x6A descriptor with empty body (component_type bits all 0).
        let desc = [0x6A, 0x00];
        let pkt = synth_pmt_private_audio(0x1000, 0x0289, &desc);
        assert_eq!(parse_pmt_audio(&pkt, None), Some((0x0289, 0x81)));
    }

    /// DVB Enhanced AC-3 descriptor (tag 0x7A) on a private-data
    /// stream resolves to ATSC E-AC-3 (0x87). The original
    /// motivating case for this fix — ffmpeg-muxed E-AC-3 sources
    /// were silently passing through instead of transcoding.
    #[test]
    fn dvb_eac3_descriptor_resolves_to_0x87() {
        let desc = [0x7A, 0x00];
        let pkt = synth_pmt_private_audio(0x1000, 0x0289, &desc);
        assert_eq!(parse_pmt_audio(&pkt, None), Some((0x0289, 0x87)));
    }

    /// DVB AAC descriptor (tag 0x7C) on a private-data stream
    /// resolves to AAC LATM (0x11) — the broadcast carriage form
    /// (ETSI TS 101 154). The LATM splitter + libavcodec `aac_latm`
    /// decoder handle it downstream.
    #[test]
    fn dvb_aac_descriptor_resolves_to_0x11() {
        // 0x7C descriptor body: 1 byte profile_and_level (AAC-LC L4)
        let desc = [0x7C, 0x01, 0x28];
        let pkt = synth_pmt_private_audio(0x1000, 0x0289, &desc);
        assert_eq!(parse_pmt_audio(&pkt, None), Some((0x0289, 0x11)));
    }

    /// MPEG-2 registration_descriptor (tag 0x05) with `format_identifier
    /// = "AC-3"` is the ATSC-via-Cablelabs carriage form for AC-3 on
    /// `stream_type = 0x06`. Resolves to ATSC AC-3 (0x81).
    #[test]
    fn registration_descriptor_ac3_resolves_to_0x81() {
        let desc = [0x05, 0x04, b'A', b'C', b'-', b'3'];
        let pkt = synth_pmt_private_audio(0x1000, 0x0289, &desc);
        assert_eq!(parse_pmt_audio(&pkt, None), Some((0x0289, 0x81)));
    }

    /// Same flavour for E-AC-3 (`"EAC3"`).
    #[test]
    fn registration_descriptor_eac3_resolves_to_0x87() {
        let desc = [0x05, 0x04, b'E', b'A', b'C', b'3'];
        let pkt = synth_pmt_private_audio(0x1000, 0x0289, &desc);
        assert_eq!(parse_pmt_audio(&pkt, None), Some((0x0289, 0x87)));
    }

    /// Private-data streams that aren't audio (Opus, AC-4, DTS, …) or
    /// that carry no recognised descriptor must NOT be picked up as
    /// audio. The replacer would otherwise try to decode raw Opus
    /// frames with the libavcodec AC-3 decoder. Note: Opus is the one
    /// codec whose downstream path *does* accept stream_type 0x06, but
    /// only when routed there explicitly — `select_audio_es` is the
    /// re-encode gate and Opus isn't a re-encodable target on this
    /// MPEG-TS surface.
    #[test]
    fn unrecognised_private_descriptor_is_skipped() {
        // 0xAB is a placeholder descriptor tag we don't recognise.
        let desc = [0xAB, 0x02, 0x00, 0x00];
        let pkt = synth_pmt_private_audio(0x1000, 0x0289, &desc);
        assert_eq!(parse_pmt_audio(&pkt, None), None);

        // Opus registration: also skipped by the re-encode gate.
        let desc = [0x05, 0x04, b'O', b'p', b'u', b's'];
        let pkt = synth_pmt_private_audio(0x1000, 0x0289, &desc);
        assert_eq!(parse_pmt_audio(&pkt, None), None);
    }

    /// `resolve_private_audio_stream_type` survives a descriptor loop
    /// whose declared length runs past the buffer (malformed PMT) by
    /// returning `None` rather than reading out of bounds.
    #[test]
    fn descriptor_resolver_rejects_truncated_loop() {
        // tag 0x6A claims len=10 but only 2 bytes follow.
        let desc = [0x6A, 0x0A, 0xAA, 0xBB];
        assert_eq!(resolve_private_audio_stream_type(&desc), None);
    }

    /// Multiple descriptors in the same ES loop: the resolver finds
    /// the codec descriptor wherever it sits. DVB PMTs commonly have
    /// an ISO-639 language descriptor BEFORE the codec descriptor.
    #[test]
    fn descriptor_resolver_walks_past_language_descriptor() {
        // 0x0A ISO-639 language (4 bytes "eng" + audio_type), then 0x6A AC-3.
        let desc = [
            0x0A, 0x04, b'e', b'n', b'g', 0x00, // language
            0x6A, 0x00,                          // AC-3
        ];
        assert_eq!(resolve_private_audio_stream_type(&desc), Some(0x81));
    }

    /// Seamless input switching between inputs with different audio
    /// codecs (AAC → AC-3) must reset the decoder / encoder /
    /// transcoder so the new source's PCM isn't fed into an
    /// encoder initialised for the old format.
    #[test]
    fn codec_change_on_pmt_update_resets_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        assert_eq!(r.source_stream_type, 0x0F);
        assert_eq!(r.audio_pid, Some(0x0101));

        // Simulate state built up from streaming the old input: the
        // codecs are open, output PTS has been anchored, some PCM has
        // been queued in the accumulator. The reset path must wipe
        // all of this.
        r.codecs_ready = true;
        r.out_pts_anchored = true;
        r.resolved_sample_rate = 48_000;
        r.resolved_channels = 2;
        r.accumulator = vec![vec![0.5f32; 1024], vec![0.5f32; 1024]];

        // Input switch: same PMT / audio PID, but the new input is
        // AC-3 (stream_type 0x81).
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x81), &mut out);

        assert_eq!(r.source_stream_type, 0x81, "new source codec learned");
        assert!(
            !r.codecs_ready,
            "codecs_ready must be cleared so encoders re-init for new input"
        );
        assert!(
            !r.out_pts_anchored,
            "PTS must re-anchor to the new input's timeline"
        );
        assert_eq!(r.resolved_sample_rate, 0);
        assert_eq!(r.resolved_channels, 0);
        assert!(r.accumulator.is_empty(), "stale PCM must be dropped");
    }

    /// PID-only change (same codec, different audio PID) must also
    /// reset the pipeline — the old decoder PES buffer and PCM
    /// accumulator belong to a different elementary stream.
    #[test]
    fn audio_pid_change_on_pmt_update_resets_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        r.codecs_ready = true;
        r.out_pts_anchored = true;

        r.process(&synth_pmt_audio(0x1000, 0x0102, 0x0F), &mut out);

        assert_eq!(r.audio_pid, Some(0x0102));
        assert!(!r.codecs_ready);
        assert!(!r.out_pts_anchored);
    }

    /// Regression guard: unchanged PMTs arriving many times per second
    /// must not flip the reset path, otherwise every frame would pay
    /// the cost of closing and reopening the decoder + encoder.
    #[test]
    fn repeated_unchanged_pmt_does_not_reset_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.codecs_ready = true;
        r.out_pts_anchored = true;
        r.resolved_sample_rate = 48_000;

        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        assert!(r.codecs_ready, "unchanged PMT must not reset codec state");
        assert!(r.out_pts_anchored);
        assert_eq!(r.resolved_sample_rate, 48_000);
    }

    /// A PAT that relocates the program to a different PMT PID must
    /// also trigger the reset path (same kind of program-level
    /// discontinuity the video replacer handles).
    #[test]
    fn pmt_pid_change_on_pat_update_resets_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.codecs_ready = true;
        r.out_pts_anchored = true;

        r.process(&synth_pat(0x1001), &mut out);

        assert_eq!(r.pmt_pid, Some(0x1001));
        assert_eq!(r.audio_pid, None, "audio_pid must be cleared pending new PMT");
        assert!(!r.codecs_ready);
        assert!(!r.out_pts_anchored);
    }

    #[test]
    fn build_audio_pes_has_pts_and_stream_id() {
        let pes = build_audio_pes(0xC0, &[1, 2, 3, 4], 0x1234_5678);
        assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01]);
        assert_eq!(pes[3], 0xC0); // audio stream_id
        assert_eq!(pes[7], 0x80); // PTS flag
        assert_eq!(pes[8], 5);    // PES header data len
        // last 4 bytes should be the ES payload
        assert_eq!(&pes[pes.len() - 4..], &[1, 2, 3, 4]);
    }

    /// Each encoded audio frame is emitted as its own PES, so the
    /// payload starts at an access-unit boundary — ETSI TS 101 154
    /// §C.4 requires `data_alignment_indicator = 1` for broadcast.
    #[test]
    fn build_audio_pes_sets_data_alignment_indicator() {
        let pes = build_audio_pes(0xC0, &[0u8; 16], 0);
        // Byte 6 carries the marker + DAI flag. Bit 2 (0x04) is DAI.
        assert_eq!(pes[6] & 0x04, 0x04, "data_alignment_indicator must be 1");
    }

    /// Build a PMT TS packet with one audio ES carrying an
    /// ISO-639 language descriptor + an AAC descriptor (0x7C) with the
    /// caller-supplied `profile_and_level` body byte. Mirrors the
    /// real-world DVB shape we see on Sky Sports recordings.
    fn synth_pmt_audio_with_aac_desc(
        pmt_pid: u16,
        audio_pid: u16,
        stream_type: u8,
        aac_profile_and_level: u8,
    ) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = pmt_pid as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let s = 5;
        pkt[s] = 0x02; // table_id = PMT
        // 9 (header) + 5 (ES fixed) + 9 (descriptors: 0x0A len 4 + 0x7C len 1) + 4 (CRC) = 27
        let section_length: u16 = 27;
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00;
        pkt[s + 4] = 0x01;
        pkt[s + 5] = 0xC1;
        pkt[s + 6] = 0x00;
        pkt[s + 7] = 0x00;
        pkt[s + 8] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 9] = audio_pid as u8;
        pkt[s + 10] = 0xF0;
        pkt[s + 11] = 0x00;
        pkt[s + 12] = stream_type;
        pkt[s + 13] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 14] = audio_pid as u8;
        // ES_info_length = 9 (4-byte ISO-639 desc + 1-byte AAC desc + 2x 2-byte tag/len)
        pkt[s + 15] = 0xF0;
        pkt[s + 16] = 0x09;
        // ISO-639 language descriptor (0x0A), len 4: "eng" + audio_type 0
        pkt[s + 17] = 0x0A;
        pkt[s + 18] = 0x04;
        pkt[s + 19] = b'e';
        pkt[s + 20] = b'n';
        pkt[s + 21] = b'g';
        pkt[s + 22] = 0x00;
        // AAC descriptor (0x7C), len 1: profile_and_level
        pkt[s + 23] = 0x7C;
        pkt[s + 24] = 0x01;
        pkt[s + 25] = aac_profile_and_level;
        let crc = mpeg2_crc32(&pkt[s..s + 26]);
        pkt[s + 26] = (crc >> 24) as u8;
        pkt[s + 27] = (crc >> 16) as u8;
        pkt[s + 28] = (crc >> 8) as u8;
        pkt[s + 29] = crc as u8;
        pkt
    }

    /// Re-encoding to AAC-LC must neutralise the inherited AAC
    /// descriptor's profile_and_level so a strict broadcast decoder
    /// doesn't refuse audio output when the source advertised HE-AAC
    /// but we emit AAC-LC.
    #[test]
    fn pmt_aac_descriptor_neutralised_for_aac_target() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        // Source advertises HE-AAC L3 (0x51) but the ES (we don't
        // feed any here) would carry plain AAC-LC after re-encode.
        let pmt = synth_pmt_audio_with_aac_desc(0x1000, 0x0101, 0x0F, 0x51);
        r.process(&pmt, &mut out);
        assert_eq!(out.len(), 188);
        // ts header (4) + pointer (1) + table_id (1) + section_length (2)
        //   + program_number (2) + version (1) + section# (2) + PCR_PID (2)
        //   + program_info_length (2) = 17 → ES entry starts here.
        // ES: stream_type (1) + es_pid (2) + es_info_length (2) = 5 → desc list at 22.
        // Desc list: ISO-639 (2 + 4 = 6 bytes) → AAC tag at 28, len at 29, body at 30.
        assert_eq!(out[5 + 23], 0x7C, "AAC descriptor tag still present");
        assert_eq!(out[5 + 24], 0x01, "AAC descriptor body length unchanged");
        assert_eq!(
            out[5 + 25],
            0xFE,
            "AAC profile_and_level neutralised (was 0x51 HE-AAC, now 0xFE = unspecified)"
        );
        // Section CRC must be valid after the in-place rewrite.
        let section_length =
            (((out[5 + 1] & 0x0F) as usize) << 8) | (out[5 + 2] as usize);
        let crc_off = 5 + 3 + section_length - 4;
        let computed = mpeg2_crc32(&out[5..crc_off]);
        let stored = ((out[crc_off] as u32) << 24)
            | ((out[crc_off + 1] as u32) << 16)
            | ((out[crc_off + 2] as u32) << 8)
            | (out[crc_off + 3] as u32);
        assert_eq!(computed, stored, "CRC32 must match after descriptor rewrite");
    }

    /// Re-encoding to a non-AAC target (MP2 / AC-3) must DROP the source's
    /// AAC descriptor: a 0x7C on an MP2 or AC-3 ES is non-conformant
    /// (EN 300 468 / TS 101 154). This test used to pin the stale
    /// descriptor as intent. The source here is DVB-flavoured (0x7C), so
    /// AC-3 goes out as 0x06 + "AC-3" registration + 0x6A.
    #[test]
    fn pmt_aac_descriptor_dropped_for_non_aac_target() {
        let mut r = TsAudioReplacer::new(&enc("ac3"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        let pmt = synth_pmt_audio_with_aac_desc(0x1000, 0x0101, 0x0F, 0x51);
        r.process(&pmt, &mut out);
        assert_eq!(out.len(), 188);
        let s = crate::engine::ts_parse::find_section_in_packet(&out, 0x02, Some(1)).unwrap();
        let sec = &out[s.start..s.end()];
        assert_eq!(mpeg2_crc32(sec), 0, "CRC valid");
        let v = parse_pmt(sec).unwrap();
        assert_eq!(v.es[0].stream_type, 0x06);
        let tags: Vec<u8> =
            crate::engine::ts_pmt_edit::descriptors(v.es_info(&v.es[0])).map(|(t, _)| t).collect();
        assert_eq!(tags, vec![0x0A, 0x05, 0x6A], "7C dropped; AC-3 registration + 6A added");

        // MP2 target: 0x03, 7C gone, language kept.
        let mut r = TsAudioReplacer::new(&enc("mp2"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        r.process(&pmt, &mut out);
        let s = crate::engine::ts_parse::find_section_in_packet(&out, 0x02, Some(1)).unwrap();
        let v = parse_pmt(&out[s.start..s.end()]).unwrap();
        assert_eq!(v.es[0].stream_type, 0x03);
        let tags: Vec<u8> =
            crate::engine::ts_pmt_edit::descriptors(v.es_info(&v.es[0])).map(|(t, _)| t).collect();
        assert_eq!(tags, vec![0x0A]);
    }

    // ── multi-section / multi-packet PMT (defects 6 / 6c) ─────────────

    use crate::engine::ts_test_fixtures::{
        packetize_sections, pat_packet, pmt_section, two_packet_pmt, vh1_pat_packet,
        vh1_pmt_packet, VH1_PMT_OFFSET, VH1_PROGRAM,
    };

    fn pmt_in(out: &[u8], program: u16) -> Vec<u8> {
        let mut asm = crate::engine::ts_parse::SectionAssembler::new();
        let mut found = None;
        for p in out.chunks(TS_PACKET_SIZE) {
            for sec in asm.feed(ts_pusi(p), &p[ts_payload_offset(p)..]) {
                if is_pmt_for(sec, program) {
                    found = Some(sec.to_vec());
                }
            }
        }
        found.expect("PMT in output")
    }

    #[test]
    fn vh1_pmt_is_learned_and_rewritten_behind_the_private_section() {
        // AAC target, pinned to the AC-3 audio 0x0E10 like the e2e run.
        let mut cfg = enc("aac_lc");
        cfg.source_audio_pid = Some(0x0E10);
        let mut r = TsAudioReplacer::new(&cfg, None).unwrap();
        let mut out = Vec::new();
        r.process(&vh1_pat_packet(), &mut out);
        out.clear();
        let pmt = vh1_pmt_packet();
        r.process(&pmt, &mut out);
        assert_eq!(r.audio_pid, Some(0x0E10));
        assert_eq!(r.source_stream_type, 0x81);
        assert_eq!(out.len(), 188, "same packet count");
        assert_eq!(&out[..VH1_PMT_OFFSET], &pmt[..VH1_PMT_OFFSET], "0xC0 section byte-identical");
        assert!(crate::engine::ts_parse::verify_psi_crc(&out, VH1_PMT_OFFSET));
        let v_out = pmt_in(&out, VH1_PROGRAM);
        let v = parse_pmt(&v_out).unwrap();
        let a = v.es.iter().find(|e| e.pid == 0x0E10).unwrap();
        assert_eq!(a.stream_type, 0x0F);
        // Every other ES entry untouched.
        assert_eq!(v.es.len(), 6);
    }

    #[test]
    fn target_in_the_second_packet_of_a_pmt_is_learned_and_crc_valid() {
        let (sec, target) = two_packet_pmt(1, 4);
        let pkts = packetize_sections(0x1000, &[&sec], 0);
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        for p in &pkts {
            r.process(p, &mut out);
        }
        assert_eq!(r.audio_pid, Some(target), "ES beyond the first packet is found");
        assert_eq!(out.len(), 2 * 188);
        let got = pmt_in(&out, 1);
        assert_eq!(mpeg2_crc32(&got), 0, "one valid-CRC section");
        let v = parse_pmt(&got).unwrap();
        assert_eq!(v.es.iter().find(|e| e.pid == target).unwrap().stream_type, 0x0F);
        assert_eq!(out[3] & 0x0F, 0);
        assert_eq!(out[188 + 3] & 0x0F, 1, "CC continuous");
    }

    #[test]
    fn two_programs_sharing_a_pmt_pid_match_on_program_number() {
        // Program 2's PMT first in the packet, program 1's second; the
        // replacer follows the lowest program number from the PAT.
        let p2 = pmt_section(2, 0, 0x200, &[], &[(0x03, 0x201, &[])]);
        let p1 = pmt_section(1, 0, 0x100, &[], &[(0x0F, 0x101, &[])]);
        let pkts = packetize_sections(0x1000, &[&p2, &p1], 0);
        assert_eq!(pkts.len(), 1);
        let mut r = TsAudioReplacer::new(&enc("mp2"), None).unwrap();
        let mut out = Vec::new();
        r.process(&pat_packet(&[(1, 0x1000), (2, 0x1000)], 0, 0), &mut out);
        out.clear();
        r.process(&pkts[0], &mut out);
        assert_eq!(r.audio_pid, Some(0x101));
        let untouched = pmt_in(&out, 2);
        assert_eq!(untouched, p2, "the other program's PMT is byte-identical");
    }

    #[test]
    fn output_version_tracks_source_content_without_a_reset() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        let v1 = pmt_section(1, 0, 0x101, &[], &[(0x0F, 0x101, &[])]);
        let v2 = pmt_section(1, 1, 0x101, &[], &[(0x0F, 0x101, &[]), (0x06, 0x102, &[0x56, 0x00])]);
        let mut versions = Vec::new();
        for (i, sec) in [&v1, &v1, &v1, &v2, &v2].iter().enumerate() {
            out.clear();
            r.process(&packetize_sections(0x1000, &[sec], i as u8)[0], &mut out);
            let got = pmt_in(&out, 1);
            versions.push((got[5] >> 1) & 0x1F);
        }
        assert_eq!(versions[0], versions[2], "repeated PMT: constant version");
        assert_ne!(versions[2], versions[3], "an added ES bumps the version");
        assert_eq!(versions[3], versions[4]);
    }

    /// A PMT whose CRC does not verify is never learned from or rebuilt
    /// (rebuilding would give the damaged bytes a fresh, valid CRC).
    #[test]
    fn a_damaged_pmt_is_neither_learned_nor_rebuilt() {
        let mut r = TsAudioReplacer::new(&enc("mp2"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        let mut pmt = synth_pmt_audio(0x1000, 0x0101, 0x0F);
        pmt[5 + 14] ^= 0x01; // flip a bit of the audio PID
        r.process(&pmt, &mut out);
        assert_eq!(r.audio_pid, None);
        assert_eq!(out, pmt.to_vec(), "passed through untouched");
    }

    /// An input switch bumps the output version even when the rebuilt PMT
    /// is byte-identical, so a receiver that cached the previous source's
    /// PMT re-parses after an `A → B → A` round trip.
    #[test]
    fn an_input_switch_bumps_the_output_version() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        let v1 = (pmt_in(&out, 1)[5] >> 1) & 0x1F;
        r.external_reset_handle().store(true, Ordering::Relaxed);
        out.clear();
        let mut pmt = synth_pmt_audio(0x1000, 0x0101, 0x0F);
        pmt[3] = 0x11;
        r.process(&pmt, &mut out);
        let v2 = (pmt_in(&out, 1)[5] >> 1) & 0x1F;
        assert_ne!(v1, v2);
    }

    /// Output transcodes to AAC; an input switch brings a source whose only
    /// audio is DTS, so its PMT passes through unedited. It used to keep
    /// its SOURCE version — here 1, the very version the rebuilt PMT of the
    /// previous input carried — and a receiver caching by version kept the
    /// old PMT (AAC on the audio PID). Once the stage has stamped, the
    /// passthrough PMT is stamped from the same sequence. Before any stamp
    /// a passthrough PMT stays byte-identical.
    #[test]
    fn a_passthrough_pmt_after_a_rebuilt_one_gets_a_new_version() {
        let aac = pmt_section(1, 1, 0x101, &[], &[(0x0F, 0x101, &[])]);
        let dts = pmt_section(1, 1, 0x101, &[], &[(0x82, 0x101, &[])]);
        let ver = |s: &[u8]| (s[5] >> 1) & 0x1F;

        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        // Before any stamp: a DTS PMT is byte-identical, source version
        // (here 5) and all.
        out.clear();
        let dts_v5 = pmt_section(1, 5, 0x101, &[], &[(0x82, 0x101, &[])]);
        let dts_pkt = packetize_sections(0x1000, &[&dts_v5], 0)[0];
        r.process(&dts_pkt, &mut out);
        assert_eq!(out, dts_pkt.to_vec(), "untouched before the stage ever stamped");

        // Input A (AAC): rebuilt and stamped.
        out.clear();
        r.process(&packetize_sections(0x1000, &[&aac], 1)[0], &mut out);
        let a = pmt_in(&out, 1);
        assert_eq!(parse_pmt(&a).unwrap().es[0].stream_type, 0x0F);
        // Switch to input B (DTS only): passthrough, but a fresh version.
        r.external_reset_handle().store(true, Ordering::Relaxed);
        let mut vs = Vec::new();
        for cc in 2..5u8 {
            out.clear();
            r.process(&packetize_sections(0x1000, &[&dts], cc)[0], &mut out);
            let b = pmt_in(&out, 1);
            assert_eq!(mpeg2_crc32(&b), 0);
            assert_eq!(parse_pmt(&b).unwrap().es[0].stream_type, 0x82, "content passed through");
            vs.push(ver(&b));
        }
        assert_ne!(vs[0], ver(&a), "B's PMT must not repeat A's version");
        assert!(vs.iter().all(|v| *v == vs[0]), "and holds while B repeats: {vs:?}");
    }

    #[test]
    fn flavour_is_latched_across_a_dvb_to_atsc_switch() {
        let mut r = TsAudioReplacer::new(&enc("ac3"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        // DVB source first (0x7C on the AAC ES).
        out.clear();
        r.process(&synth_pmt_audio_with_aac_desc(0x1000, 0x0101, 0x0F, 0x51), &mut out);
        assert_eq!(parse_pmt(&pmt_in(&out, 1)).unwrap().es[0].stream_type, 0x06);
        // Switch to an ATSC-looking input: signalling must not flip.
        r.external_reset_handle().store(true, Ordering::Relaxed);
        out.clear();
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x81), &mut out);
        assert_eq!(parse_pmt(&pmt_in(&out, 1)).unwrap().es[0].stream_type, 0x06);
    }

    /// A DVB-flavoured AC-3 output (0x06 + "AC-3" + 0x6A) of an ingress
    /// transcode becomes the flow's broadcast: the shared demuxer must still
    /// lock it as AC-3 and surface its PES as `OtherAudio { 0x81 }` — the arm
    /// `replay::export_mp4` builds its AC-3 track from, and the one the
    /// display / RTMP / WebRTC paths decode.
    #[test]
    fn dvb_flavoured_ac3_output_demuxes_as_ac3() {
        use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
        let mut r = TsAudioReplacer::new(&enc("ac3"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        r.process(&synth_pmt_audio_with_aac_desc(0x1000, 0x0101, 0x0F, 0x51), &mut out);
        assert_eq!(parse_pmt(&pmt_in(&out, 1)).unwrap().es[0].stream_type, 0x06);
        let mut demux = TsDemuxer::new(None);
        demux.demux(&synth_pat(0x1000));
        demux.demux(&out);
        assert_eq!(demux.audio_pid(), Some(0x0101));
        let pes = build_audio_pes(0xBD, &[0x0B, 0x77, 1, 2, 3, 4, 5, 6], 90_000);
        let mut cc = 0u8;
        let mut frames = Vec::new();
        for _ in 0..2 {
            for p in packetize_ts(0x0101, &pes, &mut cc) {
                frames.extend(demux.demux(&p));
            }
        }
        assert!(
            frames.iter().any(|f| matches!(f, DemuxedFrame::OtherAudio { stream_type: 0x81, .. })),
            "AC-3 PES surfaced on the 0x81 arm"
        );
    }

    /// The re-encoded AC-3 ES rides PES private_stream_1 (0xBD) — ATSC A/52
    /// Annex A and ETSI TS 101 154 both require it for AC-3, and DVB
    /// carriage signals the ES as private data (0x06) — while MP2 / AAC
    /// keep the MPEG audio id 0xC0. Every AC-3 PES used to go out as 0xC0.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn re_encoded_pes_use_the_codec_stream_id() {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        for (codec, want) in [("ac3", 0xBDu8), ("mp2", 0xC0)] {
            let mut r = TsAudioReplacer::new(&enc(codec), None).unwrap();
            let mut out = Vec::new();
            r.process(&synth_pat(0x1000), &mut out);
            r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
            let (mut off, mut pts, mut cc) = (0usize, 90_000u64, 0u8);
            while off + 7 <= ADTS.len() {
                let len = (((ADTS[off + 3] as usize) & 0x03) << 11)
                    | ((ADTS[off + 4] as usize) << 3)
                    | ((ADTS[off + 5] as usize) >> 5);
                if len == 0 || off + len > ADTS.len() {
                    break;
                }
                for p in packetize_ts(0x0101, &build_audio_pes(0xC0, &ADTS[off..off + len], pts), &mut cc) {
                    r.process(&p, &mut out);
                }
                pts += 1920;
                off += len;
            }
            let ids: Vec<u8> = out
                .chunks(TS_PACKET_SIZE)
                .filter(|p| ts_pid(p) == 0x0101 && ts_pusi(p))
                .map(|p| p[ts_payload_offset(p) + 3])
                .collect();
            assert!(!ids.is_empty(), "{codec}: re-encoded audio emitted");
            assert!(ids.iter().all(|&id| id == want), "{codec}: PES stream_id {ids:02X?}");
        }
    }

    #[test]
    fn ts_signalling_override_pins_the_flavour() {
        let mut cfg = enc("ac3");
        cfg.ts_signalling = Some(crate::config::models::TsAudioSignalling::Atsc);
        let mut r = TsAudioReplacer::new(&cfg, None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        r.process(&synth_pmt_audio_with_aac_desc(0x1000, 0x0101, 0x0F, 0x51), &mut out);
        let got = pmt_in(&out, 1);
        let v = parse_pmt(&got).unwrap();
        assert_eq!(v.es[0].stream_type, 0x81);
        let tags: Vec<u8> =
            crate::engine::ts_pmt_edit::descriptors(v.es_info(&v.es[0])).map(|(t, _)| t).collect();
        assert_eq!(tags, vec![0x0A, 0x05]);
    }

    // ── engage watchdog (defect 6b) ──

    fn future(secs: u64) -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(secs)
    }

    #[test]
    fn dts_only_program_raises_codec_not_replaceable_once() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let (tx, mut rx) = crate::manager::events::event_channel();
        r.set_event_watchdog(tx, "out-1", false);
        let dts = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x06, 0x101, &[0x7B, 0x00])]);
        let mut out = Vec::new();
        for i in 0..12u8 {
            r.process(&synth_pat(0x1000), &mut out);
            r.process(&packetize_sections(0x1000, &[&dts], i)[0], &mut out);
        }
        assert!(r.audio_pid.is_none());
        match r.poll_engage(future(6)) {
            Some(EngageEvent::NotFound { reason, .. }) => {
                assert_eq!(reason.as_str(), "codec_not_replaceable")
            }
            other => panic!("{other:?}"),
        }
        assert!(r.poll_engage(future(30)).is_none(), "exactly once");
        let ev = rx.try_recv().expect("event emitted");
        assert_eq!(ev.output_id.as_deref(), Some("out-1"));
        assert_eq!(ev.details.unwrap()["error_code"], "audio_transcode_source_not_found");
    }

    /// A PMT PID that carries only a user-private table (what the replacer
    /// saw on VH1 before the section walker): reason `pmt_not_parsed`, with
    /// the table it did find.
    #[test]
    fn a_pmt_pid_without_a_pmt_raises_pmt_not_parsed() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let c0 = [0xC0u8, 0x00, 0x05, 1, 2, 3, 4, 5];
        let mut out = Vec::new();
        for i in 0..12u8 {
            r.process(&synth_pat(0x1000), &mut out);
            r.process(&packetize_sections(0x1000, &[&c0], i)[0], &mut out);
        }
        match r.poll_engage(future(6)) {
            Some(EngageEvent::NotFound { reason, details }) => {
                assert_eq!(reason.as_str(), "pmt_not_parsed");
                assert_eq!(details["first_table_id"], 0xC0);
            }
            other => panic!("{other:?}"),
        }
    }

    /// No PAT at all: nothing fires until 10 s and 1000 packets.
    #[test]
    fn a_stream_without_a_pat_raises_no_pat() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut es = [0xFFu8; TS_PACKET_SIZE];
        es[0] = TS_SYNC_BYTE;
        es[1] = 0x01;
        es[2] = 0x00;
        es[3] = 0x10;
        let chunk: Vec<u8> = std::iter::repeat_n(es, 100).flatten().collect();
        let mut out = Vec::new();
        for _ in 0..11 {
            r.process(&chunk, &mut out);
        }
        assert!(r.poll_engage(future(6)).is_none(), "5 s is not enough without PMT evidence");
        match r.poll_engage(future(11)) {
            Some(EngageEvent::NotFound { reason, .. }) => assert_eq!(reason.as_str(), "no_pat"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_normal_pmt_raises_nothing() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        for i in 0..12u8 {
            r.process(&synth_pat(0x1000), &mut out);
            let mut pmt = synth_pmt_audio(0x1000, 0x0101, 0x0F);
            pmt[3] = 0x10 | (i & 0x0F);
            r.process(&pmt, &mut out);
        }
        assert!(r.poll_engage(future(60)).is_none());
    }

    #[test]
    fn a_pinned_but_absent_pid_warns_without_the_timer() {
        let mut cfg = enc("aac_lc");
        cfg.source_audio_pid = Some(0x0999);
        let mut r = TsAudioReplacer::new(&cfg, None).unwrap();
        let (tx, mut rx) = crate::manager::events::event_channel();
        r.set_event_watchdog(tx, "in-1", true);
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        let ev = rx.try_recv().expect("pinned warning emitted at parse time");
        assert_eq!(ev.input_id.as_deref(), Some("in-1"));
        assert_eq!(ev.details.unwrap()["error_code"], "audio_source_pid_not_found");
        // Deduped: the same (pinned, actual) pair does not warn again.
        let mut pmt = synth_pmt_audio(0x1000, 0x0101, 0x0F);
        pmt[3] = 0x11;
        r.process(&pmt, &mut out);
        assert!(rx.try_recv().is_err());
    }

    // ── PTS sample-anchor regression tests ──
    //
    // These tests pin the replacement of the legacy FIFO PTS queue
    // with the anchor + samples-emitted model. The motivating bug:
    // for any mapping where source AU count ≠ encoder output frame
    // count (false-syncword splitter hit, frame-size mismatch like
    // AC-3 → HE-AAC v1, decoder priming) the FIFO emitted duplicate
    // and skipped PTS values — receivers heard sync-jump glitches and
    // eventually muted the audio entirely. The anchor model is
    // monotonic and exact within ±1 tick regardless of how the
    // decoder and encoder buffer.

    /// Anchor returned verbatim when no output samples have been
    /// emitted yet (start of stream / immediately after re-anchor).
    #[test]
    fn next_output_pts_anchor_only_returns_anchor() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.out_pts_90k = 0xAA_BBCC_DDEE;
        r.samples_since_anchor = 0;
        r.resolved_sample_rate = 48_000;
        assert_eq!(r.next_output_pts_90k(48_000), 0xAA_BBCC_DDEE);
    }

    /// At 48 kHz the advance is exact for any frame size we emit
    /// (1024 / 1152 / 1536 / 2048 samples each map to integer
    /// 90 kHz tick counts). 1000 frames at AAC-LC frame size = 1024:
    /// expected advance = 1000 * 1024 * 90000 / 48000 = 1_920_000.
    #[test]
    fn next_output_pts_advances_exactly_at_48k() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.out_pts_90k = 1_000_000;
        r.resolved_sample_rate = 48_000;
        r.samples_since_anchor = 1024 * 1000; // 1000 AAC-LC frames worth
        assert_eq!(r.next_output_pts_90k(48_000), 1_000_000 + 1_920_000);
    }

    /// 44.1 kHz exercises the "rounds once per PES" property. The
    /// legacy code that added (samples * 90000 / sr) per emit would
    /// accumulate (90000 mod 44100) per frame; anchor + sample-count
    /// math rounds exactly once, so the worst-case error is ≤1 tick
    /// no matter how many frames have been emitted.
    #[test]
    fn next_output_pts_advances_without_accumulating_at_44k() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.out_pts_90k = 0;
        r.resolved_sample_rate = 44_100;
        // 1000 AAC-LC frames at 44.1 kHz:
        //   advance = 1024 * 1000 * 90000 / 44100 = 2_089_795.918...
        // The single-shot integer division floors to 2_089_795.
        r.samples_since_anchor = 1024 * 1000;
        let pts = r.next_output_pts_90k(44_100);
        assert_eq!(pts, 2_089_795);
        // Compare against the legacy accumulate-each-frame model: if
        // we'd added (1024 * 90000 / 44100) = 2089 ticks per frame
        // for 1000 frames, the running counter would have reached
        // 2_089_000 — 795 ticks shy of the true elapsed time. The
        // anchor model recovers that 795-tick drift on every frame.
        let legacy_per_frame = (1024u64 * 90_000) / 44_100;
        let legacy_accumulated = legacy_per_frame * 1000;
        assert!(
            pts > legacy_accumulated,
            "anchor model must NOT under-count vs. legacy per-frame accumulation"
        );
        assert!(pts - legacy_accumulated <= 1000,
            "drift between anchor model and legacy accumulator stays \
             within sub-frame ticks — single-shot rounding is bounded"
        );
    }

    /// Unset / unknown sample rate must short-circuit to the anchor
    /// rather than divide by zero. Hit when the first PES failed to
    /// decode and the encoder still gets called on a flush.
    #[test]
    fn next_output_pts_handles_unresolved_sample_rate() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.out_pts_90k = 12_345;
        r.samples_since_anchor = 99_999;
        assert_eq!(r.next_output_pts_90k(0), 12_345);
    }

    /// `reset_source_state` must zero the sample counter and clear
    /// the expected-next source PTS — otherwise an input switch would
    /// keep advancing PTS from the OLD input's accumulated samples
    /// against the NEW input's anchor.
    #[test]
    fn reset_source_state_clears_pts_arithmetic_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        // Simulate a fully running pipeline.
        r.out_pts_anchored = true;
        r.out_pts_90k = 1_000_000;
        r.samples_since_anchor = 48_000 * 30;
        r.expected_next_src_pts_90k = Some(1_000_000 + 90_000 * 30);

        // Codec swap forces a reset.
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x81), &mut out);

        assert!(!r.out_pts_anchored, "anchor flag cleared on reset");
        assert_eq!(r.samples_since_anchor, 0, "sample counter zeroed");
        assert!(
            r.expected_next_src_pts_90k.is_none(),
            "expected-next must be cleared so the new input doesn't \
             trip the discontinuity guard against a stale value"
        );
    }

    /// A PES that fails to decode (no syncframes found) must still
    /// advance `expected_next_src_pts_90k` to its own pts. The previous
    /// code left it stale, so a run of failed PESes — e.g. the malformed
    /// AC-3 bytes ffmpeg's mpegts muxer emits at a `-stream_loop` wrap
    /// boundary — would let the projection fall hundreds of milliseconds
    /// behind. The next successful PES then saw a synthetic forward
    /// delta against the stale projection and fired a phantom
    /// discontinuity, re-anchoring `out_pts_90k` forward. Stack a few of
    /// those across one loop wrap and the output audio PTS races ahead
    /// of the passthrough video PTS lineage.
    #[test]
    fn decode_failure_when_anchored_advances_expected_next_to_current_pts() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        // Wire just enough state for consume_pes to walk to the end:
        // anchored, audio_pid known, source codec set, sample-rate stale
        // from a previous (hypothetical) decode.
        r.out_pts_anchored = true;
        r.out_pts_90k = 1_000_000;
        r.samples_since_anchor = 0;
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F; // AAC ADTS
        // Stale projection from an earlier successful PES.
        r.expected_next_src_pts_90k = Some(1_000_000 + 2_880);

        // Build a PES with a valid PTS but ES bytes that don't pass any
        // ADTS / AC-3 / MP2 / LATM splitter — so `decoded_frames` stays
        // empty and `source_samples_in_pes` stays 0.
        let pes = build_audio_pes(0xC0, &[0u8; 32], 1_100_000);
        let mut out = Vec::new();
        let _ = r.consume_pes(&pes, &mut out);

        assert_eq!(
            r.expected_next_src_pts_90k,
            Some(1_100_000),
            "decode failure on an anchored PES must roll expected-next to \
             the current pts so successive failures don't accumulate a \
             stale projection and fire phantom forward-jump discontinuities"
        );
    }

    // ─── Master-clock anchor (Phase B, step 2) ────────────────────

    /// Without a pacer attached, `anchor_target` returns the source PTS
    /// — preserving today's anchor-to-source behaviour for callers
    /// that haven't opted into master-clock anchoring.
    #[test]
    fn anchor_target_with_no_pacer_returns_src_pts() {
        let v = anchor_target(None, 1_234_567);
        assert_eq!(v, 1_234_567);
    }

    /// With a pacer attached, anchor_target STILL returns src_pts.
    /// This is the post-fix correct behaviour: the audio replacer
    /// emits source-relative PTS so the downstream `ts_pts_rewriter`
    /// (or assembler-side rewriter) is the single owner of master-
    /// clock anchoring. Double-anchoring breaks A/V sync.
    #[test]
    fn anchor_target_always_returns_src_pts_to_avoid_double_anchor() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        let pacer = std::sync::Arc::new(AvSyncPacer::new(handle));
        assert_eq!(anchor_target(Some(&pacer), 126_000), 126_000);
        assert_eq!(anchor_target(Some(&pacer), 0xABCDEF), 0xABCDEF);
    }

    /// First PES with pacer set: out_pts_90k anchors to src_pts, NOT
    /// to a master-clock value. Downstream rewriter owns master anchor.
    #[test]
    fn first_pes_anchor_uses_src_pts_when_pacer_set() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        let pacer = std::sync::Arc::new(AvSyncPacer::new(handle));
        r.set_av_sync_pacer(pacer);

        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;

        let src_pts = 1_234_567u64;
        let pes = build_audio_pes(0xC0, &[0u8; 32], src_pts);
        let mut out = Vec::new();
        let _ = r.consume_pes(&pes, &mut out);

        assert!(r.out_pts_anchored);
        assert_eq!(
            r.out_pts_90k, src_pts,
            "out_pts_90k must equal src_pts so downstream rewriter \
             (ts_pts_rewriter / assembler rewriter) sees source-relative \
             PTS and is the single owner of master-clock anchoring"
        );
    }

    // ─── Per-input PCR forward-jump signal (silence-pad redesign) ──

    /// **The bug catcher.** Two replacers configured with INDEPENDENT
    /// per-input signal Arcs must NOT see each other's PCR jumps.
    /// This is the architectural change vs the per-flow shared
    /// pacer signal that caused the "constant silence" regression in
    /// the first 0.84.0 attempt — every passive input's loop-wrap
    /// signal accumulated into the shared counter, the active
    /// replacer padded silence for the sum, audio drowned.
    #[test]
    fn pcr_jump_signal_is_per_input_not_shared() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let sig_a = std::sync::Arc::new(AtomicI64::new(0));
        let sig_b = std::sync::Arc::new(AtomicI64::new(0));

        let mut r_a = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r_a.set_pcr_jump_signal(sig_a.clone());
        r_a.audio_pid = Some(0x0101);
        r_a.source_stream_type = 0x0F;

        let mut r_b = TsAudioReplacer::new(&enc("mp2"), None).unwrap();
        r_b.set_pcr_jump_signal(sig_b.clone());
        r_b.audio_pid = Some(0x0102);
        r_b.source_stream_type = 0x0F;

        // Input A's rewriter signals 1.333 s forward jump on input A's
        // counter ONLY. Input B's counter stays at zero.
        sig_a.fetch_add(35_991_000, Ordering::Release);

        // Replacer A consumes its signal → pending_silence non-zero.
        let _ = r_a.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 1), &mut Vec::new());
        assert_eq!(r_a.pending_silence_27mhz, 35_991_000);

        // Replacer B consumes its independent signal → pending stays 0.
        let _ = r_b.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 2), &mut Vec::new());
        assert_eq!(
            r_b.pending_silence_27mhz, 0,
            "passive input B must NOT receive input A's loop-wrap signal — \
             per-input Arc isolation is the architectural fix for the \
             cross-input silence pollution that caused 0.84.0's audio drop"
        );

        // After consume A drained its counter via swap(0); both are
        // now back at zero. A new jump on B fires only B's counter.
        assert_eq!(sig_a.load(Ordering::Acquire), 0);
        assert_eq!(sig_b.load(Ordering::Acquire), 0);
        sig_b.fetch_add(21_384_000, Ordering::Release);
        let _ = r_a.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 3), &mut Vec::new());
        let _ = r_b.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 4), &mut Vec::new());
        assert_eq!(
            r_a.pending_silence_27mhz, 35_991_000,
            "no double-charge on A — B's signal must NOT reach A"
        );
        assert_eq!(r_b.pending_silence_27mhz, 21_384_000);
    }

    /// Signal Arc shared by setter is reflected by the replacer's
    /// next consume_pes — covers the basic flow on one input.
    #[test]
    fn pcr_jump_signal_drains_into_pending_silence() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let sig = std::sync::Arc::new(AtomicI64::new(0));
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.set_pcr_jump_signal(sig.clone());
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;

        sig.fetch_add(35_991_000, Ordering::Release);
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 100), &mut Vec::new());
        assert_eq!(r.pending_silence_27mhz, 35_991_000);
        // Signal drained — `swap(0)` semantics.
        assert_eq!(sig.load(Ordering::Acquire), 0);
    }

    /// `reset_source_state` (input switch / codec change) clears both
    /// `pending_silence_27mhz` AND drains the shared signal so the
    /// new source doesn't inherit stale loop-wrap pads.
    #[test]
    fn reset_source_state_drains_pcr_jump_signal() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let sig = std::sync::Arc::new(AtomicI64::new(99_999_999));
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.set_pcr_jump_signal(sig.clone());
        r.pending_silence_27mhz = 12_345_678;

        r.reset_source_state("test");
        assert_eq!(r.pending_silence_27mhz, 0);
        assert_eq!(
            sig.load(Ordering::Acquire), 0,
            "reset must drain the shared signal so the new pipeline \
             doesn't pick up stale loop-wrap pads"
        );
    }

    // ─── Sub-500ms forward source-PTS jump detector (0.87.0 fix) ──
    //
    // Bug: per-loop media_player splices set the next loop's first PES
    // to `max(audio_max, pcr_max) + SPLICE_GUARD_TICKS_90K`. On real
    // broadcast captures `pcr_max - audio_max ≈ 238 ms`, so audio PES
    // PTS jumps forward by ~268 ms per loop. That's under the 500 ms
    // discontinuity threshold, so the discontinuity-guard re-anchor did
    // not catch it. The free-running output clock kept advancing at the
    // steady (samples_since_anchor / sample_rate) rate while video
    // passthrough PESes advanced with the source's per-loop forward
    // jump verbatim — audio drifted against video by the jump amount
    // per loop (-26.8 ms/min over 30 min × 10 loops on the Sky Witness
    // 1080i25 capture, cell 4 of testbed/full_test_2026-05-21/v2).
    //
    // Fix: on the media_player path (no `av_sync_pacer`), TRACK the
    // forward source-PTS step by advancing the anchor `out_pts_90k` (a
    // pure relabel — reproduces the drift-free passthrough timeline, no
    // silence inserted so decoded PCM stays gap-free per gate 6). The
    // master-clock-paced paths (live SRT / output transcode) keep the
    // older silence-pad behaviour. Deadband 5 ms — well above MP2 / AAC /
    // AC-3 per-PES jitter, well below 500 ms.

    /// Forward source-PTS jump in `(5 ms, 500 ms]` on the media_player
    /// path relabels the anchor (tracks the source) instead of queuing
    /// silence — the per-loop loop-splice catch-up that fixes the drift.
    #[test]
    fn sub_500ms_forward_source_jump_relabels_anchor_on_media_player_path() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        assert!(r.av_sync_pacer.is_none(), "media_player path has no pacer");
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        // Prime the anchor + expected_next_src_pts via a first PES.
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 90_000), &mut Vec::new());
        assert!(r.out_pts_anchored);
        let out_after_first = r.out_pts_90k;
        let expected_after_first = r
            .expected_next_src_pts_90k
            .expect("first PES must stamp expected_next");
        // Source skips forward by 268 ms (24 120 ticks @ 90 kHz) —
        // the typical Sky Witness loop-splice forward jump.
        let jump_90k: u64 = 24_120;
        let next_pts = expected_after_first.wrapping_add(jump_90k);
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], next_pts), &mut Vec::new());
        // Delta = jump_90k > 5 ms deadband → tracked via anchor relabel,
        // NOT silence.
        assert_eq!(
            r.pending_silence_27mhz, 0,
            "media_player forward jump must NOT queue silence (gate 6) — it relabels the anchor"
        );
        assert_eq!(
            r.out_pts_90k,
            out_after_first.wrapping_add(jump_90k),
            "media_player forward jump must advance out_pts_90k by the source delta"
        );
    }

    /// Forward source-PTS jump of exactly 80 ms (threshold boundary)
    /// does NOT queue silence — the threshold is strict `>`, so 80 ms
    /// jitter stays inert (covers MP2/AC-3 every-other-frame timing
    /// noise without false positives).
    #[test]
    fn forward_jump_at_threshold_boundary_does_not_queue_silence() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 100_000), &mut Vec::new());
        let expected = r.expected_next_src_pts_90k.unwrap();
        // 80 ms = 7200 ticks — exactly the threshold.
        let next_pts = expected.wrapping_add(7_200);
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], next_pts), &mut Vec::new());
        assert_eq!(
            r.pending_silence_27mhz, 0,
            "exactly-80ms forward jump must NOT queue silence — threshold is strict `>`"
        );
    }

    /// Sub-500ms BACKWARD source-PTS jump must NOT queue silence
    /// (silence would push output ahead, not catch up). Backward
    /// small jumps are absorbed silently as PTS noise.
    #[test]
    fn sub_500ms_backward_source_jump_does_not_queue_silence() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 200_000), &mut Vec::new());
        let expected = r.expected_next_src_pts_90k.unwrap();
        // 200 ms backward.
        let next_pts = expected.wrapping_sub(18_000);
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], next_pts), &mut Vec::new());
        assert_eq!(
            r.pending_silence_27mhz, 0,
            "backward source-PTS jump must NOT queue silence — silence only catches up forward drift"
        );
    }

    /// Catastrophic forward jump (> 500 ms) still hits the
    /// discontinuity-guard re-anchor branch (sets `out_pts_90k =
    /// candidate`, resets `samples_since_anchor` to 0) — NOT the
    /// silence-pad branch. The two paths are mutually exclusive:
    /// > 500 ms uses re-anchor; (80 ms, 500 ms] uses silence-pad;
    /// ≤ 80 ms is inert.
    #[test]
    fn forward_jump_over_500ms_still_uses_reanchor_not_silence() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 300_000), &mut Vec::new());
        let expected = r.expected_next_src_pts_90k.unwrap();
        // 600 ms = 54 000 ticks — past the 500 ms threshold.
        let next_pts = expected.wrapping_add(54_000);
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], next_pts), &mut Vec::new());
        assert_eq!(
            r.pending_silence_27mhz, 0,
            "> 500ms jump must use the catastrophic re-anchor branch, not silence-pad"
        );
    }

    /// **Loop-drift fix (2026-06-02).** A FORWARD source-PTS discontinuity
    /// (> 500 ms — media_player loop splice) must RE-ANCHOR even when the
    /// re-encoder has run effective output PTS slightly AHEAD of the new
    /// source PTS (the per-loop silence-pad frame-quantization overshoot).
    /// Gating the suppress on `forward_delta < 0` (the pre-fix behaviour) let
    /// that overshoot accumulate as unbounded A/V drift on looped AC-3/MP2/AAC
    /// re-encodes; the gate is now the SOURCE `delta` sign.
    #[test]
    fn forward_discontinuity_reanchors_even_when_output_ran_ahead() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        r.resolved_sample_rate = 48_000;
        r.out_pts_anchored = true;
        r.out_pts_90k = 1_000_000;
        // effective = 1_000_000 + 36_640 * 90000/48000 = 1_068_700.
        r.samples_since_anchor = 36_640;
        r.expected_next_src_pts_90k = Some(1_000_000);
        // Forward jump +60 000 ticks (> 500 ms): candidate = 1_060_000, which
        // is BEHIND current effective (1_068_700) so forward_delta < 0. The
        // pre-fix guard suppressed here; the fix re-anchors on the positive
        // source delta.
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 1_060_000), &mut Vec::new());
        assert_eq!(
            r.out_pts_90k, 1_060_000,
            "forward source discontinuity must re-anchor out_pts_90k to the source PTS \
             even when effective output ran ahead (else per-loop overshoot drifts unbounded)"
        );
        assert_eq!(
            r.samples_since_anchor, 0,
            "re-anchor must reset samples_since_anchor"
        );
    }

    /// A BACKWARD source-PTS reset (ffmpeg `-stream_loop -c copy` wrap, which
    /// resets audio PTS to a small value every loop) must still be SUPPRESSED:
    /// re-anchoring would rewind output PTS by seconds and make receivers drop
    /// frames. Only the FORWARD direction was changed by the loop-drift fix.
    #[test]
    fn backward_discontinuity_still_suppressed() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        r.resolved_sample_rate = 48_000;
        r.out_pts_anchored = true;
        r.out_pts_90k = 2_000_000;
        r.samples_since_anchor = 0;
        r.expected_next_src_pts_90k = Some(1_060_000);
        // Backward jump: source PTS resets to 1_000_000 (delta = -60 000).
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 1_000_000), &mut Vec::new());
        assert_eq!(
            r.out_pts_90k, 2_000_000,
            "backward source reset must NOT re-anchor (suppress to keep output PTS monotonic)"
        );
    }

    /// **media_player source-PTS forward-step tracking.** With no
    /// `av_sync_pacer` wired (the media_player input-transcode path), a
    /// forward source-PTS step below the 500 ms re-anchor threshold must be
    /// TRACKED by advancing `out_pts_90k` (a pure PTS relabel that
    /// reproduces the drift-free passthrough timeline) and must NOT queue
    /// silence (a >20 ms silence gap in decoded PCM would violate gate 6).
    /// This is the fix for the Sky Witness 1080i25 loop −26.8 ms/min A/V
    /// drift: the free-running sample clock loses the file-side per-loop
    /// splice step, which passthrough audio carries verbatim.
    #[test]
    fn media_player_forward_step_relabels_anchor_without_silence() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        assert!(r.av_sync_pacer.is_none(), "media_player path has no pacer");
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        r.out_pts_anchored = true;
        r.out_pts_90k = 1_000_000;
        r.samples_since_anchor = 0;
        r.resolved_sample_rate = 48_000;
        r.expected_next_src_pts_90k = Some(1_000_000);
        let out_before = r.out_pts_90k;
        let step_90k: u64 = 4_500; // 50 ms forward splice step
        let _ = r.consume_pes(
            &build_audio_pes(0xC0, &[0u8; 32], 1_000_000 + step_90k),
            &mut Vec::new(),
        );
        assert_eq!(
            r.pending_silence_27mhz, 0,
            "media_player forward step must NOT queue silence (gate 6) — it relabels the anchor"
        );
        assert_eq!(
            r.out_pts_90k,
            out_before.wrapping_add(step_90k),
            "media_player forward step must advance out_pts_90k by the source delta (track source PTS)"
        );
    }

    /// No-regression guard for the master-clock-paced (live SRT / RTP /
    /// output transcode) path: with an `av_sync_pacer` set, a > 80 ms
    /// forward source step still uses the silence-pad branch (queues
    /// silence, leaves `out_pts_90k` untouched) — the media_player relabel
    /// branch is scoped to the no-pacer path and must NOT engage here.
    #[test]
    fn paced_path_forward_jump_uses_silence_not_relabel() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        r.set_av_sync_pacer(std::sync::Arc::new(AvSyncPacer::new(handle)));
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        r.out_pts_anchored = true;
        r.out_pts_90k = 1_000_000;
        r.resolved_sample_rate = 48_000;
        // Leave the catch-up anchors unset so the master-clock catch-up
        // stays inert; we are exercising the forward-jump silence branch.
        r.first_pes_master_27mhz = None;
        r.first_pes_src_pts_90k = None;
        r.expected_next_src_pts_90k = Some(1_000_000);
        let out_before = r.out_pts_90k;
        // 100 ms forward (> 80 ms threshold).
        let _ = r.consume_pes(
            &build_audio_pes(0xC0, &[0u8; 32], 1_000_000 + 9_000),
            &mut Vec::new(),
        );
        assert!(
            r.pending_silence_27mhz > 0,
            "paced path must queue silence on a >80ms forward jump"
        );
        assert_eq!(
            r.out_pts_90k, out_before,
            "paced path must NOT relabel out_pts_90k (relabel is media_player-only)"
        );
    }

    // ─── Silence-pad apply: out_pts_90k must NOT advance (0.87.0 fix) ──
    //
    // Bug: pre-0.87 silence-pad apply did `out_pts_90k += jump_90k −
    // frame_step` AND added silence_samples to the accumulator. That
    // was only correct when exactly one silence frame was emitted
    // (K = 1). For K > 1 the effective_out_pts = `out_pts_90k +
    // samples_since_anchor * 90000 / sr` over-advanced by
    // `(K − 1) * frame_step` per jump — e.g. 268 ms loop splice =>
    // K = 12 AAC-LC frames => 21 120 ticks (235 ms) over.
    //
    // Fix: silence content only. Each silence frame the encoder
    // produces advances `samples_since_anchor` by frame_size, so
    // long-term effective_out_pts advances by exactly
    // `silence_samples * 90000 / sr` — the correct catch-up amount,
    // independent of K.

    /// Apply path with pending silence: `out_pts_90k` must NOT
    /// change, and the accumulator must receive exactly
    /// `silence_samples = jump_27mhz * sr / 27_000_000` zero samples
    /// per channel. The encoder side will advance the effective PTS
    /// via `samples_since_anchor` over multiple frame emits as it
    /// drains the accumulator — covered in the next test.
    #[test]
    fn silence_pad_apply_does_not_advance_out_pts_90k() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        // Hand-bring the replacer into the apply-ready state — bypass
        // the full PES path so the test doesn't depend on a real ADTS
        // frame round-tripping through fdk-aac.
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        r.out_pts_90k = 1_000_000_000;
        r.samples_since_anchor = 4_096;
        r.codecs_ready = true;
        r.resolved_sample_rate = 48_000;
        r.resolved_channels = 2;
        r.accumulator = vec![Vec::new(); 2];
        let out_pts_before = r.out_pts_90k;
        let samples_before = r.samples_since_anchor;
        // 268 ms forward source-PTS jump = 24 120 90 kHz ticks =
        // 7 236 000 27 MHz ticks — the Sky Witness loop-splice case
        // that motivated this fix.
        r.pending_silence_27mhz = 7_236_000;
        r.apply_pending_silence_pad();
        assert_eq!(r.pending_silence_27mhz, 0, "silence queue must drain");
        // **The core invariant.** `out_pts_90k` is the anchor base;
        // silence-pad must not nudge it. The pre-0.87 code did
        // `out_pts_90k += jump_90k - frame_step`, which double-counted
        // with the silence content's contribution to effective PTS.
        assert_eq!(
            r.out_pts_90k, out_pts_before,
            "silence-pad must NOT advance out_pts_90k — silence frames carry the PTS advance"
        );
        assert_eq!(
            r.samples_since_anchor, samples_before,
            "silence-pad must NOT touch samples_since_anchor — drain_encoder advances it as silence frames emit"
        );
        // 7_236_000 * 48_000 / 27_000_000 = 12_864 silence samples
        // per channel.
        let expected_silence_samples: usize = 12_864;
        for (ch, acc) in r.accumulator.iter().enumerate() {
            assert_eq!(
                acc.len(), expected_silence_samples,
                "channel {ch}: silence samples added (got {}, want {})",
                acc.len(), expected_silence_samples
            );
            assert!(
                acc.iter().all(|&s| s == 0.0f32),
                "channel {ch}: padded samples must be exact zeros (digital silence)"
            );
        }
    }

    // ─── Master-clock-aware catch-up (0.86.2 follow-up) ──────────────
    //
    // Source-side splice asymmetries (e.g. media_player loops where
    // pcr_max - audio_max ≈ 200 ms produces a 268 ms video PES gap but
    // only ~4.5 ms audio PES gap per loop) leave the encoder stamping
    // output PESes at sample-rate cadence, drifting behind passthrough
    // video by ~27 ms/min on Sky Witness. The forward-jump detector
    // above doesn't catch this because the audio PES PTS sequence
    // stays continuous. The catch-up below uses the per-flow master
    // clock as the wallclock-truth reference, queueing silence when
    // effective output PTS falls behind master by > 200 ms.
    //
    // Master clock kind is abstracted by `av_sync_pacer.now_27mhz()`:
    // wallclock (default, host CLOCK_TAI), ptp (grandmaster), or
    // source_pcr_pll (PLL-recovered). Catch-up logic is clock-agnostic
    // — same code, three different clocks.

    /// First PES with `av_sync_pacer` set records both anchors so
    /// the catch-up has a starting point to measure elapsed time
    /// against.
    #[test]
    fn first_pes_records_master_clock_and_src_pts_anchors() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        let pacer = std::sync::Arc::new(AvSyncPacer::new(handle));
        r.set_av_sync_pacer(pacer);
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        assert!(r.first_pes_master_27mhz.is_none());
        assert!(r.first_pes_src_pts_90k.is_none());
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 1_234_567), &mut Vec::new());
        assert!(r.first_pes_master_27mhz.is_some(), "master anchor must be recorded on first PES");
        assert_eq!(
            r.first_pes_src_pts_90k,
            Some(1_234_567),
            "src-PTS anchor must equal the first PES's source PTS"
        );
    }

    /// First PES without `av_sync_pacer` leaves anchors at `None` —
    /// the catch-up below is a no-op without a master clock to
    /// reference. Preserves zero-cost behaviour on output paths that
    /// don't wire the pacer.
    #[test]
    fn first_pes_without_pacer_leaves_catchup_anchors_none() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 1_234_567), &mut Vec::new());
        assert!(r.first_pes_master_27mhz.is_none());
        assert!(r.first_pes_src_pts_90k.is_none());
    }

    /// Catastrophic > 500 ms re-anchor branch must also re-anchor the
    /// catch-up state — otherwise the next lag measurement would
    /// reference pre-discontinuity history and fire immediately with a
    /// huge stale lag value.
    #[test]
    fn over_500ms_reanchor_also_resets_catchup_anchors() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        let pacer = std::sync::Arc::new(AvSyncPacer::new(handle));
        r.set_av_sync_pacer(pacer);
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        // First PES seeds the catch-up anchors.
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 100_000), &mut Vec::new());
        let original_master = r.first_pes_master_27mhz;
        let original_src_pts = r.first_pes_src_pts_90k;
        // Force the output to be sitting somewhere — the > 500 ms
        // forward branch needs current_effective_out_pts_90k to make
        // candidate (= pts) forward of where output is.
        r.resolved_sample_rate = 48_000;
        r.samples_since_anchor = 0;
        r.out_pts_90k = 100_000;
        // Trigger > 500 ms forward jump (1 sec). Expected_next was
        // updated at the end of the first PES to ~100_000 + small_span;
        // a +90_000 jump from there crosses the 45_000 threshold.
        // Sleep a tiny bit so master clock advances and we can verify
        // it's been refreshed.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 100_000 + 90_000), &mut Vec::new());
        // Both catch-up anchors must have been refreshed (master moved
        // forward by at least 2 ms = 54 000 27 MHz ticks).
        assert_ne!(
            r.first_pes_master_27mhz, original_master,
            "> 500 ms re-anchor must refresh first_pes_master_27mhz"
        );
        assert_ne!(
            r.first_pes_src_pts_90k, original_src_pts,
            "> 500 ms re-anchor must refresh first_pes_src_pts_90k"
        );
    }

    /// `reset_source_state` (input switch / codec change) clears the
    /// catch-up anchors alongside the other source-relative state.
    #[test]
    fn reset_source_state_clears_catchup_anchors() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.first_pes_master_27mhz = Some(123_456_789);
        r.first_pes_src_pts_90k = Some(42_000);
        r.reset_source_state("test");
        assert!(r.first_pes_master_27mhz.is_none());
        assert!(r.first_pes_src_pts_90k.is_none());
    }

    /// **Catch-up fires when effective PTS lags master by > 200 ms.**
    /// Hand-rig the state so the lag computation evaluates to a known
    /// large value, then call consume_pes and verify
    /// `pending_silence_27mhz` got the deficit.
    #[test]
    fn catchup_queues_silence_when_lag_exceeds_threshold() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        let pacer = std::sync::Arc::new(AvSyncPacer::new(handle));
        r.set_av_sync_pacer(pacer.clone());
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        // Anchor at a master_now from 500 ms in the past so when
        // consume_pes runs, the lag computation sees ~500 ms elapsed.
        let now = pacer.now_27mhz();
        r.first_pes_master_27mhz = Some(now.saturating_sub(13_500_000)); // 500 ms ago
        r.first_pes_src_pts_90k = Some(0);
        r.out_pts_anchored = true;
        r.out_pts_90k = 0;
        r.samples_since_anchor = 0; // effective_out_pts_elapsed = 0
        r.resolved_sample_rate = 48_000;
        // expected_next_src_pts_90k arbitrary — only the catch-up
        // branch matters here; we want a tiny same-PES delta so the
        // discontinuity branches don't fire.
        r.expected_next_src_pts_90k = Some(2_160);
        // Process a PES with pts close to expected — discontinuity
        // guard inert, but catch-up sees ~500 ms master_elapsed and
        // 0 output_elapsed → lag ≈ 500 ms, > 200 ms threshold.
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 2_160), &mut Vec::new());
        // Catch-up FIRES (master lag ~500 ms > the 200 ms threshold) but the
        // queued silence is CAPPED at one frame per fire (commit 0944ab1:
        // "cap catch-up silence per fire to one AC-3 frame") so a large lag is
        // bled off over multiple PESes as whole-frame steps rather than one big
        // jump — see memory project_master_clock_catchup_wallclock_fallback.
        // So it queues the per-fire cap (~32 ms = 864_000 ticks), NOT the full
        // ~500 ms lag.
        assert!(
            r.pending_silence_27mhz > 0,
            "catch-up must fire (queue some silence) when master lag exceeds the threshold; got {}",
            r.pending_silence_27mhz
        );
        assert!(
            r.pending_silence_27mhz <= 13_500_000 / 2,
            "per-fire catch-up silence must be capped (not the full ~500 ms lag); got {}",
            r.pending_silence_27mhz
        );
    }

    /// Catch-up suppresses absurd lag values (> 2 sec sanity ceiling).
    /// PLL warmup, PTP servo transients, and 33-bit PTS-wrap interactions
    /// can produce multi-second / multi-hour apparent lag values; the
    /// ceiling prevents us from queueing seconds-or-more of silence
    /// based on garbage anchor values. Observed on cell 6 (sync-test
    /// source_pcr_pll, 17-hour lag during PLL warmup) before the
    /// ceiling was added — catch-up fired 543 times in 3 min, each
    /// queueing 5 sec of silence.
    #[test]
    fn catchup_suppresses_lag_above_sanity_ceiling() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        let pacer = std::sync::Arc::new(AvSyncPacer::new(handle));
        r.set_av_sync_pacer(pacer.clone());
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        // Anchor master ~10 sec in the past — well above the 2 sec
        // sanity ceiling.
        let now = pacer.now_27mhz();
        r.first_pes_master_27mhz = Some(now.saturating_sub(270_000_000)); // 10 s ago
        r.first_pes_src_pts_90k = Some(0);
        r.out_pts_anchored = true;
        r.out_pts_90k = 0;
        r.samples_since_anchor = 0;
        r.resolved_sample_rate = 48_000;
        r.expected_next_src_pts_90k = Some(2_160);
        let pending_before = r.pending_silence_27mhz;
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 2_160), &mut Vec::new());
        assert_eq!(
            r.pending_silence_27mhz, pending_before,
            "lag > sanity ceiling must NOT queue silence — caller's master clock or anchors are pathological"
        );
    }

    /// Catch-up stays inert when output PTS tracks master clock.
    /// Hand-rig state so `output_pts_elapsed` matches
    /// `master_elapsed` within the 200 ms threshold; verify no
    /// silence is queued. This is the source_pcr_pll / PTP case
    /// where audio + master + video all track the same rate.
    #[test]
    fn catchup_does_not_fire_when_output_tracks_master() {
        use crate::engine::av_sync_mux::AvSyncPacer;
        use crate::engine::master_clock::{
            MasterClockHandle, MasterClockKind, WallclockMaster,
        };
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let handle = MasterClockHandle::new(
            std::sync::Arc::new(WallclockMaster::new()),
            MasterClockKind::Wallclock,
        );
        let pacer = std::sync::Arc::new(AvSyncPacer::new(handle));
        r.set_av_sync_pacer(pacer.clone());
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        // Anchor master ~50 ms in the past; arrange effective PTS to
        // also be ~50 ms ahead of the anchor (samples_since_anchor *
        // 90000 / 48000 = 50 ms → 2400 samples = ~4500 90k ticks).
        let now = pacer.now_27mhz();
        r.first_pes_master_27mhz = Some(now.saturating_sub(1_350_000)); // 50 ms ago
        r.first_pes_src_pts_90k = Some(0);
        r.out_pts_anchored = true;
        r.out_pts_90k = 0;
        r.samples_since_anchor = 2_400; // 50 ms of audio at 48 kHz
        r.resolved_sample_rate = 48_000;
        r.expected_next_src_pts_90k = Some(2_160);
        let pending_before = r.pending_silence_27mhz;
        let _ = r.consume_pes(&build_audio_pes(0xC0, &[0u8; 32], 2_160), &mut Vec::new());
        assert_eq!(
            r.pending_silence_27mhz, pending_before,
            "lag ≈ 0 (within 200 ms threshold) must NOT queue silence — \
             catch-up is inert when source/output track master"
        );
    }

    /// **Long-form effective-PTS catch-up invariant.** After K silence
    /// frames have been encoded out of the silence-padded accumulator,
    /// `effective_out_pts = out_pts_90k + samples_since_anchor *
    /// 90000 / sr` must advance by exactly `silence_samples *
    /// 90000 / sr` from its pre-pad value — i.e. the silence
    /// content alone carries the jump catch-up, with no out_pts_90k
    /// contribution. Simulates `drain_encoder` consuming the silence
    /// in 1024-sample AAC-LC chunks. Demonstrates that the pre-0.87
    /// `out_pts_90k += jump_90k - frame_step` was overshoot for K>1.
    #[test]
    fn silence_pad_effective_pts_advance_matches_jump_amount_long_term() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.audio_pid = Some(0x0101);
        r.source_stream_type = 0x0F;
        r.out_pts_90k = 1_000_000_000;
        r.samples_since_anchor = 0;
        r.codecs_ready = true;
        r.resolved_sample_rate = 48_000;
        r.resolved_channels = 2;
        r.accumulator = vec![Vec::new(); 2];

        let pre_effective = r.next_output_pts_90k(48_000);
        // 268 ms jump.
        r.pending_silence_27mhz = 7_236_000;
        let expected_advance_90k: u64 = 7_236_000 / 300; // 24 120 ticks
        r.apply_pending_silence_pad();

        // Simulate drain_encoder: while accumulator has ≥ 1024
        // samples in channel 0, pull a frame and bump
        // samples_since_anchor by 1024.
        const FRAME: usize = 1024;
        while r.accumulator[0].len() >= FRAME {
            for ch in r.accumulator.iter_mut() {
                ch.drain(..FRAME);
            }
            r.samples_since_anchor =
                r.samples_since_anchor.saturating_add(FRAME as u64);
        }
        // To complete the catch-up the leftover (0 to 1023 samples)
        // would be merged with the next real PES's samples and drained
        // as part of a normal frame — the long-term cumulative advance
        // matches `expected_advance_90k` once those leftover samples
        // are encoded. Verify both: (a) the immediate effective PTS
        // is close to expected (within one frame_step), and (b) the
        // leftover sample count is < FRAME so the round-up completes
        // on the next real PES.
        let post_effective = r.next_output_pts_90k(48_000);
        let actual_advance_90k = post_effective.wrapping_sub(pre_effective);
        let frame_step_90k: u64 = (FRAME as u64) * 90_000 / 48_000;
        assert!(
            actual_advance_90k <= expected_advance_90k
                && actual_advance_90k + frame_step_90k > expected_advance_90k,
            "after silence drain the effective PTS must have advanced by at most \
             expected_advance_90k ({expected_advance_90k}) and the leftover < 1 frame_step \
             ({frame_step_90k}); got advance = {actual_advance_90k}"
        );
        // The leftover < FRAME completes the catch-up on the next encoder call.
        assert!(
            r.accumulator[0].len() < FRAME,
            "leftover silence samples in accumulator (< 1024) finishes catch-up on next PES"
        );
    }

    // ── PCR on the audio PID (defect x) and the pre-PMT gate (4) ──

    fn adts_frames() -> Vec<&'static [u8]> {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let mut v = Vec::new();
        let mut off = 0usize;
        while off + 7 <= ADTS.len() {
            let len = (((ADTS[off + 3] as usize) & 0x03) << 11)
                | ((ADTS[off + 4] as usize) << 3)
                | ((ADTS[off + 5] as usize) >> 5);
            if len == 0 || off + len > ADTS.len() {
                break;
            }
            v.push(&ADTS[off..off + len]);
            off += len;
        }
        v
    }

    /// Put a PCR into a packet's stuffing adaptation field (≥ 7 bytes).
    fn put_pcr_in_stuffing(pkt: &mut [u8; 188], pcr: u64) -> bool {
        if pkt[3] & 0x20 == 0 || pkt[4] < 7 {
            return false;
        }
        pkt[5] = 0x10;
        crate::engine::ts_parse::write_pcr(pkt, pcr)
    }

    #[test]
    fn a_pcr_on_the_audio_pid_survives_as_af_only_with_continuous_cc() {
        use crate::engine::ts_parse::{extract_pcr, pcr_only_packet, ts_cc};
        let mut r = TsAudioReplacer::new(&enc("mp2"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        // PCR_PID = the audio PID (a radio service).
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        out.clear();
        let (mut pts, mut cc) = (900_000u64, 0u8);
        let mut pcrs_in = Vec::new();
        let mut in_payload = 0;
        for (i, f) in adts_frames().iter().enumerate() {
            let pcr = pts * 300 - 100 * 27_000;
            let mut pkts = packetize_ts(0x0101, &build_audio_pes(0xC0, f, pts), &mut cc);
            // The PCR rides in the PES's last (payload) packet when its
            // stuffing has room; otherwise, and on every even frame, in an
            // AF-only carrier ahead of the PES (DI on the fourth frame).
            let last = pkts.len() - 1;
            if i % 2 == 1 && put_pcr_in_stuffing(&mut pkts[last], pcr) {
                in_payload += 1;
                pcrs_in.push((pcr, false));
            } else {
                let p = pcr_only_packet(0x0101, cc.wrapping_sub(pkts.len() as u8 + 1) & 0x0F, pcr, i == 4);
                r.process(&p, &mut out);
                pcrs_in.push((pcr, i == 4));
            }
            for p in &pkts {
                r.process(p, &mut out);
            }
            pts += 1920;
        }
        let audio: Vec<&[u8]> = out.chunks(188).filter(|p| ts_pid(p) == 0x0101).collect();
        let pcrs_out: Vec<(u64, bool)> = audio
            .iter()
            .filter_map(|p| {
                extract_pcr(p).map(|v| (v, crate::engine::ts_parse::ts_discontinuity_indicator(p)))
            })
            .collect();
        assert!(in_payload >= 3, "some PCRs rode in payload packets: {in_payload}");
        assert_eq!(pcrs_out, pcrs_in, "every input PCR, value and DI unchanged");
        let mut payload = 0;
        let mut last_payload_cc: Option<u8> = None;
        for p in &audio {
            if ts_has_payload(p) {
                assert!(extract_pcr(p).is_none(), "re-encoded PES carry no PCR");
                if let Some(prev) = last_payload_cc {
                    assert_eq!(ts_cc(p), (prev + 1) & 0x0F, "payload CC continuous");
                }
                last_payload_cc = Some(ts_cc(p));
                payload += 1;
            } else {
                assert_eq!(ts_cc(p), last_payload_cc.unwrap_or(15), "AF-only repeats the payload CC");
            }
        }
        assert!(payload > 0, "re-encoded audio emitted");
    }

    #[test]
    fn nothing_but_psi_passes_before_the_pmt() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        let early = build_audio_pes(0xC0, &[0xFF, 0xF1, 0, 0, 0, 0, 0], 90_000);
        let mut cc = 0u8;
        for p in packetize_ts(0x0101, &early, &mut cc) {
            r.process(&p, &mut out);
        }
        r.process(&synth_pat(0x1000), &mut out);
        assert_eq!(out, synth_pat(0x1000).to_vec(), "the source audio never went out");
        assert_eq!(r.stats_handle().pre_pmt_dropped_packets.load(Ordering::Relaxed), 1);
        out.clear();
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        assert!(!out.is_empty(), "the PMT itself goes out");
        // A non-replaced PID now passes through.
        out.clear();
        let other = {
            let mut p = [0xAAu8; 188];
            p[0] = TS_SYNC_BYTE;
            p[1] = 0x02;
            p[2] = 0x00;
            p[3] = 0x10;
            p
        };
        r.process(&other, &mut out);
        assert_eq!(out, other.to_vec());
    }

    #[test]
    fn a_takeover_continues_the_passthrough_cc() {
        // DTS first (not replaceable: its PID passes through), then an
        // input switch to AAC on the same PID: the replacer's first packet
        // continues the CC the passthrough left on the wire.
        use crate::engine::ts_parse::{pcr_only_packet, ts_cc};
        let mut r = TsAudioReplacer::new(&enc("mp2"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x82), &mut out);
        let mut cc = 0u8;
        for p in packetize_ts(0x0101, &build_audio_pes(0xC0, &[0x7F; 300], 90_000), &mut cc) {
            r.process(&p, &mut out);
        }
        assert_eq!(cc, 2, "two passthrough packets, CC 0 and 1");
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        out.clear();
        r.process(&pcr_only_packet(0x0101, 5, 27_000_000, false), &mut out);
        let got: Vec<&[u8]> = out.chunks(188).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(ts_cc(got[0]), 1, "the AF-only carrier repeats the last CC on the wire");
        assert_eq!(r.out_audio_cc, 2, "the first re-encoded payload continues at 2");
        // The replacer carries the PID (CC 2..=8 out), then an input switch
        // moves the PMT PID with the audio still on 0x0101: nothing passed
        // through since the takeover, so its own CC carries on.
        r.out_audio_cc = 9;
        r.process(&synth_pat(0x1001), &mut out);
        r.process(&synth_pmt_audio(0x1001, 0x0101, 0x0F), &mut out);
        assert_eq!(r.replaced_pid(), Some(0x0101));
        assert_eq!(r.out_audio_cc, 9, "no rewind to a stale passthrough CC");
    }
}
