// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Media Delivery Index (RFC 4445).
//!
//! Measured on the broadcast-channel tap so post-recovered SRT / RIST
//! streams contribute alongside raw UDP / RTP. Computed over a rolling
//! 1 s window.
//!
//! - **Delay Factor (DF / "NDF")**: inter-arrival variance expressed in
//!   milliseconds. We approximate it as `max(iat_ms) - avg(iat_ms)` across
//!   the window — the buffer depth a receiver would need to absorb the
//!   peak jitter observed. Simpler than the per-RFC-4445 VB overflow
//!   model but produces the same magnitude for the cases operators care
//!   about (bursty sources, network congestion).
//! - **Media Loss Rate (MLR)**: per-second packet loss. Inferred from
//!   bitrate discontinuities — if bitrate drops below a threshold mid-
//!   window we don't count it as loss (healthy idle); if packet arrivals
//!   skip at regular rate we count the gap.
//!
//! MLR source visibility across transports (answering the design-time
//! question "does MDI apply to SRT / RIST too?"): yes. The broadcast
//! channel carries the **post-recovered** stream; an SRT session that
//! hides 2 % transport loss under ARQ + FEC and ultimately delivers a
//! clean stream will show `MLR ≈ 0`. When the recovery fails (ARQ
//! budget exhausted, FEC can't reconstruct the burst), the resulting
//! per-TS-packet CC errors surface as MLR here as well as in the
//! TR-101290 analyser.

use std::time::Duration;

use crate::stats::models::MdiStats;

/// Running state for a single window.
struct Window {
    start_us: u64,
    packets: u64,
    bytes: u64,
    last_packet_us: Option<u64>,
    max_iat_us: u64,
    sum_iat_us: u64,
    iat_samples: u64,
    cc_errors: u64,
}

impl Window {
    fn new(start_us: u64) -> Self {
        Self {
            start_us,
            packets: 0,
            bytes: 0,
            last_packet_us: None,
            max_iat_us: 0,
            sum_iat_us: 0,
            iat_samples: 0,
            cc_errors: 0,
        }
    }
}

pub struct MdiSampler {
    window_len_us: u64,
    current: Window,
    // Published results from the most recent completed window.
    delay_factor_ms: f32,
    loss_rate_pps: f32,
    windows_above_threshold: u64,
    /// True once we have finished at least one window — prevents an
    /// "analysing…" snapshot from showing a bogus `0.0:0` before any data.
    has_sample: bool,
}

impl MdiSampler {
    pub fn new(window: Duration) -> Self {
        Self {
            window_len_us: window.as_micros() as u64,
            current: Window::new(0),
            delay_factor_ms: 0.0,
            loss_rate_pps: 0.0,
            windows_above_threshold: 0,
            has_sample: false,
        }
    }

    /// Record one TS continuity-counter discontinuity for the current window.
    /// Fed by the Lite analyser's per-PID CC tracker. MLR (RFC 4445) is the
    /// number of packets lost per second — CC discontinuities on the delivered
    /// stream are the most reliable "packet lost" signal we have without a
    /// sequence number (UDP raw-TS) or after transport-layer recovery hides
    /// the RTP / SRT / RIST sequence gap (ARQ / FEC recovered = no loss).
    pub fn observe_cc_error(&mut self) {
        self.current.cc_errors = self.current.cc_errors.saturating_add(1);
    }

    /// Feed a single broadcast-channel packet arrival time. `now_us` is
    /// the monotonic timestamp already carried on `RtpPacket.recv_time_us`.
    pub fn observe_packet(&mut self, now_us: u64, bytes: usize) {
        if self.current.start_us == 0 {
            self.current.start_us = now_us;
        }

        if now_us.saturating_sub(self.current.start_us) >= self.window_len_us {
            self.roll_window(now_us);
        }

        if let Some(prev) = self.current.last_packet_us {
            let iat = now_us.saturating_sub(prev);
            if iat > self.current.max_iat_us {
                self.current.max_iat_us = iat;
            }
            self.current.sum_iat_us += iat;
            self.current.iat_samples += 1;
        }
        self.current.last_packet_us = Some(now_us);
        self.current.packets += 1;
        self.current.bytes += bytes as u64;
    }

