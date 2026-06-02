// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-input de-jitter buffer — the ingress counterpart to the egress
//! release-rate servo in [`crate::engine::wire_emit`].
//!
//! ## Why an input de-jitter buffer (and why the fixed delay isn't one)
//!
//! The per-input `ingress_delay_ms` knob (the `Delayed` mode of
//! [`crate::engine::ingress_publisher`]) releases each packet at
//! `recv_time_us + delay_ms`. That is a **pure delay line**: it shifts the
//! whole timeline by a constant but reproduces the inter-arrival spacing
//! exactly — network jitter in, the same jitter out, just later. Useful for
//! deterministic cross-device alignment, useless for absorbing packet-delay
//! variation (PDV). This module is the thing that actually removes PDV.
//!
//! A real broadcast de-jitter buffer (an IRD / gateway input stage)
//! *decouples* output cadence from arrival cadence: it buffers to a target
//! fill and releases packets paced at the **recovered media rate**, so the
//! downstream stream is smooth regardless of how bursty the network
//! delivered it. This module is exactly that, built on the same proven
//! closed-loop design as the egress servo:
//!
//! 1. Recover the nominal source rate from inter-PCR observations (EMA,
//!    PID-locked for MPTS — identical math to
//!    [`crate::engine::wire_emit::TargetState::derive_target_servo`]).
//! 2. Release via a **leaky bucket** at that rate, trimmed ±`authority`
//!    (‰) by the buffer-fill error so the queue is held at `setpoint_ms`
//!    of content. A steady source-rate-vs-wallclock offset is absorbed as
//!    a steady rate trim with no accumulation and no loss.
//! 3. A hard **residence-cap shed** bounds latency by construction: any
//!    packet older than the cap (and the stale backlog behind it) is
//!    dropped and the pacing re-anchored — the downstream consumers
//!    re-clock from the (untouched) PCR exactly as they would after any
//!    network loss.
//!
//! ## Which inputs use this
//!
//! Wired on **raw UDP** and **raw RTP** (single-leg and SMPTE 2022-7
//! dual-leg) — the only TS-carrying inputs with *no* transport-layer
//! de-jitter. SRT (libsrt TSBPD), RIST (`RistSocket` reorder buffer),
//! WebRTC (str0m jitter buffer), RTMP/RTSP (TCP), ST 2110 / MXL (PTP
//! raster) and the locally-paced inputs (media-player / replay /
//! test-pattern) all already re-time at the transport or are not
//! arrival-jittered, so they are deliberately excluded.
//!
//! ## Cooperation with SMPTE 2022-7
//!
//! On a 2022-7 dual-leg RTP input the de-jitter sits **after** the
//! [`crate::redundancy::merger::BufferedHitlessMerger`]: the merger
//! de-duplicates + gap-fills in seq order and drains in **bursts** at its
//! path-differential deadline; this buffer then re-paces that bursty drain
//! to the recovered rate. They are complementary — the merger absorbs
//! *inter-leg skew* (`path_differential_ms`), the de-jitter absorbs
//! *residual PDV / rate offset* (`setpoint_ms`). Total added latency is
//! the sum of the two; that is the documented cost of running both. The
//! buffer never reorders (FIFO) so the merger's strict-monotonic seq-order
//! output is preserved.
//!
//! ## recv_time_us re-stamping (two points)
//!
//! `recv_time_us` is re-stamped twice so each buffer stage measures only its
//! own residence:
//!
//! - **On buffer ENTRY** (`now` when the packet is dequeued from the submit
//!   mpsc): so this buffer's residence-cap shed measures only the time spent
//!   in *this* buffer. Critical on a 2022-7 dual-leg RTP input — the
//!   `BufferedHitlessMerger` upstream holds each packet up to
//!   `path_differential_ms` (≤ 2 s) and preserves the original socket-arrival
//!   stamp; without the entry re-stamp the shed would measure
//!   `merger_hold + dejitter_hold` and a satellite-class path differential
//!   would trip the cap on the first check (a false shed storm). The merger's
//!   hold is a fixed, bounded, intentional latency accounted for separately.
//! - **On RELEASE** (`now` when forwarded to broadcast): so the *downstream*
//!   egress shed (which also keys off `recv_time_us`) measures only the egress
//!   queue residence, not this buffer's hold.
//!
//! The wire-level `sequence_number` / `rtp_timestamp` are left untouched.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::engine::packet::RtpPacket;
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::time::now_us;

