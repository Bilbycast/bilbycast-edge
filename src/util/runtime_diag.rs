// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Process-wide runtime diagnostics for the data-plane hot path.
//!
//! Tracks state that the operator needs to see surfaced on
//! `HealthPayload.scheduling_status` — `mlockall` outcome at startup,
//! the effective `RLIMIT_RTPRIO` ceiling, and whether at least one
//! wire-emit thread successfully acquired `SCHED_FIFO`. These together
//! tell the operator at a glance whether the edge is running with the
//! latency budget the design targets (sub-µs SO_TXTIME on a PTP NIC,
//! ~50–500 µs `clock_nanosleep` on SCHED_FIFO) or has silently fallen
//! back to default scheduling (~1–5 ms p99 — degraded broadcast quality).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// `mlockall` outcome captured once at startup. Static for process
/// lifetime — there is no useful unlock path on this hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MlockallStatus {
    /// `BILBYCAST_MLOCKALL` was not set to `1`, so the hook didn't run.
    /// Default for development and testbed setups — production deploys
    /// via the systemd unit set the env var.
    Disabled,
    /// `mlockall(MCL_CURRENT | MCL_FUTURE)` returned 0. All current
    /// allocations and every future page touched will be locked into
    /// RAM, eliminating major page-fault stalls on the hot path.
    Locked,
    /// `mlockall` was attempted but failed. Carries the raw errno so the
    /// operator can distinguish EPERM (no CAP_IPC_LOCK and RLIMIT_MEMLOCK
    /// not high enough) from ENOMEM (the configured limit was reached).
    Failed { errno: i32 },
}

static MLOCKALL_STATUS: OnceLock<MlockallStatus> = OnceLock::new();
static SCHED_FIFO_GRANTED_ANY: AtomicBool = AtomicBool::new(false);
static SCHED_FIFO_FAILED_ANY: AtomicBool = AtomicBool::new(false);
static RLIMIT_RTPRIO_MAX: AtomicU32 = AtomicU32::new(0);

/// Attempt `mlockall(MCL_CURRENT | MCL_FUTURE)` on Linux. On any other
/// platform this is a no-op that records `Disabled` — there's no
/// equivalent system call we can reasonably target.
///
/// Safe to call exactly once at startup. The result is cached and
/// readable via [`mlockall_status`] for the rest of the process
/// lifetime.
#[cfg(target_os = "linux")]
pub fn try_mlockall_at_startup() -> MlockallStatus {
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    let status = if rc == 0 {
        MlockallStatus::Locked
    } else {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        MlockallStatus::Failed { errno }
    };
    let _ = MLOCKALL_STATUS.set(status);
    status
}

#[cfg(not(target_os = "linux"))]
pub fn try_mlockall_at_startup() -> MlockallStatus {
    let _ = MLOCKALL_STATUS.set(MlockallStatus::Disabled);
    MlockallStatus::Disabled
}

/// Record that the env-var gate was not set, so `mlockall` was
/// deliberately skipped. Distinct from `Failed` so the manager UI can
/// show "not attempted" without flagging an error.
pub fn note_mlockall_disabled() {
    let _ = MLOCKALL_STATUS.set(MlockallStatus::Disabled);
}

/// Return the captured `mlockall` status. Returns `Disabled` until
/// either [`try_mlockall_at_startup`] or [`note_mlockall_disabled`] has
/// been called.
pub fn mlockall_status() -> MlockallStatus {
    MLOCKALL_STATUS.get().copied().unwrap_or(MlockallStatus::Disabled)
}

/// Report a SCHED_FIFO acquisition outcome from a wire-emit (or any
/// other hot-path) thread. Aggregated across threads so the manager
/// can show one badge — green if all threads got SCHED_FIFO, red if any
/// failed. Failure on even one thread degrades the entire flow's
/// timing precision, so the failure latch is sticky.
pub fn record_sched_fifo_outcome(granted: bool) {
    if granted {
        SCHED_FIFO_GRANTED_ANY.store(true, Ordering::Relaxed);
    } else {
        SCHED_FIFO_FAILED_ANY.store(true, Ordering::Relaxed);
    }
}

