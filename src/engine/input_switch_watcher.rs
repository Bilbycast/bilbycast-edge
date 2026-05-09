// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-output "input switched" notifier.
//!
//! Each transcoding output (UDP / RTP / SRT / RIST that builds a
//! `TsVideoReplacer` or `TsAudioReplacer`) registers the replacers'
//! `external_reset_handle()` flags here and gets a small spawned task
//! that listens on the flow's `active_input_rx` watch channel. On every
//! change, the task flips every registered flag — the replacers
//! consume the flag at the top of their next `process()` call and run
//! `reset_source_state()`, the same path that already fires on a
//! codec/PID change.
//!
//! Why this exists: the codec/PID-change reset path inside the
//! replacers does not fire when the operator switches between two
//! inputs that share a codec and PID layout (the camera-A↔camera-B
//! case). Without an external trigger the replacers keep the previous
//! input's PTS anchor and emit output PTSes that no longer line up
//! with the master-clock-paced output PCR. Receivers see PTS values
//! outside their STC window and stay stuck. This module is the
//! external trigger.
//!
//! The hot path is untouched — the watcher sleeps in `changed().await`
//! until an operator switches inputs (rare), then does one
//! `Relaxed` atomic store per registered handle. The replacers'
//! per-`process()` check is also `Relaxed` and collapses rapid repeat
//! switches into a single reset.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Spawn a small task that flips every handle in `handles` to `true`
/// on every change of the flow's active input, until `cancel` fires
/// or the watch sender is dropped.
///
/// `handles` is consumed; pass empty to make this a no-op (the spawn
/// is skipped). Caller is responsible for dropping the returned
/// JoinHandle if it doesn't need to await it.
pub fn spawn(
    output_id: String,
    mut active_input_rx: watch::Receiver<String>,
    handles: Vec<Arc<AtomicBool>>,
    cancel: CancellationToken,
) {
    if handles.is_empty() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                changed = active_input_rx.changed() => {
                    if changed.is_err() {
                        // Sender dropped — flow is shutting down.
                        break;
                    }
                    let new_id = active_input_rx.borrow().clone();
                    tracing::debug!(
                        "input_switch_watcher: output '{output_id}' active input -> '{new_id}', \
                         flagging {} replacer reset handle(s)",
                        handles.len()
                    );
                    for h in &handles {
                        h.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    });
}
