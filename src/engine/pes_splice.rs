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
//! follow-up) needs an IDR-aware boundary detector + SPS/PPS/VPS
//! sentinel that's out of scope here. Non-audio slots fall through to
//! `PmtBump` silently so the assembler stays uniform.
//!
//! AAC AudioSpecificConfig sentinel (edge 0.65.0): the splice arm path
//! optionally snapshots A's most recent AAC params (profile, sample
//! rate index, channel configuration) from the active leg's ADTS sync
//! header; on B's first PUSI=1 PES at PTS ≥ threshold the same params
//! are parsed and compared. Mismatch → refuse the splice with
//! `CodecParamMismatch`; the assembler falls back to PmtBump and emits
//! `pes_splice_codec_param_mismatch`. AAC-LATM and non-AAC audio
//! commit on PTS alone (the sentinel falls through). See
//! [`AacAudioParams`] + [`parse_aac_adts_params`].
//!
//! Video splice MVP (edge 0.66.0): [`VideoSpliceState`] is the sibling
//! state machine for H.264 (stream_type `0x1B`) and HEVC (`0x24`) slots.
//! The arm path follows the same shape as audio. The boundary detector
//! is IDR-aware: B's first PUSI=1 PES at PTS ≥ threshold must additionally
//! carry an IDR NAL — H.264 `nal_unit_type == 5`, HEVC `nal_unit_type` in
//! 16..=21 (IRAP family: BLA / IDR / CRA). Without an IDR the receiver
//! cannot decode the post-splice bitstream and would freeze on the next
//! anchor frame, so a non-IDR PES from B is held the same as a PES below
//! the PTS threshold. SPS/PPS/VPS codec-param sentinel is a Session B
//! follow-up; today's behaviour: commit on the first IDR PES past
//! threshold regardless of parameter set, fail-safe on missing data.
//!
//! The state machine is **pure** — it doesn't own a clock or a channel;
//! the caller (`ts_assembler`) drives transitions via `now` / per-packet
//! `observe_b_packet`. Keeps the hot path free of any sleeps.

use std::time::{Duration, Instant};

use crate::engine::content_analysis::bitreader::{unescape_rbsp, BitReader};
use crate::engine::ts_parse::{extract_pes_pts, pes_payload_offset, ts_pusi};

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

/// AAC AudioSpecificConfig snapshot parsed from an ADTS sync header.
///
/// ADTS carries the AudioSpecificConfig fields inline at the start of
/// every frame, so the codec-param sentinel can sample it in O(1) on the
/// splice arm path + B's first PUSI=1 PES — no PMT-descriptor lookup is
/// needed. AAC-LATM (stream_type `0x11`) embeds its
/// `AudioSpecificConfig` inside StreamMuxConfig and is not parsed here;
/// the sentinel returns `None` for LATM payloads which causes the
/// splice to commit without the additional check (same as today's
/// audio MVP).
///
/// All three fields are the raw on-the-wire codes from MPEG-4 ADTS:
/// - `profile`: `MPEG-4 AOT − 1` (0=Main, 1=LC, 2=SSR, 3=LTP). Today's
///   broadcast AAC is almost always LC (=1).
/// - `sample_rate_idx`: MPEG-4 sampling_frequency_index (0..15). 0=96k,
///   3=48k, 4=44.1k, ... 15 means "next 24 bits carry the explicit
///   frequency", never used in ADTS in practice.
/// - `channel_config`: MPEG-4 channel configuration (0..7). 0=defined
///   by the program, 1=mono, 2=stereo, 6=5.1.
///
/// Comparing all three covers every receiver-visible AAC parameter
/// change that produces an audible click on a mid-PES splice: profile
/// switches the decoder mode, sample_rate forces a resampler restart,
/// and channel_config changes the multi-channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacAudioParams {
    pub profile: u8,
    pub sample_rate_idx: u8,
    pub channel_config: u8,
}

impl AacAudioParams {
    /// Human label for the sample rate (`"48000"`, `"44100"`, ...).
    /// Falls back to the raw index in `Reserved-N` form for never-seen
    /// codes (index 13/14 are reserved in the ADTS spec).
    pub fn sample_rate_hz(&self) -> u32 {
        match self.sample_rate_idx {
            0 => 96000,
            1 => 88200,
            2 => 64000,
            3 => 48000,
            4 => 44100,
            5 => 32000,
            6 => 24000,
            7 => 22050,
            8 => 16000,
            9 => 12000,
            10 => 11025,
            11 => 8000,
            12 => 7350,
            _ => 0,
        }
    }
}

/// Parse the AAC `AudioSpecificConfig` fields out of an ADTS sync header
/// at the start of a PES payload. Returns `None` when the payload is
/// shorter than the 7-byte ADTS header or the sync word doesn't match
/// (e.g. AAC-LATM, mid-PES bytes, malformed frame).
///
/// The 7-byte ADTS header layout (reference: ISO/IEC 13818-7 §6):
/// ```text
/// byte 0       1                 2                 3
/// FFFFFFFF  | FFFF VLLP        | PPSS SSCC      | CCFA HHHH ...
///             ^^^^             | ^^   ^         | ^^
///             sync (12 bits)   | profile (2b)   |
///                              | sample_rate (4b)
///                              |                | channel_config (3b, split)
/// ```
///
/// Pure-bitwise, O(1), allocation-free. Designed for the splice arm
/// path and B's first PES test — both fire well outside the per-packet
/// hot path so this can sit on `block_in_place` callers happily.
pub fn parse_aac_adts_params(payload: &[u8]) -> Option<AacAudioParams> {
    if payload.len() < 7 {
        return None;
    }
    if payload[0] != 0xFF || (payload[1] & 0xF0) != 0xF0 {
        return None;
    }
    let profile = (payload[2] >> 6) & 0x03;
    let sample_rate_idx = (payload[2] >> 2) & 0x0F;
    let channel_config = ((payload[2] & 0x01) << 2) | ((payload[3] >> 6) & 0x03);
    Some(AacAudioParams {
        profile,
        sample_rate_idx,
        channel_config,
    })
}

/// Parse the AAC `AudioSpecificConfig` fields out of an LATM
/// `AudioMuxElement` at the start of a PES payload. Returns `None`
/// when the payload is too short, when the encoder reused the previous
/// `StreamMuxConfig` (`useSameStreamMux=1`, we don't have it), when
/// `audioMuxVersion=1` (extended config, not supported by today's
/// broadcast use case), or when `numProgram>0` / `numLayer>0` (multi-
/// program / SBR-as-separate-layer setups). Mainstream broadcast AAC-LC
/// over LATM falls into the supported subset; failure modes are
/// fail-safe (caller commits on PTS alone, no behavioural change vs
/// the audio MVP).
///
/// Output is normalised to the same `AacAudioParams` shape used by the
/// ADTS sentinel:
/// - `profile` carries `AudioObjectType − 1`, clamped to `0..=3` to
///   match the 2-bit ADTS profile field (AAC-LC → 1, the broadcast
///   default). HE-AAC v1/v2 signalled at the ASC layer as
///   AOT=5/29 still maps to profile=3 — receivers re-init the decoder
///   on any SBR/PS toggle anyway.
/// - `sample_rate_idx` carries the 4-bit `samplingFrequencyIndex`; the
///   spec's `15` escape (24-bit explicit frequency) preserves that
///   sentinel value so a mismatch against any non-15 index still
///   triggers the codec-param-mismatch refusal.
/// - `channel_config` carries the 4-bit `channelConfiguration`.
///
/// Reference: ISO/IEC 14496-3 § 1.7.3 (StreamMuxConfig) + § 1.6.2.1
/// (AudioSpecificConfig).
pub fn parse_aac_latm_params(payload: &[u8]) -> Option<AacAudioParams> {
    let mut br = BitReader::new(payload);
    let use_same_stream_mux = br.read_bit()?;
    if use_same_stream_mux != 0 {
        return None;
    }
    let audio_mux_version = br.read_bit()?;
    if audio_mux_version != 0 {
        return None;
    }
    br.read_bit()?; // allStreamsSameTimeFraming
    br.read_bits(6)?; // numSubFrames
    let num_program = br.read_bits(4)?;
    if num_program != 0 {
        return None;
    }
    let num_layer = br.read_bits(3)?;
    if num_layer != 0 {
        return None;
    }
    // prog=0 lay=0 → useSameConfig is implicit 0, ASC follows.
    let aot_5 = br.read_bits(5)? as u8;
    let audio_object_type = if aot_5 == 31 {
        let ext = br.read_bits(6)? as u16;
        (32 + ext).min(255) as u8
    } else {
        aot_5
    };
    let sfi_4 = br.read_bits(4)? as u8;
    let sample_rate_idx = if sfi_4 == 15 {
        br.read_bits(24)?; // consume the 24-bit explicit frequency
        15
    } else {
        sfi_4
    };
    let channel_config = br.read_bits(4)? as u8;
    let profile = audio_object_type.saturating_sub(1).min(3);
    Some(AacAudioParams {
        profile,
        sample_rate_idx,
        channel_config,
    })
}

