// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! "Configured but never engaged" watchdog for the TS transcode replacers.
//!
//! A `TsAudioReplacer` / `TsVideoReplacer` that cannot find its target ES —
//! no PAT, a PMT it cannot parse, no ES of its kind, or only codecs it cannot
//! decode (DTS, Opus, AC-4, VC-1, …) — falls back to passing the source ES
//! through. That fallback is deliberate, but it used to be invisible: the
//! only watchdog, the video decode-stall check, counts decoder input frames,
//! which stay at 0 when no PID is ever learned. `g12-vh1` ran 120 s with
//! `audio_encode` + `video_encode` configured, every transcode counter at 0
//! and not one warning.
//!
//! [`TranscodeEngageWatch`] is driven from the codec thread on every
//! `process()` call (no timers). It starts on the first call and again on
//! every source reset, and raises one Warning —
//! `audio_transcode_source_not_found` / `video_transcode_source_not_found` —
//! when, 5 s in, the replacer has still not locked and either
//!
//! - the PMT PID is known and has carried at least 10 PUSI packets, or
//! - at least 10 s and 1000 TS packets have passed (this covers a source
//!   with no PAT at all, and a PMT PID that never carries a PMT).
//!
//! A lock after the Warning raises the Info `*_transcode_source_found`.
//! Losing the lock (a PMT update removed the ES) re-arms the watch.

use std::time::{Duration, Instant};

use crate::manager::events::{category, EventSender, EventSeverity};

/// Earliest a not-found Warning may fire after the watch (re)starts.
pub const ENGAGE_WARN_AFTER: Duration = Duration::from_secs(5);
/// PMT-PID PUSI packets that must have been seen for the 5 s condition.
pub const ENGAGE_MIN_PMT_PUSI: u32 = 10;
/// The PMT-less condition: this long and this many TS packets.
pub const ENGAGE_NO_PMT_AFTER: Duration = Duration::from_secs(10);
pub const ENGAGE_NO_PMT_MIN_PACKETS: u64 = 1000;

/// Which replacer the watch serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscodeKind {
    Audio,
    Video,
}

impl TranscodeKind {
    fn noun(self) -> &'static str {
        match self {
            TranscodeKind::Audio => "audio",
            TranscodeKind::Video => "video",
        }
    }
    fn block(self) -> &'static str {
        match self {
            TranscodeKind::Audio => "audio_encode",
            TranscodeKind::Video => "video_encode",
        }
    }
}

/// Why the replacer has not engaged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotFoundReason {
    /// No PAT has been seen on the stream.
    NoPat,
    /// The PAT names a PMT PID but no PMT for the program could be parsed
    /// from it (`first_table_id` says what the PID does carry).
    PmtNotParsed,
    /// The PMT has no ES of this kind at all.
    NoSupportedEs,
    /// The PMT has ES of this kind, but none in a codec the replacer can
    /// decode (DTS, Opus, AC-4, SMPTE 302M, VC-1, JPEG XS, …).
    CodecNotReplaceable,
}

impl NotFoundReason {
    pub fn as_str(self) -> &'static str {
        match self {
            NotFoundReason::NoPat => "no_pat",
            NotFoundReason::PmtNotParsed => "pmt_not_parsed",
            NotFoundReason::NoSupportedEs => "no_supported_es",
            NotFoundReason::CodecNotReplaceable => "codec_not_replaceable",
        }
    }
    fn explain(self) -> &'static str {
        match self {
            NotFoundReason::NoPat => "no PAT has been received",
            NotFoundReason::PmtNotParsed => "no PMT for the program could be parsed",
            NotFoundReason::NoSupportedEs => "the PMT carries no stream of this kind",
            NotFoundReason::CodecNotReplaceable => {
                "the PMT only carries codecs the transcoder cannot decode"
            }
        }
    }
}

/// One event the watch wants emitted.
#[derive(Clone, Debug, PartialEq)]
pub enum EngageEvent {
    NotFound { reason: NotFoundReason, details: serde_json::Value },
    Found { details: serde_json::Value },
}

/// Per-replacer engage watchdog. See the module docs.
#[derive(Debug)]
pub struct TranscodeEngageWatch {
    kind: TranscodeKind,
    pinned_pid: Option<u16>,
    started: Option<Instant>,
    packets: u64,
    pat_seen: bool,
    pmt_pid: Option<u16>,
    program_number: Option<u16>,
    pmt_pusi: u32,
    first_table_id: Option<u8>,
    pmt_parsed: bool,
    /// Last parsed ES list: `(pid, stream_type)`.
    es: Vec<(u16, u8)>,
    /// An ES of this kind exists but its codec is not replaceable.
    unsupported_candidate: bool,
    locked: bool,
    /// The Warning has fired in the current window (one per window).
    warned: bool,
    /// A Warning is still unanswered by an Info. Unlike `warned` it
    /// survives [`Self::on_reset`], so a Warning raised for one source is
    /// closed by the first lock after an input switch too, rather than
    /// being left as the last word on the Events page.
    owes_found: bool,
}

