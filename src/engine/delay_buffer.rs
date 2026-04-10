// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Per-output delay buffer for stream synchronization.
//!
//! When a flow fans out to multiple outputs with different processing
//! latencies (e.g., a clean SRT feed alongside a commentary-processed
//! feed), the faster output can be delayed to match the slower one.
//!
//! The [`DelayBuffer`] queues received packets and releases them after
//! a configurable delay, using the packet's `recv_time_us` monotonic
//! timestamp as the reference clock.
//!
//! # Performance
//!
//! - **Non-blocking**: packets are consumed from the broadcast channel
//!   immediately; only the socket send is deferred.
//! - **Zero-copy**: `RtpPacket.data` uses `bytes::Bytes` — moving
//!   packets into/out of the VecDeque copies only the 40-byte struct.
//! - **O(1) amortized** push/pop (VecDeque ring buffer).
//! - Overflow protection drops the oldest packets when the buffer
//!   exceeds capacity.

use std::collections::VecDeque;

use super::packet::RtpPacket;

/// A FIFO delay buffer that holds packets until their scheduled
/// release time (`recv_time_us + delay_us`) has passed.
///
/// Packets arrive in monotonically increasing `recv_time_us` order
/// from the broadcast channel, so the queue is naturally sorted —
/// no heap or binary search needed.
pub struct DelayBuffer {
    delay_us: u64,
    buffer: VecDeque<RtpPacket>,
    max_capacity: usize,
    overflow_dropped: u64,
}

#[allow(dead_code)]
impl DelayBuffer {
    /// Create a new delay buffer.
    ///
    /// `delay_ms` is the output delay in milliseconds. `max_capacity`
    /// limits the buffer size; excess packets are dropped from the
    /// front (oldest first).
    pub fn new(delay_ms: u64, max_capacity: usize) -> Self {
        Self {
            delay_us: delay_ms * 1000,
            buffer: VecDeque::with_capacity(max_capacity.min(16384)),
            max_capacity,
            overflow_dropped: 0,
        }
    }

    /// Compute a reasonable max capacity for the given delay.
    ///
    /// A 1080p50 MPEG-TS stream at ~20 Mbps produces ~1,700 pkt/s;
    /// at 100 Mbps it's ~9,500 pkt/s. We budget 2,000 pkt/s per
    /// second of delay plus 2 seconds of headroom.
    pub fn auto_capacity(delay_ms: u64) -> usize {
        let secs = delay_ms / 1000 + 2;
        (secs as usize * 2000).max(16384)
    }

    /// Enqueue a packet. If the buffer exceeds `max_capacity`, the
    /// oldest packet is dropped and counted.
    pub fn push(&mut self, packet: RtpPacket) {
        if self.buffer.len() >= self.max_capacity {
            self.buffer.pop_front();
            self.overflow_dropped += 1;
        }
        self.buffer.push_back(packet);
    }

    /// Drain all packets whose release time has passed.
    ///
    /// Returns an iterator that yields packets where
    /// `recv_time_us + delay_us <= now_us`.
    pub fn drain_ready(&mut self, now_us: u64) -> DrainReady<'_> {
        DrainReady {
            buffer: &mut self.buffer,
            deadline_us: now_us,
            delay_us: self.delay_us,
        }
    }

    /// The release time of the front (oldest) packet, if any.
    ///
    /// Use this to set the `tokio::time::Sleep` deadline. Returns
    /// `recv_time_us + delay_us` of the front packet.
    pub fn next_release_time(&self) -> Option<u64> {
        self.buffer.front().map(|p| p.recv_time_us + self.delay_us)
    }

    /// Number of packets currently buffered.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Total number of packets dropped due to overflow.
    pub fn overflow_dropped(&self) -> u64 {
        self.overflow_dropped
    }

    /// Update the delay dynamically (e.g., when frame rate changes in
    /// `TargetFrames` mode). Takes effect for all future `drain_ready` and
    /// `next_release_time` calls. Already-buffered packets adjust to the
    /// new delay automatically since their release time is computed on
    /// the fly from `recv_time_us + delay_us`.
    pub fn set_delay_us(&mut self, delay_us: u64) {
        self.delay_us = delay_us;
    }

    /// Current delay in microseconds.
    pub fn delay_us(&self) -> u64 {
        self.delay_us
    }

    /// Flush all buffered packets (e.g., on SRT reconnect).
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