/// Extract `AacAudioParams` from a PUSI=1 TS packet carrying an AAC
/// frame at the start of its PES payload. Dispatches on `stream_type`:
/// - `0x0F` → ADTS sync header parser ([`parse_aac_adts_params`]).
/// - `0x11` → LATM `AudioMuxElement` parser ([`parse_aac_latm_params`]),
///   added edge 0.66.0 (PES Switch Phase 4 LATM sentinel).
/// - Any other stream_type → `None` (caller already gates on this).
///
/// Returns `None` when the packet isn't a PUSI=1 PES, the PES payload
/// offset can't be located, or the parser bails (mid-PES bytes,
/// reused-config LATM frame, unsupported extension, fragmented frame).
/// `None` is fail-safe — the splice falls through to today's
/// commit-on-PTS behaviour.
pub fn extract_aac_params_from_pes(pkt: &[u8], stream_type: u8) -> Option<AacAudioParams> {
    if !ts_pusi(pkt) {
        return None;
    }
    let es_start = pes_payload_offset(pkt)?;
    match stream_type {
        0x0F => parse_aac_adts_params(&pkt[es_start..]),
        0x11 => parse_aac_latm_params(&pkt[es_start..]),
        _ => None,
    }
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
    /// (a) the to-leg produces a PUSI=1 PES with `pts ≥ threshold_pts`
    ///     and codec params matching A (or A's params are unparseable
    ///     so the sentinel falls through), or
    /// (b) the to-leg's first aligned PES has codec params that differ
    ///     from A's last params (refuse → `CodecParamMismatch`), or
    /// (c) `Instant::now() ≥ deadline` (caller falls back to PmtBump).
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
        /// MPEG-TS stream_type of the slot. Used by the codec-param
        /// sentinel to decide whether to look for an ADTS header on
        /// B's first PES (only `0x0F` AAC ADTS today; `0x11` LATM and
        /// every non-AAC codec skip the sentinel).
        stream_type: u8,
        /// `AudioSpecificConfig` snapshot from A's last PUSI=1 PES at
        /// arm time (refreshed via `record_a_audio_params` until A's
        /// AU completes). `None` when A's payload couldn't be parsed
        /// (LATM, mid-PES at arm, malformed frame) — the sentinel then
        /// falls through to today's PTS-only commit so we don't refuse
        /// a perfectly compatible splice on noise.
        expected_aac_params: Option<AacAudioParams>,
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
        expected_aac_params: Option<AacAudioParams>,
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
            stream_type,
            expected_aac_params,
        };
        true
    }

    /// Refresh A's `AudioSpecificConfig` snapshot from a fresh PUSI=1
    /// packet on the active leg. Caller invokes this on every A-leg
    /// PUSI=1 while the splice is `Pending` and `a_au_completed` is
    /// still false — that's the window where A's last frame fully
    /// emits and the snapshot stays receiver-meaningful. No-op when
    /// `Idle`, when A's AU has already completed, when the slot's
    /// stream_type isn't AAC ADTS, or when the payload doesn't parse
    /// as ADTS (LATM, fragmented frame).
    pub fn record_a_audio_params(&mut self, pkt: &[u8]) {
        let AudioSpliceState::Pending {
            a_au_completed,
            stream_type,
            expected_aac_params,
            ..
        } = self
        else {
            return;
        };
        if *a_au_completed {
            return;
        }
        // AAC ADTS (0x0F) + LATM (0x11). Every other codec skips the
        // sentinel — `extract_aac_params_from_pes` returns `None` for
        // them anyway, but a leading stream_type check keeps the no-op
        // path cheap on the per-packet hot loop.
        if *stream_type != 0x0F && *stream_type != 0x11 {
            return;
        }
        if let Some(params) = extract_aac_params_from_pes(pkt, *stream_type) {
            *expected_aac_params = Some(params);
        }
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
            threshold_pts,
            stream_type,
            expected_aac_params,
            to_input_id,
            ..
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
        if ahead > 1 << 31 {
            return None;
        }
        // PTS at or past threshold — now the codec-param sentinel.
        // Only fires when (a) the slot is AAC ADTS or AAC-LATM, (b) we
        // captured a baseline from A, and (c) B's PES yields a parseable
        // header. Any miss in (a)/(b)/(c) means we don't have enough
        // information to refuse — fall through to today's commit.
        let stream_type = *stream_type;
        let a_params = *expected_aac_params;
        let is_aac = matches!(stream_type, 0x0F | 0x11);
        let b_params = if is_aac && a_params.is_some() {
            extract_aac_params_from_pes(pkt, stream_type)
        } else {
            None
        };
        if let (Some(a), Some(b)) = (a_params, b_params) {
            if a != b {
                let outcome = SpliceOutcome::CodecParamMismatch {
                    to_input_id: std::mem::take(to_input_id),
                    a_params: a,
                    b_params: b,
                };
                *self = AudioSpliceState::Idle;
                return Some(outcome);
            }
        }
        let outcome = SpliceOutcome::Committed { first_b_pts: pts };
        *self = AudioSpliceState::Idle;
        Some(outcome)
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
    /// To-leg's first PUSI=1 PES arrived in time and at the right PTS,
    /// but its AAC `AudioSpecificConfig` differs from A's (channel
    /// count, sample rate, or profile). A mid-PES splice would click;
    /// caller falls back to PmtBump (flip active + bump PMT v+1 +
    /// DI=1) so receivers re-init the decoder cleanly on the new
    /// codec params, and emits `pes_splice_codec_param_mismatch` for
    /// operator visibility.
    CodecParamMismatch {
        to_input_id: String,
        a_params: AacAudioParams,
        b_params: AacAudioParams,
    },
    /// Video sibling to [`SpliceOutcome::CodecParamMismatch`]: B's
    /// first IDR PES past threshold parsed cleanly but its SPS-derived
    /// codec parameters differ from A's last snapshot. A mid-stream
    /// PES splice with changed parameters would require the receiver
    /// to re-initialise its decoder mid-AU (different bit depth,
    /// chroma format, resolution, profile/level), which most decoders
    /// cannot do without a clean PMT bump. The assembler falls back
    /// to `PmtBump` and emits `pes_splice_codec_param_mismatch` with
    /// `kind: "video"` carrying the full SPS-field diff so operators
    /// can see which field changed (the AAC sentinel only ever flags
    /// three fields; the video sentinel flags seven, so the diff
    /// is genuinely useful for triage).
    VideoCodecParamMismatch {
        to_input_id: String,
        a_params: VideoCodecParams,
        b_params: VideoCodecParams,
    },
}

// ── Video splice — IDR-aware boundary detector + state machine ───────────

/// Default splice budget for video in milliseconds. Sized to cover one
/// typical broadcast GoP (closed GoP of 0.5–2 s on every common codec
/// profile) plus a small encoder buffer. Operators can override via
/// `splice_budget_ms`; the validator still bounds the value to
/// [`SPLICE_BUDGET_MS_RANGE`].
pub const DEFAULT_VIDEO_SPLICE_BUDGET_MS: u32 = 2000;

/// Conservative one-frame interval in 90 kHz PTS ticks used by the video
/// splice machine to derive `threshold_pts = last_a_pts + this`. 3600
/// ticks (= 40 ms) is exactly one frame at 25 fps and a *little* longer
/// than a frame at 29.97 / 50 / 59.94 / 60 fps — so for the high-rate
/// cases the state machine waits one extra frame in the worst case, the
/// same fail-safe over-estimate the audio path uses (see
/// [`audio_frame_duration_90k`]). 24-fps content (3750 ticks/frame) is
/// the *one* case where 3600 under-estimates one frame; in practice 24p
/// is only used in cinema-on-air contribution where the GoP is still
/// closed at 12–24 frames and the IDR PTS easily clears the threshold.
pub const VIDEO_FRAME_DURATION_90K: u64 = 3600;

/// `true` iff the slot's MPEG-TS `stream_type` is one of the video
/// codecs the splice state machine knows how to align. H.264 (`0x1B`)
/// and HEVC (`0x24`) are supported; MPEG-2 video (`0x02`) is *not* —
/// receiver-side GoP recovery is uglier and a real splice would
/// re-acquire anyway, so today's [`SpliceMode::PmtBump`] fallback is
/// the right call for legacy content.
///
/// [`SpliceMode::PmtBump`]: crate::config::models::SpliceMode::PmtBump
pub fn is_supported_video_stream_type(stream_type: u8) -> bool {
    matches!(stream_type, 0x1B | 0x24)
}

/// `true` iff the raw NAL header byte is the start of an IDR-equivalent
/// AU for `stream_type`.
///
/// - H.264 (`0x1B`): NAL header is `forbidden_zero(1) | nal_ref_idc(2) |
///   nal_unit_type(5)`. IDR = `nal_unit_type == 5`.
/// - HEVC (`0x24`): NAL header byte 0 is `forbidden_zero(1) |
///   nal_unit_type(6) | layer_id[5](1)`. IRAP family = `nal_unit_type
///   ∈ {16..=21}` (BLA_W_LP, BLA_W_RADL, BLA_N_LP, IDR_W_RADL, IDR_N_LP,
///   CRA_NUT). All of these are valid splice points for a receiver.
/// - Other stream types return `false` — caller already gates the
///   walker on [`is_supported_video_stream_type`].
fn nal_is_idr(nal: u8, stream_type: u8) -> bool {
    match stream_type {
        0x1B => (nal & 0x1F) == 5,
        0x24 => {
            let nut = (nal >> 1) & 0x3F;
            (16..=21).contains(&nut)
        }
        _ => false,
    }
}

/// Walk the PES payload inside a TS packet looking for an Annex-B
/// start code followed by an IDR NAL header. Returns `true` on the
/// first hit.
///
/// `stream_type` is the slot's MPEG-TS stream type — `0x1B` (H.264) or
/// `0x24` (HEVC). Other values short-circuit to `false`. The walker
/// recognises both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start
/// codes, mirroring [`crate::engine::content_analysis::gop`]'s NAL
/// detector. Allocation-free, bounded by the TS packet size (~ 180 B
/// of PES payload after the header) so it's safe to run on the
/// per-packet hot path of an armed video splice.
///
/// Caller responsibilities:
/// - Pre-filter on PUSI=1 (the start of a PES is where IDR NALs live).
///   Mid-PES packets won't carry the slice header in their first bytes
///   and would mis-fire — the splice state machine gates on
///   [`crate::engine::ts_parse::ts_pusi`] before reaching this helper.
/// - Gate on [`is_supported_video_stream_type`] — non-video PES is
///   never IDR-bearing.
///
/// Edge case: the typical NAL order for an H.264 IDR PES is AUD →
/// (SPS) → (PPS) → SEI → slice (NAL type 5). On most encoders this all
/// fits in the first TS packet of the PES (~170 B usable after the PES
/// header). If an encoder pushes the IDR slice past the first TS packet
/// (rare, only with very large SEI), this helper misses; the splice
/// then waits for the next IDR PES, which is exactly the safe
/// behaviour — `pes_splice_timeout` fires after the budget if no IDR
/// arrives.
pub fn pes_contains_idr(pkt: &[u8], stream_type: u8) -> bool {
    if !is_supported_video_stream_type(stream_type) {
        return false;
    }
    let Some(es_start) = pes_payload_offset(pkt) else {
        return false;
    };
    let bytes = &pkt[es_start..];
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        let is_sc3 = bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1;
        let is_sc4 = bytes[i] == 0
            && bytes[i + 1] == 0
            && bytes[i + 2] == 0
            && bytes[i + 3] == 1;
        if is_sc4 {
            if i + 5 > bytes.len() {
                break;
            }
            if nal_is_idr(bytes[i + 4], stream_type) {
                return true;
            }
            i += 5;
            continue;
        }
        if is_sc3 {
            if nal_is_idr(bytes[i + 3], stream_type) {
                return true;
            }
            i += 4;
            continue;
        }
        i += 1;
    }
    false
}