impl TranscodeEngageWatch {
    pub fn new(kind: TranscodeKind, pinned_pid: Option<u16>) -> Self {
        Self {
            kind,
            pinned_pid,
            started: None,
            packets: 0,
            pat_seen: false,
            pmt_pid: None,
            program_number: None,
            pmt_pusi: 0,
            first_table_id: None,
            pmt_parsed: false,
            es: Vec::new(),
            unsupported_candidate: false,
            locked: false,
            warned: false,
            owes_found: false,
        }
    }

    /// Restart the watch (source reset / input switch). Everything learned
    /// about the previous source is forgotten and the clock restarts on the
    /// next [`Self::tick`] — except an unanswered Warning, which the next
    /// lock still answers with `*_transcode_source_found`.
    pub fn on_reset(&mut self) {
        let owes_found = self.owes_found;
        *self = Self::new(self.kind, self.pinned_pid);
        self.owes_found = owes_found;
    }

    /// Count TS packets handed to the replacer.
    pub fn note_packets(&mut self, n: u64) {
        self.packets = self.packets.saturating_add(n);
    }

    pub fn note_pat(&mut self, program_number: u16, pmt_pid: u16) {
        self.pat_seen = true;
        self.program_number = Some(program_number);
        self.pmt_pid = Some(pmt_pid);
    }

    /// A PUSI packet on the PMT PID.
    pub fn note_pmt_pusi(&mut self) {
        self.pmt_pusi = self.pmt_pusi.saturating_add(1);
    }

    /// A complete unit arrived on the PMT PID; `first_table_id` is the
    /// table of its first section.
    pub fn note_pmt_unit(&mut self, first_table_id: Option<u8>) {
        if self.first_table_id.is_none() {
            self.first_table_id = first_table_id;
        }
    }

    /// The program's PMT parsed. `unsupported_candidate`: an ES of this
    /// kind is present but not decodable.
    pub fn note_pmt_parsed(&mut self, es: Vec<(u16, u8)>, unsupported_candidate: bool) {
        self.pmt_parsed = true;
        self.es = es;
        self.unsupported_candidate = unsupported_candidate;
    }

    /// The replacer has (still) a decodable target. Returns the Info event
    /// when this lock follows a Warning.
    pub fn note_locked(&mut self, pid: u16, stream_type: u8) -> Option<EngageEvent> {
        if self.locked {
            return None;
        }
        self.locked = true;
        if !self.owes_found {
            return None;
        }
        self.warned = false;
        self.owes_found = false;
        Some(EngageEvent::Found {
            details: serde_json::json!({
                "error_code": format!("{}_transcode_source_found", self.kind.noun()),
                "source_pid": pid,
                "source_stream_type": stream_type,
                "pmt_pid": self.pmt_pid,
                "program_number": self.program_number,
            }),
        })
    }

    /// The target disappeared (PMT update); the watch re-arms from now.
    pub fn note_unlocked(&mut self, now: Instant) {
        if self.locked {
            self.locked = false;
            self.started = Some(now);
        }
    }

    fn reason(&self) -> NotFoundReason {
        if !self.pat_seen {
            NotFoundReason::NoPat
        } else if !self.pmt_parsed {
            NotFoundReason::PmtNotParsed
        } else if self.unsupported_candidate {
            NotFoundReason::CodecNotReplaceable
        } else {
            NotFoundReason::NoSupportedEs
        }
    }

    /// Advance the clock. Returns the one-shot Warning when due.
    pub fn tick(&mut self, now: Instant) -> Option<EngageEvent> {
        let started = *self.started.get_or_insert(now);
        if self.locked || self.warned {
            return None;
        }
        let waited = now.saturating_duration_since(started);
        if waited < ENGAGE_WARN_AFTER {
            return None;
        }
        let pmt_evidence = self.pmt_pid.is_some() && self.pmt_pusi >= ENGAGE_MIN_PMT_PUSI;
        let long_silence =
            waited >= ENGAGE_NO_PMT_AFTER && self.packets >= ENGAGE_NO_PMT_MIN_PACKETS;
        if !pmt_evidence && !long_silence {
            return None;
        }
        self.warned = true;
        self.owes_found = true;
        let reason = self.reason();
        let es: Vec<serde_json::Value> = self
            .es
            .iter()
            .map(|(pid, st)| serde_json::json!({ "pid": pid, "stream_type": st }))
            .collect();
        Some(EngageEvent::NotFound {
            reason,
            details: serde_json::json!({
                "error_code": format!("{}_transcode_source_not_found", self.kind.noun()),
                "reason": reason.as_str(),
                "pmt_pid": self.pmt_pid,
                "program_number": self.program_number,
                "first_table_id": self.first_table_id,
                "es": es,
                "pinned_pid": self.pinned_pid,
                "waited_ms": waited.as_millis() as u64,
            }),
        })
    }

