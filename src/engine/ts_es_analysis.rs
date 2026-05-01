// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! PID-bus Phase 8: per-elementary-stream analyzer. One lightweight task
//! per `(input_id, source_pid)` channel on the flow's [`FlowEsBus`].
//!
//! Each task subscribes to its own broadcast channel on the bus and
//! updates a dedicated [`PerEsAccumulator`] — packets, bytes, CC errors,
//! PCR discontinuity events, last-seen `stream_type`. Bitrate is derived
//! at snapshot time by the accumulator's `ThroughputEstimator`.
//!
//! Design rationale (Phase 8 Q1 / Q2 — see project_pid_bus memory):
//!
//! - **Lazy-per-PID spawning**: each `(input_id, source_pid)` gets its
//!   own analyzer task, not a single monolithic fan-in. A slow or stalled
//!   PID can't starve siblings; each task owns its own `PerEsAccumulator`
//!   cell so there is zero contention on the atomic counters.
//! - **Key internally by `(input_id, source_pid)`**: this is the stable
//!   identity across runtime plan swaps. Egress `out_pid` is a property
//!   of the current plan — it lives in `FlowStatsAccumulator.pid_routing`
//!   and is annotated into the wire shape at snapshot time.
//! - **No heavy parsing**: this is the lightweight shim the Phase 8
//!   scope note calls out. Deep PES / PMT / TR-101290 parsing stays on
//!   the flow-level analyzer subscribed to `broadcast_tx` so passthrough
//!   flows lose nothing. Per-ES gives operators CC + PCR disc + bitrate
//!   breakdown — enough to locate a problem PID during an incident.
//!
//! Performance invariant: one task, one broadcast receiver, one atomic
//! increment per packet. No allocation on the hot path. On lag
//! (`RecvError::Lagged`) we just resync — counters stay consistent
//! because CC-jump is already a valid discontinuity event in broadcast.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::stats::collector::{FlowStatsAccumulator, PerEsAccumulator};

use super::ts_es_bus::{EsPacket, FlowEsBus};

/// Spawn one analyzer task for a given `(input_id, source_pid)` pair.
///
/// Returns a [`JoinHandle`] the caller should keep alongside the flow's
/// other analyzer handles so it is cancelled on flow teardown.
pub fn spawn_per_es_analyzer(
    input_id: impl Into<String>,
    source_pid: u16,
    bus: Arc<FlowEsBus>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let input_id = input_id.into();
    let rx = bus.subscribe(&input_id, source_pid);
    let acc = flow_stats.per_es_acc(&input_id, source_pid);
    tokio::spawn(async move {
        run(rx, acc, cancel).await;
    })
}

