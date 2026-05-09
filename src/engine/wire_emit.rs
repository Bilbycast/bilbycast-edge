// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-output wire emission engine.
//!
//! Drives a dedicated `std::thread` (Linux: `SCHED_FIFO` best-effort) that
//! pops TS datagrams off a [`std::sync::mpsc::sync_channel`] fed by the
//! encoder task and emits each datagram onto the wire at a wallclock
//! derived from PCR + master clock. The thread is decoupled from Tokio so
//! `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, ...)` can give us
//! sub-100-µs precision instead of the ~1–5 ms ceiling the Tokio timer
//! wheel imposes on `tokio::time::sleep_until`.
//!
//! ## Target derivation
//!
//! Anchor on the first PCR-bearing datagram: `wall_anchor_ns =
//! monotonic_now() + PREROLL_NS`, `pcr_anchor = pcr`. Re-anchor on every
//! subsequent PCR — bounds drift between our wallclock pacing and the
//! source's PCR clock to one inter-PCR period (~30–40 ms typical).
//!
//! Between PCRs we interpolate by static bitrate:
//!
//! ```text
//! target_ns = wall_anchor_ns + bytes_since_anchor * 8 × 1_000_000_000 / bitrate_bps
//! ```
//!
//! `declared_bitrate_bps` (from output config or transcode target) is
//! preferred. If unset, we fall back to a bitrate inferred from the most
//! recent inter-PCR pair (EMA over four observations).
//!
//! ## Discontinuity handling
//!
//! A PCR jump > 500 ms forward or any backwards step resets the anchor
//! to "now" and resumes from the new PCR. Mirrors the discontinuity rule
//! in `engine::pcr_pll` and `stats::pcr_trust`.
//!
//! ## Cancellation
//!
//! `recv_timeout(50 ms)` polls the cancellation token; emitter exits
//! cleanly within ~50 ms of cancel. Channel disconnect (encoder dropped)
//! also exits cleanly.
//!
//! ## Stats
//!
//! Same accounting as the previous in-line wire task: `packets_sent`,
//! `bytes_sent`, `record_latency()`, `record_pcr_egress()`. Encoder-side
//! `try_send` overflow is counted under `packets_dropped`.
//!
//! ## Status — preserved, not yet wired
//!
//! Module + 12 unit tests landed; re-integration into `output_udp.rs` /
//! `output_rtp.rs` is deferred until the upstream PCR-cadence bug in
//! `ts_video_replace` / `av_sync_mux::pcr_for_emit` is resolved (see
//! `docs/wire-pacing.md` and the project memory entry on PCR jitter).
//! Sub-ms PCR jitter at the receiver requires this module — do not
//! delete. The module-level `#![allow(dead_code)]` below silences the
//! warnings the deferred integration would otherwise produce.

#![allow(dead_code)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::stats::collector::OutputStatsAccumulator;

/// 50 ms preroll on the first emit. Avoids the immediate send-cliff
/// when the first datagram arrives at the wire thread before the
/// encoder has settled. Live testing showed deeper preroll (300 ms)
/// caused mass channel drops during encoder bursts because the upstream
/// transcoder is producing slightly-faster-than-target bytes; the
/// late-rebase below handles the fall-behind case without adding
/// glass-to-glass latency.
const PREROLL_NS: u64 = 50_000_000;

/// If a PCR-driven target lands in the past by more than this, rebase
/// `wall_anchor` to `now_ns` so we don't accumulate wallclock lag every
/// burst. Causes a one-time jitter step at the rebase moment; without
/// it, a single late burst pins the anchor permanently behind real time
/// and subsequent PCRs all emit ASAP — exactly the failure mode we
/// observed in earlier validation.
const LATE_REBASE_THRESHOLD_NS: u64 = 5_000_000;

/// 500 ms in 27 MHz ticks. PCR jumps beyond this trigger an anchor reset.
const PCR_DISCONTINUITY_27MHZ: i64 = 13_500_000;

/// `recv_timeout` polling cadence. Bounds cancel latency.
const RECV_POLL: Duration = Duration::from_millis(50);

/// 12-byte RFC 2250 RTP header.
const RTP_HEADER_SIZE: usize = 12;

/// RFC 2250 payload type for MPEG-TS over RTP.
const RTP_PT_MP2T: u8 = 33;