/// Compressed-video codec family tracked by the video splice sentinel.
/// Mirrors the MPEG-TS stream_type discriminator used elsewhere — keeps
/// the [`VideoCodecParams`] comparison codec-aware (mismatched codec
/// families always count as a mismatch, even when individual field
/// values happen to coincide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 / AVC. MPEG-TS stream_type `0x1B`.
    Avc,
    /// H.265 / HEVC. MPEG-TS stream_type `0x24`.
    Hevc,
}

/// SPS-derived codec parameters compared by the video splice sentinel.
///
/// The fields chosen are exactly the ones a hardware video decoder
/// (Appear X10, Cobalt 9202, Cisco D9824 — the gate-7 receivers) has to
/// re-initialise on if they change mid-PES:
///
/// - `profile_idc` — receiver picks a different profile-specific
///   decode pipeline (Baseline / Main / High / High10 / High 4:2:2 /
///   Main10 / Main 4:2:2 10). Mismatch = decoder restart required.
/// - `level_idc` — buffer sizes / pipeline depth change. Mismatch =
///   decoder may need to re-allocate VPP / CABAC / DPB pools.
/// - `chroma_format_idc` — 4:2:0 / 4:2:2 / 4:4:4. Mismatch =
///   different subsampling, fundamentally different colour pipeline.
/// - `bit_depth_luma` / `bit_depth_chroma` — 8-bit vs 10-bit vs
///   12-bit. Mismatch = decoder output bit depth flips, downstream
///   colour processing pipeline reshapes.
/// - `width` / `height` — coded luma dimensions. Mismatch = output
///   surface allocation changes; on most receivers this is a hard
///   re-acquire.
///
/// `frame_mbs_only_flag` (H.264) is folded into `height` via the
/// `(2 − flag)` field-height multiplier per § 7.4.2.1.1; interlaced
/// vs progressive flips show up as a 2× height change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoCodecParams {
    pub codec: VideoCodec,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub width: u32,
    pub height: u32,
}

/// Walk Annex-B NALs in a PUSI=1 TS packet's PES payload looking for
/// an SPS NAL (H.264 `nal_unit_type == 7`, HEVC `nal_unit_type == 33`),
/// then return its **unescaped RBSP** byte slice ready for SPS parsing.
///
/// Returns `None` when the packet isn't a PUSI, the PES payload offset
/// can't be located, the stream_type isn't supported, or no SPS NAL
/// appears before either the end of the TS packet or the next NAL
/// start code. The walker bounds itself to the ~180 B of PES payload
/// inside a single TS packet, so it's safe to call on the splice-arm
/// path + B's first-IDR test (both fire well off the per-packet hot
/// loop).
///
/// The RBSP unescape pass (`0x00 0x00 0x03` → `0x00 0x00`) allocates
/// once per call — at most a few times per second per active video
/// splice. Allocation cost is in the snapshot-refresh path, not the
/// every-packet path.
fn extract_sps_rbsp(pkt: &[u8], stream_type: u8) -> Option<Vec<u8>> {
    if !ts_pusi(pkt) {
        return None;
    }
    let target_nut: u8 = match stream_type {
        0x1B => 7,
        0x24 => 33,
        _ => return None,
    };
    let es_start = pes_payload_offset(pkt)?;
    let bytes = &pkt[es_start..];
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        let is_sc3 = bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1;
        let is_sc4 = bytes[i] == 0
            && bytes[i + 1] == 0
            && bytes[i + 2] == 0
            && bytes[i + 3] == 1;
        let (sc_size, nal_off) = if is_sc4 {
            (4usize, i + 4)
        } else if is_sc3 {
            (3usize, i + 3)
        } else {
            i += 1;
            continue;
        };
        if nal_off >= bytes.len() {
            return None;
        }
        let nut = match stream_type {
            0x1B => bytes[nal_off] & 0x1F,
            0x24 => (bytes[nal_off] >> 1) & 0x3F,
            _ => return None,
        };
        if nut == target_nut {
            // NAL header size: 1 byte for H.264, 2 bytes for HEVC.
            let payload_start = nal_off
                + match stream_type {
                    0x1B => 1,
                    0x24 => 2,
                    _ => return None,
                };
            // Scan forward for the next start code or buffer end.
            let mut end = payload_start;
            while end + 3 <= bytes.len() {
                if bytes[end] == 0
                    && bytes[end + 1] == 0
                    && (bytes[end + 2] == 0 || bytes[end + 2] == 1)
                {
                    break;
                }
                end += 1;
            }
            if end < bytes.len() && bytes.len() - end < 3 {
                end = bytes.len();
            }
            if payload_start >= end {
                return None;
            }
            return Some(unescape_rbsp(&bytes[payload_start..end]));
        }
        i = nal_off + sc_size.min(1); // advance past this NAL header byte
    }
    None
}

/// Parse an H.264 SPS RBSP into [`VideoCodecParams`]. Bails (returns
/// `None`) on any failure mode the bit reader hits — out-of-buffer,
/// malformed Exp-Golomb, scaling-list flag set (would require parsing
/// large variable-length scaling lists; the sentinel doesn't need that
/// data and the splice falls through to a PTS-only commit). The
/// `expected_profile` byte at `rbsp[0]` is taken at face value; the
/// `level_idc` byte is at `rbsp[2]`.
pub fn parse_avc_sps_params(rbsp: &[u8]) -> Option<VideoCodecParams> {
    if rbsp.len() < 4 {
        return None;
    }
    let profile_idc = rbsp[0];
    // rbsp[1] = constraint_set flags + reserved (not needed for the sentinel).
    let level_idc = rbsp[2];
    let mut br = BitReader::new(&rbsp[3..]);
    br.read_ue()?; // seq_parameter_set_id

    let extended_profile = matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    );
    let (chroma_format_idc, bit_depth_luma, bit_depth_chroma);
    if extended_profile {
        let cf = br.read_ue()? as u8;
        chroma_format_idc = cf;
        if cf == 3 {
            br.read_bit()?; // separate_colour_plane_flag
        }
        bit_depth_luma = 8 + (br.read_ue()? as u8);
        bit_depth_chroma = 8 + (br.read_ue()? as u8);
        br.read_bit()?; // qpprime_y_zero_transform_bypass_flag
        let seq_scaling = br.read_bit()?;
        if seq_scaling == 1 {
            // Scaling-list parsing is non-trivial and not needed for the
            // sentinel; surface a partial snapshot so the caller can
            // still compare the three fields parsed so far (chroma,
            // bit-depth). width/height are returned as 0 — comparison
            // against another partial snapshot still flags a mismatch
            // when the partial fields differ.
            return Some(VideoCodecParams {
                codec: VideoCodec::Avc,
                profile_idc,
                level_idc,
                chroma_format_idc,
                bit_depth_luma,
                bit_depth_chroma,
                width: 0,
                height: 0,
            });
        }
    } else {
        // Baseline / Main / Extended → 4:2:0 8-bit by spec.
        chroma_format_idc = 1;
        bit_depth_luma = 8;
        bit_depth_chroma = 8;
    }
    br.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = br.read_ue()?;
    if pic_order_cnt_type == 0 {
        br.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        br.read_bit()?; // delta_pic_order_always_zero_flag
        br.read_se()?; // offset_for_non_ref_pic
        br.read_se()?; // offset_for_top_to_bottom_field
        let n = br.read_ue()?;
        for _ in 0..n.min(256) {
            br.read_se()?;
        }
    }
    br.read_ue()?; // max_num_ref_frames
    br.read_bit()?; // gaps_in_frame_num_value_allowed_flag
    let pic_width_in_mbs_minus1 = br.read_ue()?;
    let pic_height_in_map_units_minus1 = br.read_ue()?;
    let frame_mbs_only_flag = br.read_bit()?;

    let width = (pic_width_in_mbs_minus1 + 1) * 16;
    let height = (2 - frame_mbs_only_flag as u32) * (pic_height_in_map_units_minus1 + 1) * 16;

    Some(VideoCodecParams {
        codec: VideoCodec::Avc,
        profile_idc,
        level_idc,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
        width,
        height,
    })
}

/// Parse an HEVC SPS RBSP into [`VideoCodecParams`]. Same shape /
/// fail-safe semantics as [`parse_avc_sps_params`]. The
/// `profile_tier_level` walk mirrors the production parser in
/// `engine::content_analysis::signalling::skip_profile_tier_level` —
/// 112 bits per sub_layer profile rather than the spec's 88, because
/// the spec-correct chunking reads `pic_width` / `pic_height` as 0 on
/// some live 4K HEVC encoders (encoder quirk; see comment on
/// `skip_profile_tier_level`).
pub fn parse_hevc_sps_params(rbsp: &[u8]) -> Option<VideoCodecParams> {
    let mut br = BitReader::new(rbsp);
    br.read_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = br.read_bits(3)? as u8;
    br.read_bit()?; // sps_temporal_id_nesting_flag

    // profile_tier_level — inline rather than reusing
    // signalling::skip_profile_tier_level because we want the
    // general_profile_idc + general_level_idc returned, not skipped.
    br.read_bits(2)?; // general_profile_space
    br.read_bit()?; // general_tier_flag
    let general_profile_idc = br.read_bits(5)? as u8;
    br.read_bits(32)?; // general_profile_compatibility_flag[32]
    // general_progressive(1) interlaced(1) non_packed(1) frame_only(1) + 44 reserved
    // → 16 + 32 = 48 bits.
    br.read_bits(16)?;
    br.read_bits(32)?;
    let general_level_idc = br.read_bits(8)? as u8;
    let mut sub_layer_profile_present = [0u8; 8];
    let mut sub_layer_level_present = [0u8; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_layer_profile_present[i] = br.read_bit()?;
        sub_layer_level_present[i] = br.read_bit()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            br.read_bits(2)?; // reserved_zero_2bits
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_layer_profile_present[i] == 1 {
            // 112 bits — matches signalling.rs production-tested chunking.
            br.read_bits(32)?;
            br.read_bits(32)?;
            br.read_bits(16)?;
            br.read_bits(32)?;
        }
        if sub_layer_level_present[i] == 1 {
            br.read_bits(8)?;
        }
    }

    br.read_ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = br.read_ue()? as u8;
    if chroma_format_idc == 3 {
        br.read_bit()?;
    }
    let pic_width = br.read_ue()?;
    let pic_height = br.read_ue()?;
    let conformance_window_flag = br.read_bit()?;
    if conformance_window_flag == 1 {
        br.read_ue()?;
        br.read_ue()?;
        br.read_ue()?;
        br.read_ue()?;
    }
    let bit_depth_luma = 8 + (br.read_ue()? as u8);
    let bit_depth_chroma = 8 + (br.read_ue()? as u8);

    Some(VideoCodecParams {
        codec: VideoCodec::Hevc,
        profile_idc: general_profile_idc,
        level_idc: general_level_idc,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
        width: pic_width,
        height: pic_height,
    })
}

