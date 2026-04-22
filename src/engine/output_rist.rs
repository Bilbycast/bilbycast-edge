// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RIST Simple Profile (TR-06-1:2020) output.
//!
//! Subscribes to the flow's broadcast channel, runs the optional program
//! filter / audio encode / video encode / delay-buffer stages (identical
//! to the UDP and RTP outputs), and transmits aligned 7×188-byte MPEG-TS
//! datagrams via [`rist_transport::RistSocket::sender`]. The underlying
//! RistSocket wraps each payload in RTP, performs NACK-driven
//! retransmission, and exchanges RTCP with the peer.
//!
//! Optional SMPTE 2022-7 redundancy: a second `RistSocket::sender` is
//! instantiated with a separate local bind and a separate remote address;
//! every outbound datagram is duplicated to both legs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use rist_transport::{RistSocket, RistSocketConfig};

use crate::config::models::{RistOutputConfig, RistOutputRedundancyConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::time::now_us;

use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::ts_audio_replace::TsAudioReplacer;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;
use super::ts_video_replace::TsVideoReplacer;

const TS_SYNC_BYTE: u8 = 0x47;
const TS_PACKET_SIZE: usize = 188;
const TS_PACKETS_PER_DATAGRAM: usize = 7;
const TS_SYNC_CONFIRM_COUNT: usize = 3;
const RTP_HEADER_MIN_SIZE: usize = 12;

/// Spawn a RIST output task.
pub fn spawn_rist_output(
    config: RistOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("ts".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
    });

    tokio::spawn(async move {
        if let Err(e) = rist_output_loop(
            &config,
            &mut rx,
            output_stats,
            cancel,
            frame_rate_rx,
            &event_sender,
            &flow_id,
        )
        .await
        {
            tracing::error!("RIST output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::RIST,
                format!("RIST output '{}' exited with error: {e}", config.id),
                &flow_id,
            );
        }
    })
}

fn socket_config_for_sender(
    cname: Option<&str>,
    buffer_ms: Option<u32>,
    rtcp_interval_ms: Option<u32>,
    retransmit_buffer_capacity: Option<usize>,
    local_addr: SocketAddr,
) -> RistSocketConfig {
    let mut sc = RistSocketConfig::default();
    sc.local_addr = local_addr;
    if let Some(ms) = buffer_ms {
        sc.buffer_size = Duration::from_millis(ms as u64);
    }
    if let Some(ms) = rtcp_interval_ms {
        sc.rtcp_interval = Duration::from_millis(ms as u64);
    }
    if let Some(cap) = retransmit_buffer_capacity {
        sc.retransmit_buffer_capacity = cap;
    }
    sc.cname = cname.map(|s| s.to_string());
    sc
}

async fn build_sender(
    remote: SocketAddr,
    local_addr: Option<&str>,
    cname: Option<&str>,
    buffer_ms: Option<u32>,
    rtcp_interval_ms: Option<u32>,
    retransmit_buffer_capacity: Option<usize>,
) -> anyhow::Result<RistSocket> {
    // RistChannel::bind computes RTCP port = rtp_port + 1 using the REQUESTED
    // port, not the OS-assigned port, so passing port 0 makes RTCP land on
    // port 1 (a privileged port on Linux). Pick an explicit even port in the
    // dynamic range and retry on collision.
    if let Some(addr) = local_addr {
        let sc = socket_config_for_sender(
            cname,
            buffer_ms,
            rtcp_interval_ms,
            retransmit_buffer_capacity,
            addr.parse()?,
        );
        return RistSocket::sender(sc, remote)
            .await
            .map_err(|e| anyhow::anyhow!("RIST sender bind/connect failed: {e}"));
    }

    let bind_ip = if remote.is_ipv4() { "0.0.0.0" } else { "[::]" };
    for _ in 0..32 {
        // Even port in [49152, 65534] (IANA dynamic/private range).
        let port: u16 = (49152 + (rand::random::<u16>() % 8192) * 2).min(65534);
        let local_str = format!("{}:{}", bind_ip, port & !1);
        let local: SocketAddr = local_str.parse().unwrap();
        let sc = socket_config_for_sender(
            cname,
            buffer_ms,
            rtcp_interval_ms,
            retransmit_buffer_capacity,
            local,
        );
        match RistSocket::sender(sc, remote).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                tracing::debug!(
                    "RIST sender bind on {local_str} failed ({e}), retrying with a different port"
                );
                continue;
            }
        }
    }
    Err(anyhow::anyhow!(
        "RIST sender: exhausted 32 bind attempts for ephemeral even port; set local_addr explicitly"
    ))
}

