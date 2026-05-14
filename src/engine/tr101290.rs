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
/// PCR_AC residual threshold in nanoseconds. The TR-101290 §5.2.2 PCR_AC
/// spec is ±500 ns — but that's the encoder's deviation against an
/// idealised 27 MHz reference clock, measured at the encoder bench. A
/// downstream receiver doesn't have an idealised reference: it has the
/// host wall clock plus arbitrary network / SRT-jitter-buffer / scheduler
/// jitter on packet arrival.
///
/// We fit a least-squares line through a sliding window of (PCR, wall)
/// samples and measure each new sample's residual against it. Slope
/// absorbs encoder↔host rate skew, intercept absorbs mean network delay.
/// What's *not* absorbed is per-sample wall-clock jitter — and that's
/// directly bounded by the upstream jitter (10s of ms on a real SRT
/// path with TSBPD release patterns). Setting the threshold below that
/// floor flags every PCR even on a clean stream.
///
/// 100 ms catches genuine encoder/PCR-source failures (which produce
/// residuals ≥ hundreds of ms or PCR leaps) without firing on normal
/// receive-side jitter. For tightly-controlled local paths a follow-up
/// could derive an adaptive threshold from the EWMA wall-jitter band
/// rather than this fixed value.
const PCR_JITTER_THRESHOLD_NS: u64 = 100_000_000;
/// Sliding-window length for the PCR regression (per PCR-bearing PID).
/// At a typical PCR cadence of ~25 Hz, 16 samples is ~640 ms — enough to
/// average out short bursts without lagging real rate drift.
pub(crate) const PCR_HISTORY_LEN: usize = 16;
/// Minimum samples required before the regression residual is meaningful.
/// Below this the accuracy check is skipped.
const PCR_HISTORY_MIN: usize = 4;

/// PAT / PMT must appear at least every 500 ms per TR-101290.
const PAT_PMT_TIMEOUT: Duration = Duration::from_millis(500);

// ── TR 101 290 P2-extended / P3 fixed PIDs (per ETSI EN 300 468) ──
const CAT_PID: u16 = 0x0001;
const NIT_PID_DEFAULT: u16 = 0x0010; // de-facto NIT PID; MIB lookup happens via PAT prog 0
const SDT_BAT_PID: u16 = 0x0011;
const EIT_PID: u16 = 0x0012;
const RST_PID: u16 = 0x0013;
const TDT_TOT_PID: u16 = 0x0014;

/// PCR-repetition timeout per TR 101 290 §5.2.2 (PCR_repetition_error):
/// PCR must arrive at least every 100 ms on a PCR-bearing PID.
const PCR_REPETITION_TIMEOUT: Duration = Duration::from_millis(100);

/// PTS-repetition timeout per TR 101 290 §5.2.1 (PTS_error): every PES PID
/// carrying a PTS field must produce one within 700 ms (the spec's safety
/// margin for low-frame-rate streams).
const PTS_TIMEOUT: Duration = Duration::from_millis(700);

/// CAT timeout (TR 101 290 §5.2.4 — CAT_error). Once a CAT has been seen
/// and CA descriptors have been advertised, the CAT must repeat within
/// 500 ms — same window as PAT/PMT.
const CAT_TIMEOUT: Duration = Duration::from_millis(500);

/// SI-table timeouts. The TR 101 290 P3 thresholds use repetition rates
/// per ETSI TR 101 211. For our purposes a single timeout = signal lost
/// + emit one error per window suffices; the manager UI's per-counter
/// rendering already shows trends.
const SDT_TIMEOUT: Duration = Duration::from_millis(2_000);
const NIT_TIMEOUT: Duration = Duration::from_millis(10_000);
const EIT_TIMEOUT: Duration = Duration::from_millis(2_000);
const TDT_TIMEOUT: Duration = Duration::from_millis(30_000);
const RST_TIMEOUT: Duration = Duration::from_millis(60_000);

// ── Analyzer Task ──────────────────────────────────────────────────────────

/// Spawn the TR-101290 analyzer as an independent broadcast subscriber.
///
/// Optionally receives the flow's `active_input_rx` watch — when present,
/// the analyzer resets its IAT/PDV running stats on every active-input
/// change so the inter-input silence gap doesn't poison the min/max/EWMA.
/// (Per-PID PSI/PCR/PTS trackers are managed by `extract_es_pids_from_pmt`
/// via PMT-driven `.retain` calls, so they don't need a separate reset.)
pub fn spawn_tr101290_analyzer(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<Tr101290Accumulator>,
    cancel: CancellationToken,
    active_input_rx: Option<tokio::sync::watch::Receiver<String>>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(tr101290_analyzer_loop(rx, stats, cancel, active_input_rx))
}

