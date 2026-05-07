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

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::engine::master_clock::SourcePcrPllMaster;
use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{
    extract_pcr, TS_PACKET_SIZE, TS_SYNC_BYTE,
};

/// Spawn the ingress PCR sampler. Returns the join handle so the
/// FlowRuntime can hold ownership for the flow's lifetime; shutdown is
/// driven by `cancel`.
pub fn spawn_pcr_ingress_sampler(
    master: Arc<SourcePcrPllMaster>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = rx.recv() => {
                    match msg {
                        Ok(pkt) => sample_packet(&master, &pkt),
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Passive observer: resync silently.
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    })
}

/// Scan a single `RtpPacket` for PCR samples, feeding the master's PLL
/// for each one. Pure function — no I/O, no allocations on the hot path.
fn sample_packet(master: &SourcePcrPllMaster, pkt: &RtpPacket) {
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
        None => return,
    };
    while i + TS_PACKET_SIZE <= payload.len() {
        let pkt = &payload[i..i + TS_PACKET_SIZE];
        if pkt[0] == TS_SYNC_BYTE {
            if let Some(pcr_27mhz) = extract_pcr(pkt) {
                master.record_sample(pcr_27mhz);
            }
            i += TS_PACKET_SIZE;
        } else {
            // Resync — scan forward for the next sync byte.
            match payload[i + 1..].iter().position(|&b| b == TS_SYNC_BYTE) {
                Some(p) => i = i + 1 + p,
                None => return,
            }
        }
    }
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
        };
        sample_packet(&master, &pkt);
        // First sample primes the PLL (no cumulative count yet).
        let t = master.pll().telemetry();
        assert_eq!(t.samples, 0);
    }

    #[test]
    fn skips_rtp_header_correctly() {
        let pcr_base = 90_000u64;
        let mut data = vec![0u8; 12]; // basic 12-byte RTP header
        data[0] = 0x80; // V=2, no padding/ext, CC=0
        data[1] = 96; // payload type
        data.extend_from_slice(&build_pcr_packet(pcr_base));
        let master = Arc::new(SourcePcrPllMaster::new("test"));
        let pkt = RtpPacket {
            data: Bytes::from(data),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: false,
            upstream_seq: None,
            upstream_leg_id: None,
        };
        sample_packet(&master, &pkt);
        // Prime — no cumulative samples yet.
        let t = master.pll().telemetry();
        assert_eq!(t.samples, 0);
        // Feed one more — Δ should be honoured and bump samples to 1.
        sample_packet(&master, &pkt);
        // Same PCR + zero Δwall → discontinuity filter resets, still 0.
        // Build a different PCR for the second sample.
        let mut data2 = vec![0u8; 12];
        data2[0] = 0x80;
        data2[1] = 96;
        data2.extend_from_slice(&build_pcr_packet(pcr_base + 3_600));
        let pkt2 = RtpPacket {
            data: Bytes::from(data2),
            sequence_number: 1,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: false,
            upstream_seq: None,
            upstream_leg_id: None,
        };
        // Tiny sleep ensures wall_ns differs.
        std::thread::sleep(std::time::Duration::from_millis(2));
        sample_packet(&master, &pkt2);
        let t2 = master.pll().telemetry();
        // The first call primed the PLL; the second call (same PCR) and
        // third (advanced PCR) each produce a delta. Just confirm we
        // saw at least one accepted sample — the RTP header skip is
        // working if PCRs reach the PLL.
        assert!(t2.samples >= 1, "ingress sampler did not feed PLL via RTP path");
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
