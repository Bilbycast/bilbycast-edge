// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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

        // Read previous values BEFORE updating state
        let prev_recv = state.last_recv_time_us;
        let prev_ts = state.last_rtp_timestamp;

        // IAT: delta between consecutive packet arrival times
        if let Some(prev_recv_us) = prev_recv {
            let iat = recv_us.saturating_sub(prev_recv_us) as f64;
            if iat < state.iat_min_us {
                state.iat_min_us = iat;
            }
            if iat > state.iat_max_us {
                state.iat_max_us = iat;
            }
            state.iat_sum_us += iat;
            state.iat_count += 1;
        }

        // PDV/Jitter: RFC 3550 exponential moving average
        // D(i-1,i) = |(recv_i - recv_{i-1}) - (ts_i - ts_{i-1})|
        // J = J + (|D| - J) / 16
        //
        // Bug #3 fix: rtp_ts is a u32 and `wrapping_sub` returns the
        // unsigned distance — for backwards jumps (e.g. TS reset / RTSP
        // re-sync) that distance is ~u32::MAX rather than a small
        // negative number, producing 16 BILLION µs of fake jitter. We
        // cast to i32 to interpret the wraparound as a signed delta,
        // then drop the sample entirely if the resulting tick delta is
        // implausibly large (more than 10 seconds at 90 kHz). Real
        // jitter on a healthy stream is in the milliseconds.
        if let (Some(prev_recv_us), Some(prev_rtp_ts)) = (prev_recv, prev_ts) {
            let recv_delta = recv_us as f64 - prev_recv_us as f64;
            let ts_delta_ticks_signed = rtp_ts.wrapping_sub(prev_rtp_ts) as i32;
            // Sanity threshold: 10 seconds of 90 kHz ticks. Anything
            // larger is a stream reset or a malformed packet, not real
            // jitter — skip the EWMA update.
            const MAX_PLAUSIBLE_TICK_DELTA: i32 = 90_000 * 10;
            if ts_delta_ticks_signed.abs() <= MAX_PLAUSIBLE_TICK_DELTA {
                let ts_delta = ts_delta_ticks_signed as f64 * (1_000_000.0 / 90_000.0);
                let d = (recv_delta - ts_delta).abs();
                // Clamp single-sample contribution to 1 second so a
                // single bad packet can't poison the EWMA average.
                let d_clamped = d.min(1_000_000.0);
                state.jitter_us += (d_clamped - state.jitter_us) / 16.0;
            }
        }

        // Update state AFTER reading previous values
        state.last_recv_time_us = Some(recv_us);
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
        stats.window_tei_errors.fetch_add(1, Ordering::Relaxed);
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
                stats.window_cc_errors.fetch_add(1, Ordering::Relaxed);
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

            // CRC-32 verification (Priority 2)
            if let Some(section_start) = psi_section_start(pkt) {
                if !verify_psi_crc(pkt, section_start) {
                    stats.crc_errors.fetch_add(1, Ordering::Relaxed);
                    stats.window_crc_errors.fetch_add(1, Ordering::Relaxed);
                }
            }

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

            // CRC-32 verification (Priority 2)
            if let Some(section_start) = psi_section_start(pkt) {
                if !verify_psi_crc(pkt, section_start) {
                    stats.crc_errors.fetch_add(1, Ordering::Relaxed);
                    stats.window_crc_errors.fetch_add(1, Ordering::Relaxed);
                }
            }

            // Extract ES PIDs from PMT for PID error tracking (Priority 1)
            extract_es_pids_from_pmt(pkt, state);
            // VSF TR-07 detection: scan PMT for JPEG XS stream type (0x61)
            detect_jpeg_xs_in_pmt(pkt, state);
        }
    }

    // ── 5b. Track ES PID presence for PID error check (Priority 1) ──
    if state.es_pids.contains_key(&pid) {
        state.es_pids.insert(pid, Some(now));
    }

    // ── 6. PCR checks ──
    if let Some(pcr_value) = extract_pcr(pkt) {
        // When discontinuity_indicator is set, the PCR jump is expected
        // (e.g., stream switch) — skip discontinuity and accuracy checks
        // but still update the tracker for subsequent packets.
        let discontinuity_expected = ts_discontinuity_indicator(pkt);

        if let Some(prev) = state.pcr_tracker.get(&pid) {
            if !discontinuity_expected {
                // PCR discontinuity: jump > 100ms or backwards
                let pcr_delta = if pcr_value >= prev.last_pcr_value {
                    pcr_value - prev.last_pcr_value
                } else {
                    // PCR went backwards
                    stats
                        .pcr_discontinuity_errors
                        .fetch_add(1, Ordering::Relaxed);
                    stats
                        .window_pcr_discontinuity_errors
                        .fetch_add(1, Ordering::Relaxed);
                    0 // Skip accuracy check for backwards PCR
                };

                if pcr_delta > 0 {
                    if pcr_delta > PCR_DISCONTINUITY_THRESHOLD {
                        stats
                            .pcr_discontinuity_errors
                            .fetch_add(1, Ordering::Relaxed);
                        stats
                            .window_pcr_discontinuity_errors
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
                        stats
                            .window_pcr_accuracy_errors
                            .fetch_add(1, Ordering::Relaxed);
                    }
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

/// Get the section start offset within a TS packet containing a PSI section
/// with PUSI set. Returns the offset of the table_id byte.
fn psi_section_start(pkt: &[u8]) -> Option<usize> {
    if !ts_pusi(pkt) {
        return None;
    }
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }
    let pointer = pkt[offset] as usize;
    let section_start = offset + 1 + pointer;
    if section_start >= TS_PACKET_SIZE {
        return None;
    }
    Some(section_start)
}

/// Extract elementary stream PIDs from a PMT section and register them
/// for PID error tracking (TR-101290 Priority 1).
fn extract_es_pids_from_pmt(pkt: &[u8], state: &mut crate::stats::collector::Tr101290State) {
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

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;

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

    let mut new_es_pids = std::collections::HashSet::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        new_es_pids.insert(es_pid);
        // Register ES PID if not already tracked
        state.es_pids.entry(es_pid).or_insert(None);
        pos += 5 + es_info_length;
    }
    // Remove ES PIDs no longer referenced by this PMT
    state.es_pids.retain(|pid, _| new_es_pids.contains(pid));
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
            stats.window_pat_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    // PMT timeouts
    for (_, last_time) in &state.pmt_pids {
        match last_time {
            Some(t) if now.duration_since(*t) > PAT_PMT_TIMEOUT => {
                stats.pmt_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_pmt_errors.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                // PMT PID discovered in PAT but never seen yet — count as error
                // only if we've been running long enough (PAT was already seen)
                stats.pmt_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_pmt_errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    // PID error check (Priority 1): ES PIDs referenced in PMT must appear
    for (_, last_time) in &state.es_pids {
        match last_time {
            Some(t) if now.duration_since(*t) > PAT_PMT_TIMEOUT => {
                stats.pid_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_pid_errors.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                // ES PID discovered in PMT but never seen yet
                stats.pid_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_pid_errors.fetch_add(1, Ordering::Relaxed);
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

        // CC error → priority 1 not OK (must increment both cumulative and windowed)
        stats.cc_errors.fetch_add(1, Ordering::Relaxed);
        stats.window_cc_errors.fetch_add(1, Ordering::Relaxed);
        let snap = stats.snapshot();
        assert!(!snap.priority1_ok, "P1 should fail with CC error in window");
        assert!(snap.priority2_ok);

        // After snapshot, windowed CC was reset. Add TEI + CC again for combined test.
        stats.cc_errors.fetch_add(1, Ordering::Relaxed);
        stats.window_cc_errors.fetch_add(1, Ordering::Relaxed);
        stats.tei_errors.fetch_add(1, Ordering::Relaxed);
        stats.window_tei_errors.fetch_add(1, Ordering::Relaxed);
        let snap = stats.snapshot();
        assert!(!snap.priority1_ok, "P1 should fail with CC error in window");
        assert!(!snap.priority2_ok, "P2 should fail with TEI error in window");

        // After snapshot, windowed counters are reset → priorities should be OK again
        let snap = stats.snapshot();
        assert!(snap.priority1_ok, "P1 should be OK after window reset");
        assert!(snap.priority2_ok, "P2 should be OK after window reset");
        // But cumulative counters remain
        assert_eq!(snap.cc_errors, 2);
        assert_eq!(snap.tei_errors, 1);
    }

    #[test]
    fn test_mpeg2_crc32_known_value() {
        // MPEG-2 CRC-32 of an empty payload with initial 0xFFFFFFFF should produce a known value
        assert_eq!(mpeg2_crc32(&[]), 0xFFFFFFFF);
        // A valid PAT/PMT section (including its CRC) should produce 0
        // Test with a minimal PAT: table_id=0, section_length includes CRC
        // We verify the algorithm works by checking round-trip
        let data = [0x00, 0x01, 0x02, 0x03];
        let crc = mpeg2_crc32(&data);
        assert_ne!(crc, 0); // Non-trivial data should not produce 0
        // Append CRC bytes (big-endian) and verify the result is 0
        let mut with_crc = data.to_vec();
        with_crc.push((crc >> 24) as u8);
        with_crc.push((crc >> 16) as u8);
        with_crc.push((crc >> 8) as u8);
        with_crc.push(crc as u8);
        assert_eq!(mpeg2_crc32(&with_crc), 0, "CRC should be 0 when CRC bytes are appended");
    }

    #[test]
    fn test_discontinuity_indicator() {
        // Packet with adaptation field and discontinuity_indicator set
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x01; // PID high byte
        pkt[2] = 0x00; // PID low byte
        pkt[3] = 0x30; // adaptation_field_control = 0x11 (AF + payload)
        pkt[4] = 0x07; // adaptation field length
        pkt[5] = 0x80; // discontinuity_indicator = 1
        assert!(ts_discontinuity_indicator(&pkt));

        // Same packet without discontinuity flag
        pkt[5] = 0x00;
        assert!(!ts_discontinuity_indicator(&pkt));

        // Packet without adaptation field
        pkt[3] = 0x10; // payload only
        assert!(!ts_discontinuity_indicator(&pkt));
    }

    /// Bug #3 regression: a u32 RTP timestamp wraparound used to be
    /// computed via `wrapping_sub` and cast to `f64`, producing a
    /// 4-billion-tick "delta" that turned into a 16-billion-microsecond
    /// jitter sample. The fix interprets the wraparound as a signed
    /// delta and clamps any single sample to ≤1 second.
    #[test]
    fn jitter_handles_rtp_timestamp_wraparound_without_blowing_up() {
        // Simulate the math from analyze_rtp_packet directly so we can
        // verify the fix in isolation.
        fn jitter_step_fixed(
            prev_recv_us: u64,
            recv_us: u64,
            prev_rtp_ts: u32,
            rtp_ts: u32,
        ) -> Option<f64> {
            let recv_delta = recv_us as f64 - prev_recv_us as f64;
            let ts_delta_ticks_signed = rtp_ts.wrapping_sub(prev_rtp_ts) as i32;
            const MAX_PLAUSIBLE_TICK_DELTA: i32 = 90_000 * 10;
            if ts_delta_ticks_signed.abs() > MAX_PLAUSIBLE_TICK_DELTA {
                return None;
            }
            let ts_delta = ts_delta_ticks_signed as f64 * (1_000_000.0 / 90_000.0);
            let d = (recv_delta - ts_delta).abs();
            Some(d.min(1_000_000.0))
        }

        // The OLD buggy code, for comparison:
        fn jitter_step_old(
            prev_recv_us: u64,
            recv_us: u64,
            prev_rtp_ts: u32,
            rtp_ts: u32,
        ) -> f64 {
            let recv_delta = recv_us as f64 - prev_recv_us as f64;
            let ts_delta = rtp_ts.wrapping_sub(prev_rtp_ts) as f64
                * (1_000_000.0 / 90_000.0);
            (recv_delta - ts_delta).abs()
        }

        // Normal step: ~33 ms apart, ~3000 ticks (33ms × 90 = 2970).
        let d_normal = jitter_step_fixed(1_000_000, 1_033_000, 100_000, 102_970).unwrap();
        assert!(d_normal < 1000.0, "normal jitter sample too high: {d_normal}");

        // Forward wraparound: prev=u32::MAX-100, new=200 (+300 ticks =
        // 3.3 ms of expected ts_delta). recv_delta = 33.3 ms wallclock,
        // so |33.3-3.3| = 30 ms of measured jitter. Both old and new
        // code give the same result for forward wraps because u32
        // wrapping_sub already produces the right answer there.
        let d_fwd_wrap = jitter_step_fixed(
            2_000_000, 2_033_300, u32::MAX - 100, 200
        ).unwrap();
        assert!(
            d_fwd_wrap < 100_000.0,
            "forward wrap should give sane jitter, got {d_fwd_wrap}"
        );

        // The actual Bug #3 trigger: prev=200, new=u32::MAX-100.
        // wrapping_sub = u32::MAX - 300 ≈ 4.29 BILLION ticks (forward
        // jump) per the OLD code, which produces ~47 BILLION µs of fake
        // jitter. The NEW code interprets the wraparound as i32 = -301
        // ticks (small backward step), giving a sane jitter sample.
        let d_bad = jitter_step_old(3_000_000, 3_033_000, 200, u32::MAX - 100);
        assert!(
            d_bad > 1e9,
            "OLD code should produce huge fake jitter, got {d_bad}"
        );
        let d_fixed = jitter_step_fixed(3_000_000, 3_033_000, 200, u32::MAX - 100).unwrap();
        assert!(
            d_fixed < 100_000.0,
            "FIXED code should produce sane jitter, got {d_fixed}"
        );

        // A genuinely huge backward step (RTSP source reset 11 seconds
        // backwards, ts drops by 11×90_000 = 990_000): the signed
        // delta is -990_000, which is more than the 10-second threshold,
        // so the sample is dropped.
        let result_huge_back = jitter_step_fixed(
            4_000_000, 4_033_000, 1_000_000, 10_000
        );
        assert!(
            result_huge_back.is_none(),
            "expected >10s backward step to be dropped, got {result_huge_back:?}"
        );

        // A small reorder (90 ticks = 1 ms backward): accepted, ~34 ms
        // jitter sample.
        let d_reorder = jitter_step_fixed(5_000_000, 5_033_000, 5000, 4910).unwrap();
        assert!(
            d_reorder > 30_000.0 && d_reorder < 40_000.0,
            "expected ~34 ms jitter, got {d_reorder}"
        );

        // Sanity: the worst-case acceptable sample is clamped to 1 s.
        let d_clamped = jitter_step_fixed(0, 5_000_000, 0, 0).unwrap();
        assert!(d_clamped <= 1_000_000.0);
    }
}