async fn tr101290_analyzer_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<Tr101290Accumulator>,
    cancel: CancellationToken,
    active_input_rx: Option<tokio::sync::watch::Receiver<String>>,
) {
    tracing::info!("TR-101290 analyzer started");

    let mut interval = tokio::time::interval(PAT_PMT_TIMEOUT);
    // The first tick fires immediately; consume it so the first real check
    // happens after 500 ms of data collection.
    interval.tick().await;

    // The watch is held in an Option but `tokio::select!` needs a concrete
    // future on every arm. Box-ed dyn future indirection keeps the loop
    // tidy and the no-watch case (non-TS or test paths) free of overhead.
    let mut maybe_rx = active_input_rx;
    if let Some(ref mut r) = maybe_rx {
        // Mark current value as seen so the first changed() doesn't fire
        // on startup.
        r.mark_changed();
        let _ = r.borrow_and_update();
    }

    loop {
        let switch_fut = async {
            match maybe_rx {
                Some(ref mut r) => match r.changed().await {
                    Ok(()) => Some(r.borrow_and_update().clone()),
                    Err(_) => std::future::pending::<Option<String>>().await,
                },
                None => std::future::pending::<Option<String>>().await,
            }
        };

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("TR-101290 analyzer stopping (cancelled)");
                break;
            }

            _ = interval.tick() => {
                check_pat_pmt_timeouts(&stats);
            }

            new_id = switch_fut => {
                if let Some(id) = new_id {
                    let mut state = stats.state.lock().unwrap();
                    state.last_recv_time_us = None;
                    state.last_rtp_timestamp = None;
                    state.iat_min_us = f64::MAX;
                    state.iat_max_us = 0.0;
                    state.iat_sum_us = 0.0;
                    state.iat_count = 0;
                    state.jitter_us = 0.0;
                    drop(state);
                    tracing::info!(
                        "TR-101290 analyzer: reset IAT/PDV running stats for new active input '{id}'"
                    );
                }
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
        if state.first_pat_time.is_none() {
            state.first_pat_time = Some(now);
        }

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
            state.pmt_errored.retain(|pid| pmt_set.contains(pid));
        }
    }

    // ── 5. PMT handling ──
    if state.pmt_pids.contains_key(&pid) {
        state.pmt_pids.insert(pid, Some(now));
        // PMT PID was just seen on the wire — clear any stale error latch.
        state.pmt_errored.remove(&pid);
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
        state.es_errored.remove(&pid);
    }

    // ── 6. PCR checks ──
    if let Some(pcr_value) = extract_pcr(pkt) {
        // When discontinuity_indicator is set, the PCR jump is expected
        // (e.g., stream switch) — skip discontinuity and accuracy checks
        // but still reset the tracker for subsequent packets.
        let discontinuity_expected = ts_discontinuity_indicator(pkt);

        if discontinuity_expected {
            // Restart the regression window; the rate before the
            // discontinuity is meaningless once the source clock leaps.
            state.pcr_tracker.remove(&pid);
        } else if let Some(prev) = state.pcr_tracker.get(&pid) {
            // ── Discontinuity check (jump > 100 ms or backwards) ──
            if pcr_value >= prev.last_pcr_value {
                let pcr_delta = pcr_value - prev.last_pcr_value;
                if pcr_delta > PCR_DISCONTINUITY_THRESHOLD {
                    stats
                        .pcr_discontinuity_errors
                        .fetch_add(1, Ordering::Relaxed);
                    stats
                        .window_pcr_discontinuity_errors
                        .fetch_add(1, Ordering::Relaxed);
                }
            } else {
                stats
                    .pcr_discontinuity_errors
                    .fetch_add(1, Ordering::Relaxed);
                stats
                    .window_pcr_discontinuity_errors
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        // ── Accuracy check via least-squares regression on a sliding
        //    window of (pcr_27mhz, wall_us_since_anchor) samples ──
        //
        // Fit pcr ≈ a · wall + b across the window, then measure the
        // residual of the new sample against the line. The slope `a`
        // absorbs encoder↔host rate skew, the intercept `b` absorbs the
        // mean network delay; what's left is the per-sample deviation —
        // i.e. PCR_AC, modulo the wall-clock's own ~µs jitter.
        let entry = state.pcr_tracker.entry(pid).or_insert_with(|| PcrState {
            last_pcr_value: pcr_value,
            last_pcr_wall_time: now,
            history: std::collections::VecDeque::with_capacity(PCR_HISTORY_LEN),
            history_anchor: now,
        });

        let wall_us = now.duration_since(entry.history_anchor).as_micros() as u64;
        if entry.history.len() >= PCR_HISTORY_MIN && !discontinuity_expected {
            if let Some(residual_ns) = pcr_residual_ns(&entry.history, pcr_value, wall_us) {
                if residual_ns > PCR_JITTER_THRESHOLD_NS {
                    stats
                        .pcr_accuracy_errors
                        .fetch_add(1, Ordering::Relaxed);
                    stats
                        .window_pcr_accuracy_errors
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Append the new sample, bounded.
        if entry.history.len() == PCR_HISTORY_LEN {
            entry.history.pop_front();
        }
        entry.history.push_back((pcr_value, wall_us));
        entry.last_pcr_value = pcr_value;
        entry.last_pcr_wall_time = now;

        // PCR repetition (P2 — split from `pcr_discontinuity_errors`).
        // Repetition counts the *absence* of a PCR within 100 ms, while
        // discontinuity counts unexpected jumps when one is present.
        // Both used to live under `pcr_discontinuity_errors`; they're now
        // distinct fields so the manager UI can highlight a stalled PCR
        // source separately from a PCR jump.
        state.pcr_repetition_tracker.insert(pid, now);
        state.pcr_repetition_errored.remove(&pid);
    }

    // ── 7. PSI / SI table tracking (P2-extended + P3) ──
    match pid {
        CAT_PID => {
            state.cat_seen = true;
            state.last_cat_time = Some(now);
            if ts_pusi(pkt) {
                if let Some(section_start) = psi_section_start(pkt) {
                    if !verify_psi_crc(pkt, section_start) {
                        stats.crc_errors.fetch_add(1, Ordering::Relaxed);
                        stats.window_crc_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        SDT_BAT_PID => {
            state.sdt_seen = true;
            state.last_sdt_time = Some(now);
        }
        EIT_PID => {
            state.eit_seen = true;
            state.last_eit_time = Some(now);
        }
        TDT_TOT_PID => {
            state.tdt_seen = true;
            state.last_tdt_time = Some(now);
        }
        RST_PID => {
            state.rst_seen = true;
            state.last_rst_time = Some(now);
        }
        NIT_PID_DEFAULT => {
            // Best-effort — the canonical NIT PID is announced in PAT
            // program 0 but the de-facto-default 0x0010 catches every
            // EBU-DVB stream we see. Treat it as a cue without false
            // positives.
            state.nit_seen = true;
            state.last_nit_time = Some(now);
        }
        _ => {}
    }

    // ── 8. Unreferenced PID tracking (P3 §5.3.4) ──
    // A PID is unreferenced if it carries traffic but does not appear in
    // PAT, any PMT, the CAT, NIT/SDT/EIT/TDT/RST slots, or the reserved
    // 0x1FFF null. Stamp the first observation; the snapshot path counts
    // distinct unreferenced PIDs as the error rate.
    if pid != NULL_PID
        && pid != PAT_PID
        && pid != CAT_PID
        && pid != NIT_PID_DEFAULT
        && pid != SDT_BAT_PID
        && pid != EIT_PID
        && pid != RST_PID
        && pid != TDT_TOT_PID
        && !state.pmt_pids.contains_key(&pid)
        && !state.es_pids.contains_key(&pid)
        && !state.pmt_referenced_data_pids.contains(&pid)
        && !state.unreferenced_pids.contains_key(&pid)
    {
        state.unreferenced_pids.insert(pid, ());
        stats.unreferenced_pid_errors.fetch_add(1, Ordering::Relaxed);
        stats
            .window_unreferenced_pid_errors
            .fetch_add(1, Ordering::Relaxed);
    }

    // ── 9. PTS tracking (P2 §5.2.1) ──
    // Cheap PTS extraction: PUSI-marked packets on ES PIDs may carry a
    // PES header with a PTS field. We don't reassemble full PES — we
    // only need the wall-clock of the most recent PTS to detect a
    // 700 ms gap. The PES PTS_DTS_flags live at byte 7 of the PES
    // header; bit 7 (0x80) signals "PTS present".
    if ts_pusi(pkt) && state.es_pids.contains_key(&pid) && ts_has_payload(pkt) {
        if let Some(pts_value) = crate::engine::ts_parse::extract_pes_pts(pkt) {
            state.pts_tracker.insert(pid, (pts_value, now));
            state.pts_errored.remove(&pid);
        }
    }
}

/// Compute |residual_ns| of a new (pcr_27mhz, wall_us) sample against the
/// least-squares regression fit through `history`. Returns `None` if the
/// regression is degenerate (all wall samples identical, vanishing slope
/// variance, or implausible result — e.g. a stream-source change the
/// regression didn't learn yet).
///
/// The math: fit pcr_i ≈ a · wall_i + b across the window using
/// closed-form OLS, then residual = new_pcr - (a · new_wall + b). All
/// arithmetic is f64 — the window is small (≤ 16 samples) so precision
/// is not a concern.
fn pcr_residual_ns(
    history: &std::collections::VecDeque<(u64, u64)>,
    new_pcr: u64,
    new_wall_us: u64,
) -> Option<u64> {
    let n = history.len() as f64;
    if n < 2.0 {
        return None;
    }
    let mut sum_x = 0.0_f64;
    let mut sum_y = 0.0_f64;
    let mut sum_xx = 0.0_f64;
    let mut sum_xy = 0.0_f64;
    for &(pcr, wall) in history.iter() {
        let x = wall as f64;
        let y = pcr as f64;
        sum_x += x;
        sum_y += y;
        sum_xx += x * x;
        sum_xy += x * y;
    }
    let denom = n * sum_xx - sum_x * sum_x;
    if denom.abs() < 1e-6 {
        // All wall samples coincide — can't infer a rate.
        return None;
    }
    let a = (n * sum_xy - sum_x * sum_y) / denom;
    let b = (sum_y - a * sum_x) / n;
    let predicted = a * (new_wall_us as f64) + b;
    let residual_ticks = (new_pcr as f64) - predicted;
    // Convert from 27 MHz ticks to nanoseconds: 1 tick = 1000/27 ns.
    let residual_ns = (residual_ticks.abs() * 1000.0 / 27.0).round();
    // Implausibly large residuals (> 1 s) usually mean the regression
    // hasn't caught up with a stream-source change — treat as
    // unmeasurable rather than tagging an error.
    if residual_ns > 1_000_000_000.0 {
        return None;
    }
    Some(residual_ns as u64)
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

/// MPEG-TS stream_type values that carry **data only** (no A/V essence) and
/// are emitted on broadcaster-defined intervals far longer than TR-101290's
/// A/V-oriented timeouts (PAT_PMT_TIMEOUT = 500 ms, PTS_TIMEOUT = 700 ms).
/// Tracking them as ES PIDs produces a steady stream of false-positive
/// `pid_errors` / `pts_errors` on real broadcast captures (especially
/// ISDB-T / ARIB streams with their DSM-CC carousels and private-section
/// caption tables, and DVB streams with MHP carousels and SI tables in the
/// PMT). TR 101 290 is fundamentally a service-quality spec for the A/V
/// path; data PIDs are out of scope for QoS monitoring.
///
/// Skipped:
/// - `0x05` ISO/IEC 13818-1 private_sections
/// - `0x0B` ISO/IEC 13818-6 DSM-CC U-N messages (object carousel)
/// - `0x0C` ISO/IEC 13818-6 DSM-CC stream descriptors
/// - `0x0D` ISO/IEC 13818-6 DSM-CC sections (any type)
/// - `0x14` ISO/IEC 13818-6 synchronized download protocol
fn is_data_only_stream_type(stream_type: u8) -> bool {
    matches!(stream_type, 0x05 | 0x0B | 0x0C | 0x0D | 0x14)
}

/// Extract elementary stream PIDs from a PMT section and register them
/// for PID error tracking (TR-101290 Priority 1). Data-only stream types
/// (DSM-CC carousels, private sections) are filtered out — see
/// [`is_data_only_stream_type`] for the rationale.
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
    let mut new_data_pids = std::collections::HashSet::new();
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let stream_type = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        if is_data_only_stream_type(stream_type) {
            // Data-only PIDs (DSM-CC carousels, private sections) — file
            // them under `pmt_referenced_data_pids` so they don't fire
            // false-positive pid_errors / pts_errors against the A/V
            // timeouts, but DO count as "referenced" for the unreferenced-
            // PID check (they're legitimately in the PMT).
            new_data_pids.insert(es_pid);
            state.pmt_referenced_data_pids.insert(es_pid);
        } else {
            new_es_pids.insert(es_pid);
            // Register A/V ES PID if not already tracked
            state.es_pids.entry(es_pid).or_insert(None);
        }
        pos += 5 + es_info_length;
    }
    // Prune data PIDs no longer referenced by this PMT.
    state
        .pmt_referenced_data_pids
        .retain(|pid| new_data_pids.contains(pid));
    // Remove ES PIDs no longer referenced by this PMT.
    state.es_pids.retain(|pid, _| new_es_pids.contains(pid));
    // Prune the per-ES-PID stateful trackers in lock-step. Without this, an
    // input switch (or any PMT update that drops a PID) leaves stale entries
    // in `pts_tracker` and `pcr_repetition_tracker` that fire timeout errors
    // forever in the 500 ms sweep, manifesting as a steady ~2 errors/sec/
    // orphan on `pts_errors` / `pcr_repetition_errors` even when the live
    // stream is clean. `pcr_tracker` is pruned too so the regression window
    // resets cleanly if a PCR-bearing PID disappears and later reappears.
    // `cc_tracker` is intentionally NOT pruned here because it also tracks
    // PAT/PMT/NIT/SDT continuity, which are not in `new_es_pids`.
    state.pts_tracker.retain(|pid, _| new_es_pids.contains(pid));
    state
        .pcr_repetition_tracker
        .retain(|pid, _| new_es_pids.contains(pid));
    state
        .pcr_tracker
        .retain(|pid, _| new_es_pids.contains(pid));
    // Drop error latches for PIDs that are no longer referenced; if they
    // ever come back, they get a fresh chance to fire one error.
    state.es_errored.retain(|pid| new_es_pids.contains(pid));
    state.pts_errored.retain(|pid| new_es_pids.contains(pid));
    state
        .pcr_repetition_errored
        .retain(|pid| new_es_pids.contains(pid));
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
///
/// Each per-PID timeout (PMT-PID, ES-PID, PCR-repetition, PTS) latches
/// after firing once: it does not re-fire on the next sweep until the PID
/// has been observed again on the wire. Without that latch, every stale
/// or never-yet-seen entry produces 2 errors/sec/orphan indefinitely —
/// the dominant source of `pat_errors` / `pmt_errors` / `pid_errors` /
/// `pts_errors` / `pcr_repetition_errors` over-counting on healthy
/// streams. A startup grace of 2 × PAT_PMT_TIMEOUT after the first PAT
/// gives newly-registered PIDs a chance to arrive before they're tagged.
fn check_pat_pmt_timeouts(stats: &Tr101290Accumulator) {
    let mut state = stats.state.lock().unwrap();
    let now = Instant::now();

    // Only check timeouts after we've seen at least one PAT (avoid false
    // alarms during startup or for non-TS streams).
    if !state.pat_seen {
        return;
    }

    let startup_grace_elapsed = state
        .first_pat_time
        .map_or(false, |t| now.duration_since(t) > PAT_PMT_TIMEOUT * 2);

    // PAT timeout — singleton, no per-PID latch needed; just only fire
    // once per stall window.
    if let Some(last) = state.last_pat_time {
        if now.duration_since(last) > PAT_PMT_TIMEOUT {
            // Fire once per stall window: re-anchor `last_pat_time` to now
            // so the next PAT_PMT_TIMEOUT must elapse before we count again.
            stats.pat_errors.fetch_add(1, Ordering::Relaxed);
            stats.window_pat_errors.fetch_add(1, Ordering::Relaxed);
            state.last_pat_time = Some(now);
        }
    }

    // PMT timeouts — latched.
    let pmt_stale: Vec<u16> = state
        .pmt_pids
        .iter()
        .filter_map(|(pid, last_time)| {
            if state.pmt_errored.contains(pid) {
                return None;
            }
            match last_time {
                Some(t) if now.duration_since(*t) > PAT_PMT_TIMEOUT => Some(*pid),
                None if startup_grace_elapsed => Some(*pid),
                _ => None,
            }
        })
        .collect();
    for pid in pmt_stale {
        stats.pmt_errors.fetch_add(1, Ordering::Relaxed);
        stats.window_pmt_errors.fetch_add(1, Ordering::Relaxed);
        state.pmt_errored.insert(pid);
    }

    // PID error check (Priority 1): ES PIDs referenced in PMT must appear.
    // Latched.
    let es_stale: Vec<u16> = state
        .es_pids
        .iter()
        .filter_map(|(pid, last_time)| {
            if state.es_errored.contains(pid) {
                return None;
            }
            match last_time {
                Some(t) if now.duration_since(*t) > PAT_PMT_TIMEOUT => Some(*pid),
                None if startup_grace_elapsed => Some(*pid),
                _ => None,
            }
        })
        .collect();
    for pid in es_stale {
        stats.pid_errors.fetch_add(1, Ordering::Relaxed);
        stats.window_pid_errors.fetch_add(1, Ordering::Relaxed);
        state.es_errored.insert(pid);
    }

    // ── P2-extended timeouts ──
    // PCR repetition (§5.2.2): a PCR-bearing PID must emit a PCR within
    // 100 ms of the previous one. Latched the same way as `pid_errors`.
    let pcr_rep_stale: Vec<u16> = state
        .pcr_repetition_tracker
        .iter()
        .filter_map(|(pid, last)| {
            if state.pcr_repetition_errored.contains(pid) {
                None
            } else if now.duration_since(*last) > PCR_REPETITION_TIMEOUT {
                Some(*pid)
            } else {
                None
            }
        })
        .collect();
    for pid in pcr_rep_stale {
        stats.pcr_repetition_errors.fetch_add(1, Ordering::Relaxed);
        stats
            .window_pcr_repetition_errors
            .fetch_add(1, Ordering::Relaxed);
        state.pcr_repetition_errored.insert(pid);
    }

    // PTS error (§5.2.1): every PES PID that has produced a PTS once
    // must keep producing one within 700 ms. Latched.
    let pts_stale: Vec<u16> = state
        .pts_tracker
        .iter()
        .filter_map(|(pid, (_pts, last_seen))| {
            if state.pts_errored.contains(pid) {
                None
            } else if now.duration_since(*last_seen) > PTS_TIMEOUT {
                Some(*pid)
            } else {
                None
            }
        })
        .collect();
    for pid in pts_stale {
        stats.pts_errors.fetch_add(1, Ordering::Relaxed);
        stats.window_pts_errors.fetch_add(1, Ordering::Relaxed);
        state.pts_errored.insert(pid);
    }

    // CAT error (§5.2.4): once observed, must repeat within 500 ms.
    if state.cat_seen {
        if let Some(last) = state.last_cat_time {
            if now.duration_since(last) > CAT_TIMEOUT {
                stats.cat_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_cat_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // ── P3 timeouts (only count once observed, mirror `pat_seen`) ──
    if state.sdt_seen {
        if let Some(last) = state.last_sdt_time {
            if now.duration_since(last) > SDT_TIMEOUT {
                stats.sdt_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_sdt_errors.fetch_add(1, Ordering::Relaxed);
                stats.si_repetition_errors.fetch_add(1, Ordering::Relaxed);
                stats
                    .window_si_repetition_errors
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if state.nit_seen {
        if let Some(last) = state.last_nit_time {
            if now.duration_since(last) > NIT_TIMEOUT {
                stats.nit_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_nit_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if state.eit_seen {
        if let Some(last) = state.last_eit_time {
            if now.duration_since(last) > EIT_TIMEOUT {
                stats.eit_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_eit_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if state.tdt_seen {
        if let Some(last) = state.last_tdt_time {
            if now.duration_since(last) > TDT_TIMEOUT {
                stats.tdt_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_tdt_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if state.rst_seen {
        if let Some(last) = state.last_rst_time {
            if now.duration_since(last) > RST_TIMEOUT {
                stats.rst_errors.fetch_add(1, Ordering::Relaxed);
                stats.window_rst_errors.fetch_add(1, Ordering::Relaxed);
            }
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

    // ── PCR accuracy regression — ingress side ─────────────────────────
    //
    // The old per-pair `pcr_delta_27mhz` vs `wall_delta_27mhz` test fired
    // on every PCR-bearing packet of any real receive-side stream because
    // network/scheduler jitter is intrinsically µs–ms while the threshold
    // was 500 ns. The new check fits a least-squares line through a small
    // window of (pcr, wall) samples and measures the residual of the
    // newest sample against the line — slope absorbs encoder↔host rate
    // skew, intercept absorbs mean network delay.

    #[test]
    fn pcr_residual_perfect_constant_rate() {
        // 25 PCRs/sec at exactly 27 MHz wall-rate: residual must be 0.
        let mut h: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
        for i in 0..PCR_HISTORY_LEN as u64 {
            // PCR cadence = 40 ms = 1_080_000 ticks; wall cadence = 40 ms.
            h.push_back((i * 1_080_000, i * 40_000));
        }
        let next_pcr = (PCR_HISTORY_LEN as u64) * 1_080_000;
        let next_wall = (PCR_HISTORY_LEN as u64) * 40_000;
        let r = pcr_residual_ns(&h, next_pcr, next_wall).unwrap();
        assert!(r < 100, "perfect stream should have ~0 ns residual, got {r}");
    }

    #[test]
    fn pcr_residual_constant_offset_absorbed() {
        // Same constant rate as above, but every wall sample is offset
        // by +50 ms — mimics a long-tail constant-delay network path.
        // The intercept absorbs it; residual stays small.
        let mut h: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
        for i in 0..PCR_HISTORY_LEN as u64 {
            h.push_back((i * 1_080_000, i * 40_000 + 50_000));
        }
        let next_pcr = (PCR_HISTORY_LEN as u64) * 1_080_000;
        let next_wall = (PCR_HISTORY_LEN as u64) * 40_000 + 50_000;
        let r = pcr_residual_ns(&h, next_pcr, next_wall).unwrap();
        assert!(r < 100, "constant delay should be absorbed, got {r}");
    }

    #[test]
    fn pcr_residual_constant_rate_skew_absorbed() {
        // Encoder runs at 27 MHz, host wall runs slow (only ~26.95 MHz
        // effective). The slope absorbs the skew; residual stays small.
        let mut h: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
        for i in 0..PCR_HISTORY_LEN as u64 {
            // PCR advances 1_080_000 / sample; wall advances 40_080 µs
            // (0.2% slow) — pure skew, no jitter.
            h.push_back((i * 1_080_000, i * 40_080));
        }
        let next_pcr = (PCR_HISTORY_LEN as u64) * 1_080_000;
        let next_wall = (PCR_HISTORY_LEN as u64) * 40_080;
        let r = pcr_residual_ns(&h, next_pcr, next_wall).unwrap();
        assert!(r < 100, "constant skew should be absorbed, got {r}");
    }

    #[test]
    fn pcr_residual_flags_real_jitter() {
        // Build a clean window, then feed a sample that arrives 200 ms
        // late — a real PCR_AC violation that should fire even at the
        // receiver-jitter-tolerant 100 ms threshold.
        let mut h: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
        for i in 0..PCR_HISTORY_LEN as u64 {
            h.push_back((i * 1_080_000, i * 40_000));
        }
        let next_pcr = (PCR_HISTORY_LEN as u64) * 1_080_000;
        let next_wall = (PCR_HISTORY_LEN as u64) * 40_000 + 200_000;
        let r = pcr_residual_ns(&h, next_pcr, next_wall).unwrap();
        // 200 ms wall offset on a perfectly-rated stream → residual ≈ 200 ms.
        assert!(
            (180_000_000..=220_000_000).contains(&r),
            "expected ~200 ms residual, got {r}"
        );
        assert!(r > PCR_JITTER_THRESHOLD_NS, "200 ms must trip the threshold");
    }

    #[test]
    fn pcr_residual_too_few_samples() {
        let mut h: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
        h.push_back((0, 0));
        // Single-sample regression: undefined slope.
        assert!(pcr_residual_ns(&h, 1_080_000, 40_000).is_none());
    }

    #[test]
    fn pcr_residual_implausibly_large_returns_none() {
        // Stream-source change before the regression has caught up:
        // huge PCR jump, huge wall delta. The residual can be > 1 s in
        // 27 MHz tick space; the helper bails so we don't tag a fake
        // accuracy error during the catch-up window.
        let mut h: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
        for i in 0..PCR_HISTORY_LEN as u64 {
            h.push_back((i * 1_080_000, i * 40_000));
        }
        // PCR jumps 100 s ahead with no matching wall jump.
        let next_pcr = 2_700_000_000_u64;
        let next_wall = (PCR_HISTORY_LEN as u64) * 40_000;
        assert_eq!(pcr_residual_ns(&h, next_pcr, next_wall), None);
    }

    /// Data-only stream types must be classified as such.
    #[test]
    fn data_only_stream_types_recognised() {
        assert!(is_data_only_stream_type(0x05)); // private_sections
        assert!(is_data_only_stream_type(0x0B)); // DSM-CC U-N
        assert!(is_data_only_stream_type(0x0C)); // DSM-CC stream descriptors
        assert!(is_data_only_stream_type(0x0D)); // DSM-CC sections
        assert!(is_data_only_stream_type(0x14)); // synchronized download
    }

    /// A/V stream types must NOT be classified as data-only.
    #[test]
    fn av_stream_types_not_data_only() {
        for st in [0x01, 0x02, 0x03, 0x04, 0x06, 0x0F, 0x11, 0x1B, 0x24, 0x81, 0x87, 0xAC] {
            assert!(
                !is_data_only_stream_type(st),
                "stream_type 0x{st:02X} must NOT be data-only"
            );
        }
    }

    /// PMTs that mix A/V and data PIDs must register A/V into `es_pids`
    /// and data into `pmt_referenced_data_pids` — the regression we're
    /// guarding against is the ISDB-T case where DSM-CC carousels (0x0B/0x0C)
    /// and ARIB private sections (0x05) used to fire false-positive
    /// `pid_errors` against the 500 ms PAT_PMT_TIMEOUT.
    #[test]
    fn pmt_isdb_t_carousels_dont_register_as_av_es_pids() {
        // Build a PMT packet listing 5 ES entries:
        //   - 0x0111 type 0x1B (H.264) → A/V
        //   - 0x0112 type 0x11 (LATM)  → A/V
        //   - 0x01F4 type 0x05 (private sections) → data
        //   - 0x0384 type 0x0B (DSM-CC U-N) → data
        //   - 0x05DC type 0x0C (DSM-CC stream desc) → data
        let entries: [(u8, u16); 5] = [
            (0x1B, 0x0111),
            (0x11, 0x0112),
            (0x05, 0x01F4),
            (0x0B, 0x0384),
            (0x0C, 0x05DC),
        ];
        let mut pkt = vec![0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID hi = 0
        pkt[2] = 0x10; // PID lo = 0x10 (some PMT PID)
        pkt[3] = 0x10; // afc=01 (payload only), CC=0
        pkt[4] = 0x00; // pointer_field
        let mut p = 5;
        pkt[p] = 0x02; // table_id=PMT
        let n = entries.len();
        let section_length = 9 + 5 * n + 4;
        pkt[p + 1] = 0xB0 | (((section_length as u16) >> 8) & 0x0F) as u8;
        pkt[p + 2] = (section_length as u16 & 0xFF) as u8;
        // program_number, version+cni, section_no, last_section_no
        pkt[p + 3] = 0x5C;
        pkt[p + 4] = 0x20; // program 0x5C20 = 23584
        pkt[p + 5] = 0xC1;
        pkt[p + 6] = 0x00;
        pkt[p + 7] = 0x00;
        // PCR_PID
        pkt[p + 8] = 0xE1;
        pkt[p + 9] = 0x00;
        // program_info_length=0
        pkt[p + 10] = 0xF0;
        pkt[p + 11] = 0x00;
        p += 12;
        for (st, es_pid) in entries.iter() {
            pkt[p] = *st;
            pkt[p + 1] = 0xE0 | ((*es_pid >> 8) & 0x1F) as u8;
            pkt[p + 2] = (*es_pid & 0xFF) as u8;
            pkt[p + 3] = 0xF0; // ES_info_length=0
            pkt[p + 4] = 0x00;
            p += 5;
        }
        // Leave CRC bytes as zero — the extractor doesn't verify CRC.

        let stats = Arc::new(Tr101290Accumulator::new());
        let mut state = stats.state.lock().unwrap();
        extract_es_pids_from_pmt(&pkt, &mut state);

        // A/V PIDs must be tracked for QoS.
        assert!(state.es_pids.contains_key(&0x0111));
        assert!(state.es_pids.contains_key(&0x0112));
        // Data PIDs must NOT be tracked for QoS — they live in the
        // separate referenced-but-not-monitored set.
        assert!(!state.es_pids.contains_key(&0x01F4));
        assert!(!state.es_pids.contains_key(&0x0384));
        assert!(!state.es_pids.contains_key(&0x05DC));
        // Data PIDs ARE counted as PMT-referenced (so they don't fire
        // unreferenced_pid_errors when packets land for them).
        assert!(state.pmt_referenced_data_pids.contains(&0x01F4));
        assert!(state.pmt_referenced_data_pids.contains(&0x0384));
        assert!(state.pmt_referenced_data_pids.contains(&0x05DC));
    }
}