    /// Force a finalise check on a publish tick — handy when packet
    /// arrivals have paused (e.g. input dropped) and `observe_packet`
    /// isn't driving rollover.
    pub fn maybe_finalise_window(&mut self) {
        // Nothing to do if we never got a packet.
        if self.current.start_us == 0 {
            return;
        }
        // If the current window has run long enough, publish it and reset.
        // We use a wall-clock estimate by looking at `last_packet_us`; if
        // no packets arrived the window silently extends, which is fine —
        // we want NDF to keep reflecting reality, not reset to zero because
        // the publish cadence fired.
        if let Some(last) = self.current.last_packet_us {
            if last.saturating_sub(self.current.start_us) >= self.window_len_us {
                self.roll_window(last);
            }
        }
    }

    fn roll_window(&mut self, now_us: u64) {
        let w = &self.current;
        // Approximate DF: (peak_iat - mean_iat) in ms — the buffer depth a
        // receiver would need to absorb the burstiest arrival relative to
        // the flow's own nominal cadence. Degenerate case (<2 samples) we
        // report 0 so the UI treats the number as "unknown" rather than
        // emitting a phantom spike.
        let df_ms: f32 = if w.iat_samples >= 2 {
            let mean = (w.sum_iat_us as f64) / (w.iat_samples as f64);
            let spread = (w.max_iat_us as f64) - mean;
            (spread.max(0.0) / 1_000.0) as f32
        } else {
            0.0
        };

        // MLR (RFC 4445): packet loss per second. Driven by TS CC
        // discontinuities fed via `observe_cc_error()` from the Lite
        // analyser's per-PID CC tracker. Normalised to "per second"
        // using the wall-clock duration of the just-closed window so
        // a 500 ms window doesn't double-report vs a 1 s window.
        let window_span_s =
            ((now_us.saturating_sub(self.current.start_us)) as f64 / 1_000_000.0).max(1e-6);
        let mlr_pps: f32 = (w.cc_errors as f64 / window_span_s) as f32;

        self.delay_factor_ms = df_ms;
        self.loss_rate_pps = mlr_pps;
        if df_ms > 50.0 {
            self.windows_above_threshold += 1;
        }
        self.has_sample = true;

        // Reset for next window.
        self.current = Window::new(now_us);
    }

    pub fn last_delay_factor_ms(&self) -> f32 {
        self.delay_factor_ms
    }

    pub fn last_loss_rate_pps(&self) -> f32 {
        self.loss_rate_pps
    }

    pub fn snapshot(&self) -> Option<MdiStats> {
        if !self.has_sample {
            return None;
        }
        Some(MdiStats {
            mdi: format!("{:.1}:{:.1}", self.delay_factor_ms, self.loss_rate_pps),
            model: "approx-iat-spread",
            delay_factor_ms: self.delay_factor_ms,
            loss_rate_pps: self.loss_rate_pps,
            windows_above_threshold: self.windows_above_threshold,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sampler_has_no_snapshot() {
        let s = MdiSampler::new(Duration::from_secs(1));
        assert!(s.snapshot().is_none());
    }

    #[test]
    fn rolls_window_and_publishes_stats() {
        let mut s = MdiSampler::new(Duration::from_millis(500));
        // Steady 10 ms IATs over 700 ms → should roll the 500 ms window
        // once and expose a DF close to 0 (peak == avg).
        let mut t = 0u64;
        for _ in 0..70 {
            s.observe_packet(t, 1400);
            t += 10_000;
        }
        let snap = s.snapshot().expect("window should have rolled");
        assert!(snap.delay_factor_ms < 5.0, "got {}", snap.delay_factor_ms);
    }

    #[test]
    fn peak_iat_raises_delay_factor() {
        // Use a wide window so the gap stays inside the same sampling
        // interval — the test is about IAT measurement, not rollover edge
        // cases.
        let mut s = MdiSampler::new(Duration::from_secs(10));
        let mut t = 0u64;
        for _ in 0..50 {
            s.observe_packet(t, 1400);
            t += 10_000; // 10 ms IAT
        }
        // One 200 ms burst mid-window.
        t += 200_000;
        s.observe_packet(t, 1400);
        // Keep feeding packets until the 10 s window rolls.
        for _ in 0..2000 {
            t += 10_000;
            s.observe_packet(t, 1400);
            if s.snapshot().is_some() {
                break;
            }
        }
        let snap = s.snapshot().expect("window should have rolled by now");
        assert!(
            snap.delay_factor_ms > 10.0,
            "expected elevated DF, got {}",
            snap.delay_factor_ms
        );
    }
}