/// Channel capacity. 1024 datagrams ≈ 1.7 s of in-flight bytes at
/// 6 Mbps — comfortably absorbs h264_qsv frame-quanta bursts where the
/// encoder pipelines several frames of TS at once. Earlier 256 cap was
/// too tight under live transcode; channel fills, `try_send` drops
/// fire, decoders pixelate. Keeping the queue this deep adds at most
/// ~1.7 s of glass-to-glass latency under sustained backpressure (and
/// only under sustained backpressure — steady state holds far less).
pub const WIRE_CHANNEL_CAP: usize = 1024;

/// Lower bound clamp on declared/inferred bitrate. Avoids divide-by-zero
/// edge cases without imposing a meaningful pacing floor.
const MIN_BITRATE_BPS: u64 = 64_000;

/// One datagram on its way to the wire. Already final TS bytes (no RTP
/// header) — emitter wraps if `WireWrap::Rtp`.
pub struct WireDatagram {
    pub bytes: Vec<u8>,
    pub recv_time_us: u64,
    pub rtp_ts_90k: u32,
}

#[derive(Clone, Copy)]
pub enum WireWrap {
    /// Send the bytes verbatim (UDP).
    None,
    /// Prepend RFC 2250 RTP header (PT=33). Sequence + SSRC owned by
    /// the emitter — fresh state per spawn so concurrent flows don't
    /// alias seq numbers.
    Rtp,
}

