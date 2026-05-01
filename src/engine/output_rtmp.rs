// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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

use crate::config::models::{RtmpOutputConfig, VideoEncodeConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;

use super::audio_decode::{AacDecoder, DecodeStats, sample_rate_from_index, sr_index_from_hz};
use super::audio_encode::{AudioCodec, AudioEncoder, AudioEncoderError, EncoderParams};
use super::audio_silence::SilenceGenerator;
use super::packet::RtpPacket;
use super::rtmp::client::RtmpClient;
use super::ts_demux::{DemuxedFrame, TsDemuxer};
use super::ts_video_replace::VideoEncodeStats;

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
    /// When `silent_fallback` is set we build the encoder eagerly (with
    /// declared target params) before any source audio arrives, so the
    /// decoder and transcoder are lazily filled the first time real AAC
    /// shows up — hence both are `Option`.
    Active {
        /// `None` until the first real AAC frame arrives (silent-fallback
        /// builds the encoder ahead of any source audio).
        decoder: Option<AacDecoder>,
        encoder: AudioEncoder,
        decode_stats: Arc<DecodeStats>,
        /// Optional planar PCM shuffle / resample stage between decoder and
        /// encoder. `None` unless `config.transcode` was set. Passthrough
        /// fast-paths inside `PlanarAudioTranscoder` mean an empty block is
        /// effectively a clone.
        transcoder: Option<super::audio_transcode::PlanarAudioTranscoder>,
        /// Retained to re-plan the transcoder when silent-fallback sees
        /// its first real AAC frame and needs to resample from the source
        /// rate/channels to the encoder's declared input. `None` in the
        /// legacy path (transcoder was already built against real params).
        pending_transcode_cfg: Option<super::audio_transcode::TranscodeJson>,
        /// Silent-PCM generator + audio-drop watchdog. `Some` iff
        /// `audio_encode.silent_fallback = true`.
        silence: Option<SilenceGenerator>,
    },
    /// Decoder or encoder construction failed once. Drop audio for the
    /// rest of the output's lifetime; the failure event was already
    /// emitted on the first frame attempt.
    Failed,
}

/// Per-output video transcoding state. Mirrors the shape of
/// [`EncoderState`] for audio: lazy-open on the first source frame,
/// fallback to drop-video on any error.
///
/// The decoder is opened as soon as we learn the source `stream_type`
/// from the PMT (H.264 = 0x1B, HEVC = 0x24). The encoder is deferred
/// until we get the first decoded frame, because the encoder needs the
/// source resolution — the `video_encode` config may leave `width` /
/// `height` unset and we currently use the source resolution.
enum VideoEncoderState {
    /// `video_encode` is unset. Passthrough every frame verbatim (H.264
    /// via classic FLV, HEVC via Enhanced RTMP).
    Disabled,
    /// `video_encode` is set; we haven't built the decoder + encoder yet.
    #[cfg(feature = "video-thumbnail")]
    Lazy { cfg: VideoEncodeConfig },
    /// `video_encode` pipeline is live.
    #[cfg(feature = "video-thumbnail")]
    Active(Box<VideoActive>),
    /// Decoder or encoder construction failed; drop video for the rest
    /// of the output's lifetime. The failure event was already emitted.
    Failed,
}

