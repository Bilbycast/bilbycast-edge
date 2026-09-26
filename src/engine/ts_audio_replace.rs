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
//! 2. Every audio TS packet feeds an access-unit cutter (`audio_au`): each
//!    AU is decoded as soon as its last byte arrives — across PES
//!    boundaries, whatever the source muxer's PES packing — and its PCM goes
//!    through the optional channel / rate stage into the target encoder,
//!    whose frames are re-packetized as TS on the same audio PID.
//! 3. Raw audio TS packets are dropped from the output — they are replaced
//!    by the re-encoded equivalents.
//!
//! **Timing.** Output PTS come from a sample-count model anchored on the
//! source PTS (one rounding per frame, no drift), minus the latency the codec
//! libraries declare for this pipeline — the decoder's (fdk-aac: 0 once
//! opened without concealment delay or limiter), the resampler's and the
//! encoder's priming (fdk-aac `nDelay`, libavcodec `initial_padding`) — so a
//! receiver presents each sample at its source PTS. The content is held to
//! the source timeline by comparing, at the first AU of every PES, the PES
//! PTS with where the decoded content ends: a gap is filled (silence on a
//! live source, a timestamp step on `media_player`), an overlap is dropped,
//! a >500 ms step re-anchors. Nothing in it reads the host's clock, so host
//! load and backpressure cannot move the audio. The one clock it reads is
//! the program's own PCR: a gap in the source audio is filled with silence
//! as that clock passes it, not in a burst when the audio returns (see
//! [`ClockFill`]).
//!
//! This is the streaming variant of the HLS segment-level remuxer in
//! `output_hls.rs`. State is kept across chunks (ES cutter, decoder,
//! encoder, PCM accumulator, PMT identity) so every call to [`process`] is
//! incremental. [`flush`] drains any trailing AU / encoder buffer on
//! shutdown.
//!
//! The replacer is fully synchronous — output tasks that want to use it
//! inside an async context should call it from `tokio::task::block_in_place`
//! or delegate to a dedicated worker thread. AAC / codec operations take
//! single-digit milliseconds per frame and must not run inline on a
//! single-threaded runtime.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};
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
    pub pre_pmt_dropped_packets: AtomicU64,
    /// Corrections the source-timeline tracker applied: a gap filled with
    /// silence or a timestamp step, an overlap dropped.
    pub timeline_corrections: AtomicU64,
    /// Samples (per channel, at the decoded rate) of silence inserted: gaps
    /// in the source timeline and access units that failed to decode.
    pub silence_inserted_samples: AtomicU64,
    /// Samples (per channel, at the decoded rate) dropped where the source
    /// timeline overlapped content already placed.
    pub dropped_samples: AtomicU64,
}

use crate::config::models::AudioEncodeConfig;

use super::audio_au::{AuCutter, AuFormat, AuHeader, CutAu};
use super::audio_encode::AudioCodec;
use super::audio_transcode::{PlanarAudioTranscoder, TranscodeJson};
use super::transcode_engage::{
    EngageEvent, PassthroughCc, PrePmtGate, TranscodeEngageWatch, TranscodeKind,
};
use super::ts_parse::{
    extract_pcr, parse_pat_programs, pcr_only_packet, ts_discontinuity_indicator,
    ts_has_payload, ts_payload_offset, ts_pid, ts_pusi, PAT_PID, TS_PACKET_SIZE, TS_SYNC_BYTE,
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

/// How the replacer fills a forward gap in the source audio timeline (a PES
/// whose PTS lies beyond the end of the audio decoded so far).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GapFill {
    /// Insert digital silence for the gap: on a live source the gap is time
    /// that passed with no audio (lost or undecodable access units, an
    /// off-air stretch, a splice), and the video advanced through it.
    #[default]
    Silence,
    /// Step the output timestamps over the gap instead, leaving the decoded
    /// audio untouched: a `media_player` file splice steps its PTS while the
    /// audio content is continuous, and silence there would put a gap into
    /// otherwise gap-free programme audio.
    Relabel,
}

/// 33-bit PTS arithmetic.
const PTS_MASK: u64 = 0x1_FFFF_FFFF;
/// A source-timeline offset beyond this, either way, re-anchors the output
/// at the new PTS: a restart, a splice across seconds, a file loop that
/// resets its PTS. The PCR and the video follow the source across such a
/// step, so the audio must too.
const REANCHOR_90K: i64 = 45_000; // 500 ms
/// An offset this large is corrected at the PES that shows it.
const IMMEDIATE_90K: u64 = 9_000; // 100 ms
/// Offsets up to this are PES timestamp jitter and never corrected.
const DEADBAND_90K: u64 = 450; // 5 ms
/// A smaller offset is corrected once it has kept its sign for this long
/// (and for at least two PES), whatever the muxer's PES packing.
const PERSIST_90K: i64 = 13_500; // 150 ms
/// `av_skew` is not published for this long after an anchor.
const AV_SKEW_HOLDOFF_90K: u64 = 90_000; // 1 s
/// Resampler chunk (input frames) of the replacer's rate conversion.
const SRC_CHUNK_FRAMES: usize = super::audio_transcode::STREAM_CHUNK_FRAMES;

/// Signed difference `a − b` of two 33-bit PTS values, wrap-aware.
fn pts_diff(a: u64, b: u64) -> i64 {
    let d = (a.wrapping_sub(b) & PTS_MASK) as i64;
    if d >= 1 << 32 { d - (1 << 33) } else { d }
}

/// The source audio timeline the re-encoded output is held to.
///
/// `samples` counts the content placed since `base_90k` at the decoded rate:
/// every decoded sample, plus inserted silence, minus dropped samples — each
/// counted when it is queued, so a correction is never counted twice. The
/// content therefore ends at `base_90k + samples / rate`, and the PTS of the
/// first AU of each PES says where it should end. The count is signed: an
/// overlap found just after an anchor can queue a drop larger than what has
/// been placed since (a PES stamped up to 500 ms before the anchor), and the
/// content then ends before `base_90k` until that much has been decoded.
#[derive(Clone, Debug, Default)]
struct Timeline {
    anchored: bool,
    base_90k: u64,
    samples: i64,
    /// Decoded sample rate; 0 until the first decode after an anchor.
    rate: u32,
    /// The current run of same-signed offsets below [`IMMEDIATE_90K`]: its
    /// sign (0 = none), the PTS it started at and its length in PES.
    run_sign: i8,
    run_since_90k: u64,
    run_marks: u32,
}

/// What one PES PTS asks of the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimelineAction {
    /// Nothing decoded since the anchor yet: (re)anchor at this PTS.
    Anchor,
    /// Within tolerance.
    Hold,
    /// A step of more than 500 ms, forward or back: re-anchor the output
    /// here.
    Reanchor,
    /// A gap of this many ticks before this PES's audio.
    Gap(i64),
    /// This PES's audio starts this many ticks before the content's end.
    Overlap(i64),
}

impl Timeline {
    /// Where the content placed so far ends, on the source timeline.
    fn end_90k(&self) -> u64 {
        if self.rate == 0 {
            return self.base_90k;
        }
        let span = (self.samples as i128 * 90_000).div_euclid(self.rate as i128);
        (self.base_90k as i128 + span).rem_euclid(PTS_MASK as i128 + 1) as u64
    }

    fn clear_run(&mut self) {
        self.run_sign = 0;
        self.run_marks = 0;
    }

    /// Judge the PTS `pts` of a PES's first AU against the content's end.
    /// With [`GapFill::Relabel`] a forward offset beyond the deadband is
    /// acted on at once: a file source's timestamps carry no jitter to
    /// filter, and a splice step left for 150 ms would present that much
    /// audio early by the step.
    fn check(&mut self, pts: u64, gap_fill: GapFill) -> TimelineAction {
        if !self.anchored || self.rate == 0 {
            return TimelineAction::Anchor;
        }
        let off = pts_diff(pts, self.end_90k());
        if off.abs() > REANCHOR_90K {
            return TimelineAction::Reanchor;
        }
        let magnitude = off.unsigned_abs();
        if magnitude <= DEADBAND_90K {
            self.clear_run();
            return TimelineAction::Hold;
        }
        if off > 0 && gap_fill == GapFill::Relabel {
            self.clear_run();
            return TimelineAction::Gap(off);
        }
        let sign = off.signum() as i8;
        if sign != self.run_sign {
            self.run_sign = sign;
            self.run_since_90k = pts;
            self.run_marks = 1;
        } else {
            self.run_marks += 1;
        }
        let persisted = self.run_marks >= 2 && pts_diff(pts, self.run_since_90k) >= PERSIST_90K;
        if magnitude < IMMEDIATE_90K && !persisted {
            return TimelineAction::Hold;
        }
        self.clear_run();
        if off > 0 { TimelineAction::Gap(off) } else { TimelineAction::Overlap(off) }
    }
}

/// Clock fill (see [`ClockFill`]): nothing is filled until the source has
/// been watched this long since its timeline anchored.
const CLOCK_FILL_WARMUP_90K: i64 = 180_000; // 2 s
/// What the source does is judged over its last this many seconds of
/// program clock: long enough to hold every stretch of its packing, short
/// enough to forget a flow start's or a splice's transient.
const CLOCK_FILL_WINDOW_S: usize = 10;
/// How far the headroom may drop below the lowest the source left in the
/// window before a gap is filled — the silence then brings it back to that
/// floor.
const CLOCK_FILL_TOL_90K: i64 = 900; // 10 ms
/// How much longer than any stretch of the source's own in the window the
/// content end must stand still before it is a gap and not the source's
/// packing.
const CLOCK_FILL_QUIET_TOL_90K: i64 = 900; // 10 ms
/// Most silence the clock places before the audio returns: past this the
/// audio has stopped rather than paused, and its return re-anchors.
const CLOCK_FILL_MAX_90K: i64 = REANCHOR_90K;

/// One second of what the source did against its clock: the lowest headroom
/// and the longest stretch without the content end moving.
#[derive(Clone, Copy, Debug, Default)]
struct FillSecond {
    /// Program clock second this bucket holds, on the unwrapped clock
    /// ([`ClockFill::ext_pcr_90k`]); `None` unused.
    sec: Option<u64>,
    floor_90k: i64,
    quiet_90k: i64,
}

/// Clock-driven gap fill: silence for a gap in the source audio placed
/// while the program's clock runs through it, not when the audio returns.
///
/// A gap is found at the next PES's PTS ([`Timeline::check`]) — so its
/// silence used to be encoded all at once when the audio came back, stamped
/// for the time the gap began: a burst of frames up to the whole gap behind
/// the program's PCR. At a media-player loop of an MPTS played whole the
/// program's audio pauses as long as the most demanding program needs (Spain
/// 244 ms, 770_H 464 ms); the burst arrived up to 203 ms late and the
/// transcode PCR stage raised its delay `D` by 163–290 ms at every loop,
/// pushing the video's T-STD residency past 1 s.
///
/// Instead the replacer watches the headroom — where the decoded content
/// ends minus the program clock — after every packet, the clock read from
/// the program's PCRs and interpolated between them by packet count, as a
/// T-STD model does. Over the last [`CLOCK_FILL_WINDOW_S`] seconds it keeps
/// the lowest headroom the source left and the longest stretch its content
/// end stood still. When the content end has stood still longer than that
/// and the headroom has fallen [`CLOCK_FILL_TOL_90K`] below its floor, the
/// source is missing audio it always had by now: silence is placed up to
/// the floor as the clock goes on, until the audio returns. Each silent
/// frame therefore leaves where, and against the same PCR, a source frame
/// would have at the source's worst. Nothing is speculated: a source that
/// keeps its own lead never sees silence placed ahead of audio still to
/// come. The returning audio settles what remains at once — a gap filled,
/// an overlap dropped.
#[derive(Clone, Debug, Default)]
struct ClockFill {
    /// The program's last PCR (90 kHz), the packets since, the clock one
    /// packet stood for over the last PCR interval (90 kHz × 2^16) and that
    /// interval — the interpolation never runs past it.
    last_pcr_90k: Option<u64>,
    pkts: u64,
    tpp_q16: u64,
    step_90k: i64,
    /// The last PCR unwrapped: its 33-bit value plus the laps the clock has
    /// run since this time base began. The window's seconds and the
    /// warm-up are counted on it — the 33-bit value wraps every 26.5 h,
    /// and keyed on it the window forgot everything at the wrap (the
    /// second jumps 95 443 -> 0) and filled the next PES's wait with
    /// silence.
    ext_pcr_90k: u64,
    /// The first clock reading since the timeline anchored (unwrapped):
    /// the warm-up's start.
    since_90k: Option<u64>,
    /// Where the content ended at the previous reading, and the clock at
    /// which the source last moved it; whether the current stretch ends in
    /// a timeline correction (then it was no stretch of the source's).
    last_end_90k: Option<u64>,
    moved_at_90k: Option<u64>,
    corrected: bool,
    window: [FillSecond; CLOCK_FILL_WINDOW_S],
    /// Filling: the clock placed `filled_90k` of silence since the source
    /// audio last arrived; `outage` once that reached the cap.
    active: bool,
    filled_90k: i64,
    outage: bool,
}

impl ClockFill {
    /// Forget what was learned (a new timeline), keeping the clock.
    fn relearn(&mut self) {
        *self = ClockFill {
            last_pcr_90k: self.last_pcr_90k,
            pkts: self.pkts,
            tpp_q16: self.tpp_q16,
            step_90k: self.step_90k,
            ext_pcr_90k: self.ext_pcr_90k,
            ..ClockFill::default()
        };
    }

    /// A PCR on the program's PCR_PID: `false` (and everything learned
    /// dropped) when it starts a new time base.
    fn note_pcr(&mut self, c: u64, di: bool) -> bool {
        let prev = self.last_pcr_90k.replace(c);
        let step = prev.map(|p| pts_diff(c, p));
        if di || step.is_some_and(|s| !(0..=90_000).contains(&s)) {
            *self = ClockFill { last_pcr_90k: Some(c), ext_pcr_90k: c, ..ClockFill::default() };
            return false;
        }
        match step {
            Some(s) => self.ext_pcr_90k += s as u64,
            None => self.ext_pcr_90k = c,
        }
        if let Some(s) = step
            && self.pkts >= 4
            && (1..=9_000).contains(&s)
        {
            self.tpp_q16 = ((s as u64) << 16) / self.pkts;
            self.step_90k = s;
        }
        self.pkts = 0;
        true
    }

    /// The program clock now: the last PCR plus the packets since, never
    /// past one PCR interval.
    fn now(&self) -> Option<u64> {
        let c = self.last_pcr_90k?;
        let ahead = ((self.pkts * self.tpp_q16) >> 16).min(self.step_90k.max(0) as u64);
        Some((c + ahead) & PTS_MASK)
    }

    /// A clock reading `c` (the last PCR or later) unwrapped, see
    /// [`Self::ext_pcr_90k`].
    fn unwrapped(&self, c: u64) -> u64 {
        match self.last_pcr_90k {
            Some(last) => self.ext_pcr_90k + pts_diff(c, last).max(0) as u64,
            None => c,
        }
    }

    /// The window's lowest headroom and longest stretch, over the seconds
    /// up to `sec`.
    fn window_at(&self, sec: u64) -> Option<(i64, i64)> {
        let mut it = self
            .window
            .iter()
            .filter(|b| b.sec.is_some_and(|s| sec.wrapping_sub(s) < CLOCK_FILL_WINDOW_S as u64));
        let first = it.next()?;
        Some(it.fold((first.floor_90k, first.quiet_90k), |(f, q), b| {
            (f.min(b.floor_90k), q.max(b.quiet_90k))
        }))
    }

    /// Record a headroom and / or a stretch in the bucket of second `sec`.
    fn record(&mut self, sec: u64, headroom: Option<i64>, quiet: Option<i64>) {
        let b = &mut self.window[(sec % CLOCK_FILL_WINDOW_S as u64) as usize];
        if b.sec != Some(sec) {
            *b = FillSecond { sec: Some(sec), floor_90k: i64::MAX, quiet_90k: 0 };
        }
        if let Some(h) = headroom {
            b.floor_90k = b.floor_90k.min(h);
        }
        if let Some(q) = quiet {
            b.quiet_90k = b.quiet_90k.max(q);
        }
    }
}

/// The codec pipeline's latency, declared by the libraries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Latency {
    /// Decoder implementation delay + resampler delay + encoder priming, in
    /// 90 kHz ticks: output sample `k`'s content is the source's from this
    /// much earlier than the model position `k` is stamped at.
    declared_90k: u64,
    /// What the wire stamps subtract. Equal to `declared_90k`; `av_skew`
    /// reports any difference as lip-sync error.
    applied_90k: u64,
}