async fn run(
    mut rx: broadcast::Receiver<EsPacket>,
    acc: Arc<PerEsAccumulator>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = rx.recv() => {
                match result {
                    Ok(pkt) => process(&pkt, &acc),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Lost packets between this analyzer and the bus. The
                        // CC-jump on the next packet already counts as a
                        // discontinuity — no separate signal needed.
                        acc.last_cc.store(0x100, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

/// Per-packet processing. Kept as a free function so it can be unit-tested
/// without touching the tokio runtime or the broadcast machinery.
#[inline]
fn process(pkt: &EsPacket, acc: &PerEsAccumulator) {
    acc.packets.fetch_add(1, Ordering::Relaxed);
    acc.bytes
        .fetch_add(pkt.payload.len() as u64, Ordering::Relaxed);
    if pkt.stream_type != 0 {
        acc.stream_type
            .store(pkt.stream_type as u64, Ordering::Relaxed);
    }

    // CC tracking. Adaptation-field-only TS packets (no payload) don't
    // increment the CC, so we check the adaptation field control bits
    // before comparing.
    if pkt.payload.len() >= 4 {
        let afc = (pkt.payload[3] >> 4) & 0x03;
        let has_payload = (afc & 0x01) != 0;
        if has_payload {
            let cc = pkt.payload[3] & 0x0F;
            let prev = acc.last_cc.load(Ordering::Relaxed);
            if prev <= 0x0F {
                let expected = (prev as u8 + 1) & 0x0F;
                if cc != expected {
                    acc.cc_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            acc.last_cc.store(cc as u64, Ordering::Relaxed);
        }
    }

    // PCR discontinuity detection. Broadcast definition of
    // discontinuity: PCR advanced more than 100 ms (2_700_000 ticks) or
    // went backwards. Matches the threshold the flow-level TR-101290
    // analyzer uses for `pcr_discontinuity_errors`.
    if pkt.has_pcr {
        if let Some(pcr) = pkt.pcr {
            const MAX_FORWARD_27MHZ: u64 = 100 * 27_000; // 100 ms in 27 MHz ticks
            let prev = acc.last_pcr_27mhz.load(Ordering::Relaxed);
            if prev != u64::MAX {
                if pcr < prev || (pcr - prev) > MAX_FORWARD_27MHZ {
                    acc.pcr_discontinuity_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            acc.last_pcr_27mhz.store(pcr, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn mk_ts_pkt(pid: u16, cc: u8, af_bit: u8) -> EsPacket {
        // Minimal 188-byte TS packet. Byte 3: AFC (bits 5-4) + CC (bits 3-0).
        let mut buf = vec![0u8; 188];
        buf[0] = 0x47;
        buf[1] = ((pid >> 8) & 0x1F) as u8;
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = (af_bit << 4) | (cc & 0x0F);
        EsPacket {
            source_pid: pid,
            stream_type: 0x1B,
            payload: Bytes::from(buf),
            is_pusi: false,
            has_pcr: false,
            pcr: None,
            recv_time_us: 0,
            upstream_seq: None,
        }
    }

    #[test]
    fn cc_sequence_no_errors() {
        let acc = Arc::new(PerEsAccumulator::new("in".into(), 0x100));
        for cc in 0..=15u8 {
            process(&mk_ts_pkt(0x100, cc, 0x1), &acc);
        }
        assert_eq!(acc.cc_errors.load(Ordering::Relaxed), 0);
        assert_eq!(acc.packets.load(Ordering::Relaxed), 16);
    }

    #[test]
    fn cc_jump_counts_as_error() {
        let acc = Arc::new(PerEsAccumulator::new("in".into(), 0x100));
        process(&mk_ts_pkt(0x100, 0, 0x1), &acc);
        process(&mk_ts_pkt(0x100, 1, 0x1), &acc);
        process(&mk_ts_pkt(0x100, 5, 0x1), &acc); // jump from 1 → 5, expected 2
        assert_eq!(acc.cc_errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn adaptation_only_packets_do_not_increment_cc() {
        let acc = Arc::new(PerEsAccumulator::new("in".into(), 0x100));
        process(&mk_ts_pkt(0x100, 0, 0x1), &acc); // payload present
        // AF-only (AFC=0x2): CC is frozen, no error expected even if cc value
        // looks wrong.
        process(&mk_ts_pkt(0x100, 7, 0x2), &acc);
        process(&mk_ts_pkt(0x100, 1, 0x1), &acc); // payload, cc rolls to 1 (expected)
        assert_eq!(acc.cc_errors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pcr_discontinuity_triggers_on_backward_jump() {
        let acc = Arc::new(PerEsAccumulator::new("in".into(), 0x100));
        let mut pkt = mk_ts_pkt(0x100, 0, 0x1);
        pkt.has_pcr = true;
        pkt.pcr = Some(1_000_000);
        process(&pkt, &acc);
        // 40 ms forward — normal, no disc.
        pkt.pcr = Some(1_000_000 + 40 * 27_000);
        process(&pkt, &acc);
        assert_eq!(acc.pcr_discontinuity_errors.load(Ordering::Relaxed), 0);
        // Jump backwards → discontinuity.
        pkt.pcr = Some(500_000);
        process(&pkt, &acc);
        assert_eq!(acc.pcr_discontinuity_errors.load(Ordering::Relaxed), 1);
    }
}
