// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{SrtInputConfig, SrtMode};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
use crate::srt::connection::{
    accept_srt_connection, bind_srt_listener_for_input, bind_srt_listener_for_redundancy,
    connect_srt_input, connect_srt_redundancy_leg, spawn_srt_stats_poller,
};
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::rtp_parse::{is_likely_rtp, parse_rtp_sequence_number, parse_rtp_timestamp};
use crate::util::time::now_us;

use super::input_transcode::{publish_input_packet, InputTranscoder};
use super::packet::RtpPacket;

// ── Constants ──────────────────────────────────────────────────────────────

/// TS sync byte — used to detect raw TS data (first byte = 0x47).
const TS_SYNC_BYTE: u8 = 0x47;

/// TS packet size.
const TS_PACKET_SIZE: usize = 188;

// ── SRT Payload Format Detection ───────────────────────────────────────────

/// Detected payload format inside SRT datagrams.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SrtPayloadFormat {
    /// Not yet detected — waiting for first packet.
    Unknown,
    /// RTP/TS per SMPTE ST 2022-2: RTP header (12+ bytes) + TS packets.
    RtpTs,
    /// Raw MPEG-2 TS: one or more 188-byte packets with no RTP header.
    /// This is what OBS, Haivision, srt-live-transmit, and vMix send.
    RawTs,
}

/// Detect the payload format of an SRT datagram.
///
/// Checks whether the first bytes look like an RTP header (version 2) or
/// a raw TS sync byte (0x47). If the data starts with 0x47 and its length
/// is a multiple of 188, it's almost certainly raw TS.
fn detect_format(data: &[u8]) -> SrtPayloadFormat {
    if data.len() < 12 {
        return SrtPayloadFormat::Unknown;
    }

    // Check for RTP version 2 header
    if is_likely_rtp(data) {
        return SrtPayloadFormat::RtpTs;
    }

    // Check for raw TS: starts with sync byte and length is multiple of 188
    if data[0] == TS_SYNC_BYTE && data.len() >= TS_PACKET_SIZE {
        return SrtPayloadFormat::RawTs;
    }

    SrtPayloadFormat::Unknown
}

// ── Public Entry Point ─────────────────────────────────────────────────────

/// Spawn a task that receives packets from an SRT connection and publishes
/// them to a broadcast channel for fan-out to outputs.
///
/// Supports both RTP-wrapped TS (SMPTE ST 2022-2) and raw TS over SRT.
/// Format is auto-detected from the first received packet:
/// - **RTP/TS**: packets are parsed for RTP seq/timestamp, forwarded with
///   `is_raw_ts = false`.
/// - **Raw TS**: a synthetic incrementing sequence number is generated,
///   forwarded with `is_raw_ts = true` so outputs know not to strip an
///   RTP header.
///
/// If the config has redundancy enabled, two SRT legs are connected and
/// packets are merged using SMPTE 2022-7 hitless protection switching.
/// For raw TS, 2022-7 merge uses a synthetic sequence counter per leg.
pub fn spawn_srt_input(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Build ingress transcoder once per connection cycle so PMT discovery
        // state and codec buffers persist across reconnects within a single
        // configured flow.
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("SRT input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("SRT input: transcode setup failed, falling back to passthrough: {e}");
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::FLOW,
                    format!("SRT input transcode disabled: {e}"),
                    &flow_id,
                );
                None
            }
        };
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_ref(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
        );

        let result = if config.redundancy.is_some() {
            srt_input_redundant_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder).await
        } else {
            srt_input_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder).await
        };
        if let Err(e) = result {
            tracing::error!("SRT input task exited with error: {e}");
            event_sender.emit_flow(EventSeverity::Critical, category::FLOW, format!("Flow input lost: {e}"), &flow_id);
        }
    })
}

// ---------------------------------------------------------------------------
// Single-leg SRT input (no redundancy)
// ---------------------------------------------------------------------------