/// Extract [`VideoCodecParams`] from a PUSI=1 TS packet carrying an
/// SPS NAL at the start of its PES payload (typically the first NAL
/// after AUD on an H.264 IDR PES, or `SPS_NUT` on an HEVC IRAP PES).
/// Returns `None` when the packet isn't a PUSI=1 video PES, no SPS is
/// in the payload, or the parser fails — caller treats `None` as
/// "fall through to PTS-only commit" the same way the AAC sentinel
/// treats `None` from `extract_aac_params_from_pes`.
pub fn extract_video_params_from_pes(pkt: &[u8], stream_type: u8) -> Option<VideoCodecParams> {
    let rbsp = extract_sps_rbsp(pkt, stream_type)?;
    match stream_type {
        0x1B => parse_avc_sps_params(&rbsp),
        0x24 => parse_hevc_sps_params(&rbsp),
        _ => None,
    }
}

/// State of a single switch slot's PES-aligned video splice.
///
/// Sibling to [`AudioSpliceState`] — same shape, same lifecycle, same
/// commit semantics on `pts ≥ threshold_pts`. The one difference is
/// that B's first PUSI=1 PES past the threshold must *additionally*
/// carry an IDR NAL ([`pes_contains_idr`]); a non-IDR PES is held the
/// same as a PES below threshold. Without this, a downstream decoder
/// would receive bytes that depend on AUs it never decoded (the
/// preceding GoP from A), and freeze on the next anchor frame
/// regardless of PMT-version bumps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSpliceState {
    /// No splice in flight; the slot follows today's behaviour.
    Idle,
    /// Splice armed — outbound is held until one of:
    /// (a) the to-leg produces a PUSI=1 PES with `pts ≥ threshold_pts`
    ///     and an IDR NAL in the same packet AND its SPS parameters
    ///     match A's snapshot (or either snapshot is unparseable —
    ///     fail-safe to commit),
    /// (b) the to-leg's first aligned IDR PES has SPS parameters that
    ///     differ from A's (refuse → `VideoCodecParamMismatch`), or
    /// (c) `Instant::now() ≥ deadline` (caller falls back to PmtBump).
    Pending {
        /// Input id of the to-leg the operator is switching to.
        to_input_id: String,
        /// Last PTS observed on the from-leg before `arm` was called.
        last_a_pts: u64,
        /// `last_a_pts + VIDEO_FRAME_DURATION_90K` (mod 2^33). The
        /// first accepted PES must have `pts ≥ threshold_pts`.
        threshold_pts: u64,
        /// Wallclock budget — `Instant::now() + splice_budget`. On
        /// expiry the assembler falls back to PmtBump and emits
        /// `pes_splice_timeout`.
        deadline: Instant,
        /// `true` once we've observed A's *next* PUSI=1 after arming.
        /// That's the marker that A's current AU (whose PTS the
        /// threshold is built off) has finished emitting and we
        /// should stop forwarding A's bytes — otherwise we'd emit a
        /// fragment of A's next AU into the receiver's pipeline and
        /// trigger a decoder error.
        a_au_completed: bool,
        /// MPEG-TS stream_type of the slot (`0x1B` or `0x24`). Used by
        /// [`pes_contains_idr`] to pick the per-codec IDR rule.
        stream_type: u8,
        /// SPS-derived parameter snapshot from A's last PUSI=1 IDR PES
        /// at arm time (refreshed via [`VideoSpliceState::record_a_video_params`]
        /// until A's AU completes). `None` when A's payload didn't
        /// carry a parseable SPS (mid-PES at arm, or A's encoder
        /// suppresses SPS between IDRs) — sentinel falls through to a
        /// PTS+IDR-only commit so we don't refuse a perfectly
        /// compatible splice on noise.
        expected_params: Option<VideoCodecParams>,
    },
}

impl VideoSpliceState {
    /// Construct an Idle state.
    pub fn new() -> Self {
        VideoSpliceState::Idle
    }

    /// Arm the splice. Returns `false` and stays `Idle` when the slot's
    /// `stream_type` isn't one of the supported video codecs — caller
    /// falls back to PmtBump silently.
    pub fn arm(
        &mut self,
        to_input_id: String,
        stream_type: u8,
        last_a_pts: u64,
        now: Instant,
        budget: Duration,
        expected_params: Option<VideoCodecParams>,
    ) -> bool {
        if !is_supported_video_stream_type(stream_type) {
            return false;
        }
        let threshold_pts =
            last_a_pts.wrapping_add(VIDEO_FRAME_DURATION_90K) & 0x1_FFFF_FFFF;
        *self = VideoSpliceState::Pending {
            to_input_id,
            last_a_pts,
            threshold_pts,
            deadline: now + budget,
            a_au_completed: false,
            stream_type,
            expected_params,
        };
        true
    }

    /// Refresh A's SPS parameter snapshot from a PUSI=1 IDR PES on the
    /// active leg. Caller invokes this on every A-leg PUSI=1 while the
    /// splice is `Pending` and `a_au_completed` is still false — that's
    /// the window where A's last frame fully emits and the snapshot
    /// stays receiver-meaningful. No-op when `Idle`, when A's AU has
    /// already completed, or when the payload doesn't carry a parseable
    /// SPS (most non-IDR PES, or encoders that suppress SPS between
    /// IRAPs).
    pub fn record_a_video_params(&mut self, pkt: &[u8]) {
        let VideoSpliceState::Pending {
            a_au_completed,
            stream_type,
            expected_params,
            ..
        } = self
        else {
            return;
        };
        if *a_au_completed {
            return;
        }
        if let Some(params) = extract_video_params_from_pes(pkt, *stream_type) {
            *expected_params = Some(params);
        }
    }

    /// Decide whether to forward or drop a packet from the **from-leg**
    /// during a pending splice. Identical semantics to
    /// [`AudioSpliceState::observe_a_packet`]: forwards until A's first
    /// PUSI=1 after `arm` flips `a_au_completed`; thereafter returns
    /// [`FromPacketAction::Drop`]. Outside of `Pending` always returns
    /// `Forward` so the assembler can call unconditionally.
    pub fn observe_a_packet(&mut self, pkt: &[u8]) -> FromPacketAction {
        let VideoSpliceState::Pending { a_au_completed, .. } = self else {
            return FromPacketAction::Forward;
        };
        if *a_au_completed {
            return FromPacketAction::Drop;
        }
        if ts_pusi(pkt) {
            // A's next AU is starting — the AU whose PTS we captured at
            // arm-time is finished emitting. Drop this PES-start byte
            // (it's the next AU we won't emit) and stop forwarding A
            // entirely.
            *a_au_completed = true;
            return FromPacketAction::Drop;
        }
        FromPacketAction::Forward
    }

    /// Process one fan-in packet from the **to-leg**. Returns:
    /// - `Some(SpliceOutcome::Committed)` when the packet is a PUSI=1
    ///   PES with PTS ≥ threshold, an IDR NAL in its payload, and
    ///   either no codec-param baseline exists (fail-safe commit) or
    ///   B's SPS matches A's.
    /// - `Some(SpliceOutcome::VideoCodecParamMismatch)` when an IDR PES
    ///   arrived at the right PTS but B's SPS parameters differ from
    ///   A's — caller falls back to PmtBump.
    /// - `None` when the packet is mid-PES, below-threshold, or
    ///   non-IDR (the next IDR PES will commit; budget exhaustion is
    ///   the fallback).
    pub fn observe_b_packet(&mut self, pkt: &[u8]) -> Option<SpliceOutcome> {
        let VideoSpliceState::Pending {
            threshold_pts,
            stream_type,
            expected_params,
            to_input_id,
            ..
        } = self
        else {
            return None;
        };
        if !ts_pusi(pkt) {
            return None;
        }
        let pts = extract_pes_pts(pkt)?;
        let threshold = *threshold_pts;
        // 33-bit PTS comparison with wrap tolerance: anything within
        // 2^31 ticks (≈ 6.6 h) ahead of `threshold_pts` counts as
        // "≥ threshold". A "behind" PTS is the wrap-back case → wait.
        let ahead = pts.wrapping_sub(threshold) & 0x1_FFFF_FFFF;
        if ahead > 1 << 31 {
            return None;
        }
        // PTS at/past threshold — now require an IDR NAL in the same
        // packet. Non-IDR PES (P/B frames) cannot be a splice point
        // for the receiver. The check runs only here, not per-packet
        // on the from-leg, so the per-packet hot-path cost is unchanged
        // when the splice is *not* armed.
        let st = *stream_type;
        if !pes_contains_idr(pkt, st) {
            return None;
        }
        // IDR + PTS aligned — now the codec-param sentinel. Only fires
        // when (a) we captured a baseline from A and (b) B's IDR PES
        // yields a parseable SPS. Any miss in (a)/(b) means we don't
        // have enough information to refuse — fall through to commit.
        let a_params = *expected_params;
        let b_params = if a_params.is_some() {
            extract_video_params_from_pes(pkt, st)
        } else {
            None
        };
        if let (Some(a), Some(b)) = (a_params, b_params) {
            if a != b {
                let outcome = SpliceOutcome::VideoCodecParamMismatch {
                    to_input_id: std::mem::take(to_input_id),
                    a_params: a,
                    b_params: b,
                };
                *self = VideoSpliceState::Idle;
                return Some(outcome);
            }
        }
        let outcome = SpliceOutcome::Committed { first_b_pts: pts };
        *self = VideoSpliceState::Idle;
        Some(outcome)
    }