    /// Emit `ev` on `sender`, scoped to the input (`input_scope`) or output
    /// `id` exactly like `video_transcode_decode_stalled`.
    pub fn emit(&self, ev: &EngageEvent, sender: &EventSender, id: &str, input_scope: bool) {
        let noun = if input_scope { "Input" } else { "Output" };
        let (severity, message, details) = match ev {
            EngageEvent::NotFound { reason, details } => (
                EventSeverity::Warning,
                format!(
                    "{noun} '{id}': {} is configured but the transcoder has found nothing to \
                     re-encode ({}; reason {}) — the source {} is passing through unchanged.",
                    self.kind.block(),
                    reason.explain(),
                    reason.as_str(),
                    self.kind.noun(),
                ),
                details.clone(),
            ),
            EngageEvent::Found { details } => (
                EventSeverity::Info,
                format!(
                    "{noun} '{id}': the {} transcoder found its source and is re-encoding.",
                    self.kind.noun()
                ),
                details.clone(),
            ),
        };
        match ev {
            EngageEvent::NotFound { .. } => tracing::warn!(id, "{message}"),
            EngageEvent::Found { .. } => tracing::info!(id, "{message}"),
        }
        if input_scope {
            sender.emit_input_with_details(severity, category::FLOW, message, id, details);
        } else {
            sender.emit_output_with_details(severity, category::FLOW, message, id, details);
        }
    }
}

/// One-shot `*_source_pid_not_found` Warning for an operator-pinned source
/// PID that is not in the PMT (the replacer fell back to the first
/// decodable ES). Emitted from the replacers' existing `(pinned, actual)`
/// dedupe branch, so it fires once per distinct pair.
pub fn emit_pinned_pid_absent(
    kind: TranscodeKind,
    sender: &EventSender,
    id: &str,
    input_scope: bool,
    pinned: u16,
    actual: u16,
    actual_stream_type: u8,
) {
    let noun = if input_scope { "Input" } else { "Output" };
    let message = format!(
        "{noun} '{id}': {}.source_{}_pid 0x{pinned:04X} is not in the PMT (or is not a \
         decodable {} stream) — transcoding the first decodable {} stream, PID 0x{actual:04X}, \
         instead.",
        kind.block(),
        kind.noun(),
        kind.noun(),
        kind.noun(),
    );
    let details = serde_json::json!({
        "error_code": format!("{}_source_pid_not_found", kind.noun()),
        "pinned_pid": pinned,
        "actual_pid": actual,
        "actual_stream_type": actual_stream_type,
    });
    if input_scope {
        sender.emit_input_with_details(EventSeverity::Warning, category::FLOW, message, id, details);
    } else {
        sender.emit_output_with_details(EventSeverity::Warning, category::FLOW, message, id, details);
    }
}

/// `audio_encode.source_audio_pid` on an output that picks its audio track
/// itself — RTMP, CMAF and WebRTC through `TsDemuxer::with_audio_pid` (inside
/// an [`AudioTrackWatch`]), HLS in its remux — rather than through a
/// `TsAudioReplacer`: the replacers' `(pinned, actual)` dedupe, as a value
/// the output keeps for its lifetime. Those outputs used to ignore the pin
/// and publish the track they picked.
#[derive(Debug)]
pub struct AudioPinWatch {
    pin: Option<u16>,
    /// The `(pinned, actual)` pair last warned about; cleared once the
    /// output is on the pin again, so a later drop-out warns again.
    warned: Option<(u16, u16)>,
}

impl AudioPinWatch {
    pub fn new(pin: Option<u16>) -> Self {
        Self { pin, warned: None }
    }

    /// The pinned PID, `None` when nothing is pinned.
    #[cfg_attr(not(feature = "media-codecs"), allow(dead_code))]
    pub fn pin(&self) -> Option<u16> {
        self.pin
    }

