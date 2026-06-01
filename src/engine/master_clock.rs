// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

// Phase 1 lands the abstraction + Wallclock impl ahead of the consumers
// in Phases 3 + 4 + 6. Allow dead_code on this module so the stub items
// don't trip CI before their wiring lands.
#![allow(dead_code)]

//! Master clock abstraction for broadcast-grade A/V sync.
//!
//! Every flow's output emission is paced against a single per-flow master
//! clock — either an external PTP grandmaster, the source's recovered PCR,
//! an ALSA audio master, or last-resort wallclock. PCR generation,
//! emission timing, and (in time) cross-edge coherence all bottom out on
//! [`MasterClock::now_27mhz`].
//!
//! ## Selection policy
//!
//! [`select_master_kind_for_input`] picks the [`MasterClockKind`] for a
//! flow at start-up, from the type of the input that *drives* the master
//! clock — the active input for a passthrough flow, or the designated
//! `pcr_source` input ([`resolve_pcr_source_input_id`]) for a PID-bus
//! assembled flow. Rows are evaluated in priority order:
//!
//! | Driving input / flow shape                       | Master                     |
//! |--------------------------------------------------|----------------------------|
//! | ST 2110-20/-23/-30/-31/-40, MXL                  | `Ptp`                      |
//! | WebRTC / WHEP ingress                            | `Wallclock` + warn event   |
//! | Flow has `assembly` set (PID-bus SPTS/MPTS)      | `SourcePcrPll`             |
//! | SRT / RTP / UDP / RIST / RTMP / RTSP carrying TS | `Wallclock` (PLL opt-in)   |
//! | media_player / test_pattern / replay / rtp_audio | `Wallclock`                |
//! | bonded / none active                             | `Wallclock`                |
//!
//! Assembled flows default to `SourcePcrPll` because the assembler
//! generates fresh output PCR from [`MasterClock::now_27mhz`] and needs a
//! recovered source clock to keep it coherent. Plain contribution flows
//! default to `Wallclock` and only run the source-PCR PLL when an operator
//! opts in per-flow via `master_clock.kind = "contribution"` (preferred,
//! surfaces intent on telemetry) or the legacy `"source_pcr_pll"`. The
//! former PLL-everywhere default was dropped because internet contribution's
//! ms-scale PCR-arrival jitter tripped spurious fallback alarms on flows
//! whose output A/V sync was unaffected.
//!
//! Operators can pin a master per-flow via
//! [`crate::config::models::FlowConfig::master_clock`]; that override beats
//! the auto-selection policy.
//!
//! ## Design constraints
//!
//! - **Lock-free read.** [`now_27mhz`](MasterClock::now_27mhz) runs on the
//!   data path of every PCR-bearing emit. Implementations read a single
//!   atomic / parking-lot RwLock and never block.
//! - **Stable identifier.** [`source_id`](MasterClock::source_id) is a
//!   short string suitable for telemetry events; doesn't change after
//!   construction.
//! - **Lock-state gate.** Outputs that need a locked master before
//!   emitting PCR (every PID-bus assembler and every transcoding output)
//!   gate on [`is_locked`](MasterClock::is_locked); outputs that can run
//!   open-loop (passthrough) ignore it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Trait every master-clock implementation provides.
///
/// Cheap to clone (always `Arc<dyn MasterClock>`). Thread-safe; called
/// from every output's data path.
pub trait MasterClock: Send + Sync + 'static {
    /// Current master-clock value in 27 MHz ticks. Wraps modulo 2^33 × 300
    /// (the full MPEG-TS PCR modulus).
    fn now_27mhz(&self) -> u64;

    /// Stable identifier for telemetry / events. Examples:
    /// - `"wallclock"`
    /// - `"source_pcr"`
    /// - `"ptp:domain=0:gm=de:ad:be:ef:..."`
    fn source_id(&self) -> &str;

    /// True once the clock has converged. Outputs that need broadcast-
    /// grade timing gate PCR emission on this. `Wallclock` always
    /// returns `true`.
    fn is_locked(&self) -> bool;

    /// Operator-set lipsync offset in 90 kHz ticks. Positive = audio
    /// behind video (delay audio); negative = audio ahead of video
    /// (delay video). Bounded ±200 ms (±18 000 ticks). Default 0.
    fn lipsync_offset_90k(&self) -> i64 {
        0
    }

    /// Telemetry snapshot: lock state, recovered rate offset (ppm),
    /// recent jitter (µs). Cheap; called once per stats tick (1 Hz).
    fn telemetry(&self) -> MasterClockTelemetry {
        MasterClockTelemetry {
            kind: self.source_id().to_string(),
            locked: self.is_locked(),
            rate_offset_ppm: 0.0,
            jitter_us: 0,
            configured_kind: None,
            fallback_active: false,
            fallback_reason: None,
            active_input_id: None,
            rate_source: None,
        }
    }
}

/// Tagged enum describing which master a flow is using. Carried on the
/// flow's `MasterClockHandle` so telemetry can render the right label
/// without a `dyn`-cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterClockKind {
    /// Source-recovered PCR via [`crate::engine::pcr_pll::PcrPll`].
    SourcePcrPll,
    /// External PTP grandmaster via the `ptp4l` socket reader.
    Ptp,
    /// ALSA audio master from the local-display output.
    AudioMaster,
    /// **By-design** wallclock: this flow doesn't need a recovered
    /// source clock because output PCR comes from the source bytes
    /// directly (passthrough) or from source PTS (transcoded paths
    /// via [`crate::engine::av_sync_mux::pcr_for_emit`]). The runtime
    /// backing is identical to [`Self::Wallclock`] — what's different
    /// is that no PLL is spawned, no fallback watcher runs, and no
    /// "fallback" alarm ever fires. Telemetry reports `kind:
    /// "passthrough"` so the operator sees an intentional state, not
    /// a degraded one. Auto-selected by [`select_master_kind_for_input`]
    /// for typical contribution-to-distribution flows.
    Passthrough,
    /// Sender-timestamp recovery for SRT / RIST inputs. Uses the
    /// transport's per-packet sender timestamp (libsrt's
    /// `SRT_MsgCtrl::srctime` — surfaced on `RtpPacket.sender_timestamp_us`
    /// by `input_srt`) as the rate reference, with MPEG-TS PCR from the
    /// bytes as a per-packet fallback when the sender didn't propagate
    /// srctime. Cleaner under wide-area network jitter because the
    /// timestamp is set at the sender's `sendmsg()` — pre-network-
    /// jitter — and is not subject to the bursty arrival cadence that
    /// PCR-from-bytes sees behind the TSBPD latency buffer.
    ///
    /// Runtime is identical to [`Self::SourcePcrPll`] (same `PcrPll`
    /// recovers rate from any monotonic-clock sample stream); what
    /// differs is the sampler's per-packet decision rule (prefer
    /// srctime, fall back to PCR). Telemetry exposes which source
    /// actually fed the most recent sample via `rate_source`.
    SenderTimestamp,
    /// Last-resort monotonic wall clock. Loud telemetry warning.
    /// Distinct from [`Self::Passthrough`] semantically: this is the
    /// PLL's fallback path (we *wanted* a source clock and didn't get
    /// one), while `Passthrough` is the auto-selected "this flow
    /// doesn't need a master clock" choice.
    Wallclock,
}

impl MasterClockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourcePcrPll => "source_pcr_pll",
            Self::Ptp => "ptp",
            Self::AudioMaster => "audio_master",
            Self::Passthrough => "passthrough",
            Self::SenderTimestamp => "sender_timestamp",
            Self::Wallclock => "wallclock",
        }
    }
}

