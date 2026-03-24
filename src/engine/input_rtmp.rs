// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! RTMP input — accepts incoming publish connections and remuxes to MPEG-TS.
//!
//! This module runs an RTMP server that accepts one publisher at a time (OBS,
//! ffmpeg, Wirecast, etc.). The received H.264 video and AAC audio are remuxed
//! into MPEG-TS packets and pushed into the broadcast channel as `RtpPacket`
//! structs, identical to how SRT and RTP inputs work.
//!
//! ## Data flow
//!
//! ```text
//! [OBS/ffmpeg] --(RTMP/TCP)--> [RTMP server] --(FLV tags)--> [TS muxer] --(TS packets)--> [broadcast channel]
//! ```
//!
//! ## Configuration
//!
//! ```json
//! {
//!   "type": "rtmp",
//!   "listen_addr": "0.0.0.0:1935",
//!   "app": "live",
//!   "stream_key": "my_secret_key"
//! }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtmpInputConfig;
use crate::stats::collector::FlowStatsAccumulator;

use super::packet::RtpPacket;
use super::rtmp::server::{RtmpMediaMessage, RtmpServerConfig, run_rtmp_server};
use super::rtmp::ts_mux::TsMuxer;

/// Spawn the RTMP input task.
///
/// Returns a `JoinHandle` for the task. The task runs until `cancel` is triggered
/// or the RTMP server encounters a fatal error.
pub fn spawn_rtmp_input(
    config: RtmpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("RTMP input starting on {} (app='{}')", config.listen_addr, config.app);

        let (media_tx, media_rx) = mpsc::channel::<RtmpMediaMessage>(1024);
        let is_publishing = Arc::new(AtomicBool::new(false));

        let server_config = RtmpServerConfig {
            listen_addr: config.listen_addr.clone(),
            expected_app: config.app.clone(),
            expected_stream_key: config.stream_key.clone(),
        };

        // Spawn the RTMP server
        let cancel_server = cancel.clone();
        let is_pub = is_publishing.clone();
        tokio::spawn(async move {
            if let Err(e) = run_rtmp_server(server_config, media_tx, is_pub, cancel_server).await {
                tracing::error!("RTMP server error: {e:#}");
            }
        });

        // Process media messages from the RTMP server
        process_media(media_rx, broadcast_tx, stats, cancel).await;
    })
}

