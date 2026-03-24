// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! TR-101290 MPEG Transport Stream analyzer.
//!
//! Subscribes to a flow's broadcast channel as an independent consumer and
//! performs Priority 1 and Priority 2 checks on the MPEG-TS packets carried
//! inside RTP datagrams. Because the analyzer is just another broadcast
//! subscriber, it **cannot** add jitter or block the hot path — if it falls
//! behind, it receives `Lagged(n)` and silently skips packets.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::stats::collector::{PcrState, Tr101290Accumulator};

use super::packet::RtpPacket;
use super::ts_parse::*;

// ── Constants ──────────────────────────────────────────────────────────────

/// Number of consecutive bad sync bytes before declaring sync loss.
const SYNC_LOSS_THRESHOLD: u32 = 5;
/// Number of consecutive good sync bytes to regain sync.
const SYNC_REGAIN_THRESHOLD: u32 = 5;

/// Maximum allowed PCR discontinuity in 27 MHz ticks (100 ms).
const PCR_DISCONTINUITY_THRESHOLD: u64 = 27_000_000 / 10; // 2_700_000
/// Maximum allowed PCR jitter in nanoseconds (500 ns).
const PCR_JITTER_THRESHOLD_NS: u64 = 500;

/// PAT / PMT must appear at least every 500 ms per TR-101290.
const PAT_PMT_TIMEOUT: Duration = Duration::from_millis(500);

// ── Analyzer Task ──────────────────────────────────────────────────────────

/// Spawn the TR-101290 analyzer as an independent broadcast subscriber.
pub fn spawn_tr101290_analyzer(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<Tr101290Accumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(tr101290_analyzer_loop(rx, stats, cancel))
}

async fn tr101290_analyzer_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<Tr101290Accumulator>,
    cancel: CancellationToken,
) {
    tracing::info!("TR-101290 analyzer started");

    let mut interval = tokio::time::interval(PAT_PMT_TIMEOUT);
    // The first tick fires immediately; consume it so the first real check
    // happens after 500 ms of data collection.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("TR-101290 analyzer stopping (cancelled)");
                break;
            }

            _ = interval.tick() => {
                check_pat_pmt_timeouts(&stats);
            }

            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        process_rtp_packet(&packet, &stats);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("TR-101290 analyzer lagged, skipped {n} packets");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("TR-101290 analyzer: broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }
}

/// Process a single packet: strip the RTP header (if present) and iterate
/// over the contained 188-byte MPEG-TS packets.
///
/// Handles both RTP-wrapped TS (strip RTP header first) and raw TS
/// (process bytes directly).
fn process_rtp_packet(packet: &RtpPacket, stats: &Tr101290Accumulator) {
    let payload = strip_rtp_header(packet);
    if payload.is_empty() {
        return;
    }
    let now = Instant::now();

    let mut state = stats.state.lock().unwrap();

    // ── IAT and PDV/jitter computation (RP 2129 U2, M2, M3) ──
    // Runs per-RTP-packet (not per-TS-packet) on this independent analyzer task.
    {
        let recv_us = packet.recv_time_us;
        let rtp_ts = packet.rtp_timestamp;

        // IAT: delta between consecutive packet arrival times
        if let Some(prev_recv) = state.last_recv_time_us {
            let iat = recv_us.saturating_sub(prev_recv) as f64;
            if iat < state.iat_min_us {
                state.iat_min_us = iat;
            }
            if iat > state.iat_max_us {
                state.iat_max_us = iat;
            }
            state.iat_sum_us += iat;
            state.iat_count += 1;
        }
        state.last_recv_time_us = Some(recv_us);

        // PDV/Jitter: RFC 3550 exponential moving average
        // D(i-1,i) = |(recv_i - recv_{i-1}) - (ts_i - ts_{i-1})|
        // J = J + (|D| - J) / 16
        if let (Some(prev_recv), Some(prev_ts)) =
            (state.last_recv_time_us, state.last_rtp_timestamp)
        {
            // recv delta in microseconds
            let recv_delta = recv_us as f64 - state.last_recv_time_us.unwrap_or(recv_us) as f64;
            // RTP timestamp delta in microseconds (assume 90kHz clock → 1 tick = 11.11 us)
            let ts_delta = rtp_ts.wrapping_sub(prev_ts) as f64 * (1_000_000.0 / 90_000.0);
            let d = (recv_delta - ts_delta).abs();
            state.jitter_us += (d - state.jitter_us) / 16.0;
            let _ = prev_recv; // suppress warning
        }
        state.last_rtp_timestamp = Some(rtp_ts);
    }

    // Iterate over 188-byte TS packets in the RTP payload
    let mut offset = 0;
    while offset + TS_PACKET_SIZE <= payload.len() {
        let ts_pkt = &payload[offset..offset + TS_PACKET_SIZE];
        process_ts_packet(ts_pkt, now, stats, &mut state);
        offset += TS_PACKET_SIZE;
    }
}

