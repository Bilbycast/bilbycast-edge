// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-input ingress publisher — the per-input buffering front-end that
//! sits between an input task and the flow's broadcast channel. It selects
//! one of three release modes:
//!
//! - **`Direct`** (default) — publish immediately, zero overhead.
//! - **`Delayed`** (per-input `ingress_delay_ms`) — a **fixed delay line**:
//!   each packet is released at exactly `recv_time_us + delay_ms`. This
//!   shifts the whole timeline by a constant but reproduces the
//!   inter-arrival spacing exactly, so it does **NOT** remove jitter. It is
//!   a deterministic alignment / cross-device-sync tool — the input-side
//!   counterpart to the per-output `OutputDelay::Fixed { ms }`, but applied
//!   *before* fan-out so every consumer (TR-101290, content analysis,
//!   thumbnail, replay, PID bus, PCR PLL, all outputs) shares the same
//!   shifted timeline.
//! - **`Dejitter`** (per-input `ingress_dejitter_ms`) — hands off to the
//!   [`crate::engine::ingress_dejitter`] release-rate servo, which paces
//!   release at the recovered source rate and **actually absorbs network
//!   packet-delay variation (PDV)**. This is the real broadcast-grade input
//!   buffer. It supersedes `Delayed` when both are configured.
//!
//! ## Delay vs. de-jitter — pick by intent
//!
//! Reach for the fixed **delay** only when you need a precise, constant,
//! deterministic offset (aligning two feeds / two edges). For absorbing
//! network jitter on a contribution feed, use **de-jitter** — its whole
//! point is to decouple output cadence from arrival, which is the *opposite*
//! of holding a fixed relative offset. SRT/RIST/WebRTC inputs already
//! re-time at the transport, so a fixed delay on top of them is rarely
//! useful (and on SRT we warn when both `delay` and `SRT_LATENCY` are set).
//!
//! ## Implementation
//!
//! `Delayed` reuses [`crate::engine::delay_buffer::DelayBuffer`] (the same
//! ring + release-time machinery that powers per-output delay) and drives a
//! dedicated drainer task per input that publishes onto the flow's broadcast
//! channel; `Dejitter` drives the servo drainer in
//! [`crate::engine::ingress_dejitter`]. When neither knob is set the handle
//! is a thin shim that calls `broadcast::Sender::send` directly — no task,
//! no allocation, no measurable overhead.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::engine::delay_buffer::DelayBuffer;
use crate::engine::packet::RtpPacket;
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::time::now_us;

/// Per-input ingress publisher. Either publishes immediately to the
/// flow broadcast channel (when no buffer is configured) or hands off to
/// a drainer task that releases packets at their scheduled time.
///
/// Cheap to clone — the inner state is `Arc`-wrapped.
#[derive(Clone)]
pub struct IngressPublisher {
    inner: Arc<IngressPublisherInner>,
}

enum IngressPublisherInner {
    /// Delay/de-jitter disabled. Direct publish to broadcast.
    Direct(broadcast::Sender<RtpPacket>),
    /// Fixed-delay enabled. Submit via mpsc; drainer task owns the
    /// `DelayBuffer` and the `broadcast::Sender`, releasing at
    /// `recv_time_us + delay_ms` (preserves inter-arrival spacing — a
    /// delay line, not a de-jitter buffer).
    Delayed {
        submit: mpsc::Sender<RtpPacket>,
        delay_ms: u16,
    },
    /// De-jitter enabled. Submit via mpsc; the
    /// [`crate::engine::ingress_dejitter`] drainer task owns the buffer +
    /// release-rate servo + the `broadcast::Sender` and releases packets
    /// paced at the recovered source rate (actually removes PDV).
    Dejitter {
        submit: mpsc::Sender<RtpPacket>,
    },
}

impl IngressPublisher {
    /// Build an `IngressPublisher`. Mode precedence:
    ///
    /// 1. `dejitter_ms` set (> 0) → **de-jitter** drainer (rate-paced
    ///    release; the real broadcast-grade buffer that absorbs PDV).
    ///    Supersedes the fixed delay — if both are set, de-jitter wins and
    ///    a warning is logged.
    /// 2. `delay_ms` set (> 0) → **fixed-delay** drainer (constant offset,
    ///    does not remove jitter).
    /// 3. neither → direct passthrough (zero overhead).
    ///
    /// Any spawned drainer task exits when either `cancel` fires or the
    /// `IngressPublisher` (and all its clones) are dropped, closing the
    /// mpsc.
    pub fn new(
        delay_ms: Option<u16>,
        dejitter_ms: Option<u32>,
        broadcast_tx: broadcast::Sender<RtpPacket>,
        input_id: &str,
        cancel: CancellationToken,
        stats: Arc<FlowStatsAccumulator>,
    ) -> Self {
        if let Some(dj) = dejitter_ms.filter(|v| *v > 0) {
            if delay_ms.filter(|v| *v > 0).is_some() {
                tracing::warn!(
                    "input '{input_id}': both ingress_dejitter_ms={dj} and ingress_delay_ms \
                     set — de-jitter wins (it supersedes the fixed-delay buffer; \
                     unset ingress_delay_ms to silence this)"
                );
            }
            let cfg = crate::engine::ingress_dejitter::IngressDejitterConfig::from_ms(Some(dj));
            let submit = crate::engine::ingress_dejitter::start(
                broadcast_tx,
                cfg,
                input_id,
                stats,
                cancel,
            );
            return Self {
                inner: Arc::new(IngressPublisherInner::Dejitter { submit }),
            };
        }
        let ms = delay_ms.unwrap_or(0);
        if ms == 0 {
            return Self {
                inner: Arc::new(IngressPublisherInner::Direct(broadcast_tx)),
            };
        }
        // Cap capacity so a stalled drainer task can't grow unbounded.
        // 4096 raw packets ≈ ~170 ms of buffer at 500 Mbps — generous
        // headroom on top of the `delay_ms` envelope so transient
        // input bursts don't drop at the mpsc boundary.
        let (tx, rx) = mpsc::channel::<RtpPacket>(4096);
        let input_id = input_id.to_string();
        tokio::spawn(run_drainer(rx, broadcast_tx, ms, input_id, cancel));
        Self {
            inner: Arc::new(IngressPublisherInner::Delayed {
                submit: tx,
                delay_ms: ms,
            }),
        }
    }

