// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! RTSP input — pulls H.264/AAC from RTSP sources (IP cameras, media servers).
//!
//! Uses the `retina` pure-Rust RTSP client for signaling and RTP reception.
//! Received H.264 NALUs and AAC frames are muxed into MPEG-TS and published
//! to the flow's broadcast channel, identical to other input types.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{RtspInputConfig, RtspTransport};
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::FlowStatsAccumulator;

use super::packet::RtpPacket;
use super::rtmp::ts_mux::TsMuxer;

/// Spawn an RTSP input task.
///
/// Connects to the RTSP source, receives H.264 video (and optionally AAC audio),
/// muxes into MPEG-TS, and publishes `RtpPacket` to the broadcast channel.
/// Automatically reconnects on connection loss with configurable delay.
pub fn spawn_rtsp_input(
    config: RtspInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("RTSP input started, connecting to {}", config.rtsp_url);
        rtsp_input_loop(config, broadcast_tx, stats, cancel, event_sender, flow_id).await;
    })
}

async fn rtsp_input_loop(
    config: RtspInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) {
    let reconnect_delay = std::time::Duration::from_secs(config.reconnect_delay_secs);
    // Track whether we need to emit a disconnect event. Set to true when
    // connected, cleared after emitting the disconnect event. This prevents
    // spamming the same warning on every reconnection attempt.
    let mut disconnect_event_pending = false;

    loop {
        match run_rtsp_session(&config, &broadcast_tx, &stats, &cancel, &event_sender, &flow_id, &mut disconnect_event_pending).await {
            Ok(()) => {
                tracing::info!("RTSP input stopped (cancelled)");
                break;
            }
            Err(e) => {
                tracing::warn!(
                    "RTSP connection lost: {}. Reconnecting in {}s...",
                    e,
                    config.reconnect_delay_secs
                );
                // Emit disconnect event once per connection cycle (not on every retry)
                if disconnect_event_pending {
                    event_sender.emit_flow(
                        EventSeverity::Warning,
                        "rtsp",
                        format!("RTSP input disconnected: {e}. Reconnecting in {}s", config.reconnect_delay_secs),
                        &flow_id,
                    );
                    disconnect_event_pending = false;
                }
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::info!("RTSP input stopped during reconnect wait");
                        break;
                    }
                    _ = tokio::time::sleep(reconnect_delay) => continue,
                }
            }
        }
    }
}