    /// Check whether the splice budget has expired. Returns
    /// `Some(SpliceOutcome::Timeout { to_input_id })` exactly once on
    /// the first call past the deadline; transitions back to Idle
    /// afterwards. The caller falls back to PmtBump on the to-leg.
    pub fn check_timeout(&mut self, now: Instant) -> Option<SpliceOutcome> {
        let VideoSpliceState::Pending {
            deadline,
            to_input_id,
            ..
        } = self
        else {
            return None;
        };
        if now >= *deadline {
            let to = std::mem::take(to_input_id);
            *self = VideoSpliceState::Idle;
            Some(SpliceOutcome::Timeout { to_input_id: to })
        } else {
            None
        }
    }

    /// `true` iff the splice is currently armed.
    pub fn is_pending(&self) -> bool {
        matches!(self, VideoSpliceState::Pending { .. })
    }

    /// Returns the to-leg's input_id when armed.
    pub fn pending_to_input_id(&self) -> Option<&str> {
        if let VideoSpliceState::Pending { to_input_id, .. } = self {
            Some(to_input_id)
        } else {
            None
        }
    }
}

impl Default for VideoSpliceState {
    fn default() -> Self {
        VideoSpliceState::Idle
    }
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

    /// Helper: arm with no codec-param sentinel (matches today's MVP
    /// path for tests that only exercise PTS alignment).
    fn arm_no_sentinel(
        s: &mut AudioSpliceState,
        to: &str,
        stream_type: u8,
        last_a_pts: u64,
        now: Instant,
        budget: Duration,
    ) -> bool {
        s.arm(to.to_string(), stream_type, last_a_pts, now, budget, None)
    }

