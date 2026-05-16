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

use std::sync::{Arc, OnceLock};
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
}

impl SourcePcrPllMaster {
    pub fn new(source_id: impl Into<String>) -> Self {
        Self {
            pll: Arc::new(crate::engine::pcr_pll::PcrPll::default()),
            epoch: Instant::now(),
            source_id: source_id.into(),
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
        }
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
            PtpLockState::Locked
        )
    }

    fn telemetry(&self) -> MasterClockTelemetry {
        let snap = self.state.snapshot();
        let locked = matches!(
            snap.lock_state,
            crate::engine::st2110::ptp::PtpLockState::Locked
        );
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
    fallback_active: AtomicBool,
    /// Set once when the fallback fires. Read by `telemetry()`.
    fallback_reason: OnceLock<String>,
    source_id: String,
}

impl PcrPllWithFallback {
    pub fn new(pll_master: Arc<SourcePcrPllMaster>) -> Self {
        let source_id = pll_master.source_id().to_string();
        Self {
            pll_master,
            wallclock: WallclockMaster::new(),
            fallback_active: AtomicBool::new(false),
            fallback_reason: OnceLock::new(),
            source_id,
        }
    }

    pub fn pll_master(&self) -> Arc<SourcePcrPllMaster> {
        self.pll_master.clone()
    }

    /// Flip the master onto wallclock and record the reason. Idempotent —
    /// subsequent calls are no-ops.
    pub fn activate_fallback(&self, reason: impl Into<String>) {
        // Set reason first so any reader that observes `fallback_active`
        // sees a populated reason.
        let _ = self.fallback_reason.set(reason.into());
        self.fallback_active.store(true, Ordering::Release);
    }

    pub fn is_fallback_active(&self) -> bool {
        self.fallback_active.load(Ordering::Acquire)
    }
}

impl MasterClock for PcrPllWithFallback {
    fn now_27mhz(&self) -> u64 {
        if self.fallback_active.load(Ordering::Acquire) {
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
            // Wallclock fallback is always "locked" — operator sees the
            // fallback chip on the UI but consumers see a usable clock.
            true
        } else {
            self.pll_master.is_locked()
        }
    }

    fn telemetry(&self) -> MasterClockTelemetry {
        if self.fallback_active.load(Ordering::Acquire) {
            // Inherit the wallclock telemetry but stamp the configured
            // kind + fallback details so UI keeps the operator informed.
            let mut t = self.wallclock.telemetry();
            t.kind = MasterClockKind::Wallclock.as_str().to_string();
            t.configured_kind = Some(MasterClockKind::SourcePcrPll.as_str().to_string());
            t.fallback_active = true;
            t.fallback_reason = self.fallback_reason.get().cloned();
            t
        } else {
            self.pll_master.telemetry()
        }
    }

    fn lipsync_offset_90k(&self) -> i64 {
        self.pll_master.lipsync_offset_90k()
    }
}

/// Spawn the PLL→Wallclock fallback watcher.
///
/// Polls the PLL's [`PcrPll::telemetry`] periodically. Fires fallback
/// (via [`PcrPllWithFallback::activate_fallback`]) when:
///
/// - **No samples after 5 s** → `no_pcr_observed`. The fast path is
///   important for sources that never emit PCR at all (constant-
///   bitrate raw TS, some legacy contribution feeds): waiting the
///   full timeout for one of those would silently delay output
///   start by `timeout_s` seconds.
/// - **Not locked at timeout** with samples > 0 → either
///   `insufficient_samples` (still under the 100-sample minimum) or
///   `jitter_too_high` (samples present but p99 over the lock
///   threshold).
///
/// `timeout_s == 0` disables the watcher entirely (strict-PLL mode).
pub fn spawn_pll_fallback_watcher(
    wrapper: Arc<PcrPllWithFallback>,
    flow_id: String,
    input_id: String,
    timeout_s: u32,
    events: crate::manager::events::EventSender,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if timeout_s == 0 {
            // Strict-PLL mode — operator opted out of fallback.
            return;
        }
        // Single timeout. Waiting the full `pll_lock_timeout_s` before
        // judging lock keeps the watcher from racing the broadcast
        // pipeline's warm-up (file open, codec init, first PES, input
        // forwarder pickup — collectively a few hundred ms to a few
        // seconds depending on input type). A `samples == 0` outcome
        // at the deadline is then a real "the source never emitted
        // PCR" signal, not a startup artefact. The earlier 5 s
        // fast-path was tempting but proved noisy: high-bitrate media
        // player inputs took up to 5 s to start publishing under heavy
        // CPU + Tokio startup load and the watcher fired before any
        // PCR sample landed at the PLL, even though packets were
        // streaming healthily within seconds.
        let deadline = std::time::Duration::from_secs(timeout_s as u64);
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(deadline) => {}
        }
        if wrapper.is_fallback_active() {
            return;
        }
        let t = wrapper.pll_master.pll().telemetry();
        if t.locked {
            return; // PLL converged within budget — done, no fallback.
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
            &input_id,
            reason,
            t.samples,
            t.jitter_us,
            deadline,
            &events,
        );
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
        // Wallclock-only inputs share the host clock.
        InputConfig::Webrtc(_) | InputConfig::Whep(_) => ClockIdentity::Wallclock,
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
