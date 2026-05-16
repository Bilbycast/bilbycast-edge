// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Dedicated OS-thread runtime for codec work (encoders, decoders,
//! transcoders).
//!
//! The data-plane hot path's most CPU-intensive work — x264 / x265 /
//! NVENC / QSV / VAAPI video encode, fdk-aac / libavcodec audio encode,
//! libavcodec video decode for transcode + content analysis — runs on
//! these threads, not on Tokio workers via `tokio::task::block_in_place`
//! or `tokio::task::spawn_blocking`.
//!
//! Why: `block_in_place` marks the current Tokio worker as blocking and
//! forces the runtime to recruit a replacement, causing work-stealing
//! churn that adds 100s of µs to ms of scheduling jitter to *every other
//! task* on the runtime — including the per-flow PCR PLL sampler, the
//! master-clock samplers, and the wire-tx producers that feed the
//! `engine::wire_emit` thread. By moving encoder calls onto dedicated
//! OS threads with `SCHED_FIFO` priority 40 (one below wire-emit's 50,
//! so wire pacing always wins) and optional CPU pinning, the encoder
//! becomes invisible to the Tokio runtime — no churn, no jitter
//! leakage.
//!
//! ## Lifecycle model
//!
//! One codec thread per transcoded output (audio or video). The thread
//! is spawned by the output's `spawn_*` function when the output starts
//! and exits when the output stops. The thread owns the encoder /
//! decoder context for its entire lifetime — no per-frame state
//! transfer across the thread boundary.
//!
//! ## Channel pattern (recommended)
//!
//! Output task ⇄ codec thread communicate via two `tokio::sync::mpsc`
//! channels:
//!   - `frame_tx` (output task → codec thread): bounded, drop-on-full.
//!     The codec thread receives via `blocking_recv()` which doesn't
//!     touch the Tokio runtime.
//!   - `result_tx` (codec thread → output task): bounded, drop-on-full
//!     on the codec side. The output task receives via async `recv()`.
//!
//! This combination keeps the codec thread blocking (good — no Tokio
//! reactor wake jitter) while the output task stays fully async (good
//! for everything else it does — broadcast subscribe, socket send,
//! state machine).
//!
//! ## Priority + pinning
//!
//! - `SCHED_FIFO` priority 40 by default — one below wire-emit's 50.
//!   The wire-emit thread always wins when both are runnable, so codec
//!   work cannot delay wire-time pacing.
//! - CPU pinning: round-robin across `BILBYCAST_CODEC_CPUS` (parsed
//!   exactly like `BILBYCAST_WIRE_EMIT_CPUS`). Operators should place
//!   codec cores on a *different* set than wire-emit cores — codec
//!   work is CPU-heavy and would crowd wire-emit's responsiveness if
//!   they shared cores.
//!
//! ## What this module IS NOT
//!
//! - **Not a job pool with closure submission**. Encoder contexts are
//!   not `Send` across per-frame boundaries cheaply; instead each thread
//!   owns its context for the output's lifetime and processes frames in
//!   a long-running loop.
//! - **Not a thread pool with fixed size**. The thread count grows with
//!   transcoded outputs, bounded externally by the resource budget
//!   (cost-units + per-family HW session limits). The aggregate live
//!   count is surfaced on `HealthPayload.resource_budget.threads.
//!   codec_pool_count` so operators can spot leaks.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

use crate::util::runtime_diag::{
    apply_cpu_pinning, apply_sched_fifo, note_codec_thread_exited,
    note_codec_thread_spawned, parse_cpu_set_env,
};

/// Default SCHED_FIFO priority for codec threads. One below wire-emit's
/// 50 so wire-time pacing always preempts codec work — encoder runs
/// cannot delay PCR-bearing packet emission.
pub const CODEC_SCHED_FIFO_PRIORITY: i32 = 40;

