// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Trailing PCR stage of a TS transcode chain — the remux model.
//!
//! A transcode chain (`TsAudioReplacer` → `TsVideoReplacer`, on an output in
//! `transcode_chain` or on an input in `input_transcode`) re-encodes one or
//! two elementary streams and forwards everything else. Its output PCR used
//! to be regenerated from the re-encoded video PTS, floored on a decaying
//! audio lag: a clock that ran ~15 500 ppm fast against the PTS it described
//! (40.6 ms PCR steps per 40 ms frame on a PAFF source), snapped back with
//! zero and backward steps, and could only appear once per frame — none at
//! all while the decoder waited for its first recovery point.
//!
//! The chain now keeps the **input's own PCR timeline**:
//!
//! - The replacers only preserve PCR *positions*. Every input PCR on the
//!   source PCR_PID — a video payload packet included — leaves the replacer
//!   as an adaptation-field-only packet at the same stream position, value
//!   and discontinuity_indicator unchanged (on the video PID when video is
//!   re-encoded, on the audio PID when that is where the PCR rides). No
//!   re-encoded PES carries a PCR.
//! - This stage, after both replacers, is the single owner of the delay.
//!   It rewrites every PCR on the output program's PCR_PID to
//!   `input PCR − D` and checks the decode timestamp of every re-encoded PES
//!   against the input PCR at its position. Every ES therefore keeps its
//!   source T-STD lead, shifted by `D` minus its own pipeline delay, and the
//!   output PCR advances exactly as the input's did: no rate error, no zero
//!   or backward steps, and a PCR as often as the input carried one.
//!
//! `D` is *measured*, not configured. It starts at 80 ms — the pre-roll the
//! old PTS-derived PCR kept — so a pipeline that needs no more never steps
//! the clock. The first re-encoded PES of each PID in an epoch latches
//! `D ≥ max(0, lateness) + 80 ms`, where lateness is how far the PES arrived
//! behind its own decode time (`input PCR − DTS`); the stage's very first
//! latch may also *lower* `D`, but only as far as the residency cap below
//! demands (an audio-only chain over a long-lead passthrough video). A latch
//! that would move `D` by less than 10 ms leaves it alone — every change is
//! a PCR step. After that a PES that is
//! still late is the exception path: `D` is raised to its lateness + 80 ms,
//! the next PCR carries DI = 1, `late_frames` counts it and a rate-limited
//! Warning (`transcode_pcr_late`) says so. After the first latch `D` only
//! grows — a lowered `D` is another PCR step — but for one exception below.
//! The margin is cut (never below lateness + 40 ms) so the largest lead of
//! the program's video observed in the epoch stays within the 1 s T-STD
//! residency (ISO/IEC 13818-1 §2.4.2.6); when even that cannot be met the
//! stage keeps the PES on time and says so
//! (`transcode_pcr_residency_exceeded`). The cap is re-checked every time a
//! larger video lead is seen: before anything re-encoded has been measured
//! `D` is lowered to it; after that it is lowered **once per epoch**, to the
//! cap less 20 ms of headroom but never below the largest lateness measured
//! in the epoch + 40 ms (one forward PCR step, DI), and the Warning covers
//! the rest.
//!
//! Lateness is the pipeline's, not the source's: a stretch in which the
//! input carried no PCR at all — a paused or variable-frame-rate source
//! whose muxer stamps a PCR per frame (the RTMP / RTSP / WebRTC ingest
//! muxer) — is a *gap*, and the part of it beyond the input's usual PCR
//! step is taken off the lateness of a PES decoded before it. Frames that
//! were in the pipeline when the source paused still reach the wire late,
//! but they do not raise `D` for every frame after them.
//!
//! **Holds.** A source can also keep a frame waiting with its clock running:
//! a silent input video PID (the tail of a media-player file, whose last
//! PES and whatever picture the decoder held back wait for the next loop's
//! first video). The video replacer measures that silence on the input and
//! reports each frame that sat through it ([`TsPcrRemux::note_source_holds`]);
//! its hold is taken off its lateness like a gap, and if it is still behind
//! the output PCR it is dropped as stale — never a `D` raise. The replacer
//! drops such a frame itself before the encoder when it can (it reads this
//! stage's `D` through [`PcrRemuxStats`]); the drop here is the backstop.
//!
//! **Nothing re-encoded.** While neither replacer re-encodes (its codec
//! cannot be decoded, a replacer fell back to passthrough) the stage leaves
//! every byte alone — no `D` at all.
//!
//! **Epochs.** An input PCR that steps backward, or carries DI on a step
//! the input's cadence does not predict, starts an epoch: DI goes on the
//! next output PCR and every PID latches again. A DI on a predicted PCR
//! (within twice the usual step, or 100 ms) is no new time base — the
//! flow's discontinuity watch stamps its DI one PCR after the jump — and
//! passes without one. A forward step without DI is the input's own clock,
//! however long — a PCR-per-frame source at 5 fps steps 200 ms every frame,
//! at 0.5 fps two seconds — and passes as the step it is. On an epoch of
//! more than 1 s the re-encoded PES still in flight from the previous epoch
//! are recognised by being closer to the old timeline than to the new one
//! and are dropped (their continuity counters renumbered), so they neither
//! reach the wire behind the DI nor drive `D`. That holds only where the
//! PID's own PES moved with the PCR: the stage watches each re-encoded
//! PID's *input* PES ahead of the replacers ([`TsPcrRemux::observe_input`]),
//! and when its next PES after the step carries straight on from the one
//! before, the PCR stepped alone and nothing is stale. Such a step is an
//! upstream transcode's own `D` raise or lowering — an ingress transcode
//! ahead of this output's, whose muxer-mode rewriter passes the step
//! through (`TsPtsRewriter::note_upstream_pcr_steps`) — and judged by the
//! PCR alone it made every re-encoded PES "closer to the old timeline" for
//! as long as the step, ~1.5 s of re-encoded media dropped behind a deep
//! ingress encoder's first latch.
//!
//! **No input PCR.** When a re-encoded video PES on the PCR_PID arrives and
//! no input PCR has ever been seen, or the video replacer counted a second
//! of the input's own video decode time without one
//! ([`TsPcrRemux::set_input_pcr_starved`] — never the re-encoded DTS, which
//! leave the codec in bursts), the stage synthesises one from the video:
//! `DTS − extra − D` in an AF-only packet before every video PES and
//! interpolated ones in between (at most 35 ms apart on the observed packet
//! rate), with the Info `transcode_pcr_synthesized`. The synthetic clock is
//! the video's own, so nothing latches against it and it never moves `D`:
//! the audio, muxed behind the video, moves only `extra`, holding the
//! synthetic clock behind the video far enough to keep the audio ahead of
//! it. The first input PCR ends synthesis (DI) and drops `extra`.
//!
//! Deterministic: every value is a function of the byte stream, so two
//! outputs fed the same bytes emit the same PCR unless one of them had to
//! raise `D`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::ts_parse::{
    extract_pcr, extract_pes_dts, extract_pes_pts, mpeg2_crc32, parse_pat_programs, pcr_diff_27mhz,
    pcr_only_packet, set_discontinuity_indicator, strip_to_af_only, ts_cc,
    ts_discontinuity_indicator, ts_has_payload, ts_payload_offset, ts_pid, ts_pusi, write_pcr,
    CcRenumber, SectionAssembler, PAT_PID, PCR_MODULUS_27MHZ, TS_PACKET_SIZE, TS_SYNC_BYTE,
};
use super::ts_pmt_edit::{is_pmt_for, parse_pmt};

/// Margin a latch (and a guard raise) leaves between a re-encoded PES and
/// the output PCR: 80 ms, the pre-roll FFmpeg's muxer and the old
/// PTS-derived PCR used.
const MARGIN_27MHZ: i64 = 80 * 27_000;
/// The least margin the residency cap may leave: one 25 fps frame.
const MIN_MARGIN_27MHZ: i64 = 40 * 27_000;
/// A latch that would move `D` by less than this leaves it alone: every
/// change is a PCR discontinuity, and a PES a few ms inside the 80 ms margin
/// is still tens of ms early (measured on Sky: the first audio PES latched
/// 80.24 ms against the initial 80 — a DI for a quarter of a millisecond).
const LATCH_TOLERANCE_27MHZ: i64 = 10 * 27_000;
/// Steps of this stage's own kept for [`TsPcrRemux::take_pcr_steps`]
/// between two reads (a chunk rarely carries more than one).
const PCR_STEPS_KEPT: usize = 16;
/// ISO/IEC 13818-1 §2.4.2.6: data leaves the elementary-stream buffer within
/// one second.
const MAX_RESIDENCY_27MHZ: i64 = 27_000_000;
/// The one post-latch lowering of `D` goes this far below the residency
/// cap: room for the video's lead to grow a little more before the
/// Warning, and a step always worth its DI (never a few ms over the cap
/// left standing because the step would have been under
/// [`LATCH_TOLERANCE_27MHZ`] — VH1 sat at 1 002–1 008 ms that way).
const RESIDENCY_HEADROOM_27MHZ: i64 = 20 * 27_000;
/// A forward input PCR step is a gap (see module doc) when it exceeds twice
/// the input's usual step and this — MPEG-TS asks for a PCR at least every
/// 100 ms.
const GAP_MIN_27MHZ: i64 = 100 * 27_000;
/// Continuous input PCR steps the usual step is the median of.
const CADENCE_STEPS: usize = 8;
/// Gaps remembered, and for how long (in input PCR time): a PES decoded
/// longer ago than the lateness bound never reaches `D` anyway.
const MAX_GAPS: usize = 32;
const GAP_MEMORY_27MHZ: i64 = 2 * MAX_LATENESS_27MHZ;
/// An epoch jump beyond this makes frames in flight "stale" (see module doc).
const STALE_EPOCH_27MHZ: i64 = 27_000_000;
/// Input PCR steps past [`STALE_EPOCH_27MHZ`] remembered by the input watch.
const INPUT_EPOCHS_KEPT: usize = 8;
/// Video leads beyond this are off any timeline; the residency cap ignores
/// them.
const SANE_27MHZ: i64 = 10 * 27_000_000;
/// A re-encoded PES later than this is not a deep pipeline (x264's longest
/// lookahead is ~2.5 s at 25 fps) but a frame stamped on some other
/// timeline; it never moves `D` — raising the delay by seconds would cost
/// every later frame that latency.
const MAX_LATENESS_27MHZ: i64 = 5 * 27_000_000;
/// Spacing target of synthesised in-between PCRs (TR 101 290: ≤ 40 ms).
const SYNTH_SPACING_27MHZ: i64 = 35 * 27_000;
/// Source holds remembered until their PES passes (see
/// [`TsPcrRemux::note_source_holds`]).
const HOLDS_MAX: usize = 64;
/// Rate limit of the `transcode_pcr_late` Warning.
const LATE_WARN_EVERY: Duration = Duration::from_secs(10);

/// Lock-free counters, surfaced as `transcode_pcr` on output and input stats.
#[derive(Debug, Default)]
pub struct PcrRemuxStats {
    /// Current delay `D` (27 MHz ticks).
    pub offset_27mhz: AtomicU64,
    /// Re-encoded PES that arrived behind the output PCR (the guard fired).
    pub late_frames: AtomicU64,
    /// Times `D` was raised after its first latch.
    pub offset_raises: AtomicU64,
    /// Re-encoded PES from a previous epoch dropped after a > 1 s jump.
    pub stale_frames_dropped: AtomicU64,
    /// PCRs synthesised from video because the input carried none.
    pub synthesized_pcrs: AtomicU64,
    /// Input PCR epochs (discontinuities) seen.
    pub epochs: AtomicU64,
}