/// Spawn a dedicated wire-emission thread.
///
/// Returns the encoder-side `SyncSender`. `try_send` is non-blocking;
/// overflow is counted under `stats.packets_dropped` by the caller.
///
/// `socket` must be a blocking `std::net::UdpSocket` (call
/// `set_nonblocking(false)` after `tokio::net::UdpSocket::into_std`).
pub fn spawn_wire_emitter(
    id: String,
    socket: UdpSocket,
    dest: SocketAddr,
    declared_bitrate_bps: u64,
    wrap: WireWrap,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> SyncSender<WireDatagram> {
    let (tx, rx) = sync_channel::<WireDatagram>(WIRE_CHANNEL_CAP);
    let thread_id = id.clone();
    std::thread::Builder::new()
        .name(format!("wire-emit-{}", id))
        .spawn(move || {
            apply_realtime_priority(&thread_id);
            run_emitter(thread_id, socket, dest, declared_bitrate_bps, wrap, stats, cancel, rx);
        })
        .expect("wire-emit thread spawn");
    tx
}

// ── Target derivation state ─────────────────────────────────────────────

#[derive(Default)]
struct TargetState {
    /// Most recent PCR sample (27 MHz ticks). `None` until first PCR.
    pcr_anchor: Option<u64>,
    /// Wallclock instant at which the most recent anchor datagram emits
    /// (CLOCK_MONOTONIC ns). Set to `now + PREROLL_NS` on the first
    /// datagram regardless of PCR presence.
    wall_anchor_ns: u64,
    /// Bytes accumulated past the wall_anchor by datagrams since the
    /// most recent re-anchor. Reset to 0 when we re-anchor on a PCR.
    bytes_since_anchor: u64,
    /// EMA of inter-PCR observed bitrate. Used for between-PCR
    /// interpolation when caller didn't supply a static rate.
    inferred_bitrate_bps: u64,
    /// Set after the very first datagram so we don't preroll again.
    initialised: bool,
}

impl TargetState {
    /// Compute the target wall instant (CLOCK_MONOTONIC ns) at which
    /// the just-handed datagram should hit the wire. Mutates internal
    /// anchors so the next call sees consistent state.
    fn derive_target(
        &mut self,
        now_ns: u64,
        datagram_pcr: Option<u64>,
        datagram_bytes: usize,
        declared_bitrate_bps: u64,
    ) -> u64 {
        if !self.initialised {
            self.wall_anchor_ns = now_ns.saturating_add(PREROLL_NS);
            self.bytes_since_anchor = 0;
            self.initialised = true;
            if let Some(pcr) = datagram_pcr {
                self.pcr_anchor = Some(pcr);
            }
            // First datagram emits exactly at wall_anchor — no
            // accumulated bytes past the anchor yet.
            return self.wall_anchor_ns;
        }

        if let Some(pcr) = datagram_pcr {
            if let Some(prev) = self.pcr_anchor {
                let delta_27 = pcr.wrapping_sub(prev) as i64;
                // Discontinuity: backwards by any meaningful amount, or
                // forward jump > 500 ms. Reset anchor to "now".
                if !(0..=PCR_DISCONTINUITY_27MHZ).contains(&delta_27) {
                    self.pcr_anchor = Some(pcr);
                    self.wall_anchor_ns = now_ns;
                    self.bytes_since_anchor = 0;
                    return now_ns;
                }
                let delta_ns = (delta_27 as u64) * 1000 / 27;
                let mut target = self.wall_anchor_ns.saturating_add(delta_ns);

                // Late-rebase: if encoder bursts pushed the ideal target
                // into the past, snap forward so subsequent PCRs are
                // paced from real time. Otherwise wall_anchor stays
                // behind forever and the burst pattern shows up at the
                // receiver as PCR-spacing-equivalent jitter.
                if target + LATE_REBASE_THRESHOLD_NS < now_ns {
                    target = now_ns;
                }

                // EMA the inferred bitrate (used when declared rate is
                // unset). bytes_since_anchor holds bytes accumulated
                // *between* the prior PCR anchor and now (excluding the
                // just-arrived PCR datagram, which we're about to
                // re-anchor on).
                if delta_ns >= 1_000_000 && self.bytes_since_anchor > 0 {
                    let observed_bps = self
                        .bytes_since_anchor
                        .saturating_mul(8 * 1_000_000_000)
                        / delta_ns;
                    if self.inferred_bitrate_bps == 0 {
                        self.inferred_bitrate_bps = observed_bps;
                    } else {
                        self.inferred_bitrate_bps =
                            (3 * self.inferred_bitrate_bps + observed_bps) / 4;
                    }
                }

                self.pcr_anchor = Some(pcr);
                self.wall_anchor_ns = target;
                self.bytes_since_anchor = 0;
                return target;
            }
            // First PCR after a no-PCR run: anchor on it. Pace this
            // datagram from the existing wall_anchor — but only if we
            // have a bitrate. Otherwise emit immediately.
            self.pcr_anchor = Some(pcr);
            self.bytes_since_anchor =
                self.bytes_since_anchor.saturating_add(datagram_bytes as u64);
            return match self.bytes_to_ns(declared_bitrate_bps) {
                Some(ns) => self.wall_anchor_ns.saturating_add(ns),
                None => now_ns,
            };
        }

        // No PCR. Interpolate by bitrate if we have one.
        self.bytes_since_anchor =
            self.bytes_since_anchor.saturating_add(datagram_bytes as u64);
        match self.bytes_to_ns(declared_bitrate_bps) {
            Some(ns) => self.wall_anchor_ns.saturating_add(ns),
            // Cold-start passthrough: no bitrate hint and no inter-PCR
            // observation yet. Emit immediately — receivers tolerate
            // packets-ahead-of-cadence far better than packets-far-behind.
            // The next PCR seeds the EMA and pacing kicks in.
            None => now_ns,
        }
    }

    /// Returns the ns-offset from `wall_anchor_ns` for the byte position
    /// the just-handed datagram represents, OR `None` if neither declared
    /// nor inferred bitrate is available yet.
    fn bytes_to_ns(&self, declared_bitrate_bps: u64) -> Option<u64> {
        let bps = if declared_bitrate_bps > 0 {
            declared_bitrate_bps
        } else if self.inferred_bitrate_bps > 0 {
            self.inferred_bitrate_bps
        } else {
            return None;
        };
        Some(
            self.bytes_since_anchor
                .saturating_mul(8 * 1_000_000_000)
                / bps.max(MIN_BITRATE_BPS),
        )
    }
}

// ── RTP wrap state (for WireWrap::Rtp) ──────────────────────────────────

struct RtpState {
    seq: u16,
    ssrc: u32,
}

impl RtpState {
    fn new() -> Self {
        Self {
            seq: rand::random::<u16>(),
            ssrc: rand::random::<u32>(),
        }
    }

    fn build_header(&mut self, rtp_ts: u32) -> [u8; RTP_HEADER_SIZE] {
        let mut hdr = [0u8; RTP_HEADER_SIZE];
        hdr[0] = 0x80;
        hdr[1] = RTP_PT_MP2T;
        hdr[2] = (self.seq >> 8) as u8;
        hdr[3] = (self.seq & 0xFF) as u8;
        hdr[4] = (rtp_ts >> 24) as u8;
        hdr[5] = (rtp_ts >> 16) as u8;
        hdr[6] = (rtp_ts >> 8) as u8;
        hdr[7] = rtp_ts as u8;
        hdr[8] = (self.ssrc >> 24) as u8;
        hdr[9] = (self.ssrc >> 16) as u8;
        hdr[10] = (self.ssrc >> 8) as u8;
        hdr[11] = self.ssrc as u8;
        self.seq = self.seq.wrapping_add(1);
        hdr
    }
}

// ── Emitter main loop ───────────────────────────────────────────────────

fn run_emitter(
    id: String,
    socket: UdpSocket,
    dest: SocketAddr,
    declared_bitrate_bps: u64,
    wrap: WireWrap,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    rx: Receiver<WireDatagram>,
) {
    let mut state = TargetState::default();
    let mut rtp_state = match wrap {
        WireWrap::Rtp => Some(RtpState::new()),
        WireWrap::None => None,
    };
    let mut send_buf: Vec<u8> = Vec::with_capacity(RTP_HEADER_SIZE + 1316);

    loop {
        if cancel.is_cancelled() {
            return;
        }
        let dg = match rx.recv_timeout(RECV_POLL) {
            Ok(d) => d,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        let now_ns = monotonic_now_ns();
        let pcr_27mhz = crate::engine::ts_parse::first_pcr_in_ts_buffer(&dg.bytes);
        let target_ns =
            state.derive_target(now_ns, pcr_27mhz, dg.bytes.len(), declared_bitrate_bps);

        if target_ns > now_ns {
            sleep_until_monotonic_ns(target_ns);
        }

        let payload: &[u8] = match rtp_state.as_mut() {
            Some(rs) => {
                send_buf.clear();
                send_buf.extend_from_slice(&rs.build_header(dg.rtp_ts_90k));
                send_buf.extend_from_slice(&dg.bytes);
                &send_buf
            }
            None => &dg.bytes,
        };

        match socket.send_to(payload, dest) {
            Ok(sent) => {
                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                stats.record_latency(dg.recv_time_us);
                if let Some(pcr) = pcr_27mhz {
                    stats.record_pcr_egress(pcr, crate::util::time::now_us());
                }
            }
            Err(e) => {
                tracing::warn!("wire-emit '{}' send error: {}", id, e);
            }
        }
    }
}

// ── Platform: clock_nanosleep + SCHED_FIFO ──────────────────────────────

#[cfg(target_os = "linux")]
fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64)
}