/// Configuration for a single codec thread. The `who` field is the
/// human-readable role used in logs (e.g. `"video-encode-out1"`, not a
/// thread name — that's set automatically as `codec-{who}`).
#[derive(Clone, Debug)]
pub struct CodecThreadConfig {
    pub who: String,
    /// SCHED_FIFO priority (1..=99). `None` keeps SCHED_OTHER (default
    /// scheduler) — useful for non-realtime auxiliary codec work like
    /// content-analysis sampling.
    pub sched_fifo_priority: Option<i32>,
}

impl CodecThreadConfig {
    /// Default real-time codec config: SCHED_FIFO 40 + round-robin CPU
    /// pinning from `BILBYCAST_CODEC_CPUS`.
    pub fn realtime(who: impl Into<String>) -> Self {
        Self {
            who: who.into(),
            sched_fifo_priority: Some(CODEC_SCHED_FIFO_PRIORITY),
        }
    }
}

/// Spawn a codec thread that runs `work` until the closure returns.
/// The thread owns its encoder/decoder context for the entire lifetime
/// of `work`. Channels (frame in, encoded out) live in `work`'s scope.
///
/// Returns the `JoinHandle` so the caller can `.join()` on shutdown.
/// Most callers can drop the handle — the thread will exit when the
/// closure returns (typically when its input channel is closed via
/// the output's CancellationToken triggering channel-sender drop).
///
/// The thread is registered in the global codec-thread count via
/// `note_codec_thread_spawned` / `note_codec_thread_exited`, surfaced
/// on `HealthPayload.resource_budget.threads.codec_pool_count` so the
/// manager can show the live codec fleet without crawling
/// `/proc/<pid>/task/`.
pub fn spawn_codec_thread<F>(config: CodecThreadConfig, work: F) -> JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    let who = config.who.clone();
    let prio = config.sched_fifo_priority;
    let cpu_index = next_codec_cpu_index();
    std::thread::Builder::new()
        .name(format!("codec-{who}"))
        .spawn(move || {
            note_codec_thread_spawned();
            // Apply SCHED_FIFO first; pinning is best-effort and
            // doesn't affect correctness even on failure. Both helpers
            // log warn-level on failure so operators see degraded
            // tuning without log diving.
            let _sched_fifo = match prio {
                Some(p) => apply_sched_fifo(&who, p),
                None => false,
            };
            let _pinned = apply_cpu_pinning(&who, cpu_index);
            tracing::info!(
                "codec '{who}' thread starting (sched_fifo_prio={:?}, pinned_cpu={:?})",
                prio, cpu_index
            );
            work();
            tracing::debug!("codec '{who}' thread exited");
            note_codec_thread_exited();
        })
        .expect("codec thread spawn")
}

/// Round-robin next CPU from `BILBYCAST_CODEC_CPUS`. Separate from the
/// wire-emit CPU set so operators can dedicate distinct cores to
/// wire-pacing vs. encoding.
fn next_codec_cpu_index() -> Option<usize> {
    static SET: OnceLock<Vec<usize>> = OnceLock::new();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let set = SET.get_or_init(|| parse_cpu_set_env("BILBYCAST_CODEC_CPUS"));
    if set.is_empty() {
        return None;
    }
    let i = COUNTER.fetch_add(1, Ordering::Relaxed) % set.len();
    Some(set[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::runtime_diag;

    #[test]
    fn spawn_runs_closure_and_increments_count() {
        let before = runtime_diag::codec_thread_count();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let handle = spawn_codec_thread(
            CodecThreadConfig { who: "test-codec".into(), sched_fifo_priority: None },
            move || {
                done_tx.send(()).unwrap();
            },
        );
        done_rx.recv().unwrap();
        handle.join().unwrap();
        // After join, the spawned-counter must have come back to the
        // pre-spawn value. Allow for races by retrying briefly.
        let mut attempts = 0;
        while runtime_diag::codec_thread_count() != before && attempts < 50 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            attempts += 1;
        }
        assert_eq!(runtime_diag::codec_thread_count(), before);
    }
}