async fn run_rtsp_session(
    config: &RtspInputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
    disconnect_event_pending: &mut bool,
) -> anyhow::Result<()> {
    use retina::client::{PlayOptions, SessionGroup, SetupOptions};
    use retina::codec::{CodecItem, FrameFormat};

    let parsed_url = url::Url::parse(&config.rtsp_url)?;

    // Build session options with optional credentials
    let mut session_opts = retina::client::SessionOptions::default()
        .session_group(Arc::new(SessionGroup::default()));

    if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        session_opts = session_opts.creds(Some(retina::client::Credentials {
            username: user.clone(),
            password: pass.clone(),
        }));
    }

    // Connect and DESCRIBE
    let mut session = retina::client::Session::describe(parsed_url, session_opts).await?;

    // Select transport
    let transport = match config.transport {
        RtspTransport::Tcp => retina::client::Transport::Tcp(Default::default()),
        RtspTransport::Udp => retina::client::Transport::Udp(Default::default()),
    };

    // Discover streams and setup video/audio with Annex B framing
    let mut has_video = false;
    let mut has_audio = false;
    let mut is_h265 = false;
    let mut streams_to_setup: Vec<(usize, &str)> = Vec::new();

    for i in 0..session.streams().len() {
        let stream = &session.streams()[i];
        let media = stream.media();
        let encoding = stream.encoding_name();

        if media == "video" && encoding.eq_ignore_ascii_case("h264") {
            streams_to_setup.push((i, "video"));
        } else if media == "video" && encoding.eq_ignore_ascii_case("h265") {
            streams_to_setup.push((i, "video"));
            is_h265 = true;
        } else if media == "audio"
            && (encoding.eq_ignore_ascii_case("mpeg4-generic")
                || encoding.eq_ignore_ascii_case("aac"))
        {
            streams_to_setup.push((i, "audio"));
        }
    }

    for (i, kind) in &streams_to_setup {
        let opts = SetupOptions::default()
            .transport(transport.clone())
            .frame_format(FrameFormat::SIMPLE);
        session.setup(*i, opts).await?;
        if *kind == "video" {
            has_video = true;
        } else if *kind == "audio" {
            has_audio = true;
        }
        tracing::info!("RTSP: setup {} stream {}", kind, i);
    }

    // PLAY
    let mut demuxed = session
        .play(PlayOptions::default())
        .await?
        .demuxed()?;

    tracing::info!("RTSP: connected and playing from {}", config.rtsp_url);
    event_sender.emit_flow(
        EventSeverity::Info,
        "rtsp",
        format!("RTSP connected to {}", config.rtsp_url),
        flow_id,
    );
    *disconnect_event_pending = true;

    let mut ts_muxer = TsMuxer::new();
    ts_muxer.set_has_video(has_video);
    ts_muxer.set_has_audio(has_audio);
    if is_h265 {
        ts_muxer.set_video_stream_type(super::rtmp::ts_mux::STREAM_TYPE_H265);
    }
    let mut seq_num: u16 = 0;

    // Receive loop
    loop {
        let item = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            item = demuxed.next() => {
                match item {
                    Some(Ok(item)) => item,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow::anyhow!("RTSP stream ended")),
                }
            }
        };

        match item {
            CodecItem::VideoFrame(frame) => {
                let is_keyframe = frame.is_random_access_point();
                let pts_90khz = frame.timestamp().elapsed().max(0) as u64;
                let data = frame.into_data();

                // FrameFormat::SIMPLE gives Annex B — TsMuxer expects this
                let ts_chunks = ts_muxer.mux_video(&data, pts_90khz, pts_90khz, is_keyframe);

                // Bundle all TS packets from this frame into a single RtpPacket
                // to reduce broadcast channel pressure (individual 188-byte packets
                // cause media analyzer lag and missed PAT/PMT detection)
                if !ts_chunks.is_empty() {
                    let total_len: usize = ts_chunks.iter().map(|c| c.len()).sum();
                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                    for chunk in &ts_chunks {
                        combined.extend_from_slice(chunk);
                    }
                    let pkt = RtpPacket {
                        data: combined.freeze(),
                        sequence_number: seq_num,
                        rtp_timestamp: pts_90khz as u32,
                        recv_time_us: crate::util::time::now_us(),
                        is_raw_ts: true,
                    };
                    seq_num = seq_num.wrapping_add(1);
                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                    stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                    let _ = broadcast_tx.send(pkt);
                }
            }
            CodecItem::AudioFrame(frame) => {
                let pts_90khz = frame.timestamp().elapsed().max(0) as u64;
                let data = frame.data();

                // FrameFormat::SIMPLE gives ADTS-wrapped AAC
                // TsMuxer.mux_audio expects raw AAC + sample_rate_idx + channels
                // ADTS header contains this info; for now pass through as raw
                // Default: 48kHz (idx=3), stereo (2ch)
                let ts_chunks = ts_muxer.mux_audio(data, pts_90khz, 3, 2);

                if !ts_chunks.is_empty() {
                    let total_len: usize = ts_chunks.iter().map(|c| c.len()).sum();
                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                    for chunk in &ts_chunks {
                        combined.extend_from_slice(chunk);
                    }
                    let pkt = RtpPacket {
                        data: combined.freeze(),
                        sequence_number: seq_num,
                        rtp_timestamp: pts_90khz as u32,
                        recv_time_us: crate::util::time::now_us(),
                        is_raw_ts: true,
                    };
                    seq_num = seq_num.wrapping_add(1);
                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                    stats.input_bytes.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                    let _ = broadcast_tx.send(pkt);
                }
            }
            _ => {} // SenderReport, MessageFrame, etc.
        }
    }
}