    /// Publish a packet. Non-blocking; returns `true` if accepted.
    /// On the delay / de-jitter path, returns `false` when the drainer's
    /// mpsc is full (caller increments their drop counter).
    pub fn send(&self, packet: RtpPacket) -> bool {
        match self.inner.as_ref() {
            IngressPublisherInner::Direct(tx) => tx.send(packet).is_ok(),
            IngressPublisherInner::Delayed { submit, .. } => submit.try_send(packet).is_ok(),
            IngressPublisherInner::Dejitter { submit } => submit.try_send(packet).is_ok(),
        }
    }

    /// Returns the configured fixed-delay duration in milliseconds.
    /// `0` means the fixed-delay path is not active (direct or
    /// de-jitter publish path).
    #[allow(dead_code)]
    pub fn delay_ms(&self) -> u16 {
        match self.inner.as_ref() {
            IngressPublisherInner::Direct(_) => 0,
            IngressPublisherInner::Delayed { delay_ms, .. } => *delay_ms,
            IngressPublisherInner::Dejitter { .. } => 0,
        }
    }
}

/// Fixed-delay drainer task that owns the `DelayBuffer` and the broadcast
/// sender. Periodically wakes to release any packets whose
/// `recv_time_us + delay_us` has passed, forwarding each to the
/// broadcast channel. (Jitter absorption is the separate
/// [`crate::engine::ingress_dejitter`] servo drainer; this one only delays.)
async fn run_drainer(
    mut rx: mpsc::Receiver<RtpPacket>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    delay_ms: u16,
    input_id: String,
    cancel: CancellationToken,
) {
    let delay_us = delay_ms as u64 * 1000;
    // DelayBuffer auto-capacity rule: 2 000 pkt/s × (delay + 2 s)
    // headroom. Same heuristic the per-output delay buffer uses.
    let max_capacity = DelayBuffer::auto_capacity(delay_ms as u64);
    let mut buffer = DelayBuffer::new(delay_ms as u64, max_capacity);
    let release_sleep = tokio::time::sleep(std::time::Duration::from_secs(86400));
    tokio::pin!(release_sleep);
    loop {
        if let Some(release_us) = buffer.next_release_time() {
            let now = now_us();
            let wait = release_us.saturating_sub(now);
            release_sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + std::time::Duration::from_micros(wait));
        } else {
            // Park release_sleep far in the future so it never fires
            // while the buffer is empty.
            release_sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + std::time::Duration::from_secs(86400));
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(
                    "ingress-delay '{input_id}' draining + exiting (cancelled, delay={delay_ms} ms)"
                );
                break;
            }
            r = rx.recv() => match r {
                Some(p) => buffer.push(p),
                None => {
                    // All senders dropped — flush remaining and exit.
                    for p in buffer.drain_ready(u64::MAX) {
                        let _ = broadcast_tx.send(p);
                    }
                    break;
                }
            },
            _ = &mut release_sleep, if buffer.len() > 0 => {
                let now = now_us();
                for p in buffer.drain_ready(now) {
                    let _ = broadcast_tx.send(p);
                }
            }
        }
    }
    let _ = delay_us; // kept for log clarity in future tracing
}

/// Emit a Warning event at startup when a fixed ingress delay is set
/// on an SRT input that already has protocol-level latency
/// configured — operators who set both are usually double-buffering.
/// Called by [`crate::engine::input_srt::spawn_srt_input`] at startup.
pub fn warn_if_double_buffer_srt(
    input_id: &str,
    flow_id: &str,
    ingress_delay_ms: Option<u16>,
    srt_latency_ms: Option<u32>,
    events: &crate::manager::events::EventSender,
) {
    if let (Some(s), Some(l)) = (ingress_delay_ms.filter(|v| *v > 0), srt_latency_ms.filter(|v| *v > 0)) {
        let msg = format!(
            "SRT input '{input_id}': ingress_delay_ms={s} is layered on top of SRT \
             latency={l} ms (libsrt's native jitter buffer); this is usually redundant. \
             Consider unsetting one of the two."
        );
        tracing::warn!("{msg}");
        events.emit_flow_with_details(
            crate::manager::events::EventSeverity::Warning,
            crate::manager::events::category::SRT,
            msg,
            flow_id,
            serde_json::json!({
                "input_id": input_id,
                "ingress_delay_ms": s,
                "srt_latency_ms": l,
            }),
        );
    }
}
