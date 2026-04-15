// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::BytesMut;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::models::UdpOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;
use crate::util::time::now_us;

use super::audio_302m::S302mOutputPipeline;
use super::audio_transcode::InputFormat;
use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::ts_audio_replace::TsAudioReplacer;
use super::ts_program_filter::TsProgramFilter;
use super::ts_video_replace::TsVideoReplacer;

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
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

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
        if matches!(config.transport_mode.as_deref(), Some("audio_302m")) {
            if let Err(e) =
                udp_output_loop_302m(&config, &mut rx, output_stats, cancel, input_format).await
            {
                tracing::error!(
                    "UDP output '{}' (audio_302m) exited with error: {e}",
                    config.id
                );
            }
        } else if let Err(e) = udp_output_loop(&config, &mut rx, output_stats, cancel, frame_rate_rx).await {
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
) -> anyhow::Result<()> {
    let (socket, dest) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
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
            return udp_output_loop(config, rx, stats, cancel, None).await;
        }
    };

    let out_channels = match input.channels {
        1 => 2,
        n if n % 2 == 0 && n <= 8 => n,
        n if n > 8 => 8,
        _ => input.channels + 1,
    };
    let mut pipeline = match S302mOutputPipeline::new(input, out_channels, 24, 4_000) {
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
                            match socket.send_to(&datagram, dest).await {
                                Ok(sent) => {
                                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                    stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                    stats.record_latency(packet.recv_time_us);
                                }
                                Err(e) => {
                                    tracing::warn!("UDP output '{}' (302M) send error: {e}", config.id);
                                }
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
) -> anyhow::Result<()> {
    let (socket, dest) =
        create_udp_output(&config.dest_addr, config.bind_addr.as_deref(), config.interface_addr.as_deref(), config.dscp).await?;

    tracing::info!("UDP output '{}' started -> {}", config.id, dest);

    // Optional MPTS → SPTS program filter.
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "UDP output '{}': program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    // Optional audio ES replacement (decode + re-encode audio in the TS).
    let mut audio_replacer = match config.audio_encode.as_ref() {
        Some(enc) => match TsAudioReplacer::new(enc) {
            Ok(r) => {
                tracing::info!(
                    "UDP output '{}': audio_encode active ({})",
                    config.id,
                    r.target_description()
                );
                Some(r)
            }
            Err(e) => {
                tracing::error!(
                    "UDP output '{}': audio_encode rejected: {e}; audio will be left untouched",
                    config.id
                );
                None
            }
        },
        None => None,
    };
    let mut replace_scratch: Vec<u8> = Vec::new();

    // Optional video ES replacement (decode + re-encode video in the TS).
    let mut video_replacer = match config.video_encode.as_ref() {
        Some(enc) => match TsVideoReplacer::new(enc) {
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
                    "UDP output '{}': video_encode active ({})",
                    config.id,
                    r.target_description()
                );
                Some(r)
            }
            Err(e) => {
                tracing::error!(
                    "UDP output '{}': video_encode rejected: {e}; video will be left untouched",
                    config.id
                );
                None
            }
        },
        None => None,
    };
    let mut video_replace_scratch: Vec<u8> = Vec::new();

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

    loop {
        if let Some(ref db) = delay_buf {
            if let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }
        }

        let mut packets_to_send: Vec<RtpPacket> = Vec::new();

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

        for packet in &packets_to_send {
            // Extract raw TS payload: strip RTP header if present
            let ts_data = if packet.is_raw_ts {
                &packet.data[..]
            } else if packet.data.len() > RTP_HEADER_MIN_SIZE {
                &packet.data[RTP_HEADER_MIN_SIZE..]
            } else {
                continue;
            };

            // Apply program filter (MPTS → SPTS) if configured, producing
            // `filtered_bytes` (borrow or scratch).
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

            // Apply audio ES replacement if configured. The replacer owns
            // its own TS state — it rewrites the PMT, decodes+re-encodes
            // audio PES, and leaves video / other PIDs untouched.
            let after_audio: &[u8] = if let Some(ref mut replacer) = audio_replacer {
                replace_scratch.clear();
                tokio::task::block_in_place(|| {
                    replacer.process(filtered_bytes, &mut replace_scratch);
                });
                &replace_scratch
            } else {
                filtered_bytes
            };

            // Apply video ES replacement if configured. Runs after audio
            // so both transforms stack cleanly on the same TS stream.
            if let Some(ref mut vreplacer) = video_replacer {
                video_replace_scratch.clear();
                tokio::task::block_in_place(|| {
                    vreplacer.process(after_audio, &mut video_replace_scratch);
                });
                ts_buf.extend_from_slice(&video_replace_scratch);
            } else {
                ts_buf.extend_from_slice(after_audio);
            }

            // Find TS sync boundary on first data
            if !ts_sync_found {
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
                                config.id, offset, offset
                            );
                            let _ = ts_buf.split_to(offset);
                        } else {
                            tracing::info!(
                                "UDP output '{}': TS sync found (already aligned)",
                                config.id
                            );
                        }
                        ts_sync_found = true;
                    } else {
                        tracing::warn!(
                            "UDP output '{}': no TS sync found in {} bytes, discarding",
                            config.id, ts_buf.len()
                        );
                        ts_buf.clear();
                    }
                }
            }

            // Send complete TS-aligned datagrams
            if ts_sync_found {
                while ts_buf.len() >= ts_datagram_size {
                    let datagram = ts_buf.split_to(ts_datagram_size);
                    match socket.send_to(&datagram, dest).await {
                        Ok(sent) => {
                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                            stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                            stats.record_latency(packet.recv_time_us);
                        }
                        Err(e) => {
                            tracing::warn!("UDP output '{}' send error: {e}", config.id);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