/// Snapshot of master-clock health surfaced on `FlowStats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterClockTelemetry {
    /// Tagged kind ("wallclock", "source_pcr_pll", "ptp", "audio_master").
    /// When PLL fallback has fired, this reports the **actual** kind
    /// currently driving (e.g. "wallclock") while `configured_kind`
    /// preserves what the operator requested.
    pub kind: String,
    /// True when the clock is converged enough for broadcast-grade emit.
    pub locked: bool,
    /// Recovered rate vs. local CPU clock, in ppm. Only meaningful for
    /// PLL-style masters; `0.0` for Wallclock + Ptp.
    pub rate_offset_ppm: f64,
    /// Recent jitter in microseconds (p99 over last 1 s window).
    pub jitter_us: u64,
    /// What the operator (or auto-policy) configured, regardless of
    /// whether a fallback has since fired. `None` when the field is
    /// not relevant (e.g. on bare wallclock flows that never had a
    /// PLL configured). Additive — older managers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_kind: Option<String>,
    /// `true` when the PLL→Wallclock fallback has fired on this
    /// master. UI can render a "running degraded" chip.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fallback_active: bool,
    /// Human-readable reason the fallback fired
    /// ("insufficient_samples" / "jitter_too_high" / "no_pcr_observed").
    /// Set only when `fallback_active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// ID of the input currently feeding the clock. For
    /// `source_pcr_pll` this is the active input whose PCR samples the
    /// PLL is tracking (refreshes on operator-driven input switches
    /// without re-creating the master). `None` for clock kinds that
    /// don't take their rate from a single ingress stream
    /// (`ptp`, bare `wallclock`). Additive — older managers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_input_id: Option<String>,
    /// Which rate-sample source last fed the PLL. `"sender_timestamp"`
    /// for SRT/RIST inputs delivering libsrt's `SRT_MsgCtrl::srctime`,
    /// `"pcr"` for MPEG-TS PCR sampled from the adaptation field,
    /// `"none"` before the first sample lands. Additive — older
    /// managers ignore it; only meaningful for `source_pcr_pll`-class
    /// clocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_source: Option<String>,
}

#[inline]
fn is_false(b: &bool) -> bool { !*b }

impl Default for MasterClockTelemetry {
    fn default() -> Self {
        Self {
            kind: "wallclock".into(),
            locked: true,
            rate_offset_ppm: 0.0,
            jitter_us: 0,
            configured_kind: None,
            fallback_active: false,
            fallback_reason: None,
            active_input_id: None,
            rate_source: None,
        }
    }
}

/// Rate-sample source feeding the PLL. Surfaced on telemetry so
/// operators can tell whether the master clock is recovering from
/// SRT/RIST sender timestamps (cleaner — set at sendmsg() time, pre-
/// network-jitter) or MPEG-TS PCR sampled from the bytes (subject to
/// the bursty arrival cadence behind a 200 ms+ latency buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateSampleSource {
    /// No samples have reached the PLL yet (startup state).
    None,
    /// Most recent sample was from `pkt.sender_timestamp_us`.
    SenderTimestamp,
    /// Most recent sample was from an MPEG-TS adaptation-field PCR.
    Pcr,
}

impl RateSampleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SenderTimestamp => "sender_timestamp",
            Self::Pcr => "pcr",
        }
    }
}

/// Source-PCR-PLL master: wraps [`crate::engine::pcr_pll::PcrPll`] behind
/// the [`MasterClock`] trait. Ingress samples are fed from the per-flow
/// PCR ingress sampler (see `flow.rs`'s `spawn_pcr_ingress_sampler`).
pub struct SourcePcrPllMaster {
    pll: Arc<crate::engine::pcr_pll::PcrPll>,
    /// Process-monotonic epoch — both `record_sample` (sampler side) and
    /// `now_27mhz` (output side) reference this same anchor so the
    /// `wall_ns` values land on the same clock.
    epoch: Instant,
    source_id: String,
    /// Latest sample source — lock-free atomic so `telemetry()` can
    /// read without coordinating with the sampler thread. Stored as
    /// `u8` to fit in an atomic; `RateSampleSource::as_str` decodes.
    rate_source: std::sync::atomic::AtomicU8,
}

impl SourcePcrPllMaster {
    pub fn new(source_id: impl Into<String>) -> Self {
        Self::new_with_config(source_id, crate::engine::pcr_pll::PcrPllConfig::default())
    }

    /// Build a master with custom PLL parameters. Used by
    /// `flow.rs::build_master_clock` to thread the per-flow
    /// `master_clock.pll_lock_jitter_us` operator override into the
    /// PI-controller's lock criterion. All other parameters take their
    /// defaults (see [`crate::engine::pcr_pll::PcrPllConfig::default`]).
    pub fn new_with_config(
        source_id: impl Into<String>,
        config: crate::engine::pcr_pll::PcrPllConfig,
    ) -> Self {
        Self {
            pll: Arc::new(crate::engine::pcr_pll::PcrPll::new(config)),
            epoch: Instant::now(),
            source_id: source_id.into(),
            rate_source: std::sync::atomic::AtomicU8::new(RateSampleSource::None as u8),
        }
    }

    pub fn with_pll(
        pll: Arc<crate::engine::pcr_pll::PcrPll>,
        epoch: Instant,
        source_id: impl Into<String>,
    ) -> Self {
        Self {
            pll,
            epoch,
            source_id: source_id.into(),
            rate_source: std::sync::atomic::AtomicU8::new(RateSampleSource::None as u8),
        }
    }

    /// Lock-free read of the latest rate-sample source. Exposed on
    /// `MasterClockTelemetry.rate_source`.
    pub fn rate_source(&self) -> RateSampleSource {
        match self.rate_source.load(std::sync::atomic::Ordering::Relaxed) {
            x if x == RateSampleSource::SenderTimestamp as u8 => {
                RateSampleSource::SenderTimestamp
            }
            x if x == RateSampleSource::Pcr as u8 => RateSampleSource::Pcr,
            _ => RateSampleSource::None,
        }
    }