    /// The output is on audio `track` (`(pid, stream_type)`, `None` while it
    /// has none). Returns `(pinned, actual, actual_stream_type)` the first
    /// time it is off the pin on a given actual PID — the moment to raise
    /// [`emit_pinned_audio_pid_absent`]. Cheap enough to call per datagram.
    pub fn check(&mut self, track: Option<(u16, u8)>) -> Option<(u16, u16, u8)> {
        let (pin, (actual, stream_type)) = (self.pin?, track?);
        if pin == actual {
            self.warned = None;
            return None;
        }
        if self.warned == Some((pin, actual)) {
            return None;
        }
        self.warned = Some((pin, actual));
        Some((pin, actual, stream_type))
    }
}

/// The `audio_source_pid_not_found` Warning for an output that picks its
/// own audio track ([`AudioPinWatch`]): the code, category, scope and
/// details [`emit_pinned_pid_absent`] raises for a replacer, with a message
/// naming what the output fell back to — `fallback`, e.g. "the program's
/// first audio stream it can publish".
pub fn emit_pinned_audio_pid_absent(
    sender: &EventSender,
    output_id: &str,
    pinned: u16,
    actual: u16,
    actual_stream_type: u8,
    fallback: &str,
) {
    let message = format!(
        "Output '{output_id}': audio_encode.source_audio_pid 0x{pinned:04X} is not in the PMT \
         (or is not an audio stream this output can publish) — using {fallback}, PID \
         0x{actual:04X} (stream_type 0x{actual_stream_type:02X}), instead."
    );
    tracing::warn!(
        error_code = "audio_source_pid_not_found",
        pinned_pid = format!("0x{pinned:04X}"),
        actual_pid = format!("0x{actual:04X}"),
        actual_stream_type = format!("0x{actual_stream_type:02X}"),
        "{message}"
    );
    sender.emit_output_with_details(
        EventSeverity::Warning,
        category::FLOW,
        message,
        output_id,
        serde_json::json!({
            "error_code": "audio_source_pid_not_found",
            "pinned_pid": pinned,
            "actual_pid": actual,
            "actual_stream_type": actual_stream_type,
        }),
    );
}

/// Whether an output that publishes AAC — RTMP, CMAF — can publish audio
/// its demuxer routes as `stream_type` (`TsDemuxer::audio_track`): AAC in
/// ADTS (0x0F) as it is, and — through `audio_encode`, on a build with
/// `media-codecs` — what libavcodec decodes into the AAC encoder: MP2, AC-3,
/// E-AC-3, AAC-LATM (without the block `codec_needs_encode` says so). Never
/// Opus, whose frames reach no decoder on these outputs, nor AC-4, which
/// nothing here decodes.
pub fn aac_output_can_publish(stream_type: u8) -> bool {
    #[cfg(feature = "media-codecs")]
    if crate::engine::audio_decode::ff_codec_for_stream_type(stream_type)
        .is_some_and(|codec| codec != video_codec::AudioDecoderCodec::Opus)
    {
        return true;
    }
    stream_type == 0x0F
}

/// What an output that picks its audio track through
/// `TsDemuxer::with_audio_pid` — RTMP, CMAF, WebRTC — owes the operator about
/// that track: `audio_source_pid_not_found` for a pin it could not take
/// ([`AudioPinWatch`]), and `audio_codec_unpublishable` for a track in a
/// codec it cannot publish (`can_publish`, the predicate its demuxer was
/// built with), each once per episode. Kept for the output's lifetime, so a
/// reconnect or a new viewer repeats neither. Those outputs used to publish
/// such a track as no audio at all, without a word.
#[derive(Debug)]
pub struct AudioTrackWatch {
    pin: AudioPinWatch,
    can_publish: fn(u8) -> bool,
    /// The `(pid, stream_type)` last reported unpublishable. Cleared once
    /// the output is on audio it can publish, so each episode on an
    /// unpublishable track is reported once.
    unpublishable: Option<(u16, u8)>,
}

impl AudioTrackWatch {
    pub fn new(pin: Option<u16>, can_publish: fn(u8) -> bool) -> Self {
        Self { pin: AudioPinWatch::new(pin), can_publish, unpublishable: None }
    }

    /// The output is on audio `track` (`TsDemuxer::audio_track`, `None`
    /// while it has none). Raises `audio_source_pid_not_found` on `sender`
    /// the moment the output is off its pin, and returns `(pid, stream_type)`
    /// when an `audio_codec_unpublishable` is owed — the output raises that
    /// one itself ([`emit_audio_codec_unpublishable`]), saying what it can
    /// carry. Cheap enough to call after every `demux`.
    pub fn poll(
        &mut self,
        track: Option<(u16, u8)>,
        sender: &EventSender,
        output_id: &str,
    ) -> Option<(u16, u8)> {
        if let Some((pinned, actual, stream_type)) = self.pin.check(track) {
            // Off the pin, the demuxer is on the first track the output can
            // publish, or — when there is none — on the first of all.
            let fallback = if (self.can_publish)(stream_type) {
                "the program's first audio stream it can publish"
            } else {
                "the program's first audio stream (it can publish none of them)"
            };
            emit_pinned_audio_pid_absent(sender, output_id, pinned, actual, stream_type, fallback);
        }
        let (pid, stream_type) = track?;
        if (self.can_publish)(stream_type) {
            self.unpublishable = None;
            return None;
        }
        if self.unpublishable == Some((pid, stream_type)) {
            return None;
        }
        self.unpublishable = Some((pid, stream_type));
        Some((pid, stream_type))
    }
}

