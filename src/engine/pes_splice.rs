// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! PES Switch Phase 4 — audio-aligned splice state machine.
//!
//! `SwitchActiveInput` with `splice_mode = PesAligned` holds the outbound
//! bytes of a switch slot at the from-leg's last fully-emitted PES boundary,
//! then concatenates the to-leg's next PES with monotonically-greater PTS.
//! On budget exhaustion the path falls back to today's `PmtBump` behaviour
//! and emits `pes_splice_timeout`. Both paths emit `pes_splice_completed`
//! on success so operators can correlate switch events to receiver-side
//! behaviour.
//!
//! This first land is **audio-only** — video splice (PES Switch Phase 4
//! follow-up) needs an IDR-aware boundary detector + codec-param sentinel
//! that's out of scope here. Non-audio slots fall through to `PmtBump`
//! silently so the assembler stays uniform.
//!
//! The state machine is **pure** — it doesn't own a clock or a channel;
//! the caller (`ts_assembler`) drives transitions via `now` / per-packet
//! `observe_b_packet`. Keeps the hot path free of any sleeps.

use std::time::{Duration, Instant};

use crate::engine::ts_parse::{extract_pes_pts, ts_pusi};

/// Default splice budget for audio in milliseconds. ≥8 audio frames at
/// every common codec rate (AAC-LC 21.3 ms, MP2 24 ms, AC-3 32 ms), so
/// the from-leg has time to flush its last buffered AU and the to-leg
/// has time to align its first AU's PTS.
pub const DEFAULT_AUDIO_SPLICE_BUDGET_MS: u32 = 200;

/// Inclusive range accepted by the validator for an operator-supplied
/// `splice_budget_ms`.
pub const SPLICE_BUDGET_MS_RANGE: std::ops::RangeInclusive<u32> = 20..=5000;

/// Nominal audio frame duration in 90 kHz PTS ticks for one MPEG-TS
/// `stream_type`. Returns `None` when the stream type isn't a known
/// audio codec — caller falls back to `PmtBump` for that slot.
///
/// Values assume 48 kHz sample rate (the broadcast norm); the splice
/// only requires "PTS strictly greater than last_a_pts by ~one frame"
/// to be receiver-safe, so a small over-estimate is harmless — it just
/// makes the splice wait one extra frame in the worst case.
pub fn audio_frame_duration_90k(stream_type: u8) -> Option<u64> {
    match stream_type {
        // MPEG-1 / MPEG-2 audio (MP1/MP2): 1152 samples / 48 000 Hz × 90 000 = 2160.
        0x03 | 0x04 => Some(2160),
        // AAC ADTS / LATM: 1024 samples / 48 000 Hz × 90 000 = 1920.
        0x0F | 0x11 => Some(1920),
        // AC-3, E-AC-3, DTS, DTS-HD, Atmos: 1536 samples / 48 000 Hz × 90 000 = 2880.
        0x81 | 0x83 | 0x84 | 0x85 | 0x87 => Some(2880),
        _ => None,
    }
}

/// `true` iff the stream_type is one of the audio codecs the splice
/// state machine knows how to align. Anything else returns `false` and
/// the assembler falls through to `PmtBump`.
pub fn is_supported_audio_stream_type(stream_type: u8) -> bool {
    audio_frame_duration_90k(stream_type).is_some()
}