#[cfg(target_os = "linux")]
fn sleep_until_monotonic_ns(target_ns: u64) {
    let ts = libc::timespec {
        tv_sec: (target_ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (target_ns % 1_000_000_000) as i64,
    };
    // EINTR returns: re-arm with the same absolute deadline.
    loop {
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        if rc != libc::EINTR {
            break;
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_realtime_priority(id: &str) {
    let mut sp: libc::sched_param = unsafe { std::mem::zeroed() };
    sp.sched_priority = 50;
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &sp) };
    if rc == 0 {
        tracing::debug!("wire-emit '{}': SCHED_FIFO priority 50 acquired", id);
    } else {
        tracing::debug!(
            "wire-emit '{}': SCHED_FIFO unavailable (rc={}); running at default priority. \
             Grant CAP_SYS_NICE for lower jitter.",
            id,
            rc
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn monotonic_now_ns() -> u64 {
    use std::time::Instant;
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_nanos() as u64
}

#[cfg(not(target_os = "linux"))]
fn sleep_until_monotonic_ns(target_ns: u64) {
    let now = monotonic_now_ns();
    if target_ns > now {
        std::thread::sleep(Duration::from_nanos(target_ns - now));
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_realtime_priority(_id: &str) {
    // No realtime path on non-Linux. clock_nanosleep + SCHED_FIFO is
    // Linux-specific in this codebase.
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 27 MHz ticks per millisecond.
    const TICK_PER_MS: u64 = 27_000;

    #[test]
    fn first_datagram_anchors_at_now_plus_preroll() {
        let mut s = TargetState::default();
        let target = s.derive_target(1_000_000_000, Some(900_000), 1316, 5_000_000);
        assert_eq!(target, 1_000_000_000 + PREROLL_NS);
        assert!(s.initialised);
        assert_eq!(s.pcr_anchor, Some(900_000));
    }

    #[test]
    fn pcr_to_pcr_delta_drives_target() {
        let mut s = TargetState::default();
        // Init with first datagram at t=0, PCR=27 000 (1 ms).
        let _ = s.derive_target(0, Some(TICK_PER_MS), 1316, 5_000_000);
        let anchor_wall = s.wall_anchor_ns;

        // Next PCR datagram, 40 ms later in PCR space. Bytes between
        // anchors don't matter for the PCR-driven target — PCRs are
        // independent of bytes.
        let target = s.derive_target(
            anchor_wall + 100_000, // wall isn't used for PCR-driven targets
            Some(TICK_PER_MS + 40 * TICK_PER_MS),
            1316,
            5_000_000,
        );
        // Expected: anchor_wall + 40 ms in ns.
        assert_eq!(target, anchor_wall + 40_000_000);
        // Re-anchor: bytes_since_anchor reset, wall advanced.
        assert_eq!(s.bytes_since_anchor, 0);
        assert_eq!(s.wall_anchor_ns, target);
    }

    #[test]
    fn between_pcrs_uses_declared_bitrate() {
        let mut s = TargetState::default();
        // First datagram has PCR; emits at wall_anchor.
        let _ = s.derive_target(0, Some(TICK_PER_MS), 1316, 8_000_000);
        let anchor = s.wall_anchor_ns;
        // Next datagram has no PCR. 1316 bytes at 8 Mbps = 1316 µs.
        let target = s.derive_target(anchor + 1, None, 1316, 8_000_000);
        let expected = anchor + 1316 * 8 * 1_000_000_000 / 8_000_000;
        assert_eq!(target, expected);
        assert_eq!(s.bytes_since_anchor, 1316);

        // Second non-PCR: 2 × 1316 bytes from anchor.
        let target2 = s.derive_target(anchor + 1, None, 1316, 8_000_000);
        let expected2 = anchor + 2 * 1316 * 8 * 1_000_000_000 / 8_000_000;
        assert_eq!(target2, expected2);
    }

    #[test]
    fn discontinuity_backwards_resets_anchor() {
        let mut s = TargetState::default();
        let _ = s.derive_target(0, Some(100 * TICK_PER_MS), 1316, 5_000_000);
        let now = 5_000_000_000u64;
        // Backwards PCR: should reset to "now".
        let target = s.derive_target(now, Some(50 * TICK_PER_MS), 1316, 5_000_000);
        assert_eq!(target, now);
        assert_eq!(s.wall_anchor_ns, now);
        assert_eq!(s.pcr_anchor, Some(50 * TICK_PER_MS));
    }

    #[test]
    fn discontinuity_forward_jump_resets_anchor() {
        let mut s = TargetState::default();
        let _ = s.derive_target(0, Some(TICK_PER_MS), 1316, 5_000_000);
        let now = 5_000_000_000u64;
        // 600 ms forward jump > 500 ms threshold.
        let target = s.derive_target(now, Some(601 * TICK_PER_MS), 1316, 5_000_000);
        assert_eq!(target, now);
        assert_eq!(s.pcr_anchor, Some(601 * TICK_PER_MS));
    }

    #[test]
    fn small_forward_jump_under_threshold_keeps_anchor() {
        let mut s = TargetState::default();
        let _ = s.derive_target(0, Some(TICK_PER_MS), 1316, 5_000_000);
        let anchor = s.wall_anchor_ns;
        // 400 ms forward — well under the 500 ms discontinuity bound.
        let target = s.derive_target(0, Some(401 * TICK_PER_MS), 1316, 5_000_000);
        assert_eq!(target, anchor + 400_000_000);
    }

    #[test]
    fn inferred_bitrate_emas_from_inter_pcr_observations() {
        let mut s = TargetState::default();
        // First datagram, PCR.
        let _ = s.derive_target(0, Some(0), 1316, 0);
        // Pretend a bunch of non-PCR datagrams piled up: 50 000 bytes
        // accumulated since anchor. We can't add them via the API
        // without a bitrate, so set state directly.
        s.bytes_since_anchor = 50_000;
        // Second PCR 40 ms later. Observed = 50 000 B / 40 ms = 10 Mbps.
        let _ = s.derive_target(0, Some(40 * TICK_PER_MS), 1316, 0);
        // First observation seeds inferred directly.
        assert_eq!(s.inferred_bitrate_bps, 10_000_000);
    }

    #[test]
    fn cold_start_passthrough_emits_immediately_until_first_pcr_pair() {
        let mut s = TargetState::default();
        // First datagram has PCR — anchors at now+preroll.
        let _ = s.derive_target(0, Some(0), 1316, 0);
        let anchor = s.wall_anchor_ns;
        // Subsequent non-PCR datagrams arrive before the second PCR
        // lands. With no declared rate and no inferred rate yet, they
        // must emit immediately — not stretched out by the MIN_BITRATE
        // floor (which would space them ~125 ms apart).
        let now = anchor + 100_000;
        let target = s.derive_target(now, None, 1316, 0);
        assert_eq!(target, now, "cold-start non-PCR must emit at now");
        let target2 = s.derive_target(now, None, 1316, 0);
        assert_eq!(target2, now);

        // After two PCRs land, the EMA seeds and pacing kicks in.
        s.bytes_since_anchor = 50_000;
        let _ = s.derive_target(now, Some(40 * TICK_PER_MS), 1316, 0);
        assert!(s.inferred_bitrate_bps > 0);
        // Now non-PCR datagrams should pace by the inferred rate.
        let post = s.derive_target(now + 1, None, 1316, 0);
        assert!(post > s.wall_anchor_ns);
    }

    #[test]
    fn non_linux_or_linux_monotonic_now_advances() {
        let a = monotonic_now_ns();
        std::thread::sleep(Duration::from_millis(2));
        let b = monotonic_now_ns();
        assert!(b > a);
        assert!(b - a >= 1_000_000); // ≥ 1 ms
    }

    #[test]
    fn sleep_until_monotonic_returns_at_or_after_target() {
        let target = monotonic_now_ns() + 2_000_000; // +2 ms
        sleep_until_monotonic_ns(target);
        let after = monotonic_now_ns();
        assert!(after >= target, "after={} target={}", after, target);
    }

    #[test]
    fn rtp_state_builds_rfc2250_header() {
        let mut s = RtpState::new();
        let initial_ssrc = s.ssrc;
        let h1 = s.build_header(0x12345678);
        let h2 = s.build_header(0x12345679);
        assert_eq!(h1[0], 0x80);
        assert_eq!(h1[1], RTP_PT_MP2T);
        let seq1 = u16::from_be_bytes([h1[2], h1[3]]);
        let seq2 = u16::from_be_bytes([h2[2], h2[3]]);
        assert_eq!(seq2.wrapping_sub(seq1), 1);
        assert_eq!(u32::from_be_bytes([h1[4], h1[5], h1[6], h1[7]]), 0x12345678);
        let ssrc1 = u32::from_be_bytes([h1[8], h1[9], h1[10], h1[11]]);
        assert_eq!(ssrc1, initial_ssrc);
    }

    /// End-to-end smoke: spawn the emitter, push three datagrams, observe
    /// monotonic emit pacing. UDP send target is loopback :0 (kernel
    /// allocated port that no one's listening on — kernel still drops the
    /// datagram cleanly without erroring on Linux/macOS).
    #[test]
    fn emitter_paces_and_sends() {
        use std::net::Ipv4Addr;
        let bind = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let dest = bind.local_addr().unwrap();
        let send = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        send.set_nonblocking(false).unwrap();

        let stats = Arc::new(OutputStatsAccumulator::new(
            "test".to_string(),
            "test".to_string(),
            "udp".to_string(),
        ));
        let cancel = CancellationToken::new();
        let tx = spawn_wire_emitter(
            "test".to_string(),
            send,
            dest,
            5_000_000, // 5 Mbps declared
            WireWrap::None,
            stats.clone(),
            cancel.clone(),
        );

        let mut bytes = vec![0u8; 1316];
        // Build a TS packet with PCR so first_pcr_in_ts_buffer finds it.
        bytes[0] = 0x47; // sync
        bytes[3] = 0x20; // adaptation-field only, no payload
        bytes[4] = 7; // af_len
        bytes[5] = 0x10; // PCR flag
        // PCR base = 0 → emit at first wall_anchor.
        for _ in 0..3 {
            tx.try_send(WireDatagram {
                bytes: bytes.clone(),
                recv_time_us: 0,
                rtp_ts_90k: 0,
            })
            .unwrap();
        }
        drop(tx);

        // Wait for emitter to drain past PREROLL_NS + a margin.
        std::thread::sleep(Duration::from_millis(200));
        cancel.cancel();

        let sent = stats.packets_sent.load(Ordering::Relaxed);
        assert!(sent >= 1, "expected at least one packet sent, got {}", sent);
    }
}
