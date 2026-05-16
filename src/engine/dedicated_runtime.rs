// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-task dedicated `tokio::current_thread` runtime on an OS thread
//! with `SCHED_FIFO` + optional CPU pinning.
//!
//! Stages 2 and 3 of the data-plane redesign lift the PID-bus demuxer
//! + assembler (Stage 2) and the PCR PLL sampler (Stage 3) off the
//! main Tokio worker pool. The minimum-disruption way to do that is
//! **not** to rewrite each component as synchronous OS-thread code —
//! the assembler alone is ~2000 lines of `async` with `tokio::select!`
//! over multiple channels — but to give each its own `current_thread`
//! tokio runtime running on a dedicated OS thread.
//!
//! Compared to `tokio::spawn` on the default multi-thread runtime:
//! - the spawned future never competes with main-runtime tasks for a
//!   worker (no work-stealing churn);
//! - the thread can hold `SCHED_FIFO` priority so kernel scheduling
//!   prefers it over any SCHED_OTHER load;
//! - the thread can be pinned to a dedicated CPU via
//!   `BILBYCAST_PID_BUS_CPUS` or `BILBYCAST_PLL_CPUS`, ensuring it
//!   doesn't share a core with the main runtime's worker pool;
//! - the future is `'static` and runs to completion on its own thread,
//!   so a panic doesn't poison the main runtime.
//!
//! Caveat: each spawned future gets its own thread + runtime, so this
//! is intended for **long-lived components** (one per flow, not per
//! packet). The codec-thread path uses raw `std::thread` for the same
//! reason; here we add `tokio::current_thread` runtime on top because
//! the spawned code is already async and we don't want to rewrite it.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

use crate::util::runtime_diag::{apply_cpu_pinning, apply_sched_fifo, parse_cpu_set_env};

/// Default SCHED_FIFO priority used when the caller doesn't override.
/// `35` sits between the wire-emit thread (50) and the codec thread
/// (40), so prioritisation across the data-plane fleet is:
///   wire-emit (50)  >  codec (40)  >  pid-bus / pll (35)  >  Tokio
/// Putting the PID-bus + PLL just below the codec threads is right
/// because the codec produces output the PID-bus consumes, and the
/// PID-bus produces output the wire-emit consumes — a higher-priority
/// downstream thread should be able to preempt slower upstream work
/// when there's bytes ready to go on the wire.
pub const DEDICATED_DEFAULT_PRIORITY: i32 = 35;

/// Build configuration for one dedicated-runtime thread.
#[derive(Clone, Debug)]
pub struct DedicatedRuntimeConfig {
    /// Human-readable thread role used in logs.
    pub who: String,
    /// SCHED_FIFO priority (1..=99). `None` to keep SCHED_OTHER.
    pub sched_fifo_priority: Option<i32>,
    /// Env var that the round-robin CPU pinning consults at startup
    /// (e.g. `"BILBYCAST_PID_BUS_CPUS"`). Empty / unset means no
    /// pinning. Each unique `cpu_env_var` value gets its own
    /// `OnceLock<Vec<usize>>` cache so different env vars don't share
    /// a round-robin counter.
    pub cpu_env_var: &'static str,
}

impl DedicatedRuntimeConfig {
    pub fn new(who: impl Into<String>, cpu_env_var: &'static str) -> Self {
        Self {
            who: who.into(),
            sched_fifo_priority: Some(DEDICATED_DEFAULT_PRIORITY),
            cpu_env_var,
        }
    }
}

/// Spawn `future` on a dedicated OS thread with `SCHED_FIFO` + optional
/// CPU pinning, running on its own `tokio::runtime::Builder::
/// new_current_thread()` runtime. Returns the `JoinHandle` for the OS
/// thread; calling `.join()` waits for the future to complete.
///
/// The future is **moved** to the new thread, so all of its captures
/// must be `Send`. The returned thread keeps polling the future until
/// it resolves (typically when a CancellationToken passed inside the
/// future fires).
pub fn spawn_dedicated<F>(config: DedicatedRuntimeConfig, future: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let who = config.who.clone();
    let prio = config.sched_fifo_priority;
    let cpu_env_var = config.cpu_env_var;
    let cpu_index = next_cpu_index(cpu_env_var);
    std::thread::Builder::new()
        .name(format!("dedicated-{who}"))
        .spawn(move || {
            let _sched_fifo = match prio {
                Some(p) => apply_sched_fifo(&who, p),
                None => false,
            };
            let _pinned = apply_cpu_pinning(&who, cpu_index);
            tracing::info!(
                "dedicated-runtime '{who}' starting (sched_fifo_prio={:?}, pinned_cpu={:?})",
                prio, cpu_index
            );
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_name(format!("dedicated-{who}-rt"))
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(
                        "dedicated-runtime '{who}': failed to build tokio runtime: {e}"
                    );
                    return;
                }
            };
            rt.block_on(future);
            tracing::debug!("dedicated-runtime '{who}' future completed; thread exiting");
        })
        .expect("dedicated runtime thread spawn")
}

/// Cached per-env-var CPU pinning sets + round-robin counters. Sharing
/// would let unrelated workloads steal one another's pin slots, so we
/// keep them separate.
fn next_cpu_index(env_var: &'static str) -> Option<usize> {
    static CACHE: OnceLock<
        std::sync::Mutex<std::collections::HashMap<&'static str, (Vec<usize>, AtomicUsize)>>,
    > = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock().ok()?;
    let entry = guard
        .entry(env_var)
        .or_insert_with(|| (parse_cpu_set_env(env_var), AtomicUsize::new(0)));
    if entry.0.is_empty() {
        return None;
    }
    let i = entry.1.fetch_add(1, Ordering::Relaxed) % entry.0.len();
    Some(entry.0[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn future_runs_to_completion_on_dedicated_thread() {
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let ran2 = ran.clone();
        let handle = spawn_dedicated(
            DedicatedRuntimeConfig {
                who: "test-dedicated".into(),
                sched_fifo_priority: None,
                cpu_env_var: "BILBYCAST_TEST_DEDICATED_CPUS_DOES_NOT_EXIST",
            },
            async move {
                tokio::task::yield_now().await;
                ran2.store(true, Ordering::Relaxed);
            },
        );
        handle.join().unwrap();
        assert!(ran.load(Ordering::Relaxed));
    }
}