/// Per-input de-jitter telemetry handle, registered on the flow's
/// `FlowStatsAccumulator` keyed by `input_id` (mirroring the per-input
/// video/audio stats-handle DashMaps). Keeping these per-input — rather than
/// a single flow-shared counter — is what lets the snapshot surface the
/// **active** input's de-jitter state correctly when a flow has more than one
/// de-jittered UDP/RTP input (e.g. a warm-passive failover pair): a flow-level
/// counter would conflate sheds across inputs and a flow-level depth gauge
/// would report whichever input registered first. Lock-free; the drainer owns
/// the writes, the 1 Hz snapshot path reads.
#[derive(Debug, Default)]
pub struct IngressDejitterStats {
    /// Cumulative packets shed by the residence cap.
    pub shed: AtomicU64,
    /// Live de-jitter buffer occupancy (packets).
    pub depth: AtomicUsize,
}

/// PCR discontinuity threshold (27 MHz ticks) — 0.5 s. A forward jump
/// larger than this, or any backward step, re-anchors the rate
/// measurement (input switch / source restart / loop seam). Mirrors
/// `wire_emit::PCR_DISCONTINUITY_27MHZ`.
const PCR_DISCONTINUITY_27MHZ: i64 = 13_500_000;

/// If no PCR matches the locked `anchor_pid` for this long, drop the lock
/// so the scanner re-locks onto whatever PCR PID is now present. Covers
/// seamless input switches where the new program carries its PCR on a
/// different PID. Microseconds.
const ANCHOR_STALE_US: u64 = 2_000_000;

/// mpsc capacity from the input task into the drainer. 4096 raw packets is
/// generous headroom on top of the `setpoint_ms` envelope; the residence
/// cap sheds long before this fills, so it only guards against a producer
/// burst racing the drainer wake-up.
const DRAINER_QUEUE_CAP: usize = 4096;

/// De-jitter policy. Built from the per-input `ingress_dejitter_ms`
/// (`None` → env / 60 ms default), mirroring
/// [`crate::engine::wire_emit::DejitterConfig::servo_with`].
#[derive(Clone, Copy, Debug)]
pub struct IngressDejitterConfig {
    /// Buffer fill the servo holds the queue centred on, in ms of content.
    pub setpoint_ms: u64,
    /// Servo rate authority in ‰ of the recovered source rate. 50 = ±5 %.
    pub authority_permille: u64,
    /// Hard residence cap (µs). Older packets + the stale backlog are shed.
    pub shed_residence_us: u64,
    /// After a shed, drain the buffer down to this many packets.
    pub drain_floor_pkts: usize,
}

