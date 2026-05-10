// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CBR (constant bitrate) MPEG-TS NULL packet padder.
//!
//! Sits between the per-output transcoder pipeline and the wire-emit
//! channel. The transcoder produces VBR encoded TS at whatever rate the
//! encoder happens to emit; downstream broadcast multiplexers / receivers
//! that expect a CBR feed need that rate inflated to a stable target.
//!
//! ## Algorithm
//!
//! Time-based budget: track wallclock-elapsed since the padder started
//! and the cumulative bytes emitted (input + padding). On every
//! [`process`](TsNullPadder::process) call:
//!
//! 1. Sample monotonic now → compute `budget_bytes = target_bps × elapsed
//!    / 8`.
//! 2. Emit the input bytes verbatim into `output`.
//! 3. While `bytes_emitted + 188 ≤ budget_bytes`, append a NULL TS packet
//!    (PID `0x1FFF`, payload-only, monotonic CC). Stops one packet short
//!    of the budget so the next call can resume cleanly without
//!    overshoot.
//!
//! Wire pacing remains the responsibility of `engine::wire_emit`, which
//! observes inter-PCR delta on the inflated stream and releases packets
//! at the target wallclock cadence. By the time `wire_emit` sees the
//! padded stream, the observed inter-PCR rate matches the configured
//! CBR target.
//!
//! ## Continuity
//!
//! NULL-PID continuity counters are not strictly required by ISO/IEC
//! 13818-1 (PID `0x1FFF` is exempt from CC checks) but are emitted
//! anyway because some loose receivers (notably FFmpeg `tsanalyzer`
//! before 5.1) flag the warning even on the NULL PID. CC starts at 0
//! and increments mod 16.
//!
//! ## Cold start
//!
//! The first call records the epoch and emits no padding (budget is 0).
//! Subsequent calls hit the steady-state budget formula. This avoids a
//! pathological "first call dumps a second's worth of NULLs" burst.
//!
//! ## Burst cap
//!
//! A single `process` call emits at most [`MAX_NULL_BURST`] NULL packets
//! (~64 KiB). After a long upstream silence the budget overflow is
//! absorbed gradually instead of all at once — wire_emit's send queue
//! is bounded, and dumping 1 second's worth of padding at once would
//! overflow it.

use crate::engine::ts_parse::{TS_PACKET_SIZE, TS_SYNC_BYTE};

/// PID 0x1FFF — TS NULL packet, ignored by every TR-101290-compliant
/// receiver.
const NULL_PID: u16 = 0x1FFF;

/// Per-call upper bound on emitted NULL packets. ~64 KiB, well within a
/// single recvmsg's worth of buffer on every wire-emit downstream.
const MAX_NULL_BURST: usize = 350;

/// Minimum target rate (1 Mbps). Below this the padder is pointless —
/// the transcoder's natural rate would dominate.
pub const MIN_TARGET_KBPS: u32 = 1_000;

/// Maximum target rate (1 Gbps). Sanity bound; production CBR feeds
/// rarely exceed 100 Mbps.
pub const MAX_TARGET_KBPS: u32 = 1_000_000;

/// Time-based CBR null-packet padder. Cheap to construct; one instance
/// per output. Not `Sync` (the `bytes_emitted` + `cc` counters mutate
/// on the data path); driven from a single output task.
pub struct TsNullPadder {
    target_bps: u64,
    epoch_ns: u64,
    bytes_emitted: u64,
    null_cc: u8,
    initialised: bool,
}

impl TsNullPadder {
    /// Construct a padder targeting `target_kbps` kilobits per second.
    /// The caller is expected to validate the rate is within
    /// `[MIN_TARGET_KBPS, MAX_TARGET_KBPS]` at config load time.
    pub fn new(target_kbps: u32) -> Self {
        Self {
            target_bps: u64::from(target_kbps) * 1_000,
            epoch_ns: 0,
            bytes_emitted: 0,
            null_cc: 0,
            initialised: false,
        }
    }

    /// Forward the input TS bytes into `output` and append NULL packets
    /// to keep the cumulative emit rate at the configured target. The
    /// input is appended verbatim — no parsing, no rewriting; the caller
    /// is responsible for 188-byte alignment.
    pub fn process(&mut self, input: &[u8], output: &mut Vec<u8>) {
        let now_ns = monotonic_now_ns();
        if !self.initialised {
            self.epoch_ns = now_ns;
            self.initialised = true;
        }

        // Always emit the input first so the encoder never sees back-pressure
        // from the padder. NULLs are filler around the natural rate.
        output.extend_from_slice(input);
        self.bytes_emitted = self
            .bytes_emitted
            .saturating_add(input.len() as u64);

        let elapsed_ns = now_ns.saturating_sub(self.epoch_ns);
        // budget_bytes = target_bps × elapsed_s / 8
        //              = target_bps × elapsed_ns / 8e9
        // Use u128 intermediate to avoid u64 overflow at high bitrates
        // over long uptimes (1 Gbps × 1 day ≈ 10 PB).
        let budget = ((self.target_bps as u128) * (elapsed_ns as u128) / 8_000_000_000) as u64;

        let mut emitted_this_call = 0usize;
        while self.bytes_emitted + TS_PACKET_SIZE as u64 <= budget
            && emitted_this_call < MAX_NULL_BURST
        {
            let pkt = self.make_null_packet();
            output.extend_from_slice(&pkt);
            self.bytes_emitted = self
                .bytes_emitted
                .saturating_add(TS_PACKET_SIZE as u64);
            emitted_this_call += 1;
        }
    }

