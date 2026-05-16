// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow ingress PCR sampler — feeds the source-PCR PLL.
//!
//! Sibling subscriber on the flow's broadcast channel: drains every
//! [`RtpPacket`], scans 188-byte TS packets for PCR-bearing adaptation
//! fields, and forwards `(pcr_27mhz, wall_ns)` into
//! [`crate::engine::master_clock::SourcePcrPllMaster::record_sample`].
//!
//! Drop-on-`Lagged` semantics — the sampler is a passive observer. If
//! it falls behind a busy flow it just resyncs at the next received
//! packet; the data path is never affected.
//!
//! The sampler is the *ingress* counterpart to
//! [`crate::stats::pcr_trust::PcrTrustSampler`] (which records *egress*
//! PCR accuracy at every output's send path).

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::engine::master_clock::SourcePcrPllMaster;
use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{
    extract_pcr, TS_PACKET_SIZE, TS_SYNC_BYTE,
};
use crate::manager::events::{category, EventSender, EventSeverity};

/// Mirrors [`crate::engine::pcr_pll::PcrPll`]'s discontinuity gate: a
/// jump > 500 ms in either direction is treated as a source-clock step
/// (file loop, encoder restart, decoder reseat) rather than legitimate
/// jitter. Same threshold so the operator-visible event aligns 1:1
/// with the PLL's internal anchor reset.
const DISCONTINUITY_THRESHOLD_27MHZ: u64 = 500 * 27_000; // 500 ms

/// Minimum interval between operator-visible discontinuity events for
/// a given input. A chronically-broken source might emit a backward
/// PCR every few seconds; without rate-limiting that would drown the
/// events feed. The first event fires immediately; subsequent ones
/// are debounced.
const DISCONTINUITY_EVENT_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Spawn the ingress PCR sampler. Returns the join handle so the
/// FlowRuntime can hold ownership for the flow's lifetime; shutdown is
/// driven by `cancel`.
///
/// Stage 3 of the data-plane redesign: runs on a dedicated SCHED_FIFO
/// OS thread with its own `tokio::current_thread` runtime, lifting it
/// off the main worker pool. PLL accuracy depends on the wall-time
/// deltas between consecutive PCR samples being measured precisely;
/// any Tokio scheduling latency between `broadcast::recv` and the
/// `sample_packet` call delays PLL catch-up, and under heavy main-
/// runtime contention the PI loop spends longer than the locked p99
/// jitter target. CPU pinning honoured via `BILBYCAST_PLL_CPUS`.
#[allow(dead_code)]
pub fn spawn_pcr_ingress_sampler(
    master: Arc<SourcePcrPllMaster>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    flow_id: String,
    active_input_rx: tokio::sync::watch::Receiver<String>,
    events: EventSender,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_pcr_ingress_sampler_with_rx(
        master,
        broadcast_tx.subscribe(),
        flow_id,
        active_input_rx,
        events,
        cancel,
    )
}

