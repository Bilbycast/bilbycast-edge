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
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::OutputStatsAccumulator;

use super::audio_decode::{AacDecoder, sample_rate_from_index};
use super::audio_encode::{AudioCodec, AudioEncoder, AudioEncoderError, EncoderParams};
use super::packet::RtpPacket;
use super::rtmp::client::RtmpClient;
use super::ts_demux::{DemuxedFrame, TsDemuxer};

/// Per-output encoder state for the audio_encode bridge. Built lazily on
/// the first AAC frame so we can read the demuxer's cached AAC config and
/// decide whether to fast-path passthrough or actually decode + re-encode.
enum EncoderState {
    /// audio_encode is unset. Passthrough every AAC frame as-is.
    Disabled,
    /// audio_encode is set but we haven't seen the first AAC frame yet.
    Lazy,
    /// audio_encode is set and the source AAC config matches the requested
    /// codec / SR / channels — fast-path passthrough with no decode/encode.
    Transparent,
    /// Decoder + encoder are running. Each AAC frame goes through them.
    Active {
        decoder: AacDecoder,
        encoder: AudioEncoder,
    },
    /// Decoder or encoder construction failed once. Drop audio for the
    /// rest of the output's lifetime; the failure event was already
    /// emitted on the first frame attempt.
    Failed,
}

/// Spawn an async task that consumes RTP packets from the broadcast channel,
/// demuxes H.264/AAC from the MPEG-TS payload, and publishes to an RTMP server.
///
/// `compressed_audio_input` is the flag computed once per flow in `flow.rs`
/// from `audio_decode::input_can_carry_ts_audio`. When the input cannot
/// carry TS audio (e.g. PCM-only sources like ST 2110-30) and the output
/// nonetheless has `audio_encode` set, the encoder will refuse to start
/// at first-frame time and emit a failure event.
pub fn spawn_rtmp_output(
    config: RtmpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    compressed_audio_input: bool,
    flow_id: String,
    event_sender: EventSender,
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
                compressed_audio_input, &flow_id, &event_sender,
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
    compressed_audio_input: bool,
    flow_id: &str,
    event_sender: &EventSender,
) -> anyhow::Result<()> {
    let mut demuxer = TsDemuxer::new(config.program_number);
    let mut sent_video_header = false;
    let mut sent_audio_header = false;
    let mut base_pts: Option<u64> = None;
    // Lazy encoder: built on first AAC frame so we can read the demuxer's
    // cached AAC config and decide between Transparent / Active / Failed.
    let mut encoder_state: EncoderState = if config.audio_encode.is_some() {
        EncoderState::Lazy
    } else {
        EncoderState::Disabled
    };
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

                    // Lazy: build the encoder once we have the first
                    // ADTS frame (so we can read its profile / SR / ch).
                    if matches!(encoder_state, EncoderState::Lazy) {
                        encoder_state = build_encoder_state(
                            config,
                            &demuxer,
                            compressed_audio_input,
                            cancel,
                            stats,
                            flow_id,
                            event_sender,
                        );
                    }

                    // Send the FLV AAC sequence header on first frame.
                    // Always uses AOT=2 (AAC-LC) — even for HE-AAC output,
                    // most RTMP servers (Twitch, YouTube, nginx-rtmp)
                    // expect AOT=2 in the ASC and detect SBR / PS from
                    // the bitstream itself. This matches what `ffmpeg
                    // -c:a aac -profile:a aac_he -f flv` writes.
                    if !sent_audio_header {
                        if let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() {
                            let _ = profile;
                            // For Transparent / Disabled paths, write the
                            // ASC from the demuxer's cached config so the
                            // sample rate matches the source.
                            // For Active path, the encoder may resample;
                            // pull the resolved target SR/ch from the
                            // encoder params.
                            let (asc_sr_idx, asc_ch_cfg) = match &encoder_state {
                                EncoderState::Active { encoder, .. } => {
                                    let p = encoder.params();
                                    (
                                        sr_index_from_hz(p.target_sample_rate).unwrap_or(sr_idx),
                                        p.target_channels,
                                    )
                                }
                                _ => (sr_idx, ch_cfg),
                            };
                            // ASC uses AOT=2 (AAC-LC) → ADTS profile=1.
                            let header = build_aac_sequence_header(1, asc_sr_idx, asc_ch_cfg);
                            client.send_audio(&header, ts_ms).await?;
                            sent_audio_header = true;
                            tracing::debug!("RTMP output '{}': sent AAC sequence header", config.id);
                        }
                    }

                    if !sent_audio_header {
                        continue;
                    }

                    match &mut encoder_state {
                        EncoderState::Disabled | EncoderState::Transparent => {
                            // Existing passthrough path: write the raw
                            // ADTS-stripped AAC frame as an FLV audio tag.
                            let tag = build_aac_raw_tag(&data);
                            let tag_len = tag.len();
                            client.send_audio(&tag, ts_ms).await?;
                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                            stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
                        }
                        EncoderState::Active { decoder, encoder } => {
                            // Decode AAC → planar f32 → submit to encoder.
                            // Errors are logged once at debug; we don't
                            // tear down the output on a single bad frame.
                            match decoder.decode_frame(&data) {
                                Ok(planar) => {
                                    encoder.submit_planar(&planar, pts);
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        "RTMP output '{}': AAC decode failed: {e}",
                                        config.id
                                    );
                                }
                            }
                            // Drain any encoded frames the encoder has
                            // ready and write them as FLV audio tags.
                            for frame in encoder.drain() {
                                let tag = build_aac_raw_tag(&frame.data);
                                let tag_len = tag.len();
                                client.send_audio(&tag, ts_ms).await?;
                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
                            }
                        }
                        EncoderState::Failed | EncoderState::Lazy => {
                            // Failed: drop audio silently for the rest of
                            // the output's lifetime. Lazy: should be
                            // unreachable now (we built it above), but
                            // fall through safely.
                        }
                    }
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

