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
    warned: bool,
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
        }
    }

    /// Restart the watch (source reset / input switch). Everything learned
    /// about the previous source is forgotten and the clock restarts on the
    /// next [`Self::tick`].
    pub fn on_reset(&mut self) {
        *self = Self::new(self.kind, self.pinned_pid);
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
        if !self.warned {
            return None;
        }
        self.warned = false;
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
}