impl PcrRemuxStats {
    pub fn snapshot(&self) -> crate::stats::models::TranscodePcrStats {
        crate::stats::models::TranscodePcrStats {
            offset_ms: self.offset_27mhz.load(Ordering::Relaxed) as f64 / 27_000.0,
            late_frames: self.late_frames.load(Ordering::Relaxed),
            offset_raises: self.offset_raises.load(Ordering::Relaxed),
            stale_frames_dropped: self.stale_frames_dropped.load(Ordering::Relaxed),
            synthesized_pcrs: self.synthesized_pcrs.load(Ordering::Relaxed),
            epochs: self.epochs.load(Ordering::Relaxed),
        }
    }
}

/// Per re-encoded PID state.
#[derive(Debug, Clone, Copy)]
struct Checked {
    pid: u16,
    /// This PID's first PES of the current epoch was measured.
    latched: bool,
    /// After a > 1 s epoch jump: PES closer to the old timeline are stale.
    stale: bool,
    /// Inside a PES being dropped as stale.
    dropping: bool,
    /// Renumbering for the payload packets dropped on this PID.
    cc: CcRenumber,
    /// CC of the last payload packet written on this PID.
    last_out_cc: Option<u8>,
}

impl Checked {
    fn new(pid: u16) -> Self {
        Self {
            pid,
            latched: false,
            stale: false,
            dropping: false,
            cc: CcRenumber::default(),
            last_out_cc: None,
        }
    }
}

/// Synthesis state (no input PCR).
#[derive(Debug, Default)]
struct Synth {
    active: bool,
    /// `(unshifted value, packet index)` of the last video-anchored PCR.
    anchor: Option<(u64, u64)>,
    /// `(value delta, packet delta)` between the last two anchors.
    rate: Option<(i64, u64)>,
    /// How far the synthetic clock is held behind the video's DTS beyond
    /// `D`, so the other re-encoded PID (audio muxed behind the video) stays
    /// ahead of it. Raised only while synthesising, back to 0 when an input
    /// PCR returns — the synthetic clock never moves `D`.
    extra: i64,
    /// Packet index at which the next in-between PCR is due.
    next_at: Option<u64>,
    /// In-between PCRs stay below this unshifted value (next anchor − 1 ms).
    limit: u64,
    /// The Info was emitted.
    announced: bool,
}

/// Each re-encoded PID's own PES timeline on the *input*, ahead of the
/// replacers ([`TsPcrRemux::observe_input`]): whether it moved with a PCR
/// step past [`STALE_EPOCH_27MHZ`] or went straight on.
#[derive(Debug, Default)]
struct InputPesWatch {
    /// Last input PCR on the PCR PID.
    last_pcr: Option<u64>,
    /// `(PID, decode time 90 kHz)` of the last input PES per checked slot.
    last_ts: [Option<(u16, u64)>; 2],
    /// Input PCR steps past [`STALE_EPOCH_27MHZ`], newest last: `(the PCR
    /// after the step, per checked slot whether its PES moved with it)` —
    /// `Some(false)` when its next PES continued the one before, `Some(true)`
    /// when it jumped too or there was nothing before it to continue,
    /// `None` until that PES comes.
    epochs: std::collections::VecDeque<(u64, [Option<bool>; 2])>,
}

/// The trailing PCR stage. See the module docs.
pub struct TsPcrRemux {
    program: Option<u16>,
    pmt_pid: Option<u16>,
    pmt_pid_shared: bool,
    pmt_asm: SectionAssembler,
    /// Output PCR_PID; `None` before the PMT or when it is 0x1FFF.
    pcr_pid: Option<u16>,
    /// The program's ES PIDs (from the same PMT): only their video leads
    /// count toward the residency cap.
    program_es: Vec<u16>,
    /// `[audio, video]` PIDs the replacers re-encode.
    checked: [Option<Checked>; 2],
    /// `D`, 27 MHz ticks, never negative.
    offset: i64,
    latched_once: bool,
    /// Last input (unshifted) PCR — the synthetic clock while synthesising.
    last_in_pcr: Option<u64>,
    last_out_pcr: Option<u64>,
    /// The delay the last output PCR carried on the input's own timeline
    /// (`None` after a synthesised one): a PCR carrying another is a step
    /// of this stage's, recorded in `pcr_steps` when they are reported.
    applied_at_last_out: Option<i64>,
    /// Whether [`Self::take_pcr_steps`] is read (an ingress chain, whose
    /// muxer-mode rewriter must tell this stage's steps from the source's).
    report_steps: bool,
    /// `(output PCR, delay added there)` of each output PCR that stepped
    /// because `D` changed, since the last [`Self::take_pcr_steps`].
    pcr_steps: Vec<(u64, i64)>,
    pending_di: bool,
    /// Last input PCR of the previous epoch, while stale PES may still come.
    old_epoch_pcr: Option<u64>,
    /// The input PCR that started that epoch — the key of its entry in
    /// [`InputPesWatch::epochs`].
    stale_epoch_at: Option<u64>,
    input_watch: InputPesWatch,
    /// Largest `DTS − input PCR` of the program's video in the epoch.
    max_video_lead: Option<i64>,
    /// Recent continuous input PCR steps (a ring) — the usual step.
    steps: [i64; CADENCE_STEPS],
    steps_len: usize,
    steps_next: usize,
    /// Input PCR gaps of the epoch: `(input PCR after the gap, the part of
    /// the step beyond the usual one)`.
    gaps: std::collections::VecDeque<(u64, i64)>,
    ever_input_pcr: bool,
    /// The video replacer counted a second of the input's own video decode
    /// time without an input PCR (`SourceClockWatch`): the input carries
    /// none. Measured on the input, not on the re-encoded DTS, which leave
    /// the codec in bursts.
    input_starved: bool,
    /// `(PTS 90 kHz, hold 27 MHz)` of re-encoded video frames that waited
    /// on a silent input video PID (`SourceClockWatch`), in emission order.
    source_holds: std::collections::VecDeque<(u64, i64)>,
    /// Largest lateness measured this epoch (after gaps and holds) — the
    /// floor of the one post-latch lowering of `D`.
    max_lateness: Option<i64>,
    /// `D` was lowered once this epoch to meet the residency cap.
    lowered_in_epoch: bool,
    synth: Synth,
    packets: u64,
    last_late_warn: Option<Instant>,
    late_since_warn: u64,
    residency_warned: bool,
    stats: Arc<PcrRemuxStats>,
    /// `(sender, entity id, input_scope)`.
    event_sink: Option<(crate::manager::events::EventSender, String, bool)>,
}

impl Default for TsPcrRemux {
    fn default() -> Self {
        Self::new()
    }
}

/// `(stream_id, DTS or PTS in 90 kHz)` of the PES starting in `pkt`.
fn pes_decode_ts(pkt: &[u8]) -> Option<(u8, u64)> {
    let off = ts_payload_offset(pkt);
    if off + 4 > TS_PACKET_SIZE {
        return None;
    }
    let sid = pkt[off + 3];
    let ts = extract_pes_dts(pkt).or_else(|| extract_pes_pts(pkt))?;
    Some((sid, ts))
}

impl TsPcrRemux {
    pub fn new() -> Self {
        Self {
            program: None,
            pmt_pid: None,
            pmt_pid_shared: false,
            pmt_asm: SectionAssembler::new(),
            pcr_pid: None,
            program_es: Vec::new(),
            checked: [None, None],
            offset: MARGIN_27MHZ,
            latched_once: false,
            last_in_pcr: None,
            last_out_pcr: None,
            applied_at_last_out: None,
            report_steps: false,
            pcr_steps: Vec::new(),
            pending_di: false,
            old_epoch_pcr: None,
            stale_epoch_at: None,
            input_watch: InputPesWatch::default(),
            max_video_lead: None,
            steps: [0; CADENCE_STEPS],
            steps_len: 0,
            steps_next: 0,
            gaps: std::collections::VecDeque::new(),
            ever_input_pcr: false,
            input_starved: false,
            source_holds: std::collections::VecDeque::new(),
            max_lateness: None,
            lowered_in_epoch: false,
            synth: Synth::default(),
            packets: 0,
            last_late_warn: None,
            late_since_warn: 0,
            residency_warned: false,
            stats: Arc::new(PcrRemuxStats::default()),
            event_sink: None,
        }
    }

    /// Shared counters for the stats snapshot.
    pub fn stats_handle(&self) -> Arc<PcrRemuxStats> {
        self.stats.clone()
    }

    /// Record every step of this stage's own — an output PCR that moved
    /// because `D` changed, not because the input's did — for
    /// [`Self::take_pcr_steps`].
    pub fn report_pcr_steps(&mut self) {
        self.report_steps = true;
    }

    /// The steps of this stage's own since the last call, as `(output PCR
    /// value, delay added there)` in 27 MHz ticks: that PCR stepped back by
    /// the delay added (forward when it is negative) while the input's
    /// clock ran on. An ingress chain hands them to the muxer-mode
    /// rewriter behind it (`TsPtsRewriter::note_upstream_pcr_steps`),
    /// which passes such a step through as a PCR step whatever its size —
    /// bridged as a source discontinuity, a step past 500 ms moved every
    /// PES timestamp on the flow by the step instead.
    pub fn take_pcr_steps(&mut self) -> Vec<(u64, i64)> {
        std::mem::take(&mut self.pcr_steps)
    }

    /// The output PCR `out_pcr` goes out on the input's timeline (`epoch`:
    /// it starts a new one): note a step of this stage's own.
    fn note_out_pcr_delay(&mut self, out_pcr: u64, epoch: bool) {
        let applied = self.applied();
        if self.report_steps
            && !epoch
            && let Some(prev) = self.applied_at_last_out
            && prev != applied
        {
            if self.pcr_steps.len() == PCR_STEPS_KEPT {
                self.pcr_steps.remove(0);
            }
            self.pcr_steps.push((out_pcr, applied - prev));
        }
        self.applied_at_last_out = Some(applied);
    }

    /// Where the Warnings / Info go: `input_scope` selects input- vs
    /// output-scoped events, like the replacers' watchdogs.
    pub fn set_event_sink(
        &mut self,
        sender: crate::manager::events::EventSender,
        id: impl Into<String>,
        input_scope: bool,
    ) {
        self.event_sink = Some((sender, id.into(), input_scope));
    }

    /// The chunk about to go through the replacers, before they re-encode
    /// it: the input's own PES of each re-encoded PID are watched across an
    /// input PCR step past [`STALE_EPOCH_27MHZ`], so a step of the PCR alone
    /// (the PID's next PES carries straight on) makes nothing stale. See
    /// the module doc's **Epochs**. Called before the replacers on every
    /// chunk.
    pub fn observe_input(&mut self, ts: &[u8]) {
        if !ts.len().is_multiple_of(TS_PACKET_SIZE) {
            return;
        }
        for pkt in ts.chunks_exact(TS_PACKET_SIZE) {
            if pkt[0] != TS_SYNC_BYTE {
                continue;
            }
            let pid = ts_pid(pkt);
            let w = &mut self.input_watch;
            if Some(pid) == self.pcr_pid
                && let Some(pcr) = extract_pcr(pkt)
            {
                if let Some(last) = w.last_pcr
                    && pcr_diff_27mhz(pcr, last).abs() > STALE_EPOCH_27MHZ
                {
                    if w.epochs.len() == INPUT_EPOCHS_KEPT {
                        w.epochs.pop_front();
                    }
                    // Nothing before it to continue: taken as moved.
                    let slot = |t: Option<(u16, u64)>| t.is_none().then_some(true);
                    w.epochs.push_back((pcr, [slot(w.last_ts[0]), slot(w.last_ts[1])]));
                }
                w.last_pcr = Some(pcr);
            }
            let Some(i) = self.checked_index(pid) else {
                continue;
            };
            if !(ts_pusi(pkt) && ts_has_payload(pkt)) {
                continue;
            }
            let Some((_, ts90)) = pes_decode_ts(pkt) else {
                continue;
            };
            let ts90 = ts90 & 0x1_FFFF_FFFF;
            let w = &mut self.input_watch;
            // A PES within a second of its PID's previous one carries its
            // timeline on.
            let prev = w.last_ts[i].filter(|(p, _)| *p == pid).map(|(_, t)| t);
            let moved_now =
                prev.is_none_or(|t| pcr_diff_27mhz(ts90 * 300, t * 300).abs() > STALE_EPOCH_27MHZ);
            for (_, moved) in w.epochs.iter_mut() {
                if moved[i].is_none() {
                    moved[i] = Some(moved_now);
                }
            }
            w.last_ts[i] = Some((pid, ts90));
        }
    }