/// `true` if at least one wire-emit thread successfully acquired
/// `SCHED_FIFO`. Default `false` until the first wire-emit thread
/// reports.
pub fn sched_fifo_granted_any() -> bool {
    SCHED_FIFO_GRANTED_ANY.load(Ordering::Relaxed)
}

/// `true` if any thread attempted to acquire `SCHED_FIFO` and failed.
/// Sticky — once latched, stays `true` for the rest of the process
/// lifetime. The manager UI uses this to surface a Critical-level
/// warning on the per-node Resources card.
pub fn sched_fifo_failed_any() -> bool {
    SCHED_FIFO_FAILED_ANY.load(Ordering::Relaxed)
}

/// Capture the effective `RLIMIT_RTPRIO` ceiling once at startup. The
/// kernel allows `sched_setscheduler(SCHED_FIFO)` at any priority
/// within `[1, rlim_cur]` without `CAP_SYS_NICE`. `0` means SCHED_FIFO
/// is denied for the running user; `50` (systemd default in our unit)
/// allows priority 1..=50.
#[cfg(target_os = "linux")]
pub fn capture_rlimit_rtprio() -> u32 {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut rl) };
    let max = if rc == 0 {
        // `rlim_cur` can be RLIM_INFINITY (u64::MAX on glibc). The
        // kernel itself only honours up to 99 for SCHED_FIFO, so
        // clamping is informational, not load-bearing.
        std::cmp::min(rl.rlim_cur as u64, 99) as u32
    } else {
        0
    };
    RLIMIT_RTPRIO_MAX.store(max, Ordering::Relaxed);
    max
}

#[cfg(not(target_os = "linux"))]
pub fn capture_rlimit_rtprio() -> u32 {
    0
}

/// Read the captured `RLIMIT_RTPRIO` ceiling. `0` until
/// [`capture_rlimit_rtprio`] has been called or if the syscall failed.
pub fn rlimit_rtprio_max() -> u32 {
    RLIMIT_RTPRIO_MAX.load(Ordering::Relaxed)
}