/// State of a single switch slot's PES-aligned audio splice.
///
/// Driven by the assembler's main loop:
/// - At `SwitchActiveInput { splice_mode: PesAligned }` time the
///   assembler calls [`AudioSpliceState::arm`] with the from-leg's
///   most recent PTS and the to-leg's input id.
/// - On every fan-in packet from the **from-leg** the assembler calls
///   [`AudioSpliceState::observe_a_packet`]. Returns
///   [`FromPacketAction::Forward`] until A's next PUSI=1 (= A's
///   current AU completed). After that the state machine flips
///   internally and subsequent calls return [`FromPacketAction::Drop`]
///   so the assembler stops emitting A bytes mid-AU.
/// - On every fan-in packet from the **to-leg** the assembler calls
///   [`AudioSpliceState::observe_b_packet`]. Returns
///   [`SpliceOutcome::Committed`] when B's first PUSI=1 PES with
///   `pts ≥ threshold_pts` arrives → assembler flips the active leg,
///   bumps PMT, arms DI=1, emits the `pes_splice_completed` event,
///   and the state resets to Idle.
/// - On every wakeup (e.g. the 20 ms flush tick) the assembler calls
///   [`AudioSpliceState::check_timeout`]. On `Some(SpliceOutcome::Timeout)`
///   the assembler runs the legacy PmtBump path and emits
///   `pes_splice_timeout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSpliceState {
    /// No splice in flight; the slot follows today's behaviour.
    Idle,
    /// Splice armed — outbound is held until either:
    /// (a) the to-leg produces a PUSI=1 PES with `pts ≥ threshold_pts`, or
    /// (b) `Instant::now() ≥ deadline` (caller falls back to PmtBump).
    Pending {
        /// Input id of the to-leg the operator is switching to.
        to_input_id: String,
        /// Last PTS observed on the from-leg before `arm` was called.
        /// The first acceptable B PES must carry `pts > last_a_pts`
        /// (strictly greater — equal would alias the previous frame).
        last_a_pts: u64,
        /// `last_a_pts + audio_frame_duration_90k(stream_type)`. The
        /// first PES we accept must have `pts ≥ threshold_pts`.
        threshold_pts: u64,
        /// Wallclock budget — `Instant::now() + splice_budget`.
        deadline: Instant,
        /// `true` once we've observed A's *next* PUSI=1 after arming —
        /// that's the marker that A's current AU has finished emitting
        /// and we should stop forwarding A's bytes (otherwise we'd
        /// emit a fragment of A's next AU and create a decoder click
        /// when the receiver tries to decode an incomplete frame).
        /// Initially `false`; the first PUSI=1 packet from A flips it
        /// to `true`. From then on A's packets are dropped at the
        /// assembler edge.
        a_au_completed: bool,
    },
}

/// Per-packet directive returned by [`AudioSpliceState::observe_a_packet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FromPacketAction {
    /// Forward this A-leg packet to the output — A's current AU is
    /// still mid-emission.
    Forward,
    /// Drop this A-leg packet — A's current AU has completed; the
    /// next AU belongs to a stream we're not going to emit.
    Drop,
}

impl AudioSpliceState {
    /// Construct an Idle state.
    pub fn new() -> Self {
        AudioSpliceState::Idle
    }

    /// Arm the splice. Returns `false` and stays `Idle` when the
    /// stream type isn't a supported audio codec (caller falls back
    /// to PmtBump silently — non-audio splice is a separate Phase 4
    /// follow-up).
    ///
    /// `last_a_pts` is the most recent PTS observed on the from-leg's
    /// PUSI=1 audio packets. `now` is the caller's `Instant::now()`
    /// snapshot so the state machine stays time-source-pluggable for
    /// tests.
    pub fn arm(
        &mut self,
        to_input_id: String,
        stream_type: u8,
        last_a_pts: u64,
        now: Instant,
        budget: Duration,
    ) -> bool {
        let Some(frame_dur) = audio_frame_duration_90k(stream_type) else {
            return false;
        };
        let threshold_pts = last_a_pts.wrapping_add(frame_dur) & 0x1_FFFF_FFFF; // 33-bit wrap
        *self = AudioSpliceState::Pending {
            to_input_id,
            last_a_pts,
            threshold_pts,
            deadline: now + budget,
            a_au_completed: false,
        };
        true
    }