    /// Whether checked slot `i`'s input PES moved with the PCR at the step
    /// that started the current stale epoch (see [`Self::observe_input`]):
    /// `None` until its next PES is seen.
    fn input_pes_moved(&self, i: usize) -> Option<bool> {
        let at = self.stale_epoch_at?;
        let (_, moved) = self.input_watch.epochs.iter().rev().find(|(v, _)| *v == at)?;
        moved[i]
    }

    /// The PIDs whose PES the replacers produce (`None` = that stage is
    /// absent or not re-encoding). Called before every [`Self::process`].
    pub fn set_replaced_pids(&mut self, audio: Option<u16>, video: Option<u16>) {
        let before = self.applied();
        for (slot, pid) in self.checked.iter_mut().zip([audio, video]) {
            match (slot.as_ref().map(|c| c.pid), pid) {
                (Some(a), Some(b)) if a == b => {}
                (_, Some(b)) => *slot = Some(Checked::new(b)),
                (_, None) => *slot = None,
            }
        }
        let after = self.applied();
        self.stats.offset_27mhz.store(after as u64, Ordering::Relaxed);
        if after != before && self.last_out_pcr.is_some() {
            // Re-encoding started or stopped: the PCR steps by `D`.
            self.pending_di = true;
        }
    }

    /// Whether the video replacer finds the input without a PCR (see
    /// [`Self::input_starved`]). Called before every [`Self::process`].
    pub fn set_input_pcr_starved(&mut self, starved: bool) {
        self.input_starved = starved;
    }

    /// Re-encoded video frames that waited on a silent input video PID, as
    /// the video replacer reports them (`TsVideoReplacer::take_source_holds`).
    /// Called before every [`Self::process`], with the frames that chunk
    /// carries.
    pub fn note_source_holds(&mut self, holds: Vec<(u64, i64)>) {
        for h in holds {
            if self.source_holds.len() == HOLDS_MAX {
                self.source_holds.pop_front();
            }
            self.source_holds.push_back(h);
        }
    }

    /// The hold of the re-encoded video PES stamped `pts_90k`, and forget
    /// the entries of frames before it.
    fn take_hold(&mut self, pts_90k: u64) -> i64 {
        match self.source_holds.iter().position(|(p, _)| *p == pts_90k) {
            Some(i) => {
                let (_, hold) = self.source_holds[i];
                self.source_holds.drain(..=i);
                hold
            }
            None => 0,
        }
    }

    /// Whether a replacer re-encodes anything.
    fn engaged(&self) -> bool {
        self.checked.iter().any(Option::is_some)
    }

    /// The delay applied to the PCR: `D` while something is re-encoded,
    /// none otherwise (the stream then passes untouched).
    fn applied(&self) -> i64 {
        if self.engaged() { self.offset } else { 0 }
    }

    /// The applied delay in 27 MHz ticks.
    #[cfg(test)]
    pub fn offset_27mhz(&self) -> u64 {
        self.applied() as u64
    }

    fn checked_index(&self, pid: u16) -> Option<usize> {
        self.checked.iter().position(|c| c.is_some_and(|c| c.pid == pid))
    }

    fn shift(&self, pcr: u64) -> u64 {
        let d = self.applied() as u64 % PCR_MODULUS_27MHZ;
        (pcr % PCR_MODULUS_27MHZ + PCR_MODULUS_27MHZ - d) % PCR_MODULUS_27MHZ
    }

    /// Process one chunk of 188-byte-aligned TS, appending to `out`.
    pub fn process(&mut self, ts: &[u8], out: &mut Vec<u8>) {
        if ts.is_empty() {
            return;
        }
        if !ts.len().is_multiple_of(TS_PACKET_SIZE) {
            out.extend_from_slice(ts);
            return;
        }
        for chunk in ts.chunks_exact(TS_PACKET_SIZE) {
            if chunk[0] != TS_SYNC_BYTE {
                out.extend_from_slice(chunk);
                continue;
            }
            let mut pkt = [0u8; TS_PACKET_SIZE];
            pkt.copy_from_slice(chunk);
            self.packets += 1;
            self.maybe_synth_between(out);
            let pid = ts_pid(&pkt);
            if pid == PAT_PID {
                if ts_pusi(&pkt) {
                    self.observe_pat(&pkt);
                }
            } else if Some(pid) == self.pmt_pid {
                self.observe_pmt(&pkt);
            }
            if Some(pid) == self.pcr_pid
                && let Some(pcr) = extract_pcr(&pkt)
            {
                self.on_input_pcr(&mut pkt, pcr);
            }
            let ci = self.checked_index(pid);
            if ts_pusi(&pkt) && ts_has_payload(&pkt) {
                let keep = match pes_decode_ts(&pkt) {
                    Some((sid, ts90)) => self.on_pes_start(pid, ci, sid, ts90, &pkt, out),
                    None => true,
                };
                if let Some(i) = ci
                    && let Some(c) = self.checked[i].as_mut()
                {
                    c.dropping = !keep;
                }
            }
            if let Some(i) = ci
                && !self.emit_checked(i, &mut pkt)
            {
                continue;
            }
            out.extend_from_slice(&pkt);
        }
    }

    fn observe_pat(&mut self, pkt: &[u8]) {
        let mut programs = parse_pat_programs(pkt);
        if programs.is_empty() {
            return;
        }
        programs.sort_by_key(|(n, _)| *n);
        let (program, pmt_pid) = programs[0];
        self.pmt_pid_shared = programs.iter().filter(|(_, p)| *p == pmt_pid).count() > 1;
        if self.pmt_pid != Some(pmt_pid) {
            self.pmt_asm.reset();
        }
        self.program = Some(program);
        self.pmt_pid = Some(pmt_pid);
    }

    fn observe_pmt(&mut self, pkt: &[u8]) {
        let Some(program) = self.program else {
            return;
        };
        let shared = self.pmt_pid_shared;
        let mut found = None;
        for sec in self.pmt_asm.push_packet(pkt) {
            let long_pmt = sec.len() >= 5 && sec[0] == 0x02 && sec[1] & 0x80 != 0;
            let ours = is_pmt_for(sec, program) || (!shared && long_pmt);
            if ours
                && mpeg2_crc32(sec) == 0
                && let Some(v) = parse_pmt(sec)
            {
                found = Some((v.pcr_pid, v.es.iter().map(|e| e.pid).collect::<Vec<_>>()));
            }
        }
        if let Some((p, es)) = found {
            self.pcr_pid = (p != 0x1FFF).then_some(p);
            self.program_es = es;
        }
    }

    /// Start an epoch: DI on the next output PCR, every PID latches again.
    /// `old` is the previous epoch's last PCR when frames in flight may be
    /// stale (a jump of more than 1 s).
    fn epoch_change(&mut self, old: Option<u64>) {
        self.pending_di = true;
        for c in self.checked.iter_mut().flatten() {
            c.latched = false;
            c.stale = old.is_some();
        }
        self.old_epoch_pcr = old;
        self.stale_epoch_at = None;
        self.max_video_lead = None;
        self.max_lateness = None;
        self.lowered_in_epoch = false;
        self.steps_len = 0;
        self.steps_next = 0;
        self.gaps.clear();
        self.residency_warned = false;
        self.stats.epochs.fetch_add(1, Ordering::Relaxed);
    }

    fn on_input_pcr(&mut self, pkt: &mut [u8; TS_PACKET_SIZE], pcr: u64) {
        let mut epoch = false;
        if self.synth.active {
            // Back to the input's timeline — a new clock for the receiver.
            self.synth = Synth { announced: self.synth.announced, ..Synth::default() };
            self.epoch_change(None);
            epoch = true;
            tracing::info!("ts_pcr_remux: input PCR present again — synthesis stops");
        } else if let Some(last) = self.last_in_pcr {
            let step = pcr_diff_27mhz(pcr, last);
            // A DI on a PCR the input's cadence predicts is not a new time
            // base: the watcher's DI lands one PCR after the jump it saw, a
            // media-player playlist transition keeps its timeline, and an
            // MPTS ingress used to flag ~91 % of its PCRs. An epoch there
            // re-latched every PID — raise-only, so `D` crept 80 -> 199 ms
            // on Spain. The DI byte still passes.
            if step < 0 || (ts_discontinuity_indicator(pkt) && !self.continues(step)) {
                let stale = step.abs() > STALE_EPOCH_27MHZ;
                self.epoch_change(stale.then_some(last));
                self.stale_epoch_at = stale.then_some(pcr);
                epoch = true;
            } else {
                self.note_step(pcr, step);
            }
        }
        self.last_in_pcr = Some(pcr);
        self.ever_input_pcr = true;
        let out_pcr = self.shift(pcr);
        write_pcr(pkt, out_pcr);
        if std::mem::take(&mut self.pending_di) {
            set_discontinuity_indicator(pkt);
        }
        self.last_out_pcr = Some(out_pcr);
        self.note_out_pcr_delay(out_pcr, epoch);
    }

