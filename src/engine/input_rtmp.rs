// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;

use super::input_transcode::{publish_input_packet, InputTranscoder};
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
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
    force_idr: Arc<std::sync::atomic::AtomicBool>,
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
        let server_event_sender = event_sender.clone();
        let server_flow_id = flow_id.clone();
        let server_listen_addr = config.listen_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = run_rtmp_server(server_config, media_tx, is_pub, cancel_server).await {
                tracing::error!("RTMP server error: {e:#}");
                use crate::manager::events::{BindProto, BindScope};
                let scope = BindScope::flow(&server_flow_id);
                if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                    server_event_sender.emit_port_conflict(
                        "RTMP server",
                        &server_listen_addr,
                        BindProto::Tcp,
                        scope,
                        &e,
                    );
                } else {
                    server_event_sender.emit_flow_with_details(
                        EventSeverity::Critical, category::RTMP,
                        format!("RTMP server error: {e}"),
                        &server_flow_id,
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
            }
        });

        // Process media messages from the RTMP server
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("RTMP input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("RTMP input: transcode setup failed, passthrough: {e}");
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
        process_media(media_rx, broadcast_tx, stats, cancel, event_sender, flow_id, &mut transcoder).await;
    })
}

/// Process media messages from the RTMP server, mux into TS, and push to broadcast.
async fn process_media(
    mut media_rx: mpsc::Receiver<RtmpMediaMessage>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    transcoder: &mut Option<InputTranscoder>,
) {
    let mut muxer = TsMuxer::new();
    let mut seq_num: u16 = 0;
    let mut has_sent_sps_pps = false;
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut audio_sample_rate_idx: u8 = 4; // default 44.1kHz
    let mut audio_channels: u8 = 2; // default stereo

    // Audio PTS derivation state. RTMP carries millisecond-resolution
    // timestamps but real AAC frames are 1024 samples ≈ 21.33 ms (48 kHz)
    // or 23.22 ms (44.1 kHz). Multiplying ms*90 produces PES PTS deltas
    // that oscillate between 23 ms and 24 ms (or 21 ms / 22 ms) — close
    // on average but never exactly the AAC frame duration. Downstream
    // tools that decode the AAC and re-derive frame timing from a
    // sample counter (ffmpeg's `-f null` PCM pipeline, every audio
    // muxer with strict DTS checks) flag the resulting irregular spacing
    // as "non monotonically increasing dts" — see Bug #11 in the
    // 2026-04-09 test report.
    //
    // The fix is to anchor a sample counter to the FIRST audio frame's
    // RTMP timestamp and emit every subsequent PES PTS as exactly
    // `anchor + frames * 1024 * 90000 / sample_rate`. This produces a
    // strictly monotonic, evenly-spaced sequence whose deltas exactly
    // match the AAC nominal frame duration, satisfying every downstream
    // muxer regardless of how it computes DTS.
    let mut audio_anchor_pts_90khz: Option<u64> = None;
    let mut audio_frames_emitted: u64 = 0;

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
                                    event_sender.emit_flow(
                                        EventSeverity::Info,
                                        category::RTMP,
                                        "RTMP publisher connected",
                                        &flow_id,
                                    );
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

                                // Bundle all TS packets from this frame into one RtpPacket
                                if !ts_packets.is_empty() {
                                    let total_len: usize = ts_packets.iter().map(|c| c.len()).sum();
                                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                                    for chunk in &ts_packets {
                                        combined.extend_from_slice(chunk);
                                    }
                                    let recv_time = wall_clock_micros();
                                    let packet = RtpPacket {
                                        data: combined.freeze(),
                                        sequence_number: seq_num,
                                        rtp_timestamp: dts_90khz as u32,
                                        recv_time_us: recv_time,
                                        is_raw_ts: true,
                                    };
                                    seq_num = seq_num.wrapping_add(1);
                                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                                    stats.input_bytes.fetch_add(total_len as u64, Ordering::Relaxed);
                                    if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                        publish_input_packet(transcoder, &broadcast_tx, packet);
                                    } else {
                                        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                    }
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

                                // Sample-counter-derived PTS — see
                                // `audio_anchor_pts_90khz` doc above for the
                                // rationale (Bug #11). The first frame
                                // anchors to its RTMP-supplied wall time;
                                // subsequent frames are spaced exactly one
                                // AAC frame duration apart at 90 kHz.
                                let sample_rate_hz = aac_sample_rate_hz(audio_sample_rate_idx);
                                let anchor = *audio_anchor_pts_90khz
                                    .get_or_insert_with(|| (timestamp_ms as u64) * 90);
                                let pts_90khz = anchor
                                    + audio_frames_emitted * 1024 * 90_000 / sample_rate_hz as u64;
                                audio_frames_emitted += 1;

                                let ts_packets = muxer.mux_audio(raw_aac, pts_90khz, audio_sample_rate_idx, audio_channels);

                                if !ts_packets.is_empty() {
                                    let total_len: usize = ts_packets.iter().map(|c| c.len()).sum();
                                    let mut combined = bytes::BytesMut::with_capacity(total_len);
                                    for chunk in &ts_packets {
                                        combined.extend_from_slice(chunk);
                                    }
                                    let recv_time = wall_clock_micros();
                                    let packet = RtpPacket {
                                        data: combined.freeze(),
                                        sequence_number: seq_num,
                                        rtp_timestamp: pts_90khz as u32,
                                        recv_time_us: recv_time,
                                        is_raw_ts: true,
                                    };
                                    seq_num = seq_num.wrapping_add(1);
                                    stats.input_packets.fetch_add(1, Ordering::Relaxed);
                                    stats.input_bytes.fetch_add(total_len as u64, Ordering::Relaxed);
                                    if !stats.bandwidth_blocked.load(Ordering::Relaxed) {
                                        publish_input_packet(transcoder, &broadcast_tx, packet);
                                    } else {
                                        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                    }
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
                        event_sender.emit_flow(
                            EventSeverity::Warning,
                            category::RTMP,
                            "RTMP publisher disconnected",
                            &flow_id,
                        );
                        has_sent_sps_pps = false;
                        // Reset audio anchor so the next publisher restarts
                        // the sample counter from its own RTMP wall time.
                        audio_anchor_pts_90khz = None;
                        audio_frames_emitted = 0;
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

/// Map an MPEG-4 AAC `samplingFrequencyIndex` (per ISO 14496-3 §1.6.3.4
/// Table 1.18) to the actual sample rate in Hz. Used to derive the
/// per-frame PTS increment for the audio sample-counter clock (Bug #11).
///
/// Returns 44100 for unknown indices, matching the legacy
/// `audio_sample_rate_idx` default in [`process_media`]. The escape
/// value (idx 15, "explicit frequency in next 24 bits") is unsupported
/// in the AAC sequence headers we receive over RTMP and falls through
/// to the default.
fn aac_sample_rate_hz(idx: u8) -> u32 {
    match idx {
        0 => 96_000,
        1 => 88_200,
        2 => 64_000,
        3 => 48_000,
        4 => 44_100,
        5 => 32_000,
        6 => 24_000,
        7 => 22_050,
        8 => 16_000,
        9 => 12_000,
        10 => 11_025,
        11 => 8_000,
        12 => 7_350,
        _ => 44_100,
    }
}

/// Wall-clock microseconds since the Unix epoch.
///
/// Used as the `recv_time_us` field on `RtpPacket`s emitted by the RTMP
/// input. Downstream consumers (notably the HLS output) compare this
/// against the segment-start time to decide when to cut a new segment, so
/// it must advance with real wall-clock time. The previous implementation
/// used `Instant::now().elapsed()` which always returns ~0 µs (the elapsed
/// time since the freshly-constructed Instant), so HLS segment cutting
/// from RTMP-sourced flows never crossed the duration threshold and
/// produced zero segments — see the 2026-04-09 Bug B fix in QUALITY_REPORT.md.
fn wall_clock_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for Bug B (2026-04-09): RTMP packets must carry a
    /// monotonically advancing wall-clock `recv_time_us` so the HLS output
    /// can compare consecutive packets and decide when to cut a segment.
    /// The previous implementation called `Instant::now().elapsed()` which
    /// returns the duration since the *just-constructed* Instant — i.e.
    /// approximately zero microseconds, every single call. With every
    /// packet stamped at ~0 µs the HLS segment-boundary check
    /// `elapsed_us >= segment_duration_us` never fired and zero segments
    /// were uploaded.
    #[test]
    fn wall_clock_micros_advances_between_calls() {
        let t1 = wall_clock_micros();
        // Sleep enough to be visibly larger than any plausible per-call
        // jitter in CI.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t2 = wall_clock_micros();
        assert!(
            t2 > t1 + 10_000,
            "wall_clock_micros() did not advance: t1={t1} t2={t2}"
        );
        // And the absolute value must look like a real Unix timestamp,
        // not a tiny number of microseconds since process start. Anything
        // after 2026-01-01 is fine.
        let jan_2026_us: u64 = 1_767_225_600_000_000;
        assert!(
            t1 > jan_2026_us,
            "wall_clock_micros() returned a non-Unix value: {t1}"
        );
    }
}