/// The `audio_codec_unpublishable` Warning [`AudioTrackWatch::poll`] owes:
/// the audio track output `output_id` is on, PID `pid`, is in a codec it can
/// neither carry nor re-encode, so the source's audio is not published. A
/// program whose first audio track is AC-4 used to go out that way without a
/// word. `kind` names the output ("RTMP"), `category` is its event category
/// and `carries` what it can publish. Output-scoped, with the flow in
/// `details`, like `codec_needs_encode`.
#[allow(clippy::too_many_arguments)]
pub fn emit_audio_codec_unpublishable(
    sender: &EventSender,
    category: &str,
    kind: &str,
    output_id: &str,
    flow_id: &str,
    pid: u16,
    stream_type: u8,
    carries: &str,
) {
    let codec = crate::engine::ts_demux::audio_codec_name(stream_type);
    let message = format!(
        "{kind} output '{output_id}': its audio track, PID 0x{pid:04X}, is {codec} (stream_type \
         0x{stream_type:02X}), which {kind} cannot carry or re-encode here ({carries}); the \
         source's audio is not published. If the program carries an audio track it can \
         publish, pin it with `audio_encode.source_audio_pid`"
    );
    tracing::warn!(error_code = "audio_codec_unpublishable", "{message}");
    sender.emit_output_with_details(
        EventSeverity::Warning,
        category,
        message,
        output_id,
        serde_json::json!({
            "error_code": "audio_codec_unpublishable",
            "audio_pid": pid,
            "stream_type": stream_type,
            "source_codec": codec,
            "flow_id": flow_id,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn not_found_reason(ev: Option<EngageEvent>) -> Option<&'static str> {
        match ev {
            Some(EngageEvent::NotFound { reason, .. }) => Some(reason.as_str()),
            _ => None,
        }
    }

    #[test]
    fn no_pat_needs_ten_seconds_and_traffic() {
        let start = t0();
        let mut w = TranscodeEngageWatch::new(TranscodeKind::Audio, None);
        assert!(w.tick(start).is_none());
        w.note_packets(5000);
        assert!(w.tick(start + Duration::from_secs(6)).is_none(), "no PMT evidence yet");
        assert_eq!(not_found_reason(w.tick(start + Duration::from_secs(10))), Some("no_pat"));
        assert!(w.tick(start + Duration::from_secs(20)).is_none(), "fires once");
    }

    #[test]
    fn unparsed_pmt_reports_its_first_table_id() {
        let start = t0();
        let mut w = TranscodeEngageWatch::new(TranscodeKind::Video, None);
        w.tick(start);
        w.note_pat(2010, 0x31);
        for _ in 0..10 {
            w.note_pmt_pusi();
            w.note_pmt_unit(Some(0xC0));
        }
        assert!(w.tick(start + Duration::from_millis(4999)).is_none());
        match w.tick(start + Duration::from_secs(5)) {
            Some(EngageEvent::NotFound { reason, details }) => {
                assert_eq!(reason, NotFoundReason::PmtNotParsed);
                assert_eq!(details["first_table_id"], 0xC0);
                assert_eq!(details["error_code"], "video_transcode_source_not_found");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unsupported_codec_warns_once_then_lock_reports_found() {
        let start = t0();
        let mut w = TranscodeEngageWatch::new(TranscodeKind::Audio, None);
        w.tick(start);
        w.note_pat(1, 0x100);
        let mut warnings = Vec::new();
        for i in 0..30 {
            w.note_pmt_pusi();
            w.note_pmt_parsed(vec![(0x101, 0x06)], true);
            if let Some(r) = not_found_reason(w.tick(start + Duration::from_millis(5000 + i * 100))) {
                warnings.push((i, r));
            }
        }
        // Fires on the 10th PMT PUSI, and only once.
        assert_eq!(warnings, vec![(9, "codec_not_replaceable")]);
        match w.note_locked(0x102, 0x0F) {
            Some(EngageEvent::Found { details }) => {
                assert_eq!(details["error_code"], "audio_transcode_source_found")
            }
            other => panic!("{other:?}"),
        }
        assert!(w.note_locked(0x102, 0x0F).is_none());
    }

    #[test]
    fn a_normal_source_raises_nothing() {
        let start = t0();
        let mut w = TranscodeEngageWatch::new(TranscodeKind::Audio, None);
        w.tick(start);
        w.note_pat(1, 0x100);
        w.note_pmt_pusi();
        w.note_pmt_parsed(vec![(0x101, 0x0F)], false);
        assert!(w.note_locked(0x101, 0x0F).is_none(), "no Info without a Warning");
        for s in 5..60 {
            w.note_pmt_pusi();
            assert!(w.tick(start + Duration::from_secs(s)).is_none());
        }
    }

    #[test]
    fn reset_restarts_the_clock() {
        let start = t0();
        let mut w = TranscodeEngageWatch::new(TranscodeKind::Audio, None);
        w.tick(start);
        w.note_pat(1, 0x100);
        for _ in 0..20 {
            w.note_pmt_pusi();
        }
        w.on_reset();
        let later = start + Duration::from_secs(6);
        assert!(w.tick(later).is_none(), "clock restarted at the reset");
        w.note_pat(1, 0x100);
        for _ in 0..20 {
            w.note_pmt_pusi();
        }
        assert!(w.tick(later + Duration::from_secs(4)).is_none());
        assert_eq!(not_found_reason(w.tick(later + Duration::from_secs(5))), Some("pmt_not_parsed"));
    }

    /// A Warning for one source followed by an input switch (a reset) and
    /// a lock on the new source must still be answered by the Info — the
    /// reset used to forget the Warning, leaving it the last word on the
    /// Events page although transcoding had resumed. A second Warning for
    /// a second failing source still fires, and one Info closes both.
    #[test]
    fn a_warning_is_answered_by_the_first_lock_after_a_reset() {
        let start = t0();
        let mut w = TranscodeEngageWatch::new(TranscodeKind::Audio, None);
        w.tick(start);
        w.note_pat(1, 0x100);
        for _ in 0..10 {
            w.note_pmt_pusi();
        }
        w.note_pmt_parsed(vec![(0x101, 0x82)], true);
        assert_eq!(
            not_found_reason(w.tick(start + Duration::from_secs(5))),
            Some("codec_not_replaceable")
        );
        w.on_reset();
        w.note_pat(1, 0x100);
        w.note_pmt_parsed(vec![(0x101, 0x0F)], false);
        match w.note_locked(0x101, 0x0F) {
            Some(EngageEvent::Found { details }) => {
                assert_eq!(details["error_code"], "audio_transcode_source_found")
            }
            other => panic!("{other:?}"),
        }
        assert!(w.note_locked(0x101, 0x0F).is_none(), "answered once");

        // Two failing sources in a row: two Warnings, one Info.
        let mut w = TranscodeEngageWatch::new(TranscodeKind::Audio, None);
        let t = start + Duration::from_secs(100);
        for k in 0..2u64 {
            let t = t + Duration::from_secs(20 * k);
            w.tick(t);
            w.note_pat(1, 0x100);
            for _ in 0..10 {
                w.note_pmt_pusi();
            }
            assert!(not_found_reason(w.tick(t + Duration::from_secs(5))).is_some(), "source {k}");
            w.on_reset();
        }
        assert!(matches!(w.note_locked(0x101, 0x0F), Some(EngageEvent::Found { .. })));
    }

    /// An output off its pinned audio PID is told once per distinct actual
    /// PID — the replacers' `(pinned, actual)` rule — however often it is
    /// polled, and again after it has been back on the pin.
    #[test]
    fn a_missed_audio_pin_is_reported_once_per_pair_and_rearmed_on_the_pin() {
        let mut w = AudioPinWatch::new(Some(0x305));
        assert_eq!(w.check(None), None, "no track yet: nothing to judge");
        assert_eq!(w.check(Some((0x301, 0x81))), Some((0x305, 0x301, 0x81)));
        assert_eq!(w.check(Some((0x301, 0x81))), None, "once per pair");
        assert_eq!(w.check(Some((0x302, 0x0F))), Some((0x305, 0x302, 0x0F)), "a new actual PID");
        assert_eq!(w.check(Some((0x305, 0x03))), None, "on the pin");
        assert_eq!(w.check(Some((0x302, 0x0F))), Some((0x305, 0x302, 0x0F)), "re-armed");

        let mut unpinned = AudioPinWatch::new(None);
        assert_eq!(unpinned.check(Some((0x301, 0x81))), None, "nothing pinned, nothing to say");
    }

    /// What RTMP and CMAF can publish: AAC as it is; MP2 / AC-3 / E-AC-3 /
    /// AAC-LATM through `audio_encode` where libavcodec is compiled in;
    /// never Opus or AC-4.
    #[test]
    fn aac_outputs_publish_aac_and_what_their_decoders_read_never_opus_or_ac4() {
        assert!(aac_output_can_publish(0x0F));
        for st in [0x03, 0x04, 0x80, 0x81, 0xC1, 0x87, 0xC2, 0x11] {
            assert_eq!(aac_output_can_publish(st), cfg!(feature = "media-codecs"), "stream_type {st:#04x}");
        }
        assert!(!aac_output_can_publish(0x06), "Opus");
        assert!(!aac_output_can_publish(crate::engine::ts_demux::SYNTHETIC_STREAM_TYPE_AC4));
    }

    /// The `error_code` of every event raised so far.
    fn raised(rx: &mut tokio::sync::mpsc::Receiver<crate::manager::events::Event>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|ev| ev.details.unwrap()["error_code"].as_str().unwrap().to_string())
            .collect()
    }

    /// An output on a track it cannot publish is owed one
    /// `audio_codec_unpublishable` per episode: not again while it stays
    /// there, however often polled, and again after it has been on audio it
    /// can publish. No track yet is nothing to judge.
    #[test]
    fn an_unpublishable_audio_track_is_owed_once_per_episode() {
        let (tx, mut rx) = crate::manager::events::event_channel();
        let ac4 = crate::engine::ts_demux::SYNTHETIC_STREAM_TYPE_AC4;
        let mut w = AudioTrackWatch::new(None, aac_output_can_publish);
        assert_eq!(w.poll(None, &tx, "o"), None);
        assert_eq!(w.poll(Some((0x300, ac4)), &tx, "o"), Some((0x300, ac4)));
        for _ in 0..3 {
            assert_eq!(w.poll(Some((0x300, ac4)), &tx, "o"), None, "once per episode");
        }
        assert_eq!(w.poll(Some((0x302, 0x06)), &tx, "o"), Some((0x302, 0x06)), "another track: Opus");
        assert_eq!(w.poll(Some((0x301, 0x0F)), &tx, "o"), None, "AAC publishes");
        assert_eq!(w.poll(Some((0x300, ac4)), &tx, "o"), Some((0x300, ac4)), "a new episode");
        assert!(raised(&mut rx).is_empty(), "the output raises that one itself");
    }

    /// A missed pin raises `audio_source_pid_not_found` from the watch, once
    /// per pair, its message naming what the demuxer fell back to: the first
    /// track the output can publish, or — when the program has none — the
    /// first of all, which is also owed `audio_codec_unpublishable`.
    #[test]
    fn a_missed_pin_names_the_fallback_the_demuxer_took() {
        let (tx, mut rx) = crate::manager::events::event_channel();
        let ac4 = crate::engine::ts_demux::SYNTHETIC_STREAM_TYPE_AC4;
        let mut w = AudioTrackWatch::new(Some(0x305), aac_output_can_publish);
        assert_eq!(w.poll(Some((0x301, 0x0F)), &tx, "o"), None);
        assert_eq!(w.poll(Some((0x301, 0x0F)), &tx, "o"), None);
        let ev = rx.try_recv().expect("one Warning");
        assert!(rx.try_recv().is_err(), "once per pair");
        assert!(ev.message.contains("first audio stream it can publish, PID 0x0301"), "{}", ev.message);
        assert_eq!(ev.details.unwrap()["error_code"], "audio_source_pid_not_found");

        assert_eq!(w.poll(Some((0x300, ac4)), &tx, "o"), Some((0x300, ac4)));
        let ev = rx.try_recv().expect("a new pair");
        assert!(ev.message.contains("it can publish none of them"), "{}", ev.message);

        assert_eq!(w.poll(Some((0x305, 0x0F)), &tx, "o"), None, "on the pin");
        assert!(raised(&mut rx).is_empty());
    }

    /// `audio_codec_unpublishable` names the output, PID and codec, scoped to
    /// the output with the flow in `details`.
    #[test]
    fn audio_codec_unpublishable_names_the_pid_and_codec() {
        let (tx, mut rx) = crate::manager::events::event_channel();
        emit_audio_codec_unpublishable(&tx, category::CMAF, "CMAF", "c1", "f", 0x300, 0xAC, "AAC only");
        let ev = rx.try_recv().expect("one Warning");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::CMAF);
        assert_eq!(ev.output_id.as_deref(), Some("c1"));
        assert!(
            ev.message.starts_with("CMAF output 'c1': its audio track, PID 0x0300, is AC-4 (stream_type 0xAC)"),
            "{}",
            ev.message
        );
        assert_eq!(
            ev.details.unwrap(),
            serde_json::json!({
                "error_code": "audio_codec_unpublishable",
                "audio_pid": 0x300,
                "stream_type": 0xAC,
                "source_codec": "AC-4",
                "flow_id": "f",
            })
        );
    }

    /// The output's Warning carries the replacer's code, category, severity,
    /// scope and details.
    #[test]
    fn a_missed_audio_pin_on_an_output_is_the_replacers_warning() {
        let (tx, mut rx) = crate::manager::events::event_channel();
        emit_pinned_audio_pid_absent(&tx, "rtmp-1", 0x305, 0x300, 0xAC, "the program's first audio stream");
        let ev = rx.try_recv().expect("one Warning");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::FLOW);
        assert_eq!(ev.output_id.as_deref(), Some("rtmp-1"));
        assert!(ev.message.contains("0x0305") && ev.message.contains("PID 0x0300"), "{}", ev.message);
        assert_eq!(
            ev.details.unwrap(),
            serde_json::json!({
                "error_code": "audio_source_pid_not_found",
                "pinned_pid": 0x305,
                "actual_pid": 0x300,
                "actual_stream_type": 0xAC,
            })
        );
    }
}

/// How long [`PrePmtGate`] holds a replacer's output to PSI before it gives
/// up and falls back to passthrough.
pub const PRE_PMT_GATE: Duration = Duration::from_secs(5);

/// The replacers' pre-PMT gate (defect 4). Until the program's PMT has been
/// parsed a replacer forwards only PSI / SI (PIDs 0x00–0x1F, and the PMT PID
/// it routes before asking) and null packets: source ES ahead of the PMT
/// would reach the wire untranscoded — raw timestamps, no SPS / PPS, source
/// PCRs — and the replacer's first packet on the PID would then jump its CC.
/// Nothing ahead of the PMT is decodable anyway. A PMT that never parses
/// opens the gate after [`PRE_PMT_GATE`] (today's passthrough — the engage
/// watchdog's `*_transcode_source_not_found` says why). Re-armed when the
/// PAT moves the program to a new PMT PID.
#[derive(Debug, Default)]
pub struct PrePmtGate {
    pmt_parsed: bool,
    started: Option<Instant>,
    fallback: bool,
}

impl PrePmtGate {
    /// The program's PMT parsed: the gate opens.
    pub fn open(&mut self) {
        self.pmt_parsed = true;
    }

    /// A new program (PMT PID change): closed until its PMT parses.
    pub fn rearm(&mut self, now: Instant) {
        *self = Self { started: Some(now), ..Self::default() };
    }

    /// Whether `pid` must be dropped now. The timer arms on the first packet
    /// of any kind.
    pub fn drops(&mut self, pid: u16, now: Instant, what: &str) -> bool {
        if self.pmt_parsed || self.fallback {
            return false;
        }
        let started = *self.started.get_or_insert(now);
        if pid <= 0x1F || pid == 0x1FFF {
            return false;
        }
        if now.saturating_duration_since(started) >= PRE_PMT_GATE {
            self.fallback = true;
            tracing::warn!(
                "{what}: no PMT for the program within {} s — passing the stream through \
                 untranscoded until one parses",
                PRE_PMT_GATE.as_secs()
            );
            return false;
        }
        true
    }
}

/// Last passthrough continuity counter per PID, so a replacer that takes a
/// PID over continues its CC sequence. One 8 KiB table per replacer,
/// allocated once (`0xFF` = nothing passed through since the replacer last
/// took the PID over).
pub struct PassthroughCc(Box<[u8; 8192]>);

impl Default for PassthroughCc {
    fn default() -> Self {
        Self(Box::new([0xFF; 8192]))
    }
}

impl PassthroughCc {
    /// Record the CC of a packet that went out untouched.
    pub fn note(&mut self, pid: u16, pkt: &[u8]) {
        self.0[(pid & 0x1FFF) as usize] = pkt[3] & 0x0F;
    }

    /// The replacer takes `pid` over: the CC its first payload packet
    /// should carry, when packets passed through on the PID since it last
    /// held it. Consumes the entry — from here the replacer's own counter
    /// is the last CC on the wire, and a later takeover (after a program
    /// re-layout the gate held back) must not rewind to this one.
    pub fn take_next_after(&mut self, pid: u16) -> Option<u8> {
        match std::mem::replace(&mut self.0[(pid & 0x1FFF) as usize], 0xFF) {
            0xFF => None,
            cc => Some((cc + 1) & 0x0F),
        }
    }
}
