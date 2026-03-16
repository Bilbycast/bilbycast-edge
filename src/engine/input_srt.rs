use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::SrtInputConfig;
use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
use crate::srt::connection::{connect_srt_input, connect_srt_redundancy_leg};
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::rtp_parse::{is_likely_rtp, parse_rtp_sequence_number, parse_rtp_timestamp};
use crate::util::time::now_us;

use super::packet::RtpPacket;

/// Spawn a task that receives RTP packets from an SRT connection and
/// publishes them to a broadcast channel for fan-out to outputs.
/// If the config has redundancy enabled, two SRT legs are connected
/// and packets are merged using SMPTE 2022-7 hitless merge.
pub fn spawn_srt_input(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = if config.redundancy.is_some() {
            srt_input_redundant_loop(config, broadcast_tx, stats, cancel).await
        } else {
            srt_input_loop(config, broadcast_tx, stats, cancel).await
        };
        if let Err(e) = result {
            tracing::error!("SRT input task exited with error: {e}");
        }
    })
}

// ---------------------------------------------------------------------------
// Single-leg SRT input (no redundancy)
// ---------------------------------------------------------------------------

/// Core receive loop for a single-leg SRT input (no redundancy).
///
/// Establishes an SRT connection (caller or listener, per config), then
/// receives packets asynchronously using `socket.recv().await`. Each
/// received packet is parsed for RTP headers, sequence gaps are tracked
/// for loss stats, and packets are published to the broadcast channel
/// for fan-out to outputs.
///
/// If the SRT connection drops (recv returns an error), the outer loop
/// closes the socket and attempts to reconnect after a 1-second delay.
/// Reconnection continues until the cancellation token fires.
async fn srt_input_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    // Outer reconnection loop
    loop {
        // Connect with retry (respects cancellation)
        let socket = match connect_srt_input(&config, &cancel).await {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    tracing::info!("SRT input stopping (cancelled during connect)");
                    return Ok(());
                }
                tracing::error!("SRT input connection failed: {e}");
                return Err(e);
            }
        };

        tracing::info!(
            "SRT input connected: mode={:?} local={}",
            config.mode,
            config.local_addr,
        );

        let mut last_seq: Option<u16> = None;

        // Inner packet processing loop — runs until disconnect or cancel
        let connection_lost = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("SRT input stopping (cancelled)");
                    let _ = socket.close().await;
                    return Ok(());
                }
                result = socket.recv() => {
                    match result {
                        Ok(data) => {
                            if !is_likely_rtp(&data) {
                                continue;
                            }

                            let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
                            let ts = parse_rtp_timestamp(&data).unwrap_or(0);

                            // Detect sequence gaps for loss counting
                            if let Some(prev) = last_seq {
                                let expected = prev.wrapping_add(1);
                                if seq != expected {
                                    let gap = seq.wrapping_sub(expected) as u64;
                                    if gap > 0 && gap < 1000 {
                                        stats.input_loss.fetch_add(gap, Ordering::Relaxed);
                                    }
                                }
                            }
                            last_seq = Some(seq);

                            // Update stats
                            stats.input_packets.fetch_add(1, Ordering::Relaxed);
                            stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

                            let packet = RtpPacket {
                                data,
                                sequence_number: seq,
                                rtp_timestamp: ts,
                                recv_time_us: now_us(),
                            };

                            // Send to broadcast channel for fan-out
                            let _ = broadcast_tx.send(packet);
                        }
                        Err(_) => {
                            // Connection lost
                            tracing::warn!("SRT input connection lost, will reconnect");
                            break true;
                        }
                    }
                }
            }
        };

        // Close old socket before reconnecting
        let _ = socket.close().await;

        if !connection_lost {
            break;
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input stopping during reconnect delay");
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT input attempting reconnection...");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Dual-leg SRT input with SMPTE 2022-7 hitless merge
// ---------------------------------------------------------------------------

/// Core receive loop for dual-leg SRT input with SMPTE 2022-7 hitless merge.
///
/// Connects two independent SRT legs (leg 1 from the main config, leg 2 from
/// the redundancy config). Both legs are received asynchronously using
/// `tokio::select!` over `socket.recv().await`.
///
/// Each received packet is passed through [`HitlessMerger::try_merge`], which
/// implements SMPTE 2022-7 sequence-based deduplication:
///
/// - If the packet's sequence number advances the merged output stream, it is
///   forwarded to the broadcast channel and the producing leg is noted.
/// - If the packet is a duplicate (already forwarded via the other leg) or
///   out-of-order, it is silently dropped.
///
/// A **redundancy switch** occurs when the merger starts forwarding packets
/// from a different leg than it was previously using. This happens when the
/// previously active leg is lagging or has lost packets. Each switch is
/// counted in `stats.redundancy_switches` for monitoring.
///
/// If both legs are lost, the loop terminates.
async fn srt_input_redundant_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Outer reconnection loop — restarts both legs on total connection loss
    loop {
        // --- Connect leg 1 ---
        let socket_leg1 = match connect_srt_input(&config, &cancel).await {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                return Err(e);
            }
        };
        tracing::info!(
            "SRT input leg1 connected: mode={:?} local={}",
            config.mode,
            config.local_addr
        );

        // --- Connect leg 2 (best-effort — fall back to single-leg if it fails) ---
        let socket_leg2 = match connect_srt_redundancy_leg(redundancy, &cancel).await {
            Ok(s) => Some(s),
            Err(e) => {
                if cancel.is_cancelled() {
                    let _ = socket_leg1.close().await;
                    return Ok(());
                }
                tracing::warn!("SRT input leg2 connection failed: {e}, running single-leg");
                None
            }
        };
        if socket_leg2.is_some() {
            tracing::info!(
                "SRT input leg2 connected: mode={:?} local={}",
                redundancy.mode,
                redundancy.local_addr
            );
        }

        let mut merger = HitlessMerger::new();
        let mut prev_active_leg = ActiveLeg::None;
        let mut leg1_alive = true;
        let mut leg2_alive = socket_leg2.is_some();

        // Use a dummy socket reference for leg2 when not connected. The `if leg2_alive`
        // guard on the select branch ensures we never actually recv from it.
        let dummy_leg2;
        let socket_leg2_ref = match &socket_leg2 {
            Some(s) => s,
            None => {
                // We need a reference that lives long enough but is never used
                dummy_leg2 = socket_leg1.clone();
                &dummy_leg2
            }
        };

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("SRT input (redundant) stopping (cancelled)");
                    let _ = socket_leg1.close().await;
                    if let Some(ref s) = socket_leg2 {
                        let _ = s.close().await;
                    }
                    return Ok(());
                }
                result = socket_leg1.recv(), if leg1_alive => {
                    match result {
                        Ok(data) => {
                            process_redundant_packet(
                                data,
                                ActiveLeg::Leg1,
                                &mut merger,
                                &mut prev_active_leg,
                                &stats,
                                &broadcast_tx,
                            );
                        }
                        Err(_) => {
                            tracing::warn!("SRT input leg1 connection lost");
                            leg1_alive = false;
                            if !leg2_alive {
                                tracing::warn!("SRT input (redundant) both legs lost, will reconnect");
                                break;
                            }
                        }
                    }
                }
                result = socket_leg2_ref.recv(), if leg2_alive => {
                    match result {
                        Ok(data) => {
                            process_redundant_packet(
                                data,
                                ActiveLeg::Leg2,
                                &mut merger,
                                &mut prev_active_leg,
                                &stats,
                                &broadcast_tx,
                            );
                        }
                        Err(_) => {
                            tracing::warn!("SRT input leg2 connection lost");
                            leg2_alive = false;
                            if !leg1_alive {
                                tracing::warn!("SRT input (redundant) both legs lost, will reconnect");
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Cleanup before reconnect
        let _ = socket_leg1.close().await;
        if let Some(ref s) = socket_leg2 {
            let _ = s.close().await;
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input (redundant) stopping during reconnect delay");
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT input (redundant) attempting reconnection...");
    }
}

/// Process a single packet from a redundancy leg through the hitless merger.
///
/// Checks if the packet is valid RTP, attempts merge, tracks redundancy
/// switches, updates stats, and broadcasts if the packet advances the stream.
fn process_redundant_packet(
    data: bytes::Bytes,
    leg: ActiveLeg,
    merger: &mut HitlessMerger,
    prev_active_leg: &mut ActiveLeg,
    stats: &Arc<FlowStatsAccumulator>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
) {
    if !is_likely_rtp(&data) {
        return;
    }

    let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
    let ts = parse_rtp_timestamp(&data).unwrap_or(0);

    // Try merge — only forward if this packet advances the sequence
    if let Some(chosen_leg) = merger.try_merge(seq, leg) {
        // Track redundancy switches
        if *prev_active_leg != ActiveLeg::None && chosen_leg != *prev_active_leg {
            stats
                .redundancy_switches
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                "SRT input redundancy switch: {:?} -> {:?} at seq {}",
                prev_active_leg,
                chosen_leg,
                seq
            );
        }
        *prev_active_leg = chosen_leg;

        // Update stats
        stats.input_packets.fetch_add(1, Ordering::Relaxed);
        stats
            .input_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        let packet = RtpPacket {
            data,
            sequence_number: seq,
            rtp_timestamp: ts,
            recv_time_us: now_us(),
        };

        let _ = broadcast_tx.send(packet);
    }
    // else: duplicate or out-of-order — silently dropped
}