    /// Feed a sample taken from the SRT/RIST sender's `srctime`
    /// (microseconds since Unix epoch on the sender's wallclock).
    /// Bypasses the PCR-from-bytes path. Cleaner under wide-area
    /// network jitter — the timestamp is set at the sender's
    /// `sendmsg()`, pre-TSBPD-buffering.
    ///
    /// **Caveat**: this recovers the sender's *wallclock* rate, not
    /// the source's media-clock rate. For production encoders these
    /// are typically the same crystal (encoder reads one clock for
    /// both PTS generation and `sendmsg` pacing). For ffmpeg `-re`
    /// reading a file, sender wallclock and media clock diverge by
    /// whatever the host's CPU clock vs the file's encoded PCR rate
    /// is — typically a few ppm, harmless in practice.
    pub fn record_sender_timestamp(&self, srctime_us: i64, recv_time_us: u64) {
        // Convert µs → 27 MHz tick equivalent (×27). This makes the
        // PLL's recovered_rate dimensionally identical to the PCR-from-
        // bytes path and lets the rest of the data plane consume
        // `now_27mhz()` without caring which source fed the PLL.
        let pseudo_27mhz = (srctime_us as i128 * 27) as u64;
        let wall_ns = (recv_time_us as u128) * 1000;
        self.pll.record_sample(pseudo_27mhz, wall_ns);
        self.rate_source.store(
            RateSampleSource::SenderTimestamp as u8,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn pll(&self) -> Arc<crate::engine::pcr_pll::PcrPll> {
        self.pll.clone()
    }

    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    /// Convenience: feed a PCR sample using the master's process epoch.
    /// **Avoid for ingress-sampler use** — `wall_ns` is sampled at the
    /// time of THIS call, which means broadcast-channel scheduler
    /// latency / batch draining gets baked into the measured rate.
    /// The PCR ingress sampler should use [`record_sample_at`]
    /// instead, passing the original `RtpPacket::recv_time_us`
    /// captured at UDP recv() return — that's a true kernel-delivery
    /// timestamp, not a "whenever-the-task-got-around-to-it" one.
    pub fn record_sample(&self, pcr_27mhz: u64) {
        let wall_ns = self.epoch.elapsed().as_nanos();
        self.pll.record_sample(pcr_27mhz, wall_ns);
    }

    /// Feed a PCR sample with the wallclock timestamp captured at the
    /// time the source datagram was received from the kernel
    /// (`RtpPacket::recv_time_us`). This is the correct ingress
    /// timestamp for PLL rate tracking — using fresh wall reads at
    /// PCR-extraction time bakes broadcast-subscriber scheduling
    /// jitter into `Δwall_ns` and forces the PLL to perceive the
    /// source as drifting hundreds of ppm even when it's clean.
    ///
    /// `recv_time_us` is in `util::time::now_us()` units (process
    /// monotonic, microseconds). This master converts to ns and feeds
    /// the PLL directly — the PLL is epoch-agnostic, only Δs matter.
    pub fn record_sample_at(&self, pcr_27mhz: u64, recv_time_us: u64) {
        let wall_ns = (recv_time_us as u128) * 1000;
        self.pll.record_sample(pcr_27mhz, wall_ns);
        self.rate_source.store(
            RateSampleSource::Pcr as u8,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Reset the underlying PLL's anchor + lock state. Called on
    /// active-input switches so the new stream's PCR base re-anchors
    /// cleanly rather than tripping the inner discontinuity filter.
    pub fn reset_anchor(&self) {
        self.pll.reset_anchor();
    }
}

impl MasterClock for SourcePcrPllMaster {
    fn now_27mhz(&self) -> u64 {
        let wall_ns = self.epoch.elapsed().as_nanos();
        self.pll.now_27mhz(wall_ns)
    }

    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn is_locked(&self) -> bool {
        self.pll.is_locked()
    }

    fn telemetry(&self) -> MasterClockTelemetry {
        let t = self.pll.telemetry();
        MasterClockTelemetry {
            kind: MasterClockKind::SourcePcrPll.as_str().to_string(),
            locked: t.locked,
            rate_offset_ppm: t.rate_offset_ppm,
            jitter_us: t.jitter_us,
            configured_kind: None,
            fallback_active: false,
            fallback_reason: None,
            active_input_id: None,
            rate_source: Some(self.rate_source().as_str().to_string()),
        }
    }
}

/// PTP master: drives PCR off the local realtime clock (which `ptp4l +
/// phc2sys` slave to the grandmaster) and gates lock state on the PTP
/// reporter's `PtpLockState`.
///
/// Cross-edge coherence: two edges anchored to the same grandmaster
/// produce identical `now_27mhz()` values within the PTP servo's
/// residual offset (typically < 1 µs on a well-tuned plant). Multiple
/// edges feeding a 2022-7 hitless receiver therefore emit
/// PCR-equivalent streams without any external genlock.
pub struct PtpMasterClock {
    state: Arc<crate::engine::st2110::ptp::PtpStateHandle>,
    source_id: String,
}

impl PtpMasterClock {
    pub fn new(state: Arc<crate::engine::st2110::ptp::PtpStateHandle>) -> Self {
        let snap = state.snapshot();
        let source_id = match snap.grandmaster_id {
            Some(gm) => format!("ptp:domain={}:gm={gm}", snap.domain),
            None => format!("ptp:domain={}", snap.domain),
        };
        Self { state, source_id }
    }
}

impl MasterClock for PtpMasterClock {
    fn now_27mhz(&self) -> u64 {
        // Local realtime clock — when ptp4l + phc2sys are running, this
        // is the PTP grandmaster's clock within the servo's offset
        // tolerance. SystemTime can go backwards across leap seconds /
        // NTP corrections, but on a PTP-synced host phc2sys keeps the
        // monotonic relationship intact.
        let dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let ns = dur.as_nanos();
        let ticks = (ns as u64).saturating_mul(27) / 1000;
        const PCR_MODULUS: u64 = (1u64 << 33) * 300;
        ticks % PCR_MODULUS
    }

    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn is_locked(&self) -> bool {
        use crate::engine::st2110::ptp::PtpLockState;
        matches!(
            self.state.snapshot().lock_state,
            PtpLockState::Locked | PtpLockState::Master
        )
    }

    fn telemetry(&self) -> MasterClockTelemetry {
        let snap = self.state.snapshot();
        let locked = snap.lock_state.is_healthy();
        MasterClockTelemetry {
            kind: MasterClockKind::Ptp.as_str().to_string(),
            locked,
            rate_offset_ppm: 0.0,
            jitter_us: snap
                .offset_ns
                .map(|o| (o.unsigned_abs() / 1000) as u64)
                .unwrap_or(0),
            configured_kind: None,
            fallback_active: false,
            fallback_reason: None,
            active_input_id: None,
            rate_source: None,
        }
    }
}

/// Last-resort master: monotonic wall clock pinned at flow start.
///
/// `now_27mhz()` returns `(Instant::now() − epoch).as_nanos() × 27 / 1000`
/// modulo the PCR space. Always reports `is_locked = true` so flows that
/// fall back to wallclock still produce PCR — but the manager UI surfaces
/// the `wallclock` kind so operators know they're in degraded mode.
pub struct WallclockMaster {
    epoch: Instant,
    /// Random offset folded into now() so two parallel edges that land
    /// on `Wallclock` don't accidentally appear coherent.
    offset_27mhz: u64,
    source_id: String,
    lipsync_offset_90k: AtomicU64,
}

impl WallclockMaster {
    pub fn new() -> Self {
        // Random offset so wallclock-master flows on different edges
        // never claim coherence.
        let offset_27mhz = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            std::time::SystemTime::now().hash(&mut h);
            std::process::id().hash(&mut h);
            (h.finish() & ((1u64 << 33) - 1)) * 300
        };
        Self {
            epoch: Instant::now(),
            offset_27mhz,
            source_id: "wallclock".to_string(),
            lipsync_offset_90k: AtomicU64::new(0),
        }
    }

    /// Bias the lipsync offset (90 kHz ticks). Bounded ±200 ms.
    pub fn set_lipsync_offset_90k(&self, ticks: i64) {
        let clamped = ticks.clamp(-18_000, 18_000);
        self.lipsync_offset_90k.store(clamped as u64, Ordering::Relaxed);
    }
}

impl Default for WallclockMaster {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterClock for WallclockMaster {
    fn now_27mhz(&self) -> u64 {
        let elapsed_ns = self.epoch.elapsed().as_nanos();
        // 27 MHz = 27 ticks per microsecond = 27/1000 ticks per ns.
        let elapsed_ticks = (elapsed_ns as u64).saturating_mul(27) / 1000;
        const PCR_MODULUS: u64 = (1u64 << 33) * 300;
        (self.offset_27mhz + elapsed_ticks) % PCR_MODULUS
    }

    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn is_locked(&self) -> bool {
        true
    }

    fn lipsync_offset_90k(&self) -> i64 {
        self.lipsync_offset_90k.load(Ordering::Relaxed) as i64
    }

    fn telemetry(&self) -> MasterClockTelemetry {
        MasterClockTelemetry {
            kind: MasterClockKind::Wallclock.as_str().to_string(),
            locked: true,
            rate_offset_ppm: 0.0,
            jitter_us: 0,
            configured_kind: None,
            fallback_active: false,
            fallback_reason: None,
            active_input_id: None,
            rate_source: None,
        }
    }
}

/// PLL-with-wallclock-fallback wrapper.
///
/// Wraps a [`SourcePcrPllMaster`] and a [`WallclockMaster`] behind a
/// single dyn-`MasterClock` façade. A watcher task spawned at flow
/// start monitors the PLL's lock state; if the PLL fails to lock
/// within the configured timeout (or sees zero PCR samples after a
/// shorter grace window), the watcher flips `fallback_active` and
/// `now_27mhz()` switches to driving the output from the wallclock
/// master instead. The transition is one-shot — once fallen back,
/// the master stays on wallclock for the flow's lifetime (no
/// oscillation).
///
/// The configured kind is preserved on telemetry so the manager UI
/// can render "PCR PLL → Wallclock (fallback)" rather than silently
/// re-labelling the flow's clock as wallclock.
pub struct PcrPllWithFallback {
    pll_master: Arc<SourcePcrPllMaster>,
    wallclock: WallclockMaster,
    /// Optional middle fallback rung for the `auto` cascade
    /// (PLL → PTP → Wallclock). When `Some` and locked at fallback-fire
    /// time, the master falls to PTP instead of straight to wallclock; if
    /// PTP later loses lock it demotes one-way to wallclock (never
    /// re-promotes — avoids emitting PCR off an unlocked CLOCK_REALTIME
    /// and avoids epoch oscillation if PTP flaps). `None` = classic
    /// PLL → Wallclock wrapper, behaviourally unchanged.
    ptp_fallback: Option<Arc<dyn MasterClock>>,
    fallback_active: AtomicBool,
    /// When in fallback: true = currently driving from the PTP rung,
    /// false = wallclock. Latched at fallback-fire from PTP health;
    /// demoted to false (one-way) by `now_27mhz` if PTP loses lock.
    fallback_uses_ptp: AtomicBool,
    /// Reason the most recent fallback fired. Cleared on
    /// `deactivate_fallback`. Read by `telemetry()` when the active
    /// flag is set.
    fallback_reason: std::sync::RwLock<Option<String>>,
    /// Operator's configured label for telemetry — `Some("auto")` for the
    /// cascade so the UI renders "Auto → PTP/Wallclock"; `None` for the
    /// classic wrapper (which reports `source_pcr_pll`).
    configured_kind: Option<&'static str>,
    source_id: String,
}

impl PcrPllWithFallback {
    pub fn new(pll_master: Arc<SourcePcrPllMaster>) -> Self {
        let source_id = pll_master.source_id().to_string();
        Self {
            pll_master,
            wallclock: WallclockMaster::new(),
            ptp_fallback: None,
            fallback_active: AtomicBool::new(false),
            fallback_uses_ptp: AtomicBool::new(false),
            fallback_reason: std::sync::RwLock::new(None),
            configured_kind: None,
            source_id,
        }
    }

    /// Cascade constructor for `master_clock.kind = "auto"`: PLL primary,
    /// optional PTP middle rung (`None` when the node has no PTP role
    /// configured → classic PLL → Wallclock), wallclock floor. `label` is
    /// the operator-configured kind surfaced on telemetry (e.g. `"auto"`).
    pub fn new_cascade(
        pll_master: Arc<SourcePcrPllMaster>,
        ptp_fallback: Option<Arc<dyn MasterClock>>,
        label: &'static str,
    ) -> Self {
        let source_id = pll_master.source_id().to_string();
        Self {
            pll_master,
            wallclock: WallclockMaster::new(),
            ptp_fallback,
            fallback_active: AtomicBool::new(false),
            fallback_uses_ptp: AtomicBool::new(false),
            fallback_reason: std::sync::RwLock::new(None),
            configured_kind: Some(label),
            source_id,
        }
    }

    pub fn pll_master(&self) -> Arc<SourcePcrPllMaster> {
        self.pll_master.clone()
    }

    /// Flip the master onto wallclock and record the reason. Re-fireable —
    /// `deactivate_fallback` clears the state so subsequent activate
    /// calls record fresh reasons.
    pub fn activate_fallback(&self, reason: impl Into<String>) {
        // Cascade target decision, latched here so the data path doesn't
        // flap: prefer the PTP rung when present AND locked at this moment;
        // else wallclock. `now_27mhz` only ever demotes PTP → wallclock
        // one-way if PTP later loses lock. For the classic wrapper
        // (`ptp_fallback == None`) this is always false → wallclock, exactly
        // as before.
        let use_ptp = self
            .ptp_fallback
            .as_ref()
            .map_or(false, |p| p.is_locked());
        self.fallback_uses_ptp.store(use_ptp, Ordering::Release);
        // Record reason first so any reader observing `fallback_active`
        // sees a populated reason. `expect` is safe — write lock
        // poisoning would mean a panic already shredded the engine; we
        // run with `RUST_BACKTRACE=full` to surface it.
        if let Ok(mut w) = self.fallback_reason.write() {
            *w = Some(reason.into());
        }
        self.fallback_active.store(true, Ordering::Release);
    }

    /// Flip the master back onto the PLL and clear the recorded reason.
    /// Idempotent. Called by:
    ///  - the watcher when the PLL re-locks (after fallback fired and
    ///    the operator either fixed the source or switched to a healthy
    ///    input)
    ///  - `on_input_switch` so the new input gets a fresh `pll_lock_timeout_s`
    ///    grace window before re-falling back
    pub fn deactivate_fallback(&self) {
        if self
            .fallback_active
            .swap(false, Ordering::AcqRel)
        {
            if let Ok(mut w) = self.fallback_reason.write() {
                *w = None;
            }
            tracing::info!(
                source_id = %self.source_id,
                "master clock: PLL fallback deactivated (PLL re-locked or input switched)"
            );
        }
    }

    pub fn is_fallback_active(&self) -> bool {
        self.fallback_active.load(Ordering::Acquire)
    }

    /// Active-input-switch hook. Reset the PLL anchor + clear any
    /// existing fallback so the new input has a clean `pll_lock_timeout_s`
    /// window to acquire lock. The watcher restarts its grace window
    /// from the moment `active_input_tx` publishes the new ID — this
    /// method only handles the master-clock state.
    pub fn on_input_switch(&self) {
        self.pll_master.reset_anchor();
        self.deactivate_fallback();
    }
}

impl MasterClock for PcrPllWithFallback {
    fn now_27mhz(&self) -> u64 {
        if self.fallback_active.load(Ordering::Acquire) {
            // Self-heal: if the PLL has acquired lock while in fallback
            // (operator fixed the source / input switched to a healthy
            // one / late lock after the watcher fired), leave fallback
            // immediately rather than wait for the watcher's next poll.
            if self.pll_master.is_locked() {
                self.deactivate_fallback();
                return self.pll_master.now_27mhz();
            }
            // Cascade middle rung: drive from PTP while it stays locked.
            // The first time it loses lock, demote one-way to wallclock so
            // we never emit PCR off an unlocked, undisciplined PTP clock.
            if self.fallback_uses_ptp.load(Ordering::Acquire) {
                if let Some(ptp) = &self.ptp_fallback {
                    if ptp.is_locked() {
                        return ptp.now_27mhz();
                    }
                }
                self.fallback_uses_ptp.store(false, Ordering::Release);
            }
            self.wallclock.now_27mhz()
        } else {
            self.pll_master.now_27mhz()
        }
    }

    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn is_locked(&self) -> bool {
        if self.fallback_active.load(Ordering::Acquire) {
            if self.pll_master.is_locked() {
                self.deactivate_fallback();
                return true;
            }
            // Wallclock fallback is always "locked" — operator sees the
            // fallback chip on the UI but consumers see a usable clock.
            true
        } else {
            self.pll_master.is_locked()
        }
    }

    fn telemetry(&self) -> MasterClockTelemetry {
        if self.fallback_active.load(Ordering::Acquire) {
            // Self-heal here too so the 1 Hz telemetry tick guarantees
            // the manager UI clears the fallback chip the moment the
            // PLL has re-locked — without waiting for the next emit-path
            // `now_27mhz` to trigger it.
            if self.pll_master.is_locked() {
                self.deactivate_fallback();
                return self.pll_master.telemetry();
            }
            // Cascade PTP rung active → report its real servo telemetry,
            // labelled `auto`. PTP is a legitimate resolved rung, not an
            // alarm, so `fallback_active` stays false (the "Auto → PTP"
            // label tells the story).
            let on_ptp = self.fallback_uses_ptp.load(Ordering::Acquire)
                && self
                    .ptp_fallback
                    .as_ref()
                    .map_or(false, |p| p.is_locked());
            if on_ptp {
                let mut t = self.ptp_fallback.as_ref().unwrap().telemetry();
                t.configured_kind =
                    Some(self.configured_kind.unwrap_or("auto").to_string());
                t.fallback_active = false;
                return t;
            }
            // Wallclock rung. The classic wrapper surfaces the alarm chip +
            // reason ("Wallclock (fallback from source_pcr_pll)"); the
            // cascade treats wallclock as its expected floor (the
            // "Auto → Wallclock" label communicates it) and does not alarm.
            let is_cascade = self.configured_kind.is_some();
            let mut t = self.wallclock.telemetry();
            t.kind = MasterClockKind::Wallclock.as_str().to_string();
            t.configured_kind = Some(
                self.configured_kind
                    .unwrap_or(MasterClockKind::SourcePcrPll.as_str())
                    .to_string(),
            );
            t.fallback_active = !is_cascade;
            t.fallback_reason = if is_cascade {
                None
            } else {
                self.fallback_reason.read().ok().and_then(|g| g.clone())
            };
            t
        } else {
            self.pll_master.telemetry()
        }
    }

    fn lipsync_offset_90k(&self) -> i64 {
        self.pll_master.lipsync_offset_90k()
    }
}

/// Spawn the continuous PLL ↔ Wallclock fallback monitor.
///
/// Runs as a long-lived poller that handles three transitions:
///
/// 1. **Initial grace window**: after spawn, gives the PLL
///    `timeout_s` seconds to acquire lock. If lock doesn't land,
///    activates fallback with `no_pcr_observed` / `insufficient_samples`
///    / `jitter_too_high` reason.
/// 2. **Recovery**: if the PLL acquires lock while in fallback (operator
///    fixed the source / input switched to a healthy stream), the
///    monitor deactivates fallback so output PCR follows the source
///    again. Self-heal also fires from `now_27mhz` / `telemetry`, this
///    branch is the belt-and-braces backstop and emits the recovery
///    event.
/// 3. **Input switch**: when `active_input_rx` publishes a new ID, the
///    grace window restarts so the new input has a fresh
///    `timeout_s` to acquire lock before re-falling-back. The monitor
///    also calls `PcrPllWithFallback::on_input_switch` to reset the
///    PLL anchor + clear stale fallback state.
///
/// `timeout_s == 0` disables the monitor entirely (strict-PLL mode).
pub fn spawn_pll_fallback_watcher(
    wrapper: Arc<PcrPllWithFallback>,
    flow_id: String,
    mut active_input_rx: tokio::sync::watch::Receiver<String>,
    timeout_s: u32,
    events: crate::manager::events::EventSender,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if timeout_s == 0 {
            // Strict-PLL mode — operator opted out of fallback.
            return;
        }
        let deadline = std::time::Duration::from_secs(timeout_s as u64);
        let poll_interval = std::time::Duration::from_millis(500);
        // Grace-window anchor. Reset on every input switch so the new
        // input gets a fresh `timeout_s` before re-falling-back. Seeded
        // at spawn so the first input also gets the grace window.
        let mut window_start = std::time::Instant::now();
        let mut current_input = active_input_rx.borrow().clone();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(poll_interval) => {}
                res = active_input_rx.changed() => {
                    if res.is_err() {
                        // sender dropped → flow shutting down
                        return;
                    }
                    let new_input = active_input_rx.borrow().clone();
                    if new_input != current_input {
                        tracing::info!(
                            flow_id = %flow_id,
                            previous = %current_input,
                            new = %new_input,
                            "master clock: active input changed — resetting PLL anchor + fallback grace window"
                        );
                        current_input = new_input;
                        window_start = std::time::Instant::now();
                        wrapper.on_input_switch();
                    }
                    continue;
                }
            }
            // ── Periodic poll body ────────────────────────────────────
            // Recovery: PLL re-locked while we were in fallback. Emit
            // the recovery event (UI clears the fallback chip via the
            // separate self-heal in `telemetry`).
            if wrapper.is_fallback_active() && wrapper.pll_master.pll().is_locked() {
                wrapper.deactivate_fallback();
                let t = wrapper.pll_master.pll().telemetry();
                events.emit_flow_with_details(
                    crate::manager::events::EventSeverity::Info,
                    crate::manager::events::category::MASTER_CLOCK,
                    format!(
                        "PCR PLL re-acquired lock on flow '{}' (input '{}'); leaving wallclock fallback",
                        flow_id, current_input,
                    ),
                    &flow_id,
                    serde_json::json!({
                        "error_code": "master_clock_pll_recovered",
                        "input_id": current_input,
                        "samples_received": t.samples,
                        "p99_jitter_us": t.jitter_us,
                    }),
                );
                // Recovery resets the grace window — if we de-lock again
                // we want the same grace before re-falling-back.
                window_start = std::time::Instant::now();
                continue;
            }
            // No-op if already in fallback (re-fire is gated by the
            // recovery branch above; otherwise we'd loop-fire the
            // event).
            if wrapper.is_fallback_active() {
                continue;
            }
            // Not in fallback — has the grace window expired without
            // lock?
            if window_start.elapsed() < deadline {
                continue;
            }
            let t = wrapper.pll_master.pll().telemetry();
            if t.locked {
                continue;
            }
            let reason = if t.samples == 0 {
                "no_pcr_observed"
            } else if t.samples < 100 {
                "insufficient_samples"
            } else {
                "jitter_too_high"
            };
            fire_fallback(
                &wrapper,
                &flow_id,
                &current_input,
                reason,
                t.samples,
                t.jitter_us,
                deadline,
                &events,
            );
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn fire_fallback(
    wrapper: &Arc<PcrPllWithFallback>,
    flow_id: &str,
    input_id: &str,
    reason: &str,
    samples_received: u64,
    p99_jitter_us: u64,
    waited: std::time::Duration,
    events: &crate::manager::events::EventSender,
) {
    wrapper.activate_fallback(reason);
    let msg = format!(
        "PCR PLL did not lock within {:.0}s on flow '{}' (reason: {}); falling back to wallclock",
        waited.as_secs_f32(),
        flow_id,
        reason,
    );
    tracing::warn!("{msg}");
    events.emit_flow_with_details(
        crate::manager::events::EventSeverity::Warning,
        crate::manager::events::category::MASTER_CLOCK,
        msg,
        flow_id,
        serde_json::json!({
            "error_code": "master_clock_pll_fallback",
            "input_id": input_id,
            "samples_received": samples_received,
            "samples_needed": 100u64,
            "p99_jitter_us": p99_jitter_us,
            "lock_threshold_us": 100u64,
            "fallback_reason": reason,
            "waited_s": waited.as_secs(),
        }),
    );
}

/// Per-flow handle carrying the dyn master + its kind tag.
///
/// Cheap to clone (Arc + Copy enum). Threaded through `FlowRuntime` and
/// every output / transcoder that needs a clock reference.
#[derive(Clone)]
pub struct MasterClockHandle {
    inner: Arc<dyn MasterClock>,
    kind: MasterClockKind,
    /// Manager-driven trim. AtomicI64 so the trim knob update path
    /// stays lock-free relative to the data path that reads it.
    lipsync_offset_90k: Arc<std::sync::atomic::AtomicI64>,
    /// Set true once the manager UI requested a degraded-clock event so
    /// we don't spam the operator. Mirrors the `WallclockMaster` fall-
    /// back path.
    degraded_warned: Arc<AtomicBool>,
    /// Operator's *configured* kind label when it differs from the
    /// resolved runtime `kind` (e.g. `auto` resolving to `ptp` /
    /// `wallclock`). Surfaced on `MasterClockTelemetry.configured_kind`.
    /// `Copy`, set once at construction before any clone — all clones
    /// inherit it without an atomic.
    configured_kind: Option<&'static str>,
}

impl MasterClockHandle {
    /// Build a `SourcePcrPll` handle. Returns the handle and the
    /// underlying `SourcePcrPllMaster` so the caller can wire the
    /// ingress sampler.
    pub fn source_pcr_pll(
        source_id: impl Into<String>,
    ) -> (Self, Arc<SourcePcrPllMaster>) {
        let inner = Arc::new(SourcePcrPllMaster::new(source_id));
        let handle = Self::new(inner.clone(), MasterClockKind::SourcePcrPll);
        (handle, inner)
    }

    /// Build a `Ptp` handle wrapping the existing PTP state reporter.
    /// `state` is the same `PtpStateHandle` ST 2110 inputs / outputs
    /// already plumb through `engine::st2110_io`.
    pub fn ptp(state: Arc<crate::engine::st2110::ptp::PtpStateHandle>) -> Self {
        let inner = Arc::new(PtpMasterClock::new(state));
        Self::new(inner, MasterClockKind::Ptp)
    }

    pub fn new(inner: Arc<dyn MasterClock>, kind: MasterClockKind) -> Self {
        Self {
            inner,
            kind,
            lipsync_offset_90k: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            degraded_warned: Arc::new(AtomicBool::new(false)),
            configured_kind: None,
        }
    }

    /// Tag the handle with the operator's *configured* kind label when it
    /// differs from the resolved runtime kind (e.g. `auto` resolving to
    /// `ptp` or `wallclock`). Surfaced on
    /// `MasterClockTelemetry.configured_kind` for the manager UI. Call
    /// before any clone — the tag is `Copy` and inherited by clones.
    pub fn with_configured_kind(mut self, configured: &'static str) -> Self {
        self.configured_kind = Some(configured);
        self
    }

    /// Build a Wallclock handle. Convenience wrapper for fall-back paths
    /// and unit tests.
    pub fn wallclock() -> Self {
        Self::new(Arc::new(WallclockMaster::new()), MasterClockKind::Wallclock)
    }

    /// Build a Passthrough handle. Runtime is the same as [`Self::wallclock`]
    /// but the kind label is `Passthrough` so telemetry distinguishes the
    /// auto-selected "no master clock needed" choice from a degraded
    /// fallback. No PLL spawn, no fallback watcher — pure source-driven
    /// timing semantics for flows whose output PCR comes from source bytes
    /// (passthrough) or source PTS (transcoded).
    pub fn passthrough() -> Self {
        Self::new(Arc::new(WallclockMaster::new()), MasterClockKind::Passthrough)
    }

    pub fn now_27mhz(&self) -> u64 {
        self.inner.now_27mhz()
    }

    pub fn now_90khz(&self) -> u64 {
        self.now_27mhz() / 300
    }

    pub fn source_id(&self) -> &str {
        self.inner.source_id()
    }

    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    pub fn kind(&self) -> MasterClockKind {
        self.kind
    }

    /// Operator-set trim. Bounded ±200 ms; updates are lock-free.
    pub fn set_lipsync_offset_90k(&self, ticks: i64) {
        let clamped = ticks.clamp(-18_000, 18_000);
        self.lipsync_offset_90k.store(clamped, Ordering::Relaxed);
    }

    pub fn lipsync_offset_90k(&self) -> i64 {
        self.lipsync_offset_90k.load(Ordering::Relaxed)
    }

    pub fn telemetry(&self) -> MasterClockTelemetry {
        let mut t = self.inner.telemetry();
        // If the inner is a fallback-aware wrapper (e.g.
        // `PcrPllWithFallback`), it has already populated `kind` with
        // the *actual* driving clock (wallclock when fallback fired,
        // source_pcr_pll otherwise) and set `configured_kind` to the
        // operator's request. Trust those values.
        //
        // Otherwise — the inner is a vanilla clock backing a typed
        // handle (e.g. a bare `WallclockMaster` returned via
        // `MasterClockHandle::wallclock()`) — overwrite `kind` with
        // the handle's stored tag so the manager UI reports the
        // operator-requested kind, not the implementation detail
        // (a SourcePcrPll handle backed by a WallclockMaster during
        // a Phase-3 stub would otherwise label itself "wallclock").
        if t.configured_kind.is_none() {
            t.kind = self.kind.as_str().to_string();
            // Surface the operator's configured label (e.g. `auto`) when
            // it differs from the resolved runtime kind, so the UI can
            // render "Auto → PTP" rather than just "PTP".
            if let Some(ck) = self.configured_kind {
                t.configured_kind = Some(ck.to_string());
            }
        }
        t
    }

    /// Mark the handle "degraded warning emitted" exactly once. Returns
    /// `true` on the first call so the caller can emit a one-shot event.
    pub fn mark_degraded_warned(&self) -> bool {
        !self.degraded_warned.swap(true, Ordering::Relaxed)
    }
}

impl std::fmt::Debug for MasterClockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterClockHandle")
            .field("kind", &self.kind)
            .field("source_id", &self.inner.source_id())
            .field("locked", &self.inner.is_locked())
            .field("lipsync_offset_90k", &self.lipsync_offset_90k())
            .finish()
    }
}

/// Fine-grained per-input clock identity used by the PID-bus
/// cross-clock-compatibility check.
///
/// `MasterClockKind` answers "what kind of clock recovers this input's
/// time domain?" but does not answer "are two inputs co-clocked?".
/// Two `MasterClockKind::SourcePcrPll` inputs from independent SRT
/// senders are NOT co-clocked (each upstream encoder has its own
/// 27 MHz oscillator); two `MasterClockKind::Ptp` inputs are co-clocked
/// iff they share a PTP domain.
///
/// This enum is the identity the PID-bus assembler uses to validate
/// that every slot in a single output program comes from inputs that
/// are *actually* coherent — preventing the silent A/V-drift class
/// of bugs that arises when an operator wires "video from encoder A"
/// + "audio from encoder B" through Phase 2's cross-flow ES routing.
///
/// Equality semantics:
/// - `Ptp { domain: N }` == `Ptp { domain: N }` (same grandmaster).
/// - `Ptp { domain: N }` != `Ptp { domain: M }` for N≠M.
/// - `SourcePcr { input_id: A }` == `SourcePcr { input_id: A }` (same input).
/// - `SourcePcr { input_id: A }` != `SourcePcr { input_id: B }` for A≠B
///   (each input owns its own PLL).
/// - `Wallclock` == `Wallclock` (the host's monotonic clock is shared).
/// - Cross-kind comparisons (`Ptp` vs `SourcePcr` etc.) are always !=.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClockIdentity {
    /// PTP grandmaster on a specific IEEE 1588 domain.
    Ptp { domain: u8 },
    /// Source-PCR PLL pinned to the named input. Each upstream encoder
    /// has its own oscillator, so the `input_id` is part of the identity.
    SourcePcr { input_id: String },
    /// Host wallclock — degraded, but all wallclock-only inputs on the
    /// same node share the same clock instance so they're nominally
    /// equal. The degraded-clock warning still fires via the existing
    /// telemetry path.
    Wallclock,
}

impl ClockIdentity {
    /// Short human-readable tag, useful in error messages and the
    /// structured details of `pid_bus_master_clock_mismatch` events.
    pub fn label(&self) -> String {
        match self {
            Self::Ptp { domain } => format!("ptp:domain={domain}"),
            Self::SourcePcr { input_id } => format!("source_pcr:input={input_id}"),
            Self::Wallclock => "wallclock".to_string(),
        }
    }
}

/// Derive the [`ClockIdentity`] for a node's input from its definition.
///
/// Used by the PID-bus assembly compatibility check: every slot in
/// one output program must report the same identity from this
/// function, otherwise the resolve-time check rejects the plan with
/// `pid_bus_master_clock_mismatch`.
///
/// The mapping mirrors [`select_master_kind_for_input`] but additionally
/// pins each clock to its concrete coherence-defining property:
/// PTP-domain for the `Ptp` family, the input's globally-unique id for
/// the `SourcePcr` family. Two inputs with separate ids — even if both
/// are SRT listeners on the same port number across reboots — are not
/// co-clocked, because each ingest task drives its own
/// [`SourcePcrPllMaster`] PLL instance.
///
/// `clock_domain` falls back to 0 (the IEEE 1588 default profile
/// domain) when the input config doesn't specify one — matches
/// `ptp4l`'s default.
pub fn clock_identity_for_input(
    input: &crate::config::models::InputDefinition,
) -> ClockIdentity {
    use crate::config::models::InputConfig;
    // Testbed-only escape hatch for Phase 4 PES-splice verification:
    // when `BILBYCAST_TESTBED_SHARED_WALLCLOCK=1`, treat every input as
    // co-clocked on the host wallclock. NEVER set this in production —
    // it removes the silent A/V-drift guard for cross-flow ES routing.
    if matches!(
        std::env::var("BILBYCAST_TESTBED_SHARED_WALLCLOCK").as_deref(),
        Ok("1") | Ok("true")
    ) {
        return ClockIdentity::Wallclock;
    }
    match &input.config {
        // Source-PCR family — identity pinned to this input's id.
        InputConfig::Srt(_)
        | InputConfig::Rtp(_)
        | InputConfig::Udp(_)
        | InputConfig::Rist(_)
        | InputConfig::Rtmp(_)
        | InputConfig::Rtsp(_)
        | InputConfig::MediaPlayer(_)
        | InputConfig::TestPattern(_)
        | InputConfig::Replay(_)
        | InputConfig::RtpAudio(_)
        | InputConfig::Bonded(_) => ClockIdentity::SourcePcr {
            input_id: input.id.clone(),
        },
        // PTP family — identity is the configured grandmaster domain.
        InputConfig::St2110_20(c) => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        InputConfig::St2110_23(c) => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        InputConfig::St2110_30(c) => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        InputConfig::St2110_31(c) => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        InputConfig::St2110_40(c) => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        // MXL is PTP-anchored at v1.0 — same identity rule as ST 2110.
        InputConfig::MxlVideo(c) => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        InputConfig::MxlAudio(c) => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        InputConfig::MxlAnc(c)   => ClockIdentity::Ptp { domain: c.clock_domain.unwrap_or(0) },
        // Wallclock-only inputs share the host clock.
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => ClockIdentity::Wallclock,
    }
}

/// Selection-policy entry point. Returns the auto-default for a flow
/// based on its active input config + flow shape. Per-flow
/// `master_clock` overrides are applied by the caller in `flow.rs`.
///
/// **Selection priority:**
///
/// 1. **PTP for ST 2110 inputs** — these carry essence on PTP-disciplined
///    transports; the PTP master is the only correct one.
/// 2. **Wallclock for WebRTC-like inputs** — WebRTC ingress is wallclock-
///    paced by the str0m session. A degraded-clock warning fires (see
///    `flow.rs::build_master_clock`).
/// 3. **SourcePcrPll for assembled / synthesized flows** — when the flow
///    has [`FlowConfig::assembly`] set (PID-bus SPTS/MPTS), output PCR
///    is generated by the assembler from `master.now_27mhz()` and
///    therefore needs a real clock. The PLL recovers source PCR so the
///    assembled stream's clock tracks the contribution source.
/// 4. **Passthrough for everything else** — most contribution-to-
///    distribution flows. Output PCR comes from the source bytes
///    directly (passthrough outputs) or from source PTS (transcoded
///    outputs via `av_sync_mux::pcr_for_emit`). The master clock is
///    informational; running the PLL would only generate misleading
///    fallback alarms on legitimate sources that have ms-scale PCR
///    arrival jitter (typical for internet contribution over SRT/RIST).
///    Operators who want PLL behaviour explicitly pin
///    `master_clock.kind: "source_pcr_pll"` per-flow.
///
/// This split is the answer to a real customer pain: before it, every
/// SRT/RIST/RTP input flow ran the PLL by default, and the fallback
/// timer fired any time the source's PCR-vs-wallclock jitter exceeded
/// 100 µs — which is essentially every internet-delivered stream.
/// Customers saw a stream of "PLL fallback" alarms on flows whose
/// output A/V sync was unaffected by the alarm.
/// Resolve the input ID that should drive the master clock for a
/// given flow. For PID-bus assembled flows, this is the operator-
/// designated `pcr_source.input_id` (flow-level for SPTS, or the
/// first program's pcr_source for MPTS, with the flow-level setting
/// as a fallback). For non-assembled flows, returns `None` and the
/// caller falls back to the "active input" semantic.
///
/// The runtime then uses this to:
///   1. Pick the master-clock kind based on this input's type (not
///      the active input's), via [`select_master_kind_for_input`].
///   2. Subscribe the PCR ingress sampler to this input's per-input
///      broadcast (via `FlowManager::subscribe_input`), rather than
///      the flow's broadcast — which, for assembled flows, carries
///      the assembler's *output* PCR (the very value the master clock
///      generated) and would form a self-referential loop.
///
/// Returns `None` when:
/// - The flow has no assembly (`flow.assembly` is `None` or
///   `assembly.kind == Passthrough`), and
/// - No flow-level or per-program `pcr_source` is set.
pub fn resolve_pcr_source_input_id(
    flow: &crate::config::models::FlowConfig,
) -> Option<&str> {
    use crate::config::models::AssemblyKind;
    let asm = flow.assembly.as_ref()?;
    if matches!(asm.kind, AssemblyKind::Passthrough) {
        return None;
    }
    if let Some(src) = asm.pcr_source.as_ref() {
        return Some(&src.input_id);
    }
    // No flow-level pcr_source. Look at the first program's pcr_source
    // (MPTS scenarios where each program designates its own).
    asm.programs
        .first()
        .and_then(|p| p.pcr_source.as_ref())
        .map(|src| src.input_id.as_str())
}

pub fn select_master_kind_for_input(
    input: Option<&crate::config::models::InputConfig>,
    flow: &crate::config::models::FlowConfig,
) -> MasterClockKind {
    use crate::config::models::InputConfig;
    let Some(input) = input else {
        return MasterClockKind::Passthrough;
    };
    // ST 2110 always wants PTP — they're PTP-disciplined transports.
    if matches!(
        input,
        InputConfig::St2110_20(_)
            | InputConfig::St2110_23(_)
            | InputConfig::St2110_30(_)
            | InputConfig::St2110_31(_)
            | InputConfig::St2110_40(_)
    ) {
        return MasterClockKind::Ptp;
    }
    // WebRTC ingress is wallclock-paced — the str0m session sets its own
    // PTS rate.
    if matches!(input, InputConfig::Webrtc(_) | InputConfig::Whep(_)) {
        return MasterClockKind::Wallclock;
    }
    // PID-bus assembled flows synthesize output PCR from the master
    // clock — they NEED a recovered source clock to keep that PCR
    // coherent with the source. Default to SourcePcrPll so the
    // assembler tracks the contribution's clock.
    if flow.assembly.is_some() {
        return MasterClockKind::SourcePcrPll;
    }
    // Everything else (passthrough / transcoded / single-input flows
    // without assembly): default to `Wallclock`.
    //
    // The previous default was `Passthrough` — a wallclock-equivalent
    // backend with a different tag. The new explicit Wallclock default
    // pairs with the encoder-style PTS regeneration work
    // (`engine::ts_pts_rewriter` + `TsAudioReplacer::set_av_sync_pacer`):
    // those callers anchor against `master.now_27mhz()` and need a
    // monotonic, always-locked master. Wallclock satisfies both,
    // without a PLL that fails to lock on loop-every-30s contribution
    // sources (the failure mode that motivated this work — see
    // `.claude-memory/monorepo/project_audio_transcode_stale_expected.md`).
    //
    // Operators who run on PTP-disciplined or clean-PCR sources and
    // want source-PCR-PLL pacing opt-in explicitly via
    // `master_clock.kind = "contribution"` (preferred, surfaces intent
    // in telemetry) or the legacy `master_clock.kind = "source_pcr_pll"`.
    match input {
        InputConfig::Srt(_)
        | InputConfig::Rtp(_)
        | InputConfig::Udp(_)
        | InputConfig::Rist(_)
        | InputConfig::Rtmp(_)
        | InputConfig::Rtsp(_)
        | InputConfig::MediaPlayer(_)
        | InputConfig::TestPattern(_)
        | InputConfig::Replay(_)
        | InputConfig::RtpAudio(_)
        | InputConfig::Bonded(_) => MasterClockKind::Wallclock,
        // ST 2110 + WebRTC handled above; arms here are for completeness
        // of the match (won't be reached).
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => MasterClockKind::Wallclock,
        InputConfig::St2110_20(_)
        | InputConfig::St2110_23(_)
        | InputConfig::St2110_30(_)
        | InputConfig::St2110_31(_)
        | InputConfig::St2110_40(_) => MasterClockKind::Ptp,
        // MXL — same selection rule as ST 2110: PTP-disciplined,
        // validation rejects `master_clock=wallclock` on MXL flows.
        InputConfig::MxlVideo(_)
        | InputConfig::MxlAudio(_)
        | InputConfig::MxlAnc(_) => MasterClockKind::Ptp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallclock_is_always_locked() {
        let h = MasterClockHandle::wallclock();
        assert!(h.is_locked());
        assert_eq!(h.kind(), MasterClockKind::Wallclock);
        assert_eq!(h.source_id(), "wallclock");
    }

    #[test]
    fn with_configured_kind_surfaces_label_but_keeps_resolved_kind() {
        // `auto` resolving to a wallclock backend must report
        // kind = "wallclock" (what is actually running) AND
        // configured_kind = "auto" (what the operator asked for).
        let h = MasterClockHandle::wallclock().with_configured_kind("auto");
        let t = h.telemetry();
        assert_eq!(t.kind, "wallclock");
        assert_eq!(t.configured_kind.as_deref(), Some("auto"));
        // Untagged handle leaves configured_kind absent.
        assert_eq!(MasterClockHandle::wallclock().telemetry().configured_kind, None);
    }

    #[test]
    fn master_clock_kind_config_auto_serde() {
        use crate::config::models::MasterClockKindConfig;
        let parsed: MasterClockKindConfig =
            serde_json::from_str("\"auto\"").expect("\"auto\" should deserialize");
        assert_eq!(parsed, MasterClockKindConfig::Auto);
        assert_eq!(
            serde_json::to_string(&MasterClockKindConfig::Auto).unwrap(),
            "\"auto\""
        );
    }

    #[test]
    fn cascade_without_ptp_rung_falls_to_wallclock_labelled_auto() {
        // No PTP role configured → cascade degrades to PLL → Wallclock, but
        // labelled `auto` and WITHOUT the alarm chip (the "Auto → Wallclock"
        // label communicates the degraded floor).
        let pll = Arc::new(SourcePcrPllMaster::new("test-auto"));
        let w = PcrPllWithFallback::new_cascade(pll, None, "auto");
        assert!(!w.is_fallback_active());
        w.activate_fallback("jitter_too_high");
        assert!(w.is_fallback_active());
        let t = w.telemetry();
        assert_eq!(t.kind, "wallclock");
        assert_eq!(t.configured_kind.as_deref(), Some("auto"));
        assert!(!t.fallback_active, "cascade must not raise the alarm chip");
        assert!(w.now_27mhz() > 0);
    }

    #[test]
    fn classic_fallback_still_alarms_with_source_pcr_pll_label() {
        // The classic PLL → Wallclock wrapper is unchanged: alarm chip +
        // source_pcr_pll label + reason.
        let pll = Arc::new(SourcePcrPllMaster::new("test-classic"));
        let w = PcrPllWithFallback::new(pll);
        w.activate_fallback("jitter_too_high");
        let t = w.telemetry();
        assert_eq!(t.kind, "wallclock");
        assert_eq!(t.configured_kind.as_deref(), Some("source_pcr_pll"));
        assert!(t.fallback_active, "classic wrapper still alarms");
        assert_eq!(t.fallback_reason.as_deref(), Some("jitter_too_high"));
    }

    #[test]
    fn wallclock_advances_monotonically() {
        let h = MasterClockHandle::wallclock();
        let a = h.now_27mhz();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = h.now_27mhz();
        // 5 ms = 135 000 ticks; expect ≥ 100k delta to absorb sleep jitter.
        assert!(b > a, "wallclock did not advance: {a} → {b}");
        assert!(
            b - a >= 100_000,
            "wallclock advance too small: {} ticks",
            b - a
        );
    }

    #[test]
    fn now_90khz_is_now_27mhz_div_300() {
        let h = MasterClockHandle::wallclock();
        let a = h.now_27mhz();
        let b = h.now_90khz();
        // Allow a small race window between the two reads.
        assert!(b * 300 <= a + 30_000, "90k vs 27m mismatch: {a} {b}");
    }

    #[test]
    fn lipsync_offset_clamps_to_200_ms() {
        let h = MasterClockHandle::wallclock();
        h.set_lipsync_offset_90k(50_000); // 555 ms
        assert_eq!(h.lipsync_offset_90k(), 18_000);
        h.set_lipsync_offset_90k(-50_000);
        assert_eq!(h.lipsync_offset_90k(), -18_000);
        h.set_lipsync_offset_90k(1_500); // 16.7 ms — within bounds
        assert_eq!(h.lipsync_offset_90k(), 1_500);
    }

    #[test]
    fn mark_degraded_warned_is_one_shot() {
        let h = MasterClockHandle::wallclock();
        assert!(h.mark_degraded_warned());
        assert!(!h.mark_degraded_warned());
        assert!(!h.mark_degraded_warned());
    }

    #[test]
    fn telemetry_carries_kind_label() {
        let h = MasterClockHandle::wallclock();
        let t = h.telemetry();
        assert_eq!(t.kind, "wallclock");
        assert!(t.locked);
        assert_eq!(t.rate_offset_ppm, 0.0);
    }

    #[test]
    fn master_clock_kind_serde_roundtrip() {
        let k = MasterClockKind::SourcePcrPll;
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, "\"source_pcr_pll\"");
        let back: MasterClockKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, MasterClockKind::SourcePcrPll);
    }