async fn rist_output_loop(
    config: &RistOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    events: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    let remote: SocketAddr = config.remote_addr.parse()?;

    let socket_leg1 = build_sender(
        remote,
        config.local_addr.as_deref(),
        config.cname.as_deref(),
        config.buffer_ms,
        config.rtcp_interval_ms,
        config.retransmit_buffer_capacity,
    )
    .await?;
    stats.set_rist_stats(socket_leg1.stats());

    let socket_leg2: Option<RistSocket> = if let Some(ref red) = config.redundancy {
        let s = build_sender_redundancy(red, config).await?;
        stats.set_rist_leg2_stats(s.stats());
        Some(s)
    } else {
        None
    };

    tracing::info!(
        "RIST output '{}' started -> {}{}",
        config.id,
        remote,
        socket_leg2
            .as_ref()
            .map(|_| format!(" (+2022-7 leg2 -> {})", config.redundancy.as_ref().unwrap().remote_addr))
            .unwrap_or_default()
    );
    events.emit_flow(
        EventSeverity::Info,
        category::RIST,
        format!("RIST output '{}' connected -> {}", config.id, remote),
        flow_id,
    );

    let mut program_filter = config.program_number.map(TsProgramFilter::new);
    let mut filter_scratch: Vec<u8> = Vec::new();

    let mut pid_remapper = config.pid_map.as_ref().and_then(|m| {
        let r = TsPidRemapper::new(m);
        if r.is_active() {
            tracing::info!(
                "RIST output '{}': pid_map active ({} entries)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut remap_scratch: Vec<u8> = Vec::new();

    let mut audio_replacer = match config.audio_encode.as_ref() {
        Some(enc) => match TsAudioReplacer::new(enc, config.transcode.clone()) {
            Ok(r) => {
                tracing::info!(
                    "RIST output '{}': audio_encode active ({})",
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
                    "RIST output '{}': audio_encode rejected: {e}; audio will be left untouched",
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
        },
        None => None,
    };
    let mut replace_scratch: Vec<u8> = Vec::new();

    let mut video_replacer = match config.video_encode.as_ref() {
        Some(enc) => match TsVideoReplacer::new(enc, None) {
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
                    "RIST output '{}': video_encode active ({})",
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
                    "RIST output '{}': video_encode rejected: {e}; video will be left untouched",
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
        },
        None => None,
    };
    let mut video_replace_scratch: Vec<u8> = Vec::new();

    let mut ts_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut ts_sync_found = false;

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
                delay_sleep
                    .as_mut()
                    .reset(Instant::now() + Duration::from_micros(wait));
            }
        }

        let mut packets_to_send: Vec<RtpPacket> = Vec::new();

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RIST output '{}' stopping (cancelled)", config.id);
                socket_leg1.close();
                if let Some(s) = socket_leg2 { s.close(); }
                return Ok(());
            }
            result = rx.recv() => match result {
                Ok(packet) => {
                    if let Some(ref mut db) = delay_buf {
                        db.push(packet);
                    } else {
                        packets_to_send.push(packet);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    tracing::warn!("RIST output '{}' lagged, dropped {n} packets", config.id);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("RIST output '{}' broadcast channel closed", config.id);
                    break;
                }
            },
            _ = &mut delay_sleep, if delay_buf.as_ref().map_or(false, |db| db.len() > 0) => {
                let db = delay_buf.as_mut().unwrap();
                let now = now_us();
                for packet in db.drain_ready(now) {
                    packets_to_send.push(packet);
                }
            }
        }

        for packet in &packets_to_send {
            let ts_data = if packet.is_raw_ts {
                &packet.data[..]
            } else if packet.data.len() > RTP_HEADER_MIN_SIZE {
                &packet.data[RTP_HEADER_MIN_SIZE..]
            } else {
                continue;
            };

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

            let after_audio: &[u8] = if let Some(ref mut replacer) = audio_replacer {
                replace_scratch.clear();
                tokio::task::block_in_place(|| {
                    replacer.process(filtered_bytes, &mut replace_scratch);
                });
                &replace_scratch
            } else {
                filtered_bytes
            };

            let after_video: &[u8] = if let Some(ref mut vreplacer) = video_replacer {
                video_replace_scratch.clear();
                tokio::task::block_in_place(|| {
                    vreplacer.process(after_audio, &mut video_replace_scratch);
                });
                &video_replace_scratch
            } else {
                after_audio
            };

            if let Some(ref mut remapper) = pid_remapper {
                remap_scratch.clear();
                remapper.process(after_video, &mut remap_scratch);
                ts_buf.extend_from_slice(&remap_scratch);
            } else {
                ts_buf.extend_from_slice(after_video);
            }

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
                            let _ = ts_buf.split_to(offset);
                        }
                        ts_sync_found = true;
                    } else {
                        ts_buf.clear();
                    }
                }
            }

            if ts_sync_found {
                while ts_buf.len() >= ts_datagram_size {
                    let datagram: Bytes = ts_buf.split_to(ts_datagram_size).freeze();
                    if let Err(e) = socket_leg1.send(datagram.clone()).await {
                        tracing::warn!("RIST output '{}' leg 1 send error: {e}", config.id);
                    } else {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes_sent
                            .fetch_add(datagram.len() as u64, Ordering::Relaxed);
                        stats.record_latency(packet.recv_time_us);
                    }
                    if let Some(ref s2) = socket_leg2 {
                        if let Err(e) = s2.send(datagram).await {
                            tracing::warn!(
                                "RIST output '{}' leg 2 send error: {e}",
                                config.id
                            );
                        }
                    }
                }
            }
        }
    }

    socket_leg1.close();
    if let Some(s) = socket_leg2 {
        s.close();
    }
    Ok(())
}

async fn build_sender_redundancy(
    red: &RistOutputRedundancyConfig,
    parent: &RistOutputConfig,
) -> anyhow::Result<RistSocket> {
    let remote: SocketAddr = red.remote_addr.parse()?;
    build_sender(
        remote,
        red.local_addr.as_deref(),
        parent.cname.as_deref(),
        parent.buffer_ms,
        parent.rtcp_interval_ms,
        parent.retransmit_buffer_capacity,
    )
    .await
}