async fn srt_input_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => srt_input_listener_loop(config, broadcast_tx, stats, cancel, events, flow_id, transcoder).await,
        _ => srt_input_caller_loop(config, broadcast_tx, stats, cancel, events, flow_id, transcoder).await,
    }
}

/// Listener-mode input: binds the listener once and re-accepts new callers
/// on disconnect.
async fn srt_input_listener_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    let mut listener = bind_srt_listener_for_input(&config).await?;
    let mut format = SrtPayloadFormat::Unknown;
    let mut last_seq: Option<u16> = None;
    let mut raw_ts_seq_counter: u16 = 0;
    let mut raw_ts_timestamp: u32 = 0;

    loop {
        let socket = match accept_srt_connection(&mut listener, &cancel).await {
            Ok(s) => s,
            Err(_) if cancel.is_cancelled() => {
                tracing::info!("SRT input stopping (cancelled during accept)");
                let _ = listener.close().await;
                return Ok(());
            }
            Err(e) => {
                tracing::error!("SRT input accept failed: {e}");
                events.emit_flow_with_details(
                    EventSeverity::Critical, category::SRT,
                    format!("SRT input connection failed: {e}"), flow_id,
                    serde_json::json!({
                        "mode": "listener",
                        "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                        "stream_id": config.stream_id.as_deref().unwrap_or(""),
                        "error": e.to_string(),
                    }),
                );
                let _ = listener.close().await;
                return Err(e);
            }
        };

        tracing::info!(
            "SRT input connected: mode=listener local={}",
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_flow_with_details(
            EventSeverity::Info, category::SRT,
            "SRT input connected (mode=listener)", flow_id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.input_srt_stats_cache.clone(), poller_cancel.clone());

        let disconnected = srt_input_recv_loop(
            &socket, &broadcast_tx, &stats, &cancel,
            &mut format, &mut last_seq, &mut raw_ts_seq_counter, &mut raw_ts_timestamp,
            transcoder,
        ).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            let _ = listener.close().await;
            return Ok(());
        }

        events.emit_flow_with_details(
            EventSeverity::Warning, category::SRT,
            "SRT input disconnected, reconnecting", flow_id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input stopping during reconnect delay");
                let _ = listener.close().await;
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT input waiting for caller to reconnect...");
    }
}