    /// Decide whether to forward or drop a packet from the **from-leg**
    /// during a pending splice. Returns [`FromPacketAction::Forward`]
    /// until A's first PUSI=1 after `arm` flips `a_au_completed`;
    /// thereafter returns [`FromPacketAction::Drop`].
    ///
    /// Outside of `Pending` always returns `Forward` so the assembler
    /// can call this unconditionally on every packet from the active
    /// leg without paying a state-check fast-path cost.
    pub fn observe_a_packet(&mut self, pkt: &[u8]) -> FromPacketAction {
        let AudioSpliceState::Pending { a_au_completed, .. } = self else {
            return FromPacketAction::Forward;
        };
        if *a_au_completed {
            return FromPacketAction::Drop;
        }
        if ts_pusi(pkt) {
            // A's next AU is starting — the AU whose PTS we captured
            // at arm-time is finished emitting. Drop this packet (it's
            // already the next AU we'd be torn off) and stop
            // forwarding A entirely.
            *a_au_completed = true;
            return FromPacketAction::Drop;
        }
        FromPacketAction::Forward
    }

    /// Process one fan-in packet from the **to-leg**. Returns
    /// `Some(SpliceOutcome::Committed)` when the packet is a PUSI=1
    /// PES with PTS ≥ threshold — that's the commit signal. Returns
    /// `None` when the packet is mid-PES (no PUSI) or carries a PTS
    /// below threshold (still waiting for the next AU).
    ///
    /// Caller must guarantee `packet_input_id` is the to-leg before
    /// calling; from-leg packets are dropped at the main loop edge.
    pub fn observe_b_packet(&mut self, pkt: &[u8]) -> Option<SpliceOutcome> {
        let AudioSpliceState::Pending {
            threshold_pts, ..
        } = self
        else {
            return None;
        };
        if !ts_pusi(pkt) {
            return None;
        }
        let pts = extract_pes_pts(pkt)?;
        // 33-bit PTS comparison with wrap tolerance: anything within
        // 2^31 ticks (≈ 6.6 h) ahead of `threshold_pts` counts as
        // "≥ threshold". A "behind" PTS is the wrap-back case — treat
        // it as still waiting.
        let threshold = *threshold_pts;
        let ahead = pts.wrapping_sub(threshold) & 0x1_FFFF_FFFF;
        if ahead <= 1 << 31 {
            // PTS is at or past the threshold → commit.
            let outcome = SpliceOutcome::Committed { first_b_pts: pts };
            *self = AudioSpliceState::Idle;
            Some(outcome)
        } else {
            None
        }
    }

    /// Check whether the splice budget has expired. Returns
    /// `Some(SpliceOutcome::Timeout { to_input_id })` exactly once on
    /// the first call past the deadline; transitions back to Idle
    /// afterwards. The caller flips `active_leg_input` to `to_input_id`
    /// (the legacy PmtBump fallback path).
    pub fn check_timeout(&mut self, now: Instant) -> Option<SpliceOutcome> {
        let AudioSpliceState::Pending {
            deadline,
            to_input_id,
            ..
        } = self
        else {
            return None;
        };
        if now >= *deadline {
            let to = std::mem::take(to_input_id);
            *self = AudioSpliceState::Idle;
            Some(SpliceOutcome::Timeout { to_input_id: to })
        } else {
            None
        }
    }

    /// `true` iff the splice is currently armed.
    pub fn is_pending(&self) -> bool {
        matches!(self, AudioSpliceState::Pending { .. })
    }

    /// Returns the to-leg's input_id when armed.
    pub fn pending_to_input_id(&self) -> Option<&str> {
        if let AudioSpliceState::Pending { to_input_id, .. } = self {
            Some(to_input_id)
        } else {
            None
        }
    }
}

impl Default for AudioSpliceState {
    fn default() -> Self {
        AudioSpliceState::Idle
    }
}

