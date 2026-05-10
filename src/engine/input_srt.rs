// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Maximum time we wait for the SMPTE 2022-7 leg-2 socket to complete its
/// initial handshake before giving up and running the input in single-leg
/// mode. Prevents a misconfigured or unreachable leg-2 peer from gating
/// ingest on the healthy leg-1.
const INITIAL_LEG2_CONNECT_BUDGET: Duration = Duration::from_secs(15);

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{SrtInputConfig, SrtMode};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
use crate::srt::connection::{
    accept_srt_connection, bind_srt_listener_for_bonded_input,
    bind_srt_listener_for_input, bind_srt_listener_for_redundancy, connect_srt_group,
    connect_srt_input, connect_srt_redundancy_leg, spawn_srt_group_stats_poller,
    spawn_srt_socket_group_stats_poller, spawn_srt_stats_poller, SrtConnectionParams,
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
    force_idr: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Apply interface_binding (loose only on SRT in Phase 1): when set
        // and `local_addr` is unset, resolve the NIC name to its primary
        // IPv4 and inject as the SRT source. Strict mode rejected at
        // validation time — see `validate_interface_binding`.
        let mut config = config;
        match crate::util::socket::srt_local_addr_from_binding(
            config.interface_binding.as_ref(),
            config.local_addr.as_deref(),
        ) {
            Ok(Some(addr)) => {
                tracing::info!(
                    "SRT input '{}': interface_binding {} → local_addr {}",
                    input_id,
                    config.interface_binding.as_ref().map(|b| b.name.as_str()).unwrap_or("?"),
                    addr,
                );
                config.local_addr = Some(addr);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("SRT input '{input_id}': interface_binding failed: {e}");
                event_sender.emit_flow_with_details(
                    EventSeverity::Critical,
                    crate::manager::events::category::SRT,
                    format!("SRT input '{input_id}': interface_binding rejected: {e}"),
                    &flow_id,
                    serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
                );
                return;
            }
        }
        // Apply same to the redundancy leg (independent NIC pinning for
        // 2022-7-style SRT dual-leg).
        if let Some(red) = config.redundancy.as_mut() {
            match crate::util::socket::srt_local_addr_from_binding(
                red.interface_binding.as_ref(),
                red.local_addr.as_deref(),
            ) {
                Ok(Some(addr)) => {
                    tracing::info!(
                        "SRT input '{}' leg2: interface_binding {} → local_addr {}",
                        input_id,
                        red.interface_binding.as_ref().map(|b| b.name.as_str()).unwrap_or("?"),
                        addr,
                    );
                    red.local_addr = Some(addr);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("SRT input '{input_id}' leg2: interface_binding failed: {e}");
                    event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        crate::manager::events::category::SRT,
                        format!("SRT input '{input_id}' leg2: interface_binding rejected: {e}"),
                        &flow_id,
                        serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
                    );
                    return;
                }
            }
        }
        // Apply per-endpoint binding for SRT bonding members. Each member
        // can pin a different NIC — the whole point of bonding for path
        // diversity.
        if let Some(bonding) = config.bonding.as_mut() {
            for ep in bonding.endpoints.iter_mut() {
                match crate::util::socket::srt_local_addr_from_binding(
                    ep.interface_binding.as_ref(),
                    ep.local_addr.as_deref(),
                ) {
                    Ok(Some(addr)) => {
                        ep.local_addr = Some(addr);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("SRT input '{input_id}' bonding endpoint: {e}");
                        event_sender.emit_flow_with_details(
                            EventSeverity::Critical,
                            crate::manager::events::category::SRT,
                            format!("SRT input '{input_id}' bonding endpoint rejected: {e}"),
                            &flow_id,
                            serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
                        );
                        return;
                    }
                }
            }
        }
        // Build ingress transcoder once per connection cycle so PMT discovery
        // state and codec buffers persist across reconnects within a single
        // configured flow.
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
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

        let result = if config.bonding.is_some() {
            srt_input_bonded_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder).await
        } else if config.redundancy.is_some() {
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
    let mut listener = match bind_srt_listener_for_input(&config).await {
        Ok(l) => l,
        Err(e) => {
            use crate::manager::events::{BindProto, BindScope};
            let addr = config.local_addr.as_deref().unwrap_or("auto");
            let scope = BindScope::flow(flow_id);
            if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                events.emit_port_conflict("SRT input listener", addr, BindProto::Udp, scope, &e);
            } else {
                events.emit_bind_failed("SRT input listener", addr, BindProto::Udp, scope, &e);
            }
            return Err(e);
        }
    };
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
                            upstream_seq: None,
                            upstream_leg_id: None,
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

    // Bind persistent listeners for any legs in listener mode.
    // Both stay bound across outer reconnect cycles.
    let mut listener_leg1 = if config.mode == SrtMode::Listener {
        match bind_srt_listener_for_input(&config).await {
            Ok(l) => Some(l),
            Err(e) => {
                use crate::manager::events::{BindProto, BindScope};
                let addr = config.local_addr.as_deref().unwrap_or("auto");
                let scope = BindScope::flow(flow_id);
                if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                    events.emit_port_conflict("SRT input listener leg 1", addr, BindProto::Udp, scope, &e);
                } else {
                    events.emit_bind_failed("SRT input listener leg 1", addr, BindProto::Udp, scope, &e);
                }
                return Err(e);
            }
        }
    } else {
        None
    };
    let mut listener_leg2 = if redundancy.mode == SrtMode::Listener {
        match bind_srt_listener_for_redundancy(redundancy).await {
            Ok(l) => Some(l),
            Err(e) => {
                use crate::manager::events::{BindProto, BindScope};
                let addr = redundancy.local_addr.as_deref().unwrap_or("auto");
                let scope = BindScope::flow(flow_id);
                if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                    events.emit_port_conflict("SRT input listener leg 2", addr, BindProto::Udp, scope, &e);
                } else {
                    events.emit_bind_failed("SRT input listener leg 2", addr, BindProto::Udp, scope, &e);
                }
                return Err(e);
            }
        }
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

        let mut merger = HitlessMerger::new();
        let mut failover = RawTsFailover::new();
        let mut prev_active_leg = ActiveLeg::None;
        let mut leg1_alive = true;
        let mut socket_leg2: Option<Arc<srt_transport::SrtSocket>> = None;
        let mut leg2_alive = false;
        let mut format = SrtPayloadFormat::Unknown;
        let mut raw_ts_seq_leg1: u16 = 0;
        let mut raw_ts_seq_leg2: u16 = 0;
        let mut raw_ts_timestamp: u32 = 0;

        // Leg-2 connect future, pinned so we can poll it inside select! and
        // only complete once per reconnect cycle. Bounded by
        // INITIAL_LEG2_CONNECT_BUDGET so a dead or slow leg-2 peer never
        // gates ingest on a healthy leg-1. While this is pending the
        // select! below is already reading from leg-1.
        let leg2_connect_fut = {
            let events = events.clone();
            let flow_id = flow_id.to_string();
            let redundancy = redundancy.clone();
            let cancel = cancel.clone();
            let mut listener_leg2_mut = listener_leg2.take();
            async move {
                let result: (Option<Arc<srt_transport::SrtSocket>>, Option<srt_transport::SrtListener>) = if let Some(ref mut listener) = listener_leg2_mut {
                    let r = tokio::time::timeout(
                        INITIAL_LEG2_CONNECT_BUDGET,
                        accept_srt_connection(listener, &cancel),
                    ).await;
                    match r {
                        Ok(Ok(s)) => (Some(s), listener_leg2_mut),
                        Ok(Err(_)) if cancel.is_cancelled() => (None, listener_leg2_mut),
                        Ok(Err(e)) => {
                            tracing::warn!("SRT input leg2 accept failed: {e}, running single-leg");
                            (None, listener_leg2_mut)
                        }
                        Err(_) => {
                            tracing::warn!(
                                "SRT input leg2 did not come up within {:?} (accept timed out), running single-leg",
                                INITIAL_LEG2_CONNECT_BUDGET,
                            );
                            events.emit_flow_with_details(
                                EventSeverity::Warning, category::SRT,
                                format!("SRT input 2022-7 leg 2 did not connect within {:?}; running single-leg", INITIAL_LEG2_CONNECT_BUDGET),
                                &flow_id,
                                serde_json::json!({
                                    "mode": format!("{:?}", redundancy.mode),
                                    "leg": 2, "reason": "accept timed out",
                                    "budget_ms": INITIAL_LEG2_CONNECT_BUDGET.as_millis(),
                                }),
                            );
                            (None, listener_leg2_mut)
                        }
                    }
                } else {
                    match tokio::time::timeout(
                        INITIAL_LEG2_CONNECT_BUDGET,
                        connect_srt_redundancy_leg(&redundancy, &cancel),
                    ).await {
                        Ok(Ok(s)) => (Some(s), None),
                        Ok(Err(_)) if cancel.is_cancelled() => (None, None),
                        Ok(Err(e)) => {
                            tracing::warn!("SRT input leg2 connection failed: {e}, running single-leg");
                            (None, None)
                        }
                        Err(_) => {
                            tracing::warn!(
                                "SRT input leg2 did not come up within {:?} (caller connect retry budget exhausted), running single-leg",
                                INITIAL_LEG2_CONNECT_BUDGET,
                            );
                            events.emit_flow_with_details(
                                EventSeverity::Warning, category::SRT,
                                format!("SRT input 2022-7 leg 2 did not connect within {:?}; running single-leg", INITIAL_LEG2_CONNECT_BUDGET),
                                &flow_id,
                                serde_json::json!({
                                    "mode": format!("{:?}", redundancy.mode),
                                    "leg": 2, "reason": "caller connect retry budget exhausted",
                                    "budget_ms": INITIAL_LEG2_CONNECT_BUDGET.as_millis(),
                                }),
                            );
                            (None, None)
                        }
                    }
                };
                if result.0.is_some() {
                    tracing::info!(
                        "SRT input leg2 connected: mode={:?} local={}",
                        redundancy.mode,
                        redundancy.local_addr.as_deref().unwrap_or("auto")
                    );
                }
                result
            }
        };
        tokio::pin!(leg2_connect_fut);
        let mut leg2_done = false;

        loop {
            // Leg-2 recv future: only polls when we have a live leg-2
            // socket. `std::future::pending()` never resolves, so the
            // branch stays dormant until leg-2 arrives.
            let leg2_recv = async {
                match &socket_leg2 {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            };

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
                (sock, returned_listener) = &mut leg2_connect_fut, if !leg2_done => {
                    leg2_done = true;
                    // Give the listener back so next outer-loop iteration can reuse it.
                    if returned_listener.is_some() {
                        listener_leg2 = returned_listener;
                    }
                    if let Some(s) = sock {
                        socket_leg2 = Some(s);
                        leg2_alive = true;
                    }
                    continue;
                }
                result = socket_leg1.recv(), if leg1_alive => {
                    match result {
                        Ok(data) => {
                            if format == SrtPayloadFormat::Unknown {
                                format = detect_format(&data);
                                log_detected_format_redundant(format, &events, flow_id);
                            }
                            process_redundant_packet(
                                data,
                                ActiveLeg::Leg1,
                                format,
                                &mut raw_ts_seq_leg1,
                                &mut raw_ts_timestamp,
                                &mut merger,
                                &mut failover,
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
                result = leg2_recv, if leg2_alive => {
                    match result {
                        Ok(data) => {
                            if format == SrtPayloadFormat::Unknown {
                                format = detect_format(&data);
                                log_detected_format_redundant(format, &events, flow_id);
                            }
                            process_redundant_packet(
                                data,
                                ActiveLeg::Leg2,
                                format,
                                &mut raw_ts_seq_leg2,
                                &mut raw_ts_timestamp,
                                &mut merger,
                                &mut failover,
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

/// Variant of `log_detected_format` for the redundant-input loop. When raw
/// TS lands on a 2022-7-configured input, real hitless dedup is impossible
/// (no shared transport-layer seq across legs) — the runtime falls back to
/// active-leg protection-switching. Emit a Warning event so operators
/// understand what mode they're actually getting.
fn log_detected_format_redundant(
    format: SrtPayloadFormat,
    events: &EventSender,
    flow_id: &str,
) {
    log_detected_format(format);
    if matches!(format, SrtPayloadFormat::RawTs | SrtPayloadFormat::Unknown) {
        tracing::warn!(
            "SRT redundant input: raw TS has no shared cross-leg seq — running active-leg failover, NOT hitless dedup. \
             Configure the publisher to send RTP/TS (SMPTE ST 2022-2) for true 2022-7 hitless behaviour."
        );
        events.emit_flow_with_details(
            EventSeverity::Warning,
            category::REDUNDANCY,
            "SRT redundancy on raw TS: running active-leg failover, not hitless dedup",
            flow_id,
            serde_json::json!({
                "error_code": "redundancy_failover_mode",
                "format": "raw_ts",
                "fix_hint": "Configure the publisher to send RTP/TS for true SMPTE 2022-7 hitless behaviour",
            }),
        );
    }
}

/// Stall threshold for the raw-TS active-leg failover path. If the active
/// output leg is silent for longer than this, the next packet from the
/// standby leg promotes it. Sized large enough to absorb normal jitter
/// (well above the SRT TSBPD release cadence) while still failing over
/// inside an operator-noticeable window.
const RAW_TS_FAILOVER_STALL: Duration = Duration::from_millis(50);

/// State for the raw-TS-over-SRT redundancy active-leg failover path.
///
/// Raw TS over SRT does not carry a transport-layer sequence number that
/// is shared across legs (each SRT connection numbers its packets
/// independently from its own ISN, and the TS payload itself only has a
/// 4-bit per-PID continuity counter). The synthetic per-leg counters that
/// the legacy code fed into `HitlessMerger` violated the merger's
/// precondition (shared seq across legs) and produced random dedup
/// collisions plus continuous active-leg flapping. The merger is the
/// right primitive when the legs share a real seq (RTP/TS); for raw TS
/// the honest answer is protection-switching, not hitless dedup.
///
/// Behaviour: forward packets only from `active`; switch to the other leg
/// when `active`'s last arrival is older than [`RAW_TS_FAILOVER_STALL`].
/// First packet ever seen sets `active`. The unused leg's bytes are
/// silently dropped — they would be near-duplicates of the active leg's
/// bytes (the camera/encoder pushes the same TS to both ports), and we
/// have no transport-layer key to dedup them.
struct RawTsFailover {
    active: ActiveLeg,
    last_leg1: Option<std::time::Instant>,
    last_leg2: Option<std::time::Instant>,
}

impl RawTsFailover {
    fn new() -> Self {
        Self {
            active: ActiveLeg::None,
            last_leg1: None,
            last_leg2: None,
        }
    }

    /// Returns `true` if this packet should be forwarded; updates state.
    fn admit(&mut self, leg: ActiveLeg) -> bool {
        let now = std::time::Instant::now();
        match leg {
            ActiveLeg::Leg1 => self.last_leg1 = Some(now),
            ActiveLeg::Leg2 => self.last_leg2 = Some(now),
            ActiveLeg::None => return false,
        }

        if self.active == ActiveLeg::None {
            self.active = leg;
            return true;
        }

        if self.active == leg {
            return true;
        }

        // Standby-leg packet — only promote on stall of the active leg.
        let active_last = match self.active {
            ActiveLeg::Leg1 => self.last_leg1,
            ActiveLeg::Leg2 => self.last_leg2,
            ActiveLeg::None => None,
        };
        let stalled = active_last
            .map(|t| now.duration_since(t) > RAW_TS_FAILOVER_STALL)
            .unwrap_or(true);
        if stalled {
            self.active = leg;
            true
        } else {
            false
        }
    }
}

/// Process a single packet from a redundancy leg.
///
/// Dispatches by detected payload format:
/// - **RTP/TS**: feeds the real RTP sequence number through
///   [`HitlessMerger`] for true SMPTE 2022-7 dedup + gap-fill.
/// - **Raw TS / Unknown**: routes through [`RawTsFailover`] for
///   active-leg protection-switching. Real dedup is impossible without a
///   shared seq across legs.
fn process_redundant_packet(
    data: Bytes,
    leg: ActiveLeg,
    format: SrtPayloadFormat,
    raw_ts_seq: &mut u16,
    raw_ts_timestamp: &mut u32,
    merger: &mut HitlessMerger,
    failover: &mut RawTsFailover,
    prev_active_leg: &mut ActiveLeg,
    stats: &Arc<FlowStatsAccumulator>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    transcoder: &mut Option<InputTranscoder>,
) {
    let (seq, ts, is_raw, accepted_leg) = match format {
        SrtPayloadFormat::RtpTs => {
            if !is_likely_rtp(&data) {
                return;
            }
            let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
            let ts = parse_rtp_timestamp(&data).unwrap_or(0);
            match merger.try_merge(seq, leg) {
                Some(chosen_leg) => (seq, ts, false, chosen_leg),
                None => return,
            }
        }
        SrtPayloadFormat::RawTs | SrtPayloadFormat::Unknown => {
            if !failover.admit(leg) {
                return;
            }
            // Synthetic seq advances independently per leg; useful only as a
            // monotonic stamp on the published `RtpPacket` for downstream
            // bookkeeping. It is no longer used for dedup.
            let seq = *raw_ts_seq;
            *raw_ts_seq = raw_ts_seq.wrapping_add(1);
            let ts_pkts = (data.len() / TS_PACKET_SIZE) as u32;
            *raw_ts_timestamp = raw_ts_timestamp.wrapping_add(ts_pkts * 188 * 8);
            (seq, *raw_ts_timestamp, true, leg)
        }
    };

    if *prev_active_leg != ActiveLeg::None && accepted_leg != *prev_active_leg {
        stats
            .redundancy_switches
            .fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            "SRT input redundancy switch: {:?} -> {:?} at seq {}",
            prev_active_leg,
            accepted_leg,
            seq
        );
    }
    *prev_active_leg = accepted_leg;

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
        upstream_seq: None,
        upstream_leg_id: None,
    };

    publish_input_packet(transcoder, broadcast_tx, packet);
}

// ---------------------------------------------------------------------------
// Native libsrt SRT bonding (socket groups)
// ---------------------------------------------------------------------------

/// Entry point for an SRT input whose config has a `bonding` block.
///
/// Dispatches between caller-side bonding (the edge initiates a group
/// connection to N peer endpoints) and listener-side bonding (the edge
/// listens on one bind address with `SRTO_GROUPCONNECT=true` and accepts
/// bonded callers).
async fn srt_input_bonded_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => {
            srt_input_bonded_listener_loop(
                config, broadcast_tx, stats, cancel, events, flow_id, transcoder,
            )
            .await
        }
        _ => {
            srt_input_bonded_caller_loop(
                config, broadcast_tx, stats, cancel, events, flow_id, transcoder,
            )
            .await
        }
    }
}

async fn srt_input_bonded_caller_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    let bond = config.bonding.as_ref().expect("bonding must be set").clone();
    let mut format = SrtPayloadFormat::Unknown;
    let mut last_seq: Option<u16> = None;
    let mut raw_ts_seq_counter: u16 = 0;
    let mut raw_ts_timestamp: u32 = 0;

    loop {
        let params = SrtConnectionParams::from(&config);
        let group = match connect_srt_group(&params, &bond, &cancel).await {
            Ok(g) => g,
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                tracing::error!("SRT bonded input connect failed: {e}");
                events.emit_flow_with_details(
                    EventSeverity::Critical,
                    category::SRT,
                    format!("SRT bonded input connection failed: {e}"),
                    flow_id,
                    serde_json::json!({
                        "mode": "caller-bonded",
                        "bonding_mode": format!("{:?}", bond.mode),
                        "endpoints": bond.endpoints.iter().map(|e| &e.addr).collect::<Vec<_>>(),
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
        };

        tracing::info!(
            "SRT bonded input connected: mode=caller bonding={:?} endpoints={}",
            bond.mode,
            bond.endpoints.len(),
        );
        events.emit_flow_with_details(
            EventSeverity::Info,
            category::SRT,
            "SRT bonded input connected (caller)",
            flow_id,
            serde_json::json!({
                "mode": "caller-bonded",
                "bonding_mode": format!("{:?}", bond.mode),
                "endpoints": bond.endpoints.iter().map(|e| &e.addr).collect::<Vec<_>>(),
            }),
        );

        let poller_cancel = cancel.child_token();
        spawn_srt_group_stats_poller(
            group.clone(),
            bond.mode,
            stats.input_srt_bonding_stats_cache.clone(),
            poller_cancel.clone(),
        );

        let disconnected = srt_bonded_group_recv_loop(
            &group,
            &broadcast_tx,
            &stats,
            &cancel,
            &mut format,
            &mut last_seq,
            &mut raw_ts_seq_counter,
            &mut raw_ts_timestamp,
            transcoder,
        )
        .await?;
        poller_cancel.cancel();
        let _ = group.close().await;

        if !disconnected {
            break;
        }

        events.emit_flow_with_details(
            EventSeverity::Warning,
            category::SRT,
            "SRT bonded input disconnected, reconnecting",
            flow_id,
            serde_json::json!({ "mode": "caller-bonded" }),
        );
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }

    Ok(())
}

async fn srt_input_bonded_listener_loop(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    let bond = config.bonding.as_ref().expect("bonding must be set").clone();
    let mut listener = bind_srt_listener_for_bonded_input(&config).await?;
    let mut format = SrtPayloadFormat::Unknown;
    let mut last_seq: Option<u16> = None;
    let mut raw_ts_seq_counter: u16 = 0;
    let mut raw_ts_timestamp: u32 = 0;

    loop {
        let socket = match accept_srt_connection(&mut listener, &cancel).await {
            Ok(s) => s,
            Err(_) if cancel.is_cancelled() => {
                let _ = listener.close().await;
                return Ok(());
            }
            Err(e) => {
                tracing::error!("SRT bonded input accept failed: {e}");
                events.emit_flow_with_details(
                    EventSeverity::Critical,
                    category::SRT,
                    format!("SRT bonded input accept failed: {e}"),
                    flow_id,
                    serde_json::json!({
                        "mode": "listener-bonded",
                        "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                        "error": e.to_string(),
                    }),
                );
                let _ = listener.close().await;
                return Err(e);
            }
        };

        tracing::info!(
            "SRT bonded input connected: mode=listener local={}",
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_flow_with_details(
            EventSeverity::Info,
            category::SRT,
            "SRT bonded input connected (listener)",
            flow_id,
            serde_json::json!({
                "mode": "listener-bonded",
                "bonding_mode": format!("{:?}", bond.mode),
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );

        let poller_cancel = cancel.child_token();
        spawn_srt_socket_group_stats_poller(
            socket.clone(),
            bond.mode,
            stats.input_srt_bonding_stats_cache.clone(),
            poller_cancel.clone(),
        );

        let disconnected = srt_input_recv_loop(
            &socket,
            &broadcast_tx,
            &stats,
            &cancel,
            &mut format,
            &mut last_seq,
            &mut raw_ts_seq_counter,
            &mut raw_ts_timestamp,
            transcoder,
        )
        .await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            let _ = listener.close().await;
            return Ok(());
        }

        events.emit_flow_with_details(
            EventSeverity::Warning,
            category::SRT,
            "SRT bonded input disconnected, reconnecting",
            flow_id,
            serde_json::json!({ "mode": "listener-bonded" }),
        );
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = listener.close().await;
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

/// Receive loop for a bonded caller-side SRT group. Mirrors
/// [`srt_input_recv_loop`] but reads from [`srt_transport::SrtGroup`]
/// instead of an individual socket.
#[allow(clippy::too_many_arguments)]
async fn srt_bonded_group_recv_loop(
    group: &Arc<srt_transport::SrtGroup>,
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
            _ = cancel.cancelled() => return Ok(false),
            result = group.recv() => {
                match result {
                    Ok(data) => {
                        if *format == SrtPayloadFormat::Unknown {
                            *format = detect_format(&data);
                            if *format == SrtPayloadFormat::Unknown {
                                *format = SrtPayloadFormat::RawTs;
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
                                *raw_ts_timestamp =
                                    raw_ts_timestamp.wrapping_add(ts_pkts * 188 * 8);
                                (seq, *raw_ts_timestamp, true)
                            }
                        };

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
                            upstream_seq: None,
                            upstream_leg_id: None,
                        };
                        publish_input_packet(transcoder, broadcast_tx, packet);
                    }
                    Err(_) => {
                        tracing::warn!("SRT bonded input connection lost, will reconnect");
                        return Ok(true);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_first_packet_sets_active() {
        let mut f = RawTsFailover::new();
        assert!(f.admit(ActiveLeg::Leg1));
        assert_eq!(f.active, ActiveLeg::Leg1);
    }

    #[test]
    fn failover_active_leg_admitted_standby_dropped() {
        let mut f = RawTsFailover::new();
        f.admit(ActiveLeg::Leg1);
        // Active leg keeps flowing.
        assert!(f.admit(ActiveLeg::Leg1));
        // Standby leg dropped while active is healthy.
        assert!(!f.admit(ActiveLeg::Leg2));
        // Active stays leg1.
        assert_eq!(f.active, ActiveLeg::Leg1);
    }

    #[test]
    fn failover_promotes_standby_after_stall() {
        let mut f = RawTsFailover::new();
        f.admit(ActiveLeg::Leg1);
        // Backdate leg1's last arrival past the stall threshold.
        f.last_leg1 = Some(
            std::time::Instant::now() - (RAW_TS_FAILOVER_STALL + Duration::from_millis(10)),
        );
        assert!(f.admit(ActiveLeg::Leg2));
        assert_eq!(f.active, ActiveLeg::Leg2);
    }

    #[test]
    fn failover_does_not_dedup_within_active_leg() {
        // Even a "duplicate" stream from the active leg passes — the
        // primitive is failover, not dedup. If the publisher itself
        // double-sends, that's not the failover's problem.
        let mut f = RawTsFailover::new();
        f.admit(ActiveLeg::Leg1);
        for _ in 0..1000 {
            assert!(f.admit(ActiveLeg::Leg1));
        }
    }
}