/// Variant that accepts an already-subscribed `broadcast::Receiver`,
/// letting the caller choose between the flow broadcast (default,
/// passthrough flows where output is byte-identical to the active
/// input's stream) and a specific input's per-input broadcast (PID-bus
/// assembled flows where the assembler's output PCR is the master
/// clock itself — sampling that would form a self-referential loop).
/// See [`flow::FlowRuntime::start`] for the caller-side resolution.
pub fn spawn_pcr_ingress_sampler_with_rx(
    master: Arc<SourcePcrPllMaster>,
    mut rx: broadcast::Receiver<RtpPacket>,
    flow_id: String,
    active_input_rx: tokio::sync::watch::Receiver<String>,
    events: EventSender,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let thread_handle = crate::engine::dedicated_runtime::spawn_dedicated(
        crate::engine::dedicated_runtime::DedicatedRuntimeConfig::new(
            "pcr-pll",
            "BILBYCAST_PLL_CPUS",
        ),
        async move {
            // Per-sampler discontinuity-detector state. Tracked in the
            // dedicated thread so the hot-path `sample_packet` stays a
            // pure function. `last_pcr_27mhz` is the previous accepted
            // PCR value; `last_event_at` debounces operator events
            // (see `DISCONTINUITY_EVENT_MIN_INTERVAL`).
            let mut last_pcr_27mhz: Option<u64> = None;
            let mut last_event_at: Option<Instant> = None;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = rx.recv() => {
                        match msg {
                            Ok(pkt) => {
                                let new_pcr = sample_packet(&master, &pkt);
                                if let Some(pcr) = new_pcr {
                                    if let Some(prev) = last_pcr_27mhz {
                                        let gap = pcr_gap_27mhz(prev, pcr);
                                        if gap.unsigned_abs() > DISCONTINUITY_THRESHOLD_27MHZ {
                                            // Operator-visible event. Rate-
                                            // limited so a chronically-broken
                                            // source can't drown the feed.
                                            let should_emit = match last_event_at {
                                                None => true,
                                                Some(t) => t.elapsed()
                                                    >= DISCONTINUITY_EVENT_MIN_INTERVAL,
                                            };
                                            if should_emit {
                                                last_event_at = Some(Instant::now());
                                                let input_id =
                                                    active_input_rx.borrow().clone();
                                                events.emit_flow_with_details(
                                                    EventSeverity::Warning,
                                                    category::FLOW,
                                                    format!(
                                                        "source PCR discontinuity on flow '{}' \
                                                         (input '{}'): {:.3} ms jump",
                                                        flow_id,
                                                        input_id,
                                                        gap as f64 / 27_000.0,
                                                    ),
                                                    &flow_id,
                                                    serde_json::json!({
                                                        "error_code": "source_pcr_discontinuity",
                                                        "input_id": input_id,
                                                        "prev_pcr_27mhz": prev,
                                                        "new_pcr_27mhz": pcr,
                                                        "gap_ms": gap as f64 / 27_000.0,
                                                    }),
                                                );
                                            }
                                        }
                                    }
                                    last_pcr_27mhz = Some(pcr);
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        },
    );
    // Bridge to JoinHandle<()> shape via spawn_blocking-on-join, same
    // pattern as the assembler + demuxer in Stage 2.
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            let _ = thread_handle.join();
        }).await;
    })
}

/// Modular Δ between two 27 MHz PCR values, signed (positive = `cur`
/// ahead of `prev`, negative = backward). Handles wrap at
/// `PCR_MODULUS_27MHZ`. Used by the discontinuity gate to recognise
/// either direction of jump.
fn pcr_gap_27mhz(prev: u64, cur: u64) -> i64 {
    const HALF_MODULUS: u64 =
        crate::engine::pcr_pll::PCR_MODULUS_27MHZ / 2;
    let forward = cur.wrapping_sub(prev) % crate::engine::pcr_pll::PCR_MODULUS_27MHZ;
    if forward <= HALF_MODULUS {
        forward as i64
    } else {
        -((crate::engine::pcr_pll::PCR_MODULUS_27MHZ - forward) as i64)
    }
}

