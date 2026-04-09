// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use srt_transport::SrtSocket;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{SrtMode, SrtOutputConfig};
use crate::manager::events::{EventSender, EventSeverity};
use crate::srt::connection::{
    accept_srt_connection, bind_srt_listener_for_output, bind_srt_listener_for_redundancy,
    connect_srt_output, connect_srt_redundancy_leg, spawn_srt_stats_poller,
};
use crate::stats::collector::OutputStatsAccumulator;

use super::audio_302m::{S302mOutputPipeline, SRT_TS_DATAGRAM_BYTES};
use super::audio_transcode::InputFormat;
use super::packet::RtpPacket;
use super::ts_program_filter::TsProgramFilter;

/// Maximum SRT live-mode payload bytes per `srt_sendmsg` call.
///
/// SRT live mode delivers each `srt_sendmsg` as one logical message that the
/// receiver reassembles before returning from `srt_recvmsg`. The libsrt
/// receiver's default read buffer is 1456 bytes (one MSS minus headers); any
/// reassembled message larger than that is silently truncated. To stay
/// interoperable with `srt-live-transmit`, ffmpeg's libsrt input, hardware
/// decoders, and any other libsrt-based consumer, every outbound buffer is
/// chunked to ≤1316 bytes (= 7 × 188-byte MPEG-TS packets — the libsrt
/// recommended live-mode default).
///
/// UDP/RTP-sourced broadcast packets are typically already 1316 bytes and
/// pass through unchanged. RTMP/RTSP/WebRTC-sourced packets can be much
/// larger (a TsMuxer-bundled I-frame can be tens of KB) and need to be
/// split before they hit the SRT socket.
const SRT_LIVE_MAX_MSG_BYTES: usize = SRT_TS_DATAGRAM_BYTES;

/// Split `data` into ≤[`SRT_LIVE_MAX_MSG_BYTES`] chunks for SRT live-mode
/// transmission. Pure synchronous helper used by [`srt_send_chunked`] and
/// the unit tests so chunking behaviour can be verified without a real SRT
/// socket.
fn srt_chunk_for_live(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.chunks(SRT_LIVE_MAX_MSG_BYTES)
}

/// Send `data` over an SRT live-mode socket, splitting into ≤[`SRT_LIVE_MAX_MSG_BYTES`]
/// chunks if needed. Returns the total number of bytes accepted by the socket
/// or the first send error encountered.
async fn srt_send_chunked(socket: &Arc<SrtSocket>, data: &[u8]) -> anyhow::Result<usize> {
    if data.is_empty() {
        return Ok(0);
    }
    let mut total = 0usize;
    for chunk in srt_chunk_for_live(data) {
        let n = socket
            .send(chunk)
            .await
            .map_err(|e| anyhow::anyhow!("SRT send: {e}"))?;
        total += n;
    }
    Ok(total)
}

/// Spawn a task that subscribes to the broadcast channel and sends
/// RTP packets over an SRT connection. If redundancy is configured,
/// packets are duplicated to both SRT legs via SrtDuplicator.
pub fn spawn_srt_output(
    config: SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
) -> JoinHandle<()> {
    let broadcast_tx = broadcast_tx.clone();

    tokio::spawn(async move {
        let result = if config.redundancy.is_some() {
            // Redundant SRT output does not support 302M mode, so the
            // compressed-audio bridge is not applicable here.
            srt_output_redundant_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id).await
        } else {
            srt_output_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id, input_format, compressed_audio_input).await
        };
        if let Err(e) = result {
            tracing::error!("SRT output '{}' exited with error: {e}", config.id);
        }
    })
}

// ---------------------------------------------------------------------------
// Single-leg SRT output (no redundancy)
// ---------------------------------------------------------------------------

/// Core send loop for a single-leg SRT output (no redundancy).
///
/// Dispatches to listener-specific or caller-specific loops based on the
/// SRT mode. Listener mode binds once and re-accepts on disconnect; caller
/// mode reconnects with exponential back-off.
#[allow(clippy::too_many_arguments)]
async fn srt_output_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => srt_output_listener_loop(config, broadcast_tx, stats, cancel, events, flow_id, input_format, compressed_audio_input).await,
        _ => srt_output_caller_loop(config, broadcast_tx, stats, cancel, events, flow_id, input_format, compressed_audio_input).await,
    }
}