    /// A PES starts in `pkt`. Returns whether to keep it.
    fn on_pes_start(
        &mut self,
        pid: u16,
        ci: Option<usize>,
        stream_id: u8,
        ts_90k: u64,
        pkt: &[u8; TS_PACKET_SIZE],
        out: &mut Vec<u8>,
    ) -> bool {
        let video = (0xE0..=0xEF).contains(&stream_id);
        let dts27 = (ts_90k & 0x1_FFFF_FFFF) * 300;
        let hold = if ci.is_some() && video { self.take_hold(ts_90k & 0x1_FFFF_FFFF) } else { 0 };
        if let Some(i) = ci
            && video
            && Some(pid) == self.pcr_pid
        {
            self.check_synth_entry();
            if self.synth.active {
                self.synth_at_video_pes(i, dts27, pkt, out);
                return true;
            }
        }
        if self.synth.active {
            // Measured against the synthetic clock, which is the video's
            // own: only its allowance may move for it, never `D`.
            if let Some(i) = ci
                && let Some(c) = self.checked[i]
                && self.synth.anchor.is_some()
                && let Some(r) = self.last_in_pcr
            {
                self.synth_allow(c.pid, pcr_diff_27mhz(r, dts27));
            }
            return true;
        }
        let Some(in_pcr) = self.last_in_pcr else {
            return true;
        };
        let lead = pcr_diff_27mhz(dts27, in_pcr);
        if video
            && lead.abs() < SANE_27MHZ
            && self.program_es.contains(&pid)
            && self.max_video_lead.is_none_or(|m| lead > m)
        {
            self.max_video_lead = Some(lead);
            self.check_residency(pid);
        }
        let Some(i) = ci else {
            return true;
        };
        let Some(c) = self.checked[i] else {
            return true;
        };
        if c.stale {
            // In flight from the old epoch — unless this PID's own input PES
            // carried straight on across the step: then only the PCR moved.
            if let Some(old) = self.old_epoch_pcr
                && self.input_pes_moved(i) != Some(false)
                && pcr_diff_27mhz(dts27, old).abs() < lead.abs()
            {
                self.stats.stale_frames_dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            if let Some(c) = self.checked[i].as_mut() {
                c.stale = false;
            }
        }
        // What the source added is not the pipeline's lateness: an input
        // PCR gap after this PES's decode time (a paused source), or the
        // time its frame waited on a silent input video PID (a hold — the
        // tail of a media-player file). They can be the same stretch, so
        // the larger counts.
        let late_by = -lead;
        let lateness = late_by - self.gap_excess(dts27, in_pcr).max(hold);
        if lateness >= MAX_LATENESS_27MHZ || lateness <= -SANE_27MHZ {
            return true;
        }
        self.max_lateness = Some(self.max_lateness.map_or(lateness, |m| m.max(lateness)));
        if !c.latched {
            if let Some(c) = self.checked[i].as_mut() {
                c.latched = true;
            }
            self.latch(pid, lateness);
        } else if lateness > self.offset {
            self.guard(pid, lateness);
        }
        if hold > 0 && late_by > self.offset {
            // Still late after whatever the pipeline itself needed: a frame
            // the source kept waiting (R2: the last frame of a media-player
            // file, emitted with the next loop's first video ~700 ms later).
            // Stale — dropped, rather than raising `D` for every frame
            // after it or reaching the wire behind the PCR.
            self.stats.stale_frames_dropped.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                "ts_pcr_remux: dropped a re-encoded frame on PID 0x{pid:04X} that waited {:.1} ms \
                 on a silent input video PID and arrived {:.1} ms behind the output PCR",
                hold as f64 / 27_000.0,
                (late_by - self.offset) as f64 / 27_000.0
            );
            return false;
        }
        true
    }

    /// Largest `D` the 1 s residency allows for the epoch's video, if known.
    fn residency_cap(&self) -> Option<i64> {
        self.max_video_lead.map(|l| MAX_RESIDENCY_27MHZ - l)
    }

    /// The program's largest video lead just grew: re-check the cap. Before
    /// anything re-encoded has been measured `D` is lowered to it (a PCR
    /// step, DI); after that `D` stays — lowering it would make the measured
    /// PES late — and the residency Warning says so.
    fn check_residency(&mut self, pid: u16) {
        let Some(cap) = self.residency_cap() else {
            return;
        };
        if !self.engaged() || self.offset <= cap {
            return;
        }
        if !self.latched_once {
            let d = cap.max(0);
            if self.offset - d >= LATCH_TOLERANCE_27MHZ {
                self.set_offset(d, "the video's lead grew past the residency cap");
            }
            return;
        }
        // Once per epoch `D` may still come down — to the cap less a little
        // headroom, but never below the largest lateness measured this
        // epoch plus the minimum margin, so every PES already measured
        // stays on time. One forward PCR step, DI; after it the Warning
        // says what is left.
        if !self.lowered_in_epoch
            && let Some(l) = self.max_lateness
        {
            let d = (cap - RESIDENCY_HEADROOM_27MHZ).max(l + MIN_MARGIN_27MHZ).max(0);
            if self.offset - d >= LATCH_TOLERANCE_27MHZ {
                self.lowered_in_epoch = true;
                self.set_offset(d, "lowered once to hold the video within the 1 s residency");
            }
        }
        if self.offset > cap {
            self.warn_residency(pid, None);
        }
    }

    /// A continuous forward input PCR step: note a gap when it is well
    /// beyond the usual step (the median of the last few), and remember the
    /// step.
    fn note_step(&mut self, pcr: u64, step: i64) {
        if let Some(usual) = self.usual_step()
            && step > (2 * usual).max(GAP_MIN_27MHZ)
        {
            if self.gaps.len() == MAX_GAPS {
                self.gaps.pop_front();
            }
            self.gaps.push_back((pcr, step - usual));
        }
        while self.gaps.front().is_some_and(|(g, _)| pcr_diff_27mhz(pcr, *g) > GAP_MEMORY_27MHZ) {
            self.gaps.pop_front();
        }
        self.steps[self.steps_next] = step;
        self.steps_next = (self.steps_next + 1) % CADENCE_STEPS;
        self.steps_len = (self.steps_len + 1).min(CADENCE_STEPS);
    }

    /// Median of the recent continuous input PCR steps.
    fn usual_step(&self) -> Option<i64> {
        if self.steps_len == 0 {
            return None;
        }
        let mut s = self.steps;
        let s = &mut s[..self.steps_len];
        s.sort_unstable();
        Some(s[s.len() / 2])
    }

    /// A forward input PCR step the input's own cadence explains: not a
    /// gap (see [`Self::note_step`]) — within twice the usual step, or
    /// within MPEG-TS's 100 ms while the usual step is not known.
    fn continues(&self, step: i64) -> bool {
        step <= self.usual_step().map_or(GAP_MIN_27MHZ, |u| (2 * u).max(GAP_MIN_27MHZ))
    }

    /// Input PCR time the gaps between a PES's decode time and the input
    /// PCR it is measured against added beyond the usual step.
    fn gap_excess(&self, dts27: u64, in_pcr: u64) -> i64 {
        self.gaps
            .iter()
            .filter(|(g, _)| pcr_diff_27mhz(*g, dts27) > 0 && pcr_diff_27mhz(*g, in_pcr) <= 0)
            .map(|(_, e)| *e)
            .sum()
    }

    /// `(D, capped)`: `lateness + margin`, the margin cut to respect the
    /// residency cap but never below [`MIN_MARGIN_27MHZ`]; `capped` when even
    /// that minimum breaks the cap.
    fn target_offset(&self, lateness: i64, want: i64) -> (i64, bool) {
        let floor = lateness + MIN_MARGIN_27MHZ;
        let (d, capped) = match self.residency_cap() {
            Some(cap) if want > cap => (cap.max(floor), floor > cap),
            _ => (want, false),
        };
        (d.max(0), capped)
    }

    fn set_offset(&mut self, d: i64, why: &str) {
        if d == self.offset {
            return;
        }
        tracing::info!(
            "ts_pcr_remux: PCR offset {:.1} ms -> {:.1} ms ({why})",
            self.offset as f64 / 27_000.0,
            d as f64 / 27_000.0
        );
        self.offset = d;
        self.stats.offset_27mhz.store(d as u64, Ordering::Relaxed);
        if self.last_out_pcr.is_some() {
            self.pending_di = true;
        }
    }

    /// The first re-encoded PES of `pid` this epoch: `D ≥ max(0, lateness) +
    /// 80 ms`. The stage's very first latch sets `D` outright — below the
    /// initial 80 ms only when the residency cap demands it; later latches
    /// only raise it. Moves under [`LATCH_TOLERANCE_27MHZ`] are ignored.
    fn latch(&mut self, pid: u16, lateness: i64) {
        let (d, capped) = self.target_offset(lateness, lateness.max(0) + MARGIN_27MHZ);
        let d = if self.latched_once { d.max(self.offset) } else { d };
        self.latched_once = true;
        if (d - self.offset).abs() >= LATCH_TOLERANCE_27MHZ {
            self.set_offset(d, "latched on the first re-encoded PES");
        }
        if capped {
            self.warn_residency(pid, Some(lateness));
        }
    }

    /// A re-encoded PES arrived behind the output PCR.
    fn guard(&mut self, pid: u16, lateness: i64) {
        self.stats.late_frames.fetch_add(1, Ordering::Relaxed);
        let before = self.offset;
        let (d, capped) = self.target_offset(lateness, lateness + MARGIN_27MHZ);
        if d > self.offset {
            self.stats.offset_raises.fetch_add(1, Ordering::Relaxed);
            self.set_offset(d, "a re-encoded PES was late");
        }
        if capped {
            self.warn_residency(pid, Some(lateness));
        }
        self.late_since_warn += 1;
        let now = Instant::now();
        if self.last_late_warn.is_some_and(|t| now.duration_since(t) < LATE_WARN_EVERY) {
            return;
        }
        self.last_late_warn = Some(now);
        let count = std::mem::take(&mut self.late_since_warn);
        let late_ms = (lateness - before) as f64 / 27_000.0;
        tracing::warn!(
            "ts_pcr_remux: a re-encoded PES on PID 0x{pid:04X} arrived {late_ms:.1} ms behind the \
             output PCR ({count} since the last report); the transcode PCR delay was raised from \
             {:.0} ms to {:.0} ms with a PCR discontinuity",
            before as f64 / 27_000.0,
            self.offset as f64 / 27_000.0
        );
        // The event's message is constant — the numbers ride in `details`
        // only — so the manager, which coalesces unacknowledged events on
        // their message, keeps one row with a count per output instead of
        // a new alarm every report (up to 360 an hour).
        self.send_event(
            crate::manager::events::EventSeverity::Warning,
            "re-encoded PES are arriving behind the output PCR; the transcode PCR delay is \
             raised to cover them, each raise a PCR discontinuity (PID, lateness and delay in \
             the details)",
            serde_json::json!({
                "error_code": "transcode_pcr_late",
                "pid": pid,
                "late_ms": late_ms,
                "offset_ms": self.offset as f64 / 27_000.0,
                "late_frames": self.stats.late_frames.load(Ordering::Relaxed),
                "since_last_report": count,
            }),
        );
    }

    /// `lateness` is the re-encoded PES's when a latch or a guard raise hit
    /// the cap; `None` when the video's lead grew past it later (`pid` is
    /// then the video PID).
    fn warn_residency(&mut self, pid: u16, lateness: Option<i64>) {
        if self.residency_warned {
            return;
        }
        self.residency_warned = true;
        let lead_ms = self.max_video_lead.unwrap_or(0) as f64 / 27_000.0;
        let offset_ms = self.offset as f64 / 27_000.0;
        let residency_ms = lead_ms + offset_ms;
        let message = match lateness {
            Some(_) => format!(
                "keeping PID 0x{pid:04X} on time needs a transcode PCR delay of {offset_ms:.0} ms, \
                 which puts the video's largest lead at {residency_ms:.0} ms — past the 1 s T-STD \
                 residency. Lateness wins; a strict decoder may overflow its video buffer"
            ),
            None => format!(
                "the video on PID 0x{pid:04X} now leads the PCR by {lead_ms:.0} ms, which with the \
                 {offset_ms:.0} ms transcode PCR delay puts it {residency_ms:.0} ms ahead — past the \
                 1 s T-STD residency. The delay is not lowered once re-encoded PES were measured \
                 against it; a strict decoder may overflow its video buffer"
            ),
        };
        self.emit(
            crate::manager::events::EventSeverity::Warning,
            &message,
            serde_json::json!({
                "error_code": "transcode_pcr_residency_exceeded",
                "pid": pid,
                "lateness_ms": lateness.map(|l| l as f64 / 27_000.0),
                "offset_ms": offset_ms,
                "video_residency_ms": residency_ms,
            }),
        );
    }

    /// Log `message` and raise it as an event (see [`Self::send_event`]).
    fn emit(
        &self,
        severity: crate::manager::events::EventSeverity,
        message: &str,
        details: serde_json::Value,
    ) {
        match severity {
            crate::manager::events::EventSeverity::Info => tracing::info!("ts_pcr_remux: {message}"),
            _ => tracing::warn!("ts_pcr_remux: {message}"),
        }
        self.send_event(severity, message, details);
    }