impl IngressDejitterConfig {
    /// Resolve the policy. Precedence for the setpoint: explicit per-input
    /// `ingress_dejitter_ms` > `BILBYCAST_INGRESS_BUFFER_MS` env > 60 ms
    /// default (all clamped to [20, 2000] ms). Residence cap defaults to
    /// `max(4×setpoint, 250)` ms (overridable via
    /// `BILBYCAST_INGRESS_RESIDENCE_MS`) so a bigger buffer gets
    /// proportionally more burst headroom before the hard shed.
    pub fn from_ms(ingress_dejitter_ms: Option<u32>) -> Self {
        let setpoint_ms = ingress_dejitter_ms
            .map(|m| m as u64)
            .or_else(|| {
                std::env::var("BILBYCAST_INGRESS_BUFFER_MS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(60)
            .clamp(20, 2000);
        let cap_ms = std::env::var("BILBYCAST_INGRESS_RESIDENCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| (setpoint_ms.saturating_mul(4)).max(250))
            .clamp(setpoint_ms.saturating_add(40), 5_000);
        Self {
            setpoint_ms,
            authority_permille: 50,
            shed_residence_us: cap_ms.saturating_mul(1_000),
            drain_floor_pkts: 32,
        }
    }
}

/// Closed-loop ingress de-jitter servo. Recovers the source rate from
/// inter-PCR observations and paces release with a fill-trimmed leaky
/// bucket. Pure (no I/O, no clock reads beyond the `now_us` the caller
/// passes in) so the pacing math is unit-testable in isolation.
pub struct IngressDejitterServo {
    cfg: IngressDejitterConfig,
    /// PCR PID we locked onto (MPTS: every program has its own clock, so
    /// we must measure one program's cadence, not the cross-program
    /// average). `None` until the first PCR is seen / after a stale re-lock.
    anchor_pid: Option<u16>,
    /// Most recent PCR sample on `anchor_pid` (27 MHz ticks).
    pcr_anchor: Option<u64>,
    /// Bytes accumulated since the last PCR re-anchor (drives the rate EMA).
    bytes_since_anchor: u64,
    /// EMA of the inter-PCR observed bitrate — the nominal release rate.
    observed_rate_bps: u64,
    /// Wallclock (µs) at which the most recently released packet was
    /// scheduled. The leaky bucket schedules the next release relative to
    /// this.
    last_release_us: u64,
    /// Wallclock (µs) of the last datagram whose PCR matched `anchor_pid`.
    /// Drives the stale-lock reset.
    last_pcr_match_us: u64,
}

impl IngressDejitterServo {
    pub fn new(cfg: IngressDejitterConfig) -> Self {
        Self {
            cfg,
            anchor_pid: None,
            pcr_anchor: None,
            bytes_since_anchor: 0,
            observed_rate_bps: 0,
            last_release_us: 0,
            last_pcr_match_us: 0,
        }
    }

    /// Recovered source rate (bps). Telemetry / test introspection.
    #[allow(dead_code)]
    pub fn observed_rate_bps(&self) -> u64 {
        self.observed_rate_bps
    }

    /// Scan a packet for its PCR (PID-locked, RTP-header-aware), then feed
    /// the rate measurement. Called by the drainer on every arrival.
    pub fn observe_arrival(&mut self, pkt: &RtpPacket, now_us: u64) {
        // Stale-lock reset: if the locked PID has gone quiet (input switch
        // to a program with a different PCR PID), drop the lock so the
        // scan below re-locks. Only meaningful once a lock exists.
        if self.anchor_pid.is_some()
            && now_us.saturating_sub(self.last_pcr_match_us) > ANCHOR_STALE_US
        {
            self.anchor_pid = None;
            self.pcr_anchor = None;
            // Drop the stale measurement window too. Bytes accumulated during
            // the multi-second quiet gap must NOT be folded into the first
            // inter-PCR rate sample after the re-lock — that would massively
            // over-estimate the rate (e.g. a 50 Mbps source recovering as
            // ~Gbps), inflate release_bps, and defeat the de-jitter exactly at
            // the input switch this reset exists to ride through. Mirrors the
            // egress cold-start reset (wire_emit zeroes bytes_since_anchor via
            // `initialised = false`) and our own discontinuity / shed paths.
            // `observed_rate_bps` is intentionally kept as the EMA prior — the
            // post-switch source is usually the same nominal bitrate.
            self.bytes_since_anchor = 0;
        }
        let pcr = self.scan_pcr(pkt, now_us);
        self.ingest_measure(pcr, pkt.data.len());
    }

    /// Extract the PCR for the locked program from a packet's TS payload.
    /// Locks `anchor_pid` on the first PCR seen. RTP-wrapped packets have
    /// their header skipped; raw-TS packets scan from byte 0.
    fn scan_pcr(&mut self, pkt: &RtpPacket, now_us: u64) -> Option<u64> {
        let off = if pkt.is_raw_ts {
            0
        } else {
            crate::engine::input_transcode::rtp_header_length(&pkt.data).unwrap_or(0)
        };
        if off >= pkt.data.len() {
            return None;
        }
        let (pcr, pid) =
            crate::engine::ts_parse::first_pcr_in_ts_buffer_pid(&pkt.data[off..], self.anchor_pid)?;
        if self.anchor_pid.is_none() {
            self.anchor_pid = Some(pid);
        }
        self.last_pcr_match_us = now_us;
        Some(pcr)
    }

    /// Update the recovered-rate EMA from a (possibly absent) PCR sample
    /// plus this packet's byte count. Mirrors the egress servo's rate
    /// recovery; pacing is by byte/rate, never by PCR delta, and the PCR
    /// bytes are never rewritten — the receiver re-clocks from them.
    pub fn ingest_measure(&mut self, pcr: Option<u64>, bytes: usize) {
        if let Some(pcr) = pcr {
            match self.pcr_anchor {
                Some(prev) => {
                    let delta_27 = pcr.wrapping_sub(prev) as i64;
                    if (0..=PCR_DISCONTINUITY_27MHZ).contains(&delta_27) {
                        let delta_ns = (delta_27 as u64) * 1000 / 27;
                        // Skip sub-1 ms windows (clock-quantization noise).
                        if delta_ns >= 1_000_000 && self.bytes_since_anchor > 0 {
                            let observed_bps = self
                                .bytes_since_anchor
                                .saturating_mul(8 * 1_000_000_000)
                                / delta_ns;
                            self.observed_rate_bps = if self.observed_rate_bps == 0 {
                                observed_bps
                            } else {
                                (3 * self.observed_rate_bps + observed_bps) / 4
                            };
                        }
                    }
                    // Discontinuity (out-of-range delta): keep the rate so
                    // the leaky bucket never stalls, just re-anchor the
                    // measurement window.
                    self.pcr_anchor = Some(pcr);
                    self.bytes_since_anchor = 0;
                }
                None => {
                    self.pcr_anchor = Some(pcr);
                }
            }
        }
        self.bytes_since_anchor = self.bytes_since_anchor.saturating_add(bytes as u64);
    }

    /// Compute the release wall-instant (µs) for the front packet without
    /// advancing the bucket cursor. `depth_pkts` is the current buffer
    /// occupancy (the fill the servo regulates toward `setpoint_ms`).
    ///
    /// Cold start / no rate recovered yet → release ASAP (`now`). Once the
    /// rate is known: `release = nominal·(1 + authority·err)` where `err`
    /// is the ‰ buffer-fill error, and the next target is `interval` (wire
    /// time of these bytes at the trimmed rate) after the previous release,
    /// floored at `now` so a drained buffer emits immediately.
    pub fn peek_target(&self, front_bytes: usize, depth_pkts: usize, now_us: u64) -> u64 {
        let nominal = self.observed_rate_bps;
        if nominal == 0 {
            return now_us;
        }
        let fill_ms = (depth_pkts as u64)
            .saturating_mul(front_bytes as u64)
            .saturating_mul(8_000)
            / nominal.max(1);
        let setpoint_ms = self.cfg.setpoint_ms.max(1);
        let err_permille = (((fill_ms as i64 - setpoint_ms as i64) * 1000) / setpoint_ms as i64)
            .clamp(-1000, 1000);
        let factor_permille = 1000 + (self.cfg.authority_permille as i64 * err_permille) / 1000;
        let release_bps = nominal.saturating_mul(factor_permille.max(1) as u64) / 1000;
        let interval_us = (front_bytes as u64).saturating_mul(8_000_000) / release_bps.max(1);
        self.last_release_us.saturating_add(interval_us).max(now_us)
    }

    /// Commit a release at `target_us` — advance the leaky-bucket cursor.
    pub fn commit_release(&mut self, target_us: u64) {
        self.last_release_us = target_us;
    }

    /// Re-anchor after a residence-cap shed. Mirrors
    /// `wire_emit::shed_stale_backlog`: clear the PCR anchor + byte window
    /// and reset the bucket cursor to `now` so pacing restarts cleanly,
    /// but KEEP `observed_rate_bps` + `anchor_pid` (the recovered rate and
    /// program lock survive a burst shed).
    pub fn shed_reanchor(&mut self, now_us: u64) {
        self.pcr_anchor = None;
        self.bytes_since_anchor = 0;
        self.last_release_us = now_us;
    }
}

/// Build the submit channel + spawn the de-jitter drainer task. The
/// drainer owns the buffer, the servo, and the flow's `broadcast::Sender`,
/// releasing packets paced at the recovered rate. It exits when `cancel`
/// fires or the submit mpsc closes (all `IngressPublisher` clones dropped),
/// flushing whatever is buffered. Returns the submit `Sender` the
/// `IngressPublisher` holds.
pub fn start(
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cfg: IngressDejitterConfig,
    input_id: &str,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> mpsc::Sender<RtpPacket> {
    let (tx, rx) = mpsc::channel::<RtpPacket>(DRAINER_QUEUE_CAP);
    // Register a PER-INPUT telemetry handle (keyed by input_id) so the
    // snapshot surfaces THIS input's shed + depth on the active input's
    // stats — not a flow-level aggregate that would conflate / misattribute
    // across multiple de-jittered inputs.
    let dj_stats = Arc::new(IngressDejitterStats::default());
    stats.set_ingress_dejitter_stats(input_id, dj_stats.clone());
    tokio::spawn(run_drainer(
        rx,
        broadcast_tx,
        cfg,
        input_id.to_string(),
        dj_stats,
        cancel,
    ));
    tx
}

async fn run_drainer(
    mut rx: mpsc::Receiver<RtpPacket>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cfg: IngressDejitterConfig,
    input_id: String,
    dj_stats: Arc<IngressDejitterStats>,
    cancel: CancellationToken,
) {
    tracing::info!(
        "ingress-dejitter '{input_id}' active: setpoint={} ms, residence_cap={} ms, authority=±{}%",
        cfg.setpoint_ms,
        cfg.shed_residence_us / 1_000,
        cfg.authority_permille / 10,
    );
    let mut servo = IngressDejitterServo::new(cfg);
    let mut buffer: VecDeque<RtpPacket> = VecDeque::with_capacity(1024);
    let release_sleep = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(release_sleep);

    loop {
        let now = now_us();

        // ── Residence-cap shed ──
        // If the oldest packet has waited past the cap, the servo's ±authority
        // couldn't absorb the burst / rate offset. Drop it + the stale
        // backlog down to the floor, re-anchor, and re-clock from PCR
        // downstream. Bounds latency by construction.
        if let Some(front) = buffer.front() {
            if now.saturating_sub(front.recv_time_us) > cfg.shed_residence_us {
                buffer.pop_front();
                let mut shed: u64 = 1;
                while buffer.len() > cfg.drain_floor_pkts {
                    buffer.pop_front();
                    shed += 1;
                }
                servo.shed_reanchor(now);
                dj_stats.shed.fetch_add(shed, Ordering::Relaxed);
                dj_stats.depth.store(buffer.len(), Ordering::Relaxed);
                continue; // re-evaluate with the trimmed buffer
            }
        }

        // ── Release every front packet whose target has arrived ──
        let mut next_deadline_us: Option<u64> = None;
        while let Some(front) = buffer.front() {
            let front_bytes = front.data.len();
            let target = servo.peek_target(front_bytes, buffer.len(), now);
            if now >= target {
                let mut p = buffer.pop_front().expect("front present");
                servo.commit_release(target);
                // Re-stamp so downstream stages measure their own residence.
                p.recv_time_us = now;
                let _ = broadcast_tx.send(p);
            } else {
                next_deadline_us = Some(target);
                break;
            }
        }
        dj_stats.depth.store(buffer.len(), Ordering::Relaxed);

        match next_deadline_us {
            Some(target) => {
                let wait = target.saturating_sub(now_us());
                release_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_micros(wait));
            }
            None => {
                // Buffer empty (or fully drained) — park the timer.
                release_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(86_400));
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("ingress-dejitter '{input_id}' draining + exiting (cancelled)");
                for p in buffer.drain(..) {
                    let _ = broadcast_tx.send(p);
                }
                break;
            }
            r = rx.recv() => match r {
                Some(mut p) => {
                    let arrival = now_us();
                    servo.observe_arrival(&p, arrival);
                    // Re-stamp recv_time_us to the buffer-ENTRY instant so the
                    // residence-cap shed below measures only THIS buffer's hold.
                    // Upstream the packet may carry a much older socket-arrival
                    // stamp — notably on a SMPTE 2022-7 dual-leg RTP input, where
                    // the BufferedHitlessMerger holds it up to `path_differential_ms`
                    // (≤ 2 s) and preserves recv_time_us. Without this re-stamp the
                    // shed would measure merger_hold + dejitter_hold and a
                    // satellite-class path differential (200–500 ms) would trip the
                    // 250 ms cap on the very first check → a continuous false shed
                    // storm that re-injects the jitter we exist to remove. The
                    // merger's hold is a fixed, bounded, intentional latency
                    // accounted for separately; only the de-jitter queue's own
                    // residence belongs under this cap. (The release re-stamp below
                    // independently hands a fresh stamp to the downstream egress
                    // stage.)
                    p.recv_time_us = arrival;
                    buffer.push_back(p);
                }
                None => {
                    // All senders dropped — flush + exit.
                    for p in buffer.drain(..) {
                        let _ = broadcast_tx.send(p);
                    }
                    break;
                }
            },
            _ = &mut release_sleep, if !buffer.is_empty() => {
                // Wake to release; the loop top re-evaluates shed + release.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{BufMut, Bytes, BytesMut};

    fn cfg() -> IngressDejitterConfig {
        IngressDejitterConfig {
            setpoint_ms: 60,
            authority_permille: 50,
            shed_residence_us: 250_000,
            drain_floor_pkts: 32,
        }
    }

    /// Build a 188-byte TS packet on `pid` carrying `pcr_27mhz` in its
    /// adaptation field, so `scan_pcr` / `first_pcr_in_ts_buffer_pid` can
    /// decode it. Layout per ISO 13818-1: PCR base (33 bits) + 6 reserved
    /// + ext (9 bits), `pcr_27mhz = base*300 + ext`.
    fn ts_pkt_with_pcr(pid: u16, pcr_27mhz: u64) -> Bytes {
        let base = pcr_27mhz / 300;
        let ext = (pcr_27mhz % 300) as u16;
        let mut b = BytesMut::with_capacity(188);
        b.put_u8(0x47);
        // PID high bits (no PUSI), PID low
        b.put_u8(((pid >> 8) & 0x1F) as u8);
        b.put_u8((pid & 0xFF) as u8);
        // adaptation_field_control = 0b10 (AF only), CC 0
        b.put_u8(0x20);
        // adaptation field length: 1 flags byte + 6 PCR bytes = 7
        b.put_u8(7);
        // flags: PCR_flag (bit 4)
        b.put_u8(0x10);
        // PCR: 33-bit base, 6 reserved (1), 9-bit ext
        b.put_u8(((base >> 25) & 0xFF) as u8);
        b.put_u8(((base >> 17) & 0xFF) as u8);
        b.put_u8(((base >> 9) & 0xFF) as u8);
        b.put_u8(((base >> 1) & 0xFF) as u8);
        b.put_u8((((base & 0x1) << 7) as u8) | 0x7E | ((ext >> 8) & 0x1) as u8);
        b.put_u8((ext & 0xFF) as u8);
        // pad to 188
        while b.len() < 188 {
            b.put_u8(0xFF);
        }
        b.freeze()
    }

    fn raw_ts_packet(pid: u16, pcr_27mhz: u64, recv_time_us: u64) -> RtpPacket {
        RtpPacket {
            data: ts_pkt_with_pcr(pid, pcr_27mhz),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        }
    }

    #[test]
    fn scan_pcr_locks_pid_and_decodes() {
        let mut s = IngressDejitterServo::new(cfg());
        let pcr = 27_000_000u64; // exactly 1.0 s
        let pkt = raw_ts_packet(0x100, pcr, 1_000_000);
        let got = s.scan_pcr(&pkt, 1_000_000);
        assert_eq!(got, Some(pcr), "PCR must decode to the encoded value");
        assert_eq!(s.anchor_pid, Some(0x100), "scanner locks onto first PCR PID");
    }

    #[test]
    fn rate_recovered_from_inter_pcr() {
        // 1316-byte datagrams arriving 40 ms (27 MHz: 1_080_000 ticks) apart,
        // carrying ~5 Mbps: bytes/interval = rate. Feed explicit measures.
        let mut s = IngressDejitterServo::new(cfg());
        // First PCR anchors (no rate yet).
        s.ingest_measure(Some(0), 1316);
        assert_eq!(s.observed_rate_bps(), 0);
        // 40 ms later, having accumulated ~25 KB → 25_000*8 / 0.04 = 5 Mbps.
        // Accumulate bytes across the interval, then the next PCR computes.
        for _ in 0..18 {
            s.ingest_measure(None, 1316);
        }
        s.ingest_measure(Some(1_080_000), 1316); // +40 ms
        let r = s.observed_rate_bps();
        assert!(r > 0, "rate must be recovered after a full inter-PCR window");
        // ~ (20*1316*8) / 0.04 s ≈ 5.26 Mbps; allow a wide band.
        assert!(
            (3_000_000..=8_000_000).contains(&r),
            "recovered rate {r} bps out of expected band"
        );
    }

    #[test]
    fn cold_start_releases_asap() {
        let s = IngressDejitterServo::new(cfg());
        // No rate yet → target is now (emit ASAP), regardless of fill.
        assert_eq!(s.peek_target(1316, 100, 5_000_000), 5_000_000);
    }

    #[test]
    fn pacing_responds_to_fill() {
        // With a known rate, over-setpoint fill drains faster (shorter
        // interval → earlier target) than under-setpoint fill.
        let mut s = IngressDejitterServo::new(cfg());
        s.observed_rate_bps = 5_000_000;
        s.last_release_us = 1_000_000;
        let bytes = 1316;
        // depth chosen so fill is well below vs well above the 60 ms setpoint.
        let lo = s.peek_target(bytes, 2, 0); // tiny fill → slower release
        let hi = s.peek_target(bytes, 400, 0); // big fill → faster release
        assert!(
            hi < lo,
            "over-setpoint fill must release earlier (hi={hi} lo={lo})"
        );
        // Both are last_release + interval (above now=0).
        assert!(lo > 1_000_000 && hi > 1_000_000);
    }

    #[test]
    fn drained_buffer_floors_at_now() {
        let mut s = IngressDejitterServo::new(cfg());
        s.observed_rate_bps = 5_000_000;
        s.last_release_us = 1_000; // far in the past relative to now
        // target = max(last_release + interval, now) = now when drained.
        let now = 9_000_000;
        assert_eq!(s.peek_target(1316, 1, now), now);
    }

    #[test]
    fn shed_reanchor_keeps_rate_and_pid() {
        let mut s = IngressDejitterServo::new(cfg());
        s.observed_rate_bps = 5_000_000;
        s.anchor_pid = Some(0x100);
        s.pcr_anchor = Some(123);
        s.bytes_since_anchor = 9999;
        s.last_release_us = 42;
        s.shed_reanchor(7_000_000);
        assert_eq!(s.observed_rate_bps(), 5_000_000, "rate survives a shed");
        assert_eq!(s.anchor_pid, Some(0x100), "PID lock survives a shed");
        assert_eq!(s.pcr_anchor, None, "measurement window re-anchors");
        assert_eq!(s.bytes_since_anchor, 0);
        assert_eq!(s.last_release_us, 7_000_000, "bucket cursor reset to now");
    }

    #[test]
    fn discontinuity_keeps_rate() {
        // A backward / huge-forward PCR step must not corrupt the recovered
        // rate (the leaky bucket keeps pacing); it only re-anchors.
        let mut s = IngressDejitterServo::new(cfg());
        s.observed_rate_bps = 5_000_000;
        s.pcr_anchor = Some(1_000_000);
        s.bytes_since_anchor = 5000;
        // Huge forward jump (> 0.5 s) → discontinuity branch.
        s.ingest_measure(Some(1_000_000 + 20_000_000), 1316);
        assert_eq!(s.observed_rate_bps(), 5_000_000, "rate preserved across discontinuity");
        assert_eq!(s.pcr_anchor, Some(1_000_000 + 20_000_000), "re-anchored on the new PCR");
        assert_eq!(s.bytes_since_anchor, 1316, "window reset then this packet counted");
    }

    #[test]
    fn stale_lock_reset_clears_byte_window() {
        // Regression for the input-switch rate-pollution bug: when the locked
        // PCR PID goes quiet past ANCHOR_STALE_US (switch to a program with a
        // different PCR PID), the stale byte accumulation must be dropped on
        // the re-lock — otherwise the first post-switch inter-PCR window
        // computes a grossly over-estimated rate and the de-jitter is defeated
        // exactly at the switch it exists to ride through.
        let mut s = IngressDejitterServo::new(cfg());
        // Lock onto PID 0x100.
        let p1 = raw_ts_packet(0x100, 27_000_000, 1_000_000);
        s.observe_arrival(&p1, 1_000_000);
        assert_eq!(s.anchor_pid, Some(0x100));
        // Quiet gap: packets carry PCR on a DIFFERENT pid (0x200), so the scan
        // — still filtered to 0x100 — returns None and accumulates bytes
        // without advancing last_pcr_match_us. Stay inside the stale window.
        for _ in 0..1000 {
            let pq = raw_ts_packet(0x200, 0, 1_500_000);
            s.observe_arrival(&pq, 1_500_000);
        }
        assert!(s.bytes_since_anchor > 100_000, "stale byte window accumulated");
        // Now past the stale threshold, a new PCR arrives on the new PID.
        let now = 1_000_000 + ANCHOR_STALE_US + 100_000;
        let pnew = raw_ts_packet(0x200, 50_000_000, now);
        s.observe_arrival(&pnew, now);
        assert_eq!(s.anchor_pid, Some(0x200), "re-locked onto the new PCR PID");
        assert_eq!(
            s.bytes_since_anchor,
            pnew.data.len() as u64,
            "byte window must restart clean on the stale re-lock (only the \
             re-locking packet's bytes), not carry the multi-second stale sum"
        );
    }

    #[test]
    fn config_from_ms_clamps_and_derives_cap() {
        let c = IngressDejitterConfig::from_ms(Some(80));
        assert_eq!(c.setpoint_ms, 80);
        assert_eq!(c.shed_residence_us, 320_000, "cap = 4×setpoint when > 250 ms");
        let c2 = IngressDejitterConfig::from_ms(Some(40));
        assert_eq!(c2.shed_residence_us, 250_000, "cap floored at 250 ms");
        let c3 = IngressDejitterConfig::from_ms(Some(5000));
        assert_eq!(c3.setpoint_ms, 2000, "setpoint clamped to 2000 ms");
    }
}