/// Iterator returned by [`DelayBuffer::drain_ready`].
///
/// Pops packets from the front of the buffer while their release
/// time has passed. Stops at the first packet that isn't ready yet
/// (the queue is sorted by `recv_time_us`).
pub struct DrainReady<'a> {
    buffer: &'a mut VecDeque<RtpPacket>,
    deadline_us: u64,
    delay_us: u64,
}

impl<'a> Iterator for DrainReady<'a> {
    type Item = RtpPacket;

    fn next(&mut self) -> Option<RtpPacket> {
        let front = self.buffer.front()?;
        if front.recv_time_us + self.delay_us <= self.deadline_us {
            self.buffer.pop_front()
        } else {
            None
        }
    }
}

/// Resolve an [`OutputDelay`] config into a ready-to-use [`DelayBuffer`],
/// handling the async frame-rate wait for `TargetFrames` mode.
///
/// Returns `None` when no delay is needed (e.g., `Fixed { ms: 0 }` or
/// `TargetFrames` without a detected frame rate and no fallback).
///
/// For `TargetFrames`: waits up to 5 seconds for the frame rate to be
/// detected via the `frame_rate_rx` watch channel. If detected, computes
/// `target_ms = frames * 1000 / fps`. If not detected, falls back to
/// `fallback_ms`. The returned `DelayBuffer` also stores the
/// `frame_rate_rx` so the output loop can periodically check for fps
/// changes and call `set_delay_us()`.
pub async fn resolve_output_delay(
    delay: &crate::config::models::OutputDelay,
    output_id: &str,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<(DelayBuffer, Option<tokio::sync::watch::Receiver<Option<f64>>>)> {
    use crate::config::models::OutputDelay;

    match delay {
        OutputDelay::Fixed { ms } => {
            if *ms == 0 {
                return None;
            }
            tracing::info!(
                "Output '{}': fixed output delay enabled, {} ms",
                output_id, ms
            );
            let buf = DelayBuffer::new(*ms, DelayBuffer::auto_capacity(*ms));
            Some((buf, None))
        }
        OutputDelay::TargetMs { ms } => {
            tracing::info!(
                "Output '{}': target end-to-end latency enabled, {} ms",
                output_id, ms
            );
            let buf = DelayBuffer::new(*ms, DelayBuffer::auto_capacity(*ms));
            Some((buf, None))
        }
        OutputDelay::TargetFrames { frames, fallback_ms } => {
            // Try to get the frame rate from media analysis within 5 seconds.
            let mut resolved_ms: Option<u64> = None;

            if let Some(mut rx) = frame_rate_rx.clone() {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let fps = *rx.borrow_and_update();
                    if let Some(fps) = fps {
                        if fps > 0.0 {
                            let ms = (*frames * 1000.0 / fps).round() as u64;
                            tracing::info!(
                                "Output '{}': target latency {} frames at {:.2} fps = {} ms",
                                output_id, frames, fps, ms
                            );
                            resolved_ms = Some(ms.max(1).min(10000));
                            break;
                        }
                    }
                    // Wait for a change or timeout
                    tokio::select! {
                        _ = cancel.cancelled() => return None,
                        _ = tokio::time::sleep_until(deadline) => {
                            tracing::warn!(
                                "Output '{}': frame rate not detected within 5s for TargetFrames delay",
                                output_id
                            );
                            break;
                        }
                        result = rx.changed() => {
                            if result.is_err() {
                                // Sender dropped (flow shutting down)
                                break;
                            }
                            // Loop back to check the new value
                        }
                    }
                }
            }

            // Fall back to fallback_ms if frame rate wasn't detected
            if resolved_ms.is_none() {
                if let Some(fb) = fallback_ms {
                    if *fb > 0 {
                        tracing::info!(
                            "Output '{}': using fallback delay {} ms (frame rate not detected)",
                            output_id, fb
                        );
                        resolved_ms = Some(*fb);
                    }
                }
            }

            match resolved_ms {
                Some(ms) if ms > 0 => {
                    let buf = DelayBuffer::new(ms, DelayBuffer::auto_capacity(ms));
                    Some((buf, frame_rate_rx))
                }
                _ => {
                    tracing::warn!(
                        "Output '{}': TargetFrames delay disabled (no frame rate, no fallback)",
                        output_id
                    );
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn make_packet(recv_time_us: u64, seq: u16) -> RtpPacket {
        RtpPacket {
            data: Bytes::from_static(&[0x47, 0x00, 0x00, 0x10]),
            sequence_number: seq,
            rtp_timestamp: 0,
            recv_time_us,
            is_raw_ts: true,
        }
    }

    #[test]
    fn drain_releases_after_delay() {
        let mut buf = DelayBuffer::new(100, 1024); // 100ms = 100_000us

        buf.push(make_packet(1_000_000, 1)); // recv at 1.0s
        buf.push(make_packet(1_050_000, 2)); // recv at 1.05s
        buf.push(make_packet(1_150_000, 3)); // recv at 1.15s

        // At 1.1s: only packet 1 should be ready (1.0 + 0.1 = 1.1)
        let ready: Vec<_> = buf.drain_ready(1_100_000).collect();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].sequence_number, 1);

        // At 1.15s: packet 2 ready (1.05 + 0.1 = 1.15)
        let ready: Vec<_> = buf.drain_ready(1_150_000).collect();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].sequence_number, 2);

        // At 1.3s: packet 3 ready (1.15 + 0.1 = 1.25)
        let ready: Vec<_> = buf.drain_ready(1_300_000).collect();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].sequence_number, 3);

        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn drain_empty_buffer() {
        let mut buf = DelayBuffer::new(100, 1024);
        let ready: Vec<_> = buf.drain_ready(1_000_000).collect();
        assert!(ready.is_empty());
    }

    #[test]
    fn drain_nothing_ready() {
        let mut buf = DelayBuffer::new(1000, 1024); // 1 second delay
        buf.push(make_packet(1_000_000, 1));

        // At 1.5s: not ready yet (1.0 + 1.0 = 2.0)
        let ready: Vec<_> = buf.drain_ready(1_500_000).collect();
        assert!(ready.is_empty());
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn drain_multiple_at_once() {
        let mut buf = DelayBuffer::new(100, 1024);

        buf.push(make_packet(1_000_000, 1));
        buf.push(make_packet(1_010_000, 2));
        buf.push(make_packet(1_020_000, 3));

        // All three ready at 1.2s
        let ready: Vec<_> = buf.drain_ready(1_200_000).collect();
        assert_eq!(ready.len(), 3);
        assert_eq!(ready[0].sequence_number, 1);
        assert_eq!(ready[1].sequence_number, 2);
        assert_eq!(ready[2].sequence_number, 3);
    }

    #[test]
    fn overflow_drops_oldest() {
        let mut buf = DelayBuffer::new(100, 3);

        buf.push(make_packet(1_000_000, 1));
        buf.push(make_packet(1_010_000, 2));
        buf.push(make_packet(1_020_000, 3));
        assert_eq!(buf.overflow_dropped(), 0);

        // Pushing a 4th drops the oldest
        buf.push(make_packet(1_030_000, 4));
        assert_eq!(buf.overflow_dropped(), 1);
        assert_eq!(buf.len(), 3);

        // Verify packet 1 was dropped
        let ready: Vec<_> = buf.drain_ready(2_000_000).collect();
        assert_eq!(ready.len(), 3);
        assert_eq!(ready[0].sequence_number, 2);
    }

    #[test]
    fn clear_empties_buffer() {
        let mut buf = DelayBuffer::new(100, 1024);
        buf.push(make_packet(1_000_000, 1));
        buf.push(make_packet(1_010_000, 2));
        assert_eq!(buf.len(), 2);

        buf.clear();
        assert_eq!(buf.len(), 0);
        assert!(buf.next_release_time().is_none());
    }

    #[test]
    fn next_release_time_correct() {
        let mut buf = DelayBuffer::new(200, 1024); // 200ms

        assert!(buf.next_release_time().is_none());

        buf.push(make_packet(1_000_000, 1));
        assert_eq!(buf.next_release_time(), Some(1_200_000));

        buf.push(make_packet(1_100_000, 2));
        // Still returns the front packet's release time
        assert_eq!(buf.next_release_time(), Some(1_200_000));
    }

    #[test]
    fn auto_capacity_scales() {
        // 1s delay → (1+2)*2000 = 6000, but min is 16384
        assert_eq!(DelayBuffer::auto_capacity(1000), 16384);
        // 10s delay → (10+2)*2000 = 24000
        assert_eq!(DelayBuffer::auto_capacity(10000), 24000);
        // 0ms delay → (0+2)*2000 = 4000, but min is 16384
        assert_eq!(DelayBuffer::auto_capacity(0), 16384);
    }
}
