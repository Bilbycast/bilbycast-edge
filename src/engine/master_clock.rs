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
//! [`select_master_for_flow`] picks the right [`MasterClockKind`] for a
//! given flow at start-up:
//!
//! | Active input                            | Master                        |
//! |-----------------------------------------|-------------------------------|
//! | SRT / RTP / UDP / RIST carrying TS      | `SourcePcrPll`                |
//! | RTMP / RTSP carrying TS via input mux   | `SourcePcrPll`                |
//! | media_player / test_pattern / replay    | `SourcePcrPll` (synth feed)   |
//! | ST 2110-20/-23/-30/-31/-40              | `Ptp` (Phase 6)               |
//! | WebRTC ingress                          | `Wallclock` + warn event      |
//! | None active                             | `Wallclock`                   |
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
    /// Last-resort monotonic wall clock. Loud telemetry warning.
    Wallclock,
}

impl MasterClockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourcePcrPll => "source_pcr_pll",
            Self::Ptp => "ptp",
            Self::AudioMaster => "audio_master",
            Self::Wallclock => "wallclock",
        }
    }
}

/// Snapshot of master-clock health surfaced on `FlowStats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterClockTelemetry {
    /// Tagged kind ("wallclock", "source_pcr_pll", "ptp", "audio_master").
    pub kind: String,
    /// True when the clock is converged enough for broadcast-grade emit.
    pub locked: bool,
    /// Recovered rate vs. local CPU clock, in ppm. Only meaningful for
    /// PLL-style masters; `0.0` for Wallclock + Ptp.
    pub rate_offset_ppm: f64,
    /// Recent jitter in microseconds (p99 over last 1 s window).
    pub jitter_us: u64,
}

impl Default for MasterClockTelemetry {
    fn default() -> Self {
        Self {
            kind: "wallclock".into(),
            locked: true,
            rate_offset_ppm: 0.0,
            jitter_us: 0,
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
        }
    }
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
}

impl MasterClockHandle {
    pub fn new(inner: Arc<dyn MasterClock>, kind: MasterClockKind) -> Self {
        Self {
            inner,
            kind,
            lipsync_offset_90k: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            degraded_warned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Build a Wallclock handle. Convenience wrapper for fall-back paths
    /// and unit tests.
    pub fn wallclock() -> Self {
        Self::new(Arc::new(WallclockMaster::new()), MasterClockKind::Wallclock)
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
        // Carry the kind tag from the handle (the inner Wallclock backing
        // a SourcePcrPll handle would otherwise label itself "wallclock").
        let mut t = self.inner.telemetry();
        t.kind = self.kind.as_str().to_string();
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

/// Selection-policy entry point. Returns the auto-default for a flow
/// based on its active input config. Per-flow `master_clock` overrides
/// are applied by the caller in `flow.rs`.
///
/// Inputs that don't carry MPEG-TS but produce media (WebRTC) get
/// `Wallclock` with a degraded-clock warning. Pure-data inputs land on
/// `SourcePcrPll` because their TS muxer mints PCR.
pub fn select_master_kind_for_input(
    input: Option<&crate::config::models::InputConfig>,
) -> MasterClockKind {
    use crate::config::models::InputConfig;
    let Some(input) = input else {
        return MasterClockKind::Wallclock;
    };
    match input {
        InputConfig::Srt(_)
        | InputConfig::Rtp(_)
        | InputConfig::Udp(_)
        | InputConfig::Rist(_)
        | InputConfig::Rtmp(_)
        | InputConfig::Rtsp(_)
        | InputConfig::MediaPlayer(_)
        | InputConfig::TestPattern(_) => MasterClockKind::SourcePcrPll,
        InputConfig::Replay(_) => MasterClockKind::SourcePcrPll,
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => MasterClockKind::Wallclock,
        InputConfig::St2110_20(_)
        | InputConfig::St2110_23(_)
        | InputConfig::St2110_30(_)
        | InputConfig::St2110_31(_)
        | InputConfig::St2110_40(_) => MasterClockKind::Ptp,
        InputConfig::RtpAudio(_) => MasterClockKind::SourcePcrPll,
        InputConfig::Bonded(_) => MasterClockKind::SourcePcrPll,
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
    fn wallclock_independent_offsets_are_random() {
        // Two new wallclocks created back-to-back should land on
        // different offsets so cross-edge wallclock flows don't claim
        // accidental coherence.
        let a = WallclockMaster::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = WallclockMaster::new();
        assert_ne!(a.offset_27mhz, b.offset_27mhz);
    }
}