/// Analyze a single 188-byte TS packet for TR-101290 Priority 1 & 2 checks.
fn process_ts_packet(
    pkt: &[u8],
    now: Instant,
    stats: &Tr101290Accumulator,
    state: &mut crate::stats::collector::Tr101290State,
) {
    stats.ts_packets_analyzed.fetch_add(1, Ordering::Relaxed);

    // ── 1. Sync byte check ──
    if pkt[0] != TS_SYNC_BYTE {
        stats.sync_byte_errors.fetch_add(1, Ordering::Relaxed);
        state.sync_consecutive_good = 0;
        state.sync_consecutive_bad += 1;
        if state.in_sync && state.sync_consecutive_bad >= SYNC_LOSS_THRESHOLD {
            state.in_sync = false;
            stats.sync_loss_count.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("TR-101290: sync lost after {} consecutive bad sync bytes", state.sync_consecutive_bad);
        }
        return; // Cannot reliably parse a packet without sync
    }

    state.sync_consecutive_bad = 0;
    state.sync_consecutive_good += 1;
    if !state.in_sync && state.sync_consecutive_good >= SYNC_REGAIN_THRESHOLD {
        state.in_sync = true;
        tracing::info!("TR-101290: sync regained");
    }

    // ── 2. Transport Error Indicator ──
    if ts_tei(pkt) {
        stats.tei_errors.fetch_add(1, Ordering::Relaxed);
    }

    let pid = ts_pid(pkt);

    // ── 3. Continuity counter check ──
    // Skip null packets and adaptation-only packets (no payload)
    if pid != NULL_PID && ts_has_payload(pkt) {
        let cc = ts_cc(pkt);
        if let Some(&prev_cc) = state.cc_tracker.get(&pid) {
            let expected = (prev_cc + 1) & 0x0F;
            // Allow duplicate (same CC) per spec, but flag other mismatches
            if cc != expected && cc != prev_cc {
                stats.cc_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        state.cc_tracker.insert(pid, cc);
    }

    // ── 4. PAT handling (PID 0x0000) ──
    if pid == PAT_PID {
        state.last_pat_time = Some(now);
        state.pat_seen = true;

        if ts_pusi(pkt) {
            stats.pat_count.fetch_add(1, Ordering::Relaxed);
            let pmt_pids = parse_pat_pmt_pids(pkt);
            // Update known PMT PIDs — add new ones, keep existing timestamps
            for &pmt_pid in &pmt_pids {
                state.pmt_pids.entry(pmt_pid).or_insert(None);
            }
            // Remove PMT PIDs no longer referenced by PAT
            let pmt_set: std::collections::HashSet<u16> = pmt_pids.into_iter().collect();
            state.pmt_pids.retain(|pid, _| pmt_set.contains(pid));
        }
    }

    // ── 5. PMT handling ──
    if state.pmt_pids.contains_key(&pid) {
        state.pmt_pids.insert(pid, Some(now));
        if ts_pusi(pkt) {
            stats.pmt_count.fetch_add(1, Ordering::Relaxed);
            // VSF TR-07 detection: scan PMT for JPEG XS stream type (0x61)
            detect_jpeg_xs_in_pmt(pkt, state);
        }
    }

    // ── 6. PCR checks ──
    if let Some(pcr_value) = extract_pcr(pkt) {
        if let Some(prev) = state.pcr_tracker.get(&pid) {
            // PCR discontinuity: jump > 100ms or backwards
            let pcr_delta = if pcr_value >= prev.last_pcr_value {
                pcr_value - prev.last_pcr_value
            } else {
                // PCR went backwards
                stats
                    .pcr_discontinuity_errors
                    .fetch_add(1, Ordering::Relaxed);
                0 // Skip accuracy check for backwards PCR
            };

            if pcr_delta > 0 {
                if pcr_delta > PCR_DISCONTINUITY_THRESHOLD {
                    stats
                        .pcr_discontinuity_errors
                        .fetch_add(1, Ordering::Relaxed);
                }

                // PCR accuracy: compare PCR delta to wall-clock delta
                let wall_delta = now.duration_since(prev.last_pcr_wall_time);
                let wall_delta_27mhz =
                    wall_delta.as_secs() * 27_000_000 + wall_delta.subsec_nanos() as u64 * 27 / 1000;
                let jitter_27mhz = if pcr_delta > wall_delta_27mhz {
                    pcr_delta - wall_delta_27mhz
                } else {
                    wall_delta_27mhz - pcr_delta
                };
                // Convert jitter from 27MHz ticks to nanoseconds: ticks * 1000 / 27
                let jitter_ns = jitter_27mhz * 1000 / 27;
                if jitter_ns > PCR_JITTER_THRESHOLD_NS {
                    stats
                        .pcr_accuracy_errors
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        state.pcr_tracker.insert(
            pid,
            PcrState {
                last_pcr_value: pcr_value,
                last_pcr_wall_time: now,
            },
        );
    }
}

/// VSF TR-07 detection: scan a PMT section for JPEG XS stream type (0x61).
///
/// Per VSF TR-07:2022, JPEG XS video is carried in MPEG-2 TS with stream_type
/// 0x61. This function parses the PMT elementary stream loop to detect this
/// stream type, setting `state.jpeg_xs_detected` and `state.jpeg_xs_pid`.
fn detect_jpeg_xs_in_pmt(pkt: &[u8], state: &mut crate::stats::collector::Tr101290State) {
    if !ts_pusi(pkt) {
        return;
    }

    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return;
    }

    // pointer_field
    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;

    // PMT header: table_id(1) + flags+length(2) + program_number(2) +
    // version(1) + section_number(1) + last_section(1) + pcr_pid(2) +
    // program_info_length(2) = 12 bytes
    if offset + 12 > TS_PACKET_SIZE {
        return;
    }
    let table_id = pkt[offset];
    if table_id != 0x02 {
        return;
    }
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);

    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let stream_type = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

        // JPEG XS stream type per ISO/IEC 13818-1 and VSF TR-07
        if stream_type == 0x61 {
            if !state.jpeg_xs_detected {
                tracing::info!("VSF TR-07: JPEG XS stream detected on PID 0x{:04X}", es_pid);
            }
            state.jpeg_xs_detected = true;
            state.jpeg_xs_pid = Some(es_pid);
        }

        pos += 5 + es_info_length;
    }
}

/// Periodic check: flag PAT/PMT timeout errors if they haven't arrived
/// within the required 500 ms interval.
fn check_pat_pmt_timeouts(stats: &Tr101290Accumulator) {
    let state = stats.state.lock().unwrap();
    let now = Instant::now();

    // Only check timeouts after we've seen at least one PAT (avoid false
    // alarms during startup or for non-TS streams).
    if !state.pat_seen {
        return;
    }

    // PAT timeout
    if let Some(last) = state.last_pat_time {
        if now.duration_since(last) > PAT_PMT_TIMEOUT {
            stats.pat_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    // PMT timeouts
    for (_, last_time) in &state.pmt_pids {
        match last_time {
            Some(t) if now.duration_since(*t) > PAT_PMT_TIMEOUT => {
                stats.pmt_errors.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                // PMT PID discovered in PAT but never seen yet — count as error
                // only if we've been running long enough (PAT was already seen)
                stats.pmt_errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 188-byte TS packet.
    fn make_ts_packet(pid: u16, cc: u8, payload_flag: bool) -> Vec<u8> {
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((pid >> 8) & 0x1F) as u8;
        pkt[2] = (pid & 0xFF) as u8;
        let afc = if payload_flag { 0x01 } else { 0x00 };
        pkt[3] = (afc << 4) | (cc & 0x0F);
        pkt
    }

    /// Build a TS packet with TEI bit set.
    fn make_ts_packet_tei(pid: u16, cc: u8) -> Vec<u8> {
        let mut pkt = make_ts_packet(pid, cc, true);
        pkt[1] |= 0x80; // TEI bit
        pkt
    }

    /// Build a TS packet with bad sync byte.
    fn make_ts_packet_bad_sync(pid: u16, cc: u8) -> Vec<u8> {
        let mut pkt = make_ts_packet(pid, cc, true);
        pkt[0] = 0xFF; // Bad sync
        pkt
    }

    #[test]
    fn test_ts_parsing() {
        let pkt = make_ts_packet(0x0100, 5, true);
        assert_eq!(ts_pid(&pkt), 0x0100);
        assert_eq!(ts_cc(&pkt), 5);
        assert!(ts_has_payload(&pkt));
        assert!(!ts_tei(&pkt));
        assert!(!ts_pusi(&pkt));
    }

    #[test]
    fn test_tei_detection() {
        let pkt = make_ts_packet_tei(0x0100, 0);
        assert!(ts_tei(&pkt));
    }

    #[test]
    fn test_cc_error_detection() {
        let stats = Arc::new(Tr101290Accumulator::new());
        let now = Instant::now();
        let mut state = stats.state.lock().unwrap();

        // First packet: CC=0 — no error (initializes tracker)
        let pkt0 = make_ts_packet(0x100, 0, true);
        process_ts_packet(&pkt0, now, &stats, &mut state);
        assert_eq!(stats.cc_errors.load(Ordering::Relaxed), 0);

        // Second packet: CC=1 — correct, no error
        let pkt1 = make_ts_packet(0x100, 1, true);
        process_ts_packet(&pkt1, now, &stats, &mut state);
        assert_eq!(stats.cc_errors.load(Ordering::Relaxed), 0);

        // Third packet: CC=5 — discontinuity!
        let pkt5 = make_ts_packet(0x100, 5, true);
        process_ts_packet(&pkt5, now, &stats, &mut state);
        assert_eq!(stats.cc_errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_cc_wraparound() {
        let stats = Arc::new(Tr101290Accumulator::new());
        let now = Instant::now();
        let mut state = stats.state.lock().unwrap();

        // CC=14
        let pkt = make_ts_packet(0x100, 14, true);
        process_ts_packet(&pkt, now, &stats, &mut state);

        // CC=15
        let pkt = make_ts_packet(0x100, 15, true);
        process_ts_packet(&pkt, now, &stats, &mut state);

        // CC=0 — correct wraparound
        let pkt = make_ts_packet(0x100, 0, true);
        process_ts_packet(&pkt, now, &stats, &mut state);

        assert_eq!(stats.cc_errors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_duplicate_cc_allowed() {
        let stats = Arc::new(Tr101290Accumulator::new());
        let now = Instant::now();
        let mut state = stats.state.lock().unwrap();

        let pkt = make_ts_packet(0x100, 3, true);
        process_ts_packet(&pkt, now, &stats, &mut state);

        // Duplicate CC=3 is allowed per spec
        let pkt = make_ts_packet(0x100, 3, true);
        process_ts_packet(&pkt, now, &stats, &mut state);

        assert_eq!(stats.cc_errors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_null_pid_cc_not_checked() {
        let stats = Arc::new(Tr101290Accumulator::new());
        let now = Instant::now();
        let mut state = stats.state.lock().unwrap();

        // Null PID packets should not trigger CC errors regardless of CC value
        let pkt = make_ts_packet(NULL_PID, 0, true);
        process_ts_packet(&pkt, now, &stats, &mut state);
        let pkt = make_ts_packet(NULL_PID, 7, true);
        process_ts_packet(&pkt, now, &stats, &mut state);

        assert_eq!(stats.cc_errors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_sync_byte_error() {
        let stats = Arc::new(Tr101290Accumulator::new());
        let now = Instant::now();
        let mut state = stats.state.lock().unwrap();

        let pkt = make_ts_packet_bad_sync(0x100, 0);
        process_ts_packet(&pkt, now, &stats, &mut state);

        assert_eq!(stats.sync_byte_errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_sync_loss_after_threshold() {
        let stats = Arc::new(Tr101290Accumulator::new());
        let now = Instant::now();
        let mut state = stats.state.lock().unwrap();

        // Send SYNC_LOSS_THRESHOLD bad packets
        for _ in 0..SYNC_LOSS_THRESHOLD {
            let pkt = make_ts_packet_bad_sync(0x100, 0);
            process_ts_packet(&pkt, now, &stats, &mut state);
        }

        assert_eq!(stats.sync_loss_count.load(Ordering::Relaxed), 1);
        assert!(!state.in_sync);
    }

    #[test]
    fn test_tei_error_count() {
        let stats = Arc::new(Tr101290Accumulator::new());
        let now = Instant::now();
        let mut state = stats.state.lock().unwrap();

        let pkt = make_ts_packet_tei(0x100, 0);
        process_ts_packet(&pkt, now, &stats, &mut state);

        assert_eq!(stats.tei_errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_snapshot_priority_flags() {
        let stats = Tr101290Accumulator::new();

        // No errors → both priorities OK
        let snap = stats.snapshot();
        assert!(snap.priority1_ok);
        assert!(snap.priority2_ok);

        // CC error → priority 1 not OK
        stats.cc_errors.fetch_add(1, Ordering::Relaxed);
        let snap = stats.snapshot();
        assert!(!snap.priority1_ok);
        assert!(snap.priority2_ok);

        // TEI error → priority 2 not OK
        stats.tei_errors.fetch_add(1, Ordering::Relaxed);
        let snap = stats.snapshot();
        assert!(!snap.priority1_ok);
        assert!(!snap.priority2_ok);
    }
}