#[cfg(feature = "video-thumbnail")]
struct VideoActive {
    decoder: video_engine::VideoDecoder,
    /// Shared encoder pipeline — wraps `VideoEncoder` + optional
    /// `VideoScaler`. Lazy-opens on the first decoded frame. When the
    /// operator sets `video_encode.width` / `.height` to values that
    /// differ from the source, the scaler Lanczos-resizes the decoded
    /// frame to the target before encoding, instead of letting
    /// libavcodec silently crop the top-left quadrant.
    pipeline: crate::engine::video_encode_util::ScaledVideoEncoder,
    target_family: video_codec::VideoCodec,
    /// Cached encoder fps — needed to convert encoder-PTS into FLV-style
    /// millisecond timestamps after each frame encode. Mirrors the
    /// numbers the pipeline uses at lazy-open time.
    fps_num: u32,
    fps_den: u32,
    /// Cached FLV sequence-header payload built from the encoder's
    /// `extradata` on first-encoder-open. `None` until the encoder opens
    /// and emits its out-of-band SPS/PPS (or VPS/SPS/PPS).
    sequence_header_tag: Option<Vec<u8>>,
    /// True once we've written the sequence header to the RTMP peer.
    sequence_header_sent: bool,
    /// Monotonic PTS anchor in the encoder's 1 / fps_num time base.
    out_frame_count: i64,
    stats: Arc<VideoEncodeStats>,
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

    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("flv".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
    });

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
                    // Bug #10 fix: exponential backoff (1, 2, 4, 8, 16 s,
                    // capped at max(reconnect_delay_secs, 30 s)). The old
                    // code reconnected every reconnect_delay_secs (default
                    // ~3 s) regardless of how many attempts had failed,
                    // hammering an unreachable receiver and flooding logs.
                    let backoff = rtmp_reconnect_backoff(
                        attempt,
                        config.reconnect_delay_secs,
                    );
                    wait_or_cancel(&cancel, backoff).await;
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
                    let backoff = rtmp_reconnect_backoff(
                        attempt,
                        config.reconnect_delay_secs,
                    );
                    wait_or_cancel(&cancel, backoff).await;
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
    let mut encoder_state: EncoderState = match &config.audio_encode {
        None => EncoderState::Disabled,
        Some(cfg) if cfg.silent_fallback => {
            // Build the encoder eagerly so the silence generator has a
            // sink before any source audio (if any) ever arrives.
            build_encoder_state_eager_for_silent_fallback(
                config, cancel, stats, flow_id, event_sender,
            )
        }
        Some(_) => EncoderState::Lazy,
    };
    // Silence interval: ticks at the silence generator's chunk cadence
    // (~21 ms @ 48 kHz / 1024 samples). Only present when
    // `silent_fallback` is active and the eager-build succeeded.
    let mut silence_interval: Option<tokio::time::Interval> =
        if let EncoderState::Active { silence: Some(sg), .. } = &encoder_state {
            let mut iv = tokio::time::interval(sg.chunk_duration());
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            Some(iv)
        } else {
            None
        };
    let mut video_state = init_video_encoder_state(
        config,
        stats,
        flow_id,
        event_sender,
    );
    loop {
        // Silence tick → inject one zero-filled chunk into the encoder
        // if the watchdog says we should (no real audio within grace),
        // then drain + write FLV audio tags.
        let silence_tick = async {
            match silence_interval.as_mut() {
                Some(iv) => {
                    iv.tick().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let packet = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = silence_tick => {
                if let Err(e) = emit_silence_if_needed(
                    config,
                    client,
                    &mut encoder_state,
                    &mut sent_audio_header,
                    stats,
                )
                .await
                {
                    return Err(e);
                }
                client.flush().await?;
                continue;
            }
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
        let recv_time_us = packet.recv_time_us;

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
                    if process_video_frame(
                        VideoFrameSource::H264 { nalus: &nalus, is_keyframe },
                        pts,
                        ts_ms,
                        recv_time_us,
                        &demuxer,
                        &mut sent_video_header,
                        &mut video_state,
                        client,
                        config,
                        stats,
                        flow_id,
                        event_sender,
                    )
                    .await?
                    {
                        // Success (or transient skip); nothing else to do.
                    }
                }
                DemuxedFrame::H265 { nalus, pts, is_keyframe } => {
                    let ts_ms = pts_to_ms(pts, &mut base_pts);
                    if process_video_frame(
                        VideoFrameSource::H265 { nalus: &nalus, is_keyframe },
                        pts,
                        ts_ms,
                        recv_time_us,
                        &demuxer,
                        &mut sent_video_header,
                        &mut video_state,
                        client,
                        config,
                        stats,
                        flow_id,
                        event_sender,
                    )
                    .await?
                    {
                    }
                }
                DemuxedFrame::Aac { data, pts } => {
                    let ts_ms = pts_to_ms(pts, &mut base_pts);

                    // Lazy: build the encoder once we have the first
                    // ADTS frame (so we can read its profile / SR / ch).
                    // Skipped when silent_fallback already eagerly built it.
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
                            stats.record_latency(recv_time_us);
                        }
                        EncoderState::Active {
                            decoder,
                            encoder,
                            decode_stats,
                            transcoder,
                            pending_transcode_cfg,
                            silence,
                        } => {
                            // Silent-fallback mode: lazily build decoder +
                            // resampler against the source's real AAC
                            // config on the first real frame.
                            if decoder.is_none() {
                                if let Some((profile, sr_idx, ch_cfg)) =
                                    demuxer.cached_aac_config()
                                {
                                    let encoder_params = encoder.params().clone();
                                    lazy_build_decoder_and_transcoder(
                                        decoder,
                                        transcoder,
                                        pending_transcode_cfg,
                                        &encoder_params,
                                        (profile, sr_idx, ch_cfg),
                                        &config.id,
                                    );
                                }
                            }
                            // Reset the drop watchdog: real audio is
                            // flowing, suppress silence until the next
                            // grace-window expiry.
                            if let Some(sg) = silence.as_mut() {
                                sg.mark_real_audio(pts);
                            }
                            let Some(dec) = decoder.as_mut() else {
                                // Decoder build failed (or we're still in
                                // silent-only mode with an unusable source
                                // codec). Silence stays active.
                                continue;
                            };
                            decode_stats.inc_input();
                            match dec.decode_frame(&data) {
                                Ok(planar) => {
                                    decode_stats.inc_output();
                                    if let Some(tc) = transcoder.as_mut() {
                                        match tc.process(&planar) {
                                            Ok(shuffled) => {
                                                encoder.submit_planar(&shuffled, pts);
                                            }
                                            Err(e) => {
                                                tracing::debug!(
                                                    "RTMP output '{}': transcode failed: {e}",
                                                    config.id
                                                );
                                            }
                                        }
                                    } else {
                                        encoder.submit_planar(&planar, pts);
                                    }
                                }
                                Err(e) => {
                                    decode_stats.inc_error();
                                    tracing::debug!(
                                        "RTMP output '{}': AAC decode failed: {e}",
                                        config.id
                                    );
                                }
                            }
                            // Drain any encoded frames the encoder has
                            // ready and write them as FLV audio tags.
                            let drained = encoder.drain();
                            if !drained.is_empty() {
                                tracing::debug!(
                                    "RTMP output '{}': drained {} encoded frames",
                                    config.id, drained.len()
                                );
                            }
                            for frame in drained {
                                let tag = build_aac_raw_tag(&frame.data);
                                let tag_len = tag.len();
                                // Use the encoder's per-frame PTS (90 kHz)
                                // for the FLV tag so AAC frames drained as
                                // a batch don't all share the source PES
                                // ts_ms — keeps DTS strictly monotonic.
                                let frame_ts_ms = (frame.pts / 90) as u32;
                                client.send_audio(&tag, frame_ts_ms).await?;
                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
                                stats.record_latency(recv_time_us);
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

/// FourCC identifier for HEVC in the Enhanced RTMP v2 extended VideoTagHeader.
const FOURCC_HVC1: [u8; 4] = [b'h', b'v', b'c', b'1'];

/// Build an Enhanced RTMP v2 SequenceStart tag payload for HEVC.
///
/// Layout: `0x90 "hvc1" <HEVCDecoderConfigurationRecord>`
/// - first byte: `IsExHeader(1) | FrameType(1=key) | PacketType(0=SequenceStart)`.
/// - next four bytes: ASCII FourCC `hvc1`.
/// - body: the hvcC blob emitted by libx265 / hevc_nvenc when the encoder is
///   opened with `global_header = true`.
fn build_hevc_sequence_header_from_hvcc(extradata: &[u8]) -> Vec<u8> {
    // libx265's `extradata` (with `global_header = true`) is an Annex-B
    // VPS+SPS+PPS bytestream, *not* a HEVCDecoderConfigurationRecord. The
    // MP4/Matroska muxers run `hevc_mp4toannexb` on the way out — we mux
    // into FLV ourselves, so we have to assemble the hvcC here. Detect a
    // pre-built hvcC by `configurationVersion == 0x01` as the first byte;
    // otherwise split the Annex-B and rebuild.
    let hvcc = if !extradata.is_empty() && extradata[0] == 0x01 {
        extradata.to_vec()
    } else {
        match build_hvcc_from_annex_b(extradata) {
            Some(blob) => blob,
            None => {
                tracing::warn!(
                    "RTMP video_encode: HEVC extradata had no VPS/SPS/PPS NALs ({} bytes)",
                    extradata.len()
                );
                return Vec::new();
            }
        }
    };
    let mut buf = BytesMut::with_capacity(5 + hvcc.len());
    buf.put_u8(0x80 | (1 << 4) | 0); // Ex | keyframe | PacketType=SequenceStart
    buf.put_slice(&FOURCC_HVC1);
    buf.put_slice(&hvcc);
    buf.to_vec()
}

/// Assemble a HEVCDecoderConfigurationRecord (per ISO/IEC 14496-15 §8.3.3.1.2)
/// from an Annex-B VPS/SPS/PPS bytestream as emitted by libx265. We pull
/// profile_space / tier / profile_idc / level_idc straight from the SPS
/// (which mirrors them from the VPS), and use safe defaults for the rest.
fn build_hvcc_from_annex_b(extradata: &[u8]) -> Option<Vec<u8>> {
    let nalus = super::ts_demux::split_annex_b_nalus(extradata);
    let mut vps: Option<&[u8]> = None;
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    for n in &nalus {
        if n.is_empty() { continue; }
        let t = (n[0] >> 1) & 0x3F;
        match t {
            32 if vps.is_none() => vps = Some(n),
            33 if sps.is_none() => sps = Some(n),
            34 if pps.is_none() => pps = Some(n),
            _ => {}
        }
    }
    let (vps, sps, pps) = match (vps, sps, pps) {
        (Some(v), Some(s), Some(p)) => (v, s, p),
        _ => return None,
    };
    // Pull profile/tier/level from the SPS profile_tier_level() structure,
    // which immediately follows the 4-bit sps_video_parameter_set_id +
    // 3-bit sps_max_sub_layers_minus1 + 1-bit sps_temporal_id_nesting_flag
    // = first byte after the 2-byte NAL header.
    if sps.len() < 2 + 12 { return None; }
    let ptl = &sps[2..];
    let profile_space_tier_profile = ptl[0];
    let profile_compat: [u8; 4] = [ptl[1], ptl[2], ptl[3], ptl[4]];
    let constraint: [u8; 6] = [ptl[5], ptl[6], ptl[7], ptl[8], ptl[9], ptl[10]];
    let level_idc = ptl[11];

    let mut out = Vec::with_capacity(64 + vps.len() + sps.len() + pps.len());
    out.push(0x01); // configurationVersion
    out.push(profile_space_tier_profile);
    out.extend_from_slice(&profile_compat);
    out.extend_from_slice(&constraint);
    out.push(level_idc);
    out.extend_from_slice(&[0xF0, 0x00]); // reserved | min_spatial_segmentation_idc=0
    out.push(0xFC); // reserved | parallelismType=0
    out.push(0xFD); // reserved | chroma_format_idc=1 (4:2:0)
    out.push(0xF8); // reserved | bitDepthLumaMinus8=0
    out.push(0xF8); // reserved | bitDepthChromaMinus8=0
    out.extend_from_slice(&[0x00, 0x00]); // avgFrameRate=0
    // constantFrameRate(2)=0 | numTemporalLayers(3)=1 | temporalIdNested(1)=1 | lengthSizeMinusOne(2)=3
    out.push((1 << 3) | (1 << 2) | 0x03);
    out.push(3); // numOfArrays = VPS, SPS, PPS

    for (nal_type, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        out.push(0x80 | nal_type); // array_completeness=1 | reserved | NAL_unit_type
        out.extend_from_slice(&[0x00, 0x01]); // numNalus = 1
        let len = nal.len() as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    Some(out)
}

/// Build an Enhanced RTMP v2 CodedFramesX tag payload for HEVC.
///
/// Uses `PacketType=3` (CodedFramesX) which omits the 3-byte composition time
/// offset that `CodedFrames` carries — we encode with `max_b_frames = 0`, so
/// CTS ≡ PTS ≡ DTS and the field would always be zero anyway.
///
/// Layout: `<0x93|0xA3> "hvc1" <AVCC-framed NAL units>`
fn build_hevc_coded_frames_tag(avcc_nalus: &[u8], is_keyframe: bool) -> Vec<u8> {
    let frame_type: u8 = if is_keyframe { 1 } else { 2 };
    let mut buf = BytesMut::with_capacity(5 + avcc_nalus.len());
    buf.put_u8(0x80 | (frame_type << 4) | 3); // Ex | key/inter | PacketType=CodedFramesX
    buf.put_slice(&FOURCC_HVC1);
    buf.put_slice(avcc_nalus);
    buf.to_vec()
}

/// Build a classic-FLV `AVCPacketType=1` NALU tag from an already-AVCC-framed
/// byte stream. Companion to [`build_avc_nalu_tag`] but skips the Vec<Vec<u8>>
/// round-trip when the caller already has length-prefixed NALUs in one buffer
/// (the output of [`annex_b_to_avcc`]).
fn build_avc_nalu_tag_raw(avcc_nalus: &[u8], is_keyframe: bool) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(5 + avcc_nalus.len());
    let frame_type: u8 = if is_keyframe { 0x17 } else { 0x27 };
    buf.put_u8(frame_type);
    buf.put_u8(0x01); // AVCPacketType = NALU
    buf.put_u8(0x00); // composition time offset (B-frames disabled at encoder)
    buf.put_u8(0x00);
    buf.put_u8(0x00);
    buf.put_slice(avcc_nalus);
    buf.to_vec()
}

/// Build a classic-FLV AVC sequence header tag from libx264's `extradata`.
///
/// libx264 emits Annex-B-framed SPS + PPS in extradata (start codes, no
/// length prefixes) — it does *not* produce a ready-made
/// AVCDecoderConfigurationRecord. The MP4/FLV muxer normally converts via
/// the `h264_mp4toannexb` BSF, but we mux into FLV ourselves so we have to
/// do the conversion here. We split on Annex-B, pick the SPS (type 7) and
/// the first PPS (type 8), and build a proper DCR via
/// `build_avc_sequence_header`. As a fallback, when extradata already
/// looks like a DCR (first byte = `0x01`), we wrap it directly.
fn build_avc_sequence_header_from_avcc(extradata: &[u8]) -> Vec<u8> {
    if !extradata.is_empty() && extradata[0] == 0x01 {
        let mut buf = BytesMut::with_capacity(5 + extradata.len());
        buf.put_u8(0x17);
        buf.put_u8(0x00);
        buf.put_u8(0x00);
        buf.put_u8(0x00);
        buf.put_u8(0x00);
        buf.put_slice(extradata);
        return buf.to_vec();
    }

    let nalus = super::ts_demux::split_annex_b_nalus(extradata);
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    for n in &nalus {
        if n.is_empty() { continue; }
        match n[0] & 0x1F {
            7 if sps.is_none() => sps = Some(n),
            8 if pps.is_none() => pps = Some(n),
            _ => {}
        }
    }
    match (sps, pps) {
        (Some(sps), Some(pps)) => build_avc_sequence_header(sps, pps),
        _ => {
            tracing::warn!(
                "RTMP video_encode: encoder extradata had no SPS/PPS NALs ({} bytes)",
                extradata.len()
            );
            Vec::new()
        }
    }
}

/// Convert an Annex-B byte stream (NAL units delimited by `00 00 00 01` /
/// `00 00 01` start codes) into AVCC framing (each NAL prefixed by a 4-byte
/// big-endian length, no start codes). Works for both H.264 and HEVC.
///
/// Used on the output side for:
/// - the classic-FLV AVC NALU tag body, and
/// - the Enhanced-RTMP `hvc1` `CodedFramesX` tag body.
pub(crate) fn annex_b_to_avcc(data: &[u8]) -> Vec<u8> {
    let nalus = super::ts_demux::split_annex_b_nalus(data);
    let mut out = Vec::with_capacity(data.len() + nalus.len() * 4);
    for nalu in nalus {
        out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        out.extend_from_slice(&nalu);
    }
    out
}

/// Concatenate a list of NAL units (as returned by the TS demuxer) back into
/// an Annex-B byte stream suitable for `VideoDecoder::send_packet`.
#[cfg(feature = "video-thumbnail")]
fn nalus_to_annex_b(nalus: &[Vec<u8>]) -> Vec<u8> {
    let total = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nalu in nalus {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nalu);
    }
    out
}

/// Borrowed view of the source NAL units for one access unit.
enum VideoFrameSource<'a> {
    H264 { nalus: &'a [Vec<u8>], is_keyframe: bool },
    H265 { nalus: &'a [Vec<u8>], is_keyframe: bool },
}

impl<'a> VideoFrameSource<'a> {
    fn nalus(&self) -> &'a [Vec<u8>] {
        match self {
            VideoFrameSource::H264 { nalus, .. } | VideoFrameSource::H265 { nalus, .. } => nalus,
        }
    }
    fn is_keyframe(&self) -> bool {
        match self {
            VideoFrameSource::H264 { is_keyframe, .. }
            | VideoFrameSource::H265 { is_keyframe, .. } => *is_keyframe,
        }
    }
    fn is_h264(&self) -> bool {
        matches!(self, VideoFrameSource::H264 { .. })
    }
}

/// Initialise the per-output video encoder state machine at startup.
///
/// If `video_encode` is unset, returns `Disabled` and video is passed
/// through untouched. Otherwise returns `Lazy` (or `Failed` when the
/// build has no `video-thumbnail` support at all).
fn init_video_encoder_state(
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    event_sender: &EventSender,
) -> VideoEncoderState {
    let Some(enc_cfg) = config.video_encode.as_ref() else {
        return VideoEncoderState::Disabled;
    };
    let _ = (stats, flow_id, event_sender);
    #[cfg(feature = "video-thumbnail")]
    {
        VideoEncoderState::Lazy { cfg: enc_cfg.clone() }
    }
    #[cfg(not(feature = "video-thumbnail"))]
    {
        let msg = format!(
            "RTMP output '{}': video_encode requested but this build lacks \
             the `video-thumbnail` feature (no in-process video codec library)",
            config.id
        );
        tracing::error!("{msg}");
        event_sender.emit_output_with_details(
            EventSeverity::Critical,
            category::VIDEO_ENCODE,
            msg,
            &config.id,
            serde_json::json!({ "codec": enc_cfg.codec }),
        );
        VideoEncoderState::Failed
    }
}

#[cfg(feature = "video-thumbnail")]
fn resolve_backend(
    codec: &str,
    output_id: &str,
    event_sender: &EventSender,
) -> Option<video_codec::VideoEncoderCodec> {
    match codec {
        "x264" => Some(video_codec::VideoEncoderCodec::X264),
        "x265" => Some(video_codec::VideoEncoderCodec::X265),
        "h264_nvenc" => Some(video_codec::VideoEncoderCodec::H264Nvenc),
        "hevc_nvenc" => Some(video_codec::VideoEncoderCodec::HevcNvenc),
        "h264_qsv" => Some(video_codec::VideoEncoderCodec::H264Qsv),
        "hevc_qsv" => Some(video_codec::VideoEncoderCodec::HevcQsv),
        other => {
            let msg = format!(
                "RTMP output '{}': video_encode unknown codec '{other}'",
                output_id
            );
            tracing::error!("{msg}");
            event_sender.emit_output_with_details(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                msg,
                output_id,
                serde_json::json!({ "codec": other }),
            );
            None
        }
    }
}

/// Core per-frame handler: dispatches passthrough vs transcode, sends the
/// necessary FLV tags to the RTMP peer, and updates per-output stats.
///
/// Returns `Ok(true)` on success (or a transient skip such as missing
/// sequence header on a non-keyframe), `Err` on RTMP send failure (which
/// the caller treats as a reconnect trigger).
#[allow(clippy::too_many_arguments)]
async fn process_video_frame(
    src: VideoFrameSource<'_>,
    pts_90k: u64,
    ts_ms: u32,
    recv_time_us: u64,
    demuxer: &TsDemuxer,
    sent_video_header: &mut bool,
    video_state: &mut VideoEncoderState,
    client: &mut RtmpClient,
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    event_sender: &EventSender,
) -> anyhow::Result<bool> {
    match video_state {
        VideoEncoderState::Disabled => {
            passthrough_video(&src, ts_ms, recv_time_us, demuxer, sent_video_header, client, config, stats).await
        }
        VideoEncoderState::Failed => {
            // Decoder/encoder already failed once — drop video silently
            // so the output keeps running audio.
            Ok(true)
        }
        #[cfg(feature = "video-thumbnail")]
        VideoEncoderState::Lazy { cfg } => {
            let cfg = cfg.clone();
            *video_state = open_video_active(
                &cfg,
                src.is_h264(),
                config,
                stats,
                event_sender,
            );
            if matches!(video_state, VideoEncoderState::Failed) {
                return Ok(true);
            }
            // Fall through to Active on the very same frame.
            encode_one_frame(&src, pts_90k, ts_ms, recv_time_us, sent_video_header, video_state, client, config, stats, flow_id, event_sender).await
        }
        #[cfg(feature = "video-thumbnail")]
        VideoEncoderState::Active(_) => {
            encode_one_frame(&src, pts_90k, ts_ms, recv_time_us, sent_video_header, video_state, client, config, stats, flow_id, event_sender).await
        }
    }
}

/// H.264 (classic FLV) or H.265 (Enhanced RTMP) passthrough — no re-encode.
#[allow(clippy::too_many_arguments)]
async fn passthrough_video(
    src: &VideoFrameSource<'_>,
    ts_ms: u32,
    recv_time_us: u64,
    demuxer: &TsDemuxer,
    sent_video_header: &mut bool,
    client: &mut RtmpClient,
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
) -> anyhow::Result<bool> {
    let nalus = src.nalus();
    let is_keyframe = src.is_keyframe();

    match src {
        VideoFrameSource::H264 { .. } => {
            // Send sequence header on the first frame where SPS/PPS are
            // cached. Real-world broadcast streams often use open-GOP /
            // recovery-point SEI signalling instead of true IDR (NAL
            // type 5), so waiting for `is_keyframe` would never fire
            // and video would never start. Receivers gracefully handle
            // the first NALU tag being non-IDR — they treat it as
            // "wait for next IDR" but the sequence header is what
            // they actually need to initialise the decoder.
            if !*sent_video_header {
                if let (Some(sps), Some(pps)) = (demuxer.cached_sps(), demuxer.cached_pps()) {
                    let header = build_avc_sequence_header(sps, pps);
                    client.send_video(&header, ts_ms).await?;
                    *sent_video_header = true;
                    tracing::debug!("RTMP output '{}': sent AVC sequence header", config.id);
                }
            }
            if !*sent_video_header {
                return Ok(true);
            }
            // Re-send sequence header on each keyframe (some servers need this)
            if is_keyframe {
                if let (Some(sps), Some(pps)) = (demuxer.cached_sps(), demuxer.cached_pps()) {
                    let header = build_avc_sequence_header(sps, pps);
                    client.send_video(&header, ts_ms).await?;
                }
            }
            let tag = build_avc_nalu_tag(nalus, is_keyframe);
            let tag_len = tag.len();
            client.send_video(&tag, ts_ms).await?;
            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
            stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
            stats.record_latency(recv_time_us);
        }
        VideoFrameSource::H265 { .. } => {
            // HEVC passthrough over Enhanced RTMP. Build hvcC from cached
            // VPS / SPS / PPS on the first frame where they're available.
            // Same rationale as the H.264 path: open-GOP broadcast streams
            // may not surface a true IDR (`is_keyframe`) for many seconds.
            if !*sent_video_header {
                if let Some(hvcc) = build_hvcc_from_cached(demuxer) {
                    let header = build_hevc_sequence_header_from_hvcc(&hvcc);
                    client.send_video(&header, ts_ms).await?;
                    *sent_video_header = true;
                    tracing::debug!("RTMP output '{}': sent HEVC sequence header", config.id);
                }
            }
            if !*sent_video_header {
                return Ok(true);
            }
            if is_keyframe {
                if let Some(hvcc) = build_hvcc_from_cached(demuxer) {
                    let header = build_hevc_sequence_header_from_hvcc(&hvcc);
                    client.send_video(&header, ts_ms).await?;
                }
            }
            // Filter out VPS/SPS/PPS from the coded-frames payload — they
            // travel out-of-band via the sequence header.
            let avcc = annex_b_to_avcc_filtered_h265(nalus);
            if avcc.is_empty() {
                return Ok(true);
            }
            let tag = build_hevc_coded_frames_tag(&avcc, is_keyframe);
            let tag_len = tag.len();
            client.send_video(&tag, ts_ms).await?;
            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
            stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
            stats.record_latency(recv_time_us);
        }
    }
    Ok(true)
}

/// Assemble an `HEVCDecoderConfigurationRecord` (hvcC) from the cached
/// VPS / SPS / PPS emitted by the TS demuxer. Returns `None` if any of the
/// three parameter sets is missing yet.
fn build_hvcc_from_cached(demuxer: &TsDemuxer) -> Option<Vec<u8>> {
    let vps = demuxer.cached_h265_vps()?;
    let sps = demuxer.cached_h265_sps()?;
    let pps = demuxer.cached_h265_pps()?;
    Some(build_hvcc(vps, sps, pps))
}

/// Build a minimal `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §8.3.2).
///
/// We do not have the full SPS/VPS/PPS parser needed to populate every
/// profile field, so the record's profile_tier_level bits are left at zero
/// — receivers that only care about parsing the payload (e.g. ffmpeg,
/// OBS Studio, Wowza with E-RTMP) accept this. Strict conformance checkers
/// may reject it; such profiles should use video_encode to let the encoder
/// emit its own hvcC.
fn build_hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut buf = BytesMut::new();
    // configurationVersion
    buf.put_u8(1);
    // general_profile_space (2) | general_tier_flag (1) | general_profile_idc (5)
    buf.put_u8(0);
    // general_profile_compatibility_flags (32 bits)
    buf.put_u32(0);
    // general_constraint_indicator_flags (48 bits)
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    // general_level_idc
    buf.put_u8(0);
    // min_spatial_segmentation_idc (12 bits) + reserved (4 bits)
    buf.put_u8(0xF0);
    buf.put_u8(0x00);
    // parallelismType (2 bits) + reserved (6 bits)
    buf.put_u8(0xFC);
    // chromaFormat (2 bits) + reserved (6 bits) — 0x01 = 4:2:0
    buf.put_u8(0xFC | 0x01);
    // bitDepthLumaMinus8 (3 bits) + reserved (5 bits)
    buf.put_u8(0xF8);
    // bitDepthChromaMinus8 (3 bits) + reserved (5 bits)
    buf.put_u8(0xF8);
    // avgFrameRate (16 bits)
    buf.put_u16(0);
    // constantFrameRate (2) | numTemporalLayers (3) | temporalIdNested (1) | lengthSizeMinusOne (2) = 0x0F
    buf.put_u8(0x0F);
    // numOfArrays
    buf.put_u8(3);
    // Array[0]: VPS (NAL type 32)
    push_hvcc_nal_array(&mut buf, 32, &[vps]);
    // Array[1]: SPS (NAL type 33)
    push_hvcc_nal_array(&mut buf, 33, &[sps]);
    // Array[2]: PPS (NAL type 34)
    push_hvcc_nal_array(&mut buf, 34, &[pps]);
    buf.to_vec()
}

fn push_hvcc_nal_array(buf: &mut BytesMut, nal_type: u8, nals: &[&[u8]]) {
    // array_completeness (1) | reserved (1) | nal_unit_type (6)
    buf.put_u8(0x80 | (nal_type & 0x3F));
    // numNalus
    buf.put_u16(nals.len() as u16);
    for n in nals {
        buf.put_u16(n.len() as u16);
        buf.put_slice(n);
    }
}

/// Convert a list of HEVC NALUs to AVCC framing, filtering out VPS / SPS /
/// PPS (which ride in the hvcC sequence header, not the coded-frames body).
fn annex_b_to_avcc_filtered_h265(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nalus {
        if n.is_empty() {
            continue;
        }
        let nal_type = (n[0] >> 1) & 0x3F;
        if matches!(nal_type, 32 | 33 | 34) {
            continue;
        }
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

#[cfg(feature = "video-thumbnail")]
fn open_video_active(
    cfg: &VideoEncodeConfig,
    source_is_h264: bool,
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    event_sender: &EventSender,
) -> VideoEncoderState {
    let Some(backend) = resolve_backend(&cfg.codec, &config.id, event_sender) else {
        return VideoEncoderState::Failed;
    };
    let source_codec = if source_is_h264 {
        video_codec::VideoCodec::H264
    } else {
        video_codec::VideoCodec::Hevc
    };
    let decoder = match video_engine::VideoDecoder::open(source_codec) {
        Ok(d) => d,
        Err(e) => {
            let msg = format!(
                "RTMP output '{}': video_encode failed to open decoder for {:?}: {e}",
                config.id, source_codec
            );
            tracing::error!("{msg}");
            event_sender.emit_output_with_details(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                msg,
                &config.id,
                serde_json::json!({ "codec": cfg.codec }),
            );
            return VideoEncoderState::Failed;
        }
    };
    let target_family = backend.family();
    let stats_handle = Arc::new(VideoEncodeStats::default());
    let backend_tag = match backend {
        video_codec::VideoEncoderCodec::X264 => "x264",
        video_codec::VideoEncoderCodec::X265 => "x265",
        video_codec::VideoEncoderCodec::H264Nvenc | video_codec::VideoEncoderCodec::HevcNvenc => {
            "nvenc"
        }
        video_codec::VideoEncoderCodec::H264Qsv | video_codec::VideoEncoderCodec::HevcQsv => {
            "qsv"
        }
    };
    let target_codec = match target_family {
        video_codec::VideoCodec::H264 => "h264",
        video_codec::VideoCodec::Hevc => "hevc",
    };
    stats.set_video_encode_stats(
        stats_handle.clone(),
        String::new(),
        target_codec.to_string(),
        cfg.width.unwrap_or(0),
        cfg.height.unwrap_or(0),
        match (cfg.fps_num, cfg.fps_den) {
            (Some(n), Some(d)) if d > 0 => n as f32 / d as f32,
            _ => 0.0,
        },
        cfg.bitrate_kbps.unwrap_or(4000),
        backend_tag.to_string(),
    );
    tracing::info!(
        "RTMP output '{}': video_encode active ({} @ {} kbps)",
        config.id,
        backend_tag,
        cfg.bitrate_kbps.unwrap_or(4000),
    );
    event_sender.emit_output_with_details(
        EventSeverity::Info,
        category::VIDEO_ENCODE,
        format!("Video encoder started: output '{}'", config.id),
        &config.id,
        serde_json::json!({ "codec": cfg.codec }),
    );
    // RTMP FLV sequence header is out-of-band: the encoder must emit
    // extradata (SPS/PPS or VPS/SPS/PPS) so we can build the FLV header
    // before any frame tags go on the wire. Hence `global_header = true`.
    let (fps_num, fps_den) = match (cfg.fps_num, cfg.fps_den) {
        (Some(n), Some(d)) => (n, d),
        _ => (30, 1),
    };
    let pipeline = crate::engine::video_encode_util::ScaledVideoEncoder::new(
        cfg.clone(),
        backend,
        fps_num,
        fps_den,
        true,
        format!("RTMP output '{}'", config.id),
    );
    VideoEncoderState::Active(Box::new(VideoActive {
        decoder,
        pipeline,
        target_family,
        fps_num,
        fps_den,
        sequence_header_tag: None,
        sequence_header_sent: false,
        out_frame_count: 0,
        stats: stats_handle,
    }))
}

/// Push one source access unit through the decoder, drain decoded frames
/// through the encoder, and emit FLV tags per encoded frame.
#[cfg(feature = "video-thumbnail")]
#[allow(clippy::too_many_arguments)]
async fn encode_one_frame(
    src: &VideoFrameSource<'_>,
    _pts_90k: u64,
    _ts_ms: u32,
    recv_time_us: u64,
    sent_video_header: &mut bool,
    video_state: &mut VideoEncoderState,
    client: &mut RtmpClient,
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    _flow_id: &str,
    event_sender: &EventSender,
) -> anyhow::Result<bool> {
    let active = match video_state {
        VideoEncoderState::Active(a) => a,
        _ => return Ok(true),
    };

    // Concatenate the source NALUs back into an Annex-B chunk for the
    // decoder. In-process codec work runs inside `block_in_place` so the
    // tokio reactor isn't held while we spend single-digit milliseconds
    // per frame.
    let annex_b = nalus_to_annex_b(src.nalus());
    let block_result: Result<Vec<(Vec<u8>, bool, i64)>, String> = crate::timed_block_in_place!(
        "output_rtmp.video_encoder",
        crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
        {
            active.stats.input_frames.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = active.decoder.send_packet(&annex_b) {
                tracing::debug!("RTMP output '{}': decoder send_packet: {e:?}", config.id);
            }
            let mut out = Vec::new();
            loop {
                let frame = match active.decoder.receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };
                let was_open = active.pipeline.is_open();
                let encoded = match active.pipeline.encode(&frame, Some(active.out_frame_count)) {
                    Ok(frames) => frames,
                    Err(e) => {
                        if !active.pipeline.is_open() {
                            // Terminal: encoder open failed.
                            return Err(format!("encoder open failed: {e}"));
                        }
                        tracing::debug!("RTMP output '{}': encode error: {e}", config.id);
                        active.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                // First time the pipeline reported itself open, capture
                // its extradata and build the FLV sequence header —
                // classic AVC for H.264, Enhanced RTMP hvcC for HEVC.
                if !was_open && active.pipeline.is_open() {
                    if let Some(ed) = active.pipeline.extradata() {
                        active.sequence_header_tag = Some(match active.target_family {
                            video_codec::VideoCodec::H264 => {
                                build_avc_sequence_header_from_avcc(&ed)
                            }
                            video_codec::VideoCodec::Hevc => {
                                build_hevc_sequence_header_from_hvcc(&ed)
                            }
                        });
                    }
                }
                active.out_frame_count += 1;
                for ef in encoded {
                    out.push((ef.data, ef.keyframe, ef.pts));
                    active.stats.output_frames.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(out)
        }
    );

    // Only encoder *open* failure flips us to Failed. Decoder priming —
    // when the H.264/HEVC decoder needs several access units before it
    // emits its first frame — produces an empty `Ok(vec![])` and must
    // not terminate the encode pipeline.
    let encoded: Vec<(Vec<u8>, bool, i64)> = match block_result {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("RTMP output '{}': video_encode: {e}", config.id);
            event_sender.emit_output_with_details(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                format!("Video encoder failed: output '{}': {e}", config.id),
                &config.id,
                serde_json::json!({ "error": e }),
            );
            *video_state = VideoEncoderState::Failed;
            return Ok(true);
        }
    };

    let active = match video_state {
        VideoEncoderState::Active(a) => a,
        _ => return Ok(true),
    };

    // Convert encoder-PTS (in 1/fps_num ticks) to FLV-style milliseconds.
    // Using the encoder's per-frame PTS — instead of replaying the source
    // `ts_ms` for every output frame in this batch — keeps DTS strictly
    // monotonic when the decoder drains several queued frames at once
    // (which is the common path on the very first non-IDR access units).
    let fps_num = active.fps_num.max(1);
    let fps_den = active.fps_den.max(1);
    for (annex_b_encoded, keyframe, enc_pts) in encoded {
        let frame_ts_ms = (enc_pts.max(0) as u64 * 1000 * fps_den as u64 / fps_num as u64) as u32;
        // Send the sequence header once, then re-send on each subsequent
        // keyframe (mirrors the passthrough policy — some servers require
        // the ASC before they start demuxing).
        if let Some(hdr) = active.sequence_header_tag.as_ref() {
            if !active.sequence_header_sent {
                client.send_video(hdr, frame_ts_ms).await?;
                active.sequence_header_sent = true;
                *sent_video_header = true;
                tracing::debug!(
                    "RTMP output '{}': sent transcoded video sequence header",
                    config.id
                );
            } else if keyframe {
                client.send_video(hdr, frame_ts_ms).await?;
            }
        }

        let avcc = annex_b_to_avcc(&annex_b_encoded);
        let tag = match active.target_family {
            video_codec::VideoCodec::H264 => build_avc_nalu_tag_raw(&avcc, keyframe),
            video_codec::VideoCodec::Hevc => build_hevc_coded_frames_tag(&avcc, keyframe),
        };
        let tag_len = tag.len();
        client.send_video(&tag, frame_ts_ms).await?;
        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
        stats.record_latency(recv_time_us);
    }
    Ok(true)
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
    // Disabled when a transcode block is present — transcode requires the
    // decode→encode pipeline to apply channel remap / SRC.
    let no_overrides = enc_cfg.bitrate_kbps.is_none()
        && enc_cfg.sample_rate.is_none()
        && enc_cfg.channels.is_none();
    if codec == AudioCodec::AacLc && no_overrides && config.transcode.is_none() {
        tracing::info!(
            "RTMP output '{}': audio_encode same-codec passthrough (AAC-LC {} Hz {} ch)",
            config.id, input_sr, input_ch
        );
        return EncoderState::Transparent;
    }

    // Resolve the encoder's input shape. When a transcode block is set it
    // wins — fold any audio_encode overrides into it as fallbacks for fields
    // the block leaves unset, build the transcoder, and use its output as
    // the encoder input.
    let (target_sr, target_ch, transcoder) = if let Some(tj_in) = config.transcode.as_ref() {
        let merged = super::audio_transcode::TranscodeJson {
            sample_rate: tj_in.sample_rate.or(enc_cfg.sample_rate),
            channels: tj_in.channels.or(enc_cfg.channels),
            ..tj_in.clone()
        };
        match super::audio_transcode::PlanarAudioTranscoder::new(
            input_sr,
            input_ch,
            &merged,
        ) {
            Ok(tc) => (tc.out_sample_rate(), tc.out_channels(), Some(tc)),
            Err(e) => {
                let msg = format!(
                    "RTMP output '{}': audio_encode transcode build failed: {e}",
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
        }
    } else {
        (
            enc_cfg.sample_rate.unwrap_or(input_sr),
            enc_cfg.channels.unwrap_or(input_ch),
            None,
        )
    };
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    // When a transcoder is in front of the encoder, it has already aligned
    // the PCM to (target_sr, target_ch), so the encoder sees its target
    // format as input and performs no internal SRC / channel mapping.
    let (enc_in_sr, enc_in_ch) = if transcoder.is_some() {
        (target_sr, target_ch)
    } else {
        (input_sr, input_ch)
    };
    let params = EncoderParams {
        codec,
        sample_rate: enc_in_sr,
        channels: enc_in_ch,
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

    // Register decode + encode stats handles with the shared per-output
    // accumulator so the stats snapshot path can surface them to the
    // manager UI. First-wins semantics: if the output has previously been
    // Lazy→Active cycled, this is a no-op.
    let decode_stats = Arc::new(DecodeStats::new());
    stats.set_decode_stats(
        decode_stats.clone(),
        decoder.codec_name(),
        decoder.sample_rate(),
        decoder.channels(),
    );
    stats.set_encode_stats(
        encoder.stats_handle(),
        encoder.params().codec.as_str().to_string(),
        encoder.params().target_sample_rate,
        encoder.params().target_channels,
        encoder.params().target_bitrate_kbps,
    );

    EncoderState::Active {
        decoder: Some(decoder),
        encoder,
        decode_stats,
        transcoder,
        pending_transcode_cfg: None,
        silence: None,
    }
}

/// Silence-tick handler: if the drop watchdog says real audio is
/// missing, inject one zero-filled chunk into the encoder, send the
/// FLV AAC sequence header on the first emission, drain any encoded
/// frames the encoder has queued, and write them out as FLV audio
/// tags.
///
/// This is idempotent and cheap when no silence is needed: it
/// short-circuits on `should_emit() == false`.
async fn emit_silence_if_needed(
    config: &RtmpOutputConfig,
    client: &mut RtmpClient,
    encoder_state: &mut EncoderState,
    sent_audio_header: &mut bool,
    stats: &Arc<OutputStatsAccumulator>,
) -> anyhow::Result<()> {
    let EncoderState::Active {
        encoder,
        silence: Some(sg),
        ..
    } = encoder_state
    else {
        return Ok(());
    };

    if !sg.should_emit() {
        return Ok(());
    }

    if !sg.is_emitting() {
        tracing::info!(
            "RTMP output '{}': source audio absent/stalled — starting silent-AAC injection",
            config.id
        );
    }

    let (planar, pts) = sg.next_chunk();
    encoder.submit_planar(planar, pts);

    if !*sent_audio_header {
        let p = encoder.params();
        if let Some(sr_idx) = sr_index_from_hz(p.target_sample_rate) {
            let header = build_aac_sequence_header(1, sr_idx, p.target_channels);
            client.send_audio(&header, 0).await?;
            *sent_audio_header = true;
            tracing::debug!(
                "RTMP output '{}': sent AAC sequence header (silent-fallback)",
                config.id
            );
        } else {
            tracing::warn!(
                "RTMP output '{}': silent-fallback target sample_rate {} has no ADTS index; deferring sequence header",
                config.id, p.target_sample_rate
            );
        }
    }

    for frame in encoder.drain() {
        let tag = build_aac_raw_tag(&frame.data);
        let tag_len = tag.len();
        let frame_ts_ms = (frame.pts / 90) as u32;
        client.send_audio(&tag, frame_ts_ms).await?;
        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
    }

    Ok(())
}

/// Eager encoder construction for `silent_fallback = true`.
///
/// The declared `audio_encode.sample_rate` / `channels` (defaulting to
/// 48 kHz stereo) are used as both the encoder's input PCM format and
/// its output target. This lets us spin up the encoder at output
/// startup — before any source AAC frame has been seen — so the
/// silence generator has somewhere to submit its zero-filled chunks.
/// The source decoder + optional resampling transcoder are built
/// lazily on the first real AAC frame (via
/// [`lazy_build_decoder_and_transcoder`]).
fn build_encoder_state_eager_for_silent_fallback(
    config: &RtmpOutputConfig,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    event_sender: &EventSender,
) -> EncoderState {
    let Some(enc_cfg) = config.audio_encode.as_ref() else {
        return EncoderState::Disabled;
    };

    let Some(codec) = AudioCodec::parse(&enc_cfg.codec) else {
        tracing::error!(
            "RTMP output '{}': audio_encode unknown codec '{}'",
            config.id, enc_cfg.codec
        );
        return EncoderState::Failed;
    };

    // Declared encoder params. These are also the silence generator's
    // PCM format — so real audio arriving later with different params
    // is run through a resampler (lazily built on first real frame).
    let target_sr = enc_cfg.sample_rate.unwrap_or(48_000);
    let target_ch = enc_cfg.channels.unwrap_or(2).clamp(1, 2);
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: target_sr,
        channels: target_ch,
        target_bitrate_kbps: target_br,
        target_sample_rate: target_sr,
        target_channels: target_ch,
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
                "RTMP output '{}': audio_encode(silent_fallback) requires ffmpeg in PATH but it is not installed; silent-audio track disabled",
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
                "RTMP output '{}': audio_encode(silent_fallback) encoder spawn failed: {e}",
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

    let decode_stats = Arc::new(DecodeStats::new());
    stats.set_encode_stats(
        encoder.stats_handle(),
        encoder.params().codec.as_str().to_string(),
        encoder.params().target_sample_rate,
        encoder.params().target_channels,
        encoder.params().target_bitrate_kbps,
    );

    let silence = SilenceGenerator::new(target_sr, target_ch, 0);

    tracing::info!(
        "RTMP output '{}': audio_encode(silent_fallback) active codec={} target={} Hz {} ch {} kbps",
        config.id,
        encoder.params().codec.as_str(),
        target_sr, target_ch, target_br,
    );

    EncoderState::Active {
        decoder: None,
        encoder,
        decode_stats,
        transcoder: None,
        pending_transcode_cfg: config.transcode.clone(),
        silence: Some(silence),
    }
}

/// Build the source AAC decoder + optional resampler on the first real
/// AAC frame observed while the encoder is already running (the
/// silent-fallback path). Mutates the passed `decoder` / `transcoder`
/// fields in place; logs and sets `decoder = None` on failure so we
/// keep producing silence instead of crashing the output.
fn lazy_build_decoder_and_transcoder(
    decoder: &mut Option<AacDecoder>,
    transcoder: &mut Option<super::audio_transcode::PlanarAudioTranscoder>,
    pending_transcode_cfg: &Option<super::audio_transcode::TranscodeJson>,
    encoder_params: &EncoderParams,
    cached_aac: (u8, u8, u8),
    output_id: &str,
) {
    let (profile, sr_idx, ch_cfg) = cached_aac;
    if profile != 1 {
        tracing::warn!(
            "RTMP output '{}': silent-fallback ignoring non-AAC-LC source frame (profile={profile}); keeping silence track",
            output_id
        );
        return;
    }
    let dec = match AacDecoder::from_adts_config(profile, sr_idx, ch_cfg) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                "RTMP output '{}': silent-fallback AacDecoder build failed: {e}; keeping silence track",
                output_id
            );
            return;
        }
    };
    let source_sr = dec.sample_rate();
    let source_ch = dec.channels();
    *decoder = Some(dec);

    let need_transcode = pending_transcode_cfg.is_some()
        || source_sr != encoder_params.sample_rate
        || source_ch != encoder_params.channels;
    if !need_transcode {
        *transcoder = None;
        return;
    }

    let merged = super::audio_transcode::TranscodeJson {
        sample_rate: Some(encoder_params.sample_rate),
        channels: Some(encoder_params.channels),
        ..pending_transcode_cfg.clone().unwrap_or_default()
    };
    match super::audio_transcode::PlanarAudioTranscoder::new(source_sr, source_ch, &merged) {
        Ok(tc) => *transcoder = Some(tc),
        Err(e) => {
            tracing::warn!(
                "RTMP output '{}': silent-fallback transcoder build failed: {e}; source audio will be dropped but silence continues",
                output_id
            );
            // decoder stays set; without a transcoder, submitting
            // planar at the wrong SR/channels would corrupt the
            // encoder stream, so we drop real audio until a resample
            // path can be built (next frame retries via this same
            // helper because `transcoder.is_none()`).
            *decoder = None;
        }
    }
}


/// Compute the reconnect delay for an RTMP push output (Bug #10 fix).
///
/// `attempt` is the 1-indexed attempt number that just failed (i.e. the
/// next reconnect is the `attempt+1`-th try). The schedule is:
///
/// | attempt | delay |
/// |---|---|
/// | 1 | 1 s    |
/// | 2 | 2 s    |
/// | 3 | 4 s    |
/// | 4 | 8 s    |
/// | 5 | 16 s   |
/// | 6+ | 30 s (cap) |
///
/// The cap is the larger of `reconnect_delay_secs` (operator-configurable)
/// and 30 s, so a flow that asks for a longer base delay still respects
/// it. Calling code should pass the failed attempt count from the loop's
/// own counter — the helper does no state-keeping itself so it stays
/// trivially testable.
pub(crate) fn rtmp_reconnect_backoff(
    attempt: u32,
    operator_floor_secs: u64,
) -> u64 {
    // 2^(attempt-1) seconds, capped at max(operator_floor_secs, 30).
    // Exponent is clamped to 30 so the shift can never overflow u64.
    let exponent = attempt.saturating_sub(1).min(30);
    let exponential = 1u64 << exponent;
    let cap = operator_floor_secs.max(30);
    exponential.min(cap).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the exponential backoff schedule matches the docstring above.
    #[test]
    fn rtmp_reconnect_backoff_schedule() {
        // Default 3 s operator floor → cap is 30 s.
        assert_eq!(rtmp_reconnect_backoff(1, 3), 1);
        assert_eq!(rtmp_reconnect_backoff(2, 3), 2);
        assert_eq!(rtmp_reconnect_backoff(3, 3), 4);
        assert_eq!(rtmp_reconnect_backoff(4, 3), 8);
        assert_eq!(rtmp_reconnect_backoff(5, 3), 16);
        assert_eq!(rtmp_reconnect_backoff(6, 3), 30);
        assert_eq!(rtmp_reconnect_backoff(7, 3), 30);
        assert_eq!(rtmp_reconnect_backoff(99, 3), 30);
    }

    /// A larger operator floor raises the cap above 30 s. The exponential
    /// schedule continues doubling until it hits the cap.
    #[test]
    fn rtmp_reconnect_backoff_respects_operator_floor() {
        // attempt=5 → 16, attempt=6 → 32 (still below 60 s cap).
        assert_eq!(rtmp_reconnect_backoff(5, 60), 16);
        assert_eq!(rtmp_reconnect_backoff(6, 60), 32);
        // attempt=7 → 64, capped at operator floor 60.
        assert_eq!(rtmp_reconnect_backoff(7, 60), 60);
        assert_eq!(rtmp_reconnect_backoff(99, 60), 60);
    }

    #[test]
    fn annex_b_to_avcc_handles_start_code_variants() {
        // Two NALUs separated by a 4-byte and a 3-byte start code.
        let input: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E,
            0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80,
        ];
        let avcc = annex_b_to_avcc(&input);
        assert_eq!(
            avcc,
            vec![
                0x00, 0x00, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1E,
                0x00, 0x00, 0x00, 0x04, 0x68, 0xCE, 0x38, 0x80,
            ]
        );
    }

    #[test]
    fn annex_b_to_avcc_empty_input_is_empty() {
        assert!(annex_b_to_avcc(&[]).is_empty());
        assert!(annex_b_to_avcc(&[0x00, 0x00, 0x00]).is_empty());
    }

    #[test]
    fn build_hevc_sequence_header_shape() {
        // hvcC body is opaque to this test — we just check that the
        // framing bytes around it match the Enhanced RTMP v2 spec.
        // The first byte is the hvcC `configurationVersion` and must
        // be 0x01 for `build_hevc_sequence_header_from_hvcc` to take
        // the passthrough path; otherwise it parses the input as an
        // Annex-B VPS+SPS+PPS stream and rejects a random blob.
        let hvcc = [0x01, 0xBB, 0xCC];
        let tag = build_hevc_sequence_header_from_hvcc(&hvcc);
        assert_eq!(tag[0], 0x90, "IsEx(1) | FrameType(1=key) | PacketType(0)");
        assert_eq!(&tag[1..5], b"hvc1", "FourCC");
        assert_eq!(&tag[5..], &hvcc);
    }

    #[test]
    fn build_hevc_sequence_header_returns_empty_on_unparseable_extradata() {
        // Neither a pre-built hvcC (first byte 0x01) nor a valid
        // Annex-B VPS/SPS/PPS stream → the function logs a warning and
        // returns an empty Vec rather than panicking. The encode path
        // treats an empty sequence-header tag as "don't emit"; the FLV
        // muxer still works, just without the out-of-band codec config.
        let garbage = [0xAA, 0xBB, 0xCC];
        assert!(build_hevc_sequence_header_from_hvcc(&garbage).is_empty());
    }

    #[test]
    fn build_hevc_coded_frames_tag_shape() {
        let avcc = [0x01, 0x02];
        let kf = build_hevc_coded_frames_tag(&avcc, true);
        assert_eq!(kf[0], 0x93, "IsEx | keyframe | PacketType=CodedFramesX");
        assert_eq!(&kf[1..5], b"hvc1");
        let inter = build_hevc_coded_frames_tag(&avcc, false);
        assert_eq!(inter[0], 0xA3, "IsEx | inter | PacketType=CodedFramesX");
    }

    /// Total reconnect attempts in 60 s of wall clock should be ≤ 7
    /// (1+2+4+8+16+30 = 61 s for the first six tries). This matches the
    /// Bug #10 verification gate.
    #[test]
    fn rtmp_reconnect_backoff_caps_attempts_per_minute() {
        let mut elapsed: u64 = 0;
        let mut attempts: u32 = 0;
        let mut next: u32 = 1;
        while elapsed < 60 {
            elapsed += rtmp_reconnect_backoff(next, 3);
            attempts += 1;
            next += 1;
        }
        assert!(
            attempts <= 7,
            "expected ≤ 7 reconnect attempts in 60 s with the new \
             backoff, got {attempts} (elapsed={elapsed}s)"
        );
    }
}