    /// Raise an event on the stage's scope (the output, or the input on
    /// the ingress transcoder), without logging it.
    fn send_event(
        &self,
        severity: crate::manager::events::EventSeverity,
        message: &str,
        details: serde_json::Value,
    ) {
        let Some((sender, id, input_scope)) = self.event_sink.as_ref() else {
            return;
        };
        let noun = if *input_scope { "Input" } else { "Output" };
        let message = format!("{noun} '{id}': {message}");
        if *input_scope {
            sender.emit_input_with_details(
                severity,
                crate::manager::events::category::FLOW,
                message,
                id,
                details,
            );
        } else {
            sender.emit_output_with_details(
                severity,
                crate::manager::events::category::FLOW,
                message,
                id,
                details,
            );
        }
    }

    /// Enter synthesis when a re-encoded video PES on the PCR_PID finds that
    /// the input never carried a PCR, or that the video replacer counted a
    /// second of the input's own video decode time without one
    /// ([`Self::set_input_pcr_starved`]).
    fn check_synth_entry(&mut self) {
        if self.synth.active || (self.ever_input_pcr && !self.input_starved) {
            return;
        }
        self.synth.active = true;
        if self.last_out_pcr.is_some() {
            self.epoch_change(None);
        }
        tracing::info!("ts_pcr_remux: no input PCR — synthesising PCR from the re-encoded video");
        if !self.synth.announced {
            self.synth.announced = true;
            self.emit(
                crate::manager::events::EventSeverity::Info,
                "the transcoded stream carries no usable PCR; PCR is synthesised from the \
                 re-encoded video's decode timestamps until one arrives",
                serde_json::json!({ "error_code": "transcode_pcr_synthesized" }),
            );
        }
    }

    /// The synthetic clock at video decode time `raw` (27 MHz, unshifted):
    /// the video's own timeline held [`Synth::extra`] behind it.
    fn synth_value(&self, raw: u64) -> u64 {
        let extra = self.synth.extra as u64 % PCR_MODULUS_27MHZ;
        (raw % PCR_MODULUS_27MHZ + PCR_MODULUS_27MHZ - extra) % PCR_MODULUS_27MHZ
    }

    /// Synthesis: a PCR at `DTS − extra − D` right before this video PES.
    /// Nothing is latched against it — the synthetic clock is the video's,
    /// so it can neither measure the pipeline nor move `D`.
    fn synth_at_video_pes(
        &mut self,
        i: usize,
        dts27: u64,
        pkt: &[u8; TS_PACKET_SIZE],
        out: &mut Vec<u8>,
    ) {
        let Some(c) = self.checked[i] else {
            return;
        };
        if let Some((s, p)) = self.synth.anchor {
            let dv = pcr_diff_27mhz(dts27, s);
            if dv <= 0 || dv > STALE_EPOCH_27MHZ {
                // The video's own timeline jumped: a new synthetic epoch.
                self.epoch_change(None);
                self.synth.rate = None;
            } else {
                let dp = self.packets.saturating_sub(p);
                self.synth.rate = (dp > 0).then_some((dv, dp));
            }
        }
        self.synth.anchor = Some((dts27, self.packets));
        let v = self.synth_value(dts27);
        self.last_in_pcr = Some(v);
        let out_pcr = self.shift(v);
        let cc = c.last_out_cc.unwrap_or_else(|| {
            // The PES start about to go out, renumbered, minus one.
            let mut p = *pkt;
            let mut r = c.cc;
            r.emit(&mut p, false);
            ts_cc(&p).wrapping_sub(1) & 0x0F
        });
        let di = std::mem::take(&mut self.pending_di);
        out.extend_from_slice(&pcr_only_packet(c.pid, cc, out_pcr, di));
        self.last_out_pcr = Some(out_pcr);
        self.applied_at_last_out = None;
        self.stats.synthesized_pcrs.fetch_add(1, Ordering::Relaxed);
        match self.synth.rate {
            Some((dv, dp)) => {
                let every = ((SYNTH_SPACING_27MHZ as u128 * dp as u128) / dv as u128).max(1) as u64;
                self.synth.next_at = Some(self.packets + every);
                self.synth.limit = dts27 + (dv - 27_000).max(0) as u64;
            }
            None => self.synth.next_at = None,
        }
    }

    /// Synthesis: another re-encoded PID's PES (the audio) is `lateness`
    /// behind the synthetic clock. When it would leave less than the
    /// 80 ms margin under `D`, the synthetic clock is held further behind
    /// the video (one PCR step, DI) — within the video's 1 s residency.
    fn synth_allow(&mut self, pid: u16, lateness: i64) {
        if lateness >= MAX_LATENESS_27MHZ {
            return;
        }
        let need = lateness + MARGIN_27MHZ - self.offset;
        if need < LATCH_TOLERANCE_27MHZ {
            return;
        }
        // The video leads the synthetic output PCR by `D + extra`.
        let room = (MAX_RESIDENCY_27MHZ - self.offset - self.synth.extra).max(0);
        let add = need.min(room);
        if add > 0 {
            self.synth.extra += add;
            self.pending_di = true;
            tracing::info!(
                "ts_pcr_remux: synthesised PCR held {:.1} ms behind the video so PID 0x{pid:04X} \
                 stays ahead of it",
                self.synth.extra as f64 / 27_000.0
            );
        }
        if need > room {
            self.warn_residency(pid, Some(lateness));
        }
    }

    /// Synthesis: an interpolated PCR when the next one is due, as long as
    /// it stays below the next video PES's predicted value.
    fn maybe_synth_between(&mut self, out: &mut Vec<u8>) {
        if !self.synth.active {
            return;
        }
        let (Some(at), Some((s, p)), Some((dv, dp)), Some(pid)) =
            (self.synth.next_at, self.synth.anchor, self.synth.rate, self.pcr_pid)
        else {
            return;
        };
        if self.packets < at {
            return;
        }
        let raw = s + ((self.packets - p) as u128 * dv as u128 / dp as u128) as u64;
        let cc = self
            .checked_index(pid)
            .and_then(|i| self.checked[i])
            .and_then(|c| c.last_out_cc);
        let (Some(cc), true) = (cc, raw < self.synth.limit) else {
            self.synth.next_at = None;
            return;
        };
        let v = self.synth_value(raw);
        self.last_in_pcr = Some(v);
        let out_pcr = self.shift(v);
        let di = std::mem::take(&mut self.pending_di);
        out.extend_from_slice(&pcr_only_packet(pid, cc, out_pcr, di));
        self.last_out_pcr = Some(out_pcr);
        self.applied_at_last_out = None;
        self.stats.synthesized_pcrs.fetch_add(1, Ordering::Relaxed);
        let every = ((SYNTH_SPACING_27MHZ as u128 * dp as u128) / dv as u128).max(1) as u64;
        self.synth.next_at = Some(at + every);
    }

    /// Continuity for a re-encoded PID: drop the payload of a stale PES
    /// (keeping any PCR / DI as an AF-only packet) and renumber every later
    /// CC by the packets dropped. Returns `false` when the packet goes.
    fn emit_checked(&mut self, i: usize, pkt: &mut [u8; TS_PACKET_SIZE]) -> bool {
        let Some(c) = self.checked[i].as_mut() else {
            return true;
        };
        let mut stripped = false;
        if c.dropping && ts_has_payload(pkt) {
            let keeps_af = extract_pcr(pkt).is_some() || ts_discontinuity_indicator(pkt);
            if !(keeps_af && strip_to_af_only(pkt)) {
                c.cc.drop_payload();
                return false;
            }
            stripped = true;
        }
        c.cc.emit(pkt, stripped);
        if ts_has_payload(pkt) {
            c.last_out_cc = Some(ts_cc(pkt));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ts_test_fixtures::{
        packetize_sections, pat_packet, payload_packet, pes_start_packet, pmt_section,
    };

    const PMT: u16 = 0x1000;
    const V: u16 = 0x100;
    const A: u16 = 0x101;
    const MS: u64 = 27_000;

    fn psi(pcr_pid: u16) -> Vec<u8> {
        let mut v = pat_packet(&[(1, PMT)], 0, 0).to_vec();
        let pmt = pmt_section(1, 0, pcr_pid, &[], &[(0x1B, V, &[]), (0x0F, A, &[])]);
        v.extend_from_slice(&packetize_sections(PMT, &[&pmt], 0)[0]);
        v
    }

    fn pcr_pkts(out: &[u8], pid: u16) -> Vec<(u64, bool, u8)> {
        out.chunks(TS_PACKET_SIZE)
            .filter(|p| ts_pid(p) == pid)
            .filter_map(|p| extract_pcr(p).map(|v| (v, ts_discontinuity_indicator(p), ts_cc(p))))
            .collect()
    }

    /// A stream with an input PCR every 30 ms on the video PID (AF-only, as
    /// the video replacer now emits it) and a re-encoded video PES every
    /// 40 ms whose DTS is `lead` ahead of the PCR at its position.
    struct Src {
        t: u64,
        cc: u8,
    }

    impl Src {
        fn pcr(&self, t: u64) -> [u8; TS_PACKET_SIZE] {
            pcr_only_packet(V, self.cc.wrapping_sub(1) & 0x0F, t, false)
        }
        fn frame(&mut self, dts27: u64) -> Vec<u8> {
            let mut v = pes_start_packet(V, self.cc, 0xE0, dts27 / 300, Some(dts27 / 300)).to_vec();
            self.cc = (self.cc + 1) & 0x0F;
            v.extend_from_slice(&payload_packet(V, self.cc));
            self.cc = (self.cc + 1) & 0x0F;
            v
        }
    }

    fn run(stage: &mut TsPcrRemux, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        stage.set_replaced_pids(None, Some(V));
        stage.process(input, &mut out);
        out
    }

    #[test]
    fn output_pcr_is_input_minus_d_at_the_same_positions() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 27_000_000, cc: 0 };
        let mut input = psi(V);
        // 1 s: PCR every 30 ms, frames every 40 ms, DTS = PCR + 300 ms (the
        // source lead minus the pipeline), so no PES is ever late.
        let mut pcr_positions = Vec::new();
        for k in 0..100u64 {
            let t = src.t + k * 10 * MS;
            if k % 3 == 0 {
                pcr_positions.push(input.len() / TS_PACKET_SIZE);
                input.extend_from_slice(&src.pcr(t));
            }
            if k % 4 == 0 {
                input.extend(src.frame(t + 300 * MS));
            }
        }
        let out = run(&mut s, &input);
        assert_eq!(out.len(), input.len(), "nothing added or removed");
        let d = s.offset_27mhz();
        assert_eq!(d, 80 * MS, "latched: max(0, lateness) + 80 ms");
        let got = pcr_pkts(&out, V);
        assert_eq!(got.len(), pcr_positions.len());
        for (k, (i, (v, di, _))) in pcr_positions.iter().zip(&got).enumerate() {
            let src_pcr = extract_pcr(&input[i * TS_PACKET_SIZE..(i + 1) * TS_PACKET_SIZE]).unwrap();
            assert_eq!(extract_pcr(&out[i * TS_PACKET_SIZE..(i + 1) * TS_PACKET_SIZE]), Some(*v));
            assert_eq!(*v, src_pcr - d, "PCR {k} is the input's minus D at its position");
            assert!(!di, "a pipeline inside the initial 80 ms never steps the clock");
        }
        for w in got.windows(2) {
            assert!(w[1].0 > w[0].0, "strictly increasing");
            assert!(w[1].0 - w[0].0 <= 35 * MS, "spacing is the input's");
        }
    }

