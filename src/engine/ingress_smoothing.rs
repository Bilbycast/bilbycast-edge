// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-input ingress smoothing buffer.
//!
//! When set on a UDP / RTP / SRT / RTMP / RTSP input, packets are
//! queued in a per-input FIFO and released to the broadcast channel
//! at `recv_time_us + smoothing_ms`. The result: downstream
//! consumers — TR-101290 analyser, media analysis, content analysis
//! (lite/audio/video), thumbnail generator, replay recorder, PID-bus
//! demuxer + assembler, PCR PLL sampler, and every output — see a
//! jitter-smoothed stream regardless of network arrival variance.
//!
//! This is the input-side counterpart to the existing per-output
//! `OutputDelay::Fixed { ms }`. The two layers are complementary:
//! input smoothing absorbs **arrival jitter** before fan-out so all
//! consumers benefit; output delay aligns **post-broadcast latency**
//! per output for, e.g., synchronising a clean feed with a
//! commentary-processed feed.
//!
//! ## Why not just rely on per-output delay
//!
//! Output delay sits **after** the broadcast fan-out, so consumers
//! that subscribe directly to the per-flow broadcast (content
//! analysers, thumbnails, replay, PID bus, PCR PLL) never go through
//! it. Input smoothing is the only way to give those consumers a
//! pre-smoothed stream.
//!
//! ## Where it adds the most value
//!
//! - **Raw UDP** input — no protocol-level jitter buffer at all.
//! - **Raw RTP** input — no protocol-level jitter buffer at all.
//! - **2022-7 dual-leg RTP/SRT** — aligning the two legs before they
//!   reach the hitless merger reduces the working set the dedup
//!   window has to chew through.
//! - **RTSP** — RTP over TCP/UDP; useful for the UDP-RTP-over-RTSP
//!   case where the upstream stack doesn't smooth.
//! - **RTMP** — TCP-level transport smoothing usually suffices, but
//!   exposing the knob keeps the API uniform across input types.
//! - **SRT** — libsrt's `SRT_LATENCY` already absorbs network jitter
//!   at the protocol level. Layering ingress smoothing on top is
//!   usually redundant; we log a warning at startup when both are
//!   set.
//!
//! ## Implementation
//!
//! Reuses [`crate::engine::delay_buffer::DelayBuffer`] (the same
//! ring + release-time machinery that powers per-output delay) but
//! drives a dedicated drainer task per input that publishes onto the
//! flow's broadcast channel. When `smoothing_ms` is `None` or `0` the
//! handle is a thin shim that calls `broadcast::Sender::send`
//! directly — no task, no allocation, no measurable overhead.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::engine::delay_buffer::DelayBuffer;
use crate::engine::packet::RtpPacket;
use crate::util::time::now_us;

/// Per-input ingress publisher. Either publishes immediately to the
/// flow broadcast channel (when smoothing is off) or hands off to a
/// drainer task that releases packets at their scheduled time.
///
/// Cheap to clone — the inner state is `Arc`-wrapped.
#[derive(Clone)]
pub struct IngressPublisher {
    inner: Arc<IngressPublisherInner>,
}

enum IngressPublisherInner {
    /// Smoothing disabled. Direct publish to broadcast.
    Direct(broadcast::Sender<RtpPacket>),
    /// Smoothing enabled. Submit via mpsc; drainer task owns the
    /// `DelayBuffer` and the `broadcast::Sender`.
    Smoothed {
        submit: mpsc::Sender<RtpPacket>,
        smoothing_ms: u16,
    },
}