    #[test]
    fn ptp_master_reports_unavailable_until_locked() {
        use crate::engine::st2110::ptp::PtpStateHandle;
        let state = Arc::new(PtpStateHandle::new(0));
        let m = PtpMasterClock::new(state);
        assert!(!m.is_locked(), "PTP master without GM should not be locked");
        assert!(m.source_id().starts_with("ptp:domain=0"));
        // now_27mhz returns a sensible-sized value driven by SystemTime.
        let now = m.now_27mhz();
        assert!(now > 0);
    }

    #[test]
    fn ptp_master_telemetry_carries_kind() {
        use crate::engine::st2110::ptp::PtpStateHandle;
        let state = Arc::new(PtpStateHandle::new(127));
        let h = MasterClockHandle::ptp(state);
        let t = h.telemetry();
        assert_eq!(t.kind, "ptp");
        assert!(!t.locked);
    }

    #[test]
    fn wallclock_independent_offsets_are_random() {
        // Two new wallclocks created back-to-back should land on
        // different offsets so cross-edge wallclock flows don't claim
        // accidental coherence.
        let a = WallclockMaster::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = WallclockMaster::new();
        assert_ne!(a.offset_27mhz, b.offset_27mhz);
    }

    // ─── ClockIdentity ────────────────────────────────────────────────
    //
    // The enum's equality + label behaviour is what the cross-clock
    // compat check depends on. Per-input-config mapping is exercised
    // via the assembler's `clock_compat_*` test suite where the full
    // realistic plan + map shape is built — keeping these tests tight
    // here so the suite doesn't depend on every InputConfig variant's
    // unstable field set.

