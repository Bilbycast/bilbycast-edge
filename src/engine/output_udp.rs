// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::wire_emit::{AnchorSource, WireDatagram, spawn_wire_emitter};

use crate::config::models::UdpOutputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;
use crate::util::time::now_us;

use super::audio_302m::S302mOutputPipeline;
use super::audio_transcode::InputFormat;
use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::transcode_chain;
use super::ts_pid_overrides_rewriter::TsPidOverridesRewriter;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

/// TS sync byte.
const TS_SYNC_BYTE: u8 = 0x47;

/// TS packet size.
const TS_PACKET_SIZE: usize = 188;

/// Number of TS packets per UDP datagram.
/// 7 × 188 = 1316 bytes — the standard for MPEG-TS over UDP.
const TS_PACKETS_PER_DATAGRAM: usize = 7;

/// Minimum consecutive sync bytes needed to confirm TS alignment.
const TS_SYNC_CONFIRM_COUNT: usize = 3;

/// Minimum RTP header size (version 2, no CSRC, no extensions).
const RTP_HEADER_MIN_SIZE: usize = 12;

/// Spawn a task that subscribes to the broadcast channel and sends
/// raw MPEG-TS datagrams (no RTP headers) over a UDP socket.
///
/// If the input stream is RTP-wrapped (`is_raw_ts == false`), the 12-byte
/// RTP header is stripped before buffering. The TS data is re-aligned into
/// standard 7×188-byte datagrams before sending.
pub fn spawn_udp_output(
    config: UdpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    input_format: Option<InputFormat>,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    events: EventSender,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
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
        ..Default::default()
    };
    if let Some(ve) = config.video_encode.as_ref() {
        egress_static = egress_static.with_video_encode_target(ve);
    }
    if let Some(ae) = config.audio_encode.as_ref() {
        egress_static = egress_static.with_audio_encode_target(ae);
    }
    output_stats.set_egress_static(egress_static);

    tokio::spawn(async move {
        if matches!(config.transport_mode.as_deref(), Some("audio_302m")) {
            if let Err(e) =
                udp_output_loop_302m(&config, &mut rx, output_stats, cancel, input_format, &events, active_input_rx).await
            {
                tracing::error!(
                    "UDP output '{}' (audio_302m) exited with error: {e}",
                    config.id
                );
            }
        } else if let Err(e) = udp_output_loop(&config, &mut rx, output_stats, cancel, frame_rate_rx, &events, av_sync_pacer, active_input_rx).await {
            tracing::error!("UDP output '{}' exited with error: {e}", config.id);
        }
    })
}