/// Probe whether the current process can ACTUALLY acquire `SCHED_FIFO`
/// at our wire-emit priority (50), regardless of `RLIMIT_RTPRIO`. The
/// `CAP_SYS_NICE` capability lets a process call
/// `sched_setscheduler(SCHED_FIFO)` even when `RLIMIT_RTPRIO==0` (e.g.
/// `setcap cap_sys_nice=eip ./bilbycast-edge`), so a self-test on a
/// transient thread is the only reliable indicator. Restores
/// `SCHED_OTHER` before returning so the test thread leaves no
/// scheduler imprint.
///
/// Cheap — one `pthread_create` + 2 syscalls + join. Used by the
/// `main.rs` preflight to decide whether to emit the CRITICAL
/// scheduling warning.
#[cfg(target_os = "linux")]
pub fn sched_fifo_self_test_can_acquire() -> bool {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let h = std::thread::Builder::new()
        .name("sched-fifo-probe".to_string())
        .spawn(move || {
            let mut sp: libc::sched_param = unsafe { std::mem::zeroed() };
            sp.sched_priority = 50;
            let rc = unsafe {
                libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &sp)
            };
            // Best-effort restore; ignore errors (thread exits anyway).
            let mut sp_otherw: libc::sched_param = unsafe { std::mem::zeroed() };
            sp_otherw.sched_priority = 0;
            unsafe {
                let _ = libc::pthread_setschedparam(
                    libc::pthread_self(),
                    libc::SCHED_OTHER,
                    &sp_otherw,
                );
            }
            let _ = tx.send(rc == 0);
        });
    match h {
        Ok(handle) => {
            let result = rx.recv().unwrap_or(false);
            let _ = handle.join();
            result
        }
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sched_fifo_self_test_can_acquire() -> bool {
    false
}

/// Apply `SCHED_FIFO` at `priority` (1..=99) to the calling thread.
/// Returns `true` on success, `false` on any failure (logged warn-level
/// so operators see it). On failure the thread continues at
/// `SCHED_OTHER`, which is the default scheduler — functional but with
/// 1–5 ms p99 latency tails under load.
///
/// `who` is the human-readable thread role used in log messages (e.g.
/// "wire-emit-flow1", "codec-h264-out2"). It is **not** a thread name
/// and need not match `Builder::name()`.
#[cfg(target_os = "linux")]
pub fn apply_sched_fifo(who: &str, priority: i32) -> bool {
    let mut sp: libc::sched_param = unsafe { std::mem::zeroed() };
    sp.sched_priority = priority;
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &sp) };
    if rc == 0 {
        tracing::debug!("{}: SCHED_FIFO priority {} acquired", who, priority);
        record_sched_fifo_outcome(true);
        true
    } else {
        let rtprio = rlimit_rtprio_max();
        tracing::warn!(
            "{}: SCHED_FIFO priority {} denied (rc={}, RLIMIT_RTPRIO max={}) — \
             running at SCHED_OTHER; p99 latency tails expected to grow into the \
             1–5 ms range under load. Remediation: systemd unit `LimitRTPRIO=99` + \
             `RestrictRealtime=false` (production unit ships these), or grant \
             `CAP_SYS_NICE` to the process",
            who, priority, rc, rtprio,
        );
        record_sched_fifo_outcome(false);
        false
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply_sched_fifo(_who: &str, _priority: i32) -> bool {
    // Non-Linux: no SCHED_FIFO equivalent in this codebase.
    record_sched_fifo_outcome(false);
    false
}

/// Pin the calling thread to `cpu` via `pthread_setaffinity_np`.
/// Returns `Some(cpu)` on success, `None` if pinning was not requested
/// or the syscall failed.
///
/// Failures are warn-level — the thread keeps running on the kernel's
/// default placement, which may cause contention with Tokio workers
/// but does not stop it functioning.
#[cfg(target_os = "linux")]
pub fn apply_cpu_pinning(who: &str, cpu: Option<usize>) -> Option<u32> {
    let cpu = cpu?;
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut set) };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    let rc = unsafe {
        libc::pthread_setaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &set,
        )
    };
    if rc == 0 {
        tracing::debug!("{}: pinned to CPU {}", who, cpu);
        Some(cpu as u32)
    } else {
        tracing::warn!(
            "{}: pthread_setaffinity_np(CPU={}) failed: rc={} — thread will float \
             across all cores (Tokio worker contention possible)",
            who, cpu, rc,
        );
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply_cpu_pinning(_who: &str, _cpu: Option<usize>) -> Option<u32> {
    None
}

/// Parse an `OneCell` CPU-set env var once into a stable `Vec<usize>`.
/// Accepts the same syntax for every hot-path env var:
/// `"2"` / `"2,3,5"` / `"2-5"` / `"2,4-6,9"`. Invalid entries log a
/// warning and are skipped. Sorted + deduplicated.
pub fn parse_cpu_set_env(env_var: &'static str) -> Vec<usize> {
    let raw = match std::env::var(env_var) {
        Ok(v) if !v.is_empty() => v,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for tok in raw.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        if let Some((lo, hi)) = tok.split_once('-') {
            match (lo.trim().parse::<usize>(), hi.trim().parse::<usize>()) {
                (Ok(a), Ok(b)) if a <= b => out.extend(a..=b),
                _ => tracing::warn!(
                    "{}: invalid range '{tok}' — skipped",
                    env_var
                ),
            }
        } else {
            match tok.parse::<usize>() {
                Ok(n) => out.push(n),
                Err(_) => tracing::warn!(
                    "{}: invalid CPU '{tok}' — skipped",
                    env_var
                ),
            }
        }
    }
    out.sort();
    out.dedup();
    if !out.is_empty() {
        tracing::info!(
            "{}: CPU set parsed as {:?} (round-robin across spawns)",
            env_var, out
        );
    }
    out
}

/// Active codec-thread count. Incremented on
/// [`note_codec_thread_spawned`], decremented on
/// [`note_codec_thread_exited`]. Surfaced on
/// `HealthPayload.resource_budget.threads.codec_pool_count` so the
/// manager UI can show the live codec-thread fleet.
static CODEC_THREAD_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn note_codec_thread_spawned() {
    CODEC_THREAD_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn note_codec_thread_exited() {
    CODEC_THREAD_COUNT.fetch_sub(1, Ordering::Relaxed);
}

pub fn codec_thread_count() -> u32 {
    CODEC_THREAD_COUNT.load(Ordering::Relaxed)
}