    #[test]
    fn a_late_pipeline_latches_d_once_and_the_guard_raises_it() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 27_000_000, cc: 0 };
        let t = src.t;
        let mut input = psi(V);
        // PCRs every 30 ms; the encoder delivers its first frame 1.5 s
        // behind that frame's decode time (a deep lookahead).
        for k in 0..=50u64 {
            input.extend_from_slice(&src.pcr(t + k * 30 * MS));
        }
        input.extend(src.frame(t));
        run(&mut s, &input);
        assert_eq!(s.offset_27mhz(), 1_580 * MS, "lateness + 80 ms, one latch");
        assert_eq!(s.stats.late_frames.load(Ordering::Relaxed), 0);
        // Steady state at that depth: never late again, exactly one DI.
        let mut input = Vec::new();
        for k in 1..20u64 {
            input.extend_from_slice(&src.pcr(t + 1_500 * MS + k * 40 * MS));
            input.extend(src.frame(t + k * 40 * MS));
        }
        let out = run(&mut s, &input);
        let di: Vec<bool> = pcr_pkts(&out, V).iter().map(|p| p.1).collect();
        assert_eq!(di.iter().filter(|d| **d).count(), 1, "one DI, on the PCR after the latch");
        assert!(di[0]);
        assert_eq!(s.stats.late_frames.load(Ordering::Relaxed), 0);
        // One frame 200 ms later still: the guard raises D once.
        let mut input = Vec::new();
        input.extend_from_slice(&src.pcr(t + 1_500 * MS + 21 * 40 * MS));
        input.extend(src.frame(t + 21 * 40 * MS - 200 * MS));
        input.extend_from_slice(&src.pcr(t + 1_500 * MS + 22 * 40 * MS));
        let out = run(&mut s, &input);
        assert_eq!(s.stats.late_frames.load(Ordering::Relaxed), 1);
        assert_eq!(s.stats.offset_raises.load(Ordering::Relaxed), 1);
        assert_eq!(s.offset_27mhz(), (1_500 + 200 + 80) * MS);
        let got = pcr_pkts(&out, V);
        assert!(!got[0].1 && got[1].1, "DI on the PCR after the raise");
        assert_eq!(got[1].0, t + 1_500 * MS + 22 * 40 * MS - 1_780 * MS);
    }

    #[test]
    fn a_pes_seconds_off_the_timeline_never_moves_d() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 60 * 27_000_000, cc: 0 };
        let t = src.t;
        let mut input = psi(V);
        for k in 0..10u64 {
            input.extend_from_slice(&src.pcr(t + k * 40 * MS));
            input.extend(src.frame(t + k * 40 * MS + 200 * MS));
        }
        // A frame stamped 8 s in the past (a PTS from another timeline).
        input.extend_from_slice(&src.pcr(t + 400 * MS));
        input.extend(src.frame(t - 8_000 * MS));
        run(&mut s, &input);
        assert_eq!(s.offset_27mhz(), 80 * MS);
        assert_eq!(s.stats.late_frames.load(Ordering::Relaxed), 0);
    }

    /// While nothing is re-encoded (a codec the replacers cannot decode, a
    /// replacer's passthrough fallback) the stage leaves every byte alone.
    #[test]
    fn nothing_re_encoded_leaves_the_stream_byte_identical() {
        let mut s = TsPcrRemux::new();
        let src = Src { t: 27_000_000, cc: 0 };
        let mut input = psi(V);
        for k in 0..10u64 {
            input.extend_from_slice(&src.pcr(src.t + k * 30 * MS));
            input.extend_from_slice(&pes_start_packet(V, k as u8, 0xE0, 90_000 + k * 3600, None));
        }
        let mut out = Vec::new();
        s.set_replaced_pids(None, None);
        s.process(&input, &mut out);
        assert_eq!(out, input);
        assert_eq!(s.offset_27mhz(), 0);
        assert_eq!(s.stats.snapshot().offset_ms, 0.0);
    }

    /// A PCR-per-frame source (PCR = DTS, the RTMP / RTSP / WebRTC ingest
    /// muxer) whose re-encoded frame `k` leaves one frame late, after frame
    /// `k + 1`'s PCR — a pipeline one frame deep (a decoder reorder hold).
    /// At 5 fps every step is 200 ms and at 0.5 fps two seconds: the
    /// input's own clock, not an epoch — no DI per PCR, one latch, nothing
    /// dropped as stale. (The replacers' own PES wait no longer adds that
    /// frame: they emit a PCR after the output its packet completes — see
    /// `ts_video_replace`'s `a_pcr_per_frame_source_latches_no_frame_interval`.)
    #[test]
    fn a_pcr_per_frame_source_at_low_frame_rates_is_one_timeline() {
        for (interval_ms, d_ms) in [(200u64, 280u64), (2_000, 2_080)] {
            let mut s = TsPcrRemux::new();
            let mut src = Src { t: 27_000_000, cc: 0 };
            let mut input = psi(V);
            for k in 0..12u64 {
                let t = src.t + k * interval_ms * MS;
                input.extend_from_slice(&src.pcr(t));
                if k > 0 {
                    input.extend(src.frame(t - interval_ms * MS));
                }
            }
            let out = run(&mut s, &input);
            let case = format!("{interval_ms} ms per frame");
            let st = s.stats.snapshot();
            assert_eq!(st.epochs, 0, "{case}");
            assert_eq!(st.stale_frames_dropped, 0, "{case}");
            assert_eq!(st.late_frames, 0, "{case}");
            assert_eq!(s.offset_27mhz(), d_ms * MS, "{case}: one frame + 80 ms");
            let di = pcr_pkts(&out, V).iter().filter(|p| p.1).count();
            assert_eq!(di, 1, "{case}: the latch's DI only");
            let pes = out.chunks(TS_PACKET_SIZE).filter(|p| ts_pid(p) == V && ts_pusi(p)).count();
            assert_eq!(pes, 11, "{case}: every frame kept");
        }
    }

    /// A source that pauses 900 ms (no PCR, no frames) sends the frame that
    /// was in the pipeline after the pause, 940 ms behind its decode time.
    /// The pause is the source's: `D` does not grow by it.
    #[test]
    fn a_source_pause_does_not_ratchet_d() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 27_000_000, cc: 0 };
        let mut input = psi(V);
        let at = |k: u64| src.t + k * 40 * MS + if k >= 16 { 900 * MS } else { 0 };
        let times: Vec<u64> = (0..30).map(at).collect();
        for k in 0..30usize {
            input.extend_from_slice(&src.pcr(times[k]));
            if k > 0 {
                input.extend(src.frame(times[k - 1]));
            }
        }
        let out = run(&mut s, &input);
        assert_eq!(s.offset_27mhz(), 120 * MS, "one frame + 80 ms, latched once");
        let st = s.stats.snapshot();
        assert_eq!((st.epochs, st.offset_raises, st.late_frames), (0, 0, 0));
        assert_eq!(pcr_pkts(&out, V).iter().filter(|p| p.1).count(), 1);
    }

    /// The largest video lead is re-checked as it grows: before anything
    /// re-encoded was measured `D` drops to the cap; after, the residency
    /// Warning says what it cannot fix.
    #[test]
    fn a_growing_video_lead_is_held_to_the_residency_cap() {
        let t0 = 27_000_000u64;
        // Before any re-encoded audio: D follows the cap down.
        let mut s = TsPcrRemux::new();
        let mut input = psi(V);
        input.extend_from_slice(&pcr_only_packet(V, 15, t0, false));
        input.extend_from_slice(&pes_start_packet(V, 0, 0xE0, (t0 + 943 * MS) / 300, None));
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), None);
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 57 * MS);
        // After the audio latched at 80 ms against a 500 ms lead, the lead
        // grows to 973 ms: D comes down once, to the largest measured
        // lateness (5 ms) + the 40 ms minimum margin — above the 27 ms cap,
        // so the Warning says what is left. A further growth moves nothing
        // and warns no more.
        let mut s = TsPcrRemux::new();
        let (tx, mut rx) = crate::manager::events::event_channel();
        s.set_event_sink(tx, "out-1", false);
        let mut input = psi(V);
        input.extend_from_slice(&pcr_only_packet(V, 15, t0, false));
        input.extend_from_slice(&pes_start_packet(V, 0, 0xE0, (t0 + 500 * MS) / 300, None));
        input.extend_from_slice(&pes_start_packet(A, 0, 0xC0, (t0 - 5 * MS) / 300, None));
        input.extend_from_slice(&pcr_only_packet(V, 0, t0 + 30 * MS, false));
        input.extend_from_slice(&pes_start_packet(V, 1, 0xE0, (t0 + 30 * MS + 973 * MS) / 300, None));
        input.extend_from_slice(&pcr_only_packet(V, 1, t0 + 60 * MS, false));
        input.extend_from_slice(&pes_start_packet(V, 2, 0xE0, (t0 + 60 * MS + 1_000 * MS) / 300, None));
        input.extend_from_slice(&pcr_only_packet(V, 2, t0 + 90 * MS, false));
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), None);
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 45 * MS);
        let pcrs = pcr_pkts(&out, V);
        assert_eq!(pcrs.iter().map(|p| p.1).collect::<Vec<_>>(), vec![false, false, true, false]);
        assert_eq!(pcrs[2].0, t0 + 60 * MS - 45 * MS, "one forward step of 35 ms, DI");
        let ev = rx.try_recv().expect("residency Warning");
        let d = ev.details.unwrap();
        assert_eq!(d["error_code"], "transcode_pcr_residency_exceeded");
        assert!(d["lateness_ms"].is_null());
        assert!(rx.try_recv().is_err(), "once per epoch");
    }

    /// A4(a), Sky audio-only: the audio's smallest lead is 61.8 ms and the
    /// passthrough video's lead reaches 943 ms after the audio latched. `D`
    /// comes down once to the 57 ms cap less 20 ms of headroom — one forward
    /// PCR step, DI — so the video's residency is 980 ms; a lead that grows
    /// past what the headroom absorbs is only warned about.
    #[test]
    fn a_video_lead_past_the_cap_after_the_latch_lowers_d_once() {
        let t0 = 27_000_000u64;
        let mut s = TsPcrRemux::new();
        let (tx, mut rx) = crate::manager::events::event_channel();
        s.set_event_sink(tx, "out-1", false);
        let mut input = psi(V);
        input.extend_from_slice(&pcr_only_packet(V, 15, t0, false));
        input.extend_from_slice(&pes_start_packet(V, 0, 0xE0, (t0 + 434 * MS) / 300, None));
        input.extend_from_slice(&pes_start_packet(A, 0, 0xC0, (t0 + 62 * MS) / 300, None));
        input.extend_from_slice(&pcr_only_packet(V, 0, t0 + 30 * MS, false));
        input.extend_from_slice(&pes_start_packet(A, 1, 0xC0, (t0 + 30 * MS + 70 * MS) / 300, None));
        input.extend_from_slice(&pes_start_packet(V, 1, 0xE0, (t0 + 30 * MS + 943 * MS) / 300, None));
        input.extend_from_slice(&pcr_only_packet(V, 1, t0 + 60 * MS, false));
        input.extend_from_slice(&pes_start_packet(V, 2, 0xE0, (t0 + 60 * MS + 970 * MS) / 300, None));
        input.extend_from_slice(&pcr_only_packet(V, 2, t0 + 90 * MS, false));
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), None);
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 37 * MS, "the cap (1 s − 943 ms) less 20 ms");
        let pcrs = pcr_pkts(&out, V);
        assert_eq!(pcrs.iter().filter(|p| p.1).count(), 1, "one step");
        assert_eq!(pcrs[2].0 - pcrs[1].0, (30 + 43) * MS, "forward by 80 − 37 ms");
        // The later 970 ms lead is past what the headroom absorbs: no second
        // step, the Warning instead.
        let ev = rx.try_recv().expect("residency Warning for the 970 ms lead");
        assert_eq!(ev.details.unwrap()["error_code"], "transcode_pcr_residency_exceeded");
        assert!(rx.try_recv().is_err());
        assert_eq!(s.stats.offset_raises.load(Ordering::Relaxed), 0);
    }

    /// `transcode_pcr_late`'s message is the same on every report — the
    /// PID, lateness and delay ride in `details` — so the manager's
    /// message-keyed coalescing keeps one row per output, not one per
    /// report.
    #[test]
    fn the_late_warning_message_is_constant() {
        let mut s = TsPcrRemux::new();
        let (tx, mut rx) = crate::manager::events::event_channel();
        s.set_event_sink(tx, "out-1", false);
        s.guard(A, 30 * MS as i64);
        s.last_late_warn = None; // past the rate limit
        s.guard(V, 200 * MS as i64);
        let a = rx.try_recv().expect("first report");
        let b = rx.try_recv().expect("second report");
        assert_eq!(a.message, b.message);
        assert_eq!(a.output_id.as_deref(), Some("out-1"));
        assert!(!a.message.contains(" ms"), "{}", a.message);
        let (da, db) = (a.details.unwrap(), b.details.unwrap());
        for d in [&da, &db] {
            assert_eq!(d["error_code"], "transcode_pcr_late");
            for k in ["pid", "late_ms", "offset_ms", "late_frames", "since_last_report"] {
                assert!(d.get(k).is_some(), "{k} in {d}");
            }
        }
        assert_eq!((da["pid"].as_u64(), db["pid"].as_u64()), (Some(A as u64), Some(V as u64)));
        assert_ne!(da["late_ms"], db["late_ms"]);
    }

    /// Only the followed program's video counts toward the cap: another
    /// program's video on an MPTS output leaves `D` alone.
    #[test]
    fn another_programs_video_does_not_count_toward_the_cap() {
        let t0 = 27_000_000u64;
        let mut s = TsPcrRemux::new();
        let mut input = pat_packet(&[(1, PMT), (2, 0x2000)], 0, 0).to_vec();
        let pmt = pmt_section(1, 0, V, &[], &[(0x1B, V, &[]), (0x0F, A, &[])]);
        input.extend_from_slice(&packetize_sections(PMT, &[&pmt], 0)[0]);
        input.extend_from_slice(&pcr_only_packet(V, 15, t0, false));
        input.extend_from_slice(&pes_start_packet(0x200, 0, 0xE0, (t0 + 990 * MS) / 300, None));
        input.extend_from_slice(&pes_start_packet(A, 0, 0xC0, (t0 - 5 * MS) / 300, None));
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), None);
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 80 * MS);
        assert!(s.max_video_lead.is_none());
    }

    #[test]
    fn a_pcr_on_the_audio_pid_is_shifted_for_an_audio_only_chain() {
        let mut s = TsPcrRemux::new();
        let mut input = psi(A);
        let t0 = 27_000_000u64;
        for k in 0..10u64 {
            input.extend_from_slice(&pcr_only_packet(A, (k as u8).wrapping_sub(1) & 0x0F, t0 + k * 30 * MS, false));
            // Audio 20 ms late on arrival.
            input.extend_from_slice(&pes_start_packet(A, k as u8, 0xC0, (t0 + k * 30 * MS - 20 * MS) / 300, None));
        }
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), None);
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 100 * MS);
        let got = pcr_pkts(&out, A);
        assert_eq!(got.len(), 10);
        assert_eq!(got[9].0, t0 + 9 * 30 * MS - 100 * MS);
    }

    #[test]
    fn a_latch_within_10_ms_of_d_does_not_step_the_clock() {
        let mut s = TsPcrRemux::new();
        let mut input = psi(A);
        let t0 = 27_000_000u64;
        for k in 0..5u64 {
            input.extend_from_slice(&pcr_only_packet(A, 0, t0 + k * 30 * MS, false));
            // 5 ms late: the latch wants 85 ms against the initial 80.
            input.extend_from_slice(&pes_start_packet(A, k as u8, 0xC0, (t0 + k * 30 * MS - 5 * MS) / 300, None));
        }
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), None);
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 80 * MS);
        assert!(pcr_pkts(&out, A).iter().all(|p| !p.1), "no DI");
    }

    #[test]
    fn the_residency_cap_trims_the_margin() {
        // Passthrough video 950 ms ahead of the PCR; the audio needs 10 ms.
        let mut s = TsPcrRemux::new();
        let mut input = psi(V);
        let t0 = 27_000_000u64;
        input.extend_from_slice(&pcr_only_packet(V, 15, t0, false));
        input.extend_from_slice(&pes_start_packet(V, 0, 0xE0, (t0 + 950 * MS) / 300, None));
        input.extend_from_slice(&pes_start_packet(A, 0, 0xC0, (t0 - 10 * MS) / 300, None));
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), None);
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 50 * MS, "1 s − 950 ms, above the 40 ms minimum margin");
    }

    #[test]
    fn an_epoch_jump_drops_stale_frames_without_moving_d() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 60 * 27_000_000, cc: 0 };
        let mut input = psi(V);
        for k in 0..10u64 {
            input.extend_from_slice(&src.pcr(src.t + k * 40 * MS));
            input.extend(src.frame(src.t + k * 40 * MS + 200 * MS));
        }
        run(&mut s, &input);
        assert_eq!(s.offset_27mhz(), 80 * MS);
        // Input switch: the clock jumps back 5 s (DI on the input PCR); two
        // frames of the old epoch are still in the encoder, then new ones.
        let t1 = src.t - 5_000 * MS;
        let mut input = Vec::new();
        let mut p = src.pcr(t1);
        set_discontinuity_indicator(&mut p);
        input.extend_from_slice(&p);
        input.extend(src.frame(src.t + 10 * 40 * MS + 200 * MS));
        input.extend(src.frame(src.t + 11 * 40 * MS + 200 * MS));
        input.extend(src.frame(t1 + 200 * MS));
        let out = run(&mut s, &input);
        assert_eq!(s.stats.stale_frames_dropped.load(Ordering::Relaxed), 2);
        assert_eq!(s.offset_27mhz(), 80 * MS, "stale frames never drive D");
        assert_eq!(s.stats.late_frames.load(Ordering::Relaxed), 0);
        // What remains on the video PID: the PCR (DI) and one PES, CC continuous.
        let v: Vec<&[u8]> = out.chunks(TS_PACKET_SIZE).filter(|p| ts_pid(p) == V).collect();
        assert_eq!(v.len(), 3);
        assert!(ts_discontinuity_indicator(v[0]));
        let ccs: Vec<u8> = v.iter().map(|p| ts_cc(p)).collect();
        // The last payload before the switch had CC 19 & 15 = 3.
        assert_eq!(ccs, vec![3, 4, 5]);
    }

    /// The input PCR steps 1.5 s back (an upstream transcode raising its
    /// delay: DI, the PES running straight on) or 1.5 s forward (lowering
    /// it). The re-encoded PID's own input PES, seen ahead of the replacer,
    /// carry on across it, so the step is the PCR's alone and nothing is
    /// stale: every frame goes out. Judged by the PCR alone, every frame for
    /// as long as the step was "closer to the old timeline" and dropped.
    #[test]
    fn a_pcr_that_steps_alone_makes_nothing_stale() {
        for step_ms in [-1_500i64, 1_500] {
            let mut s = TsPcrRemux::new();
            let mut src = Src { t: 60 * 27_000_000, cc: 0 };
            let feed = |s: &mut TsPcrRemux, input: &[u8]| {
                // The replacer re-encodes in place: its input carries the
                // PES the stage then sees, at the same times.
                s.observe_input(input);
                run(s, input)
            };
            feed(&mut s, &psi(V));
            for k in 0..10u64 {
                let mut chunk = src.pcr(src.t + k * 40 * MS).to_vec();
                chunk.extend(src.frame(src.t + k * 40 * MS + 200 * MS));
                feed(&mut s, &chunk);
            }
            let mut frames = 0;
            let mut out = Vec::new();
            for k in 10..60u64 {
                let mut chunk = Vec::new();
                let t = (src.t + k * 40 * MS).wrapping_add_signed(step_ms * MS as i64);
                let mut p = src.pcr(t);
                if k == 10 {
                    set_discontinuity_indicator(&mut p);
                }
                chunk.extend_from_slice(&p);
                chunk.extend(src.frame(src.t + k * 40 * MS + 200 * MS));
                frames += 1;
                out.extend(feed(&mut s, &chunk));
            }
            let st = s.stats.snapshot();
            assert_eq!(st.stale_frames_dropped, 0, "{step_ms}: nothing stale");
            assert_eq!(st.epochs, 1, "{step_ms}");
            let kept = out.chunks(TS_PACKET_SIZE).filter(|p| ts_pid(p) == V && ts_pusi(p)).count();
            assert_eq!(kept, frames, "{step_ms}: every frame out");
        }
    }

    /// The same jump as [`an_epoch_jump_drops_stale_frames_without_moving_d`]
    /// with the input watched: the replacer's input PES move with the PCR
    /// (a switch to another source), so the frames still in its encoder
    /// from the old epoch are stale and dropped as before.
    #[test]
    fn an_epoch_jump_the_input_pes_follow_still_drops_the_frames_in_flight() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 60 * 27_000_000, cc: 0 };
        for k in 0..10u64 {
            let mut chunk = if k == 0 { psi(V) } else { Vec::new() };
            chunk.extend_from_slice(&src.pcr(src.t + k * 40 * MS));
            chunk.extend(src.frame(src.t + k * 40 * MS + 200 * MS));
            s.observe_input(&chunk);
            run(&mut s, &chunk);
        }
        let t1 = src.t - 5_000 * MS;
        let mut p = src.pcr(t1);
        set_discontinuity_indicator(&mut p);
        // What reached the replacer: the PCR, then the new source's PES.
        let mut seen = p.to_vec();
        seen.extend(Src { t: 0, cc: 4 }.frame(t1 + 240 * MS));
        s.observe_input(&seen);
        // What leaves it: two frames of the old epoch, then a new one.
        let mut input = p.to_vec();
        input.extend(src.frame(src.t + 10 * 40 * MS + 200 * MS));
        input.extend(src.frame(src.t + 11 * 40 * MS + 200 * MS));
        input.extend(src.frame(t1 + 200 * MS));
        run(&mut s, &input);
        assert_eq!(s.stats.stale_frames_dropped.load(Ordering::Relaxed), 2);
        assert_eq!(s.offset_27mhz(), 80 * MS, "stale frames never drive D");
    }

    #[test]
    fn no_input_pcr_synthesises_one_from_the_video() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 0, cc: 0 };
        let mut input = psi(V);
        let d0 = 90_000u64 * 300;
        for k in 0..10u64 {
            input.extend(src.frame(d0 + k * 40 * MS));
            for _ in 0..8 {
                input.extend_from_slice(&payload_packet(A, 0));
            }
        }
        let out = run(&mut s, &input);
        let got = pcr_pkts(&out, V);
        assert!(got.len() >= 19, "one per frame plus in-between: {}", got.len());
        assert_eq!(got[0].0, d0 - 80 * MS);
        // The first interval is one frame (no packet rate known yet); from
        // then on an interpolated PCR keeps every gap within 35 ms.
        assert_eq!(got[1].0 - got[0].0, 40 * MS);
        for w in got[1..].windows(2) {
            assert!(w[1].0 > w[0].0);
            assert!(w[1].0 - w[0].0 <= 35 * MS, "{} ms", (w[1].0 - w[0].0) / MS);
        }
        assert_eq!(s.stats.synthesized_pcrs.load(Ordering::Relaxed), got.len() as u64);
        // An input PCR ends synthesis with a DI.
        let mut input = src.pcr(d0 + 10 * 40 * MS - 300 * MS).to_vec();
        input.extend(src.frame(d0 + 10 * 40 * MS));
        let out = run(&mut s, &input);
        let got = pcr_pkts(&out, V);
        assert!(got[0].1, "DI on the first input PCR");
        assert!(!s.synth.active);
    }

    /// Output PCR in force at every PES start on `pid`: `(DTS 27 MHz, PCR)`.
    fn pes_vs_pcr(out: &[u8], pcr_pid: u16, pid: u16) -> Vec<(u64, u64)> {
        let mut last = None;
        let mut v = Vec::new();
        for p in out.chunks(TS_PACKET_SIZE) {
            if ts_pid(p) == pcr_pid
                && let Some(pcr) = extract_pcr(p)
            {
                last = Some(pcr);
            }
            if ts_pid(p) == pid
                && ts_pusi(p)
                && let (Some((_, ts)), Some(pcr)) = (pes_decode_ts(p), last)
            {
                v.push((ts * 300, pcr));
            }
        }
        v
    }

    /// R1. The replacers keep input PCRs at their input positions, 33 ms
    /// apart, while re-encoded frames leave the codec in bursts — here five
    /// 29.97 fps frames, 133 ms of DTS, between two input PCRs. That is
    /// not an input without PCR: nothing is synthesised, no epoch, no DI,
    /// `D` stays. (The old test — 100 ms of re-encoded DTS since the last
    /// input PCR — synthesised here, latched the audio against the video's
    /// clock and raised `D` by ~0.9 s.)
    #[test]
    fn a_burst_of_re_encoded_frames_between_input_pcrs_is_not_starvation() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 27_000_000, cc: 0 };
        let mut input = psi(V);
        let frame = 1_001 * 900; // 29.97 fps
        let mut k = 0u64;
        for n in 0..90u64 {
            input.extend_from_slice(&src.pcr(src.t + n * 33 * MS));
            if n % 5 == 4 {
                for _ in 0..5 {
                    input.extend(src.frame(src.t + 300 * MS + k * frame));
                    k += 1;
                }
            }
        }
        let out = run(&mut s, &input);
        let st = s.stats.snapshot();
        assert_eq!((st.synthesized_pcrs, st.epochs, st.offset_raises), (0, 0, 0));
        assert_eq!(s.offset_27mhz(), 80 * MS);
        assert!(pcr_pkts(&out, V).iter().all(|p| !p.1), "no DI");
    }

    /// R1. An input that loses its PCR (the video replacer counts a second of
    /// its own video decode time without one) gets a synthesised PCR — but
    /// the synthetic clock is the video's, so it never moves `D`: the
    /// audio, muxed 800 ms behind the video, is kept ahead of it by holding
    /// the synthetic clock back instead. When the input PCR returns,
    /// synthesis stops (DI) and `D` is what it was.
    #[test]
    fn synthesis_holds_its_own_clock_back_and_never_moves_d() {
        let mut s = TsPcrRemux::new();
        let t0 = 27_000_000u64;
        let mut cc = [0u8; 2];
        let av = |input: &mut Vec<u8>, cc: &mut [u8; 2], t: u64| {
            input.extend_from_slice(&pes_start_packet(V, cc[0], 0xE0, (t + 900 * MS) / 300, None));
            input.extend_from_slice(&pes_start_packet(A, cc[1], 0xC0, (t + 100 * MS) / 300, None));
            cc[0] = (cc[0] + 1) & 0x0F;
            cc[1] = (cc[1] + 1) & 0x0F;
        };
        let mut input = psi(V);
        for k in 0..10u64 {
            input.extend_from_slice(&pcr_only_packet(V, cc[0].wrapping_sub(1) & 0x0F, t0 + k * 40 * MS, false));
            av(&mut input, &mut cc, t0 + k * 40 * MS);
        }
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), Some(V));
        s.process(&input, &mut out);
        assert_eq!(s.offset_27mhz(), 80 * MS);
        // The PCR stops; the video replacer reports it.
        let mut input = Vec::new();
        for k in 10..40u64 {
            av(&mut input, &mut cc, t0 + k * 40 * MS);
        }
        s.set_input_pcr_starved(true);
        let mut out = Vec::new();
        s.process(&input, &mut out);
        assert!(s.synth.active);
        assert!(s.stats.synthesized_pcrs.load(Ordering::Relaxed) >= 29);
        assert_eq!(s.offset_27mhz(), 80 * MS, "the synthetic clock never moves D");
        let extra = s.synth.extra;
        assert!((800 * MS as i64..=840 * MS as i64).contains(&extra), "held back by the audio's lag: {extra}");
        // Once measured — against the anchor, then against the in-between
        // PCR the audio actually sits behind — every audio PES is ≥ 80 ms
        // ahead of the synthetic PCR, and the video leads it by D + extra
        // < 1 s.
        for (dts, pcr) in pes_vs_pcr(&out, V, A).into_iter().skip(2) {
            assert!(dts >= pcr + 80 * MS, "audio {} ms ahead", (dts as i64 - pcr as i64) / MS as i64);
        }
        for (dts, pcr) in pes_vs_pcr(&out, V, V).into_iter().skip(2) {
            assert_eq!(dts - pcr, 80 * MS + extra as u64);
        }
        assert!(80 * MS + (extra as u64) < 1_000 * MS);
        // The input PCR returns: synthesis stops, D stays, nothing raised.
        s.set_input_pcr_starved(false);
        let mut input = Vec::new();
        for k in 40..50u64 {
            input.extend_from_slice(&pcr_only_packet(V, cc[0].wrapping_sub(1) & 0x0F, t0 + k * 40 * MS, false));
            av(&mut input, &mut cc, t0 + k * 40 * MS);
        }
        let mut out = Vec::new();
        s.process(&input, &mut out);
        assert!(!s.synth.active);
        assert_eq!(s.synth.extra, 0);
        assert!(pcr_pkts(&out, V)[0].1, "DI: back on the input's clock");
        assert_eq!(s.offset_27mhz(), 80 * MS);
        let st = s.stats.snapshot();
        assert_eq!((st.offset_raises, st.late_frames), (0, 0));
    }

    /// A genuine PCR-less source (no input PCR ever) with its audio muxed
    /// 600 ms behind its video: a PCR is synthesised from the first frame,
    /// at most 35 ms apart, and the audio stays ahead of it.
    #[test]
    fn a_pcr_less_source_keeps_its_audio_ahead_of_the_synthesised_pcr() {
        let mut s = TsPcrRemux::new();
        let d0 = 90_000u64 * 300;
        let mut input = psi(V);
        for k in 0..25u64 {
            let t = d0 + k * 40 * MS;
            input.extend_from_slice(&pes_start_packet(V, k as u8, 0xE0, t / 300, None));
            for j in 0..6 {
                input.extend_from_slice(&payload_packet(V, (k as u8).wrapping_add(j)));
            }
            input.extend_from_slice(&pes_start_packet(A, k as u8, 0xC0, (t - 600 * MS) / 300, None));
        }
        let mut out = Vec::new();
        s.set_replaced_pids(Some(A), Some(V));
        s.process(&input, &mut out);
        let pcrs = pcr_pkts(&out, V);
        assert!(pcrs.len() >= 25);
        assert!(pcrs[3..].windows(2).all(|w| w[1].0 > w[0].0 && w[1].0 - w[0].0 <= 35 * MS));
        for (dts, pcr) in pes_vs_pcr(&out, V, A).into_iter().skip(2) {
            assert!(dts >= pcr + 80 * MS);
        }
        assert_eq!(s.offset_27mhz(), 80 * MS);
    }

    /// R2. The last frame of a media-player file waits in the pipeline for
    /// the next loop's first video, ~650 ms of stream later, and leaves
    /// 360 ms behind its decode time. The video replacer says it waited on
    /// a silent source (a hold): it is dropped as stale — `D` does not move,
    /// nothing is late on the wire, CC stays continuous. The same frame
    /// with no hold is the pipeline's and raises `D`.
    #[test]
    fn a_frame_held_by_a_silent_source_is_dropped_and_d_stays() {
        for held in [true, false] {
            let mut s = TsPcrRemux::new();
            let mut src = Src { t: 27_000_000, cc: 0 };
            let mut input = psi(V);
            for k in 0..20u64 {
                input.extend_from_slice(&src.pcr(src.t + k * 40 * MS));
                input.extend(src.frame(src.t + k * 40 * MS + 300 * MS));
            }
            let out = run(&mut s, &input);
            let before = out.chunks(TS_PACKET_SIZE).filter(|p| ts_pid(p) == V).count();
            assert_eq!(s.offset_27mhz(), 80 * MS);
            // 650 ms more of input clock with no video, then the held frame.
            let mut input = Vec::new();
            for k in 20..37u64 {
                input.extend_from_slice(&src.pcr(src.t + k * 40 * MS));
            }
            // The frame after the last one emitted (DTS t + 1060 ms), 340 ms
            // behind the input PCR (t + 1440 ms) when it leaves.
            let dts = src.t + 20 * 40 * MS + 300 * MS;
            input.extend(src.frame(dts));
            input.extend_from_slice(&src.pcr(src.t + 37 * 40 * MS));
            if held {
                s.note_source_holds(vec![(dts / 300, 610 * MS as i64)]);
            }
            let out = run(&mut s, &input);
            let st = s.stats.snapshot();
            if held {
                assert_eq!((st.stale_frames_dropped, st.offset_raises, st.late_frames), (1, 0, 0));
                assert_eq!(s.offset_27mhz(), 80 * MS);
                assert!(!out.chunks(TS_PACKET_SIZE).any(|p| ts_pid(p) == V && ts_pusi(p)));
                let ccs: Vec<u8> = out.chunks(TS_PACKET_SIZE).filter(|p| ts_pid(p) == V).map(ts_cc).collect();
                assert!(ccs.iter().all(|c| *c == ccs[0]), "AF-only PCRs keep the last payload CC");
                assert!(before > 0);
            } else {
                assert_eq!((st.stale_frames_dropped, st.offset_raises), (0, 1));
            }
        }
    }

    /// A3. A DI on a PCR the input's cadence predicts is not a new time
    /// base (the watcher's DI lands one PCR after the jump it saw; an MPTS
    /// media-player ingress used to flag ~91 % of its PCRs). No epoch, no
    /// re-latch, `D` stays; the DI byte passes. A DI on a jump — forward past
    /// the cadence, or backward — is an epoch.
    #[test]
    fn a_di_on_a_pcr_the_cadence_predicts_is_not_an_epoch() {
        let mut s = TsPcrRemux::new();
        let mut src = Src { t: 27_000_000, cc: 0 };
        let mut input = psi(V);
        for k in 0..100u64 {
            let mut p = src.pcr(src.t + k * 30 * MS);
            if k % 10 != 0 {
                set_discontinuity_indicator(&mut p);
            }
            input.extend_from_slice(&p);
            if k % 4 == 3 {
                input.extend(src.frame(src.t + k * 30 * MS + 300 * MS));
            }
        }
        let out = run(&mut s, &input);
        assert_eq!(s.stats.snapshot().epochs, 0);
        assert_eq!(s.offset_27mhz(), 80 * MS);
        assert_eq!(pcr_pkts(&out, V).iter().filter(|p| p.1).count(), 90, "the DI bytes pass");
        // A DI on a 2 s forward jump, then on a step back: two epochs.
        let mut input = Vec::new();
        for t in [src.t + 101 * 30 * MS + 2_000 * MS, src.t + 50 * 30 * MS] {
            let mut p = src.pcr(t);
            set_discontinuity_indicator(&mut p);
            input.extend_from_slice(&p);
        }
        run(&mut s, &input);
        assert_eq!(s.stats.snapshot().epochs, 2);
    }
}