    #[test]
    fn arm_rejects_non_audio_stream_type() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        let armed = arm_no_sentinel(
            &mut s,
            "to",
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
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, 90_000, now, Duration::from_millis(200)));
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
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, 90_000, now, Duration::from_millis(200)));
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
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, 90_000, now, Duration::from_millis(200)));
        // B's first PES is exactly the same as last A — alias, must wait.
        let pkt = pkt_with_pts(90_000, true);
        assert!(s.observe_b_packet(&pkt).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn skip_non_pusi() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, 90_000, now, Duration::from_millis(200)));
        let pkt = pkt_with_pts(99_999, /* pusi */ false);
        assert!(s.observe_b_packet(&pkt).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn timeout_emits_once() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(arm_no_sentinel(
            &mut s,
            "to-leg",
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
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, 90_000, now, Duration::from_millis(200)));
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
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, near_top, now, Duration::from_millis(200)));
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
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, 90_000, now, Duration::from_millis(200)));
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
        assert!(arm_no_sentinel(&mut s, "to", 0x0F, 90_000, now, Duration::from_millis(200)));
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

    // ── Codec-param sentinel ──────────────────────────────────────

    /// Build a PUSI=1 PES TS packet with PTS-only header plus an
    /// ADTS-framed AAC payload immediately after the PES header. The
    /// PES_header_data_length is 5 (PTS only) so the ES (= ADTS frame)
    /// starts at byte 18 of the TS packet.
    fn pkt_with_pts_and_adts(
        pts: u64,
        pusi: bool,
        profile: u8,
        sample_rate_idx: u8,
        channel_config: u8,
    ) -> [u8; 188] {
        let mut p = pkt_with_pts(pts, pusi);
        // ADTS at byte 18: sync 0xFFF, layer=00, protection_absent=1.
        p[18] = 0xFF;
        p[19] = 0xF1; // 11110001 — MPEG-4, layer 0, protection_absent
        p[20] = ((profile & 0x03) << 6)
            | ((sample_rate_idx & 0x0F) << 2)
            | ((channel_config & 0x04) >> 2); // top bit of channel_config
        p[21] = ((channel_config & 0x03) << 6) | 0x00; // low two bits of channel_config, no frame_length bits
        // bytes 22..24 = frame_length lower bits / buffer fullness — irrelevant for the sentinel
        p[22] = 0x00;
        p[23] = 0x80;
        p[24] = 0x00;
        p
    }

    fn aac_lc_stereo_48k() -> AacAudioParams {
        AacAudioParams {
            profile: 1,
            sample_rate_idx: 3,
            channel_config: 2,
        }
    }

    #[test]
    fn parse_adts_round_trip() {
        let pkt = pkt_with_pts_and_adts(1_000_000, true, 1, 3, 2);
        let es_start = pes_payload_offset(&pkt).expect("pes payload offset");
        let parsed = parse_aac_adts_params(&pkt[es_start..]).expect("parse adts");
        assert_eq!(parsed, aac_lc_stereo_48k());
        assert_eq!(parsed.sample_rate_hz(), 48000);
        // 5.1 from 48 kHz LC.
        let pkt = pkt_with_pts_and_adts(1_000_000, true, 1, 3, 6);
        let params = extract_aac_params_from_pes(&pkt, 0x0F).unwrap();
        assert_eq!(params.channel_config, 6);
    }

    #[test]
    fn parse_adts_rejects_non_sync() {
        assert!(parse_aac_adts_params(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).is_none());
        // Too short:
        assert!(parse_aac_adts_params(&[0xFF, 0xF1]).is_none());
    }

    #[test]
    fn extract_aac_params_skips_non_pusi() {
        let pkt = pkt_with_pts_and_adts(1_000_000, /* pusi */ false, 1, 3, 2);
        assert!(extract_aac_params_from_pes(&pkt, 0x0F).is_none());
    }

    #[test]
    fn record_a_audio_params_captures_aac() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            None, // A's params not captured at arm
        ));
        let a_pkt = pkt_with_pts_and_adts(89_000, true, 1, 3, 2); // pre-arm PUSI from A
        s.record_a_audio_params(&a_pkt);
        match &s {
            AudioSpliceState::Pending { expected_aac_params, .. } => {
                assert_eq!(*expected_aac_params, Some(aac_lc_stereo_48k()));
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn record_a_audio_params_noop_for_non_aac_stream_type() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        // MP2 audio — frame duration table accepts it, but the codec
        // sentinel is ADTS-only today.
        assert!(s.arm(
            "to".into(),
            0x04,
            90_000,
            now,
            Duration::from_millis(200),
            None,
        ));
        let a_pkt = pkt_with_pts_and_adts(89_000, true, 1, 3, 2);
        s.record_a_audio_params(&a_pkt);
        match &s {
            AudioSpliceState::Pending { expected_aac_params, .. } => {
                assert_eq!(*expected_aac_params, None);
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn record_a_audio_params_noop_after_au_completed() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            None,
        ));
        // Drive A's next PUSI=1 to mark AU completed.
        let a_first = pkt_with_pts_and_adts(91_920, true, 1, 3, 2);
        assert_eq!(s.observe_a_packet(&a_first), FromPacketAction::Drop);
        // Any subsequent record attempt is a no-op (we don't want A's
        // post-AU-completion params overwriting the snapshot since
        // those bytes won't be emitted to the receiver anyway).
        let later = pkt_with_pts_and_adts(93_840, true, 1, 3, 6);
        s.record_a_audio_params(&later);
        match &s {
            AudioSpliceState::Pending { expected_aac_params, .. } => {
                assert_eq!(*expected_aac_params, None);
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn sentinel_commits_when_aac_params_match() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            Some(aac_lc_stereo_48k()),
        ));
        // B's first PES at threshold with matching params → commit.
        let b_pkt = pkt_with_pts_and_adts(91_920, true, 1, 3, 2);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 91_920 })
        ));
        assert!(!s.is_pending());
    }

    #[test]
    fn sentinel_rejects_on_channel_count_change() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to-leg".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            Some(aac_lc_stereo_48k()),
        ));
        // B is 5.1 — channel_config changed from 2 to 6.
        let b_pkt = pkt_with_pts_and_adts(91_920, true, 1, 3, 6);
        match s.observe_b_packet(&b_pkt) {
            Some(SpliceOutcome::CodecParamMismatch {
                to_input_id,
                a_params,
                b_params,
            }) => {
                assert_eq!(to_input_id, "to-leg");
                assert_eq!(a_params, aac_lc_stereo_48k());
                assert_eq!(b_params.channel_config, 6);
                assert_eq!(b_params.sample_rate_idx, 3);
            }
            other => panic!("expected CodecParamMismatch, got {other:?}"),
        }
        assert!(!s.is_pending());
    }

    #[test]
    fn sentinel_rejects_on_sample_rate_change() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            Some(aac_lc_stereo_48k()),
        ));
        // B is 44.1 kHz (idx 4) — sample_rate changed.
        let b_pkt = pkt_with_pts_and_adts(91_920, true, 1, 4, 2);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::CodecParamMismatch { .. })
        ));
    }

    #[test]
    fn sentinel_rejects_on_profile_change() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            Some(aac_lc_stereo_48k()),
        ));
        // B is Main profile (0) instead of LC (1).
        let b_pkt = pkt_with_pts_and_adts(91_920, true, 0, 3, 2);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::CodecParamMismatch { .. })
        ));
    }

    #[test]
    fn sentinel_falls_through_when_a_params_unknown() {
        // We armed without an A baseline — sentinel can't refuse, so
        // we commit on PTS alignment alone. This is the audio-MVP
        // fallback path: AAC-LATM A leg, or arm happened before any
        // ADTS-parseable PUSI was seen.
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            None,
        ));
        let b_pkt = pkt_with_pts_and_adts(91_920, true, 1, 4, 6); // would have mismatched
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 91_920 })
        ));
    }

    #[test]
    fn sentinel_falls_through_when_b_params_unknown() {
        // A snapshot present, but B's PES isn't ADTS-parseable (e.g.
        // LATM source, or ADTS frame straddles two TS packets). We
        // can't refuse on missing data — commit on PTS alone.
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x0F,
            90_000,
            now,
            Duration::from_millis(200),
            Some(aac_lc_stereo_48k()),
        ));
        // pkt_with_pts only carries PES header + PTS, no ADTS payload
        // → parser returns None → sentinel falls through → commit.
        let b_pkt = pkt_with_pts(91_920, true);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 91_920 })
        ));
    }

    /// Build an LATM AudioMuxElement bit stream for the simple case
    /// (`useSameStreamMux=0`, `audioMuxVersion=0`,
    /// `allStreamsSameTimeFraming=1`, `numSubFrames=0`, `numProgram=0`,
    /// `numLayer=0`) followed by a minimal AudioSpecificConfig
    /// (`audioObjectType`, `samplingFrequencyIndex`, `channelConfiguration`).
    /// Returns the packed bytes ready to be planted as a PES payload.
    fn build_latm_payload(aot: u8, sfi: u8, channel_config: u8) -> Vec<u8> {
        let mut bits: Vec<u8> = vec![];
        bits.push(0); // useSameStreamMux = 0
        bits.push(0); // audioMuxVersion = 0
        bits.push(1); // allStreamsSameTimeFraming = 1
        for _ in 0..6 {
            bits.push(0); // numSubFrames = 0
        }
        for _ in 0..4 {
            bits.push(0); // numProgram = 0
        }
        for _ in 0..3 {
            bits.push(0); // numLayer = 0
        }
        // AudioSpecificConfig: AOT (5 bits — assume ≤30, no escape), SFI (4), channel_config (4)
        for i in (0..5).rev() {
            bits.push((aot >> i) & 1);
        }
        for i in (0..4).rev() {
            bits.push((sfi >> i) & 1);
        }
        for i in (0..4).rev() {
            bits.push((channel_config >> i) & 1);
        }
        pack_bits(&bits)
    }

    /// Build a PUSI=1 stream_type-0x11 PES TS packet with a LATM
    /// payload immediately after the PES header.
    fn pkt_with_pts_and_latm(
        pts: u64,
        pusi: bool,
        aot: u8,
        sfi: u8,
        channel_config: u8,
    ) -> [u8; 188] {
        let mut p = pkt_with_pts(pts, pusi);
        let latm = build_latm_payload(aot, sfi, channel_config);
        let cap = (p.len() - 18).min(latm.len());
        p[18..18 + cap].copy_from_slice(&latm[..cap]);
        p
    }

    #[test]
    fn parse_latm_round_trip_aac_lc_stereo_48k() {
        // AOT=2 (AAC-LC), SFI=3 (48 kHz), channel_config=2 (stereo).
        let payload = build_latm_payload(2, 3, 2);
        let parsed = parse_aac_latm_params(&payload).expect("parse latm");
        // LATM → AacAudioParams normalisation: profile = AOT - 1
        // (AAC-LC AOT=2 → profile=1, matching ADTS convention).
        assert_eq!(parsed.profile, 1);
        assert_eq!(parsed.sample_rate_idx, 3);
        assert_eq!(parsed.channel_config, 2);
        assert_eq!(parsed.sample_rate_hz(), 48000);
    }

    #[test]
    fn parse_latm_rejects_use_same_stream_mux() {
        // useSameStreamMux=1 means "reuse prior StreamMuxConfig". The
        // parser doesn't have it; fail-safe to None.
        let mut bits: Vec<u8> = vec![];
        bits.push(1);
        let payload = pack_bits(&bits);
        assert!(parse_aac_latm_params(&payload).is_none());
    }

    #[test]
    fn parse_latm_rejects_audio_mux_version_1() {
        let mut bits: Vec<u8> = vec![];
        bits.push(0); // useSameStreamMux=0
        bits.push(1); // audioMuxVersion=1 (extended; not supported)
        let payload = pack_bits(&bits);
        assert!(parse_aac_latm_params(&payload).is_none());
    }

    #[test]
    fn extract_latm_params_via_dispatch() {
        // The same extract path used in the assembler — dispatch on
        // stream_type 0x11 → LATM parser.
        let pkt = pkt_with_pts_and_latm(1_000_000, true, 2, 3, 6);
        let params = extract_aac_params_from_pes(&pkt, 0x11).expect("extract latm");
        assert_eq!(params.profile, 1); // AAC-LC normalised
        assert_eq!(params.sample_rate_idx, 3);
        assert_eq!(params.channel_config, 6);
    }

    #[test]
    fn extract_latm_dispatch_returns_none_for_wrong_stream_type() {
        // LATM payload but stream_type set to 0x0F (ADTS) — dispatch
        // calls the ADTS parser, which fails on the missing sync word.
        let pkt = pkt_with_pts_and_latm(1_000_000, true, 2, 3, 2);
        assert!(extract_aac_params_from_pes(&pkt, 0x0F).is_none());
    }

    #[test]
    fn sentinel_rejects_latm_channel_count_change() {
        // A is LATM AAC-LC stereo 48 kHz; B is LATM AAC-LC 5.1 48 kHz.
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        let a_params = AacAudioParams {
            profile: 1,
            sample_rate_idx: 3,
            channel_config: 2,
        };
        assert!(s.arm(
            "to-leg".into(),
            0x11, // LATM
            90_000,
            now,
            Duration::from_millis(200),
            Some(a_params),
        ));
        let b_pkt = pkt_with_pts_and_latm(91_920, true, 2, 3, 6);
        match s.observe_b_packet(&b_pkt) {
            Some(SpliceOutcome::CodecParamMismatch {
                to_input_id,
                a_params: a,
                b_params: b,
            }) => {
                assert_eq!(to_input_id, "to-leg");
                assert_eq!(a, a_params);
                assert_eq!(b.channel_config, 6);
            }
            other => panic!("expected CodecParamMismatch, got {other:?}"),
        }
    }

    #[test]
    fn sentinel_commits_latm_match() {
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        let a = AacAudioParams { profile: 1, sample_rate_idx: 3, channel_config: 2 };
        assert!(s.arm(
            "to".into(),
            0x11,
            90_000,
            now,
            Duration::from_millis(200),
            Some(a),
        ));
        let b_pkt = pkt_with_pts_and_latm(91_920, true, 2, 3, 2);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 91_920 })
        ));
    }

    #[test]
    fn sentinel_skipped_for_non_aac_stream_type() {
        // MP2 audio: frame-duration table supports the splice, but the
        // codec sentinel is ADTS-only today. Even if expected_aac_params
        // is somehow populated, an MP2 slot must commit on PTS alone.
        let mut s = AudioSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x04, // MP2
            90_000,
            now,
            Duration::from_millis(200),
            Some(aac_lc_stereo_48k()), // would mismatch any B
        ));
        let b_pkt = pkt_with_pts_and_adts(92_160, true, 0, 4, 6);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { .. })
        ));
    }

    // ── Video splice — IDR-aware boundary detector + state machine ──

    /// Build a PUSI=1 TS packet carrying a video PES (stream_id `0xE0`)
    /// with PTS-only header followed by a NAL stream. `nals` is a list
    /// of `(start_code_size, nal_header_byte)` describing Annex-B
    /// boundaries to lay into the packet. Returns the synthesised TS
    /// packet padded to 188 B with zeros.
    fn pkt_with_video_pes(pts: u64, pusi: bool, nals: &[(u8, u8)]) -> [u8; 188] {
        let mut p = [0u8; 188];
        p[0] = 0x47;
        p[1] = if pusi { 0x40 } else { 0x00 };
        p[2] = 0x11; // PID lo (0x11 — arbitrary video PID, different from audio fixture)
        p[3] = 0x10; // afc=0b01 payload-only, CC=0
        // PES header at byte 4: start code + stream_id 0xE0 (video).
        p[4] = 0x00;
        p[5] = 0x00;
        p[6] = 0x01;
        p[7] = 0xE0;
        p[8] = 0; // PES_packet_length hi
        p[9] = 0; // PES_packet_length lo
        p[10] = 0x80;
        p[11] = 0x80; // flags2 → PTS only
        p[12] = 5; // PES_header_data_length
        let m: u64 = pts & 0x1_FFFF_FFFF;
        p[13] = 0x20 | (((m >> 30) as u8) & 0x07) << 1 | 1;
        p[14] = ((m >> 22) as u8) & 0xFF;
        p[15] = ((((m >> 15) as u8) & 0x7F) << 1) | 1;
        p[16] = ((m >> 7) as u8) & 0xFF;
        p[17] = (((m as u8) & 0x7F) << 1) | 1;
        // Append NALs starting at byte 18.
        let mut cursor = 18usize;
        for &(sc_size, nal) in nals {
            if cursor >= p.len() {
                break;
            }
            if sc_size == 3 {
                if cursor + 4 > p.len() {
                    break;
                }
                p[cursor] = 0x00;
                p[cursor + 1] = 0x00;
                p[cursor + 2] = 0x01;
                p[cursor + 3] = nal;
                cursor += 4;
            } else {
                if cursor + 5 > p.len() {
                    break;
                }
                p[cursor] = 0x00;
                p[cursor + 1] = 0x00;
                p[cursor + 2] = 0x00;
                p[cursor + 3] = 0x01;
                p[cursor + 4] = nal;
                cursor += 5;
            }
        }
        p
    }

    /// H.264 NAL header byte for `nal_unit_type` (low 5 bits).
    fn avc_nal(nut: u8) -> u8 {
        // forbidden_zero(1) = 0, nal_ref_idc(2) = 11 (highest), nal_unit_type(5)
        0x60 | (nut & 0x1F)
    }

    /// HEVC NAL header byte 0 for `nal_unit_type` (6 bits packed into bits 1..6).
    fn hevc_nal(nut: u8) -> u8 {
        // forbidden_zero(1)=0, nal_unit_type(6), top bit of layer_id(1)=0
        (nut & 0x3F) << 1
    }

    #[test]
    fn video_supported_stream_types() {
        assert!(is_supported_video_stream_type(0x1B));
        assert!(is_supported_video_stream_type(0x24));
        assert!(!is_supported_video_stream_type(0x02)); // MPEG-2 video — out of scope today
        assert!(!is_supported_video_stream_type(0x0F)); // AAC — audio
    }

    #[test]
    fn pes_contains_idr_avc_type_5() {
        // AUD (type 9) → SEI (type 6) → IDR slice (type 5). Mixed 3-/4-byte SCs.
        let nals = [
            (4, avc_nal(9)),
            (3, avc_nal(6)),
            (3, avc_nal(5)),
        ];
        let pkt = pkt_with_video_pes(1_000_000, true, &nals);
        assert!(pes_contains_idr(&pkt, 0x1B));
    }

    #[test]
    fn pes_contains_idr_avc_no_idr() {
        // P-slice only — NAL type 1.
        let nals = [
            (4, avc_nal(9)),
            (3, avc_nal(1)),
        ];
        let pkt = pkt_with_video_pes(1_000_000, true, &nals);
        assert!(!pes_contains_idr(&pkt, 0x1B));
    }

    #[test]
    fn pes_contains_idr_hevc_irap_family() {
        // IDR_W_RADL = 19, IDR_N_LP = 20, CRA_NUT = 21 — every value in
        // 16..=21 must qualify as an IRAP RAP.
        for nut in 16u8..=21 {
            let nals = [(3, hevc_nal(nut))];
            let pkt = pkt_with_video_pes(1_000_000, true, &nals);
            assert!(
                pes_contains_idr(&pkt, 0x24),
                "HEVC NUT {nut} should be IDR-equivalent"
            );
        }
        // Trailing P/B slice (TRAIL_N = 0) is not a RAP.
        let nals = [(3, hevc_nal(0))];
        let pkt = pkt_with_video_pes(1_000_000, true, &nals);
        assert!(!pes_contains_idr(&pkt, 0x24));
    }

    #[test]
    fn pes_contains_idr_rejects_non_video_stream_types() {
        // Even if the bytes look like an IDR start code, the helper
        // short-circuits on stream_type — the AUDIO splice path uses
        // its own AAC sentinel, not this walker.
        let nals = [(4, avc_nal(5))];
        let pkt = pkt_with_video_pes(1_000_000, true, &nals);
        assert!(!pes_contains_idr(&pkt, 0x0F));
        assert!(!pes_contains_idr(&pkt, 0x02));
    }

    #[test]
    fn pes_contains_idr_handles_missing_pes_header() {
        // No PES start code → pes_payload_offset returns None →
        // walker returns false rather than scanning AF bytes.
        let mut p = [0u8; 188];
        p[0] = 0x47;
        p[1] = 0x40;
        p[2] = 0x11;
        p[3] = 0x10;
        // Garbage payload — no 0x000001 start code.
        for byte in &mut p[4..] {
            *byte = 0xAA;
        }
        assert!(!pes_contains_idr(&p, 0x1B));
    }

    /// Helper: arm a video splice without a codec-param baseline
    /// (matches today's MVP path for tests that only exercise PTS+IDR
    /// alignment).
    fn video_arm_no_sentinel(
        s: &mut VideoSpliceState,
        to: &str,
        stream_type: u8,
        last_a_pts: u64,
        now: Instant,
        budget: Duration,
    ) -> bool {
        s.arm(to.to_string(), stream_type, last_a_pts, now, budget, None)
    }

    #[test]
    fn video_arm_rejects_non_video_stream_type() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        // AAC stream type — not a video codec; arm must refuse.
        assert!(!video_arm_no_sentinel(
            &mut s,
            "to",
            0x0F,
            1_000_000,
            now,
            Duration::from_millis(DEFAULT_VIDEO_SPLICE_BUDGET_MS.into()),
        ));
        assert_eq!(s, VideoSpliceState::Idle);
    }

    #[test]
    fn video_commit_on_first_idr_pes_at_threshold() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        // last_a = 90_000, threshold = 93_600 (+ VIDEO_FRAME_DURATION_90K).
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        // B's first PUSI=1 PES at exactly threshold, carrying IDR.
        let nals = [(4, avc_nal(9)), (3, avc_nal(5))];
        let pkt = pkt_with_video_pes(93_600, true, &nals);
        match s.observe_b_packet(&pkt) {
            Some(SpliceOutcome::Committed { first_b_pts }) => {
                assert_eq!(first_b_pts, 93_600);
            }
            other => panic!("expected Committed, got {other:?}"),
        }
        assert_eq!(s, VideoSpliceState::Idle);
    }

    #[test]
    fn video_skips_non_idr_pes_past_threshold() {
        // B's first PUSI=1 PES is past the threshold but is a P-slice.
        // The state machine must keep waiting — committing on a
        // non-RAP would freeze the receiver's decoder on the next
        // anchor frame.
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        let p_slice = pkt_with_video_pes(99_000, true, &[(4, avc_nal(1))]);
        assert!(s.observe_b_packet(&p_slice).is_none());
        assert!(s.is_pending());
        // Next PES carries IDR → commits.
        let idr = pkt_with_video_pes(99_500, true, &[(4, avc_nal(5))]);
        assert!(matches!(
            s.observe_b_packet(&idr),
            Some(SpliceOutcome::Committed { first_b_pts: 99_500 })
        ));
    }

    #[test]
    fn video_skips_idr_below_threshold() {
        // B's first IDR PES is below threshold (B's encoder is behind
        // A's wallclock) — wait, even though the PES would otherwise
        // qualify as a clean RAP.
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        let early = pkt_with_video_pes(90_000, true, &[(4, avc_nal(5))]);
        assert!(s.observe_b_packet(&early).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn video_skips_non_pusi() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        let mid_pes = pkt_with_video_pes(99_999, /* pusi */ false, &[(4, avc_nal(5))]);
        assert!(s.observe_b_packet(&mid_pes).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn video_hevc_commits_on_idr_w_radl() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x24,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        // HEVC IDR_W_RADL = 19.
        let idr = pkt_with_video_pes(93_600, true, &[(3, hevc_nal(19))]);
        assert!(matches!(
            s.observe_b_packet(&idr),
            Some(SpliceOutcome::Committed { first_b_pts: 93_600 })
        ));
    }

    #[test]
    fn video_timeout_emits_once() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to-leg",
            0x1B,
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
        assert!(s.check_timeout(later + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn video_no_timeout_before_deadline() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        assert!(s.check_timeout(now + Duration::from_millis(500)).is_none());
        assert!(s.is_pending());
    }

    #[test]
    fn video_pts_wrap_around_threshold_commits() {
        // last_a near the top of the 33-bit space; threshold wraps
        // past 2^33. A B PES with a small PTS counts as wrapped-ahead.
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        let near_top: u64 = (1u64 << 33) - 1000;
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            near_top,
            now,
            Duration::from_millis(2000),
        ));
        // Threshold wraps to ~2600 (= 3600 - 1000). PTS = 3000 is past.
        let pkt = pkt_with_video_pes(3000, true, &[(4, avc_nal(5))]);
        assert!(matches!(
            s.observe_b_packet(&pkt),
            Some(SpliceOutcome::Committed { .. })
        ));
    }

    #[test]
    fn video_observe_does_nothing_when_idle() {
        let mut s = VideoSpliceState::new();
        let pkt = pkt_with_video_pes(1_000_000, true, &[(4, avc_nal(5))]);
        assert!(s.observe_b_packet(&pkt).is_none());
        assert!(s.check_timeout(Instant::now()).is_none());
        assert_eq!(s.observe_a_packet(&pkt), FromPacketAction::Forward);
    }

    #[test]
    fn video_from_leg_forwarded_then_dropped_at_next_pusi() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        // Continuation packet from A — forward (PUSI=0).
        let cont = pkt_with_video_pes(0, false, &[]);
        assert_eq!(s.observe_a_packet(&cont), FromPacketAction::Forward);
        // A's next PUSI=1 marks A's current AU's end. Packet dropped.
        let next_au = pkt_with_video_pes(93_600, true, &[(4, avc_nal(1))]);
        assert_eq!(s.observe_a_packet(&next_au), FromPacketAction::Drop);
        // Subsequent packets stay dropped.
        assert_eq!(s.observe_a_packet(&cont), FromPacketAction::Drop);
        assert!(s.is_pending());
    }

    // ── Video codec-param sentinel ───────────────────────────────

    /// Encode an Exp-Golomb unsigned `ue(v)` into a bit vector.
    fn push_ue(bits: &mut Vec<u8>, k: u32) {
        if k == 0 {
            bits.push(1);
            return;
        }
        let v = k + 1;
        let n_bits = 32 - v.leading_zeros();
        for _ in 0..(n_bits - 1) {
            bits.push(0);
        }
        bits.push(1);
        for i in (0..(n_bits - 1)).rev() {
            bits.push(((v >> i) & 1) as u8);
        }
    }

    /// Pack a bit vector (MSB-first per byte) into a byte stream,
    /// zero-padding any trailing partial byte.
    fn pack_bits(bits: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        let mut cur = 0u8;
        let mut nb = 0u32;
        for &b in bits {
            cur = (cur << 1) | (b & 1);
            nb += 1;
            if nb == 8 {
                out.push(cur);
                cur = 0;
                nb = 0;
            }
        }
        if nb > 0 {
            cur <<= 8 - nb;
            out.push(cur);
        }
        out
    }

    /// Build a minimal H.264 SPS RBSP for the test parser. Generates
    /// the fixed pre-amble + Exp-Golomb-encoded body covering exactly
    /// the fields [`parse_avc_sps_params`] reads. Returns the RBSP
    /// (post-NAL-header) bytes. For `extended_profile` values
    /// (100, 110, …) the optional chroma + bit-depth fields are
    /// included; for baseline/main/extended profiles (66/77/88) those
    /// fields are skipped and the parser defaults to 4:2:0 8-bit.
    fn build_avc_sps_rbsp(
        profile_idc: u8,
        level_idc: u8,
        chroma_format_idc: u8,
        bit_depth_luma_minus8: u8,
        bit_depth_chroma_minus8: u8,
        w_mbs_minus1: u32,
        h_map_units_minus1: u32,
        frame_mbs_only_flag: u8,
    ) -> Vec<u8> {
        let mut bits: Vec<u8> = vec![];
        push_ue(&mut bits, 0); // seq_parameter_set_id
        let is_extended = matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        );
        if is_extended {
            push_ue(&mut bits, chroma_format_idc as u32);
            if chroma_format_idc == 3 {
                bits.push(0); // separate_colour_plane_flag
            }
            push_ue(&mut bits, bit_depth_luma_minus8 as u32);
            push_ue(&mut bits, bit_depth_chroma_minus8 as u32);
            bits.push(0); // qpprime_y_zero_transform_bypass_flag
            bits.push(0); // seq_scaling_matrix_present_flag = 0
        }
        push_ue(&mut bits, 0); // log2_max_frame_num_minus4
        push_ue(&mut bits, 0); // pic_order_cnt_type
        push_ue(&mut bits, 0); // log2_max_pic_order_cnt_lsb_minus4
        push_ue(&mut bits, 1); // max_num_ref_frames
        bits.push(0); // gaps_in_frame_num_value_allowed_flag
        push_ue(&mut bits, w_mbs_minus1);
        push_ue(&mut bits, h_map_units_minus1);
        bits.push(frame_mbs_only_flag & 1);

        let mut out: Vec<u8> = vec![profile_idc, 0x00, level_idc];
        out.extend(pack_bits(&bits));
        out
    }

    /// Build a PUSI=1 video PES TS packet carrying AUD + SPS + IDR
    /// slice NAL units. Used by the codec-param sentinel tests to
    /// drive `extract_video_params_from_pes` + `observe_b_packet`
    /// together. SPS RBSP is the byte sequence produced by
    /// [`build_avc_sps_rbsp`].
    fn pkt_with_avc_idr_pes(pts: u64, sps_rbsp: &[u8]) -> [u8; 188] {
        let mut payload: Vec<u8> = vec![];
        // AUD: start code + NAL header (type 9, nal_ref_idc=0) + primary_pic_type
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09, 0x10]);
        // SPS: start code + NAL header (type 7, nal_ref_idc=3) + rbsp
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67]);
        payload.extend_from_slice(sps_rbsp);
        // IDR slice: start code + NAL header (type 5, nal_ref_idc=3)
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);

        let mut p = [0u8; 188];
        p[0] = 0x47;
        p[1] = 0x40; // PUSI=1
        p[2] = 0x11;
        p[3] = 0x10;
        p[4] = 0x00;
        p[5] = 0x00;
        p[6] = 0x01;
        p[7] = 0xE0;
        p[8] = 0;
        p[9] = 0;
        p[10] = 0x80;
        p[11] = 0x80;
        p[12] = 5;
        let m: u64 = pts & 0x1_FFFF_FFFF;
        p[13] = 0x20 | (((m >> 30) as u8) & 0x07) << 1 | 1;
        p[14] = ((m >> 22) as u8) & 0xFF;
        p[15] = ((((m >> 15) as u8) & 0x7F) << 1) | 1;
        p[16] = ((m >> 7) as u8) & 0xFF;
        p[17] = (((m as u8) & 0x7F) << 1) | 1;
        let cap = p.len() - 18;
        let n = payload.len().min(cap);
        p[18..18 + n].copy_from_slice(&payload[..n]);
        p
    }

    fn avc_baseline_320x240() -> VideoCodecParams {
        VideoCodecParams {
            codec: VideoCodec::Avc,
            profile_idc: 66,
            level_idc: 30,
            chroma_format_idc: 1,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            width: 320,
            height: 240,
        }
    }

    #[test]
    fn parse_avc_sps_baseline_round_trip() {
        // 320×240 baseline (profile 66, level 3.0). Verifies the parser
        // walks ue() encoded values correctly and computes width/height
        // from pic_*_in_mbs values.
        let rbsp = build_avc_sps_rbsp(66, 30, 1, 0, 0, 19, 14, 1);
        let params = parse_avc_sps_params(&rbsp).expect("parse");
        assert_eq!(params, avc_baseline_320x240());
    }

    #[test]
    fn parse_avc_sps_high_profile_10bit() {
        // High10 profile (110), Main 5.0 level (50), 4:2:0 chroma,
        // 10-bit luma + chroma, 1920×1088 coded (1080 displayed but
        // SPS rounds to nearest 16-mb boundary).
        let rbsp = build_avc_sps_rbsp(110, 50, 1, 2, 2, 119, 67, 1);
        let params = parse_avc_sps_params(&rbsp).expect("parse");
        assert_eq!(params.codec, VideoCodec::Avc);
        assert_eq!(params.profile_idc, 110);
        assert_eq!(params.level_idc, 50);
        assert_eq!(params.bit_depth_luma, 10);
        assert_eq!(params.bit_depth_chroma, 10);
        assert_eq!(params.width, 1920);
        assert_eq!(params.height, 1088);
    }

    #[test]
    fn parse_avc_sps_too_short() {
        assert!(parse_avc_sps_params(&[0x42, 0x00]).is_none());
    }

    #[test]
    fn extract_video_params_from_avc_idr_pes() {
        let rbsp = build_avc_sps_rbsp(66, 30, 1, 0, 0, 19, 14, 1);
        let pkt = pkt_with_avc_idr_pes(1_000_000, &rbsp);
        let params = extract_video_params_from_pes(&pkt, 0x1B).expect("extract");
        assert_eq!(params, avc_baseline_320x240());
    }

    #[test]
    fn extract_video_params_skips_non_pusi() {
        // Even a packet carrying a perfect SPS shouldn't yield params
        // when PUSI=0 (mid-PES bytes — the SPS is part of an earlier
        // PES and was already snapshotted from its own PUSI packet).
        let rbsp = build_avc_sps_rbsp(66, 30, 1, 0, 0, 19, 14, 1);
        let mut pkt = pkt_with_avc_idr_pes(1_000_000, &rbsp);
        pkt[1] = 0x00; // clear PUSI
        assert!(extract_video_params_from_pes(&pkt, 0x1B).is_none());
    }

    #[test]
    fn extract_video_params_no_sps_in_payload() {
        // P-slice only PES: no SPS NAL → parser returns None and the
        // sentinel falls through to PTS-only commit.
        let nals = [(4, avc_nal(9)), (3, avc_nal(1))];
        let pkt = pkt_with_video_pes(1_000_000, true, &nals);
        assert!(extract_video_params_from_pes(&pkt, 0x1B).is_none());
    }

    #[test]
    fn record_a_video_params_captures_sps() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        let rbsp = build_avc_sps_rbsp(66, 30, 1, 0, 0, 19, 14, 1);
        let a_pkt = pkt_with_avc_idr_pes(89_000, &rbsp);
        s.record_a_video_params(&a_pkt);
        match &s {
            VideoSpliceState::Pending { expected_params, .. } => {
                assert_eq!(*expected_params, Some(avc_baseline_320x240()));
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn record_a_video_params_noop_after_au_completed() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        // Drive A's next PUSI=1 → AU completed.
        let pusi = pkt_with_video_pes(91_000, true, &[(4, avc_nal(1))]);
        assert_eq!(s.observe_a_packet(&pusi), FromPacketAction::Drop);
        // Subsequent record attempts are no-ops.
        let rbsp = build_avc_sps_rbsp(66, 30, 1, 0, 0, 19, 14, 1);
        let later = pkt_with_avc_idr_pes(93_000, &rbsp);
        s.record_a_video_params(&later);
        match &s {
            VideoSpliceState::Pending { expected_params, .. } => {
                assert_eq!(*expected_params, None);
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn video_sentinel_commits_when_params_match() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
            Some(avc_baseline_320x240()),
        ));
        let rbsp = build_avc_sps_rbsp(66, 30, 1, 0, 0, 19, 14, 1);
        let b_pkt = pkt_with_avc_idr_pes(93_600, &rbsp);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 93_600 })
        ));
        assert!(!s.is_pending());
    }

    #[test]
    fn video_sentinel_rejects_on_resolution_change() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to-leg".into(),
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
            Some(avc_baseline_320x240()),
        ));
        // B is 640×480: pic_width_in_mbs_minus1 = 39, h_map_units_minus1 = 29.
        let rbsp = build_avc_sps_rbsp(66, 30, 1, 0, 0, 39, 29, 1);
        let b_pkt = pkt_with_avc_idr_pes(93_600, &rbsp);
        match s.observe_b_packet(&b_pkt) {
            Some(SpliceOutcome::VideoCodecParamMismatch {
                to_input_id,
                a_params,
                b_params,
            }) => {
                assert_eq!(to_input_id, "to-leg");
                assert_eq!(a_params.width, 320);
                assert_eq!(b_params.width, 640);
                assert_eq!(b_params.height, 480);
            }
            other => panic!("expected VideoCodecParamMismatch, got {other:?}"),
        }
        assert!(!s.is_pending());
    }

    #[test]
    fn video_sentinel_rejects_on_profile_change() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
            Some(avc_baseline_320x240()),
        ));
        // B is Main profile (77) instead of Baseline (66).
        let rbsp = build_avc_sps_rbsp(77, 30, 1, 0, 0, 19, 14, 1);
        let b_pkt = pkt_with_avc_idr_pes(93_600, &rbsp);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::VideoCodecParamMismatch { .. })
        ));
    }

    #[test]
    fn video_sentinel_rejects_on_bit_depth_change() {
        // A is High10 10-bit; B is High10 8-bit. Bit-depth mismatch
        // forces a decoder re-init.
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        let a = VideoCodecParams {
            codec: VideoCodec::Avc,
            profile_idc: 110,
            level_idc: 50,
            chroma_format_idc: 1,
            bit_depth_luma: 10,
            bit_depth_chroma: 10,
            width: 1920,
            height: 1088,
        };
        assert!(s.arm(
            "to".into(),
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
            Some(a),
        ));
        // B at 8-bit.
        let rbsp = build_avc_sps_rbsp(110, 50, 1, 0, 0, 119, 67, 1);
        let b_pkt = pkt_with_avc_idr_pes(93_600, &rbsp);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::VideoCodecParamMismatch { .. })
        ));
    }

    #[test]
    fn video_sentinel_falls_through_when_a_params_unknown() {
        // No A baseline → sentinel can't refuse → commit on IDR/PTS alone.
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
            None,
        ));
        // B has a different profile, but sentinel doesn't care because A is unknown.
        let rbsp = build_avc_sps_rbsp(77, 40, 1, 0, 0, 39, 29, 1);
        let b_pkt = pkt_with_avc_idr_pes(93_600, &rbsp);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 93_600 })
        ));
    }

    #[test]
    fn video_sentinel_falls_through_when_b_params_unparseable() {
        // A snapshot present; B's PES doesn't carry an SPS (e.g. some
        // encoders don't emit SPS on every IDR). Sentinel must
        // fail-safe to commit on IDR+PTS alone — refusing on missing
        // data would refuse perfectly compatible splices.
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(s.arm(
            "to".into(),
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
            Some(avc_baseline_320x240()),
        ));
        // IDR slice only — no SPS.
        let b_pkt = pkt_with_video_pes(93_600, true, &[(4, avc_nal(5))]);
        assert!(matches!(
            s.observe_b_packet(&b_pkt),
            Some(SpliceOutcome::Committed { first_b_pts: 93_600 })
        ));
    }

    #[test]
    fn video_commit_resets_to_idle() {
        let mut s = VideoSpliceState::new();
        let now = Instant::now();
        assert!(video_arm_no_sentinel(
            &mut s,
            "to",
            0x1B,
            90_000,
            now,
            Duration::from_millis(2000),
        ));
        let idr = pkt_with_video_pes(93_600, true, &[(4, avc_nal(5))]);
        assert!(matches!(
            s.observe_b_packet(&idr),
            Some(SpliceOutcome::Committed { .. })
        ));
        // observe_a_packet returning Forward when Idle is the safe
        // default for the new active leg (B is now A for next splice).
        let later = pkt_with_video_pes(100_000, true, &[(4, avc_nal(1))]);
        assert_eq!(s.observe_a_packet(&later), FromPacketAction::Forward);
    }
}