/// Listener-mode output: binds the listener once and re-accepts new callers
/// on disconnect, avoiding the port-rebind problem.
#[allow(clippy::too_many_arguments)]
async fn srt_output_listener_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
) -> anyhow::Result<()> {
    let mut listener = bind_srt_listener_for_output(config).await?;
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (listener): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });

    loop {
        let socket = match accept_srt_connection(&mut listener, &cancel).await {
            Ok(s) => s,
            Err(_) if cancel.is_cancelled() => {
                tracing::info!(
                    "SRT output '{}' stopping (cancelled during accept)",
                    config.id
                );
                let _ = listener.close().await;
                return Ok(());
            }
            Err(e) => {
                tracing::error!("SRT output '{}' accept failed: {e}", config.id);
                let _ = listener.close().await;
                return Err(e);
            }
        };

        tracing::info!(
            "SRT output '{}' connected: mode=listener local={}",
            config.id,
            config.local_addr,
        );
        events.emit_flow(EventSeverity::Info, "srt", format!("SRT output '{}' connected", config.id), flow_id);

        // Validate the connection is alive before committing to it.
        // Under high delay/loss, the listener may accept a stale connection
        // where the caller's handshake timed out and retried with a new socket ID.
        // Send a few packets and check for ACK responses within 5 seconds.
        let is_alive = {
            let mut test_rx = broadcast_tx.subscribe();
            let mut sent = 0u32;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            let alive = loop {
                tokio::select! {
                    _ = cancel.cancelled() => break false,
                    _ = tokio::time::sleep_until(deadline) => {
                        // Check if we received any ACKs
                        let srt_stats = socket.stats().await;
                        if srt_stats.pkt_recv_ack_total > 0 {
                            break true;
                        }
                        tracing::warn!(
                            "SRT output '{}' no ACK received after 3s, connection likely stale",
                            config.id
                        );
                        break false;
                    }
                    result = test_rx.recv() => {
                        if let Ok(packet) = result {
                            if srt_send_chunked(&socket, &packet.data).await.is_err() {
                                break false;
                            }
                            sent += 1;
                            // After sending some packets, check for ACKs periodically
                            if sent % 100 == 0 {
                                let srt_stats = socket.stats().await;
                                if srt_stats.pkt_recv_ack_total > 0 {
                                    break true;
                                }
                            }
                        }
                    }
                }
            };
            alive
        };

        if !is_alive {
            tracing::info!(
                "SRT output '{}' stale connection detected, re-accepting...",
                config.id
            );
            events.emit_flow(EventSeverity::Warning, "srt", format!("SRT output '{}' stale connection detected", config.id), flow_id);
            let _ = socket.close().await;
            continue;
        }

        // Subscribe AFTER the peer connects so no stale packets accumulate
        // during the accept() wait. The receiver is dropped and re-created
        // on each reconnect cycle.
        let mut rx = broadcast_tx.subscribe();

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.srt_stats_cache.clone(), poller_cancel.clone());

        let disconnected = srt_output_forward_loop(config, &mut rx, &stats, &cancel, &socket, &mut program_filter, input_format, compressed_audio_input).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            let _ = listener.close().await;
            return Ok(());
        }

        // Brief delay before re-accepting
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping during reconnect delay", config.id);
                let _ = listener.close().await;
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        events.emit_flow(EventSeverity::Warning, "srt", format!("SRT output '{}' disconnected", config.id), flow_id);
        tracing::info!("SRT output '{}' waiting for caller to reconnect...", config.id);
    }
}

