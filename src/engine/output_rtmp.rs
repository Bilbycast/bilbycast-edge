// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! RTMP output task — publishes demuxed H.264/AAC to an RTMP server.
//!
//! Subscribes to the flow's broadcast channel, demuxes MPEG-TS into
//! H.264 + AAC elementary streams, wraps them in FLV tags, and publishes
//! them via the RTMP client to servers like Twitch, YouTube, etc.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::{BufMut, BytesMut};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtmpOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;
use super::rtmp::client::RtmpClient;
use super::ts_demux::{DemuxedFrame, TsDemuxer};

/// Spawn an async task that consumes RTP packets from the broadcast channel,
/// demuxes H.264/AAC from the MPEG-TS payload, and publishes to an RTMP server.
pub fn spawn_rtmp_output(
    config: RtmpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    tokio::spawn(async move {
        tracing::info!(
            "RTMP output '{}' started -> {}",
            config.id,
            config.dest_url,
        );

        let mut attempt = 0u32;
        loop {
            if cancel.is_cancelled() {
                break;
            }

            attempt += 1;
            if let Some(max) = config.max_reconnect_attempts {
                if attempt > max + 1 {
                    tracing::error!(
                        "RTMP output '{}': exceeded max reconnect attempts ({})",
                        config.id, max,
                    );
                    break;
                }
            }

            if attempt > 1 {
                tracing::info!(
                    "RTMP output '{}': reconnecting (attempt {})",
                    config.id, attempt,
                );
            }

            // Connect to RTMP server
            let mut client = match RtmpClient::connect(&config.dest_url, &config.stream_key).await {
                Ok(c) => {
                    tracing::info!("RTMP output '{}': connected to {}", config.id, config.dest_url);
                    attempt = 0; // reset on successful connect
                    c
                }
                Err(e) => {
                    tracing::warn!(
                        "RTMP output '{}': connection failed: {:#}",
                        config.id, e,
                    );
                    wait_or_cancel(&cancel, config.reconnect_delay_secs).await;
                    continue;
                }
            };

            // Run the publish loop
            let err = publish_loop(
                &config, &mut client, &mut rx, &output_stats, &cancel,
            ).await;

            let _ = client.close().await;

            match err {
                Ok(()) => {
                    // Cancelled
                    tracing::info!("RTMP output '{}' cancelled", config.id);
                    break;
                }
                Err(e) => {
                    tracing::warn!("RTMP output '{}': publish error: {:#}", config.id, e);
                    wait_or_cancel(&cancel, config.reconnect_delay_secs).await;
                }
            }
        }
    })
}

/// Main publish loop: demux TS → build FLV tags → send via RTMP.
/// Returns Ok(()) when cancelled, Err on connection/send failure.
async fn publish_loop(
    config: &RtmpOutputConfig,
    client: &mut RtmpClient,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: &Arc<OutputStatsAccumulator>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let mut demuxer = TsDemuxer::new();
    let mut sent_video_header = false;
    let mut sent_audio_header = false;
    let mut base_pts: Option<u64> = None;
    loop {
        let packet = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = rx.recv() => {
                match result {
                    Ok(pkt) => pkt,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("RTMP output '{}' lagged by {n} packets", config.id);
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("RTMP output '{}' channel closed", config.id);
                        return Ok(());
                    }
                }
            }
        };

        // Extract TS payload (strip RTP header if needed)
        let ts_data = if packet.is_raw_ts {
            &packet.data[..]
        } else {
            // RTP header is at least 12 bytes
            if packet.data.len() < 12 {
                continue;
            }
            let cc = (packet.data[0] & 0x0F) as usize;
            let header_len = 12 + cc * 4;
            if packet.data.len() <= header_len {
                continue;
            }
            &packet.data[header_len..]
        };

        // Demux TS into elementary stream frames
        let frames = demuxer.demux(ts_data);

        for frame in frames {
            match frame {
                DemuxedFrame::H264 { nalus, pts, is_keyframe } => {
                    let ts_ms = pts_to_ms(pts, &mut base_pts);

                    // Send sequence header on first keyframe
                    if is_keyframe && !sent_video_header {
                        if let (Some(sps), Some(pps)) = (demuxer.cached_sps(), demuxer.cached_pps()) {
                            let header = build_avc_sequence_header(sps, pps);
                            client.send_video(&header, ts_ms).await?;
                            sent_video_header = true;
                            tracing::debug!("RTMP output '{}': sent AVC sequence header", config.id);
                        }
                    }

                    // Don't send video data before sequence header
                    if !sent_video_header {
                        continue;
                    }

                    // Re-send sequence header on each keyframe (some servers need this)
                    if is_keyframe && sent_video_header {
                        if let (Some(sps), Some(pps)) = (demuxer.cached_sps(), demuxer.cached_pps()) {
                            let header = build_avc_sequence_header(sps, pps);
                            client.send_video(&header, ts_ms).await?;
                        }
                    }

                    // Build FLV video NALU tag
                    let tag = build_avc_nalu_tag(&nalus, is_keyframe);
                    let tag_len = tag.len();
                    client.send_video(&tag, ts_ms).await?;
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
                }
                DemuxedFrame::Aac { data, pts } => {
                    let ts_ms = pts_to_ms(pts, &mut base_pts);

                    // Send audio sequence header on first AAC frame
                    if !sent_audio_header {
                        if let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() {
                            let header = build_aac_sequence_header(profile, sr_idx, ch_cfg);
                            client.send_audio(&header, ts_ms).await?;
                            sent_audio_header = true;
                            tracing::debug!("RTMP output '{}': sent AAC sequence header", config.id);
                        }
                    }

                    if !sent_audio_header {
                        continue;
                    }

                    // Build FLV audio raw tag
                    let tag = build_aac_raw_tag(&data);
                    let tag_len = tag.len();
                    client.send_audio(&tag, ts_ms).await?;
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
                }
                DemuxedFrame::Opus => {
                    // RTMP doesn't support Opus — skip
                }
            }
        }

        // Flush after each TS packet batch to keep delivery smooth
        client.flush().await?;
    }
}

