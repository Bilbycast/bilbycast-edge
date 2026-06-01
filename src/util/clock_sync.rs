// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! System-clock discipline status for `HealthPayload.clock_sync`.
//!
//! A non-privileged `adjtimex(2)` read (`modes = 0`) reports whether the
//! kernel system clock — which `CLOCK_MONOTONIC`, and therefore the
//! per-flow wallclock master clock, ride on — is being disciplined by a
//! time source (chrony / ntpd / phc2sys) or is free-running. A
//! free-running crystal drifts ~±20–100 ppm with temperature, which
//! degrades generated-PCR accuracy (most visibly on PID-bus assembled
//! flows, whose assembler synthesises output PCR from the master clock)
//! and breaks cross-edge 2022-7 coherence. A chrony/phc2sys-disciplined
//! host holds sub-ppm, comfortably inside the MPEG ±30 ppm tolerance.
//!
//! `CAP_SYS_TIME` is NOT required for a read — it is only needed when
//! `modes != 0` (a clock-adjusting write). The edge main process holds no
//! `CAP_SYS_TIME`, so this stays strictly a read.
//!
//! Called once per health tick (~15 s) and on the synchronous `/health`
//! endpoint — a single in-kernel syscall, no I/O, never on the data path.
//! Wire shape is consumed by the manager UI off the opaque cached_health
//! blob (`health.clock_sync`), capability-gated on `"clock-sync"`.

use serde::Serialize;

/// Coarse discipline classification driving the manager UI badge colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockSyncState {
    /// Kernel reports a synchronization source (chrony/ntpd/phc2sys active).
    Synchronized,
    /// `STA_UNSYNC` set or `adjtimex` returned `TIME_ERROR` — free-running.
    Unsynchronized,
    /// `adjtimex` returned a state outside the known `TIME_*` range.
    Unknown,
}

/// Snapshot of the host clock's NTP/PLL discipline state from `adjtimex(2)`.
#[derive(Debug, Clone, Serialize)]
pub struct ClockSyncStatus {
    pub state: ClockSyncState,
    /// `STA_UNSYNC` bit set in `timex.status` — the authoritative
    /// "no synchronization source" flag.
    pub unsync: bool,
    /// `timex.maxerror` — kernel-estimated maximum clock error, microseconds.
    /// Climbs toward the ~16 s cap when no time updates arrive, so it is the
    /// leading indicator of a clock that has lost (or never had) discipline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_error_us: Option<i64>,
    /// `timex.esterror` — kernel-estimated clock error, microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_error_us: Option<i64>,
    /// Total frequency correction the kernel applies, ppm. Reconstructed
    /// from BOTH the coarse `tick` register and the fine `freq` register —
    /// chrony/ntpd routinely load most of the correction into `tick`, so
    /// `freq` alone badly under-reports the real rate offset (e.g. a clock
    /// corrected by +16 ppm can read `freq` = -83 ppm with the balance in
    /// `tick`). Positive = the kernel is speeding a slow crystal up. This is
    /// the wallclock master's true rate adjustment; compare to `chronyc
    /// tracking` "Frequency". Kernel clamps the total to ~±500 ppm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freq_ppm: Option<f64>,
    /// `timex.offset` — current time offset being corrected. Microseconds
    /// in the default mode (nanoseconds when `STA_NANO` is set in `status`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_us: Option<i64>,
}

/// Read the host clock discipline state. `None` when the syscall fails
/// (e.g. seccomp-blocked) so the caller omits the field + capability and
/// the manager hides the card.
#[cfg(target_os = "linux")]
pub fn probe() -> Option<ClockSyncStatus> {
    // SAFETY: a zeroed `timex` sets `modes = 0`, making this a
    // non-privileged READ. `adjtimex` fills the struct and returns the
    // kernel clock state (TIME_*, >= 0) or -1 on error. No pointer
    // aliasing; `tx` outlives the call.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::adjtimex(&mut tx) };
    if ret == -1 {
        return None;
    }

    let unsync = (tx.status & libc::STA_UNSYNC) != 0;
    // `TIME_ERROR` (value 5) is the kernel's "clock not synchronised"
    // return. Either signal → Unsynchronized; both clear and the return is
    // a known TIME_* state → Synchronized; anything else → Unknown.
    let state = if unsync || ret == libc::TIME_ERROR {
        ClockSyncState::Unsynchronized
    } else if (0..=libc::TIME_WAIT).contains(&ret) {
        ClockSyncState::Synchronized
    } else {
        ClockSyncState::Unknown
    };

    // Total frequency correction = coarse `tick` + fine `freq`. The kernel
    // splits discipline between the two; chrony/ntpd routinely load most of
    // it into `tick`, so `freq / 65536` alone is not the real rate offset.
    // tick is microseconds per scheduler tick; nominal = 1e6 / USER_HZ.
    let fine_ppm = tx.freq as f64 / 65536.0;
    let user_hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let tick_nominal = if user_hz > 0 { 1_000_000 / user_hz } else { 10_000 };
    let tick_ppm = if tick_nominal > 0 {
        (tx.tick - tick_nominal) as f64 * 1_000_000.0 / tick_nominal as f64
    } else {
        0.0
    };

    // `as i64` / `as f64`: timex fields are `c_long`, whose width varies by
    // target (i64 on lp64); the casts keep this width-agnostic.
    Some(ClockSyncStatus {
        state,
        unsync,
        max_error_us: Some(tx.maxerror as i64),
        est_error_us: Some(tx.esterror as i64),
        freq_ppm: Some(tick_ppm + fine_ppm),
        offset_us: Some(tx.offset as i64),
    })
}

/// `adjtimex` is Linux-only; non-Linux hosts advertise no clock-sync card.
#[cfg(not(target_os = "linux"))]
pub fn probe() -> Option<ClockSyncStatus> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn probe_reads_without_capability() {
        // adjtimex(modes=0) needs no CAP_SYS_TIME → must return Some on
        // Linux CI. The kernel clamps |freq| to ~500 ppm; assert it is at
        // least within a sane sanity bound.
        let s = probe().expect("adjtimex read should succeed on Linux");
        if let Some(ppm) = s.freq_ppm {
            assert!(ppm.abs() < 10_000.0, "freq_ppm out of range: {ppm}");
        }
    }
}