/// Process media messages from the RTMP server, mux into TS, and push to broadcast.
async fn process_media(
    mut media_rx: mpsc::Receiver<RtmpMediaMessage>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) {
    let mut muxer = TsMuxer::new();
    let mut seq_num: u16 = 0;
    let mut has_sent_sps_pps = false;
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut audio_sample_rate_idx: u8 = 4; // default 44.1kHz
    let mut audio_channels: u8 = 2; // default stereo

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTMP input shutting down");
                return;
            }
            msg = media_rx.recv() => {
                match msg {
                    Some(RtmpMediaMessage::Video { data, timestamp_ms }) => {
                        if data.len() < 5 {
                            continue;
                        }

                        let frame_type = (data[0] >> 4) & 0x0F;
                        let codec_id = data[0] & 0x0F;
                        let avc_packet_type = data[1];

                        // Only handle AVC (H.264)
                        if codec_id != 7 {
                            continue;
                        }

                        match avc_packet_type {
                            0 => {
                                // AVC sequence header (SPS/PPS)
                                if let Some((s, p)) = parse_avc_decoder_config(&data[5..]) {
                                    sps = Some(s);
                                    pps = Some(p);
                                    muxer.set_has_audio(true); // Assume audio until proven otherwise
                                    tracing::info!("RTMP: received AVC sequence header (SPS+PPS)");
                                }
                            }
                            1 => {
                                // AVC NALU data
                                let is_keyframe = frame_type == 1;
                                let composition_time = ((data[2] as i32) << 16 | (data[3] as i32) << 8 | data[4] as i32) as i32;
                                // Sign extend 24-bit
                                let composition_time = if composition_time & 0x800000 != 0 {
                                    composition_time | !0xFFFFFF
                                } else {
                                    composition_time
                                };

                                let dts_ms = timestamp_ms as i64;
                                let pts_ms = dts_ms + composition_time as i64;

                                let dts_90khz = (dts_ms * 90) as u64;
                                let pts_90khz = (pts_ms.max(0) * 90) as u64;

                                // Convert length-prefixed NALUs to Annex B
                                let annex_b = length_prefixed_to_annex_b(&data[5..], &sps, &pps, is_keyframe && !has_sent_sps_pps);

                                if is_keyframe && sps.is_some() {
                                    has_sent_sps_pps = true;
                                }

                                let ts_packets = muxer.mux_video(&annex_b, pts_90khz, dts_90khz, is_keyframe);

                                for pkt_data in ts_packets {
                                    let recv_time = std::time::Instant::now().elapsed().as_micros() as u64;
                                    let packet = RtpPacket {
                                        data: pkt_data,
                                        sequence_number: seq_num,
                                        rtp_timestamp: dts_90khz as u32,
                                        recv_time_us: recv_time,
                                        is_raw_ts: true,
                                    };
                                    seq_num = seq_num.wrapping_add(1);
                                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                                    stats.input_bytes.fetch_add(188, Ordering::Relaxed);
                                    let _ = broadcast_tx.send(packet);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(RtmpMediaMessage::Audio { data, timestamp_ms }) => {
                        if data.len() < 2 {
                            continue;
                        }

                        let sound_format = (data[0] >> 4) & 0x0F;
                        let aac_packet_type = data[1];

                        // Only handle AAC
                        if sound_format != 10 {
                            continue;
                        }

                        match aac_packet_type {
                            0 => {
                                // AAC sequence header (AudioSpecificConfig)
                                if data.len() >= 4 {
                                    let asc = &data[2..];
                                    // Parse AudioSpecificConfig (2 bytes min)
                                    // Bits: [audioObjectType:5][frequencyIndex:4][channelConfiguration:4]...
                                    let freq_idx = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
                                    let ch_cfg = (asc[1] >> 3) & 0x0F;
                                    audio_sample_rate_idx = freq_idx;
                                    audio_channels = ch_cfg;
                                    muxer.set_has_audio(true);
                                    tracing::info!("RTMP: received AAC sequence header (freq_idx={freq_idx}, channels={ch_cfg})");
                                }
                            }
                            1 => {
                                // Raw AAC frame
                                let raw_aac = &data[2..];
                                let pts_90khz = (timestamp_ms as u64) * 90;

                                let ts_packets = muxer.mux_audio(raw_aac, pts_90khz, audio_sample_rate_idx, audio_channels);

                                for pkt_data in ts_packets {
                                    let recv_time = std::time::Instant::now().elapsed().as_micros() as u64;
                                    let packet = RtpPacket {
                                        data: pkt_data,
                                        sequence_number: seq_num,
                                        rtp_timestamp: pts_90khz as u32,
                                        recv_time_us: recv_time,
                                        is_raw_ts: true,
                                    };
                                    seq_num = seq_num.wrapping_add(1);
                                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                                    stats.input_bytes.fetch_add(188, Ordering::Relaxed);
                                    let _ = broadcast_tx.send(packet);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(RtmpMediaMessage::Metadata) => {
                        // Could extract resolution, bitrate, etc. from onMetaData
                        tracing::debug!("RTMP: received metadata");
                    }
                    Some(RtmpMediaMessage::Disconnected) => {
                        tracing::info!("RTMP publisher disconnected, waiting for reconnection");
                        has_sent_sps_pps = false;
                    }
                    None => {
                        // Channel closed
                        tracing::info!("RTMP input channel closed");
                        return;
                    }
                }
            }
        }
    }
}

/// Parse AVCDecoderConfigurationRecord to extract SPS and PPS.
fn parse_avc_decoder_config(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if data.len() < 8 {
        return None;
    }

    // Skip: configurationVersion(1) + AVCProfileIndication(1) + profile_compat(1)
    //       + AVCLevelIndication(1) + lengthSizeMinusOne(1)
    let mut pos = 5;

    // numOfSequenceParameterSets (lower 5 bits)
    if pos >= data.len() {
        return None;
    }
    let num_sps = (data[pos] & 0x1F) as usize;
    pos += 1;

    let mut sps_data = None;
    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            return None;
        }
        let sps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + sps_len > data.len() {
            return None;
        }
        sps_data = Some(data[pos..pos + sps_len].to_vec());
        pos += sps_len;
    }

    // numOfPictureParameterSets
    if pos >= data.len() {
        return None;
    }
    let num_pps = data[pos] as usize;
    pos += 1;

    let mut pps_data = None;
    for _ in 0..num_pps {
        if pos + 2 > data.len() {
            return None;
        }
        let pps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + pps_len > data.len() {
            return None;
        }
        pps_data = Some(data[pos..pos + pps_len].to_vec());
        pos += pps_len;
    }

    match (sps_data, pps_data) {
        (Some(s), Some(p)) => Some((s, p)),
        _ => None,
    }
}

/// Convert length-prefixed NALUs to Annex B format (start codes).
/// Optionally prepend SPS/PPS for keyframes.
fn length_prefixed_to_annex_b(
    data: &[u8],
    sps: &Option<Vec<u8>>,
    pps: &Option<Vec<u8>>,
    prepend_sps_pps: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 128);

    // Prepend SPS/PPS with start codes before keyframes
    if prepend_sps_pps {
        if let Some(s) = sps {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(s);
        }
        if let Some(p) = pps {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(p);
        }
    }

    // Convert each length-prefixed NALU to Annex B
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let nalu_len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + nalu_len > data.len() {
            break;
        }

        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + nalu_len]);
        pos += nalu_len;
    }

    out
}