/// Terminal outcome of a splice. Caller drives event emission off this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpliceOutcome {
    /// To-leg produced an aligned PES — emit `pes_splice_completed`
    /// with `first_b_pts` for receiver-side correlation.
    Committed {
        first_b_pts: u64,
    },
    /// Budget exhausted — caller falls back to PmtBump + emits
    /// `pes_splice_timeout`. `to_input_id` is the leg the operator
    /// asked to switch to; the caller flips active_leg_input to it as
    /// part of the fallback path.
    Timeout {
        to_input_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt_with_pts(pts: u64, pusi: bool) -> [u8; 188] {
        let mut p = [0u8; 188];
        p[0] = 0x47; // sync
        // byte 1: PUSI bit (0x40) + PID hi 5 bits
        p[1] = if pusi { 0x40 } else { 0x00 };
        p[2] = 0x10; // PID lo = 0x10
        p[3] = 0x10; // afc=0b01 payload only, CC=0
        // Payload at byte 4: PES start code + stream_id + length + flags + PTS
        p[4] = 0x00;
        p[5] = 0x00;
        p[6] = 0x01;
        p[7] = 0xC0; // audio stream_id
        p[8] = 0; // PES_packet_length hi
        p[9] = 0; // PES_packet_length lo
        p[10] = 0x80; // flags1 — copyright/orig irrelevant
        p[11] = 0x80; // flags2 = 0b10000000 → PTS only
        p[12] = 5; // PES_header_data_length (5 bytes of PTS)
        // PTS bytes 13..18 (PES bytes 9..14)
        let m: u64 = pts & 0x1_FFFF_FFFF; // 33 bits
        p[13] = 0x20 | (((m >> 30) as u8) & 0x07) << 1 | 1;
        p[14] = ((m >> 22) as u8) & 0xFF;
        p[15] = ((((m >> 15) as u8) & 0x7F) << 1) | 1;
        p[16] = ((m >> 7) as u8) & 0xFF;
        p[17] = (((m as u8) & 0x7F) << 1) | 1;
        p
    }

    #[test]
    fn frame_durations_known_codecs() {
        assert_eq!(audio_frame_duration_90k(0x0F), Some(1920)); // AAC-LC ADTS
        assert_eq!(audio_frame_duration_90k(0x11), Some(1920)); // AAC LATM
        assert_eq!(audio_frame_duration_90k(0x04), Some(2160)); // MP2
        assert_eq!(audio_frame_duration_90k(0x03), Some(2160)); // MP1
        assert_eq!(audio_frame_duration_90k(0x81), Some(2880)); // AC-3
        assert_eq!(audio_frame_duration_90k(0x87), Some(2880)); // E-AC-3
        // Video must return None — non-audio falls through to PmtBump.
        assert_eq!(audio_frame_duration_90k(0x1B), None); // H.264
        assert_eq!(audio_frame_duration_90k(0x24), None); // HEVC
        assert_eq!(audio_frame_duration_90k(0x02), None); // MPEG-2 video
    }

    #[test]
    fn arm_rejects_non_audio_stream_type() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        let armed = s.arm(
            "to".into(),
            0x1B, // H.264 — not supported here
            1_000_000,
            now,
            Duration::from_millis(200),
        );
        assert!(!armed);
        assert_eq!(s, AudioSpliceState::Idle);
    }

    #[test]
    fn commit_on_first_b_pes_at_threshold() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        // AAC-LC: frame = 1920 ticks. last_a = 90_000, threshold = 91_920.
        assert!(s.arm("to".into(), 0x0F, 90_000, now, Duration::from_millis(200)));
        // B emits at exactly the threshold → commit.
        let pkt = pkt_with_pts(91_920, /* pusi */ true);
        match s.observe_b_packet(&pkt) {
            Some(SpliceOutcome::Committed { first_b_pts }) => {
                assert_eq!(first_b_pts, 91_920);
            }
            other => panic!("expected Committed, got {other:?}"),
        }
        assert_eq!(s, AudioSpliceState::Idle);
    }

    #[test]
    fn commit_on_first_b_pes_past_threshold() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm("to".into(), 0x0F, 90_000, now, Duration::from_millis(200)));
        // 200 ticks past threshold.
        let pkt = pkt_with_pts(92_120, true);
        assert!(matches!(
            s.observe_b_packet(&pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 92_120 })
        ));
    }

    #[test]
    fn skip_pes_below_threshold() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm("to".into(), 0x0F, 90_000, now, Duration::from_millis(200)));
        // B's first PES is exactly the same as last A — alias, must wait.
        let pkt = pkt_with_pts(90_000, true);
        assert!(s.observe_b_packet(&pkt).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn skip_non_pusi() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm("to".into(), 0x0F, 90_000, now, Duration::from_millis(200)));
        let pkt = pkt_with_pts(99_999, /* pusi */ false);
        assert!(s.observe_b_packet(&pkt).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn timeout_emits_once() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to-leg".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(10),
        ));
        let later = now + Duration::from_millis(11);
        match s.check_timeout(later) {
            Some(SpliceOutcome::Timeout { to_input_id }) => {
                assert_eq!(to_input_id, "to-leg");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
        // Subsequent calls are no-ops.
        assert!(s.check_timeout(later + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn no_timeout_before_deadline() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm("to".into(), 0x0F, 90_000, now, Duration::from_millis(200)));
        assert!(s.check_timeout(now + Duration::from_millis(50)).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn pts_wrap_around_threshold_commits() {
        // last_a near the top of the 33-bit space; threshold wraps to
        // near zero. A B PES with PTS slightly past the wrap counts as
        // "ahead" and must commit.
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        let near_top: u64 = (1u64 << 33) - 1000;
        // AAC frame 1920 → threshold wraps past 2^33 to (1920 - 1000) = 920.
        assert!(s.arm("to".into(), 0x0F, near_top, now, Duration::from_millis(200)));
        let pkt = pkt_with_pts(2000, true); // 2000 > 920, wrapped-ahead
        assert!(matches!(
            s.observe_b_packet(&pkt),
            Some(SpliceOutcome::Committed { .. })
        ));
    }

    #[test]
    fn observe_does_nothing_when_idle() {
        let mut s = AudioSpliceState::new();
        let pkt = pkt_with_pts(1_000_000, true);
        assert!(s.observe_b_packet(&pkt).is_none());
        assert!(s.check_timeout(Instant::now()).is_none());
        // observe_a_packet always returns Forward when Idle so the
        // assembler can call it unconditionally on every fan-in
        // packet from the active leg.
        assert_eq!(s.observe_a_packet(&pkt), FromPacketAction::Forward);
    }

    #[test]
    fn from_leg_forwarded_then_dropped_at_next_pusi() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm("to".into(), 0x0F, 90_000, now, Duration::from_millis(200)));
        // First, a non-PUSI continuation packet from A: forward.
        let cont = pkt_with_pts(0, false);
        assert_eq!(s.observe_a_packet(&cont), FromPacketAction::Forward);
        // Now A's next PUSI=1 — that marks A's current AU's end. The
        // PUSI packet itself is dropped (it's already the next AU).
        let pusi = pkt_with_pts(91_920, true);
        assert_eq!(s.observe_a_packet(&pusi), FromPacketAction::Drop);
        // Subsequent A packets are also dropped.
        assert_eq!(s.observe_a_packet(&cont), FromPacketAction::Drop);
        assert_eq!(s.observe_a_packet(&pusi), FromPacketAction::Drop);
        // State still Pending until B commits or timeout.
        assert!(s.is_pending());
    }

    #[test]
    fn commit_resets_to_idle_so_subsequent_a_forwards() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm("to".into(), 0x0F, 90_000, now, Duration::from_millis(200)));
        let pusi_b = pkt_with_pts(91_920, true);
        assert!(matches!(
            s.observe_b_packet(&pusi_b),
            Some(SpliceOutcome::Committed { .. })
        ));
        // After commit the slot's "active leg" is the to-leg; the
        // from-leg is no longer the active leg so the assembler stops
        // calling observe_a_packet for it. observe_a_packet returning
        // Forward when Idle is the safe default for the *new* active
        // leg (B), which now becomes the "A" of any future splice.
        let new_a_pkt = pkt_with_pts(95_000, true);
        assert_eq!(s.observe_a_packet(&new_a_pkt), FromPacketAction::Forward);
    }
}