/// Caller-mode output: reconnects with exponential back-off on disconnect.
#[allow(clippy::too_many_arguments)]
async fn srt_output_caller_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
) -> anyhow::Result<()> {
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (caller): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    loop {
        let socket = match connect_srt_output(config, &cancel).await {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    tracing::info!(
                        "SRT output '{}' stopping (cancelled during connect)",
                        config.id
                    );
                    return Ok(());
                }
                tracing::error!("SRT output '{}' connection failed: {e}", config.id);
                events.emit_flow(EventSeverity::Critical, "srt", format!("SRT output '{}' connection failed: {e}", config.id), flow_id);
                return Err(e);
            }
        };

        tracing::info!(
            "SRT output '{}' connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr,
        );
        events.emit_flow(EventSeverity::Info, "srt", format!("SRT output '{}' connected", config.id), flow_id);

        // Subscribe AFTER the peer connects so no stale packets accumulate
        let mut rx = broadcast_tx.subscribe();

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.srt_stats_cache.clone(), poller_cancel.clone());

        let disconnected = srt_output_forward_loop(config, &mut rx, &stats, &cancel, &socket, &mut program_filter, input_format, compressed_audio_input).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            break;
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping during reconnect delay", config.id);
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        events.emit_flow(EventSeverity::Warning, "srt", format!("SRT output '{}' disconnected", config.id), flow_id);
        tracing::info!("SRT output '{}' attempting reconnection...", config.id);
    }

    Ok(())
}