impl IngressPublisher {
    /// Build an `IngressPublisher`. When `smoothing_ms` is `None` or
    /// `Some(0)`, returns a direct publisher that forwards
    /// immediately. Otherwise spawns a drainer task and returns a
    /// publisher that submits via mpsc.
    ///
    /// The drainer task exits when either `cancel` fires or the
    /// `IngressPublisher` (and all its clones) are dropped, closing
    /// the mpsc.
    pub fn new(
        smoothing_ms: Option<u16>,
        broadcast_tx: broadcast::Sender<RtpPacket>,
        input_id: &str,
        cancel: CancellationToken,
    ) -> Self {
        let ms = smoothing_ms.unwrap_or(0);
        if ms == 0 {
            return Self {
                inner: Arc::new(IngressPublisherInner::Direct(broadcast_tx)),
            };
        }
        // Cap capacity so a stalled drainer task can't grow unbounded.
        // 4096 raw packets ≈ ~170 ms of buffer at 500 Mbps — generous
        // headroom on top of the `smoothing_ms` envelope so transient
        // input bursts don't drop at the mpsc boundary.
        let (tx, rx) = mpsc::channel::<RtpPacket>(4096);
        let input_id = input_id.to_string();
        tokio::spawn(run_drainer(rx, broadcast_tx, ms, input_id, cancel));
        Self {
            inner: Arc::new(IngressPublisherInner::Smoothed {
                submit: tx,
                smoothing_ms: ms,
            }),
        }
    }

    /// Publish a packet. Non-blocking; returns `true` if accepted.
    /// On the smoothed path, returns `false` when the drainer's mpsc
    /// is full (caller increments their drop counter).
    pub fn send(&self, packet: RtpPacket) -> bool {
        match self.inner.as_ref() {
            IngressPublisherInner::Direct(tx) => tx.send(packet).is_ok(),
            IngressPublisherInner::Smoothed { submit, .. } => submit.try_send(packet).is_ok(),
        }
    }

    /// Returns the configured smoothing duration in milliseconds.
    /// `0` means smoothing is disabled (direct publish path).
    #[allow(dead_code)]
    pub fn smoothing_ms(&self) -> u16 {
        match self.inner.as_ref() {
            IngressPublisherInner::Direct(_) => 0,
            IngressPublisherInner::Smoothed { smoothing_ms, .. } => *smoothing_ms,
        }
    }
}

/// Drainer task that owns the `DelayBuffer` and the broadcast
/// sender. Periodically wakes to release any packets whose
/// `recv_time_us + smoothing_us` has passed, forwarding each to the
/// broadcast channel.
async fn run_drainer(
    mut rx: mpsc::Receiver<RtpPacket>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    smoothing_ms: u16,
    input_id: String,
    cancel: CancellationToken,
) {
    let smoothing_us = smoothing_ms as u64 * 1000;
    // DelayBuffer auto-capacity rule: 2 000 pkt/s × (smoothing + 2 s)
    // headroom. Same heuristic the per-output delay buffer uses.
    let max_capacity = DelayBuffer::auto_capacity(smoothing_ms as u64);
    let mut buffer = DelayBuffer::new(smoothing_ms as u64, max_capacity);
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
                    "ingress-smoothing '{input_id}' draining + exiting (cancelled, smoothing={smoothing_ms} ms)"
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
    let _ = smoothing_us; // kept for log clarity in future tracing
}

/// Emit a Warning event at startup when ingress smoothing is enabled
/// on an SRT input that already has protocol-level latency
/// configured — operators who set both are usually double-buffering.
/// Called by [`crate::engine::input_srt::spawn_srt_input`] at startup.
pub fn warn_if_double_buffer_srt(
    input_id: &str,
    flow_id: &str,
    ingress_smoothing_ms: Option<u16>,
    srt_latency_ms: Option<u32>,
    events: &crate::manager::events::EventSender,
) {
    if let (Some(s), Some(l)) = (ingress_smoothing_ms.filter(|v| *v > 0), srt_latency_ms.filter(|v| *v > 0)) {
        let msg = format!(
            "SRT input '{input_id}': ingress_smoothing_ms={s} is layered on top of SRT \
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
                "ingress_smoothing_ms": s,
                "srt_latency_ms": l,
            }),
        );
    }
}