    #[test]
    fn clock_identity_source_pcr_keyed_by_input_id() {
        let a = ClockIdentity::SourcePcr { input_id: "in-a".into() };
        let b = ClockIdentity::SourcePcr { input_id: "in-b".into() };
        let a2 = ClockIdentity::SourcePcr { input_id: "in-a".into() };
        assert_eq!(a, a2, "same input_id → equal");
        assert_ne!(a, b, "different input_ids → never co-clocked");
    }

    #[test]
    fn clock_identity_ptp_keyed_by_domain() {
        let d0 = ClockIdentity::Ptp { domain: 0 };
        let d0_again = ClockIdentity::Ptp { domain: 0 };
        let d127 = ClockIdentity::Ptp { domain: 127 };
        assert_eq!(d0, d0_again, "same PTP domain → co-clocked");
        assert_ne!(d0, d127, "different domains → not co-clocked");
    }

    #[test]
    fn clock_identity_cross_kind_never_equal() {
        // PTP vs SourcePcr vs Wallclock are categorically distinct;
        // assembler must reject any program mixing them.
        let p = ClockIdentity::Ptp { domain: 0 };
        let s = ClockIdentity::SourcePcr { input_id: "x".into() };
        let w = ClockIdentity::Wallclock;
        assert_ne!(p, s);
        assert_ne!(p, w);
        assert_ne!(s, w);
    }

    #[test]
    fn clock_identity_label_distinguishes_each_variant() {
        let a = ClockIdentity::SourcePcr { input_id: "in-srt-a".into() };
        let b = ClockIdentity::Ptp { domain: 127 };
        let c = ClockIdentity::Wallclock;
        assert!(a.label().contains("in-srt-a"));
        assert!(b.label().contains("127"));
        assert_eq!(c.label(), "wallclock");
        let labels = std::collections::HashSet::<String>::from_iter(
            [a.label(), b.label(), c.label()]
        );
        assert_eq!(labels.len(), 3, "labels must distinguish each variant");
    }
}