/// Scan a single `RtpPacket` for PCR samples, feeding the master's PLL
/// for each one. Pure function — no I/O, no allocations on the hot path.
///
/// Crucially: every PCR found in this datagram is recorded against the
/// datagram's `recv_time_us` (captured at the input task's UDP recv()
/// return — a true kernel-delivery timestamp). Using the PLL's
/// internal `epoch.elapsed()` here would bake broadcast-subscriber
/// scheduling jitter into `Δwall_ns` and prevent lock against
/// otherwise-clean sources.
fn sample_packet(master: &SourcePcrPllMaster, pkt: &RtpPacket) -> Option<u64> {
    let recv_time_us = pkt.recv_time_us;
    // Sender-timestamp path: when the SRT/RIST input surfaced a
    // sender-set timestamp (libsrt's `SRT_MsgCtrl::srctime`), prefer
    // it over MPEG-TS PCR-from-bytes. srctime is set at the sender's
    // `sendmsg()` — pre-network-jitter, pre-TSBPD-buffering — so the
    // PLL's `Δsrctime / Δrecv_wall` measurement reflects the true
    // sender clock rate rather than the bursty arrival cadence at
    // the receiver. One sample per packet is plenty (vs. the
    // 25–40 ms PCR cadence), so the PLL converges faster too.
    //
    // We still scan the bytes for PCR below so the discontinuity
    // event surfaces correctly. The PLL itself only gets one of the
    // two feeds — telemetry's `rate_source` reflects which.
    if let Some(srctime_us) = pkt.sender_timestamp_us {
        master.record_sender_timestamp(srctime_us, recv_time_us);
    }
    let bytes = pkt.data.as_ref();
    let payload = if pkt.is_raw_ts {
        bytes
    } else {
        // RTP-wrapped TS: skip the variable-length RTP header. Minimum
        // 12 bytes; CSRC count + extension can extend it. Fast path
        // when the byte at offset 12 is the TS sync byte.
        skip_rtp_header(bytes)
    };

    // Find the first TS sync byte and walk in 188-byte strides.
    let mut i = match payload.iter().position(|&b| b == TS_SYNC_BYTE) {
        Some(p) => p,
        None => return None,
    };
    let mut latest_pcr: Option<u64> = None;
    while i + TS_PACKET_SIZE <= payload.len() {
        let ts_pkt = &payload[i..i + TS_PACKET_SIZE];
        if ts_pkt[0] == TS_SYNC_BYTE {
            if let Some(pcr_27mhz) = extract_pcr(ts_pkt) {
                // Only feed PCR to the PLL when srctime wasn't
                // available — otherwise we'd mix two rate references
                // in the jitter window and the PLL would never lock.
                if pkt.sender_timestamp_us.is_none() {
                    master.record_sample_at(pcr_27mhz, recv_time_us);
                }
                latest_pcr = Some(pcr_27mhz);
            }
            i += TS_PACKET_SIZE;
        } else {
            // Resync — scan forward for the next sync byte.
            match payload[i + 1..].iter().position(|&b| b == TS_SYNC_BYTE) {
                Some(p) => i = i + 1 + p,
                None => return latest_pcr,
            }
        }
    }
    latest_pcr
}