    fn make_null_packet(&mut self) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((NULL_PID >> 8) as u8) & 0x1F; // PUSI=0, PID high
        pkt[2] = (NULL_PID & 0xFF) as u8;
        pkt[3] = 0x10 | (self.null_cc & 0x0F); // payload-only, CC
        self.null_cc = (self.null_cc.wrapping_add(1)) & 0x0F;
        pkt
    }
}

#[cfg(target_os = "linux")]
fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64)
}

#[cfg(not(target_os = "linux"))]
fn monotonic_now_ns() -> u64 {
    use std::time::Instant;
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_emits_input_only_no_padding() {
        let mut p = TsNullPadder::new(8_000);
        let mut out = Vec::new();
        let input = vec![0x47u8; TS_PACKET_SIZE * 3];
        p.process(&input, &mut out);
        assert_eq!(out.len(), input.len(), "first call must not pad");
    }

    #[test]
    fn padding_inflates_to_target_after_elapsed_time() {
        // 8 Mbps target. Sleep ~50 ms between two small-input calls.
        // Expected budget after 50 ms ≈ 50_000 bytes; first call emits
        // small input, second call should pad up close to budget.
        let mut p = TsNullPadder::new(8_000);
        let mut out = Vec::new();
        let small = vec![0x47u8; TS_PACKET_SIZE]; // 188 bytes
        p.process(&small, &mut out);
        assert_eq!(out.len(), 188);

        std::thread::sleep(std::time::Duration::from_millis(50));
        out.clear();
        p.process(&small, &mut out);

        // Budget at 50 ms / 8 Mbps = 50_000 bytes. We emitted 188 last
        // call + 188 this call + N × 188 of NULLs, capped at MAX_NULL_BURST.
        // Expect at least 100 NULL packets (~18800 bytes) — depending on
        // sleep precision the actual count varies but must be > 50.
        let null_packet_count = (out.len() - 188) / TS_PACKET_SIZE;
        assert!(
            null_packet_count >= 50,
            "expected ≥50 NULL packets after 50 ms at 8 Mbps, got {}",
            null_packet_count
        );
        assert!(
            null_packet_count <= MAX_NULL_BURST,
            "burst exceeded cap: {} > {}",
            null_packet_count,
            MAX_NULL_BURST,
        );
    }

    #[test]
    fn null_packets_carry_correct_pid_and_increment_cc() {
        let mut p = TsNullPadder::new(8_000);
        let mut out = Vec::new();
        // Prime epoch.
        p.process(&[], &mut out);
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Trigger padding.
        out.clear();
        p.process(&[], &mut out);
        assert!(!out.is_empty(), "10 ms of budget at 8 Mbps must emit some NULLs");
        // Inspect the first NULL packet.
        let first = &out[..TS_PACKET_SIZE];
        assert_eq!(first[0], TS_SYNC_BYTE, "sync byte");
        // PID = ((b1 & 0x1F) << 8) | b2 = 0x1FFF
        let pid = (((first[1] & 0x1F) as u16) << 8) | first[2] as u16;
        assert_eq!(pid, 0x1FFF);
        // AFC = 01 (payload-only): bits 5-4 of byte 3 = 01.
        assert_eq!(first[3] & 0x30, 0x10);
        // First NULL CC = 0; second = 1.
        if out.len() >= 2 * TS_PACKET_SIZE {
            let second = &out[TS_PACKET_SIZE..2 * TS_PACKET_SIZE];
            assert_eq!(first[3] & 0x0F, 0);
            assert_eq!(second[3] & 0x0F, 1);
        }
    }

    #[test]
    fn padder_does_not_throttle_natural_rate_above_target() {
        // Target 1 Mbps but feed 10 Mbps — padder must NOT drop input.
        let mut p = TsNullPadder::new(1_000);
        let mut out = Vec::new();
        let big = vec![0x47u8; TS_PACKET_SIZE * 100];
        p.process(&big, &mut out);
        assert_eq!(out.len(), big.len(), "input always passes through verbatim");
    }
}