/// Inner packet forwarding loop shared by listener and caller output modes.
///
/// Bridges the broadcast receiver to the SRT socket via a bounded mpsc channel
/// and a dedicated Tokio send task. Returns `Ok(true)` if the connection was
/// lost (caller should reconnect), `Ok(false)` if the broadcast channel closed
/// (flow is shutting down).
///
/// `program_filter` is owned by the caller so its PAT/PMT state survives
/// reconnects.
#[allow(clippy::too_many_arguments)]
async fn srt_output_forward_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: &Arc<OutputStatsAccumulator>,
    cancel: &CancellationToken,
    socket: &Arc<SrtSocket>,
    program_filter: &mut Option<TsProgramFilter>,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
) -> anyhow::Result<bool> {
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    let send_socket = socket.clone();
    let send_stats = stats.clone();
    let output_id = config.id.clone();
    let send_handle = tokio::spawn(async move {
        while let Some(data) = send_rx.recv().await {
            // Chunk every outbound buffer into ≤SRT_LIVE_MAX_MSG_BYTES messages
            // before handing them to the SRT socket. Protects every upstream
            // code path (passthrough TS, program-filtered TS, 302M datagram,
            // and the RTMP/RTSP/WebRTC TS-mux paths that bundle whole frames)
            // from emitting oversized SRT live-mode messages that would be
            // silently truncated by libsrt receivers.
            match srt_send_chunked(&send_socket, &data).await {
                Ok(sent) => {
                    // Count one logical packet per upstream send call (the
                    // chunking is invisible to upstream stats consumers).
                    send_stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    send_stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!("SRT output '{}' send error: {e}", output_id);
                    break;
                }
            }
        }
    });

    // Build the optional SMPTE 302M output pipeline. Active when:
    //   - `transport_mode == "audio_302m"`
    //   - upstream input is an audio essence (`input_format` is Some) OR
    //     the input is a TS-bearing source carrying AAC
    //     (`compressed_audio_input == true`).
    //
    // When active, the program filter is bypassed (the brief explicitly
    // rejects `program_number` for the 302M audio mode) and incoming RTP
    // audio packets are routed through Transcode → 302M → TsMuxer →
    // 1316-byte SRT datagrams. For the compressed-audio-input case, an
    // in-line TsDemuxer + AacDecoder runs ahead of the pipeline, which
    // is itself built lazily on the first decoded AAC frame (once the
    // sample rate / channel count is known).
    let is_302m_mode = matches!(config.transport_mode.as_deref(), Some("audio_302m"));
    let mut audio_302m: Option<S302mOutputPipeline> = None;
    if is_302m_mode && !compressed_audio_input {
        match input_format {
            Some(input) => {
                // Default to L24 / preserve channel count (rounded up to even)
                // / 4 ms packet time. Hardcoded for chunk 6; chunks 9 will let
                // the operator override via UI.
                let out_channels = match input.channels {
                    1 => 2,
                    n if n % 2 == 0 && n <= 8 => n,
                    n if n > 8 => 8,
                    _ => input.channels + 1,
                };
                match S302mOutputPipeline::new(input, out_channels, 24, 4_000) {
                    Ok(p) => {
                        tracing::info!(
                            "SRT output '{}' transport_mode=audio_302m: \
                             {}Hz/{}bit/{}ch -> 48000/24/{} 302M-in-MPEG-TS",
                            config.id,
                            input.sample_rate,
                            input.bit_depth.as_u8(),
                            input.channels,
                            out_channels,
                        );
                        audio_302m = Some(p);
                    }
                    Err(e) => {
                        tracing::error!(
                            "SRT output '{}' could not build 302M pipeline: {e}; \
                             falling back to passthrough TS",
                            config.id
                        );
                    }
                }
            }
            None => {
                tracing::warn!(
                    "SRT output '{}' transport_mode=audio_302m but upstream input \
                     is not audio; falling back to passthrough TS",
                    config.id
                );
            }
        }
    }

    // Compressed-audio bridge state. Only initialised when the 302M mode
    // is selected AND the upstream input is a TS-bearing source. All three
    // pieces are built lazily: the demuxer is created eagerly so it can
    // start parsing the PAT/PMT/ADTS headers; the decoder and pipeline
    // wait for the first AAC frame so they can pick up the sample rate
    // and channel count from `cached_aac_config()`.
    let mut ts_demuxer_302m: Option<crate::engine::ts_demux::TsDemuxer> =
        if is_302m_mode && compressed_audio_input {
            Some(crate::engine::ts_demux::TsDemuxer::new(None))
        } else {
            None
        };
    let mut aac_decoder_302m: Option<crate::engine::audio_decode::AacDecoder> = None;
    let mut logged_302m_init_failure = false;
    let mut logged_302m_decode_error: Option<String> = None;
    // Lock-free decode counters shared with the per-output stats
    // accumulator. Registered lazily the first time the decoder is built.
    let decode_stats_302m =
        std::sync::Arc::new(crate::engine::audio_decode::DecodeStats::new());
    let mut decode_stats_302m_registered = false;

    let mut filter_scratch: Vec<u8> = Vec::new();
    let connection_lost = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping (cancelled)", config.id);
                drop(send_tx);
                let _ = send_handle.await;
                return Ok(false);
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Compressed-audio bridge for the 302M mode takes
                        // precedence over both the PCM 302M path and the
                        // TS program filter. Drives a TsDemuxer + lazy
                        // AacDecoder + lazy S302mOutputPipeline and then
                        // drains the pipeline's ready datagrams into the
                        // same send queue as the PCM path.
                        if let Some(demuxer) = ts_demuxer_302m.as_mut() {
                            srt_run_compressed_302m_step(
                                &packet,
                                demuxer,
                                &mut aac_decoder_302m,
                                &mut audio_302m,
                                config,
                                &mut logged_302m_init_failure,
                                &mut logged_302m_decode_error,
                                &decode_stats_302m,
                            );
                            // Bind the decode counter handle to the output
                            // stats accumulator once, lazily — we need the
                            // decoder's sample rate / channel count to
                            // label the stage for the UI.
                            if !decode_stats_302m_registered {
                                if let Some(dec) = aac_decoder_302m.as_ref() {
                                    stats.set_decode_stats(
                                        decode_stats_302m.clone(),
                                        dec.codec_name(),
                                        dec.sample_rate(),
                                        dec.channels(),
                                    );
                                    decode_stats_302m_registered = true;
                                }
                            }
                            if let Some(pipeline) = audio_302m.as_mut() {
                                for datagram in pipeline.take_ready_datagrams() {
                                    match send_tx.try_send(datagram) {
                                        Ok(()) => {}
                                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                            stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                            tracing::warn!(
                                                "SRT output '{}' connection lost (302M compressed), will reconnect",
                                                config.id
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        // 302M path takes precedence over the TS program filter
                        // (the modes are mutually exclusive — see validator).
                        if let Some(pipeline) = audio_302m.as_mut() {
                            pipeline.process(&packet);
                            for datagram in pipeline.take_ready_datagrams() {
                                match send_tx.try_send(datagram) {
                                    Ok(()) => {}
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                        tracing::warn!(
                                            "SRT output '{}' connection lost (302M), will reconnect",
                                            config.id
                                        );
                                        break;
                                    }
                                }
                            }
                            continue;
                        }
                        // Apply program filter if configured. Skip the packet
                        // entirely when the filter eats it (no target-program
                        // bytes in this datagram).
                        let payload = if let Some(filter) = program_filter.as_mut() {
                            match filter.filter_packet(&packet, &mut filter_scratch) {
                                Some(b) => b,
                                None => continue,
                            }
                        } else {
                            packet.data
                        };
                        match send_tx.try_send(payload) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                tracing::warn!(
                                    "SRT output '{}' connection lost, will reconnect",
                                    config.id
                                );
                                break true;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "SRT output '{}' lagged, dropped {n} packets",
                            config.id
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            "SRT output '{}' broadcast channel closed",
                            config.id
                        );
                        drop(send_tx);
                        let _ = send_handle.await;
                        return Ok(false);
                    }
                }
            }
        }
    };

    drop(send_tx);
    let _ = send_handle.await;
    Ok(connection_lost)
}