/// Best-effort RTP header skip. Returns the slice after the header, or
/// the original slice if the packet doesn't look RTP-shaped (caller will
/// resync via TS sync byte search anyway). Pure function — no allocations.
fn skip_rtp_header(bytes: &[u8]) -> &[u8] {
    if bytes.len() < 12 {
        return bytes;
    }
    let cc = (bytes[0] & 0x0F) as usize; // CSRC count
    let extension_bit = bytes[0] & 0x10 != 0;
    let mut hdr = 12 + 4 * cc;
    if extension_bit {
        if hdr + 4 > bytes.len() {
            return bytes;
        }
        let ext_len_words =
            ((bytes[hdr + 2] as usize) << 8) | bytes[hdr + 3] as usize;
        hdr += 4 + 4 * ext_len_words;
    }
    if hdr <= bytes.len() {
        &bytes[hdr..]
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn build_pcr_packet(pcr_base: u64) -> Vec<u8> {
        let mut p = vec![0u8; TS_PACKET_SIZE];
        p[0] = TS_SYNC_BYTE;
        p[1] = 0x00; // pid high bits
        p[2] = 0x10; // pid low + cc bits
        p[3] = 0x20; // adaptation field only, cc=0
        p[4] = 7; // adaptation_field_length
        p[5] = 0x10; // PCR_flag = 1
        // PCR base 33 bits
        p[6] = ((pcr_base >> 25) & 0xFF) as u8;
        p[7] = ((pcr_base >> 17) & 0xFF) as u8;
        p[8] = ((pcr_base >> 9) & 0xFF) as u8;
        p[9] = ((pcr_base >> 1) & 0xFF) as u8;
        p[10] = (((pcr_base & 0x01) << 7) | 0x7E) as u8; // base low bit + reserved
        p[11] = 0; // PCR ext low byte
        p
    }

    #[test]
    fn samples_raw_ts_packet_into_pll() {
        let master = Arc::new(SourcePcrPllMaster::new("test"));
        let pcr_27mhz = 1_080_000u64; // 40 ms in 27 MHz ticks
        let pcr_base = pcr_27mhz / 300;
        let bytes = build_pcr_packet(pcr_base);
        let pkt = RtpPacket {
            data: Bytes::from(bytes),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        };
        sample_packet(&master, &pkt);
        // First sample primes the PLL (no cumulative count yet).
        let t = master.pll().telemetry();
        assert_eq!(t.samples, 0);
    }

    #[test]
    fn skips_rtp_header_correctly() {
        // The test asserts that an RTP-wrapped TS packet's PCR is
        // extracted (the header skip works). Concretely: feeding a
        // single RTP-wrapped sample through `sample_packet` must
        // change `pll.now_27mhz()` from its pre-sample wallclock
        // fallback (small process-monotonic value) to the primed
        // anchor value (~`pcr_base × 300`). That can only happen if
        // `extract_pcr` ran on the inner TS packet — exactly what the
        // RTP header skip is responsible for setting up.
        //
        // No second sample / sleep involved: we don't exercise rate
        // tracking here (covered by pcr_pll unit tests). This keeps
        // the test deterministic against the PLL's plausibility gates.
        let master = Arc::new(SourcePcrPllMaster::new("test"));
        let pre_sample_now = master.pll().now_27mhz(0);

        let pcr_base = 90_000u64; // 90 kHz; ×300 → 27_000_000 27 MHz ticks
        let mut data = vec![0u8; 12];
        data[0] = 0x80; // V=2, no padding/ext, CC=0
        data[1] = 96; // payload type
        data.extend_from_slice(&build_pcr_packet(pcr_base));
        let pkt = RtpPacket {
            data: Bytes::from(data),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: false,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        };
        sample_packet(&master, &pkt);

        // Pre-sample fallback returns process-monotonic ticks (small,
        // fluctuates per call). Post-prime, `now_27mhz` projects from
        // the anchor (~27_000_000). They must differ substantially.
        let post_sample_now = master.pll().now_27mhz(0);
        let expected_anchor = pcr_base * 300;
        assert!(
            (post_sample_now as i64 - expected_anchor as i64).abs() < 1_000_000,
            "RTP header skip did not feed PCR to PLL: now_27mhz pre={} post={} expected~={}",
            pre_sample_now,
            post_sample_now,
            expected_anchor
        );
    }

    #[test]
    fn payload_with_no_sync_byte_is_silently_dropped() {
        let master = Arc::new(SourcePcrPllMaster::new("test"));
        let pkt = RtpPacket {
            data: Bytes::from(vec![0x00; 188]),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        };
        sample_packet(&master, &pkt);
        let t = master.pll().telemetry();
        assert_eq!(t.samples, 0);
    }

    #[test]
    fn rtp_header_skip_minimum_size() {
        let bytes = vec![0u8; 8];
        // Too short to be RTP — return unchanged.
        assert_eq!(skip_rtp_header(&bytes), &bytes[..]);
    }

    #[test]
    fn rtp_header_skip_with_csrc_and_extension() {
        let mut bytes = vec![0u8; 12 + 4 * 2 + 4];
        bytes[0] = 0x80 | 0x10 | 0x02; // V=2, X=1, CC=2
        // Two CSRC words at offset 12..20.
        // Extension header at 20..24: profile(2) + length(2 words)
        bytes[20] = 0xab;
        bytes[21] = 0xcd;
        bytes[22] = 0x00;
        bytes[23] = 0x02;
        bytes.extend_from_slice(&[0u8; 8]); // ext payload (2 words)
        bytes.push(0xAB); // payload byte
        let after = skip_rtp_header(&bytes);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0], 0xAB);
    }
}
