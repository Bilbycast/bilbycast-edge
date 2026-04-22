// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use srt_protocol::error::SrtError;
use srt_transport::SrtSocket;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::models::{SrtMode, SrtOutputConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::srt::connection::{
    accept_srt_connection, bind_srt_listener_for_bonded_output, bind_srt_listener_for_output,
    bind_srt_listener_for_redundancy, connect_srt_group, connect_srt_output,
    connect_srt_redundancy_leg, spawn_srt_group_stats_poller,
    spawn_srt_socket_group_stats_poller, spawn_srt_stats_poller, SrtConnectionParams,
};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::time::now_us;

use super::audio_302m::{S302mOutputPipeline, SRT_TS_DATAGRAM_BYTES};
use super::audio_transcode::InputFormat;
use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::ts_audio_replace::TsAudioReplacer;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;
use super::ts_video_replace::TsVideoReplacer;

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

/// Maximum time we wait for the SMPTE 2022-7 leg-2 socket to complete its
/// initial handshake before giving up and running the output in single-leg
/// mode. Prevents a misconfigured or unreachable leg-2 peer from gating
/// media flow on the healthy leg-1.
const INITIAL_LEG2_CONNECT_BUDGET: Duration = Duration::from_secs(15);

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
///
/// Returns the raw [`SrtError`] so callers can distinguish transient
/// backpressure ([`SrtError::AsyncSend`], now rare given the libsrt wrapper's
/// awaiting send) from fatal connection errors that warrant a reconnect.
async fn srt_send_chunked(socket: &Arc<SrtSocket>, data: &[u8]) -> Result<usize, SrtError> {
    if data.is_empty() {
        return Ok(0);
    }
    let mut total = 0usize;
    for chunk in srt_chunk_for_live(data) {
        let n = socket.send(chunk).await?;
        total += n;
    }
    Ok(total)
}

/// `true` when `err` represents a transient Rust-side backpressure condition
/// on the Tokio → `srt-io` send channel rather than a genuine connection
/// failure. Callers should **not** tear the connection down on these — drop
/// the current packet, bump `packets_dropped`, and keep serving.
///
/// Historically `SrtSocket::send` used `try_send` onto a 256-slot mpsc and
/// surfaced this error under bursty input (especially SRT + FEC on the send
/// side). The 2026-04-20 interop run traced the "SRT flapping" failure mode
/// to the forward loop treating this as fatal; the helper is retained as
/// belt-and-suspenders even after the libsrt wrapper was switched to an
/// awaiting send. See `testbed/test_reports/srt_full_interop_2026-04-20/`.
#[inline]
fn is_transient_backpressure(err: &SrtError) -> bool {
    matches!(err, SrtError::AsyncSend)
}

/// Send sink for an SRT output — either a single socket (plain or accepted
/// bonded-listener) or a native libsrt socket group (bonded caller).
///
/// Callers of [`srt_output_forward_loop`] hand one of these in and the
/// forward loop's send task fans chunks to whichever underlying handle
/// is present. Group send in live mode has the same live-message cap as
/// socket send, so we use the same chunking helper.
#[derive(Clone)]
enum SrtSendSink {
    Socket(Arc<SrtSocket>),
    Group(Arc<srt_transport::SrtGroup>),
}

impl SrtSendSink {
    async fn send_chunked(&self, data: &[u8]) -> Result<usize, SrtError> {
        if data.is_empty() {
            return Ok(0);
        }
        match self {
            SrtSendSink::Socket(s) => srt_send_chunked(s, data).await,
            SrtSendSink::Group(g) => {
                let mut total = 0usize;
                for chunk in srt_chunk_for_live(data) {
                    let n = g.send(chunk).await?;
                    total += n;
                }
                Ok(total)
            }
        }
    }

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
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> JoinHandle<()> {
    let broadcast_tx = broadcast_tx.clone();

    // Register the static portion of this output's egress media summary so
    // the manager UI can render the pipeline / format badges from the very
    // first stats snapshot. Audio-passthrough is `true` only when no
    // `audio_encode` block is present; video-passthrough mirrors `video_encode`.
    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some(
            if matches!(config.transport_mode.as_deref(), Some("audio_302m")) {
                "audio_302m".to_string()
            } else {
                "ts".to_string()
            },
        ),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none()
            && !matches!(config.transport_mode.as_deref(), Some("audio_302m")),
        audio_only: matches!(config.transport_mode.as_deref(), Some("audio_302m")),
    });

    tokio::spawn(async move {
        let result = if config.bonding.is_some() {
            // Bonded SRT output: libsrt native socket group. Shares the
            // same forward-loop pipeline as the single-socket path, so
            // program_number, audio_encode, video_encode, delay, and
            // transport_mode=audio_302m all work transparently. The only
            // difference is that outbound packets are sent via an
            // `SrtGroup::send()` sink instead of a single socket.
            srt_output_bonded_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id, input_format, compressed_audio_input, frame_rate_rx).await
        } else if config.redundancy.is_some() {
            // Redundant SRT output does not support 302M mode, so the
            // compressed-audio bridge is not applicable here.
            srt_output_redundant_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id, frame_rate_rx).await
        } else {
            srt_output_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id, input_format, compressed_audio_input, frame_rate_rx).await
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
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => srt_output_listener_loop(config, broadcast_tx, stats, cancel, events, flow_id, input_format, compressed_audio_input, frame_rate_rx).await,
        _ => srt_output_caller_loop(config, broadcast_tx, stats, cancel, events, flow_id, input_format, compressed_audio_input, frame_rate_rx).await,
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
    _flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<()> {
    let mut listener = bind_srt_listener_for_output(config).await?;
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (listener): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut pid_remapper = build_pid_remapper(config);
    let mut audio_replacer = build_audio_replacer(config, events);
    let mut video_replacer = build_video_replacer(config, &stats, events);

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
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_output_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT output '{}' connected", config.id), &config.id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

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
                            match srt_send_chunked(&socket, &packet.data).await {
                                Ok(_) => {
                                    sent += 1;
                                    // After sending some packets, check for ACKs periodically
                                    if sent % 100 == 0 {
                                        let srt_stats = socket.stats().await;
                                        if srt_stats.pkt_recv_ack_total > 0 {
                                            break true;
                                        }
                                    }
                                }
                                Err(_) => {
                                    // Send channel may be full if the I/O thread hasn't
                                    // started draining yet. Yield briefly rather than
                                    // declaring the connection dead — the ACK deadline
                                    // is the real health check.
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
            events.emit_output(EventSeverity::Warning, category::SRT, format!("SRT output '{}' stale connection detected", config.id), &config.id);
            let _ = socket.close().await;
            continue;
        }

        // Subscribe AFTER the peer connects so no stale packets accumulate
        // during the accept() wait. The receiver is dropped and re-created
        // on each reconnect cycle.
        let mut rx = broadcast_tx.subscribe();

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.srt_stats_cache.clone(), poller_cancel.clone());

        let sink = SrtSendSink::Socket(socket.clone());
        let disconnected = srt_output_forward_loop(config, &mut rx, &stats, &cancel, &sink, &mut program_filter, &mut pid_remapper, &mut audio_replacer, &mut video_replacer, input_format, compressed_audio_input, frame_rate_rx.clone()).await?;
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

        events.emit_output_with_details(
            EventSeverity::Warning, category::SRT,
            format!("SRT output '{}' disconnected", config.id), &config.id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );
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
    _flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<()> {
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (caller): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut pid_remapper = build_pid_remapper(config);
    let mut audio_replacer = build_audio_replacer(config, events);
    let mut video_replacer = build_video_replacer(config, &stats, events);
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
                events.emit_output_with_details(
                    EventSeverity::Critical, category::SRT,
                    format!("SRT output '{}' connection failed: {e}", config.id), &config.id,
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
            "SRT output '{}' connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_output_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT output '{}' connected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

        // Subscribe AFTER the peer connects so no stale packets accumulate
        let mut rx = broadcast_tx.subscribe();

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.srt_stats_cache.clone(), poller_cancel.clone());

        let sink = SrtSendSink::Socket(socket.clone());
        let disconnected = srt_output_forward_loop(config, &mut rx, &stats, &cancel, &sink, &mut program_filter, &mut pid_remapper, &mut audio_replacer, &mut video_replacer, input_format, compressed_audio_input, frame_rate_rx.clone()).await?;
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

        events.emit_output_with_details(
            EventSeverity::Warning, category::SRT,
            format!("SRT output '{}' disconnected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
            }),
        );
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
/// Build a [`TsPidRemapper`] from `config.pid_map`. Returns `None` when
/// the map is absent or empty (no-op).
fn build_pid_remapper(config: &SrtOutputConfig) -> Option<TsPidRemapper> {
    let map = config.pid_map.as_ref()?;
    let r = TsPidRemapper::new(map);
    if r.is_active() {
        tracing::info!(
            "SRT output '{}': pid_map active ({} entries)",
            config.id,
            map.len()
        );
        Some(r)
    } else {
        None
    }
}

/// Resolve an `audio_encode` config into a [`TsAudioReplacer`]. Logs and
/// returns `None` when the codec isn't supported by this build.
fn build_audio_replacer(config: &SrtOutputConfig, events: &EventSender) -> Option<TsAudioReplacer> {
    let enc = config.audio_encode.as_ref()?;
    match TsAudioReplacer::new(enc, config.transcode.clone()) {
        Ok(r) => {
            tracing::info!(
                "SRT output '{}': audio_encode active ({})",
                config.id,
                r.target_description()
            );
            events.emit_output_with_details(
                EventSeverity::Info,
                category::AUDIO_ENCODE,
                format!("TS audio encoder started: output '{}'", config.id),
                &config.id,
                serde_json::json!({ "codec": enc.codec }),
            );
            Some(r)
        }
        Err(e) => {
            tracing::error!(
                "SRT output '{}': audio_encode rejected: {e}; audio will be left untouched",
                config.id
            );
            events.emit_output_with_details(
                EventSeverity::Critical,
                category::AUDIO_ENCODE,
                format!("TS audio encoder failed: output '{}': {e}", config.id),
                &config.id,
                serde_json::json!({ "error": e.to_string() }),
            );
            None
        }
    }
}

/// Resolve the `video_encode` config into a [`TsVideoReplacer`] and register
/// its atomic stats handle on the output's accumulator. The descriptors
/// (codec / width / height / fps / bitrate) come from the config — the actual
/// runtime values may differ on first-frame discovery (see
/// [`TsVideoReplacer`] MVP scope).
fn build_video_replacer(
    config: &SrtOutputConfig,
    stats: &OutputStatsAccumulator,
    events: &EventSender,
) -> Option<TsVideoReplacer> {
    let enc = config.video_encode.as_ref()?;
    match TsVideoReplacer::new(enc, None) {
        Ok(r) => {
            let backend = match enc.codec.as_str() {
                "x264" | "x265" => enc.codec.clone(),
                "h264_nvenc" | "hevc_nvenc" => "nvenc".to_string(),
                other => other.to_string(),
            };
            let target_codec = match enc.codec.as_str() {
                "x264" | "h264_nvenc" => "h264",
                "x265" | "hevc_nvenc" => "hevc",
                other => other,
            };
            stats.set_video_encode_stats(
                r.stats_handle(),
                String::new(),
                target_codec.to_string(),
                enc.width.unwrap_or(0),
                enc.height.unwrap_or(0),
                match (enc.fps_num, enc.fps_den) {
                    (Some(n), Some(d)) if d > 0 => n as f32 / d as f32,
                    _ => 0.0,
                },
                enc.bitrate_kbps.unwrap_or(0),
                backend,
            );
            tracing::info!(
                "SRT output '{}': video_encode active ({})",
                config.id,
                r.target_description()
            );
            events.emit_output_with_details(
                EventSeverity::Info,
                category::VIDEO_ENCODE,
                format!("Video encoder started: output '{}'", config.id),
                &config.id,
                serde_json::json!({ "codec": enc.codec }),
            );
            Some(r)
        }
        Err(e) => {
            tracing::error!(
                "SRT output '{}': video_encode rejected: {e}; video will be left untouched",
                config.id
            );
            events.emit_output_with_details(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                format!("Video encoder failed: output '{}': {e}", config.id),
                &config.id,
                serde_json::json!({ "error": e.to_string() }),
            );
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn srt_output_forward_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: &Arc<OutputStatsAccumulator>,
    cancel: &CancellationToken,
    sink: &SrtSendSink,
    program_filter: &mut Option<TsProgramFilter>,
    pid_remapper: &mut Option<TsPidRemapper>,
    audio_replacer: &mut Option<TsAudioReplacer>,
    video_replacer: &mut Option<TsVideoReplacer>,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<bool> {
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<(Bytes, u64)>(256);
    let send_sink = sink.clone();
    let send_stats = stats.clone();
    let output_id = config.id.clone();
    let send_handle = tokio::spawn(async move {
        while let Some((data, recv_time_us)) = send_rx.recv().await {
            // Chunk every outbound buffer into ≤SRT_LIVE_MAX_MSG_BYTES messages
            // before handing them to the SRT sink. Protects every upstream
            // code path (passthrough TS, program-filtered TS, 302M datagram,
            // and the RTMP/RTSP/WebRTC TS-mux paths that bundle whole frames)
            // from emitting oversized SRT live-mode messages that would be
            // silently truncated by libsrt receivers. The sink transparently
            // routes to either a single socket or a bonded SrtGroup.
            match send_sink.send_chunked(&data).await {
                Ok(sent) => {
                    // Count one logical packet per upstream send call (the
                    // chunking is invisible to upstream stats consumers).
                    send_stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    send_stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    send_stats.record_latency(recv_time_us);
                }
                Err(e) if is_transient_backpressure(&e) => {
                    // Rust-side send-channel was briefly full. Drop this
                    // packet and continue — the connection is still healthy.
                    send_stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
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
    let mut remap_scratch: Vec<u8> = Vec::new();

    // Optional output delay buffer for stream synchronization.
    // Only active for the normal TS path (not 302M, which is blocked
    // by validation). The delay buffer queues filtered packets and
    // releases them after the configured delay.
    let resolved = if let Some(ref delay) = config.delay {
        resolve_output_delay(delay, &config.id, frame_rate_rx, cancel).await
    } else {
        None
    };
    let (mut delay_buf, _frame_rate_watch) = match resolved {
        Some((buf, watch)) => (Some(buf), watch),
        None => (None, None),
    };

    let delay_sleep = tokio::time::sleep(Duration::from_secs(86400));
    tokio::pin!(delay_sleep);

    let connection_lost = loop {
        if let Some(ref db) = delay_buf {
            if let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }
        }

        // Payloads to send via the SRT mpsc channel after the select.
        // Each entry carries (data, recv_time_us) for end-to-end latency tracking.
        let mut payloads_to_send: Vec<(Bytes, u64)> = Vec::new();
        let mut got_connection_lost = false;

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
                                    match send_tx.try_send((datagram, packet.recv_time_us)) {
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
                                match send_tx.try_send((datagram, packet.recv_time_us)) {
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

                        // Apply audio ES replacement + video ES replacement
                        // if configured. Each replacer owns its own TS state
                        // and only touches its own elementary stream, so
                        // stacking them is safe. Validation guarantees these
                        // are not combined with redundancy, FEC, or 302M.
                        let need_strip = audio_replacer.is_some() || video_replacer.is_some();
                        let payload = if need_strip {
                            const RTP_HEADER_MIN: usize = 12;
                            let ts_in: &[u8] = if packet.is_raw_ts {
                                &payload[..]
                            } else if payload.len() > RTP_HEADER_MIN {
                                &payload[RTP_HEADER_MIN..]
                            } else {
                                continue;
                            };

                            let mut a_out: Vec<u8> = Vec::new();
                            let after_audio: &[u8] =
                                if let Some(replacer) = audio_replacer.as_mut() {
                                    tokio::task::block_in_place(|| {
                                        replacer.process(ts_in, &mut a_out);
                                    });
                                    &a_out
                                } else {
                                    ts_in
                                };

                            let mut v_out: Vec<u8> = Vec::new();
                            let final_bytes: &[u8] =
                                if let Some(vreplacer) = video_replacer.as_mut() {
                                    tokio::task::block_in_place(|| {
                                        vreplacer.process(after_audio, &mut v_out);
                                    });
                                    &v_out
                                } else {
                                    after_audio
                                };

                            if final_bytes.is_empty() {
                                continue;
                            }
                            Bytes::copy_from_slice(final_bytes)
                        } else {
                            payload
                        };

                        // Apply PID remap last so audio/video replacers saw the
                        // original PIDs in the source PMT. need_strip implies
                        // the bytes are now raw TS (RTP header was discarded).
                        let payload = if let Some(remapper) = pid_remapper.as_mut() {
                            let temp = RtpPacket {
                                data: payload,
                                sequence_number: packet.sequence_number,
                                rtp_timestamp: packet.rtp_timestamp,
                                recv_time_us: packet.recv_time_us,
                                is_raw_ts: packet.is_raw_ts || need_strip,
                            };
                            match remapper.process_packet(&temp, &mut remap_scratch) {
                                Some(b) => b,
                                None => continue,
                            }
                        } else {
                            payload
                        };

                        if let Some(ref mut db) = delay_buf {
                            // Re-wrap as RtpPacket for the delay buffer (using
                            // the filtered payload but preserving timing metadata).
                            db.push(RtpPacket {
                                data: payload,
                                sequence_number: packet.sequence_number,
                                rtp_timestamp: packet.rtp_timestamp,
                                recv_time_us: packet.recv_time_us,
                                is_raw_ts: packet.is_raw_ts,
                            });
                        } else {
                            payloads_to_send.push((payload, packet.recv_time_us));
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
            _ = &mut delay_sleep, if delay_buf.as_ref().map_or(false, |db| db.len() > 0) => {
                let db = delay_buf.as_mut().unwrap();
                let now = now_us();
                for packet in db.drain_ready(now) {
                    payloads_to_send.push((packet.data, packet.recv_time_us));
                }
            }
        }

        // Send all collected payloads to the SRT send task.
        for (payload, recv_time_us) in payloads_to_send {
            match send_tx.try_send((payload, recv_time_us)) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!(
                        "SRT output '{}' connection lost, will reconnect",
                        config.id
                    );
                    got_connection_lost = true;
                    break;
                }
            }
        }
        if got_connection_lost {
            break true;
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
    _flow_id: &str,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Persistent listeners for any legs in listener mode (kept across
    // reconnect cycles so ports stay bound).
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

    // Optional PID remapper. Both legs must emit identical payloads for the
    // receiver merger to dedupe by seq; a single remapper fed by both legs
    // guarantees that.
    let mut pid_remapper = build_pid_remapper(config);
    let mut remap_scratch: Vec<u8> = Vec::new();

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
            config.local_addr.as_deref().unwrap_or("auto")
        );
        events.emit_output_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT output '{}' connected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "leg": 1,
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );

        // Subscribe to broadcast and spawn leg 1 send task IMMEDIATELY.
        // Leg 2 is brought up in parallel (below) with INITIAL_LEG2_CONNECT_BUDGET —
        // while that handshake is in progress we keep leg 1 hot so the receiver
        // keeps ACK'ing and the sender's libsrt send buffer never stalls
        // on an idle socket.
        let mut rx = broadcast_tx.subscribe();

        let (send_tx_leg1, mut send_rx_leg1) = tokio::sync::mpsc::channel::<(Bytes, u64)>(256);
        let send_socket1 = socket_leg1.clone();
        let send_stats1 = stats.clone();
        let output_id1 = config.id.clone();
        let send_handle1 = tokio::spawn(async move {
            while let Some((data, recv_time_us)) = send_rx_leg1.recv().await {
                match srt_send_chunked(&send_socket1, &data).await {
                    Ok(sent) => {
                        send_stats1.packets_sent.fetch_add(1, Ordering::Relaxed);
                        send_stats1.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                        send_stats1.record_latency(recv_time_us);
                    }
                    Err(e) if is_transient_backpressure(&e) => {
                        send_stats1.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!("SRT output '{}' leg1 send error: {e}", output_id1);
                        break;
                    }
                }
            }
        });

        // Leg-2 connect/accept future, pinned so we can poll it inside the
        // main packet select! and complete it exactly once per reconnect
        // cycle. Bounded by INITIAL_LEG2_CONNECT_BUDGET so a dead leg-2 peer
        // never gates leg-1. While this is pending, leg-1 is already pumping.
        let leg2_connect_fut = {
            let events = events.clone();
            let output_id = config.id.clone();
            let redundancy = redundancy.clone();
            let cancel = cancel.clone();
            let mut listener_leg2_mut = listener_leg2.take();
            async move {
                let result: (Option<Arc<SrtSocket>>, Option<srt_transport::SrtListener>) = if let Some(ref mut listener) = listener_leg2_mut {
                    let r = tokio::time::timeout(
                        INITIAL_LEG2_CONNECT_BUDGET,
                        accept_srt_connection(listener, &cancel),
                    ).await;
                    match r {
                        Ok(Ok(s)) => (Some(s), listener_leg2_mut),
                        Ok(Err(_)) if cancel.is_cancelled() => (None, listener_leg2_mut),
                        Ok(Err(e)) => {
                            tracing::warn!("SRT output '{}' leg2 accept failed: {e}, running single-leg", output_id);
                            (None, listener_leg2_mut)
                        }
                        Err(_) => {
                            tracing::warn!(
                                "SRT output '{}' leg2 did not come up within {:?} (accept timed out), running single-leg",
                                output_id, INITIAL_LEG2_CONNECT_BUDGET,
                            );
                            events.emit_output_with_details(
                                EventSeverity::Warning, category::SRT,
                                format!("SRT output '{}' 2022-7 leg 2 did not connect within {:?}; running single-leg", output_id, INITIAL_LEG2_CONNECT_BUDGET),
                                &output_id,
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
                            tracing::warn!("SRT output '{}' leg2 connection failed: {e}, running single-leg", output_id);
                            (None, None)
                        }
                        Err(_) => {
                            tracing::warn!(
                                "SRT output '{}' leg2 did not come up within {:?} (caller connect retry budget exhausted), running single-leg",
                                output_id, INITIAL_LEG2_CONNECT_BUDGET,
                            );
                            events.emit_output_with_details(
                                EventSeverity::Warning, category::SRT,
                                format!("SRT output '{}' 2022-7 leg 2 did not connect within {:?}; running single-leg", output_id, INITIAL_LEG2_CONNECT_BUDGET),
                                &output_id,
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
                        "SRT output '{}' leg2 connected: mode={:?} local={}",
                        output_id,
                        redundancy.mode,
                        redundancy.local_addr.as_deref().unwrap_or("auto")
                    );
                }
                result
            }
        };
        tokio::pin!(leg2_connect_fut);
        let mut leg2_done = false;

        // leg 2 send task is spawned lazily once the leg-2 future resolves.
        let mut socket_leg2: Option<Arc<SrtSocket>> = None;
        let mut send_tx_leg2_opt: Option<tokio::sync::mpsc::Sender<(Bytes, u64)>> = None;

        // Optional output delay buffer for stream synchronization.
        let resolved = if let Some(ref delay) = config.delay {
            resolve_output_delay(delay, &config.id, frame_rate_rx.clone(), &cancel).await
        } else {
            None
        };
        let (mut delay_buf, _frame_rate_watch) = match resolved {
            Some((buf, watch)) => (Some(buf), watch),
            None => (None, None),
        };

        let delay_sleep = tokio::time::sleep(Duration::from_secs(86400));
        tokio::pin!(delay_sleep);

        // Packet forwarding loop — runs until both legs die or cancel
        let mut broadcast_closed = false;
        loop {
            if let Some(ref db) = delay_buf {
                if let Some(release_us) = db.next_release_time() {
                    let now = now_us();
                    let wait = release_us.saturating_sub(now);
                    delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
                }
            }

            let mut payloads_to_send: Vec<(Bytes, u64)> = Vec::new();

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
                    return Ok(());
                }
                // Leg 2 connector finished — attach send task (or stay single-leg).
                // Guarded by `!leg2_done` so select! won't re-poll a complete future.
                (sock, returned_listener) = &mut leg2_connect_fut, if !leg2_done => {
                    leg2_done = true;
                    if returned_listener.is_some() {
                        listener_leg2 = returned_listener;
                    }
                    if let Some(sock2) = sock {
                        let (tx2, mut rx2) = tokio::sync::mpsc::channel::<(Bytes, u64)>(256);
                        let send_socket2 = sock2.clone();
                        let output_id2 = config.id.clone();
                        tokio::spawn(async move {
                            while let Some((data, _recv_time_us)) = rx2.recv().await {
                                match srt_send_chunked(&send_socket2, &data).await {
                                    Ok(_) => { /* leg 1 already counted — avoid double-count */ }
                                    Err(e) if is_transient_backpressure(&e) => {
                                        // Transient Rust-side channel pressure
                                        // on leg 2 only. Drop this packet and
                                        // continue — leg 1 carries the dupe
                                        // under 2022-7 so the receiver merge
                                        // is unaffected.
                                    }
                                    Err(e) => {
                                        tracing::warn!("SRT output '{}' leg2 send error: {e}", output_id2);
                                        break;
                                    }
                                }
                            }
                        });
                        socket_leg2 = Some(sock2);
                        send_tx_leg2_opt = Some(tx2);
                    }
                    continue;
                }
                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            // Apply program filter if configured.
                            let recv_time_us = packet.recv_time_us;
                            let payload = if let Some(ref mut filter) = program_filter {
                                match filter.filter_packet(&packet, &mut filter_scratch) {
                                    Some(b) => b,
                                    None => continue,
                                }
                            } else {
                                packet.data
                            };

                            // Apply PID remap so both legs carry the remapped bytes.
                            let payload = if let Some(ref mut remapper) = pid_remapper {
                                let temp = RtpPacket {
                                    data: payload,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                };
                                match remapper.process_packet(&temp, &mut remap_scratch) {
                                    Some(b) => b,
                                    None => continue,
                                }
                            } else {
                                payload
                            };

                            if let Some(ref mut db) = delay_buf {
                                db.push(RtpPacket {
                                    data: payload,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                });
                            } else {
                                payloads_to_send.push((payload, recv_time_us));
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
                _ = &mut delay_sleep, if delay_buf.as_ref().map_or(false, |db| db.len() > 0) => {
                    let db = delay_buf.as_mut().unwrap();
                    let now = now_us();
                    for packet in db.drain_ready(now) {
                        payloads_to_send.push((packet.data, packet.recv_time_us));
                    }
                }
            }

            // Send to both legs
            let mut all_legs_lost = false;
            for (payload, recv_time_us) in payloads_to_send {
                let leg1_ok = match send_tx_leg1.try_send((payload.clone(), recv_time_us)) {
                    Ok(()) => true,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                };

                let leg2_ok = if let Some(ref tx2) = send_tx_leg2_opt {
                    match tx2.try_send((payload, recv_time_us)) {
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
                    all_legs_lost = true;
                    break;
                }
            }
            if all_legs_lost {
                break;
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

        events.emit_output_with_details(
            EventSeverity::Warning, category::SRT,
            format!("SRT output '{}' disconnected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "redundant": true,
            }),
        );
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

// ---------------------------------------------------------------------------
// Native libsrt SRT bonding (socket group) output
// ---------------------------------------------------------------------------

/// Bonded SRT output (libsrt native socket group).
///
/// Reuses the shared [`srt_output_forward_loop`] pipeline so `program_number`,
/// `audio_encode` (with optional companion `transcode`), `video_encode`,
/// output `delay`, and `transport_mode: audio_302m` all compose with
/// bonding the same way they compose with a plain SRT output — the only
/// change at the transport edge is that outbound payloads land on an
/// `SrtGroup::send()` (caller mode) or on an accepted listener socket
/// whose libsrt session is a group internally (listener mode).
///
/// Caller mode opens the group against every `bonding.endpoints[]` entry.
/// Listener mode binds once with `SRTO_GROUPCONNECT=true`; libsrt returns
/// a socket handle on accept whose `srt_send` / `srt_recv` / `srt_bstats`
/// calls all operate on the underlying group.
#[allow(clippy::too_many_arguments)]
async fn srt_output_bonded_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    _flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<()> {
    let bond = config
        .bonding
        .as_ref()
        .expect("bonding config must be present")
        .clone();

    // Shared pipeline state — persists across reconnects exactly like the
    // non-bonded caller/listener loops.
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (bonded): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut pid_remapper = build_pid_remapper(config);
    let mut audio_replacer = build_audio_replacer(config, events);
    let mut video_replacer = build_video_replacer(config, &stats, events);

    match config.mode {
        SrtMode::Listener => {
            let mut listener = bind_srt_listener_for_bonded_output(config).await?;
            loop {
                let socket = match accept_srt_connection(&mut listener, &cancel).await {
                    Ok(s) => s,
                    Err(_) if cancel.is_cancelled() => {
                        let _ = listener.close().await;
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::error!(
                            "SRT bonded output '{}' accept failed: {e}",
                            config.id
                        );
                        let _ = listener.close().await;
                        return Err(e);
                    }
                };
                tracing::info!(
                    "SRT bonded output '{}' accepted bonded caller (local={}, mode={:?})",
                    config.id,
                    config.local_addr.as_deref().unwrap_or("auto"),
                    bond.mode,
                );
                events.emit_output_with_details(
                    EventSeverity::Info,
                    category::SRT,
                    format!("SRT bonded output '{}' connected", config.id),
                    &config.id,
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
                    stats.srt_bonding_stats_cache.clone(),
                    poller_cancel.clone(),
                );

                let mut rx = broadcast_tx.subscribe();
                let sink = SrtSendSink::Socket(socket.clone());
                let disconnected = srt_output_forward_loop(
                    config, &mut rx, &stats, &cancel, &sink,
                    &mut program_filter, &mut pid_remapper, &mut audio_replacer, &mut video_replacer,
                    input_format, compressed_audio_input, frame_rate_rx.clone(),
                )
                .await?;
                poller_cancel.cancel();
                let _ = socket.close().await;

                if !disconnected {
                    let _ = listener.close().await;
                    return Ok(());
                }
                events.emit_output_with_details(
                    EventSeverity::Warning,
                    category::SRT,
                    format!("SRT bonded output '{}' disconnected", config.id),
                    &config.id,
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
        _ => {
            loop {
                let params = SrtConnectionParams::from(config);
                let group = match connect_srt_group(&params, &bond, &cancel).await {
                    Ok(g) => g,
                    Err(e) => {
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        tracing::error!(
                            "SRT bonded output '{}' connect failed: {e}",
                            config.id
                        );
                        return Err(e);
                    }
                };
                tracing::info!(
                    "SRT bonded output '{}' connected: mode=caller bonding={:?} endpoints={}",
                    config.id,
                    bond.mode,
                    bond.endpoints.len(),
                );
                events.emit_output_with_details(
                    EventSeverity::Info,
                    category::SRT,
                    format!("SRT bonded output '{}' connected", config.id),
                    &config.id,
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
                    stats.srt_bonding_stats_cache.clone(),
                    poller_cancel.clone(),
                );

                let mut rx = broadcast_tx.subscribe();
                let sink = SrtSendSink::Group(group.clone());
                let disconnected = srt_output_forward_loop(
                    config, &mut rx, &stats, &cancel, &sink,
                    &mut program_filter, &mut pid_remapper, &mut audio_replacer, &mut video_replacer,
                    input_format, compressed_audio_input, frame_rate_rx.clone(),
                )
                .await?;
                poller_cancel.cancel();
                let _ = group.close().await;

                if !disconnected {
                    return Ok(());
                }
                events.emit_output_with_details(
                    EventSeverity::Warning,
                    category::SRT,
                    format!("SRT bonded output '{}' disconnected", config.id),
                    &config.id,
                    serde_json::json!({ "mode": "caller-bonded" }),
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
            }
        }
    }
}