// ---------------------------------------------------------------------------
// Dual-leg SRT output with SMPTE 2022-7 duplication
// ---------------------------------------------------------------------------

/// Core send loop for dual-leg SRT output with SMPTE 2022-7 packet duplication.
///
/// Connects two independent SRT legs (leg 1 from the main config, leg 2 from
/// the redundancy config). Each leg gets its own dedicated Tokio send task
/// and mpsc channel, following the same pattern as the single-leg output.
///
/// Listener-mode legs bind once and re-accept on disconnect; caller-mode legs
/// reconnect with exponential back-off.
///
/// Stats counting: only leg 1 increments `packets_sent` and `bytes_sent` to
/// avoid double-counting, since both legs carry identical data.
async fn srt_output_redundant_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Bind persistent listeners for any legs in listener mode
    let mut listener_leg1 = if config.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_output(config).await?)
    } else {
        None
    };
    let mut listener_leg2 = if redundancy.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_redundancy(redundancy).await?)
    } else {
        None
    };

    // Optional MPTS → SPTS program filter (shared across both legs and across reconnects).
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (2022-7): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    // Outer reconnection loop — restarts both legs on total connection loss
    loop {
        // Connect/accept leg 1
        let socket_leg1 = if let Some(ref mut listener) = listener_leg1 {
            match accept_srt_connection(listener, &cancel).await {
                Ok(s) => s,
                Err(_) if cancel.is_cancelled() => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            match connect_srt_output(config, &cancel).await {
                Ok(s) => s,
                Err(e) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    return Err(e);
                }
            }
        };
        tracing::info!(
            "SRT output '{}' leg1 connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr
        );
        events.emit_flow(EventSeverity::Info, "srt", format!("SRT output '{}' connected", config.id), flow_id);

        // Connect/accept leg 2 (best-effort — fall back to single-leg if it fails)
        let socket_leg2 = if let Some(ref mut listener) = listener_leg2 {
            match accept_srt_connection(listener, &cancel).await {
                Ok(s) => Some(s),
                Err(_) if cancel.is_cancelled() => {
                    let _ = socket_leg1.close().await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        "SRT output '{}' leg2 accept failed: {e}, running single-leg",
                        config.id
                    );
                    None
                }
            }
        } else {
            match connect_srt_redundancy_leg(redundancy, &cancel).await {
                Ok(s) => Some(s),
                Err(e) => {
                    if cancel.is_cancelled() {
                        let _ = socket_leg1.close().await;
                        return Ok(());
                    }
                    tracing::warn!(
                        "SRT output '{}' leg2 connection failed: {e}, running single-leg",
                        config.id
                    );
                    None
                }
            }
        };
        if socket_leg2.is_some() {
            tracing::info!(
                "SRT output '{}' leg2 connected: mode={:?} local={}",
                config.id,
                redundancy.mode,
                redundancy.local_addr
            );
        }

        // Subscribe AFTER both legs connect so no stale packets accumulate
        let mut rx = broadcast_tx.subscribe();

        // Spawn async send tasks — one per leg.
        let (send_tx_leg1, mut send_rx_leg1) = tokio::sync::mpsc::channel::<Bytes>(256);

        let send_socket1 = socket_leg1.clone();
        let send_stats1 = stats.clone();
        let output_id1 = config.id.clone();
        let send_handle1 = tokio::spawn(async move {
            while let Some(data) = send_rx_leg1.recv().await {
                // Same chunking discipline as the single-leg path.
                match srt_send_chunked(&send_socket1, &data).await {
                    Ok(sent) => {
                        send_stats1.packets_sent.fetch_add(1, Ordering::Relaxed);
                        send_stats1.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!("SRT output '{}' leg1 send error: {e}", output_id1);
                        break;
                    }
                }
            }
        });

        // Only spawn leg2 send task if leg2 is connected
        let send_tx_leg2_opt = if let Some(ref sock2) = socket_leg2 {
            let (send_tx_leg2, mut send_rx_leg2) = tokio::sync::mpsc::channel::<Bytes>(256);
            let send_socket2 = sock2.clone();
            let output_id2 = config.id.clone();
            let send_handle2 = tokio::spawn(async move {
                while let Some(data) = send_rx_leg2.recv().await {
                    match srt_send_chunked(&send_socket2, &data).await {
                        Ok(_) => {
                            // Don't double-count — leg1 already counted
                        }
                        Err(e) => {
                            tracing::warn!("SRT output '{}' leg2 send error: {e}", output_id2);
                            break;
                        }
                    }
                }
            });
            let _ = send_handle2; // Task will exit when channel drops
            Some(send_tx_leg2)
        } else {
            None
        };

        // Packet forwarding loop — runs until both legs die or cancel
        let mut broadcast_closed = false;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("SRT output '{}' (redundant) stopping (cancelled)", config.id);
                    drop(send_tx_leg1);
                    drop(send_tx_leg2_opt);
                    let _ = send_handle1.await;
                    let _ = socket_leg1.close().await;
                    if let Some(ref s) = socket_leg2 {
                        let _ = s.close().await;
                    }
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                    return Ok(());
                }
                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            // Apply program filter if configured. Skip both legs
                            // when the filter eats the entire packet.
                            let payload = if let Some(ref mut filter) = program_filter {
                                match filter.filter_packet(&packet, &mut filter_scratch) {
                                    Some(b) => b,
                                    None => continue,
                                }
                            } else {
                                packet.data
                            };
                            let leg1_ok = match send_tx_leg1.try_send(payload.clone()) {
                                Ok(()) => true,
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                    false
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                            };

                            let leg2_ok = if let Some(ref tx2) = send_tx_leg2_opt {
                                match tx2.try_send(payload) {
                                    Ok(()) => true,
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                        false
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                                }
                            } else {
                                false
                            };

                            if !leg1_ok && !leg2_ok {
                                tracing::warn!(
                                    "SRT output '{}' all legs lost, will reconnect",
                                    config.id
                                );
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                            tracing::warn!(
                                "SRT output '{}' (redundant) lagged, dropped {n} packets",
                                config.id
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!(
                                "SRT output '{}' broadcast channel closed",
                                config.id
                            );
                            broadcast_closed = true;
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup sockets before reconnect (listeners stay open)
        drop(send_tx_leg1);
        drop(send_tx_leg2_opt);
        let _ = send_handle1.await;
        let _ = socket_leg1.close().await;
        if let Some(ref s) = socket_leg2 {
            let _ = s.close().await;
        }

        if broadcast_closed {
            if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
            if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
            return Ok(());
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' (redundant) stopping during reconnect delay", config.id);
                if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        events.emit_flow(EventSeverity::Warning, "srt", format!("SRT output '{}' disconnected", config.id), flow_id);
        tracing::info!("SRT output '{}' (redundant) attempting reconnection...", config.id);
    }
}

/// One step of the compressed-audio bridge for the SRT output's 302M mode.
///
/// Demuxes any TS packets in `packet`, decodes ADTS AAC frames into planar
/// f32 PCM, and feeds them through `S302mOutputPipeline::process_planar`.
/// The AAC decoder and the pipeline are both built lazily on the first
/// successfully-parsed AAC frame, once the sample rate and channel count
/// are known.
///
/// Mirrors `run_compressed_302m_step` in `output_rtp_audio.rs` — same
/// lifecycle, same lazy-init logic, same once-per-kind log deduplication.
fn srt_run_compressed_302m_step(
    packet: &RtpPacket,
    demuxer: &mut crate::engine::ts_demux::TsDemuxer,
    decoder: &mut Option<crate::engine::audio_decode::AacDecoder>,
    pipeline: &mut Option<S302mOutputPipeline>,
    config: &SrtOutputConfig,
    logged_init_failure: &mut bool,
    logged_decode_error: &mut Option<String>,
    decode_stats: &std::sync::Arc<crate::engine::audio_decode::DecodeStats>,
) {
    use crate::engine::audio_decode::AacDecoder;
    use crate::engine::ts_demux::DemuxedFrame;
    use crate::engine::ts_parse::strip_rtp_header;

    let ts_payload = strip_rtp_header(packet);
    if ts_payload.is_empty() {
        return;
    }
    let frames = demuxer.demux(ts_payload);
    if frames.is_empty() {
        return;
    }

    for frame in frames {
        let DemuxedFrame::Aac { data, pts: _ } = frame else {
            continue;
        };

        if decoder.is_none() {
            let Some((profile, sri, cc)) = demuxer.cached_aac_config() else {
                if !*logged_init_failure {
                    tracing::warn!(
                        "SRT output '{}' (302M) AAC frame without cached config",
                        config.id
                    );
                    *logged_init_failure = true;
                }
                continue;
            };
            match AacDecoder::from_adts_config(profile, sri, cc) {
                Ok(d) => {
                    let sample_rate = d.sample_rate();
                    let channels = d.channels();
                    let input = InputFormat {
                        sample_rate,
                        bit_depth: crate::engine::audio_transcode::BitDepth::L24,
                        channels,
                    };
                    let out_channels = if channels == 1 { 2 } else { channels };
                    match S302mOutputPipeline::new(input, out_channels, 24, 4_000) {
                        Ok(p) => {
                            tracing::info!(
                                "SRT output '{}' (302M) compressed bridge ready: \
                                 AAC-LC {sample_rate} Hz, {channels} ch -> 48000 Hz {out_channels} ch L24 302M",
                                config.id
                            );
                            *decoder = Some(d);
                            *pipeline = Some(p);
                        }
                        Err(e) => {
                            if !*logged_init_failure {
                                tracing::error!(
                                    "SRT output '{}' (302M) S302mOutputPipeline build failed: {e}",
                                    config.id
                                );
                                *logged_init_failure = true;
                            }
                            return;
                        }
                    }
                }
                Err(e) => {
                    if !*logged_init_failure {
                        tracing::error!(
                            "SRT output '{}' (302M) AAC decoder init failed: {e}",
                            config.id
                        );
                        *logged_init_failure = true;
                    }
                    return;
                }
            }
        }

        let (Some(dec), Some(p)) = (decoder.as_mut(), pipeline.as_mut()) else {
            continue;
        };

        decode_stats.inc_input();
        match dec.decode_frame(&data) {
            Ok(planar) => {
                decode_stats.inc_output();
                p.process_planar(&planar, packet.recv_time_us);
            }
            Err(e) => {
                decode_stats.inc_error();
                let kind = format!("{e}");
                if logged_decode_error.as_deref() != Some(kind.as_str()) {
                    tracing::warn!(
                        "SRT output '{}' (302M) AAC decode error: {e}",
                        config.id
                    );
                    *logged_decode_error = Some(kind);
                }
            }
        }
    }
}