/// Caller-mode input: reconnects with exponential back-off on disconnect.
async fn srt_input_caller_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    let mut format = SrtPayloadFormat::Unknown;
    let mut last_seq: Option<u16> = None;
    let mut raw_ts_seq_counter: u16 = 0;
    let mut raw_ts_timestamp: u32 = 0;

    loop {
        let socket = match connect_srt_input(&config, &cancel).await {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    tracing::info!("SRT input stopping (cancelled during connect)");
                    return Ok(());
                }
                tracing::error!("SRT input connection failed: {e}");
                events.emit_flow_with_details(
                    EventSeverity::Critical, category::SRT,
                    format!("SRT input connection failed: {e}"), flow_id,
                    serde_json::json!({
                        "mode": format!("{:?}", config.mode),
                        "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
                        "stream_id": config.stream_id.as_deref().unwrap_or(""),
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
        };

        tracing::info!(
            "SRT input connected: mode={:?} local={}",
            config.mode,
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_flow_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT input connected (mode={:?})", config.mode), flow_id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.input_srt_stats_cache.clone(), poller_cancel.clone());

        let disconnected = srt_input_recv_loop(
            &socket, &broadcast_tx, &stats, &cancel,
            &mut format, &mut last_seq, &mut raw_ts_seq_counter, &mut raw_ts_timestamp,
            transcoder,
        ).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            break;
        }

        events.emit_flow_with_details(
            EventSeverity::Warning, category::SRT,
            "SRT input disconnected, reconnecting", flow_id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
            }),
        );

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

/// Inner receive loop shared by listener and caller input modes.
///
/// Returns `Ok(true)` if the connection was lost (caller should reconnect),
/// `Ok(false)` if cancelled (flow is shutting down).
async fn srt_input_recv_loop(
    socket: &Arc<srt_transport::SrtSocket>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    format: &mut SrtPayloadFormat,
    last_seq: &mut Option<u16>,
    raw_ts_seq_counter: &mut u16,
    raw_ts_timestamp: &mut u32,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<bool> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input stopping (cancelled)");
                return Ok(false);
            }
            result = socket.recv() => {
                match result {
                    Ok(data) => {
                        // Auto-detect format on first packet
                        if *format == SrtPayloadFormat::Unknown {
                            *format = detect_format(&data);
                            match *format {
                                SrtPayloadFormat::RtpTs => {
                                    tracing::info!("SRT input: detected RTP/TS format (SMPTE ST 2022-2)");
                                }
                                SrtPayloadFormat::RawTs => {
                                    tracing::info!("SRT input: detected raw TS format (OBS/Haivision/srt-live-transmit compatible)");
                                }
                                SrtPayloadFormat::Unknown => {
                                    tracing::warn!("SRT input: unrecognized payload format, treating as raw data");
                                    *format = SrtPayloadFormat::RawTs;
                                }
                            }
                        }

                        let (seq, ts, is_raw) = match *format {
                            SrtPayloadFormat::RtpTs => {
                                if !is_likely_rtp(&data) {
                                    continue;
                                }
                                let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
                                let ts = parse_rtp_timestamp(&data).unwrap_or(0);
                                (seq, ts, false)
                            }
                            SrtPayloadFormat::RawTs | SrtPayloadFormat::Unknown => {
                                let seq = *raw_ts_seq_counter;
                                *raw_ts_seq_counter = raw_ts_seq_counter.wrapping_add(1);
                                let ts_pkts = (data.len() / TS_PACKET_SIZE) as u32;
                                *raw_ts_timestamp = raw_ts_timestamp.wrapping_add(ts_pkts * 188 * 8);
                                (seq, *raw_ts_timestamp, true)
                            }
                        };

                        // Detect sequence gaps for loss counting
                        if let Some(prev) = *last_seq {
                            let expected = prev.wrapping_add(1);
                            if seq != expected {
                                let gap = seq.wrapping_sub(expected) as u64;
                                if gap > 0 && gap < 1000 {
                                    stats.input_loss.fetch_add(gap, Ordering::Relaxed);
                                }
                            }
                        }
                        *last_seq = Some(seq);

                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

                        // Bandwidth limit enforcement: drop packet if flow is blocked
                        if stats.bandwidth_blocked.load(Ordering::Relaxed) {
                            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }

                        let packet = RtpPacket {
                            data,
                            sequence_number: seq,
                            rtp_timestamp: ts,
                            recv_time_us: now_us(),
                            is_raw_ts: is_raw,
                        };

                        publish_input_packet(transcoder, broadcast_tx, packet);
                    }
                    Err(_) => {
                        tracing::warn!("SRT input connection lost, will reconnect");
                        return Ok(true);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dual-leg SRT input with SMPTE 2022-7 hitless protection switching
// ---------------------------------------------------------------------------

/// Dual-leg SRT input with SMPTE 2022-7 hitless merge.
///
/// For **RTP/TS** streams: merge uses the RTP sequence number (standard 2022-7).
/// For **raw TS** streams: merge uses a synthetic per-leg sequence counter.
/// Both legs must use the same format — if one leg sends RTP and the other
/// sends raw TS, the raw TS leg is accepted but merge may not deduplicate
/// perfectly (documented limitation).
///
/// Listener-mode legs bind once and re-accept on disconnect; caller-mode legs
/// reconnect with exponential back-off.
async fn srt_input_redundant_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Bind persistent listeners for any legs in listener mode
    let mut listener_leg1 = if config.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_input(&config).await?)
    } else {
        None
    };
    let mut listener_leg2 = if redundancy.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_redundancy(redundancy).await?)
    } else {
        None
    };

    // Outer reconnection loop
    loop {
        // --- Connect/accept leg 1 ---
        let socket_leg1 = if let Some(ref mut listener) = listener_leg1 {
            match accept_srt_connection(listener, &cancel).await {
                Ok(s) => s,
                Err(_) if cancel.is_cancelled() => {
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                    return Ok(());
                }
                Err(e) => {
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                    return Err(e);
                }
            }
        } else {
            match connect_srt_input(&config, &cancel).await {
                Ok(s) => s,
                Err(e) => {
                    if cancel.is_cancelled() {
                        if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                        if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                        return Ok(());
                    }
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                    return Err(e);
                }
            }
        };
        tracing::info!(
            "SRT input leg1 connected: mode={:?} local={}",
            config.mode,
            config.local_addr.as_deref().unwrap_or("auto")
        );
        events.emit_flow_with_details(
            EventSeverity::Info, category::SRT,
            "SRT input connected (mode=listener, redundant leg 1)", flow_id,
            serde_json::json!({
                "mode": "listener",
                "leg": 1,
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );

        // --- Connect/accept leg 2 (best-effort) ---
        let socket_leg2 = if let Some(ref mut listener) = listener_leg2 {
            match accept_srt_connection(listener, &cancel).await {
                Ok(s) => Some(s),
                Err(_) if cancel.is_cancelled() => {
                    let _ = socket_leg1.close().await;
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!("SRT input leg2 accept failed: {e}, running single-leg");
                    None
                }
            }
        } else {
            match connect_srt_redundancy_leg(redundancy, &cancel).await {
                Ok(s) => Some(s),
                Err(e) => {
                    if cancel.is_cancelled() {
                        let _ = socket_leg1.close().await;
                        if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                        if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                        return Ok(());
                    }
                    tracing::warn!("SRT input leg2 connection failed: {e}, running single-leg");
                    None
                }
            }
        };
        if socket_leg2.is_some() {
            tracing::info!(
                "SRT input leg2 connected: mode={:?} local={}",
                redundancy.mode,
                redundancy.local_addr.as_deref().unwrap_or("auto")
            );
        }

        let mut merger = HitlessMerger::new();
        let mut prev_active_leg = ActiveLeg::None;
        let mut leg1_alive = true;
        let mut leg2_alive = socket_leg2.is_some();
        let mut format = SrtPayloadFormat::Unknown;
        let mut raw_ts_seq_leg1: u16 = 0;
        let mut raw_ts_seq_leg2: u16 = 0;
        let mut raw_ts_timestamp: u32 = 0;

        let dummy_leg2;
        let socket_leg2_ref = match &socket_leg2 {
            Some(s) => s,
            None => {
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
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                    return Ok(());
                }
                result = socket_leg1.recv(), if leg1_alive => {
                    match result {
                        Ok(data) => {
                            if format == SrtPayloadFormat::Unknown {
                                format = detect_format(&data);
                                log_detected_format(format);
                            }
                            process_redundant_packet(
                                data,
                                ActiveLeg::Leg1,
                                format,
                                &mut raw_ts_seq_leg1,
                                &mut raw_ts_timestamp,
                                &mut merger,
                                &mut prev_active_leg,
                                &stats,
                                &broadcast_tx,
                                transcoder,
                            );
                        }
                        Err(_) => {
                            tracing::warn!("SRT input leg1 connection lost");
                            events.emit_flow(EventSeverity::Warning, category::REDUNDANCY, "Redundant leg 1 lost", flow_id);
                            leg1_alive = false;
                            if !leg2_alive {
                                tracing::warn!("SRT input (redundant) both legs lost, will reconnect");
                                events.emit_flow(EventSeverity::Critical, category::REDUNDANCY, "Both redundant legs lost", flow_id);
                                break;
                            }
                        }
                    }
                }
                result = socket_leg2_ref.recv(), if leg2_alive => {
                    match result {
                        Ok(data) => {
                            if format == SrtPayloadFormat::Unknown {
                                format = detect_format(&data);
                                log_detected_format(format);
                            }
                            process_redundant_packet(
                                data,
                                ActiveLeg::Leg2,
                                format,
                                &mut raw_ts_seq_leg2,
                                &mut raw_ts_timestamp,
                                &mut merger,
                                &mut prev_active_leg,
                                &stats,
                                &broadcast_tx,
                                transcoder,
                            );
                        }
                        Err(_) => {
                            tracing::warn!("SRT input leg2 connection lost");
                            events.emit_flow(EventSeverity::Warning, category::REDUNDANCY, "Redundant leg 2 lost", flow_id);
                            leg2_alive = false;
                            if !leg1_alive {
                                tracing::warn!("SRT input (redundant) both legs lost, will reconnect");
                                events.emit_flow(EventSeverity::Critical, category::REDUNDANCY, "Both redundant legs lost", flow_id);
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Cleanup sockets (listeners stay open)
        let _ = socket_leg1.close().await;
        if let Some(ref s) = socket_leg2 {
            let _ = s.close().await;
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input (redundant) stopping during reconnect delay");
                if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT input (redundant) attempting reconnection...");
    }
}

fn log_detected_format(format: SrtPayloadFormat) {
    match format {
        SrtPayloadFormat::RtpTs => {
            tracing::info!("SRT input: detected RTP/TS format (SMPTE ST 2022-2)");
        }
        SrtPayloadFormat::RawTs => {
            tracing::info!("SRT input: detected raw TS format (OBS/Haivision compatible)");
        }
        SrtPayloadFormat::Unknown => {
            tracing::warn!("SRT input: unrecognized format, treating as raw data");
        }
    }
}

/// Process a single packet from a redundancy leg through the hitless merger.
///
/// Handles both RTP/TS and raw TS formats. For RTP/TS, the RTP sequence
/// number is used for 2022-7 merge. For raw TS, a per-leg synthetic
/// sequence counter is used.
fn process_redundant_packet(
    data: Bytes,
    leg: ActiveLeg,
    format: SrtPayloadFormat,
    raw_ts_seq: &mut u16,
    raw_ts_timestamp: &mut u32,
    merger: &mut HitlessMerger,
    prev_active_leg: &mut ActiveLeg,
    stats: &Arc<FlowStatsAccumulator>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    transcoder: &mut Option<InputTranscoder>,
) {
    let (seq, ts, is_raw) = match format {
        SrtPayloadFormat::RtpTs => {
            if !is_likely_rtp(&data) {
                return;
            }
            let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
            let ts = parse_rtp_timestamp(&data).unwrap_or(0);
            (seq, ts, false)
        }
        SrtPayloadFormat::RawTs | SrtPayloadFormat::Unknown => {
            let seq = *raw_ts_seq;
            *raw_ts_seq = raw_ts_seq.wrapping_add(1);
            let ts_pkts = (data.len() / TS_PACKET_SIZE) as u32;
            *raw_ts_timestamp = raw_ts_timestamp.wrapping_add(ts_pkts * 188 * 8);
            (seq, *raw_ts_timestamp, true)
        }
    };

    // For RTP/TS, 2022-7 merge uses the real RTP sequence number.
    // For raw TS with redundancy, we still attempt merge using the synthetic
    // sequence — this provides basic protection switching (if one leg drops,
    // the other takes over) but cannot deduplicate identical packets since
    // each leg generates independent synthetic sequences.
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

        stats.input_packets.fetch_add(1, Ordering::Relaxed);
        stats
            .input_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        // Bandwidth limit enforcement: drop packet if flow is blocked
        if stats.bandwidth_blocked.load(Ordering::Relaxed) {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let packet = RtpPacket {
            data,
            sequence_number: seq,
            rtp_timestamp: ts,
            recv_time_us: now_us(),
            is_raw_ts: is_raw,
        };

        publish_input_packet(transcoder, broadcast_tx, packet);
    }
}