/// One decoded PCM frame; its channel count is `planar.len()`.
struct Decoded {
    planar: Vec<Vec<f32>>,
    sample_rate: u32,
}

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
    /// The input-side `audio_encode_stats` entry, where the resolved output
    /// format is published (see [`Self::with_input_encode_handle`]).
    /// Output-side replacers reach theirs through `output_stats`.
    input_encode_handle: Option<Arc<crate::stats::collector::AudioEncodeStatsHandle>>,
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

    /// Access-unit cutter over the source audio PID's elementary stream
    /// (`audio_au`): whole AUs as soon as each is complete, across PES
    /// boundaries, each PES's PTS attached to the first AU that commences in
    /// it. Built for the locked source's framing; dropped on a reset. It
    /// checks the packets' continuity itself (`AuCutter::push_packet`): a
    /// duplicate packet is dropped, a lost one marks the AU in flight as
    /// possibly damaged.
    cutter: Option<AuCutter>,

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

    /// Output PTS model anchor (90 kHz): the model position of the first
    /// output sample emitted since the last anchor. Frame `k` is stamped
    /// `out_pts_90k + samples_since_anchor * 90000 / rate` (one rounding,
    /// so a 44.1 kHz stream never drifts) minus the pipeline latency
    /// ([`Latency`]). Anchored on a source PTS; the model position of the
    /// content decoded from a source sample is that sample's source PTS
    /// plus the declared latency, which the wire stamp takes back off.
    out_pts_90k: u64,
    /// Output samples emitted since the current anchor was set.
    samples_since_anchor: u64,

    /// The source audio timeline the output is held to.
    timeline: Timeline,
    /// How a forward gap in that timeline is filled.
    gap_fill: GapFill,
    /// The program's PCR_PID, from its PMT: the clock [`ClockFill`] runs on.
    pcr_pid: Option<u16>,
    /// Silence for a gap placed as the program clock passes it.
    clock_fill: ClockFill,
    /// Source samples still to drop from the head of the decoded PCM (an
    /// overlap the timeline found).
    pending_drop: u64,
    /// Duration (90 kHz) of access units that failed to decode before the
    /// first one decoded, when their format was not known yet: filled with
    /// silence ahead of the first decoded PCM.
    pending_fill_90k: u64,

    /// The last `xf_len` decoded samples (per channel, source rate), held
    /// back so a correction can blend into them: silence fades them out and
    /// the audio after it fades in, a drop crossfades them into the audio
    /// that follows. 2 ms.
    tail: Vec<Vec<f32>>,
    xf_len: usize,
    /// Samples of fade-in still owed to the audio after inserted silence.
    fade_in_left: usize,

    /// The codec pipeline's declared latency and what the wire stamps
    /// subtract. Latched when the pipeline opens.
    latency: Latency,
    /// A `transcode` / override stage that failed to build was reported.
    init_failure_reported: bool,

    /// Edge-added A/V skew reporter (`stats::av_skew`). At the first AU of
    /// each PES this replacer publishes where that AU's audio will be
    /// presented minus its source PTS: the timeline bookkeeping (a
    /// correction still pending) plus any latency the stamps do not
    /// cancel.
    av_skew: Option<Arc<crate::stats::av_skew::AvSkewReporter>>,
    /// Publication of `av_skew` resumes at this source PTS after an anchor.
    av_skew_from_90k: Option<u64>,

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
    /// The source's AC-3 / E-AC-3 `dialnorm` (dB), from its latest AU
    /// that carries one, and what an AC-3 encoder has been told. The
    /// dialogue level a receiver normalises to: writing libavcodec's
    /// default -31 over a -24 dB programme made the transcode play 7 dB
    /// louder than its source on every receiver that honours it.
    #[cfg(feature = "media-codecs")]
    source_dialnorm: Option<i8>,
    #[cfg(feature = "media-codecs")]
    encoder_dialnorm: Option<i8>,

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
    /// The channel / rate stage between decoder and encoder (`None` = no
    /// conversion). Built on the first decoded frame; rebuilt when the
    /// source changes format in-band (see [`Self::reformat`]).
    transcoder: Option<PlanarAudioTranscoder>,
    /// The decoded format `(rate, channels)` the stage was built for.
    stage_in: (u32, u8),
    /// The first stage's resampler delay (output frames), folded into the
    /// stamps when the pipeline opened: content from source time `t` is
    /// placed at model position `t` plus this, whichever stage converts it.
    stage_delay_latched: u64,
    /// Output frames of zero history still to drop from the head of a
    /// stage that replaced another: the one it replaced was run out, so the
    /// pipeline's delay is already in the stream.
    stage_skip: u64,
    /// Source frames sent into the current stage and output frames kept
    /// from it, and the history it keeps at its head (the first stage's
    /// resampler delay; none for a replacement): where its content ends,
    /// for running it out.
    stage_fed: u64,
    stage_kept: u64,
    stage_head: u64,

    /// One-shot "input was switched" request. The flow's per-output
    /// switch watcher flips this to `true` when the active input
    /// changes; the replacer consumes it on the next `process()` entry
    /// and runs the same `reset_source_state()` path that fires on
    /// codec/PID change. Without this, a same-codec same-PID swap leaves
    /// stale decoder + PTS-anchor state from the previous input and the
    /// receiver hears wrong-epoch PTS audio frames against a master-
    /// clock-paced PCR.
    external_reset: Arc<AtomicBool>,
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
            input_encode_handle: None,
            source_stream_type: 0,
            ts_signalling: cfg.ts_signalling.unwrap_or_default(),
            flavour: None,
            engage: TranscodeEngageWatch::new(TranscodeKind::Audio, cfg.source_audio_pid),
            event_sink: None,
            cutter: None,
            out_audio_cc: 0,
            pmt_version: OutVersion::new(),
            out_pts_90k: 0,
            samples_since_anchor: 0,
            timeline: Timeline::default(),
            gap_fill: GapFill::default(),
            pcr_pid: None,
            clock_fill: ClockFill::default(),
            pending_drop: 0,
            pending_fill_90k: 0,
            tail: Vec::new(),
            xf_len: 0,
            fade_in_left: 0,
            latency: Latency::default(),
            init_failure_reported: false,
            av_skew: None,
            av_skew_from_90k: None,
            #[cfg(feature = "fdk-aac")]
            aac_decoder: None,
            #[cfg(feature = "media-codecs")]
            ff_decoder: None,
            #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
            aac_encoder: None,
            #[cfg(feature = "media-codecs")]
            av_encoder: None,
            #[cfg(feature = "media-codecs")]
            source_dialnorm: None,
            #[cfg(feature = "media-codecs")]
            encoder_dialnorm: None,
            accumulator: Vec::new(),
            resolved_channels: 0,
            resolved_sample_rate: 0,
            codecs_ready: false,
            transcode_cfg: transcode,
            transcoder: None,
            stage_in: (0, 0),
            stage_delay_latched: 0,
            stage_skip: 0,
            stage_fed: 0,
            stage_kept: 0,
            stage_head: 0,
            external_reset: Arc::new(AtomicBool::new(false)),
        })
    }

    /// How a forward gap in the source audio timeline is filled (see
    /// [`GapFill`]); [`GapFill::Silence`] unless set. Only the
    /// `media_player` input's transcode sets [`GapFill::Relabel`].
    pub fn set_gap_fill(&mut self, fill: GapFill) {
        self.gap_fill = fill;
    }

    /// Attach the per-input edge-added A/V skew reporter. See the
    /// `av_skew` field doc-comment.
    pub fn set_av_skew_reporter(
        &mut self,
        reporter: Arc<crate::stats::av_skew::AvSkewReporter>,
    ) {
        self.av_skew = Some(reporter);
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
        self.refresh_stats_labels();
    }

    /// Attach the input-side `AudioEncodeStatsHandle`, registered with the
    /// configured target: `audio_encode.sample_rate` / `channels` are 0
    /// there when they follow the source, and the replacer publishes the
    /// format it resolved once the first frame decodes.
    pub fn with_input_encode_handle(
        &mut self,
        handle: Arc<crate::stats::collector::AudioEncodeStatsHandle>,
    ) {
        self.input_encode_handle = Some(handle);
        self.refresh_stats_labels();
    }

    /// Best-effort refresh of the registered stats handles, whenever
    /// `self.source_stream_type`, the decoded format or the resolved output
    /// format changes: the audio-decode handle gets the source codec label
    /// and the decoded PCM shape, the audio-encode handle the output format
    /// the encoder was opened at. Output-side handles are reached through
    /// [`Self::with_output_stats`], input-side ones were attached with
    /// [`Self::with_input_decode_handle`] / [`Self::with_input_encode_handle`];
    /// any may be absent.
    fn refresh_stats_labels(&self) {
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
        let decoded = self.stage_in;
        let resolved = (self.resolved_sample_rate, self.resolved_channels);
        if let Some(stats) = self.output_stats.as_ref() {
            if let Some(h) = stats.audio_decode_stats_handle() {
                if !codec_label.is_empty() {
                    h.set_input_codec(codec_label);
                }
                if decoded.0 != 0 && decoded.1 != 0 {
                    h.set_output_shape(decoded.0, decoded.1);
                }
            }
            if let Some(h) = stats.audio_encode_stats_handle()
                && resolved.0 != 0
                && resolved.1 != 0
            {
                h.set_target_shape(resolved.0, resolved.1);
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
            if decoded.0 != 0 && decoded.1 != 0 {
                h.set_output_shape(decoded.0, decoded.1);
            }
        }
        if let Some(h) = self.input_encode_handle.as_ref()
            && resolved.0 != 0
            && resolved.1 != 0
        {
            h.set_target_shape(resolved.0, resolved.1);
        }
    }

    /// The target bitrate in kbps (configured, or the codec's default).
    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
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

        for pkt in input_ts.chunks_exact(TS_PACKET_SIZE) {
            // A PCR on the program's PCR_PID, whatever else the packet is:
            // after the packet has gone out, the clock fill runs on it.
            let clock = (pkt[0] == TS_SYNC_BYTE && Some(ts_pid(pkt)) == self.pcr_pid)
                .then(|| extract_pcr(pkt).map(|v| (v, ts_discontinuity_indicator(pkt))))
                .flatten();
            self.process_packet(pkt, output, now);
            self.clock_fill.pkts += 1;
            if let Some((pcr, di)) = clock {
                self.on_program_pcr(pcr, di, output);
            }
        }
        // And on the clock where this chunk leaves it.
        if let Some(c) = self.clock_fill.now() {
            self.clock_tick(c, output);
        }

        self.engage.note_packets((input_ts.len() / TS_PACKET_SIZE) as u64);
        self.poll_engage(now);
    }

    /// One 188-byte packet of [`Self::process_at`].
    fn process_packet(&mut self, pkt: &[u8], output: &mut Vec<u8>, now: std::time::Instant) {
        if pkt[0] != TS_SYNC_BYTE {
            // Lost alignment — emit as-is and move on.
            output.extend_from_slice(pkt);
            return;
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
            return;
        }

        if self.gate.drops(pid, now, "ts_audio_replace") {
            self.stats.pre_pmt_dropped_packets.fetch_add(1, Ordering::Relaxed);
            return;
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
            return;
        }

        // Everything else: passthrough.
        self.passthrough_cc.note(pid, pkt);
        output.extend_from_slice(pkt);
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
        self.pcr_pid = (view.pcr_pid != 0x1FFF).then_some(view.pcr_pid);
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
            self.refresh_stats_labels();
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

    /// Flush the trailing access units, the crossfade tail and the encoder.
    /// Call once on graceful shutdown. No-op if codecs were never
    /// initialised.
    #[allow(dead_code)]
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        self.drain_cutter(true, output);
        if self.codecs_ready {
            let channels = self.tail.len();
            let tail = std::mem::replace(&mut self.tail, vec![Vec::new(); channels]);
            self.send_pcm(tail, output);
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
                        let pts = self.wire_pts(self.next_output_pts_90k(sr));
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

    /// Model position (90 kHz, before latency compensation) of the next
    /// output frame: the anchor plus the samples emitted since, divided
    /// once, so a long-running encoder at a non-integer-tick rate (44.1 kHz)
    /// never drifts against the source clock. The anchor verbatim while the
    /// output rate is unknown.
    fn next_output_pts_90k(&self, sample_rate: u32) -> u64 {
        if sample_rate == 0 {
            return self.out_pts_90k;
        }
        let advance =
            self.samples_since_anchor.saturating_mul(90_000) / sample_rate as u64;
        self.out_pts_90k.wrapping_add(advance)
    }

    /// The PTS a frame at model position `model_90k` goes out with: the
    /// model minus the pipeline latency, so its decoded content is
    /// presented at the source PTS it came from.
    fn wire_pts(&self, model_90k: u64) -> u64 {
        model_90k.wrapping_sub(self.latency.applied_90k) & PTS_MASK
    }

    /// Content placed but not yet in an emitted frame, in 90 kHz ticks: the
    /// encoder's input accumulator (output rate) plus the resampler's queue
    /// and the crossfade tail (source rate). Where the next source sample
    /// lands is `next_output_pts_90k + pending_out_90k` plus what the
    /// resampler's delay line holds. That is part of the declared latency
    /// (the zero history the first stage started from accounts for it) —
    /// exactly, until a stage replaces the first: [`Self::stage_offset_90k`]
    /// is the difference.
    fn pending_out_90k(&self) -> u64 {
        let out_rate = self.resolved_sample_rate as u128;
        let in_rate = self.timeline.rate as u128;
        if !self.codecs_ready || out_rate == 0 || in_rate == 0 {
            return 0;
        }
        let acc = self.accumulator.first().map_or(0, |c| c.len()) as u128;
        let src = self.tail.first().map_or(0, |c| c.len()) as u128
            + self.transcoder.as_ref().map_or(0, |t| t.buffered_frames()) as u128;
        ((acc * in_rate + src * out_rate) * 90_000 / (out_rate * in_rate)) as u64
    }

    /// What the current stage's delay line holds (its delay, less the zero
    /// history it still has to drop) minus the delay the stamps assume
    /// (the first stage's), in signed 90 kHz ticks. 0 until a stage
    /// replaces the first.
    fn stage_offset_90k(&self) -> i64 {
        let out_rate = self.resolved_sample_rate as i64;
        if out_rate == 0 {
            return 0;
        }
        let delay = self.transcoder.as_ref().map_or(0, |t| t.output_delay() as i64);
        let held = delay - self.stage_skip as i64;
        (held - self.stage_delay_latched as i64) * 90_000 / out_rate
    }

    // ── Internal helpers ─────────────────────────────────────────────

    /// Drop every pipeline stage that depends on the current source
    /// stream — ES cutter, decoder, transcoder, encoder, resolved format,
    /// accumulator, timeline. Called when the source audio codec or PID
    /// changes mid-flow (seamless input switching between inputs with
    /// different audio codecs, or a PAT/PMT program re-layout), and on an
    /// external input-switch request.
    ///
    /// The target codec itself (`self.codec`)
    /// is preserved — that's the output's configured codec, which
    /// never changes.
    fn reset_source_state(&mut self, reason: &str) {
        tracing::info!(
            "ts_audio_replace: {reason}; reopening audio decoder / encoder"
        );
        // Bytes of the old input must never be glued onto the new one.
        self.cutter = None;
        // Re-anchor output PTS to the new input's first PES so the
        // audio stays aligned with the video replacer, which also
        // re-anchors on the video-PID codec swap.
        self.timeline = Timeline::default();
        self.clock_fill = ClockFill::default();
        self.pending_drop = 0;
        self.pending_fill_90k = 0;
        self.av_skew_from_90k = None;
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
        // let `init_pipeline` rebuild them.
        #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
        {
            self.aac_encoder = None;
        }
        #[cfg(feature = "media-codecs")]
        {
            self.av_encoder = None;
            self.source_dialnorm = None;
            self.encoder_dialnorm = None;
        }
        self.transcoder = None;
        self.stage_in = (0, 0);
        self.stage_delay_latched = 0;
        self.stage_skip = 0;
        self.stage_fed = 0;
        self.stage_kept = 0;
        self.stage_head = 0;
        self.accumulator.clear();
        self.tail.clear();
        self.xf_len = 0;
        self.fade_in_left = 0;
        self.latency = Latency::default();
        self.init_failure_reported = false;
        self.samples_since_anchor = 0;
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

    /// Route one audio TS packet into the access-unit cutter and process
    /// every AU it completes.
    fn feed_audio_packet(&mut self, pkt: &[u8], output: &mut Vec<u8>) {
        if !ts_has_payload(pkt) {
            return;
        }
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            return;
        }
        let Some(fmt) = AuFormat::for_stream_type(self.source_stream_type) else {
            return;
        };
        if self.cutter.as_ref().is_none_or(|c| c.format() != fmt) {
            self.cutter = Some(AuCutter::new(fmt));
        }
        let cutter = self.cutter.as_mut().expect("cutter just built");
        // A lost packet marks a discontinuity; a duplicate brings nothing.
        if !cutter.push_packet(pkt) {
            return;
        }
        self.drain_cutter(false, output);
    }

    /// Process every access unit the cutter has ready.
    fn drain_cutter(&mut self, at_end: bool, output: &mut Vec<u8>) {
        loop {
            let Some(cutter) = self.cutter.as_mut() else {
                return;
            };
            let next = cutter.next(at_end);
            if cutter.take_discarded() > 0 {
                // Bytes that were not a whole AU of this stream.
                self.decode_stats.inc_error();
            }
            let Some(au) = next else {
                return;
            };
            self.consume_au(au, output);
        }
    }

    /// Place, decode and re-encode one access unit.
    fn consume_au(&mut self, au: CutAu, output: &mut Vec<u8>) {
        // A PES's PTS belongs to the first AU that commences in it.
        if au.pes_start
            && let Some(pts) = au.pts
        {
            self.on_pes_pts(pts, output);
        }
        if !self.timeline.anchored {
            // Nothing to place it by yet: the stream began mid-PES, or with
            // PES that carry no PTS.
            return;
        }
        #[cfg(feature = "media-codecs")]
        self.follow_dialnorm(&au);
        self.decode_stats.inc_input();
        match self.decode_au(&au.data) {
            Ok(frames) => {
                for d in frames {
                    self.decode_stats.inc_output();
                    self.push_decoded(d, output);
                }
            }
            Err(()) => {
                self.decode_stats.inc_error();
                self.fill_lost_au(&au.header, output);
            }
        }
    }

    /// Track the source's AC-3 / E-AC-3 `dialnorm` and pass a change on to
    /// an open AC-3 encoder, which was opened with `per_frame_metadata` for
    /// it (programme and advertising often carry different values). The
    /// change reaches the output with the encoder's next frame, within one
    /// frame plus the pipeline's latency of where the source made it.
    #[cfg(feature = "media-codecs")]
    fn follow_dialnorm(&mut self, au: &CutAu) {
        if self.cutter.as_ref().map(|c| c.format()) != Some(AuFormat::Ac3) {
            return;
        }
        let Some(d) = crate::engine::audio_decode::ac3_dialnorm(&au.data) else {
            return;
        };
        self.source_dialnorm = Some(d);
        if self.encoder_dialnorm.is_none_or(|e| e == d) {
            return;
        }
        if let Some(enc) = self.av_encoder.as_mut() {
            match enc.set_option("dialnorm", &d.to_string()) {
                Ok(()) => self.encoder_dialnorm = Some(d),
                Err(e) => {
                    tracing::warn!("TsAudioReplacer: could not carry dialnorm {d} dB: {e}");
                    // Once: the encoder keeps the value it has.
                    self.encoder_dialnorm = Some(d);
                }
            }
        }
    }

    /// A PCR on the program's PCR_PID went out: note the clock, then run
    /// the clock fill on it.
    fn on_program_pcr(&mut self, pcr_27mhz: u64, di: bool, output: &mut Vec<u8>) {
        let c = (pcr_27mhz / 300) & PTS_MASK;
        if self.clock_fill.note_pcr(c, di) {
            self.clock_tick(c, output);
        }
    }

    /// Learn how the source's audio runs against the program clock `c`, and
    /// fill a gap in it as the clock passes (see [`ClockFill`]).
    fn clock_tick(&mut self, c: u64, output: &mut Vec<u8>) {
        if self.gap_fill != GapFill::Silence
            || !self.timeline.anchored
            || self.timeline.rate == 0
            || !self.codecs_ready
        {
            return;
        }
        let end = self.timeline.end_90k();
        let f = &mut self.clock_fill;
        let at = f.unwrapped(c);
        let sec = at / 90_000;
        if f.last_end_90k != Some(end) {
            // The source moved the content end since the last reading. A
            // stretch that ended in a correction, or in the middle of a
            // fill, is no stretch of the source's own. (Audio with no PES
            // start of its own during a fill — the rest of a PES that
            // stalled, a file's leading continuation — goes on after the
            // silence and leaves the fill running: it fills only below the
            // floor, and the next PES's PTS settles what is left.)
            if let (Some(_), Some(m)) = (f.last_end_90k, f.moved_at_90k)
                && !f.corrected
                && !f.active
            {
                let q = pts_diff(c, m);
                f.record(sec, None, Some(q));
            }
            f.moved_at_90k = Some(c);
            f.corrected = false;
            f.last_end_90k = Some(end);
        }
        let headroom = pts_diff(end, c);
        let since = *f.since_90k.get_or_insert(at);
        let quiet = pts_diff(c, f.moved_at_90k.unwrap_or(c));
        if !f.active {
            let warm = at.saturating_sub(since) >= CLOCK_FILL_WARMUP_90K as u64;
            let window = f.window_at(sec);
            let (floor, quiet_max) = match window {
                Some(w) if warm => w,
                _ => {
                    f.record(sec, Some(headroom), None);
                    return;
                }
            };
            if quiet <= quiet_max + CLOCK_FILL_QUIET_TOL_90K {
                // The source's own stretch: its headroom is the source's.
                f.record(sec, Some(headroom), None);
                return;
            }
            if headroom >= floor - CLOCK_FILL_TOL_90K || f.outage {
                return;
            }
            f.active = true;
            f.filled_90k = 0;
            tracing::debug!(
                quiet_ms = quiet / 90,
                quiet_max_ms = quiet_max / 90,
                headroom_ms = headroom / 90,
                floor_ms = floor / 90,
                "ts_audio_replace: source audio missing past its own worst; filling on the program clock"
            );
            self.stats.timeline_corrections.fetch_add(1, Ordering::Relaxed);
        }
        let f = &mut self.clock_fill;
        let floor = f.window_at(sec).map_or(0, |(fl, _)| fl);
        let need = floor - headroom;
        if need <= 0 {
            return;
        }
        if f.filled_90k + need > CLOCK_FILL_MAX_90K {
            // The audio stopped rather than paused: its return re-anchors.
            f.outage = true;
            return;
        }
        let rate = self.timeline.rate as i128;
        let n = (need as i128 * rate / 90_000) as u64;
        if n == 0 {
            return;
        }
        self.insert_silence(n, output);
        self.timeline.samples += n as i64;
        let end = self.timeline.end_90k();
        let f = &mut self.clock_fill;
        f.filled_90k += (n as i128 * 90_000 / rate) as i64;
        // The clock moved the content end, not the source.
        f.last_end_90k = Some(end);
    }

    /// Hold the content to the source timeline at a PES PTS (see
    /// [`Timeline::check`]), then publish `av_skew`.
    fn on_pes_pts(&mut self, pts: u64, output: &mut Vec<u8>) {
        if std::mem::take(&mut self.clock_fill.active) {
            // The audio is back after a gap the clock filled: settle what
            // is left at once — the persistence rule is for timestamp
            // jitter, and this is no jitter.
            let off = pts_diff(pts, self.timeline.end_90k());
            let filled = std::mem::take(&mut self.clock_fill.filled_90k);
            self.clock_fill.outage = false;
            if off.abs() <= REANCHOR_90K {
                tracing::info!(
                    clock_filled_ms = filled / 90,
                    remainder_ms = off / 90,
                    "ts_audio_replace: source audio back after a gap filled on the program clock"
                );
                self.timeline.clear_run();
                if off > DEADBAND_90K as i64 {
                    self.fill_gap(off, output);
                } else if off < -(DEADBAND_90K as i64) {
                    self.drop_overlap(off);
                    // The audio came back with less lead than the window's
                    // floor: the source's floor is that much lower.
                    if let Some(c) = self.clock_fill.now() {
                        let sec = self.clock_fill.unwrapped(c) / 90_000;
                        if let Some((floor, _)) = self.clock_fill.window_at(sec) {
                            self.clock_fill.record(sec, Some(floor + off), None);
                        }
                    }
                }
                self.clock_fill.corrected = true;
                self.publish_av_skew(pts);
                return;
            }
        }
        match self.timeline.check(pts, self.gap_fill) {
            TimelineAction::Anchor => {
                // Nothing decoded since the anchor: it follows the PES PTS
                // until content arrives.
                self.anchor_at(pts);
                return;
            }
            TimelineAction::Hold => {}
            TimelineAction::Reanchor => {
                let content_end = self.timeline.end_90k();
                tracing::info!(
                    pts,
                    content_end,
                    step_90k = pts_diff(pts, content_end),
                    "ts_audio_replace: source PTS stepped by more than 500 ms; \
                     re-anchoring audio output PTS"
                );
                self.anchor_at(pts);
            }
            TimelineAction::Gap(off) => {
                self.clock_fill.corrected = true;
                self.fill_gap(off, output);
            }
            TimelineAction::Overlap(off) => {
                self.clock_fill.corrected = true;
                self.drop_overlap(off);
            }
        }
        self.publish_av_skew(pts);
    }

    /// (Re)anchor the output model at source PTS `pts`: the content placed
    /// but not yet emitted goes out just before it, and everything from
    /// this PES on is measured from here.
    fn anchor_at(&mut self, pts: u64) {
        let at = pts as i128 - self.pending_out_90k() as i128 - self.stage_offset_90k() as i128;
        self.out_pts_90k = at.rem_euclid(PTS_MASK as i128 + 1) as u64;
        self.samples_since_anchor = 0;
        let rate = self.timeline.rate;
        self.timeline = Timeline {
            anchored: true,
            base_90k: pts,
            rate,
            ..Timeline::default()
        };
        self.pending_drop = 0;
        self.pending_fill_90k = 0;
        self.av_skew_from_90k = Some(pts.wrapping_add(AV_SKEW_HOLDOFF_90K) & PTS_MASK);
        // A new timeline: what the source does against the clock is
        // learned again.
        self.clock_fill.relearn();
    }

    /// A gap of `off` ticks before this PES's audio: silence, or a
    /// timestamp step on `media_player`.
    fn fill_gap(&mut self, off: i64, output: &mut Vec<u8>) {
        self.stats.timeline_corrections.fetch_add(1, Ordering::Relaxed);
        match self.gap_fill {
            GapFill::Silence => {
                let n = (off as u128 * self.timeline.rate as u128 / 90_000) as u64;
                tracing::debug!(
                    gap_90k = off,
                    silence_samples = n,
                    "ts_audio_replace: source timeline gap filled with silence"
                );
                self.insert_silence(n, output);
                self.timeline.samples += n as i64;
            }
            GapFill::Relabel => {
                tracing::debug!(
                    gap_90k = off,
                    "ts_audio_replace: source timeline gap stepped over (timestamp relabel)"
                );
                self.out_pts_90k = self.out_pts_90k.wrapping_add(off as u64) & PTS_MASK;
                self.timeline.base_90k = self.timeline.base_90k.wrapping_add(off as u64) & PTS_MASK;
            }
        }
    }

    /// This PES's audio starts `-off` ticks before the content's end: drop
    /// that much from the head of the audio to come.
    fn drop_overlap(&mut self, off: i64) {
        let n = (off.unsigned_abs() as u128 * self.timeline.rate as u128 / 90_000) as u64;
        tracing::debug!(
            overlap_90k = off,
            drop_samples = n,
            "ts_audio_replace: source timeline overlap dropped"
        );
        self.stats.timeline_corrections.fetch_add(1, Ordering::Relaxed);
        self.stats.dropped_samples.fetch_add(n, Ordering::Relaxed);
        self.pending_drop += n;
        self.timeline.samples -= n as i64;
    }

    /// An access unit that did not decode: silence of its nominal length in
    /// its place, so the audio after it stays on time. When the header
    /// does not carry a length (LOAS), the timeline finds the gap at the
    /// next PES PTS instead.
    fn fill_lost_au(&mut self, h: &AuHeader, output: &mut Vec<u8>) {
        if h.samples == 0 || h.sample_rate == 0 {
            return;
        }
        if !self.codecs_ready || self.timeline.rate == 0 {
            self.pending_fill_90k += h.samples as u64 * 90_000 / h.sample_rate as u64;
            return;
        }
        let n = (h.samples as u128 * self.timeline.rate as u128 / h.sample_rate as u128) as u64;
        self.insert_silence(n, output);
        self.timeline.samples += n as i64;
    }

    /// Decode one access unit.
    fn decode_au(&mut self, au: &[u8]) -> Result<Vec<Decoded>, ()> {
        // AAC ADTS (0x0F) in-process via fdk-aac.
        #[cfg(feature = "fdk-aac")]
        if self.source_stream_type == 0x0F {
            if self.aac_decoder.is_none() {
                self.aac_decoder = Some(aac_audio::AacDecoder::open_adts().map_err(|_| ())?);
            }
            let decoder = self.aac_decoder.as_mut().expect("opened above");
            let d = decoder.decode_frame(au).map_err(|_| ())?;
            return Ok(vec![Decoded {
                planar: d.planar,
                sample_rate: decoder.sample_rate().unwrap_or(48_000),
            }]);
        }
        // MP2 / AC-3 / E-AC-3 / AAC-LATM via libavcodec, one AU per
        // `avcodec_send_packet`.
        #[cfg(feature = "media-codecs")]
        if let Some(ff_codec) =
            crate::engine::audio_decode::ff_codec_for_stream_type(self.source_stream_type)
        {
            if self.ff_decoder.is_none() {
                self.ff_decoder =
                    Some(crate::engine::audio_decode::open_ff_decoder(ff_codec).map_err(|_| ())?);
            }
            let decoder = self.ff_decoder.as_mut().expect("opened above");
            decoder.send_packet(au, 0).map_err(|_| ())?;
            let mut frames = Vec::new();
            while let Ok(frame) = decoder.receive_frame() {
                frames.push(Decoded {
                    planar: frame.planar,
                    sample_rate: frame.sample_rate,
                });
            }
            return Ok(frames);
        }
        let _ = au;
        Err(())
    }

    /// Count one decoded frame on the timeline and send it on, opening the
    /// pipeline on the first and rebuilding its channel / rate stage when
    /// the source changes format in-band.
    fn push_decoded(&mut self, d: Decoded, output: &mut Vec<u8>) {
        let n = d.planar.first().map_or(0, |c| c.len()) as u64;
        if n == 0 {
            return;
        }
        let format = (d.sample_rate, d.planar.len() as u8);
        if !self.codecs_ready {
            if self.init_pipeline(format.0, format.1).is_err() {
                return;
            }
        } else if format != self.stage_in && self.reformat(format.0, format.1, output).is_err() {
            return;
        }
        if self.timeline.rate == 0 {
            self.timeline.rate = d.sample_rate;
            // Access units that failed before this one, at their length.
            let fill = std::mem::take(&mut self.pending_fill_90k);
            if fill > 0 {
                let k = (fill as u128 * d.sample_rate as u128 / 90_000) as u64;
                self.insert_silence(k, output);
                self.timeline.samples += k as i64;
            }
        } else if self.timeline.rate != d.sample_rate {
            // The source changed rate in-band: the content so far ends
            // where it ends; count on at the new rate, and so does a drop
            // still owed.
            self.timeline.base_90k = self.timeline.end_90k();
            self.timeline.samples = 0;
            self.pending_drop = (self.pending_drop as u128 * d.sample_rate as u128
                / self.timeline.rate as u128) as u64;
            self.timeline.rate = d.sample_rate;
        }
        self.timeline.samples += n as i64;
        self.push_content(d.planar, output);
    }

    /// The channel / rate stage from decoded `(rate, channels)`: to `out`,
    /// the output format, once the encoder is open; before that, to what
    /// the configuration asks — the rule every re-encoding output shares
    /// (`audio_transcode::encoder_stage`). `Ok(None)`: no conversion.
    fn build_stage(
        &self,
        rate: u32,
        channels: u8,
        out: Option<(u32, u8)>,
    ) -> Result<Option<PlanarAudioTranscoder>, String> {
        super::audio_transcode::encoder_stage(
            self.transcode_cfg.as_ref(),
            self.sample_rate_override,
            self.channels_override,
            rate,
            channels,
            out,
        )
    }

    /// Open the channel / rate stage and the encoder for the first decoded
    /// format, and latch the pipeline's latency.
    fn init_pipeline(&mut self, sample_rate: u32, channels: u8) -> Result<(), ()> {
        match self.build_stage(sample_rate, channels, None) {
            Ok(Some(tc)) => {
                self.resolved_sample_rate = tc.out_sample_rate();
                self.resolved_channels = tc.out_channels();
                self.transcoder = Some(tc);
            }
            Ok(None) => {
                self.resolved_sample_rate = sample_rate;
                self.resolved_channels = channels;
            }
            Err(e) => {
                if !self.init_failure_reported {
                    self.init_failure_reported = true;
                    tracing::warn!(
                        "TsAudioReplacer: transcode init failed ({e}); dropping the audio"
                    );
                }
                return Err(());
            }
        }
        self.accumulator = vec![Vec::new(); self.resolved_channels as usize];
        self.init_encoder()?;
        self.stage_in = (sample_rate, channels);
        self.stage_delay_latched = self.transcoder.as_ref().map_or(0, |t| t.output_delay() as u64);
        self.stage_head = self.stage_delay_latched;
        self.stage_skip = 0;
        self.stage_fed = 0;
        self.stage_kept = 0;
        self.xf_len = (sample_rate / 500) as usize;
        self.tail = vec![Vec::new(); channels as usize];
        self.fade_in_left = 0;
        self.latch_latency(sample_rate);
        self.codecs_ready = true;
        self.refresh_stats_labels();
        Ok(())
    }

    /// The source changed format in-band: a 5.1 ↔ 2.0 switch between
    /// programme and ads (AC-3 and AAC decoders follow acmod /
    /// channel_config frame by frame), a splice to another rate. The
    /// encoder stays open at the output format — the PMT and every
    /// receiver keep it — and the channel / rate stage is rebuilt to
    /// convert the new format to it. What the old stage holds (the crossfade
    /// tail, its resampler's queue and delay line) goes out first, up to
    /// exactly the content it was given, and the new stage's zero history
    /// is dropped: the pipeline's delay is already in the stream, so every
    /// sample keeps its place and the stamps stay what they were.
    fn reformat(&mut self, rate: u32, channels: u8, output: &mut Vec<u8>) -> Result<(), ()> {
        let out = (self.resolved_sample_rate, self.resolved_channels);
        let stage = match self.build_stage(rate, channels, Some(out)) {
            Ok(s) => s,
            Err(e) => {
                if !self.init_failure_reported {
                    self.init_failure_reported = true;
                    tracing::warn!(
                        "TsAudioReplacer: no conversion from {rate} Hz x {channels} to the \
                         output's {} Hz x {} ({e}); dropping the audio while the source \
                         stays in that format",
                        out.0,
                        out.1
                    );
                }
                return Err(());
            }
        };
        tracing::info!(
            from_rate = self.stage_in.0,
            from_channels = self.stage_in.1,
            rate,
            channels,
            out_rate = out.0,
            out_channels = out.1,
            "ts_audio_replace: the source changed format in-band; converting it to the output's"
        );
        let tail = std::mem::take(&mut self.tail);
        self.send_pcm(tail, output);
        self.run_out_stage(output);
        // The new stage's zero history is dropped from the head of what it
        // produces (`keep_stage_output`), and `stage_offset_90k` counts it
        // as held until then. Both assume its first chunk's output covers
        // it — true for `sinc_len <= 256` at `SRC_CHUNK_FRAMES` = 256 (see
        // `audio_transcode::STREAM_CHUNK_FRAMES`). A longer filter would leave
        // `av_skew` and a re-anchor inside that first chunk a few samples
        // off.
        if let Some(t) = stage.as_ref() {
            debug_assert!(
                t.output_delay() as u128
                    <= (SRC_CHUNK_FRAMES as u128 * t.out_sample_rate() as u128)
                        / t.in_sample_rate().max(1) as u128,
                "the replacement stage's delay ({}) outlasts its first chunk's output",
                t.output_delay()
            );
        }
        self.stage_skip = stage.as_ref().map_or(0, |t| t.output_delay() as u64);
        self.transcoder = stage;
        self.stage_in = (rate, channels);
        self.stage_fed = 0;
        self.stage_kept = 0;
        self.stage_head = 0;
        self.xf_len = (rate / 500) as usize;
        self.tail = vec![Vec::new(); channels as usize];
        self.fade_in_left = self.fade_in_left.min(self.xf_len);
        self.refresh_stats_labels();
        Ok(())
    }

    /// Run the stage's resampler queue and delay line out with zeros, up to
    /// exactly the content it was given: its head (the first stage's
    /// history) plus its input at the output rate. A stage without rate
    /// conversion holds nothing.
    fn run_out_stage(&mut self, output: &mut Vec<u8>) {
        let Some(tc) = self.transcoder.as_ref() else {
            return;
        };
        let (in_rate, out_rate) = (tc.in_sample_rate() as u128, tc.out_sample_rate() as u128);
        if in_rate == out_rate {
            return;
        }
        let end = self.stage_head + ((self.stage_fed as u128 * out_rate + in_rate / 2) / in_rate) as u64;
        let zeros = vec![vec![0.0f32; SRC_CHUNK_FRAMES]; tc.in_channels() as usize];
        // A few chunks cover the queue and the delay line; the bound only
        // guards against a resampler that stops producing.
        for _ in 0..16 {
            if self.stage_kept >= end {
                break;
            }
            let Some(Ok(pcm)) = self.transcoder.as_mut().map(|t| t.process(&zeros)) else {
                break;
            };
            self.keep_stage_output(pcm, end - self.stage_kept);
        }
        self.drain_encoder(output);
    }

    /// Latch the latency the libraries declare for this pipeline: the
    /// decoder's implementation delay (source rate), the resampler's and
    /// the encoder's priming (output rate). One rounding.
    fn latch_latency(&mut self, source_rate: u32) {
        let out_rate = self.resolved_sample_rate as u128;
        let in_rate = source_rate as u128;
        let encoder = self.encoder_delay_samples();
        let resampler = self.transcoder.as_ref().map_or(0, |t| t.output_delay() as u64);
        let decoder = self.decoder_delay_samples();
        let den = out_rate * in_rate;
        let declared_90k = if den == 0 {
            0
        } else {
            let num = ((encoder + resampler) as u128 * in_rate + decoder as u128 * out_rate) * 90_000;
            ((num + den / 2) / den) as u64
        };
        self.latency = Latency { declared_90k, applied_90k: declared_90k };
        tracing::info!(
            encoder_delay_samples = encoder,
            resampler_delay_samples = resampler,
            decoder_delay_samples = decoder,
            latency_ms = declared_90k as f64 / 90.0,
            "ts_audio_replace: re-encoded audio stamped earlier by the codec pipeline's declared latency"
        );
    }

    /// Encoder priming in samples at the output rate: fdk-aac's `nDelay`
    /// (AAC-LC 2048 = 1600 + 448 metadata round-up; HE-AAC includes the
    /// decoder's SBR delay), libavcodec's `initial_padding` (MP2 481,
    /// AC-3 256).
    fn encoder_delay_samples(&self) -> u64 {
        #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
        if let Some(enc) = self.aac_encoder.as_ref() {
            return enc.codec_delay_samples() as u64;
        }
        #[cfg(feature = "media-codecs")]
        if let Some(enc) = self.av_encoder.as_ref() {
            return enc.initial_padding() as u64;
        }
        0
    }

    /// The source decoder's added delay in samples at the source rate. The
    /// libavcodec decoders add none; fdk-aac, opened without its
    /// concealment delay and limiter, adds none on a stream without SBR,
    /// and whatever it still reports there is compensated (and logged). An
    /// SBR stream's QMF delay belongs to the codec — a reference decoder has
    /// it too, so the source's timestamps already assume it — and is left.
    fn decoder_delay_samples(&self) -> u64 {
        #[cfg(feature = "fdk-aac")]
        if let Some(info) = self.aac_decoder.as_ref().and_then(|d| d.stream_info()) {
            use aac_codec::AacProfile;
            let no_sbr = matches!(
                info.profile,
                Some(AacProfile::AacLc | AacProfile::AacLd | AacProfile::AacEld)
            ) && info.frame_size <= 1024;
            if !no_sbr {
                return 0;
            }
            let delay = info.output_delay as u64;
            if delay > 0 {
                tracing::warn!(
                    output_delay = delay,
                    "ts_audio_replace: fdk-aac reports output delay on a stream without SBR; \
                     compensating it"
                );
            }
            return delay;
        }
        0
    }

    /// Insert `n` samples of silence at the decoded format: the held tail
    /// fades out ahead of it and the audio after it fades in.
    fn insert_silence(&mut self, n: u64, output: &mut Vec<u8>) {
        if !self.codecs_ready || n == 0 {
            return;
        }
        self.stats.silence_inserted_samples.fetch_add(n, Ordering::Relaxed);
        let channels = self.tail.len();
        let mut tail = std::mem::replace(&mut self.tail, vec![Vec::new(); channels]);
        let len = tail.first().map_or(0, |c| c.len());
        for ch in tail.iter_mut() {
            for (i, s) in ch.iter_mut().enumerate() {
                *s *= (len - i) as f32 / (len + 1) as f32;
            }
        }
        self.send_pcm(tail, output);
        let n = n as usize;
        self.send_pcm(vec![vec![0.0f32; n]; channels], output);
        self.fade_in_left = self.xf_len;
    }

    /// Queue decoded content: take any pending overlap off its head (the
    /// last of it crossfaded into the held tail), fade it in after inserted
    /// silence, hold its last `xf_len` samples back and send the rest.
    fn push_content(&mut self, mut planar: Vec<Vec<f32>>, output: &mut Vec<u8>) {
        let n = planar.first().map_or(0, |c| c.len());
        // `push_decoded` rebuilt the stage (and the tail) for this format.
        debug_assert_eq!(planar.len(), self.tail.len());
        let mut start = 0;
        if self.pending_drop > 0 {
            let d = (self.pending_drop as usize).min(n);
            self.pending_drop -= d as u64;
            if self.pending_drop > 0 {
                return;
            }
            // Blend the held tail's last `l` samples with the `l` dropped
            // samples just before the audio resumes, so the join is a 2 ms
            // crossfade rather than a step. Exactly `d` samples go.
            let t = self.tail.first().map_or(0, |c| c.len());
            let l = t.min(d);
            for (tail_ch, src_ch) in self.tail.iter_mut().zip(planar.iter()) {
                for i in 0..l {
                    let w = (i + 1) as f32 / (l + 1) as f32;
                    let ti = t - l + i;
                    tail_ch[ti] = tail_ch[ti] * (1.0 - w) + src_ch[d - l + i] * w;
                }
            }
            start = d;
        }
        if self.fade_in_left > 0 && start < n {
            let k = self.fade_in_left.min(n - start);
            let total = self.xf_len.max(1);
            let done = total - self.fade_in_left;
            for ch in planar.iter_mut() {
                for i in 0..k {
                    ch[start + i] *= (done + i + 1) as f32 / (total + 1) as f32;
                }
            }
            self.fade_in_left -= k;
        }
        // tail ++ planar[start..]: send all but the last `xf_len`.
        let mut combined: Vec<Vec<f32>> = Vec::with_capacity(planar.len());
        let mut tail_out: Vec<Vec<f32>> = Vec::with_capacity(planar.len());
        for (tail_ch, src_ch) in self.tail.iter_mut().zip(planar.iter()) {
            let mut all = std::mem::take(tail_ch);
            all.extend_from_slice(&src_ch[start..]);
            let keep = self.xf_len.min(all.len());
            let held = all.split_off(all.len() - keep);
            combined.push(all);
            tail_out.push(held);
        }
        self.tail = tail_out;
        self.send_pcm(combined, output);
    }

    /// Send PCM at the decoded format through the channel / rate stage into
    /// the encoder accumulator, and encode what is ready.
    fn send_pcm(&mut self, planar: Vec<Vec<f32>>, output: &mut Vec<u8>) {
        let n = planar.first().map_or(0, |c| c.len());
        if n == 0 {
            return;
        }
        let converted: Vec<Vec<f32>> = if let Some(ref mut tc) = self.transcoder {
            match tc.process(&planar) {
                Ok(p) => {
                    self.stage_fed += n as u64;
                    p
                }
                Err(e) => {
                    tracing::warn!(
                        "TsAudioReplacer: transcode process failed ({e}); dropping frame"
                    );
                    return;
                }
            }
        } else {
            planar
        };
        self.keep_stage_output(converted, u64::MAX);
        self.drain_encoder(output);
    }

    /// Queue what the stage produced for the encoder: after the zero
    /// history a replacement stage still has to drop, at most `limit`
    /// frames. A channel the stage does not produce is silence.
    fn keep_stage_output(&mut self, pcm: Vec<Vec<f32>>, limit: u64) {
        let n = pcm.first().map_or(0, |c| c.len());
        let skip = (self.stage_skip as usize).min(n);
        let keep = ((n - skip) as u64).min(limit) as usize;
        self.stage_skip -= skip as u64;
        self.stage_kept += keep as u64;
        for (ch, acc) in self.accumulator.iter_mut().enumerate() {
            match pcm.get(ch) {
                Some(src) => acc.extend_from_slice(&src[skip..skip + keep]),
                None => acc.extend(std::iter::repeat_n(0.0f32, keep)),
            }
        }
    }

    /// Publish the audio path's edge-added skew at a PES PTS: where this
    /// PES's first sample will be presented minus its source PTS.
    fn publish_av_skew(&mut self, pts: u64) {
        let Some(reporter) = self.av_skew.as_ref() else {
            return;
        };
        if !self.codecs_ready || self.resolved_sample_rate == 0 || self.timeline.rate == 0 {
            return;
        }
        if let Some(from) = self.av_skew_from_90k {
            if pts_diff(pts, from) < 0 {
                return;
            }
            self.av_skew_from_90k = None;
        }
        // Model position of this PES's first sample (still to be pushed), as
        // the stamps assume the stage places it…
        let at = (self.next_output_pts_90k(self.resolved_sample_rate) as i128
            + self.pending_out_90k() as i128
            + self.stage_offset_90k() as i128)
            .rem_euclid(PTS_MASK as i128 + 1) as u64;
        // …which is the first sample after any overlap still to be dropped.
        let first = pts.wrapping_add(self.pending_drop * 90_000 / self.timeline.rate as u64) & PTS_MASK;
        // Presented at the model position plus the latency the stamps do
        // not take off.
        let uncancelled = self.latency.declared_90k as i64 - self.latency.applied_90k as i64;
        reporter.set_audio_delta(pts_diff(at, first) + uncancelled);
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
                    // An AC-3 source's dialogue level goes into the AC-3
                    // it is re-encoded to, and follows it (see
                    // `follow_dialnorm`); any other source keeps
                    // libavcodec's defaults, as before.
                    let dialnorm = self.source_dialnorm.map(|d| d.to_string());
                    let mut opts: Vec<(&str, &str)> = Vec::new();
                    if codec_type == video_codec::AudioCodecType::Ac3
                        && let Some(d) = dialnorm.as_deref()
                    {
                        opts.push(("dialnorm", d));
                        opts.push(("per_frame_metadata", "1"));
                    }
                    self.av_encoder = Some(
                        video_engine::AudioEncoder::open_with_options(&cfg, &opts)
                            .map_err(|_| ())?,
                    );
                    self.encoder_dialnorm =
                        if opts.is_empty() { None } else { self.source_dialnorm };
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
    fn drain_encoder(&mut self, output: &mut Vec<u8>) {
        let audio_pid = match self.audio_pid {
            // Encoded packets always ride the source PID. Any operator
            // PID rename lands on the downstream `TsPidOverridesRewriter`
            // stage; the replacer's PMT rewrite advertises the source PID
            // so the rewriter can match and rename consistently.
            Some(p) => p,
            None => return,
        };

        // AAC branch — fdk-aac encoder.
        #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
        {
            if let Some(ref mut enc) = self.aac_encoder {
                let frame_size = enc.frame_size() as usize;
                while self.accumulator.first().map_or(0, |c| c.len()) >= frame_size {
                    let frame: Vec<Vec<f32>> = self
                        .accumulator
                        .iter_mut()
                        .map(|ch| ch.drain(..frame_size).collect())
                        .collect();
                    self.encode_stats.inc_submitted();
                    match enc.encode_frame(&frame) {
                        Ok(encoded) => {
                            let sr = self.resolved_sample_rate;
                            let model = if sr == 0 {
                                self.out_pts_90k
                            } else {
                                self.out_pts_90k.wrapping_add(
                                    self.samples_since_anchor.saturating_mul(90_000)
                                        / sr as u64,
                                )
                            };
                            let pts_for_pes =
                                model.wrapping_sub(self.latency.applied_90k) & PTS_MASK;
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
                return;
            }
        }

        // MP2 / AC-3 branch — libavcodec via video-engine.
        #[cfg(feature = "media-codecs")]
        {
            if let Some(ref mut enc) = self.av_encoder {
                let frame_size = enc.frame_size();
                while self.accumulator.first().map_or(0, |c| c.len()) >= frame_size {
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
                                let model = if sr == 0 {
                                    self.out_pts_90k
                                } else {
                                    self.out_pts_90k.wrapping_add(
                                        self.samples_since_anchor.saturating_mul(90_000)
                                            / sr as u64,
                                    )
                                };
                                let pts_for_pes =
                                    model.wrapping_sub(self.latency.applied_90k) & PTS_MASK;
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
            }
        }
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
        r.timeline.anchored = true;
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
            !r.timeline.anchored,
            "PTS must re-anchor to the new input's timeline"
        );
        assert_eq!(r.resolved_sample_rate, 0);
        assert_eq!(r.resolved_channels, 0);
        assert!(r.accumulator.is_empty(), "stale PCM must be dropped");
    }

    /// PID-only change (same codec, different audio PID) must also
    /// reset the pipeline — the old ES cutter buffer and PCM
    /// accumulator belong to a different elementary stream.
    #[test]
    fn audio_pid_change_on_pmt_update_resets_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        r.codecs_ready = true;
        r.timeline.anchored = true;

        r.process(&synth_pmt_audio(0x1000, 0x0102, 0x0F), &mut out);

        assert_eq!(r.audio_pid, Some(0x0102));
        assert!(!r.codecs_ready);
        assert!(!r.timeline.anchored);
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
        r.timeline.anchored = true;
        r.resolved_sample_rate = 48_000;

        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        assert!(r.codecs_ready, "unchanged PMT must not reset codec state");
        assert!(r.timeline.anchored);
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
        r.timeline.anchored = true;

        r.process(&synth_pat(0x1001), &mut out);

        assert_eq!(r.pmt_pid, Some(0x1001));
        assert_eq!(r.audio_pid, None, "audio_pid must be cleared pending new PMT");
        assert!(!r.codecs_ready);
        assert!(!r.timeline.anchored);
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
    #[cfg(feature = "media-codecs")]
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
        // A real syncframe: the demuxer cuts whole, validated AUs now.
        let mut e = video_engine::AudioEncoder::open(&video_codec::AudioEncoderConfig {
            codec: video_codec::AudioCodecType::Ac3,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 192,
        })
        .unwrap();
        let frame = e.encode_frame(&[vec![0.0f32; 1536], vec![0.0f32; 1536]]).unwrap().remove(0).data;
        let pes = build_audio_pes(0xBD, &frame, 90_000);
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
    // ── Output PTS model ──
    //
    // The output PTS come from an anchor + samples-emitted model: monotonic
    // and exact within ±1 tick regardless of how the decoder and encoder
    // buffer (the FIFO it replaced emitted duplicate and skipped PTS on
    // every frame-size-mismatched mapping).

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
        r.samples_since_anchor = 1024 * 1000;
        assert_eq!(r.next_output_pts_90k(48_000), 1_000_000 + 1_920_000);
    }

    /// 44.1 kHz: the anchor + sample-count model rounds once, so the error
    /// stays under a tick however many frames were emitted (adding
    /// `1024 * 90000 / 44100` per frame would fall 795 ticks short after
    /// 1000 frames).
    #[test]
    fn next_output_pts_advances_without_accumulating_at_44k() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.out_pts_90k = 0;
        r.resolved_sample_rate = 44_100;
        r.samples_since_anchor = 1024 * 1000;
        let pts = r.next_output_pts_90k(44_100);
        assert_eq!(pts, 2_089_795);
        let legacy_accumulated = (1024u64 * 90_000) / 44_100 * 1000;
        assert!(pts > legacy_accumulated && pts - legacy_accumulated <= 1000);
    }

    /// Unset / unknown sample rate must short-circuit to the anchor
    /// rather than divide by zero.
    #[test]
    fn next_output_pts_handles_unresolved_sample_rate() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.out_pts_90k = 12_345;
        r.samples_since_anchor = 99_999;
        assert_eq!(r.next_output_pts_90k(0), 12_345);
    }

    /// The wire stamp is the model minus the latched latency, wrap-aware.
    #[test]
    fn the_wire_pts_is_the_model_minus_the_latency() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        r.latency = Latency { declared_90k: 3_840, applied_90k: 3_840 };
        assert_eq!(r.wire_pts(1_000_000), 1_000_000 - 3_840);
        assert_eq!(r.wire_pts(1_000), PTS_MASK + 1 + 1_000 - 3_840, "wraps below zero");
    }

    /// `reset_source_state` must zero the sample counter and forget the
    /// timeline, the cutter and the latency — otherwise an input switch
    /// would keep advancing PTS from the OLD input's accumulated samples
    /// against the NEW input's anchor.
    #[test]
    fn reset_source_state_clears_pts_arithmetic_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        // Simulate a fully running pipeline.
        r.timeline = Timeline { anchored: true, base_90k: 1_000_000, samples: 48_000 * 30, rate: 48_000, ..Timeline::default() };
        r.out_pts_90k = 1_000_000;
        r.samples_since_anchor = 48_000 * 30;
        r.cutter = Some(AuCutter::new(AuFormat::Adts));
        r.latency = Latency { declared_90k: 3_840, applied_90k: 3_840 };
        r.pending_drop = 100;

        // Codec swap forces a reset.
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x81), &mut out);

        assert!(!r.timeline.anchored, "anchor flag cleared on reset");
        assert_eq!(r.timeline.samples, 0);
        assert_eq!(r.samples_since_anchor, 0, "sample counter zeroed");
        assert!(r.cutter.is_none(), "no byte of the old input reaches the new one");
        assert_eq!(r.latency, Latency::default());
        assert_eq!(r.pending_drop, 0);
    }

    // ── Source-timeline tracker (`Timeline::check`) ──
    //
    // The wallclock catch-up this replaced compared the master clock at the
    // moment the codec thread reached a PES with the samples emitted, so
    // host load, x264 warm-up and wire backpressure inserted 32 ms of
    // silence at a time (7 times in 200 s at load 40), and a real gap was
    // counted twice. The tracker compares source PTS with decoded content
    // only.

    const AU_TICKS: u64 = 1920; // 1024 samples at 48 kHz

    fn anchored_timeline() -> Timeline {
        Timeline { anchored: true, base_90k: 1_000_000, rate: 48_000, ..Timeline::default() }
    }

    /// Apply an action to a timeline the way the replacer does; returns
    /// whether it was a correction.
    fn apply(t: &mut Timeline, a: TimelineAction, pts: u64) -> bool {
        match a {
            TimelineAction::Hold => false,
            TimelineAction::Gap(off) => {
                t.samples += (off as i128 * 48_000 / 90_000) as i64;
                true
            }
            TimelineAction::Overlap(off) => {
                t.samples -= (off.unsigned_abs() as u128 * 48_000 / 90_000) as i64;
                true
            }
            TimelineAction::Reanchor | TimelineAction::Anchor => {
                *t = Timeline { anchored: true, base_90k: pts, rate: 48_000, ..Timeline::default() };
                true
            }
        }
    }

    /// Run `n` one-AU PES whose PTS is `pts_of(k)` against content of
    /// 1024 samples per AU; returns (corrections, worst |offset| in ticks
    /// seen before a correction).
    fn run_timeline(n: u64, pts_of: impl Fn(u64) -> u64) -> (u32, u64) {
        let mut t = anchored_timeline();
        let (mut corrections, mut worst) = (0, 0);
        for k in 0..n {
            let pts = pts_of(k);
            let off = pts_diff(pts, t.end_90k()).unsigned_abs();
            worst = worst.max(off);
            let a = t.check(pts, GapFill::Silence);
            if apply(&mut t, a, pts) {
                corrections += 1;
            }
            t.samples += 1024;
        }
        (corrections, worst)
    }

    #[test]
    fn timestamp_jitter_is_never_corrected() {
        // ±3 ms alternating, 1000 PES.
        let (c, _) = run_timeline(1000, |k| {
            let j: i64 = if k % 2 == 0 { 270 } else { -270 };
            (1_000_000i64 + (k * AU_TICKS) as i64 + j) as u64
        });
        assert_eq!(c, 0);
        // ±6 ms alternating: above the deadband, but never the same sign
        // for two PES in a row.
        let (c, _) = run_timeline(1000, |k| {
            let j: i64 = if k % 2 == 0 { 540 } else { -540 };
            (1_000_000i64 + (k * AU_TICKS) as i64 + j) as u64
        });
        assert_eq!(c, 0);
    }

    /// A 20 ms gap is corrected once it has held for 150 ms, whatever the
    /// PES packing: one AU per PES (DVB) or seven (Sky).
    #[test]
    fn a_small_gap_is_filled_once_it_persists_150_ms() {
        for aus_per_pes in [1u64, 7] {
            let mut t = anchored_timeline();
            let mut k = 0u64; // AUs of content placed
            let mut fixed_at = None;
            for pes in 0..40u64 {
                let pts = 1_000_000 + k * AU_TICKS + if pes >= 5 { 1_800 } else { 0 };
                let a = t.check(pts, GapFill::Silence);
                if let TimelineAction::Gap(off) = a {
                    assert_eq!(off, 1_800);
                    fixed_at.get_or_insert(pes);
                }
                apply(&mut t, a, pts);
                t.samples += 1024 * aus_per_pes as i64;
                k += aus_per_pes;
            }
            let fixed_at = fixed_at.expect("the gap is filled");
            let waited_ms = (fixed_at - 5) * aus_per_pes * 1024 * 1000 / 48_000;
            assert!((150..300).contains(&waited_ms), "{aus_per_pes} AU/PES: {waited_ms} ms");
        }
    }

    #[test]
    fn a_gap_of_100_ms_is_filled_at_once_and_a_10_ms_overlap_dropped() {
        let mut t = anchored_timeline();
        t.samples = 1024 * 10;
        let end = t.end_90k();
        assert_eq!(t.check(end + 9_000, GapFill::Silence), TimelineAction::Gap(9_000));
        let mut t = anchored_timeline();
        t.samples = 1024 * 10;
        assert_eq!(t.check(end - 900, GapFill::Silence), TimelineAction::Hold, "one PES is not enough");
        t.samples += 1024 * 8; // 170 ms later
        assert_eq!(t.check(t.end_90k() - 900, GapFill::Silence), TimelineAction::Overlap(-900));
    }

    /// A step of more than 500 ms re-anchors in either direction: the PCR
    /// and the video follow a source that resets back, so the audio does.
    #[test]
    fn a_step_beyond_500_ms_reanchors_either_way() {
        let mut t = anchored_timeline();
        t.samples = 1024 * 10;
        let end = t.end_90k();
        assert_eq!(t.check(end + 45_001, GapFill::Silence), TimelineAction::Reanchor);
        assert_eq!(t.check(end - 45_001, GapFill::Silence), TimelineAction::Reanchor);
        assert_eq!(t.check(end - 45_000, GapFill::Silence), TimelineAction::Overlap(-45_000), "500 ms back is an overlap");
    }

    /// A drop larger than the content placed since the anchor takes the
    /// count below zero rather than to zero: the content's end stays where
    /// it is (before the anchor), and nothing is dropped twice.
    #[test]
    fn a_count_below_zero_keeps_the_content_end() {
        let mut t = anchored_timeline();
        t.samples = 1024 - 7_200;
        assert_eq!(t.end_90k(), 1_000_000 - 11_580);
        t.samples += 7_200;
        assert_eq!(t.check(1_000_000 + 1_920, GapFill::Silence), TimelineAction::Hold);
        let mut t = Timeline { anchored: true, base_90k: 100, rate: 48_000, samples: -960, ..Timeline::default() };
        assert_eq!(t.end_90k(), PTS_MASK + 1 + 100 - 1_800, "wraps below zero");
        assert_eq!(t.check(PTS_MASK - 1_699, GapFill::Silence), TimelineAction::Hold);
    }

    /// On a file source (`GapFill::Relabel`) a forward step beyond the
    /// deadband is stepped over at the PES that shows it; a live source
    /// waits for it to persist, and an overlap waits either way.
    #[test]
    fn a_relabel_steps_at_the_first_pes_that_shows_it() {
        let mut t = anchored_timeline();
        t.samples = 1024 * 10;
        let end = t.end_90k();
        assert_eq!(t.check(end + 6_840, GapFill::Relabel), TimelineAction::Gap(6_840));
        assert_eq!(t.check(end + 6_840, GapFill::Silence), TimelineAction::Hold, "one PES is not enough");
        let mut t = anchored_timeline();
        t.samples = 1024 * 10;
        assert_eq!(t.check(end + 450, GapFill::Relabel), TimelineAction::Hold, "within the deadband");
        assert_eq!(t.check(end - 900, GapFill::Relabel), TimelineAction::Hold, "an overlap persists first");
    }

    /// A 4.5 ms forward step every 10 s "loop" (the Sky Witness file-splice
    /// residue) is under the deadband alone; two accumulate and are filled,
    /// so the error never reaches 10 ms and ends every loop within 5 ms.
    #[test]
    fn a_sub_deadband_loop_step_is_filled_once_it_accumulates() {
        let loop_aus = 469; // ≈ 10 s
        let (c, worst) = run_timeline(loop_aus * 10, |k| 1_000_000 + k * AU_TICKS + (k / loop_aus) * 405);
        assert!(worst < 900, "worst {worst} ticks");
        assert!((4..=5).contains(&c), "{c} corrections for 45 ms of steps");
    }

    /// A source whose audio clock runs 50 ppm off its STC drifts 5 ms every
    /// 100 s: held within ~5 ms by one correction per 100 s, counted.
    #[test]
    fn a_50_ppm_audio_clock_is_held_within_5_ms() {
        for ppm in [50.0f64, -50.0] {
            let n = 30 * 60 * 48_000 / 1024; // 30 min
            let (c, worst) = run_timeline(n, |k| {
                1_000_000 + (k as f64 * AU_TICKS as f64 * (1.0 + ppm * 1e-6)).round() as u64
            });
            assert!(worst <= 470, "{ppm} ppm: worst {worst} ticks");
            assert!((15..=20).contains(&c), "{ppm} ppm: {c} corrections");
        }
    }

    // ── End to end: source TS → replacer → decoded output ──

    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    mod e2e {
        use super::*;

        pub const RATE: u32 = 48_000;
        /// Source presentation origin: content sample 0 is presented at 10 s.
        pub const P0: u64 = 900_000;

        /// A 10 ms Hann-windowed 1 kHz tone burst at `rate`.
        pub fn burst(rate: u32) -> Vec<f32> {
            let n = (rate / 100) as usize;
            (0..n)
                .map(|k| {
                    let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / (n - 1) as f32).cos();
                    0.5 * w * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / rate as f32).sin()
                })
                .collect()
        }

        /// `len` samples of silence with the burst at sample `at`.
        pub fn content(at: usize, len: usize) -> Vec<f32> {
            content_at(RATE, at, len)
        }

        /// `content` at another rate.
        pub fn content_at(rate: u32, at: usize, len: usize) -> Vec<f32> {
            let mut pcm = vec![0.0f32; len];
            let b = burst(rate);
            pcm[at..at + b.len()].copy_from_slice(&b);
            pcm
        }

        #[derive(Clone, Copy, Debug)]
        pub enum Src {
            Aac,
            HeAac,
            Mp2,
            Ac3,
        }

        impl Src {
            pub fn stream_type(self) -> u8 {
                match self {
                    Src::Aac | Src::HeAac => 0x0F,
                    Src::Mp2 => 0x03,
                    Src::Ac3 => 0x81,
                }
            }
        }

        /// Encode mono `pcm` (as stereo) in `src`. Each AU carries the PTS a
        /// source muxer stamps — its first decoded sample's presentation
        /// time — so content sample `j` is presented at `P0 + j / 48 kHz`.
        pub fn encode_source(src: Src, pcm: &[f32]) -> Vec<(Vec<u8>, u64)> {
            encode_source_as(src, pcm, 2, RATE)
        }

        /// `encode_source` with `channels` channels (the content in the
        /// first two, L and R; the rest silent) at `rate`.
        pub fn encode_source_as(src: Src, pcm: &[f32], channels: u8, rate: u32) -> Vec<(Vec<u8>, u64)> {
            let pts_at = |samples: i64| (P0 as i64 + samples * 90_000 / rate as i64) as u64;
            let frame = |c: &[f32]| -> Vec<Vec<f32>> {
                (0..channels).map(|ch| if ch < 2 { c.to_vec() } else { vec![0.0; c.len()] }).collect()
            };
            let mut out = Vec::new();
            match src {
                Src::Aac | Src::HeAac => {
                    let he = matches!(src, Src::HeAac);
                    let mut e = aac_audio::AacEncoder::open(&aac_codec::EncoderConfig {
                        profile: if he { aac_codec::AacProfile::HeAacV1 } else { aac_codec::AacProfile::AacLc },
                        sample_rate: rate,
                        channels,
                        bitrate: if he { 64_000 } else { 128_000 },
                        afterburner: true,
                        sbr_signaling: aac_codec::SbrSignaling::default(),
                        transport: aac_codec::TransportType::Adts,
                    })
                    .unwrap();
                    let fs = e.frame_size() as usize;
                    let delay = e.codec_delay_samples() as i64;
                    let mut n = 0i64;
                    for chunk in pcm.chunks(fs) {
                        let mut c = chunk.to_vec();
                        c.resize(fs, 0.0);
                        let ed = e.encode_frame(&frame(&c)).unwrap();
                        if ed.bytes.is_empty() {
                            continue;
                        }
                        out.push((ed.bytes, pts_at(n * fs as i64 - delay)));
                        n += 1;
                    }
                }
                Src::Mp2 | Src::Ac3 => {
                    let (codec, kbps) = match src {
                        Src::Mp2 => (video_codec::AudioCodecType::Mp2, 192),
                        _ => (video_codec::AudioCodecType::Ac3, 384),
                    };
                    let mut e = video_engine::AudioEncoder::open(&video_codec::AudioEncoderConfig {
                        codec,
                        sample_rate: rate,
                        channels,
                        bitrate_kbps: kbps,
                    })
                    .unwrap();
                    let fs = e.frame_size();
                    for chunk in pcm.chunks(fs) {
                        let mut c = chunk.to_vec();
                        c.resize(fs, 0.0);
                        for f in e.encode_frame(&frame(&c)).unwrap() {
                            out.push((f.data.to_vec(), pts_at(f.pts)));
                        }
                    }
                    for f in e.flush().unwrap() {
                        out.push((f.data.to_vec(), pts_at(f.pts)));
                    }
                }
            }
            out
        }

        /// PAT + PMT + the given PES (ES bytes, PTS) on PID 0x0101.
        pub fn mux(stream_type: u8, pes: &[(Vec<u8>, Option<u64>)]) -> Vec<u8> {
            let mut ts = synth_pat(0x1000).to_vec();
            ts.extend_from_slice(&synth_pmt_audio(0x1000, 0x0101, stream_type));
            let mut cc = 0u8;
            for (es, pts) in pes {
                let bytes = match pts {
                    Some(p) => build_audio_pes(0xC0, es, *p),
                    None => {
                        let mut b = vec![0, 0, 1, 0xC0];
                        b.extend_from_slice(&((3 + es.len()) as u16).to_be_bytes());
                        b.extend_from_slice(&[0x80, 0x00, 0x00]);
                        b.extend_from_slice(es);
                        b
                    }
                };
                for p in packetize_ts(0x0101, &bytes, &mut cc) {
                    ts.extend_from_slice(&p);
                }
            }
            ts
        }

        /// `per_pes` AUs to a PES, stamped with its first AU's PTS.
        pub fn pack(aus: &[(Vec<u8>, u64)], per_pes: usize) -> Vec<(Vec<u8>, Option<u64>)> {
            aus.chunks(per_pes)
                .map(|c| (c.iter().flat_map(|(b, _)| b.clone()).collect(), Some(c[0].1)))
                .collect()
        }

        /// Feed `ts` one packet per call.
        pub fn run(r: &mut TsAudioReplacer, ts: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            for p in ts.chunks(TS_PACKET_SIZE) {
                r.process(p, &mut out);
            }
            out
        }

        /// The audio PES on PID 0x0101 in `ts`: (PTS, ES).
        pub fn audio_pes(ts: &[u8]) -> Vec<(u64, Vec<u8>)> {
            let mut pes: Vec<Vec<u8>> = Vec::new();
            for p in ts.chunks(TS_PACKET_SIZE) {
                if ts_pid(p) != 0x0101 || !ts_has_payload(p) {
                    continue;
                }
                let payload = &p[ts_payload_offset(p)..];
                if ts_pusi(p) {
                    pes.push(payload.to_vec());
                } else if let Some(last) = pes.last_mut() {
                    last.extend_from_slice(payload);
                }
            }
            pes.iter()
                .map(|b| {
                    let es_start = 9 + b[8] as usize;
                    (crate::engine::audio_au::parse_pes_timestamp(&b[9..14]), b[es_start..].to_vec())
                })
                .collect()
        }

        /// Decode an output audio ES with a reference decoder: each PES's
        /// channel-0 PCM with its PTS, and the rate.
        pub fn decode(target: &str, pes: &[(u64, Vec<u8>)]) -> (Vec<(u64, Vec<f32>)>, u32) {
            let mut out = Vec::new();
            match target {
                "aac_lc" | "he_aac_v1" => {
                    let mut d = aac_audio::AacDecoder::open_adts().unwrap();
                    for (pts, es) in pes {
                        out.push((*pts, d.decode_frame(es).unwrap().planar[0].clone()));
                    }
                    (out, d.sample_rate().unwrap())
                }
                _ => {
                    let codec = match target {
                        "mp2" => video_codec::AudioDecoderCodec::Mp2,
                        _ => video_codec::AudioDecoderCodec::Ac3,
                    };
                    let mut d = video_engine::AudioDecoder::open(codec).unwrap();
                    let mut rate = 0;
                    for (pts, es) in pes {
                        d.send_packet(es, 0).unwrap();
                        let mut pcm = Vec::new();
                        while let Ok(f) = d.receive_frame() {
                            rate = f.sample_rate;
                            pcm.extend_from_slice(&f.planar[0]);
                        }
                        out.push((*pts, pcm));
                    }
                    (out, rate)
                }
            }
        }

        /// Presentation time (90 kHz) of the burst in the output — each
        /// decoded sample timed from its own PES's PTS — searched within
        /// 100 ms of `expected`.
        pub fn burst_time(target: &str, out: &[u8], expected: f64) -> f64 {
            let pes = audio_pes(out);
            assert!(!pes.is_empty(), "no audio out");
            let (frames, rate) = decode(target, &pes);
            let mut pcm = Vec::new();
            let mut times = Vec::new();
            for (pts, chunk) in &frames {
                for i in 0..chunk.len() {
                    times.push(*pts as f64 + i as f64 * 90_000.0 / rate as f64);
                }
                pcm.extend_from_slice(chunk);
            }
            let b = burst(rate);
            let centre = times.iter().position(|&t| t >= expected).unwrap_or(times.len()) as i64;
            let radius = rate as i64 / 10;
            let lo = (centre - radius).max(0) as usize;
            let hi = ((centre + radius) as usize).min(pcm.len() - b.len());
            let m = (lo..hi)
                .max_by(|&x, &y| {
                    let s = |at: usize| -> f32 { b.iter().zip(&pcm[at..]).map(|(p, q)| p * q).sum() };
                    s(x).total_cmp(&s(y))
                })
                .unwrap();
            times[m]
        }

        /// Source time of content sample `at`.
        pub fn src_time(at: usize) -> f64 {
            P0 as f64 + at as f64 * 90_000.0 / RATE as f64
        }

        pub fn replacer(target: &str, sample_rate: Option<u32>, transcode: Option<TranscodeJson>) -> TsAudioReplacer {
            let mut cfg = enc(target);
            cfg.sample_rate = sample_rate;
            TsAudioReplacer::new(&cfg, transcode).unwrap()
        }

        /// |burst error| in output samples.
        pub fn err_samples(target: &str, out: &[u8], expected: f64, out_rate: u32) -> f64 {
            (burst_time(target, out, expected) - expected) * out_rate as f64 / 90_000.0
        }
    }

    /// A program (video PID 0x100 carrying the PCR every 30 ms, MP2 audio on
    /// 0x101 in 120 ms PES, each arriving `lead(source ms)` ms ahead of its
    /// PTS, all at once) whose audio has a hole `hole` (source ms after P0,
    /// length ms), as a media-player MPTS loop leaves it: the PCR runs on
    /// through it. The content is a burst at sample `burst_at`. `spread`:
    /// a constant-rate mux — each PES's packets spread over the 120 ms
    /// before it is due (less where the one before it still runs: a PES's
    /// packets never interleave with another's), and a video packet on
    /// 0x100 every millisecond, as a broadcast program runs — instead of
    /// each PES all at once.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    fn clock_fill_source(
        lead: impl Fn(u64) -> u64,
        hole: Option<(u64, u64)>,
        burst_at: usize,
        secs: u64,
        spread: bool,
    ) -> Vec<u8> {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        use e2e::*;
        let aus = encode_source(Src::Mp2, &content(burst_at, 48_000 * (secs as usize + 1)));
        // (µs, order, packets)
        let mut ev: Vec<(u64, u8, Vec<[u8; TS_PACKET_SIZE]>)> = Vec::new();
        let start = P0 / 90 - 500;
        ev.push((start * 1_000, 0, vec![pat_packet(&[(1, 0x1000)], 0, 0)]));
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x03, 0x101, &[])]);
        ev.push((start * 1_000, 1, packetize_sections(0x1000, &[&pmt], 0)));
        let end = P0 / 90 + secs * 1_000 - 500;
        for t in (start..end).step_by(30) {
            ev.push((t * 1_000, 2, vec![crate::engine::ts_parse::pcr_only_packet(0x100, 0, t * 27_000, false)]));
        }
        if spread {
            for (k, t) in (start..end).enumerate() {
                let mut v = [0xAAu8; TS_PACKET_SIZE];
                v[..4].copy_from_slice(&[TS_SYNC_BYTE, 0x01, 0x00, 0x10 | (k & 0x0F) as u8]);
                ev.push((t * 1_000 + 500, 4, vec![v]));
            }
        }
        let mut cc = 0u8;
        let mut sent_until = 0u64;
        for (es, pts) in e2e::pack(&aus, 5) {
            let pts = pts.unwrap();
            let src_ms = (pts / 90) as i64 - (P0 / 90) as i64;
            if hole.is_some_and(|(from, len)| src_ms >= from as i64 && src_ms < (from + len) as i64) {
                continue;
            }
            let at = pts / 90 - lead(src_ms.max(0) as u64);
            // The audio runs 400 ms past the last PCR: no gap at the end.
            if at >= end + 400 {
                break;
            }
            let pkts = packetize_ts(0x0101, &build_audio_pes(0xC0, &es, pts), &mut cc);
            if spread {
                let n = pkts.len() as u64;
                let from = ((at - 120) * 1_000).max(sent_until);
                let span = (at * 1_000).saturating_sub(from).max(n);
                for (j, p) in pkts.into_iter().enumerate() {
                    ev.push((from + (j as u64 + 1) * span / n, 3, vec![p]));
                }
                sent_until = from + span;
            } else {
                ev.push((at * 1_000, 3, pkts));
            }
        }
        ev.sort_by_key(|(t, o, _)| (*t, *o));
        ev.into_iter().flat_map(|(_, _, p)| p.into_iter().flatten()).collect()
    }

    /// Run `ts` through an MP2 replacer and the chain's PCR stage behind it.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    fn clock_fill_chain(
        ts: &[u8],
    ) -> (Arc<TsAudioReplacerStats>, crate::stats::models::TranscodePcrStats, Vec<u8>) {
        let mut r = e2e::replacer("mp2", None, None);
        let stats = r.stats_handle();
        let out = e2e::run(&mut r, ts);
        let mut remux = crate::engine::ts_pcr_remux::TsPcrRemux::new();
        remux.set_replaced_pids(Some(0x0101), None);
        let mut wire = Vec::new();
        for p in out.chunks(TS_PACKET_SIZE) {
            remux.process(p, &mut wire);
        }
        (stats, remux.stats_handle().snapshot(), wire)
    }

    /// Every audio PES on the wire: (PTS, PTS minus the PCR before it, ms)
    /// — its T-STD lead.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    fn wire_leads(wire: &[u8]) -> Vec<(u64, f64)> {
        let mut pcr = None;
        let mut out = Vec::new();
        for p in wire.chunks(TS_PACKET_SIZE) {
            if ts_pid(p) == 0x100
                && let Some(v) = extract_pcr(p)
            {
                pcr = Some(v / 300);
            }
            if ts_pid(p) == 0x0101
                && ts_pusi(p)
                && let (Some(c), Some(pts)) = (pcr, crate::engine::ts_parse::extract_pes_pts(p))
            {
                out.push((pts, pts_diff(pts, c) as f64 / 90.0));
            }
        }
        out
    }

    /// The silent frames placed for a hole (source ms `from`, `len` long)
    /// leave with a T-STD lead no worse than the source's own frames' worst
    /// less `slack` ms: on a constant-rate mux the trigger's tolerance and
    /// an output frame (25 ms).
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    fn assert_fill_on_time(wire: &[u8], from: u64, len: u64, slack: f64) {
        let p0 = e2e::P0 as i64;
        let in_hole = |pts: u64| {
            let ms = (pts as i64 - p0) / 90;
            ms >= from as i64 && ms < (from + len) as i64
        };
        let leads = wire_leads(wire);
        // The source's own frames: 200 ms clear of the hole either side (a
        // frame at its edge holds silence as well as audio).
        let near = |pts: u64| {
            let ms = (pts as i64 - p0) / 90;
            ms >= from as i64 - 200 && ms < (from + len) as i64 + 200
        };
        let worst = |fill: bool| {
            leads
                .iter()
                .filter(|(pts, _)| if fill { in_hole(*pts) } else { !near(*pts) })
                .filter(|(pts, _)| (*pts as i64 - p0) / 90 > 3_000)
                .map(|(_, l)| *l)
                .fold(f64::MAX, f64::min)
        };
        let (fill, own) = (worst(true), worst(false));
        assert!(fill < f64::MAX && own < f64::MAX);
        assert!(fill >= own - slack, "silent frames lead {fill:.1} ms at worst, the source's own {own:.1} ms");
    }

    /// A 360 ms hole in a program's audio while its PCR runs on (a
    /// media-player MPTS loop): the silence is placed as the clock passes
    /// the hole, every frame of it on time, so the PCR stage behind never
    /// raises its delay. Found only when the audio returned, the whole
    /// hole's silence was encoded in one burst stamped for when the hole
    /// began — ~260 ms behind the output PCR here, a `D` raise with a DI at
    /// every loop (Spain +163 ms, 770_H +290 ms on the rig).
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_hole_in_the_audio_is_filled_on_the_program_clock() {
        // A burst in the first PES after the hole (the hole takes the three
        // whole PES that start in it): presented at its source time.
        let at = 48_000 * 4 + 560 * 48;
        let (stats, pcr, out) = clock_fill_chain(&clock_fill_source(|_| 100, Some((4_000, 360)), at, 7, true));
        assert_fill_on_time(&out, 4_070, 360, 25.0);
        let silence = stats.silence_inserted_samples.load(Ordering::Relaxed);
        assert!((360 * 48 - 1_152..=360 * 48 + 1_152).contains(&silence), "{silence} samples");
        assert_eq!(stats.dropped_samples.load(Ordering::Relaxed), 0, "nothing placed ahead of the audio");
        let err = e2e::err_samples("mp2", &out, e2e::src_time(at), 48_000);
        assert!(err.abs() <= 48.0, "the audio after the hole is {err} samples off its source time");
        assert_eq!(pcr.offset_raises, 0, "{pcr:?}");
        assert_eq!(pcr.late_frames, 0, "{pcr:?}");
        assert_eq!(pcr.offset_ms, 80.0, "{pcr:?}");
        // A mux that sends each PES whole, nothing between them but PCRs:
        // the clock is read at the PCRs alone, still on time.
        let (stats, pcr, _) = clock_fill_chain(&clock_fill_source(|_| 100, Some((4_000, 360)), at, 7, false));
        let silence = stats.silence_inserted_samples.load(Ordering::Relaxed);
        assert!((360 * 48 - 1_152..=360 * 48 + 1_152).contains(&silence), "{silence} samples");
        assert_eq!((pcr.offset_raises, pcr.late_frames), (0, 0), "{pcr:?}");
    }

    /// The clock between two PCRs is the last PCR plus the packets since, at
    /// the rate the last interval showed, and never past one interval; a
    /// PCR that steps back, jumps over a second or carries DI starts over.
    #[test]
    fn the_clock_fill_reads_the_program_clock_between_pcrs() {
        let mut f = ClockFill::default();
        assert_eq!(f.now(), None);
        assert!(f.note_pcr(900_000, false));
        f.pkts = 30;
        assert!(f.note_pcr(902_700, false), "30 ms, 30 packets");
        assert_eq!(f.now(), Some(902_700));
        f.pkts = 15;
        assert_eq!(f.now(), Some(902_700 + 1_350), "15 packets: 15 ms on");
        f.pkts = 300;
        assert_eq!(f.now(), Some(902_700 + 2_700), "never past one interval");
        f.pkts = 2;
        assert!(f.note_pcr(905_400, false), "too few packets to rate");
        f.pkts = 15;
        assert_eq!(f.now(), Some(905_400 + 1_350), "the last good rate");
        f.record(10, Some(5), Some(7));
        assert!(!f.note_pcr(905_400 + 100_000, false), "a jump over a second");
        assert_eq!(f.window_at(10), None, "starts over");
        assert!(!f.note_pcr(905_400 + 100_000 + 2_700, true), "a DI");
        assert!(!f.note_pcr(905_400, false), "a step back");
    }

    /// What the source did more than ten seconds ago does not set the fill:
    /// a flow start whose audio arrived with 10 ms of lead, then 100 ms from
    /// 2 s on, fills a hole at 14 s from the 100 ms it keeps now. Judged since
    /// the anchor, the start's floor put the fill 90 ms later — on the rig
    /// (Spain, a startup floor of −9 ms under a steady ~50 ms) that was
    /// still a `D` raise.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_hole_is_filled_from_what_the_source_does_now() {
        let lead = |ms: u64| if ms < 2_000 { 10 } else { 100 };
        let (stats, pcr, wire) = clock_fill_chain(&clock_fill_source(lead, Some((14_000, 360)), 48_000, 16, true));
        assert_fill_on_time(&wire, 14_070, 360, 25.0);
        let silence = stats.silence_inserted_samples.load(Ordering::Relaxed);
        assert!((360 * 48 - 1_152..=360 * 48 + 1_152).contains(&silence), "{silence} samples");
        assert_eq!(pcr.offset_raises, 0, "{pcr:?}");
        assert!(pcr.offset_ms <= 80.0, "{pcr:?}");
    }

    /// `ts` with every PCR and PES PTS moved `by` ticks (90 kHz) on, mod
    /// 2^33.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    fn shift_clock(ts: &[u8], by: u64) -> Vec<u8> {
        let mut out = ts.to_vec();
        for p in out.chunks_mut(TS_PACKET_SIZE) {
            if let Some(v) = extract_pcr(p) {
                crate::engine::ts_parse::write_pcr(p, v + by * 300);
            }
            if ts_pusi(p)
                && let Some(pts) = crate::engine::ts_parse::extract_pes_pts(p)
            {
                let o = ts_payload_offset(p) + 9;
                let v = (pts + by) & PTS_MASK;
                p[o] = 0x21 | ((v >> 29) as u8 & 0x0E);
                p[o + 1] = (v >> 22) as u8;
                p[o + 2] = ((v >> 14) as u8 & 0xFE) | 1;
                p[o + 3] = (v >> 7) as u8;
                p[o + 4] = ((v << 1) as u8 & 0xFE) | 1;
            }
        }
        out
    }

    /// The program clock wrapping 2^33 -> 0 (every 26.5 h) is nothing to
    /// the fill: a steady source of 120 ms PES arriving whole, run across
    /// the wrap, gets no silence. Keyed on the 33-bit clock, the window
    /// forgot every second before the wrap, took the first reading after
    /// it as the source's floor and filled the next PES's wait — its
    /// audio then dropped as overlap, once a day on every 24/7 flow.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_program_clock_wrap_is_never_filled() {
        // The wrap about 5 s into a 12 s source (the fill is warm from 2 s),
        // at points across a PES interval: the forgetting shows only when
        // the source stands still well past the wrap.
        for (spread, extra) in [false, true].into_iter().flat_map(|s| [0u64, 2_700, 5_400, 8_100].map(|e| (s, e))) {
            let by = (1u64 << 33) - e2e::P0 - 5 * 90_000 + extra;
            let src = clock_fill_source(|_| 100, None, 48_000, 12, spread);
            let ts = shift_clock(&src, by);
            let mut r = e2e::replacer("mp2", None, None);
            let stats = r.stats_handle();
            let out = e2e::run(&mut r, &ts);
            assert!(
                e2e::audio_pes(&out).iter().any(|(pts, _)| *pts < 90_000),
                "the output ran past the wrap"
            );
            assert_eq!(stats.timeline_corrections.load(Ordering::Relaxed), 0, "spread {spread} +{extra}");
            assert_eq!(stats.silence_inserted_samples.load(Ordering::Relaxed), 0, "spread {spread} +{extra}");
            assert_eq!(stats.dropped_samples.load(Ordering::Relaxed), 0, "spread {spread} +{extra}");
        }
    }

    /// The window's seconds run on through a wrap of the 33-bit clock.
    #[test]
    fn the_clock_fill_window_runs_on_through_a_clock_wrap() {
        let mut f = ClockFill::default();
        let near = (1u64 << 33) - 3 * 90_000;
        assert!(f.note_pcr(near, false));
        let before = f.unwrapped(near) / 90_000;
        f.record(before, Some(9_000), Some(10_800));
        let mut c = near;
        for _ in 0..200 {
            c = (c + 2_700) & PTS_MASK;
            f.pkts = 30;
            assert!(f.note_pcr(c, false), "a 30 ms step across the wrap");
        }
        assert_eq!(c, 270_000, "wrapped");
        let now = f.unwrapped(c) / 90_000;
        assert_eq!(now - before, 6, "six seconds on");
        assert_eq!(f.window_at(now), Some((9_000, 10_800)), "the pre-wrap second is still in the window");
    }

    /// The source's own packing — 120 ms PES arriving whole — is no gap:
    /// in seven seconds the clock places nothing.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_source_s_own_packing_is_never_filled() {
        for (lead, spread) in [(40, false), (100, false), (400, false), (40, true), (100, true)] {
            let (stats, pcr, _) = clock_fill_chain(&clock_fill_source(|_| lead, None, 48_000, 7, spread));
            assert_eq!(stats.silence_inserted_samples.load(Ordering::Relaxed), 0, "lead {lead} {spread}");
            assert_eq!(stats.timeline_corrections.load(Ordering::Relaxed), 0, "lead {lead} {spread}");
            assert_eq!(pcr.offset_raises, 0, "lead {lead} {spread}: {pcr:?}");
        }
        // A lead that shrinks, 100 ms to 40 ms over the run: each new low
        // comes with the source's own stretch, never a longer one — no gap.
        let (stats, _, _) = clock_fill_chain(&clock_fill_source(|ms| 100 - ms.min(6_000) / 100, None, 48_000, 7, false));
        assert_eq!(stats.silence_inserted_samples.load(Ordering::Relaxed), 0);
        assert_eq!(stats.dropped_samples.load(Ordering::Relaxed), 0);
    }

    /// **B5.** An AC-3 source's `dialnorm` goes into the AC-3 it is
    /// re-encoded to, and follows a change (programme to ads). libavcodec's
    /// default -31 was written over every source: a -24 dB programme's
    /// transcode played 7 dB louder than its source on any receiver that
    /// normalises to dialnorm. An MP2 / AAC source keeps the default.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_ac3_sources_dialnorm_is_carried_into_the_ac3_re_encode() {
        use e2e::*;
        let pcm = content(48_000 + 333, 48_000 * 3);
        let mut e = video_engine::AudioEncoder::open_with_options(
            &video_codec::AudioEncoderConfig {
                codec: video_codec::AudioCodecType::Ac3,
                sample_rate: 48_000,
                channels: 2,
                bitrate_kbps: 384,
            },
            &[("dialnorm", "-24"), ("per_frame_metadata", "1")],
        )
        .unwrap();
        let mut aus = Vec::new();
        for (k, chunk) in pcm.chunks(1536).enumerate() {
            if k == 60 {
                e.set_option("dialnorm", "-27").unwrap();
            }
            let mut c = chunk.to_vec();
            c.resize(1536, 0.0);
            for f in e.encode_frame(&[c.clone(), c]).unwrap() {
                aus.push((f.data.to_vec(), (P0 as i64 + f.pts * 90_000 / 48_000) as u64));
            }
        }
        let ts = mux(0x81, &pack(&aus, 2));
        let mut r = replacer("ac3", None, None);
        let out = run(&mut r, &ts);
        let d: Vec<i8> = audio_pes(&out)
            .iter()
            .map(|(_, es)| crate::engine::audio_decode::ac3_dialnorm(es).expect("an AC-3 frame"))
            .collect();
        assert!(d.len() > 80, "{} frames out", d.len());
        assert!(d[..50].iter().all(|&v| v == -24), "the source's level: {:?}", &d[..50]);
        assert!(d[d.len() - 10..].iter().all(|&v| v == -27), "and its change: {:?}", &d[d.len() - 10..]);
        // An MP2 source has none to carry: the default.
        let mp2 = encode_source(Src::Mp2, &pcm);
        let out = run(&mut replacer("ac3", None, None), &mux(0x03, &pack(&mp2, 2)));
        let d: Vec<i8> = audio_pes(&out)
            .iter()
            .filter_map(|(_, es)| crate::engine::audio_decode::ac3_dialnorm(es))
            .collect();
        assert!(!d.is_empty() && d.iter().all(|&v| v == -31));
    }

    /// **B5.** Every libavcodec audio decode in the edge opens AC-3 / E-AC-3
    /// without dynamic-range compression and with the dither seeded from
    /// each frame, so a frame decodes to the same PCM whatever came before
    /// it — the reference a gate-6 comparison of an AC-3 → AC-3 transcode
    /// needs (two decodes of one 192 kbps source differed at 33.5 dB).
    #[cfg(feature = "media-codecs")]
    #[test]
    fn ac3_decodes_without_drc_and_with_consistent_dither() {
        use video_codec::AudioDecoderCodec;
        for codec in [AudioDecoderCodec::Ac3, AudioDecoderCodec::Eac3] {
            let o = crate::engine::audio_decode::ff_decoder_options(codec);
            assert!(o.contains(&("drc_scale", "0")) && o.contains(&("cons_noisegen", "1")), "{codec}");
        }
        assert!(crate::engine::audio_decode::ff_decoder_options(AudioDecoderCodec::Mp2).is_empty());
        // Low-bitrate noise leaves mantissas at zero bits, which the decoder
        // fills with dither.
        let mut e = video_engine::AudioEncoder::open(&video_codec::AudioEncoderConfig {
            codec: video_codec::AudioCodecType::Ac3,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 64,
        })
        .unwrap();
        let mut seed = 7u32;
        let mut frames = Vec::new();
        while frames.len() < 6 {
            let c: Vec<f32> = (0..1536)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.05 - 0.025
                })
                .collect();
            frames.extend(e.encode_frame(&[c.clone(), c]).unwrap().into_iter().map(|f| f.data.to_vec()));
        }
        let last = |from: usize| -> Vec<f32> {
            let mut d = crate::engine::audio_decode::open_ff_decoder(AudioDecoderCodec::Ac3).unwrap();
            let mut last = Vec::new();
            for f in &frames[from..] {
                d.send_packet(f, 0).unwrap();
                while let Ok(o) = d.receive_frame() {
                    last = o.planar[0].clone();
                }
            }
            last
        };
        assert_eq!(last(0), last(3), "the last frame decodes the same from either start");
    }

    /// **AT-1.** A burst at a known source PTS is presented at that PTS
    /// after the re-encode, for every source × target: the decoder adds no
    /// delay and the stamps take the encoder's priming (and the
    /// resampler's) back off. Before, AAC → AAC-LC / MP2 / AC-3 presented it
    /// 79.0 / 46.4 / 41.6 ms late (fdk-aac's 1744-sample decoder delay plus
    /// 2048 / 481 / 256 samples of encoder priming).
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_burst_is_presented_at_its_source_pts() {
        use e2e::*;
        let at = 48_000 + 333;
        let pcm = content(at, 48_000 * 3);
        let cases: &[(Src, &str, Option<u32>, Option<TranscodeJson>, u32)] = &[
            (Src::Aac, "aac_lc", None, None, 48_000),
            (Src::Aac, "mp2", None, None, 48_000),
            (Src::Aac, "ac3", None, None, 48_000),
            (Src::Aac, "he_aac_v1", None, None, 48_000),
            (Src::HeAac, "aac_lc", None, None, 48_000),
            (Src::Mp2, "aac_lc", None, None, 48_000),
            (Src::Ac3, "mp2", None, None, 48_000),
            // audio_encode.sample_rate alone (no transcode block) resamples.
            (Src::Aac, "aac_lc", Some(44_100), None, 44_100),
            (
                Src::Mp2,
                "mp2",
                None,
                Some(TranscodeJson { sample_rate: Some(44_100), ..Default::default() }),
                44_100,
            ),
        ];
        for (src, target, sr, tj, out_rate) in cases {
            let aus = encode_source(*src, &pcm);
            let ts = mux(src.stream_type(), &pack(&aus, 3));
            let mut r = replacer(target, *sr, tj.clone());
            let out = run(&mut r, &ts);
            let e = err_samples(target, &out, src_time(at), *out_rate);
            assert!(e.abs() <= 3.0, "{src:?} -> {target} @ {out_rate}: {e:.1} samples off");
            assert_eq!(r.resolved_sample_rate, *out_rate);
        }
    }

    /// **AT-4.** `av_skew` reports what the stamps do not cancel: 0 with
    /// the compensation, the encoder priming (2048 samples, 42.7 ms for
    /// fdk-aac LC) without it — where the burst then lands, too.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn av_skew_reports_the_uncancelled_latency() {
        use e2e::*;
        let at = 48_000 * 2;
        let pcm = content(at, 48_000 * 3);
        let ts = mux(0x0F, &pack(&encode_source(Src::Aac, &pcm), 3));
        for compensate in [true, false] {
            let mut r = replacer("aac_lc", None, None);
            let rep = Arc::new(crate::stats::av_skew::AvSkewReporter::new());
            r.set_av_skew_reporter(rep.clone());
            let split = 20 * TS_PACKET_SIZE;
            let mut out = run(&mut r, &ts[..split]);
            assert!(r.codecs_ready);
            assert_eq!(r.latency.declared_90k, 3_840, "fdk-aac LC nDelay 2048 at 48 kHz");
            if !compensate {
                r.latency.applied_90k = 0;
            }
            out.extend(run(&mut r, &ts[split..]));
            let skew = rep.snapshot();
            let e = err_samples("aac_lc", &out, src_time(at), 48_000);
            if compensate {
                assert_eq!(skew.skew_ms, 0);
                assert!(e.abs() <= 3.0, "{e}");
            } else {
                assert_eq!(skew.skew_ms, 42);
                assert!((e - 2048.0).abs() <= 3.0, "{e}");
            }
        }
    }

    /// **AT-2.** The Sky Sports Arena pattern: the last AU of a 7-AU PES
    /// straddles into the next PES (130-byte tail, data_alignment 0). Every
    /// AU decodes, no silence is inserted, the output PTS are continuous
    /// and the audio after it is on time. Before, that AU and the whole
    /// next PES (149 ms) were lost and 128 ms of silence stood in, moving
    /// the audio 21.3 ms early for good.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_au_straddling_two_pes_is_decoded() {
        use e2e::*;
        let at = 48_000 * 2 + 100;
        let pcm = content(at, 48_000 * 3);
        let aus = encode_source(Src::Aac, &pcm);
        let mut pes = pack(&aus, 7);
        // PES 5 gives its last AU's final 130 bytes to PES 6.
        let cut = pes[5].0.len() - 130;
        let tail = pes[5].0.split_off(cut);
        pes[6].0.splice(0..0, tail);
        let ts = mux(0x0F, &pes);
        let mut r = replacer("aac_lc", None, None);
        let out = run(&mut r, &ts);
        let s = r.stats_handle();
        assert_eq!(s.silence_inserted_samples.load(Ordering::Relaxed), 0);
        assert_eq!(s.timeline_corrections.load(Ordering::Relaxed), 0);
        assert_eq!(r.decode_stats.decode_errors.load(Ordering::Relaxed), 0);
        assert_eq!(r.decode_stats.input_frames.load(Ordering::Relaxed), aus.len() as u64);
        let out_pes = audio_pes(&out);
        for w in out_pes.windows(2) {
            assert_eq!(w[1].0 - w[0].0, 1920, "output PTS continuous");
        }
        let e = err_samples("aac_lc", &out, src_time(at), 48_000);
        assert!(e.abs() <= 3.0, "{e}");
    }

    /// **AT-5.** An AU is decoded and re-encoded as soon as it is complete,
    /// not when the next PES begins: audio leaves before the PES that
    /// carried it has finished arriving.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn audio_is_emitted_before_its_pes_ends() {
        use e2e::*;
        let pcm = content(1000, 48_000);
        let aus = encode_source(Src::Aac, &pcm);
        for target in ["aac_lc", "ac3"] {
            let ts = mux(0x0F, &pack(&aus[..14], 7));
            let mut r = replacer(target, None, None);
            let mut out = Vec::new();
            let pkts: Vec<&[u8]> = ts.chunks(TS_PACKET_SIZE).collect();
            // PAT, PMT, then the first 7-AU PES.
            let second_pusi = 2 + pkts[2..].iter().skip(1).position(|p| ts_pusi(p)).unwrap() + 1;
            let mut first_out = None;
            for (i, p) in pkts.iter().enumerate() {
                r.process(p, &mut out);
                if first_out.is_none() && !audio_pes(&out).is_empty() {
                    first_out = Some(i);
                }
            }
            let first_out = first_out.expect("audio out");
            assert!(first_out + 3 < second_pusi, "{target}: first audio at packet {first_out}, PES ends at {second_pusi}");
        }
    }

    /// **AT-3.** A gap in the source audio timeline is filled with exactly
    /// the missing duration of silence: at once for 150 ms (seven AUs),
    /// after 150 ms of persistence for 42.7 ms (two AUs) — and the audio
    /// after it is on time.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn lost_aus_become_exactly_their_duration_of_silence() {
        use e2e::*;
        let at = 48_000 * 2 + 50;
        let pcm = content(at, 48_000 * 3);
        let aus = encode_source(Src::Aac, &pcm);
        for (lost, immediate) in [(2usize, false), (7, true)] {
            let mut kept = aus.clone();
            kept.drain(40..40 + lost);
            let ts = mux(0x0F, &pack(&kept, 1));
            let mut r = replacer("aac_lc", None, None);
            let out = run(&mut r, &ts);
            let s = r.stats_handle();
            assert_eq!(s.silence_inserted_samples.load(Ordering::Relaxed), 1024 * lost as u64);
            assert_eq!(s.timeline_corrections.load(Ordering::Relaxed), 1);
            let e = err_samples("aac_lc", &out, src_time(at), 48_000);
            assert!(e.abs() <= 3.0, "{lost} lost: {e}");
            let _ = immediate;
        }
    }

    /// **AT-3.** A PES that starts 10 ms before the content already placed
    /// ends (an overlap) has 480 samples dropped once it persists, and the
    /// audio after it is on time.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_overlap_is_dropped() {
        use e2e::*;
        let at = 48_000 * 2 + 50;
        let pcm = content(at, 48_000 * 3);
        let mut aus = encode_source(Src::Aac, &pcm);
        for au in aus.iter_mut().skip(40) {
            au.1 -= 900;
        }
        let ts = mux(0x0F, &pack(&aus, 1));
        let mut r = replacer("aac_lc", None, None);
        let out = run(&mut r, &ts);
        let s = r.stats_handle();
        assert_eq!(s.dropped_samples.load(Ordering::Relaxed), 480);
        let e = err_samples("aac_lc", &out, src_time(at) - 900.0, 48_000);
        assert!(e.abs() <= 3.0, "{e}");
    }

    /// **AT-3.** An overlap larger than the content placed since the anchor:
    /// one AU after the anchor, the next PES is stamped 150 ms before where
    /// it ends. Exactly 150 ms is dropped and the audio after it is on
    /// time. Counting the content with a floor at zero forgot 128.7 ms of
    /// that drop, read the next PES as a new overlap, dropped 6176 more
    /// samples and put the audio 128.7 ms early for good.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_overlap_larger_than_the_content_since_the_anchor_is_dropped_once() {
        use e2e::*;
        let at = 48_000 * 2 + 50;
        let pcm = content(at, 48_000 * 3);
        let mut aus = encode_source(Src::Aac, &pcm);
        for au in aus.iter_mut().skip(1) {
            au.1 -= 13_500;
        }
        let ts = mux(0x0F, &pack(&aus, 1));
        let mut r = replacer("aac_lc", None, None);
        let out = run(&mut r, &ts);
        let s = r.stats_handle();
        assert_eq!(s.dropped_samples.load(Ordering::Relaxed), 7_200);
        assert_eq!(s.timeline_corrections.load(Ordering::Relaxed), 1);
        let e = err_samples("aac_lc", &out, src_time(at) - 13_500.0, 48_000);
        assert!(e.abs() <= 3.0, "{e}");
    }

    /// **AT-3.** `media_player`'s gap fill steps the output PTS over a
    /// splice instead of inserting silence: the decoded audio is untouched
    /// and the audio after the step is on time.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn relabel_steps_the_pts_over_a_gap() {
        use e2e::*;
        let at = 48_000 * 2 + 50;
        let pcm = content(at, 48_000 * 3);
        let mut aus = encode_source(Src::Aac, &pcm);
        for au in aus.iter_mut().skip(40) {
            au.1 += 4_500;
        }
        let ts = mux(0x0F, &pack(&aus, 1));
        let mut r = replacer("aac_lc", None, None);
        r.set_gap_fill(GapFill::Relabel);
        let out = run(&mut r, &ts);
        let s = r.stats_handle();
        assert_eq!(s.silence_inserted_samples.load(Ordering::Relaxed), 0);
        assert_eq!(s.timeline_corrections.load(Ordering::Relaxed), 1);
        let steps: Vec<u64> = audio_pes(&out).windows(2).map(|w| w[1].0 - w[0].0).filter(|&d| d != 1920).collect();
        assert_eq!(steps, vec![1920 + 4_500], "one PTS step, no silence");
        let e = err_samples("aac_lc", &out, src_time(at) + 4_500.0, 48_000);
        assert!(e.abs() <= 3.0, "{e}");
    }

    /// **AT-3.** A `media_player` splice step (76 ms, the Sky Witness loop)
    /// is stepped over at the first PES that shows it: audio a few AUs
    /// after the step is on time. Waiting for the step to persist 150 ms
    /// presented that audio 76 ms early at every loop.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn relabel_steps_at_the_first_pes_that_shows_the_step() {
        use e2e::*;
        let at = 40 * 1024 + 1_500;
        let pcm = content(at, 48_000 * 2);
        let mut aus = encode_source(Src::Aac, &pcm);
        // Encoder priming: AU k carries content from 2048 samples earlier,
        // so the content at `at` rides AU 43.
        for au in aus.iter_mut().skip(40) {
            au.1 += 6_840;
        }
        let ts = mux(0x0F, &pack(&aus, 1));
        let mut r = replacer("aac_lc", None, None);
        r.set_gap_fill(GapFill::Relabel);
        let out = run(&mut r, &ts);
        assert_eq!(r.stats_handle().timeline_corrections.load(Ordering::Relaxed), 1);
        let e = err_samples("aac_lc", &out, src_time(at) + 6_840.0, 48_000);
        assert!(e.abs() <= 3.0, "{e}");
    }

    /// **AT-3.** A step of more than 500 ms re-anchors, forward or back:
    /// the audio after it lands exactly at its PTS, with the encoder
    /// accumulator part full (MP2's 1152-sample frames against 1024-sample
    /// AAC AUs), with and without 48 → 44.1 kHz rate conversion, and with
    /// two AUs lost at the step (a single re-anchor, no extra pad).
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_step_over_500_ms_reanchors_at_the_pts() {
        use e2e::*;
        let at = 48_000 * 2 + 50;
        let pcm = content(at, 48_000 * 3);
        let aus = encode_source(Src::Aac, &pcm);
        for (target, sr, out_rate, step, lost) in [
            ("mp2", None, 48_000, 90_000i64, false),
            ("mp2", Some(44_100), 44_100, 90_000, false),
            ("aac_lc", None, 48_000, 90_000, true),
            ("mp2", None, 48_000, -90_000, false),
            ("aac_lc", Some(44_100), 44_100, -90_000, true),
        ] {
            let mut stepped = aus.clone();
            for au in stepped.iter_mut().skip(40) {
                au.1 = au.1.wrapping_add_signed(step);
            }
            if lost {
                stepped.drain(40..42);
            }
            let ts = mux(0x0F, &pack(&stepped, 1));
            let mut r = replacer(target, sr, None);
            let out = run(&mut r, &ts);
            assert_eq!(r.stats_handle().silence_inserted_samples.load(Ordering::Relaxed), 0);
            let e = err_samples(target, &out, src_time(at) + step as f64, out_rate);
            // Within 4 samples (0.09 ms): the resampler's delay line holds
            // a fraction of a sample more or less than its nominal delay
            // at the moment of the step.
            assert!(e.abs() <= 4.0, "{target} @ {out_rate} (step {step}, lost {lost}): {e}");
        }
    }

    /// **AT-3.** The only time the replacer is given is the `Instant` its
    /// caller passes with each chunk (for the pre-PMT gate and the engage
    /// watchdog). A feed in which a second of that time passes before every
    /// audio PES — the codec thread held up by host load, encoder warm-up
    /// or wire backpressure, what made the removed wallclock catch-up pad
    /// silence — gives the same bytes out as one that stalls nowhere, with
    /// no silence inserted, and so does a feed in one chunk.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_stalled_feed_gives_the_same_output() {
        use e2e::*;
        let pcm = content(20_000, 48_000 * 2);
        let ts = mux(0x0F, &pack(&encode_source(Src::Aac, &pcm), 3));
        let t0 = std::time::Instant::now();
        let mut steady = replacer("ac3", None, None);
        let mut a = Vec::new();
        for p in ts.chunks(TS_PACKET_SIZE) {
            steady.process_at(p, &mut a, t0);
        }
        let mut stalled = replacer("ac3", None, None);
        let mut b = Vec::new();
        let mut now = t0;
        for p in ts.chunks(TS_PACKET_SIZE) {
            if ts_pid(p) == 0x0101 && ts_pusi(p) {
                now += std::time::Duration::from_secs(1);
            }
            stalled.process_at(p, &mut b, now);
        }
        let mut whole_r = replacer("ac3", None, None);
        let mut whole = Vec::new();
        whole_r.process_at(&ts, &mut whole, t0);
        assert!(!audio_pes(&a).is_empty());
        assert_eq!(stalled.stats_handle().silence_inserted_samples.load(Ordering::Relaxed), 0);
        assert_eq!(stalled.stats_handle().timeline_corrections.load(Ordering::Relaxed), 0);
        assert_eq!(a, b, "a stall between PES changes nothing");
        assert_eq!(a, whole, "nor does the chunking");
    }

    /// An in-band channel-count change with `audio_encode.channels: 2`: an
    /// AC-3 source switching 5.1 → 2.0 (programme to ads) and 2.0 → 5.1,
    /// and 5.1 → 2.0 under a `transcode` block whose BS.775 preset only
    /// fits 5.1 (the default conversion stands in for it). The stage
    /// converting to the output's stereo is rebuilt for each format, so the
    /// audio after the switch is on time with nothing inserted or dropped.
    /// The downmix built for 5.1 used to refuse the stereo frames, and the
    /// output went silent until the source returned to 5.1.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_in_band_channel_change_keeps_the_audio() {
        use e2e::*;
        let at = 48_000 * 2 + 50;
        let pcm = content(at, 48_000 * 3);
        let preset = TranscodeJson {
            channels: Some(2),
            channel_map_preset: Some("5_1_to_stereo_bs775".into()),
            ..Default::default()
        };
        for (first, then, block) in [(6u8, 2u8, None), (2, 6, None), (6, 2, Some(preset))] {
            let a = encode_source_as(Src::Ac3, &pcm, first, RATE);
            let b = encode_source_as(Src::Ac3, &pcm, then, RATE);
            // AC-3 frame 40 starts 1.28 s in; the burst is 0.72 s later.
            let spliced: Vec<_> = a[..40].iter().chain(&b[40..]).cloned().collect();
            let ts = mux(0x81, &pack(&spliced, 1));
            let mut cfg = enc("aac_lc");
            cfg.channels = Some(2);
            let mut r = TsAudioReplacer::new(&cfg, block).unwrap();
            let out = run(&mut r, &ts);
            let s = r.stats_handle();
            assert_eq!(s.silence_inserted_samples.load(Ordering::Relaxed), 0, "{first} -> {then}");
            assert_eq!(s.dropped_samples.load(Ordering::Relaxed), 0, "{first} -> {then}");
            assert_eq!(r.stage_in, (48_000, then));
            let e = err_samples("aac_lc", &out, src_time(at), 48_000);
            assert!(e.abs() <= 3.0, "{first} -> {then}: {e}");
        }
    }

    /// After an in-band channel-count change with the output following the
    /// source's first format, a later overlap is still dropped (and the
    /// crossfade tail of the old format goes out): the audio after it is on
    /// time. Frames of the new count used to bypass the drop, so the audio
    /// stayed late by it while `av_skew` counted the drop as pending.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_overlap_after_an_in_band_channel_change_is_dropped() {
        use e2e::*;
        let at = 48_000 * 2 + 50;
        let pcm = content(at, 48_000 * 3);
        let a = encode_source_as(Src::Ac3, &pcm, 2, RATE);
        let b = encode_source_as(Src::Ac3, &pcm, 6, RATE);
        let mut spliced: Vec<_> = a[..20].iter().chain(&b[20..]).cloned().collect();
        for au in spliced.iter_mut().skip(40) {
            au.1 -= 900;
        }
        let ts = mux(0x81, &pack(&spliced, 1));
        let mut r = replacer("aac_lc", None, None);
        let out = run(&mut r, &ts);
        assert_eq!(r.resolved_channels, 2, "the output keeps the first format");
        assert_eq!(r.stats_handle().dropped_samples.load(Ordering::Relaxed), 480);
        let e = err_samples("aac_lc", &out, src_time(at) - 900.0, 48_000);
        assert!(e.abs() <= 3.0, "{e}");
    }

    /// A splice 1 s in from a 48 kHz to a 44.1 kHz AAC source (AUs from
    /// each on either side of it), the 44.1 kHz AUs shifted by `shift`
    /// ticks and, from 1.5 s on, stepped by `step`.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    fn rate_splice(shift: i64, step: i64) -> Vec<u8> {
        use e2e::*;
        let a = encode_source_as(Src::Aac, &content_at(48_000, 96_000, 48_000 * 3), 2, 48_000);
        let b = encode_source_as(Src::Aac, &content_at(44_100, 88_200, 44_100 * 3), 2, 44_100);
        let split = P0 + 90_000;
        let spliced: Vec<(Vec<u8>, u64)> = a
            .iter()
            .filter(|au| au.1 < split)
            .cloned()
            .chain(b.iter().filter(|au| au.1 >= split).map(|(es, pts)| {
                let stepped = if *pts >= P0 + 135_000 { step } else { 0 };
                (es.clone(), pts.wrapping_add_signed(shift + stepped))
            }))
            .collect();
        mux(0x0F, &pack(&spliced, 1))
    }

    /// An in-band sample-rate change (a splice from a 48 kHz to a 44.1 kHz
    /// AAC source), with and without a 1 s step after it: with the output
    /// at 48 kHz a resampler is inserted and its zero history dropped; with
    /// `sample_rate: 44100` the resampler the output needed so far is run
    /// out exactly and none follows. Either way the audio after the change
    /// (and after the re-anchor) is on time and `av_skew` reads 0. The
    /// encoder used to be fed 44.1 kHz PCM as if it were 48 kHz.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_in_band_rate_change_keeps_the_audio_on_time() {
        use e2e::*;
        for step in [0i64, 90_000] {
            let ts = rate_splice(0, step);
            for (sr, out_rate) in [(None, 48_000u32), (Some(44_100), 44_100)] {
                let mut r = replacer("aac_lc", sr, None);
                let rep = Arc::new(crate::stats::av_skew::AvSkewReporter::new());
                r.set_av_skew_reporter(rep.clone());
                let out = run(&mut r, &ts);
                assert_eq!(r.stage_in.0, 44_100);
                assert_eq!(r.resolved_sample_rate, out_rate);
                let e = err_samples("aac_lc", &out, src_time(96_000) + step as f64, out_rate);
                assert!(e.abs() <= 4.0, "output {out_rate}, step {step}: {e}");
                assert_eq!(rep.snapshot().skew_ms, 0, "output {out_rate}, step {step}");
            }
        }
    }

    /// A drop found at the first PES of the new rate (the 44.1 kHz source
    /// stamped 150 ms early, over 100 ms: dropped at once) was counted at
    /// the old rate: it is taken at the new one, and the audio after it is
    /// on time.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn a_drop_owed_across_an_in_band_rate_change_is_counted_at_the_new_rate() {
        use e2e::*;
        let ts = rate_splice(-13_500, 0);
        let mut r = replacer("aac_lc", None, None);
        let out = run(&mut r, &ts);
        assert_eq!(r.stats_handle().timeline_corrections.load(Ordering::Relaxed), 1);
        let e = err_samples("aac_lc", &out, src_time(96_000) - 13_500.0, 48_000);
        assert!(e.abs() <= 4.0, "{e}");
    }

    /// The stats snapshot says what the replacer resolved: the output's
    /// `audio_encode_stats` carry the format the encoder was opened at (the
    /// configuration's 0 = "follows the source" was rendered "0 Hz 0ch"),
    /// the `audio_decode_stats` the decoded source format (it carried the
    /// output's). Input side too, through the handles `InputTranscoder`
    /// attaches.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn the_stats_carry_the_decoded_and_the_resolved_format() {
        use e2e::*;
        let ts = mux(0x0F, &pack(&encode_source(Src::Aac, &content(1_000, 48_000)), 3));
        let mut cfg = enc("aac_lc");
        cfg.channels = Some(1);
        // Output side, registered as transcode_chain::build_for_output does.
        let acc = Arc::new(crate::stats::collector::OutputStatsAccumulator::new(
            "o".into(),
            "o".into(),
            "udp".into(),
        ));
        let r = TsAudioReplacer::new(&cfg, None).unwrap();
        acc.set_decode_stats(r.decode_stats_handle(), "", 0, 0);
        acc.set_encode_stats(r.encode_stats_handle(), "aac_lc", 0, 0, r.bitrate_kbps());
        let mut r = r.with_output_stats(acc.clone());
        run(&mut r, &ts);
        let snap = acc.snapshot();
        let de = snap.audio_decode_stats.expect("decode stats");
        let en = snap.audio_encode_stats.expect("encode stats");
        assert_eq!((de.input_codec.as_str(), de.output_sample_rate_hz, de.output_channels), ("AAC", 48_000, 2));
        assert_eq!((en.target_sample_rate_hz, en.target_channels), (48_000, 1));
        // Input side.
        let mut r = TsAudioReplacer::new(&cfg, None).unwrap();
        let dh = Arc::new(crate::stats::collector::AudioDecodeStatsHandle::new(r.decode_stats_handle(), "", 0, 0));
        let eh = Arc::new(crate::stats::collector::AudioEncodeStatsHandle::new(
            r.encode_stats_handle(),
            "aac_lc",
            0,
            0,
            r.bitrate_kbps(),
        ));
        r.with_input_decode_handle(dh.clone());
        r.with_input_encode_handle(eh.clone());
        run(&mut r, &ts);
        assert_eq!(
            (dh.output_sample_rate_hz.load(Ordering::Relaxed), dh.output_channels.load(Ordering::Relaxed)),
            (48_000, 2)
        );
        assert_eq!(
            (eh.target_sample_rate_hz.load(Ordering::Relaxed), eh.target_channels.load(Ordering::Relaxed)),
            (48_000, 1)
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