/// Resolve the audio_encode block and demuxer-cached AAC config into an
/// [`EncoderState`]. Called once on the first AAC frame.
///
/// - If the requested codec is `aac_lc` and the operator did not override
///   bitrate / SR / channels and the input is itself AAC-LC, returns
///   `Transparent` (zero-cost passthrough).
/// - If the input is non-AAC-LC, returns `Failed` after logging the
///   reason. (Phase A's decoder rejects HE-AAC, multichannel, etc.)
/// - If `compressed_audio_input` is false, the input cannot carry AAC at
///   all → `Failed`.
/// - Otherwise builds the AacDecoder + AudioEncoder. On any error
///   (ffmpeg missing, decoder profile reject, encoder spawn failure),
///   logs the reason and returns `Failed`.
fn build_encoder_state(
    config: &RtmpOutputConfig,
    demuxer: &TsDemuxer,
    compressed_audio_input: bool,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    event_sender: &EventSender,
) -> EncoderState {
    let Some(enc_cfg) = config.audio_encode.as_ref() else {
        return EncoderState::Disabled;
    };

    if !compressed_audio_input {
        let msg = format!(
            "RTMP output '{}': audio_encode is set but the flow input cannot carry TS audio (PCM-only source); audio will be dropped",
            config.id
        );
        tracing::error!("{msg}");
        event_sender.emit_flow(
            EventSeverity::Critical,
            crate::manager::events::category::AUDIO_ENCODE,
            msg,
            flow_id,
        );
        return EncoderState::Failed;
    }

    let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() else {
        tracing::warn!(
            "RTMP output '{}': audio_encode requested but demuxer has no cached AAC config yet; deferring",
            config.id
        );
        return EncoderState::Lazy;
    };

    if profile != 1 {
        let msg = format!(
            "RTMP output '{}': audio_encode requires AAC-LC input (ADTS profile=1, AOT=2), got profile={profile} (AOT={}); audio will be dropped",
            config.id,
            profile + 1
        );
        tracing::error!("{msg}");
        event_sender.emit_flow(
            EventSeverity::Critical,
            crate::manager::events::category::AUDIO_ENCODE,
            msg,
            flow_id,
        );
        return EncoderState::Failed;
    }

    let Some(input_sr) = sample_rate_from_index(sr_idx) else {
        tracing::error!(
            "RTMP output '{}': audio_encode rejected unsupported AAC sample_rate_index={sr_idx}",
            config.id
        );
        return EncoderState::Failed;
    };
    let input_ch = ch_cfg;
    if input_ch == 0 || input_ch > 2 {
        tracing::error!(
            "RTMP output '{}': audio_encode rejected unsupported AAC channel_config={input_ch}",
            config.id
        );
        return EncoderState::Failed;
    }

    let Some(codec) = AudioCodec::parse(&enc_cfg.codec) else {
        tracing::error!(
            "RTMP output '{}': audio_encode unknown codec '{}'",
            config.id,
            enc_cfg.codec
        );
        return EncoderState::Failed;
    };

    // Same-codec fast path: AAC-LC input → AAC-LC output, no overrides.
    let no_overrides = enc_cfg.bitrate_kbps.is_none()
        && enc_cfg.sample_rate.is_none()
        && enc_cfg.channels.is_none();
    if codec == AudioCodec::AacLc && no_overrides {
        tracing::info!(
            "RTMP output '{}': audio_encode same-codec passthrough (AAC-LC {} Hz {} ch)",
            config.id, input_sr, input_ch
        );
        return EncoderState::Transparent;
    }

    let target_sr = enc_cfg.sample_rate.unwrap_or(input_sr);
    let target_ch = enc_cfg.channels.unwrap_or(input_ch);
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: input_sr,
        channels: input_ch,
        target_bitrate_kbps: target_br,
        target_sample_rate: target_sr,
        target_channels: target_ch,
    };

    let decoder = match AacDecoder::from_adts_config(profile, sr_idx, ch_cfg) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                "RTMP output '{}': audio_encode AacDecoder build failed: {e}",
                config.id
            );
            return EncoderState::Failed;
        }
    };

    let encoder = match AudioEncoder::spawn(
        params,
        cancel.child_token(),
        flow_id.to_string(),
        config.id.clone(),
        stats.clone(),
        Some(event_sender.clone()),
    ) {
        Ok(e) => e,
        Err(AudioEncoderError::FfmpegNotFound) => {
            let msg = format!(
                "RTMP output '{}': audio_encode requires ffmpeg in PATH but it is not installed; audio will be dropped",
                config.id
            );
            tracing::error!("{msg}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                crate::manager::events::category::AUDIO_ENCODE,
                msg,
                flow_id,
            );
            return EncoderState::Failed;
        }
        Err(e) => {
            let msg = format!(
                "RTMP output '{}': audio_encode encoder spawn failed: {e}",
                config.id
            );
            tracing::error!("{msg}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                crate::manager::events::category::AUDIO_ENCODE,
                msg,
                flow_id,
            );
            return EncoderState::Failed;
        }
    };

    tracing::info!(
        "RTMP output '{}': audio_encode active codec={} {}->{} Hz {}->{} ch {} kbps",
        config.id,
        encoder.params().codec.as_str(),
        input_sr, target_sr,
        input_ch, target_ch,
        target_br,
    );
    EncoderState::Active { decoder, encoder }
}

/// Reverse of `sample_rate_from_index`: maps a sample rate (Hz) to its
/// 4-bit ADTS sample_rate_index field. Returns `None` for non-standard
/// rates. Used when building the FLV ASC for an encoder that resampled.
fn sr_index_from_hz(hz: u32) -> Option<u8> {
    Some(match hz {
        96_000 => 0,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        7_350 => 12,
        _ => return None,
    })
}