/// Convert 90kHz PTS to milliseconds relative to the first frame.
fn pts_to_ms(pts: u64, base_pts: &mut Option<u64>) -> u32 {
    let base = *base_pts.get_or_insert(pts);
    let delta = pts.wrapping_sub(base);
    (delta / 90) as u32
}

/// Build an AVC sequence header FLV video tag (AVCDecoderConfigurationRecord).
fn build_avc_sequence_header(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(16 + sps.len() + pps.len());

    // FLV video tag header
    buf.put_u8(0x17); // keyframe (1) + AVC (7)
    buf.put_u8(0x00); // AVC sequence header
    buf.put_u8(0x00); // composition time
    buf.put_u8(0x00);
    buf.put_u8(0x00);

    // AVCDecoderConfigurationRecord
    buf.put_u8(1); // configurationVersion
    buf.put_u8(if sps.len() > 1 { sps[1] } else { 66 }); // AVCProfileIndication
    buf.put_u8(if sps.len() > 2 { sps[2] } else { 0 });   // profile_compatibility
    buf.put_u8(if sps.len() > 3 { sps[3] } else { 30 });  // AVCLevelIndication
    buf.put_u8(0xFF); // lengthSizeMinusOne = 3 (4-byte NALU lengths) | reserved 0xFC
    buf.put_u8(0xE1); // numOfSequenceParameterSets = 1 | reserved 0xE0

    // SPS
    buf.put_u16(sps.len() as u16);
    buf.put_slice(sps);

    // PPS
    buf.put_u8(1); // numOfPictureParameterSets
    buf.put_u16(pps.len() as u16);
    buf.put_slice(pps);

    buf.to_vec()
}

/// Build an FLV video tag with length-prefixed NALUs.
fn build_avc_nalu_tag(nalus: &[Vec<u8>], is_keyframe: bool) -> Vec<u8> {
    // Calculate total payload size
    let payload_size: usize = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut buf = BytesMut::with_capacity(5 + payload_size);

    // FLV video tag header
    let frame_type: u8 = if is_keyframe { 0x17 } else { 0x27 }; // keyframe/inter + AVC
    buf.put_u8(frame_type);
    buf.put_u8(0x01); // AVC NALU
    buf.put_u8(0x00); // composition time offset
    buf.put_u8(0x00);
    buf.put_u8(0x00);

    // Length-prefixed NALUs
    for nalu in nalus {
        buf.put_u32(nalu.len() as u32);
        buf.put_slice(nalu);
    }

    buf.to_vec()
}

/// Build an AAC sequence header FLV audio tag (AudioSpecificConfig).
fn build_aac_sequence_header(profile: u8, sample_rate_idx: u8, channel_config: u8) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(4);

    // FLV audio tag header
    buf.put_u8(0xAF); // AAC + 44kHz + 16-bit + stereo (standard for AAC in FLV)
    buf.put_u8(0x00); // AAC sequence header

    // AudioSpecificConfig (2 bytes)
    // audioObjectType (5 bits) = profile + 1 (AAC-LC = 2)
    // samplingFrequencyIndex (4 bits)
    // channelConfiguration (4 bits)
    // remaining bits = 0
    let aot = (profile + 1) & 0x1F;
    let byte0 = (aot << 3) | (sample_rate_idx >> 1);
    let byte1 = (sample_rate_idx << 7) | (channel_config << 3);
    buf.put_u8(byte0);
    buf.put_u8(byte1);

    buf.to_vec()
}

/// Build an FLV audio tag with raw AAC frame data.
fn build_aac_raw_tag(aac_data: &[u8]) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(2 + aac_data.len());

    // FLV audio tag header
    buf.put_u8(0xAF); // AAC + 44kHz + 16-bit + stereo
    buf.put_u8(0x01); // AAC raw

    buf.put_slice(aac_data);

    buf.to_vec()
}

/// Wait for a duration or until cancelled.
async fn wait_or_cancel(cancel: &CancellationToken, secs: u64) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
    }
}