/// 302M output loop: builds a [`S302mOutputPipeline`] and ships its
/// 1316-byte (7×188) MPEG-TS datagrams over plain UDP. No RTP wrapper —
/// hardware decoders that expect raw MPEG-TS over UDP can consume this
/// directly.
async fn udp_output_loop_302m(
    config: &UdpOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    input_format: Option<InputFormat>,
    events: &EventSender,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    let (socket, dest) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
        config.interface_binding.as_ref(),
    )
    .await?;

    let input = match input_format {
        Some(i) => i,
        None => {
            tracing::warn!(
                "UDP output '{}' transport_mode=audio_302m but upstream input is not audio; \
                 falling back to passthrough TS",
                config.id
            );
            return udp_output_loop(config, rx, stats, cancel, None, events, None, active_input_rx).await;
        }
    };

    let out_channels = match input.channels {
        1 => 2,
        n if n % 2 == 0 && n <= 8 => n,
        n if n > 8 => 8,
        _ => input.channels + 1,
    };
    let mut pipeline = match S302mOutputPipeline::new(input, out_channels, 24, 4_000, config.pid_overrides.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                "UDP output '{}' could not build 302M pipeline: {e}",
                config.id
            );
            return Ok(());
        }
    };

    tracing::info!(
        "UDP output '{}' (audio_302m) -> {} ({}Hz/{}bit/{}ch input)",
        config.id,
        dest,
        input.sample_rate,
        input.bit_depth.as_u8(),
        input.channels,
    );

    let std_socket = socket
        .into_std()
        .map_err(|e| anyhow::anyhow!("convert tokio UdpSocket -> std: {e}"))?;
    std_socket.set_nonblocking(false)?;
    let wire_tx = spawn_wire_emitter(
        format!("{}-302m", config.id),
        std_socket,
        dest,
        AnchorSource::Pcr,
        stats.clone(),
        cancel.clone(),
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("UDP output '{}' (audio_302m) stopping (cancelled)", config.id);
                return Ok(());
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        pipeline.process(&packet);
                        for datagram in pipeline.take_ready_datagrams() {
                            let dg = WireDatagram {
                                bytes: Bytes::from(datagram),
                                recv_time_us: packet.recv_time_us,
                                target_tx_time_ns: None,
                                ts_offset: 0,
                            };
                            if wire_tx.try_send(dg).is_err() {
                                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("UDP output '{}' (302M) broadcast closed", config.id);
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn udp_output_loop(
    config: &UdpOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    events: &EventSender,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    let (socket, dest) =
        create_udp_output(&config.dest_addr, config.bind_addr.as_deref(), config.interface_addr.as_deref(), config.dscp, config.interface_binding.as_ref()).await?;

    tracing::info!("UDP output '{}' started -> {}", config.id, dest);

    // ── PCR-anchored wire pacing (engine::wire_emit) ─────────────────
    //
    // THIS task runs the encoder pipeline (audio + video replacers
    // with `block_in_place`) and feeds completed datagrams to a
    // dedicated wire-emit thread via std::sync::mpsc. The wire
    // thread paces emission against PCR (closed-loop on observed
    // inter-PCR rate), via SO_TXTIME on Linux ≥ 4.19 with permitted
    // setsockopt or `clock_nanosleep` fallback.
    //
    // try_send is non-blocking: if the wire thread is behind
    // (sync_channel full), drop the datagram and count it. Same drop
    // semantic as the broadcast channel's Lagged arm — slow wire
    // never propagates backpressure to the encoder.
    let std_socket = socket
        .into_std()
        .map_err(|e| anyhow::anyhow!("convert tokio UdpSocket -> std: {e}"))?;
    std_socket.set_nonblocking(false)?;
    let wire_tx = spawn_wire_emitter(
        config.id.clone(),
        std_socket,
        dest,
        AnchorSource::Pcr,
        stats.clone(),
        cancel.clone(),
    );

    // Optional MPTS → SPTS program filter.
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "UDP output '{}': program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    // Optional PID remapper (runs last in the TS transform chain, just before
    // the datagram assembler picks up bytes).
    let mut pid_remapper = config.pid_map.as_ref().and_then(|m| {
        let r = TsPidRemapper::new(m);
        if r.is_active() {
            tracing::info!(
                "UDP output '{}': pid_map active ({} entries)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut remap_scratch: Vec<u8> = Vec::new();

    // Per-program role-keyed PID rewriter. Single owner of PID remapping
    // on both passthrough and transcoded paths — the transcode replacers
    // re-encode on the source audio / video PID and let this stage handle
    // every PAT/PMT/ES PID rename, including for programs other than 1.
    let mut pid_overrides_rewriter = config.pid_overrides.as_ref().and_then(|m| {
        let r = TsPidOverridesRewriter::new(m);
        if r.is_active() {
            tracing::info!(
                "UDP output '{}': pid_overrides rewriter active ({} programs)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut overrides_scratch: Vec<u8> = Vec::new();

    // Transcoding chain (audio_encode + video_encode replacers running
    // on a dedicated SCHED_FIFO codec thread, off the Tokio runtime).
    // Stage 1 of the data-plane redesign: replaces the prior inline
    // `tokio::task::block_in_place` pattern that caused work-stealing
    // churn whenever a frame's encode took > a few hundred µs.
    // See `engine::transcode_chain` + `docs/production-tuning.md`.
    // Codec → wire pacer backpressure: codec thread pauses one frame
    // interval before each encode when wire_tx is > 75 % full. Source
    // frame interval is derived from the operator-supplied fps_num/den;
    // defaults to ~33 ms (30 fps) when unset. Without this, transient
    // VBR encoder overshoots fill wire_tx and drop fresh content (see
    // cell24 x264 case study in `engine::transcode_chain::WireBackpressure`).
    let backpressure = config.video_encode.as_ref().map(|ve| {
        let (n, d) = (ve.fps_num.unwrap_or(30) as u64, ve.fps_den.unwrap_or(1) as u64);
        let frame_interval = if n > 0 {
            std::time::Duration::from_nanos((d * 1_000_000_000) / n)
        } else {
            std::time::Duration::from_millis(33)
        };
        transcode_chain::WireBackpressure {
            depth: wire_tx.depth_handle(),
            threshold: super::wire_emit::WIRE_CHANNEL_BACKPRESSURE_THRESHOLD,
            frame_interval,
        }
    });

    let mut transcode_chain = match transcode_chain::build_for_output(
        &config.id,
        config.audio_encode.as_ref(),
        config.video_encode.as_ref(),
        config.transcode.clone(),
        &stats,
        av_sync_pacer.as_ref(),
        backpressure,
    ) {
        Ok(chain) => {
            if let Some(ref c) = chain {
                tracing::info!(
                    "UDP output '{}': transcode chain active ({})",
                    config.id,
                    c.target_description()
                );
                if config.audio_encode.is_some() {
                    events.emit_output_with_details(
                        EventSeverity::Info,
                        category::AUDIO_ENCODE,
                        format!("TS audio encoder started: output '{}'", config.id),
                        &config.id,
                        serde_json::json!({
                            "codec": config.audio_encode.as_ref().map(|e| &e.codec)
                        }),
                    );
                }
                if config.video_encode.is_some() {
                    events.emit_output_with_details(
                        EventSeverity::Info,
                        category::VIDEO_ENCODE,
                        format!("Video encoder started: output '{}'", config.id),
                        &config.id,
                        serde_json::json!({
                            "codec": config.video_encode.as_ref().map(|e| &e.codec)
                        }),
                    );
                }
            }
            chain
        }
        Err(e) => {
            tracing::error!(
                "UDP output '{}': transcode chain rejected: {e}; transcoding disabled",
                config.id
            );
            let (cat, label) = match e {
                transcode_chain::TranscodeChainError::Audio(_) => {
                    (category::AUDIO_ENCODE, "TS audio encoder")
                }
                transcode_chain::TranscodeChainError::Video(_) => {
                    (category::VIDEO_ENCODE, "Video encoder")
                }
            };
            events.emit_output_with_details(
                EventSeverity::Critical,
                cat,
                format!("{label} failed: output '{}': {e}", config.id),
                &config.id,
                serde_json::json!({ "error": e.to_string() }),
            );
            None
        }
    };

    // Optional CBR null padder: inflates the natural transcoded rate
    // up to a target wire bitrate by injecting PID 0x1FFF NULL packets.
    // Sits between the transcoder pipeline and wire_emit so the wire
    // pacer's observed inter-PCR rate matches the configured CBR
    // target. Disabled when `cbr_pad_to_kbps` is unset.
    let mut null_padder = config
        .cbr_pad_to_kbps
        .map(crate::engine::ts_null_padder::TsNullPadder::new);
    let mut pad_scratch: Vec<u8> = Vec::new();

    // Wire the per-output input-switch watcher: on every change of the
    // flow's active input, flip the replacers' external_reset flags so
    // the next process() call re-anchors PTS and forces an IDR. Without
    // this, a same-codec same-PID swap leaves stale state and the
    // receiving decoder gets stuck on PTS values that no longer line
    // up with the master-clock-paced output PCR.
    let mut switch_handles: Vec<Arc<std::sync::atomic::AtomicBool>> = Vec::new();
    if let Some(ref chain) = transcode_chain {
        if let Some(h) = chain.audio_external_reset_handle() {
            switch_handles.push(h);
        }
        if let Some(h) = chain.video_external_reset_handle() {
            switch_handles.push(h);
        }
    }
    crate::engine::input_switch_watcher::spawn(
        config.id.clone(),
        active_input_rx,
        switch_handles,
        cancel.clone(),
    );

    // Buffer for re-aligning TS data into 188-byte aligned datagrams.
    let mut ts_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut ts_sync_found = false;

    // Optional output delay buffer for stream synchronization.
    let resolved = if let Some(ref delay) = config.delay {
        resolve_output_delay(delay, &config.id, frame_rate_rx, &cancel).await
    } else {
        None
    };
    let (mut delay_buf, _frame_rate_watch) = match resolved {
        Some((buf, watch)) => (Some(buf), watch),
        None => (None, None),
    };

    let delay_sleep = tokio::time::sleep(Duration::from_secs(86400));
    tokio::pin!(delay_sleep);

    // Per-output A/V alignment buffer.
    let mut av_align_buf = if config.av_align {
        let window_ms = config.av_align_window_ms.unwrap_or(100);
        Some(crate::engine::ts_av_align::TsAvAlignBuffer::new(
            crate::engine::ts_av_align::AlignConfig {
                alignment_window_ms: window_ms,
                tolerance_ms: 5,
                max_hold_ms: window_ms * 2,
                stream_presence_timeout_ms: 500,
                capacity_packets: crate::engine::ts_av_align::TsAvAlignBuffer::initial_capacity(window_ms),
            },
        ))
    } else {
        None
    };
    let mut align_scratch: Vec<u8> = Vec::with_capacity(8192);

    // Reused per-iteration send scratch. Hoisted out of the select loop so
    // the per-packet hot path does not allocate a fresh Vec on every branch
    // — at 50 Mbps that would be ~3k allocs/sec per output.
    let mut packets_to_send: Vec<RtpPacket> = Vec::with_capacity(4);

    loop {
        if let Some(ref db) = delay_buf {
            if let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }
        }

        packets_to_send.clear();

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("UDP output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        if let Some(ref mut db) = delay_buf {
                            db.push(packet);
                        } else {
                            packets_to_send.push(packet);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!("UDP output '{}' lagged, dropped {n} packets", config.id);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("UDP output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
            _ = &mut delay_sleep, if delay_buf.as_ref().map_or(false, |db| db.len() > 0) => {
                let db = delay_buf.as_mut().unwrap();
                let now = now_us();
                for packet in db.drain_ready(now) {
                    packets_to_send.push(packet);
                }
            }
        }

        // Per-iteration handler for transcoded output bytes flushed from
        // the codec thread. Re-injected into the same downstream
        // pipeline (padder → pid_overrides → pid_remapper → ts_buf →
        // wire_tx) as the no-transcode path, just with bytes-after-
        // transcode rather than bytes-before-filter. The closure captures
        // all the per-iteration mutable scratch buffers + the long-lived
        // wire_tx so call sites stay terse.
        let forward_downstream = |downstream_input: &[u8],
                                  recv_time_us: u64,
                                  null_padder: &mut Option<crate::engine::ts_null_padder::TsNullPadder>,
                                  pad_scratch: &mut Vec<u8>,
                                  pid_overrides_rewriter: &mut Option<TsPidOverridesRewriter>,
                                  overrides_scratch: &mut Vec<u8>,
                                  pid_remapper: &mut Option<TsPidRemapper>,
                                  remap_scratch: &mut Vec<u8>,
                                  av_aligner: &mut Option<crate::engine::ts_av_align::TsAvAlignBuffer>,
                                  av_align_scratch: &mut Vec<u8>,
                                  ts_buf: &mut BytesMut,
                                  ts_sync_found: &mut bool,
                                  config_id: &str,
                                  stats: &OutputStatsAccumulator,
                                  wire_tx: &super::wire_emit::WireTxHandle| {
            let after_pad: &[u8] = if let Some(padder) = null_padder.as_mut() {
                pad_scratch.clear();
                padder.process(downstream_input, pad_scratch);
                pad_scratch
            } else {
                downstream_input
            };
            let after_overrides: &[u8] = if let Some(rw) = pid_overrides_rewriter.as_mut() {
                overrides_scratch.clear();
                rw.process(after_pad, overrides_scratch);
                overrides_scratch
            } else {
                after_pad
            };
            let after_remap: &[u8] = if let Some(remapper) = pid_remapper.as_mut() {
                remap_scratch.clear();
                remapper.process(after_overrides, remap_scratch);
                remap_scratch
            } else {
                after_overrides
            };
            if let Some(ref mut al) = *av_aligner {
                av_align_scratch.clear();
                al.process(after_remap, av_align_scratch);
                ts_buf.extend_from_slice(av_align_scratch);
            } else {
                ts_buf.extend_from_slice(after_remap);
            }
            if !*ts_sync_found {
                let min_bytes = TS_SYNC_CONFIRM_COUNT * TS_PACKET_SIZE;
                if ts_buf.len() >= min_bytes {
                    let mut found_offset = None;
                    for offset in 0..TS_PACKET_SIZE {
                        let all_sync = (0..TS_SYNC_CONFIRM_COUNT).all(|i| {
                            let pos = offset + i * TS_PACKET_SIZE;
                            pos < ts_buf.len() && ts_buf[pos] == TS_SYNC_BYTE
                        });
                        if all_sync {
                            found_offset = Some(offset);
                            break;
                        }
                    }
                    if let Some(offset) = found_offset {
                        if offset > 0 {
                            tracing::info!(
                                "UDP output '{}': TS sync found at offset {}, discarding {} leading bytes",
                                config_id, offset, offset
                            );
                            let _ = ts_buf.split_to(offset);
                        } else {
                            tracing::info!("UDP output '{}': TS sync found (already aligned)", config_id);
                        }
                        *ts_sync_found = true;
                    } else {
                        tracing::warn!(
                            "UDP output '{}': no TS sync found in {} bytes, discarding",
                            config_id, ts_buf.len()
                        );
                        ts_buf.clear();
                    }
                }
            }
            if *ts_sync_found {
                while ts_buf.len() >= ts_datagram_size {
                    let datagram = ts_buf.split_to(ts_datagram_size);
                    let dg = WireDatagram {
                        bytes: datagram.freeze(),
                        recv_time_us,
                        target_tx_time_ns: None,
                        ts_offset: 0,
                    };
                    if wire_tx.try_send(dg).is_err() {
                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        };

        for packet in &packets_to_send {
            // Extract raw TS payload: strip RTP header if present
            let ts_data = if packet.is_raw_ts {
                &packet.data[..]
            } else if packet.data.len() > RTP_HEADER_MIN_SIZE {
                &packet.data[RTP_HEADER_MIN_SIZE..]
            } else {
                continue;
            };

            // Apply program filter (MPTS → SPTS) if configured.
            let filtered_bytes: &[u8] = if let Some(ref mut filter) = program_filter {
                filter_scratch.clear();
                filter.filter_into(ts_data, &mut filter_scratch);
                if filter_scratch.is_empty() {
                    continue;
                }
                &filter_scratch
            } else {
                ts_data
            };

            // If a transcode chain is active, hand the filtered bytes
            // to the codec thread (non-blocking — drop-on-full). The
            // transcoded output arrives asynchronously on the
            // `chain.recv()` branch of the outer select! below.
            if let Some(ref chain) = transcode_chain {
                if let Err(_) = chain.try_submit(Bytes::copy_from_slice(filtered_bytes)) {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                forward_downstream(
                    filtered_bytes,
                    packet.recv_time_us,
                    &mut null_padder,
                    &mut pad_scratch,
                    &mut pid_overrides_rewriter,
                    &mut overrides_scratch,
                    &mut pid_remapper,
                    &mut remap_scratch,
                    &mut av_align_buf,
                    &mut align_scratch,
                    &mut ts_buf,
                    &mut ts_sync_found,
                    &config.id,
                    &stats,
                    &wire_tx,
                );
            }
        }

        // Drain any transcoded chunks that the codec thread has produced
        // since the last poll. Non-blocking — the outer select! also has
        // a `chain.recv()` branch that wakes us when new chunks arrive
        // without busy-waiting; this drain catches anything that came in
        // alongside the broadcast.recv path.
        if let Some(ref mut chain) = transcode_chain {
            while let Some(transcoded) = chain.try_recv() {
                forward_downstream(
                    &transcoded,
                    now_us(),
                    &mut null_padder,
                    &mut pad_scratch,
                    &mut pid_overrides_rewriter,
                    &mut overrides_scratch,
                    &mut pid_remapper,
                    &mut remap_scratch,
                    &mut av_align_buf,
                    &mut align_scratch,
                    &mut ts_buf,
                    &mut ts_sync_found,
                    &config.id,
                    &stats,
                    &wire_tx,
                );
            }
        }
    }

    Ok(())
}


