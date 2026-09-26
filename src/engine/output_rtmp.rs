// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RTMP output task — publishes demuxed H.264/AAC to an RTMP server.
//!
//! Subscribes to the flow's broadcast channel, demuxes MPEG-TS into
//! H.264 + AAC elementary streams, wraps them in FLV tags, and publishes
//! them via the RTMP client to servers like Twitch, YouTube, etc.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::{BufMut, BytesMut};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{RtmpOutputConfig, VideoEncodeConfig};
use crate::manager::events::{Event, EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;

use super::audio_decode::{AacDecoder, DecodeStats, sample_rate_from_index, sr_index_from_hz};
use super::audio_encode::{AudioCodec, AudioEncoder, AudioEncoderError, EncoderParams};
use super::audio_silence::SilenceGenerator;
use super::packet::RtpPacket;
use super::rtmp::client::{RtmpClient, RtmpConnectError, RtmpFailureKind};
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
    /// decoder is filled lazily the first time real AAC shows up — hence
    /// it is an `Option`; the stage, pinned to the encoder's format, is
    /// built with the encoder.
    Active {
        /// `None` until the first real AAC frame arrives (silent-fallback
        /// builds the encoder ahead of any source audio).
        decoder: Option<AacDecoder>,
        encoder: AudioEncoder,
        decode_stats: Arc<DecodeStats>,
        /// The channel / rate stage between decoder and encoder — the
        /// `transcode` block, or `audio_encode.sample_rate` / `channels`
        /// alone when they differ from the source
        /// (`audio_transcode::encoder_stage`) — converting to the format
        /// the encoder was opened at, in streaming mode (a constant delay,
        /// taken off the encoder's stamps). An MP2 / AC-3 / E-AC-3 source
        /// goes through it too.
        stage: super::audio_transcode::EncoderStage,
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
    /// An H.264 decoder waits for an access unit that carries the SPS
    /// (`SpsOpenGate`), so its reorder depth is seeded from it.
    #[cfg(feature = "media-codecs")]
    Lazy {
        cfg: VideoEncodeConfig,
        sps_gate: crate::engine::video_encode_util::SpsOpenGate,
    },
    /// `video_encode` pipeline is live.
    #[cfg(feature = "media-codecs")]
    Active(Box<VideoActive>),
    /// Decoder or encoder construction failed; drop video for the rest
    /// of the output's lifetime. The failure event was already emitted.
    Failed,
}

#[cfg(feature = "media-codecs")]
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
    /// Source PTS (90 kHz, display order) for every decoded frame handed to
    /// the encoder. libavcodec echoes `pkt.pts → frame.pts` through its
    /// reorder queue, so a batch drain yields N distinct increasing stamps —
    /// the property the old frame-counter was chosen to guarantee, now
    /// obtained without discarding the source clock. Popped one per emitted
    /// frame. Same contract as `ts_video_replace`'s queue of the same name.
    src_pts_queue: std::collections::VecDeque<u64>,
    /// Last wire PTS emitted, for the strictly-increasing guard.
    ///
    /// The encoder's `fps_num`/`fps_den` are deliberately no longer held here.
    /// They still open the encoder (rate control, VBV, default GOP, SPS VUI)
    /// but they no longer reach the FLV timestamp — that comes from the source
    /// clock — and keeping a copy invited the next reader to use it again.
    /// The one nominal frame step substituted when the source PTS is unusable
    /// is read off the pipeline's rate as it is needed.
    last_wire_pts_90k: Option<u64>,
    /// The encoder rate — pinned, or measured from the decoded frames before
    /// the encoder opens (it used to open at a flat 30/1).
    rate: crate::engine::video_encode_util::EncoderRateLock,
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

    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("flv".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
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
        tracing::info!(
            "RTMP output '{}' started -> {}",
            config.id,
            redact_dest_url(&config.dest_url),
        );

        let mut attempt = 0u32;
        // Dedup key for the operator-facing failure alarm: `Some(error_code)`
        // while we are in a down-episode, `None` when connected/healthy. A new
        // failure event is emitted only when the classified `error_code`
        // changes — so an unreachable receiver (or a bad stream key) raises one
        // alarm, not one per reconnect-backoff tick. Cleared (with a "connected"
        // recovery event) on the next successful connect. See
        // `classify_rtmp_failure` for the taxonomy.
        let mut signalled_code: Option<&'static str> = None;
        loop {
            if cancel.is_cancelled() {
                break;
            }

            attempt += 1;
            if let Some(max) = config.max_reconnect_attempts
                && attempt > max + 1 {
                    tracing::error!(
                        "RTMP output '{}': exceeded max reconnect attempts ({})",
                        config.id, max,
                    );
                    event_sender.send(rtmp_output_event(
                        EventSeverity::Critical,
                        "rtmp_output_gave_up",
                        format!(
                            "RTMP output '{}' gave up after {} failed connection attempts to {}",
                            config.id, max, redact_dest_url(&config.dest_url),
                        ),
                        &config,
                        &flow_id,
                        serde_json::json!({ "max_reconnect_attempts": max }),
                    ));
                    break;
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
                    tracing::info!(
                        "RTMP output '{}': connected to {}",
                        config.id,
                        redact_dest_url(&config.dest_url),
                    );
                    // If we had been alarming, clear it with a recovery event
                    // so the operator's alarm resolves.
                    if signalled_code.take().is_some() {
                        event_sender.send(rtmp_output_event(
                            EventSeverity::Info,
                            "rtmp_output_connected",
                            format!(
                                "RTMP output '{}' connected to {}",
                                config.id,
                                redact_dest_url(&config.dest_url),
                            ),
                            &config,
                            &flow_id,
                            serde_json::json!({}),
                        ));
                    }
                    attempt = 0; // reset on successful connect
                    c
                }
                Err(e) => {
                    tracing::warn!(
                        "RTMP output '{}': connection failed: {:#}",
                        config.id, e,
                    );
                    // Surface the failure to the manager events feed — but only
                    // once per distinct cause, so backoff retries don't spam.
                    // Before this, an expired/bad Twitch stream key produced no
                    // manager event at all; the output just sat in the derived
                    // "waiting" state, indistinguishable from "no input yet".
                    let (severity, code, reason, server_code) = classify_rtmp_failure(&e);
                    if signalled_code != Some(code) {
                        let mut details = serde_json::json!({ "detail": format!("{:#}", e) });
                        if let Some(sc) = server_code {
                            details["server_code"] = serde_json::Value::String(sc);
                        }
                        event_sender.send(rtmp_output_event(
                            severity,
                            code,
                            format!("RTMP output '{}' — {}", config.id, reason),
                            &config,
                            &flow_id,
                            details,
                        ));
                        signalled_code = Some(code);
                    }
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
                    // A mid-stream disconnect after a healthy connection. Shares
                    // the `rtmp_connect_failed` dedup bucket with the transport
                    // reconnect failures that follow, so the operator sees one
                    // "connection lost" alarm, not a "lost" + "can't reconnect"
                    // pair. The recovery event fires when it reconnects.
                    let code = "rtmp_connect_failed";
                    if signalled_code != Some(code) {
                        event_sender.send(rtmp_output_event(
                            EventSeverity::Warning,
                            code,
                            format!(
                                "RTMP output '{}' lost its connection to {}",
                                config.id, redact_dest_url(&config.dest_url),
                            ),
                            &config,
                            &flow_id,
                            serde_json::json!({ "detail": format!("{:#}", e) }),
                        ));
                        signalled_code = Some(code);
                    }
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
    // Lazy FFmpeg-backed audio decoder for non-AAC sources
    // (MP2 / AC-3 / E-AC-3). Opened on the first `OtherAudio` frame.
    // Mirrors the AAC decoder slot inside `EncoderState::Active` —
    // only one decoder is in flight per publish loop at a time.
    #[cfg(feature = "media-codecs")]
    let mut ff_audio_decoder: Option<video_engine::AudioDecoder> = None;
    #[cfg(feature = "media-codecs")]
    let mut ff_audio_codec: Option<video_codec::AudioDecoderCodec> = None;
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
                emit_silence_if_needed(
                    config,
                    client,
                    &mut encoder_state,
                    &mut sent_audio_header,
                    stats,
                    base_pts,
                )
                .await?;
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
                        &mut base_pts,
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
                        &mut base_pts,
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
                            Some(&data),
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
                    if !sent_audio_header
                        && let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() {
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
                            stage,
                            silence,
                        } => {
                            // Silent-fallback mode: lazily build the decoder
                            // against the source's real AAC config on the
                            // first real frame (the stage, pinned to the
                            // encoder's format, converts whatever it is).
                            if decoder.is_none()
                                && let Some((profile, sr_idx, ch_cfg)) =
                                    demuxer.cached_aac_config()
                                {
                                    lazy_build_decoder(
                                        decoder,
                                        (profile, sr_idx, ch_cfg),
                                        &config.id,
                                    );
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
                                    match encoder.submit_through(stage, &planar, dec.sample_rate(), pts) {
                                        Ok(_) => {}
                                        Err(e) => {
                                            tracing::debug!(
                                                "RTMP output '{}': transcode failed: {e}",
                                                config.id
                                            );
                                        }
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
                                // Share the connection's FLV epoch. This was
                                // the raw absolute source PTS in ms, so on any
                                // source whose PTS does not start near zero —
                                // every feed from an upstream bilbycast edge,
                                // whose rewriter anchors on the master clock —
                                // audio tags landed up to ~26.5 h ahead of
                                // 0-based video tags.
                                let base = *base_pts.get_or_insert(frame.pts);
                                let frame_ts_ms = rel_ms_monotonic(frame.pts, base);
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
                DemuxedFrame::Opus { .. } => {
                    // RTMP doesn't support Opus — skip
                }
                #[cfg(feature = "media-codecs")]
                DemuxedFrame::OtherAudio { stream_type, data, pts } => {
                    let ts_ms = pts_to_ms(pts, &mut base_pts);

                    // Re-encoding to AAC requires the encoder to be wired
                    // up (`audio_encode: aac_lc/he_aac_v1/he_aac_v2`). On
                    // a passthrough output, RTMP wants AAC bytes — MP2 /
                    // AC-3 / E-AC-3 sources need `audio_encode` to land.
                    if matches!(encoder_state, EncoderState::Lazy) {
                        encoder_state = build_encoder_state(
                            config,
                            &demuxer,
                            None,
                            compressed_audio_input,
                            cancel,
                            stats,
                            flow_id,
                            event_sender,
                        );
                    }
                    let EncoderState::Active {
                        encoder, silence, stage, ..
                    } = &mut encoder_state
                    else {
                        continue;
                    };

                    let Some(codec) = crate::engine::audio_decode::ff_codec_for_stream_type(
                        stream_type,
                    ) else {
                        continue;
                    };
                    if ff_audio_codec != Some(codec) {
                        ff_audio_decoder = crate::engine::audio_decode::open_ff_decoder(codec).ok();
                        ff_audio_codec = Some(codec);
                    }
                    let Some(dec) = ff_audio_decoder.as_mut() else {
                        continue;
                    };
                    if let Some(sg) = silence.as_mut() {
                        sg.mark_real_audio(pts);
                    }

                    // Send the FLV AAC sequence header on first frame —
                    // for OtherAudio sources we can't read ADTS config off
                    // the demuxer, so derive the ASC straight from the
                    // encoder's target params (which is what every RTMP
                    // server actually expects to see).
                    if !sent_audio_header {
                        let p = encoder.params();
                        if let Some(sr_idx) = sr_index_from_hz(p.target_sample_rate) {
                            let header =
                                build_aac_sequence_header(1, sr_idx, p.target_channels);
                            client.send_audio(&header, ts_ms).await?;
                            sent_audio_header = true;
                        }
                    }
                    if !sent_audio_header {
                        continue;
                    }

                    for au in
                        crate::engine::audio_decode::split_audio_codec_frames(&data, codec)
                    {
                        if dec.send_packet(au, pts as i64).is_err() {
                            continue;
                        }
                        while let Ok(frame) = dec.receive_frame() {
                            // To the encoder's format: MP2 at another rate
                            // was encoded as if at the encoder's (and ran
                            // at the wrong speed), 5.1 AC-3 refused.
                            match encoder.submit_through(stage, &frame.planar, frame.sample_rate, pts) {
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::debug!(
                                        "RTMP output '{}': transcode failed: {e}",
                                        config.id
                                    );
                                }
                            }
                        }
                    }
                    let drained = encoder.drain();
                    for frame in drained {
                        let tag = build_aac_raw_tag(&frame.data);
                        let tag_len = tag.len();
                        // Share the connection's FLV epoch — see the AAC
                        // path above.
                        let base = *base_pts.get_or_insert(frame.pts);
                        let frame_ts_ms = rel_ms_monotonic(frame.pts, base);
                        client.send_audio(&tag, frame_ts_ms).await?;
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
                        stats.record_latency(recv_time_us);
                    }
                }
                #[cfg(not(feature = "media-codecs"))]
                DemuxedFrame::OtherAudio { .. } => {
                    // Build without `media-codecs` lacks the libavcodec
                    // bridge needed to decode MP2 / AC-3 / E-AC-3 — drop.
                }
                // RTMP carries H.264 / HEVC + AAC — MPEG-2 video would
                // need a transcode hop we don't have on this output yet.
                DemuxedFrame::Mpeg2 { .. } => {}
                // Stream discontinuity is metadata for stateful consumers
                // that own decoder state. RTMP is a forwarding output —
                // the FLV writer re-anchors on the next IDR / AAC config
                // change naturally.
                DemuxedFrame::Discontinuity | DemuxedFrame::Scte35(_) => {}
            }
        }

        // Flush after each TS packet batch to keep delivery smooth
        client.flush().await?;
    }
}

/// Convert 90kHz PTS to milliseconds relative to the first frame.
fn pts_to_ms(pts: u64, base_pts: &mut Option<u64>) -> u32 {
    let base = *base_pts.get_or_insert(pts);
    rel_ms_wrapping(pts, base)
}

/// Largest 33-bit PTS distance still read as "forward". Only meaningful for a
/// raw source PTS, which wraps at 2^33.
const MAX_FORWARD_90K: u64 = 1 << 33;

/// Raw (wrapping, 33-bit) source PTS → FLV milliseconds relative to the epoch.
fn rel_ms_wrapping(pts: u64, base: u64) -> u32 {
    let delta = pts.wrapping_sub(base);
    if delta >= MAX_FORWARD_90K {
        return 0;
    }
    (delta / 90) as u32
}

/// An already-monotonic 90 kHz timeline → FLV milliseconds relative to the
/// epoch.
///
/// `v` is non-decreasing by construction — either the wire-PTS guard below, or
/// the audio encoder's once-anchored sample counter — so no wrap heuristic is
/// wanted here. Applying one would cap the output at 2^33 ticks (~26.5 h) and
/// then pin every later tag to 0. A value behind the epoch (the other essence
/// anchored first) clamps to 0.
fn rel_ms_monotonic(v: u64, base: u64) -> u32 {
    (v.saturating_sub(base) / 90) as u32
}

/// Ticks beyond which a forward step is read as a source discontinuity rather
/// than a real gap. Two seconds: past any legitimate inter-frame interval,
/// well short of a splice or a clock re-anchor.
const MAX_FORWARD_STEP_90K: u64 = 180_000;

/// Wire PTS (90 kHz) for one encoded video frame.
///
/// FLV carries DTS in the tag timestamp and RTMP servers drop a publisher whose
/// DTS goes backwards, so the result is forced strictly increasing. It is also
/// forced not to *leap*: the transcoded-audio timeline anchors once and then
/// free-runs on a sample count, so it physically cannot follow a forward source
/// jump. Letting video jump while audio cannot is the very A/V split this fix
/// exists to close, so both directions substitute one nominal frame step. On a
/// genuine ingest gap this compresses the video timeline by exactly the amount
/// the audio timeline is already compressed — the two stay together, which is
/// the property that matters.
fn next_wire_pts_90k(
    queue: &mut std::collections::VecDeque<u64>,
    last: Option<u64>,
    step_90k: u64,
) -> u64 {
    let raw = queue.pop_front().unwrap_or(0);
    match last {
        None => raw,
        Some(l) if raw > l && raw - l <= MAX_FORWARD_STEP_90K => raw,
        Some(l) => l.wrapping_add(step_90k),
    }
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
const FOURCC_HVC1: [u8; 4] = *b"hvc1";

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
    buf.put_u8(0x80 | (1 << 4)); // Ex | keyframe | PacketType=SequenceStart
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
#[cfg(feature = "media-codecs")]
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
/// build has no `media-codecs` support at all).
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
    #[cfg(feature = "media-codecs")]
    {
        VideoEncoderState::Lazy {
            cfg: enc_cfg.clone(),
            sps_gate: crate::engine::video_encode_util::SpsOpenGate::new(),
        }
    }
    #[cfg(not(feature = "media-codecs"))]
    {
        let msg = format!(
            "RTMP output '{}': video_encode requested but this build lacks \
             the `media-codecs` feature (no in-process video codec library)",
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

#[cfg(feature = "media-codecs")]
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
        "h264_vaapi" => Some(video_codec::VideoEncoderCodec::H264Vaapi),
        "hevc_vaapi" => Some(video_codec::VideoEncoderCodec::HevcVaapi),
        "h264_rkmpp" => Some(video_codec::VideoEncoderCodec::H264Rkmpp),
        "hevc_rkmpp" => Some(video_codec::VideoEncoderCodec::HevcRkmpp),
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

/// Resolve a `video_encode` block's codec backend chain, honouring
/// `*_auto` and surfacing host-mismatch errors before the encoder is
/// opened. Returns the **full Auto fall-through chain** so
/// [`crate::engine::video_encode_util::ScaledVideoEncoder::with_backend_chain`]
/// can demote across `avcodec_open2` failures. Single-element vec for
/// explicit codecs; the priority chain (head wins, tail are
/// fall-throughs) for `*_auto`. Falls back to a single-element chain
/// drawn from the legacy string parser when no probed-caps snapshot is
/// installed (in-process tests + early-startup paths).
#[cfg(feature = "media-codecs")]
fn resolve_backend_chain_for_config(
    cfg: &VideoEncodeConfig,
    output_id: &str,
    event_sender: &EventSender,
) -> Option<Vec<video_codec::VideoEncoderCodec>> {
    if crate::engine::hardware_probe::static_capabilities().is_some() {
        match crate::engine::hardware_probe::resolve_chain_for_video_encode_config(cfg) {
            Ok(chain) => {
                if cfg.codec.ends_with("_auto") || cfg.codec == "auto" {
                    let names: Vec<&str> = chain.iter().map(|r| r.ffmpeg_name()).collect();
                    tracing::info!(
                        "RTMP output '{}': video_encode auto-resolved '{}' → chain {:?}",
                        output_id,
                        cfg.codec,
                        names,
                    );
                }
                return Some(
                    chain
                        .iter()
                        .map(|r| r.as_video_encoder_codec())
                        .collect(),
                );
            }
            Err(e) => {
                let msg = format!(
                    "RTMP output '{}': video_encode unavailable: {}",
                    output_id,
                    e.message(),
                );
                tracing::error!("{msg}");
                event_sender.emit_output_with_details(
                    EventSeverity::Critical,
                    category::VIDEO_ENCODE,
                    msg,
                    output_id,
                    serde_json::json!({
                        "codec": cfg.codec,
                        "chroma": cfg.chroma.clone().unwrap_or_else(|| "yuv420p".to_string()),
                        "bit_depth": cfg.bit_depth.unwrap_or(8),
                        "reason": e.as_reason(),
                    }),
                );
                return None;
            }
        }
    }
    // No probed-caps snapshot — fall back to the legacy single-backend
    // string parser, wrapped in a single-element chain.
    resolve_backend(&cfg.codec, output_id, event_sender).map(|b| vec![b])
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
    base_pts: &mut Option<u64>,
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
        #[cfg(feature = "media-codecs")]
        VideoEncoderState::Lazy { cfg, sps_gate } => {
            let au = nalus_to_annex_b(src.nalus());
            let codec = if src.is_h264() {
                video_codec::VideoCodec::H264
            } else {
                video_codec::VideoCodec::Hevc
            };
            // Nothing decodes before the SPS; opening on it seeds the
            // decoder's reorder depth from it (`SpsOpenGate`).
            if !sps_gate.admits(codec, &au) {
                return Ok(true);
            }
            let cfg = cfg.clone();
            *video_state = open_video_active(
                &cfg,
                src.is_h264(),
                &au,
                config,
                stats,
                event_sender,
            );
            if matches!(video_state, VideoEncoderState::Failed) {
                return Ok(true);
            }
            // Fall through to Active on the very same frame.
            encode_one_frame(&src, pts_90k, ts_ms, base_pts, recv_time_us, sent_video_header, video_state, client, config, stats, flow_id, event_sender).await
        }
        #[cfg(feature = "media-codecs")]
        VideoEncoderState::Active(_) => {
            encode_one_frame(&src, pts_90k, ts_ms, base_pts, recv_time_us, sent_video_header, video_state, client, config, stats, flow_id, event_sender).await
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
            if !*sent_video_header
                && let (Some(sps), Some(pps)) = (demuxer.cached_sps(), demuxer.cached_pps()) {
                    let header = build_avc_sequence_header(sps, pps);
                    client.send_video(&header, ts_ms).await?;
                    *sent_video_header = true;
                    tracing::debug!("RTMP output '{}': sent AVC sequence header", config.id);
                }
            if !*sent_video_header {
                return Ok(true);
            }
            // Re-send sequence header on each keyframe (some servers need this)
            if is_keyframe
                && let (Some(sps), Some(pps)) = (demuxer.cached_sps(), demuxer.cached_pps()) {
                    let header = build_avc_sequence_header(sps, pps);
                    client.send_video(&header, ts_ms).await?;
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
            if !*sent_video_header
                && let Some(hvcc) = build_hvcc_from_cached(demuxer) {
                    let header = build_hevc_sequence_header_from_hvcc(&hvcc);
                    client.send_video(&header, ts_ms).await?;
                    *sent_video_header = true;
                    tracing::debug!("RTMP output '{}': sent HEVC sequence header", config.id);
                }
            if !*sent_video_header {
                return Ok(true);
            }
            if is_keyframe
                && let Some(hvcc) = build_hvcc_from_cached(demuxer) {
                    let header = build_hevc_sequence_header_from_hvcc(&hvcc);
                    client.send_video(&header, ts_ms).await?;
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
        if matches!(nal_type, 32..=34) {
            continue;
        }
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

#[cfg(feature = "media-codecs")]
fn open_video_active(
    cfg: &VideoEncodeConfig,
    source_is_h264: bool,
    first_au: &[u8],
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    event_sender: &EventSender,
) -> VideoEncoderState {
    let Some(backend_chain) = resolve_backend_chain_for_config(cfg, &config.id, event_sender)
    else {
        return VideoEncoderState::Failed;
    };
    // Head of the chain drives stats-label / family decisions. Every
    // candidate produces the same family (Auto family is locked;
    // explicit codec is locked) so reading the head is correct.
    let backend = *backend_chain
        .first()
        .expect("resolver guaranteed at least one candidate");
    let source_codec = if source_is_h264 {
        video_codec::VideoCodec::H264
    } else {
        video_codec::VideoCodec::Hevc
    };
    // Seeded from the access unit that triggered the open: an H.264
    // decoder's reorder depth comes from its SPS (`ReorderSeed`).
    let decoder = match video_engine::VideoDecoder::open_opts(
        source_codec,
        video_engine::DecoderOptions {
            reorder_seed: video_engine::ReorderSeed::FromAccessUnit(first_au),
            ..Default::default()
        },
    ) {
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
        video_codec::VideoEncoderCodec::H264Vaapi | video_codec::VideoEncoderCodec::HevcVaapi => {
            "vaapi"
        }
        video_codec::VideoEncoderCodec::H264Rkmpp | video_codec::VideoEncoderCodec::HevcRkmpp => {
            "rkmpp"
        }
    };
    let target_codec = match target_family {
        video_codec::VideoCodec::H264 => "h264",
        video_codec::VideoCodec::Hevc => "hevc",
        // MPEG-2 isn't a valid RTMP `video_encode` target — `target_family`
        // only ever lands on the encoder family the operator picked, and
        // the validator already refuses Mpeg2 here. Treat it as h264 so
        // the stats label stays a string we expect.
        video_codec::VideoCodec::Mpeg2 => "h264",
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
    // Unpinned, the encoder opens at the source's measured rate: the
    // placeholder here is replaced when `EncoderRateLock` locks.
    let (pinned, (fps_num, fps_den)) = match (cfg.fps_num, cfg.fps_den) {
        (Some(n), Some(d)) => (true, (n, d)),
        _ => (false, crate::engine::video_encode_util::RATE_LOCK_FALLBACK),
    };
    // B-frames are pinned off on this path. FLV carries DTS in the tag
    // timestamp and the two tag writers hard-code CTS = 0 (the Enhanced-RTMP
    // HEVC path has no CTS field at all), so an encoder that reordered would
    // stamp display-order PTS as DTS and drive it backwards — which most
    // ingests answer by dropping the publisher. It is also the precondition
    // for the source-PTS FIFO below being a plain queue. `webrtc_safe_
    // video_encode` pins the same knob for the same class of reason.
    let mut cfg = cfg.clone();
    if cfg.bframes.unwrap_or(0) != 0 {
        tracing::warn!(
            error_code = "rtmp_bframes_unsupported",
            requested = cfg.bframes.unwrap_or(0),
            "RTMP output '{}': video_encode.bframes is not supported on the RTMP path \
             (FLV tags carry no composition-time offset) — encoding with bframes = 0",
            config.id
        );
        cfg.bframes = Some(0);
    }
    let cfg = &cfg;
    let mut pipeline = crate::engine::video_encode_util::ScaledVideoEncoder::with_backend_chain(
        cfg.clone(),
        backend_chain,
        fps_num,
        fps_den,
        true,
        format!("RTMP output '{}'", config.id),
    );
    pipeline.set_resolved_backend_sink(stats_handle.resolved_backend.clone());
    // An explicit `scan: interlaced` needs to know an interlaced frame from
    // this decoder is woven (H.264) or a single field (HEVC field_seq).
    pipeline.set_source_codec(source_codec);
    VideoEncoderState::Active(Box::new(VideoActive {
        decoder,
        pipeline,
        target_family,
        src_pts_queue: std::collections::VecDeque::with_capacity(64),
        last_wire_pts_90k: None,
        rate: crate::engine::video_encode_util::EncoderRateLock::new(pinned),
        sequence_header_tag: None,
        sequence_header_sent: false,
        out_frame_count: 0,
        stats: stats_handle,
    }))
}

/// Push one source access unit through the decoder, drain decoded frames
/// through the encoder, and emit FLV tags per encoded frame.
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
async fn encode_one_frame(
    src: &VideoFrameSource<'_>,
    pts_90k: u64,
    _ts_ms: u32,
    // Shared FLV epoch for this connection — anchored by whichever essence
    // arrives first, so audio and video land on one timeline.
    base_pts: &mut Option<u64>,
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
    let block_result = crate::timed_block_in_place!(
        "output_rtmp.video_encoder",
        crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
        { transcode_access_unit(active, &annex_b, pts_90k, &config.id) }
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

    // FLV timestamps come from the SOURCE clock, popped one per emitted frame
    // from the FIFO the decode loop filled. The previous form derived them from
    // the encoder's frame index and the *declared* rate, so video ran on the
    // configured fps while transcoded audio ran on the source clock — the two
    // diverged at the rate ratio, unbounded and with no resync.
    //
    // Monotonicity, which the old form got for free, is preserved by
    // `next_wire_pts_90k`: DTS is forced strictly increasing (RTMP servers drop
    // a publisher whose DTS goes backwards) and forced not to leap, so a source
    // discontinuity does not split video away from an audio timeline that
    // cannot follow it.
    // One frame period in 90 kHz ticks at the encoder's rate — only a
    // substitute step for an unusable source PTS.
    let step = {
        let (num, den) = active.pipeline.fps();
        (90_000 * u64::from(den.max(1)) / u64::from(num.max(1))).max(1)
    };
    for (annex_b_encoded, keyframe, _enc_pts) in encoded {
        let wire_pts = next_wire_pts_90k(
            &mut active.src_pts_queue,
            active.last_wire_pts_90k,
            step,
        );
        active.last_wire_pts_90k = Some(wire_pts);
        // Anchor the shared FLV epoch if this is the first essence to arrive.
        let base = *base_pts.get_or_insert(wire_pts);
        let frame_ts_ms = rel_ms_monotonic(wire_pts, base);
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
            // Unreachable — RTMP encode targets are H.264 / HEVC. Treat
            // any phantom Mpeg2 family as h264 framing rather than
            // panicking.
            video_codec::VideoCodec::Mpeg2 => build_avc_nalu_tag_raw(&avcc, keyframe),
        };
        let tag_len = tag.len();
        client.send_video(&tag, frame_ts_ms).await?;
        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
        stats.record_latency(recv_time_us);
    }
    Ok(true)
}

/// The codec half of [`encode_one_frame`]: one source access unit through
/// the decoder, the rate lock and the encoder. Returns every encoded frame
/// as `(Annex B, keyframe, encoder pts)`; `Err` only when the encoder failed
/// to open (terminal).
#[cfg(feature = "media-codecs")]
fn transcode_access_unit(
    active: &mut VideoActive,
    annex_b: &[u8],
    pts_90k: u64,
    output_id: &str,
) -> Result<Vec<(Vec<u8>, bool, i64)>, String> {
    active.stats.input_frames.fetch_add(1, Ordering::Relaxed);
    // Feed the source PTS in so libavcodec can echo it back per
    // decoded frame. `send_packet` (no pts) threw it away at the door,
    // which is why the emit path had nothing but a frame counter to
    // work from.
    if let Err(e) = active.decoder.send_packet_with_pts(annex_b, pts_90k as i64) {
        tracing::debug!("RTMP output '{}': decoder send_packet: {e:?}", output_id);
    }
    let mut out = Vec::new();
    loop {
        let frame = match active.decoder.receive_frame() {
            Ok(f) => f,
            Err(_) => break,
        };
        // The encoder rate, from the decoded frames' own cadence
        // unless pinned. Until it is known the encoder cannot open
        // (its time base is fixed at open), so the frame is dropped
        // before it reaches the PTS queue.
        if !active.rate.admit(frame.pts(), &mut active.pipeline) {
            continue;
        }
        // Display-order source PTS for this frame, straight from the
        // decoder's reorder queue. Falls back to the access unit's own
        // PTS when the source PES carried none.
        let src_pts_for_frame = match frame.pts() {
            Some(p) if p >= 0 => p as u64,
            _ => pts_90k,
        };
        active.src_pts_queue.push_back(src_pts_for_frame);

        let was_open = active.pipeline.is_open();
        // The encoder still gets the monotonic frame counter — it wants
        // a rate-control tick, not a clock, and passing 90 kHz ticks
        // against a 1/fps timebase is the documented VBV hazard.
        let encoded = match active.pipeline.encode(&frame, Some(active.out_frame_count)) {
            Ok(frames) => frames,
            Err(e) => {
                if !active.pipeline.is_open() {
                    // Terminal: encoder open failed.
                    return Err(format!("encoder open failed: {e}"));
                }
                tracing::debug!("RTMP output '{}': encode error: {e}", output_id);
                active.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                // Keep the FIFO balanced: this frame produced no output.
                active.src_pts_queue.pop_back();
                continue;
            }
        };
        // First time the pipeline reported itself open, capture
        // its extradata and build the FLV sequence header —
        // classic AVC for H.264, Enhanced RTMP hvcC for HEVC.
        if !was_open && active.pipeline.is_open()
            && let Some(ed) = active.pipeline.extradata() {
                active.sequence_header_tag = Some(match active.target_family {
                    video_codec::VideoCodec::H264 => {
                        build_avc_sequence_header_from_avcc(&ed)
                    }
                    video_codec::VideoCodec::Hevc => {
                        build_hevc_sequence_header_from_hvcc(&ed)
                    }
                    // Unreachable — `target_family` is the encoder
                    // output, validation rejects Mpeg2 here. Treat
                    // it as h264 so we don't panic on a phantom
                    // bitstream.
                    video_codec::VideoCodec::Mpeg2 => {
                        build_avc_sequence_header_from_avcc(&ed)
                    }
                });
            }
        active.out_frame_count += 1;
        for ef in encoded {
            out.push((ef.data, ef.keyframe, ef.pts));
            active.stats.output_frames.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(out)
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
    first_frame: Option<&[u8]>,
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

    if sample_rate_from_index(sr_idx).is_none() {
        tracing::error!(
            "RTMP output '{}': audio_encode rejected unsupported AAC sample_rate_index={sr_idx}",
            config.id
        );
        return EncoderState::Failed;
    }
    if ch_cfg == 0 || ch_cfg > 2 {
        tracing::error!(
            "RTMP output '{}': audio_encode rejected unsupported AAC channel_config={ch_cfg}",
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
            config.id,
            sample_rate_from_index(sr_idx).unwrap_or(0),
            ch_cfg
        );
        return EncoderState::Transparent;
    }

    // What the decoder really hands out: an HE-AAC header gives the core's
    // rate (24 kHz for a 48 kHz service) and v2's mono core, which SBR and
    // PS double. Resolved from the header, the stage and the encoder were
    // pinned to the core's format — 48 kHz decoded PCM resampled to 24 kHz
    // (a 12 kHz audio bandwidth) and v2's stereo mixed to mono.
    let Some((input_sr, input_ch)) = first_frame
        .and_then(|f| super::audio_decode::aac_decoded_format(profile, sr_idx, ch_cfg, f))
    else {
        tracing::debug!(
            "RTMP output '{}': audio_encode waits for an AAC frame that decodes",
            config.id
        );
        return EncoderState::Lazy;
    };

    // Resolve the encoder's input shape: the channel / rate stage converts
    // the source to it — the transcode block when set (audio_encode's
    // sample_rate / channels folded in for the fields it leaves unset),
    // otherwise those two alone. Without a block they used to reach the
    // encoder as bare parameters: the rate was converted by the encoder's
    // own resampler, a channel count other than the source's refused.
    let mut stage = super::audio_transcode::EncoderStage::new(
        config.transcode.clone(),
        enc_cfg.sample_rate,
        enc_cfg.channels,
    );
    let (target_sr, target_ch) = {
        match stage.prepare(input_sr, input_ch) {
            Ok(out) => out,
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
    };
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    // The stage has already aligned the PCM to (target_sr, target_ch), so
    // the encoder sees its target format as input and performs no internal
    // SRC / channel mapping.
    let params = EncoderParams {
        codec,
        sample_rate: target_sr,
        channels: target_ch,
        target_bitrate_kbps: target_br,
        target_sample_rate: target_sr,
        target_channels: target_ch,
        opus_vbr_mode: enc_cfg.opus_vbr_mode.clone(),
        opus_fec: enc_cfg.opus_fec,
        opus_dtx: enc_cfg.opus_dtx,
        opus_frame_duration_ms: enc_cfg.opus_frame_duration_ms,
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

    let mut encoder = match AudioEncoder::spawn(
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

    // The stage's resampler delay comes off the stamps with the codec's.
    encoder.set_upstream_delay(stage.delay(), target_sr);

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
        stage,
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
    // Taken by value, deliberately: silence must never *anchor* the shared
    // epoch, only read it.
    base_pts: Option<u64>,
) -> anyhow::Result<()> {
    let EncoderState::Active {
        encoder,
        silence: Some(sg),
        ..
    } = encoder_state
    else {
        return Ok(());
    };

    // Wait for a real essence to anchor the epoch. Until one has, there is
    // nothing for silence to be in sync with, and emitting is worse than
    // waiting: the generator starts at 0 and the audio encoder latches its
    // anchor from the first submission, so a chunk sent now would pin the
    // whole audio timeline to 0 while video lands on the source clock. The
    // cost is that an output connecting before any essence arrives emits no
    // audio tag until the first access unit, where it used to emit silence
    // from t≈0 — the right trade, since a video-only opening is universally
    // tolerated and a split A/V timeline is not.
    let Some(base) = base_pts else {
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
        let frame_ts_ms = rel_ms_monotonic(frame.pts, base);
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
/// The source decoder is built lazily on the first real AAC frame (via
/// [`lazy_build_decoder`]); the encoder's stage, pinned to the declared
/// format here, converts whatever the source turns out to be.
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

    // Declared encoder params — a transcode block's first, as on every
    // other build of this stage (`audio_transcode::encoder_stage`). These
    // are also the silence generator's PCM format, and the stage below is
    // pinned to them: real audio arriving later is converted to them. Sized
    // from `audio_encode` alone, a block's `channels: 1` (with its routing)
    // met an encoder opened in stereo, and the stage fell back to the
    // default conversion — stereo out, the operator's routing dropped.
    let block = config.transcode.as_ref();
    let target_sr = block.and_then(|b| b.sample_rate).or(enc_cfg.sample_rate).unwrap_or(48_000);
    let target_ch = block.and_then(|b| b.channels).or(enc_cfg.channels).unwrap_or(2).clamp(1, 2);
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: target_sr,
        channels: target_ch,
        target_bitrate_kbps: target_br,
        target_sample_rate: target_sr,
        target_channels: target_ch,
        opus_vbr_mode: enc_cfg.opus_vbr_mode.clone(),
        opus_fec: enc_cfg.opus_fec,
        opus_dtx: enc_cfg.opus_dtx,
        opus_frame_duration_ms: enc_cfg.opus_frame_duration_ms,
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

    // Real audio, when it arrives, is converted to the format the silence
    // opened the encoder at.
    let mut stage = super::audio_transcode::EncoderStage::new(
        config.transcode.clone(),
        enc_cfg.sample_rate,
        enc_cfg.channels,
    );
    stage.pin_output(target_sr, target_ch);

    EncoderState::Active {
        decoder: None,
        encoder,
        decode_stats,
        stage,
        silence: Some(silence),
    }
}

/// Build the source AAC decoder on the first real AAC frame observed while
/// the encoder is already running (the silent-fallback path). Logs and
/// leaves `decoder = None` on failure so we keep producing silence instead
/// of crashing the output. The encoder's stage (pinned to its format)
/// converts whatever the source turns out to be.
fn lazy_build_decoder(
    decoder: &mut Option<AacDecoder>,
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
    match AacDecoder::from_adts_config(profile, sr_idx, ch_cfg) {
        Ok(d) => *decoder = Some(d),
        Err(e) => {
            tracing::warn!(
                "RTMP output '{}': silent-fallback AacDecoder build failed: {e}; keeping silence track",
                output_id
            );
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

/// Classify a failed [`RtmpClient::connect`] (or publish-loop) error into an
/// operator-facing alarm: `(severity, error_code, reason, server_code)`.
///
/// RTMP gives us no dedicated "expired stream key" signal — a server rejects a
/// bad/expired/duplicate key with a generic publish rejection (or just drops
/// the socket). So we surface the two cases we *can* positively distinguish —
/// a publish rejection (`RtmpFailureKind::PublishRejected`, the stream-key
/// case) and a connect rejection (wrong URL/app) — from everything else, which
/// is a transport-level "couldn't reach the server". The typed
/// [`RtmpConnectError`] is recovered by walking the `anyhow` chain, so the
/// classification survives the `.context(...)` wrapping applied in
/// `RtmpClient::connect`.
///
/// `error_code` is the stable, machine-readable key the manager UI badges and
/// the reconnect loop dedups on; `reason` is the human sentence.
fn classify_rtmp_failure(
    err: &anyhow::Error,
) -> (EventSeverity, &'static str, String, Option<String>) {
    for cause in err.chain() {
        if let Some(rc) = cause.downcast_ref::<RtmpConnectError>() {
            return match rc.kind {
                RtmpFailureKind::PublishRejected => (
                    EventSeverity::Critical,
                    "rtmp_publish_rejected",
                    match &rc.server_code {
                        Some(c) => format!(
                            "the server rejected the publish ({c}) — the stream key is \
                             most likely wrong, expired, or already streaming elsewhere"
                        ),
                        None => "the server rejected the publish — the stream key is most \
                                 likely wrong, expired, or already streaming elsewhere"
                            .to_string(),
                    },
                    rc.server_code.clone(),
                ),
                RtmpFailureKind::ConnectRejected => (
                    EventSeverity::Critical,
                    "rtmp_connect_rejected",
                    match &rc.server_code {
                        Some(c) => format!(
                            "the server rejected the connection ({c}) — check the \
                             destination URL and app path"
                        ),
                        None => "the server rejected the connection — check the \
                                 destination URL and app path"
                            .to_string(),
                    },
                    rc.server_code.clone(),
                ),
            };
        }
    }
    (
        EventSeverity::Warning,
        "rtmp_connect_failed",
        "could not reach the RTMP server (unreachable, refused, reset, or timed out)"
            .to_string(),
        None,
    )
}

/// Everything a log line or an operator-facing event may say about an RTMP
/// destination: `scheme://authority/<first path segment>` — which is exactly
/// what [`super::rtmp::client`]'s `parse_rtmp_url` keeps and the connection
/// actually uses. Everything past it is dropped before it can be written
/// anywhere.
///
/// `RtmpClient::connect` documents `rtmp://live.twitch.tv/app/stream_key` as an
/// acceptable value for `dest_url` and silently discards everything past the
/// first path segment, so an operator who pastes a full platform ingest URL
/// would otherwise publish their channel key to the journal at INFO and — via
/// `details["dest_url"]` — off-box to the manager, which stores it. A query
/// string goes the same way: it is discarded by the connection too, and is
/// where several CDNs put a token.
///
/// `user:pass@` userinfo is dropped for the same reason and by the same
/// authority: `parse_rtmp_url` reads only `host`, `port` and the first path
/// segment off the parsed URL, so credentials in the authority are as ignored
/// by the connection as the trailing path is — and just as much a credential
/// in a log line if they survive. Akamai-style `rtmp://user:pass@host/app` is
/// the shape an operator would paste.
///
/// Borrows whenever nothing has to be removed from the middle, so a URL that
/// carries no secret is echoed back byte-for-byte and an operator's
/// diagnostics read exactly as they always have.
fn redact_dest_url(url: &str) -> Cow<'_, str> {
    let Some(authority) = url.find("://").map(|i| i + 3) else {
        return Cow::Borrowed(url);
    };
    // The authority (optional `user:pass@`, host, optional `:port`, bracketed
    // for IPv6) runs to the first `/`, `?` or `#`; the app is the path segment
    // after a `/`, and there is nothing to keep after any other terminator.
    let rest = &url[authority..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let mut end = authority + authority_len;
    if rest.as_bytes().get(authority_len) == Some(&b'/') {
        let app = &url[end + 1..];
        end += 1 + app.find(['/', '?', '#']).unwrap_or(app.len());
    }
    match rest[..authority_len].rfind('@') {
        Some(at) => Cow::Owned(format!("{}{}", &url[..authority], &url[authority + at + 1..end])),
        None => Cow::Borrowed(&url[..end]),
    }
}

/// Build an output-scoped RTMP event carrying a stable `error_code` plus the
/// (secret-free) destination URL, merging any caller-supplied `extra` details.
/// Flow- and output-scoped so the manager UI can attribute it to the right
/// flow card and the events feed's RTMP filter picks it up. The `stream_key`
/// is deliberately never included.
fn rtmp_output_event(
    severity: EventSeverity,
    error_code: &str,
    message: String,
    config: &RtmpOutputConfig,
    flow_id: &str,
    extra: serde_json::Value,
) -> Event {
    let mut details = serde_json::json!({
        "error_code": error_code,
        "dest_url": redact_dest_url(&config.dest_url),
    });
    if let (serde_json::Value::Object(base), serde_json::Value::Object(extra)) =
        (&mut details, extra)
    {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    Event {
        severity,
        category: category::RTMP.to_string(),
        message,
        details: Some(details),
        flow_id: Some(flow_id.to_string()),
        input_id: None,
        output_id: Some(config.id.clone()),
    }
}

/// An x264 source at 25 fps (IPPP, SPS on the first AU), as Annex B access
/// units with their 90 kHz PTS. Shared by the RTMP and WebRTC rate tests.
#[cfg(all(test, feature = "video-encoder-x264"))]
pub(crate) fn x264_test_source(frames: usize, step_90k: u64) -> Vec<(Vec<u8>, u64)> {
    use video_codec::{VideoEncoderCodec, VideoEncoderConfig, VideoPreset};
    let (w, h) = (320usize, 240usize);
    let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
        codec: VideoEncoderCodec::X264,
        width: w as u32,
        height: h as u32,
        fps_num: 25,
        fps_den: 1,
        gop_size: 50,
        preset: VideoPreset::Veryfast,
        global_header: false,
        ..VideoEncoderConfig::default()
    })
    .unwrap();
    let mut aus = Vec::new();
    for i in 0..frames {
        let y: Vec<u8> = (0..w * h).map(|k| ((k % w + 5 * i) % 200) as u8 + 20).collect();
        let c = vec![128u8; w / 2 * h / 2];
        aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
    }
    aus.extend(enc.flush().unwrap());
    assert_eq!(aus.len(), frames);
    aus.into_iter().map(|f| (f.data, 900_000 + f.pts as u64 * step_90k)).collect()
}

#[cfg(all(test, feature = "video-encoder-x264"))]
mod rate_tests {
    use super::*;

    fn run(cfg: serde_json::Value, step_90k: u64) -> (VideoEncoderState, usize) {
        let cfg: VideoEncodeConfig = serde_json::from_value(cfg).unwrap();
        let config: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k"
        }))
        .unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "rtmp".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let aus = x264_test_source(40, step_90k);
        let mut state = open_video_active(&cfg, true, &aus[0].0, &config, &stats, &events);
        let mut encoded = 0;
        for (au, pts) in &aus {
            let VideoEncoderState::Active(active) = &mut state else { panic!("not active") };
            encoded += transcode_access_unit(active, au, *pts, "o").unwrap().len();
        }
        (state, encoded)
    }

    fn vui_timing(state: &VideoEncoderState) -> Option<(u32, u32)> {
        let VideoEncoderState::Active(active) = state else { return None };
        let extradata = active.pipeline.extradata()?;
        video_engine::find_h264_sps(&extradata)?.timing.map(|(n, t, _)| (n, t))
    }

    /// Unpinned, the encoder opens at the source's measured rate — it used
    /// to open at a flat 30/1, so a 25 fps source's VUI said 30 fps and CBR
    /// budgeted 25/30 of the configured bitrate. The frames decoded while
    /// the rate is measured are dropped (four here).
    #[test]
    fn an_unpinned_rtmp_encode_opens_at_the_source_rate() {
        let (state, encoded) = run(serde_json::json!({"codec": "x264", "preset": "veryfast"}), 3_600);
        let VideoEncoderState::Active(active) = &state else { panic!("not active") };
        assert_eq!(active.pipeline.fps(), (25, 1));
        assert_eq!(vui_timing(&state), Some((1, 50)), "VUI 25 fps");
        assert_eq!(encoded, 40 - 4);
        // 29.97 fps (3003-tick steps).
        let (state, _) = run(serde_json::json!({"codec": "x264", "preset": "veryfast"}), 3_003);
        let VideoEncoderState::Active(active) = &state else { panic!("not active") };
        assert_eq!(active.pipeline.fps(), (30_000, 1001));
        assert_eq!(vui_timing(&state), Some((1001, 60_000)));
    }

    /// A pinned rate is used as it is, from the first frame.
    #[test]
    fn a_pinned_rtmp_rate_encodes_every_frame() {
        let (state, encoded) = run(
            serde_json::json!({"codec": "x264", "preset": "veryfast", "fps_num": 50, "fps_den": 1}),
            3_600,
        );
        let VideoEncoderState::Active(active) = &state else { panic!("not active") };
        assert_eq!(active.pipeline.fps(), (50, 1));
        assert_eq!(encoded, 40);
    }
}

#[cfg(test)]
mod flv_timestamp_tests {
    use super::*;
    use std::collections::VecDeque;

    const STEP_25FPS: u64 = 3600; // 90_000 / 25

    /// The defect: FLV video timestamps came from the encoder's output frame
    /// index times the *declared* rate, while transcoded audio came from the
    /// source clock. On a 25 fps source at the 30/1 default the two diverged
    /// by 166.7 ms per second of playout, unbounded.
    #[test]
    fn wire_pts_tracks_the_source_not_the_declared_rate() {
        let mut q: VecDeque<u64> = (0..5).map(|i| 1_000_000 + i * STEP_25FPS).collect();
        let mut last = None;
        let mut out = Vec::new();
        for _ in 0..5 {
            let p = next_wire_pts_90k(&mut q, last, 3000 /* 30 fps step */);
            last = Some(p);
            out.push(p);
        }
        // Spacing follows the source's 25 fps, not the declared 30 fps.
        for w in out.windows(2) {
            assert_eq!(w[1] - w[0], STEP_25FPS);
        }
    }

    #[test]
    fn wire_pts_is_strictly_increasing_across_a_batch_drain() {
        // A decoder draining several frames at once must still yield distinct
        // increasing stamps — the property the old frame counter guaranteed.
        let mut q: VecDeque<u64> = VecDeque::from(vec![9_000, 12_600, 16_200]);
        let mut last = None;
        let mut prev = 0u64;
        for i in 0..3 {
            let p = next_wire_pts_90k(&mut q, last, STEP_25FPS);
            if i > 0 {
                assert!(p > prev, "DTS must not stall or go backwards");
            }
            prev = p;
            last = Some(p);
        }
    }

    #[test]
    fn a_backwards_source_pts_substitutes_one_step() {
        let mut q: VecDeque<u64> = VecDeque::from(vec![100_000, 50_000]);
        let first = next_wire_pts_90k(&mut q, None, STEP_25FPS);
        let second = next_wire_pts_90k(&mut q, Some(first), STEP_25FPS);
        assert_eq!(second, first + STEP_25FPS, "never emit a backwards DTS");
    }

    /// The audio timeline anchors once and free-runs on a sample count, so it
    /// cannot follow a forward source jump. Letting video jump while audio
    /// cannot is exactly the A/V split this fix closes, so a leap is clamped
    /// to one nominal step in both directions.
    #[test]
    fn a_forward_source_leap_is_clamped_to_one_step() {
        let mut q: VecDeque<u64> = VecDeque::from(vec![0, 90_000 * 60]);
        let first = next_wire_pts_90k(&mut q, None, STEP_25FPS);
        let second = next_wire_pts_90k(&mut q, Some(first), STEP_25FPS);
        assert_eq!(second, first + STEP_25FPS, "a 60s leap must not split A/V");
    }

    #[test]
    fn a_gap_within_tolerance_is_honoured() {
        // A real one-second gap is under the 2s threshold and passes through.
        let mut q: VecDeque<u64> = VecDeque::from(vec![0, 90_000]);
        let first = next_wire_pts_90k(&mut q, None, STEP_25FPS);
        let second = next_wire_pts_90k(&mut q, Some(first), STEP_25FPS);
        assert_eq!(second, 90_000);
    }

    /// Audio and video share one epoch, so equal source instants produce equal
    /// FLV timestamps. Before the fix, audio used the raw absolute source PTS
    /// while video started at 0 — on a feed from an upstream bilbycast edge
    /// (whose rewriter anchors on the master clock) that put audio tags up to
    /// ~26.5 h ahead of video.
    #[test]
    fn audio_and_video_share_one_epoch() {
        let epoch = 4_000_000_000u64; // a large, realistic source PTS
        let one_second_later = epoch + 90_000;
        assert_eq!(rel_ms_monotonic(epoch, epoch), 0);
        assert_eq!(rel_ms_monotonic(one_second_later, epoch), 1000);
    }

    /// A monotonic timeline must not have a wrap heuristic applied — doing so
    /// caps it at 2^33 ticks (~26.5 h) and then pins every later tag to 0.
    #[test]
    fn a_monotonic_timeline_does_not_wrap_at_33_bits() {
        let base = 0u64;
        let past_33_bits = (1u64 << 33) + 90_000;
        assert!(rel_ms_monotonic(past_33_bits, base) > 95_000_000);
    }

    #[test]
    fn a_value_behind_the_epoch_clamps_to_zero() {
        assert_eq!(rel_ms_monotonic(500, 90_000), 0);
    }

    /// The raw-source helper keeps its wrap heuristic — a 33-bit PTS really
    /// does wrap, and a value that appears to be far in the future is a wrap.
    #[test]
    fn a_wrapped_raw_source_pts_clamps_to_zero() {
        assert_eq!(rel_ms_wrapping(0, 90_000), 0, "one frame behind reads as a wrap");
        assert_eq!(rel_ms_wrapping(180_000, 90_000), 1000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **B1.** `audio_encode.sample_rate` / `channels` without a
    /// `transcode` block go through the shared channel / rate stage: the
    /// encoder is opened at the output format and fed PCM converted to it.
    /// A channel override used to open the encoder with the source's layout
    /// as input and the override as output, which the in-process backends
    /// refuse — the output raised a Critical `audio_encode` event and
    /// carried no audio.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn a_rate_and_channel_override_without_a_transcode_block_converts() {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let cfg: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "r1", "name": "r1", "dest_url": "rtmp://127.0.0.1/app", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc", "channels": 1, "sample_rate": 44100 }
        }))
        .unwrap();
        let mut demux = TsDemuxer::new(None);
        demux.demux(&crate::engine::ts_test_fixtures::aac_program_ts(ADTS));
        assert_eq!(demux.cached_aac_config(), Some((1, 3, 2)), "a stereo 48 kHz source");
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("r1".into(), "r1".into(), "rtmp".into()));
        let first = first_aac_frame(&mut TsDemuxer::new(None), &crate::engine::ts_test_fixtures::aac_program_ts(ADTS));
        let st = build_encoder_state(&cfg, &demux, Some(&first), true, &CancellationToken::new(), &stats, "f", &events);
        let EncoderState::Active { encoder, mut stage, .. } = st else {
            panic!("the output must re-encode, not fail");
        };
        let p = encoder.params();
        assert_eq!((p.sample_rate, p.channels), (44_100, 1), "the encoder takes the output format");
        assert_eq!((p.target_sample_rate, p.target_channels), (44_100, 1));
        let out = stage.process(&[vec![0.1f32; 1024], vec![0.1f32; 1024]], 48_000).unwrap();
        assert_eq!(out.len(), 1, "the stage mixes to mono");
        assert!(stage.delay() > 0, "and resamples, in streaming mode");
    }

    /// The first `Aac` frame `demux` hands out for `ts`.
    #[cfg(feature = "fdk-aac")]
    fn first_aac_frame(demux: &mut TsDemuxer, ts: &[u8]) -> Vec<u8> {
        demux
            .demux(ts)
            .into_iter()
            .find_map(|f| match f {
                DemuxedFrame::Aac { data, .. } => Some(data),
                _ => None,
            })
            .expect("an AAC frame")
    }

    /// An HE-AAC source's ADTS header says 24 kHz (the core) and, for v2,
    /// mono (the core under PS). The re-encode is set up from what the
    /// decoder hands out — 48 kHz stereo — not from that: pinned to the
    /// header, it resampled the decoded 48 kHz to 24 kHz (a 12 kHz audio
    /// bandwidth) and mixed v2's stereo to mono.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn an_he_aac_source_is_re_encoded_at_its_decoded_format() {
        let cfg: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "r1", "name": "r1", "dest_url": "rtmp://127.0.0.1/app", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc", "bitrate_kbps": 128 }
        }))
        .unwrap();
        for (v2, ch_cfg) in [(false, 2u8), (true, 1)] {
            let ts = crate::engine::ts_test_fixtures::aac_program_ts(
                &crate::engine::ts_test_fixtures::he_aac_adts(v2),
            );
            let mut demux = TsDemuxer::new(None);
            let first = first_aac_frame(&mut demux, &ts);
            assert_eq!(demux.cached_aac_config(), Some((1, 6, ch_cfg)), "v2 {v2}: LC, 24 kHz core");
            let (events, _rx) = crate::manager::events::event_channel();
            let stats = Arc::new(OutputStatsAccumulator::new("r1".into(), "r1".into(), "rtmp".into()));
            let st = build_encoder_state(
                &cfg,
                &demux,
                Some(&first),
                true,
                &CancellationToken::new(),
                &stats,
                "f",
                &events,
            );
            let EncoderState::Active { encoder, mut stage, .. } = st else {
                panic!("v2 {v2}: the output must re-encode");
            };
            let p = encoder.params();
            assert_eq!((p.sample_rate, p.channels), (48_000, 2), "v2 {v2}: the decoded format");
            let out = stage.process(&[vec![0.1f32; 2048], vec![0.1f32; 2048]], 48_000).unwrap();
            assert_eq!((out.len(), out[0].len()), (2, 2048), "v2 {v2}: nothing to convert");
            assert_eq!(stage.delay(), 0);
        }
    }

    /// A silent-fallback encoder, built before any source audio, is sized
    /// from the transcode block first — its `channels: 1` and routing (left
    /// only, here) — then `audio_encode`. Sized from `audio_encode` alone it
    /// opened in stereo, the stage pinned to that found the block's mono
    /// did not fit and fell back to the default conversion: stereo out, the
    /// routing dropped.
    #[test]
    fn a_silent_fallback_encoder_takes_the_transcode_blocks_format() {
        let cfg: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "r1", "name": "r1", "dest_url": "rtmp://127.0.0.1/app", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc", "silent_fallback": true },
            "transcode": { "channels": 1, "channel_map_with_gain": [[[0, 1.0]]] }
        }))
        .unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("r1".into(), "r1".into(), "rtmp".into()));
        let st = build_encoder_state_eager_for_silent_fallback(
            &cfg,
            &CancellationToken::new(),
            &stats,
            "f",
            &events,
        );
        let EncoderState::Active { encoder, mut stage, .. } = st else {
            panic!("silent fallback builds eagerly");
        };
        assert_eq!((encoder.params().sample_rate, encoder.params().channels), (48_000, 1));
        let out = stage.process(&[vec![0.5f32; 1024], vec![0.0f32; 1024]], 48_000).unwrap();
        assert_eq!(out.len(), 1, "mono");
        assert!((out[0][512] - 0.5).abs() < 1e-6, "the block's routing: left only, {}", out[0][512]);
    }

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

    /// A transport-level failure (no typed `RtmpConnectError` in the chain —
    /// unreachable host, reset, timeout) classifies as a Warning
    /// `rtmp_connect_failed` with no server code.
    #[test]
    fn classify_transport_failure_is_warning() {
        let err = anyhow::anyhow!("connection reset by peer");
        let (sev, code, _reason, server_code) = classify_rtmp_failure(&err);
        assert_eq!(sev, EventSeverity::Warning);
        assert_eq!(code, "rtmp_connect_failed");
        assert!(server_code.is_none());
    }

    /// The bad/expired-stream-key case: a `PublishRejected` classifies as a
    /// Critical `rtmp_publish_rejected` carrying the server code, and the
    /// classification survives the `.context(...)` wrapping that
    /// `RtmpClient::connect` applies — proving the `anyhow` chain walk works.
    #[test]
    fn classify_publish_rejected_survives_context_wrapping() {
        use anyhow::Context;
        let err: anyhow::Error = Err::<(), _>(RtmpConnectError {
            kind: RtmpFailureKind::PublishRejected,
            server_code: Some("NetStream.Publish.BadName".to_string()),
        })
        .context("RTMP: publish rejected by server")
        .unwrap_err();

        let (sev, code, reason, server_code) = classify_rtmp_failure(&err);
        assert_eq!(sev, EventSeverity::Critical);
        assert_eq!(code, "rtmp_publish_rejected");
        assert_eq!(server_code.as_deref(), Some("NetStream.Publish.BadName"));
        assert!(
            reason.contains("stream key"),
            "reason must name the stream key: {reason}"
        );
    }

    /// A `ConnectRejected` (wrong URL / app) classifies as a Critical
    /// `rtmp_connect_rejected`.
    #[test]
    fn classify_connect_rejected_is_critical() {
        let err: anyhow::Error = RtmpConnectError {
            kind: RtmpFailureKind::ConnectRejected,
            server_code: None,
        }
        .into();
        let (sev, code, _reason, _sc) = classify_rtmp_failure(&err);
        assert_eq!(sev, EventSeverity::Critical);
        assert_eq!(code, "rtmp_connect_rejected");
    }

    /// The event carries the flow + output scope and the (secret-free)
    /// destination URL, and the stream key never appears in the payload.
    #[test]
    fn rtmp_output_event_scoped_and_never_leaks_stream_key() {
        let config: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "out-1",
            "name": "twitch",
            "dest_url": "rtmp://live.twitch.tv/app",
            "stream_key": "live_000_SUPER_SECRET_KEY",
        }))
        .expect("minimal RTMP output config deserialises");

        let ev = rtmp_output_event(
            EventSeverity::Critical,
            "rtmp_publish_rejected",
            "RTMP output 'out-1' — the server rejected the publish".to_string(),
            &config,
            "flow-xyz",
            serde_json::json!({ "server_code": "NetStream.Publish.BadName" }),
        );

        assert_eq!(ev.severity, EventSeverity::Critical);
        assert_eq!(ev.category, category::RTMP);
        assert_eq!(ev.flow_id.as_deref(), Some("flow-xyz"));
        assert_eq!(ev.output_id.as_deref(), Some("out-1"));
        assert!(ev.input_id.is_none());
        let details = ev.details.clone().expect("details present");
        assert_eq!(details["error_code"], "rtmp_publish_rejected");
        assert_eq!(details["dest_url"], "rtmp://live.twitch.tv/app");
        assert_eq!(details["server_code"], "NetStream.Publish.BadName");

        let blob = format!(
            "{} {}",
            ev.message,
            serde_json::to_string(&ev.details).unwrap()
        );
        assert!(
            !blob.contains("SUPER_SECRET"),
            "stream key leaked into event payload: {blob}"
        );
    }

    /// The case the test above could not catch, because its fixture URL was
    /// already clean: the credential embedded in `dest_url` itself. The
    /// documented form of that field is a full platform ingest URL, so this is
    /// what an operator who pastes one from Twitch/YouTube actually stores.
    #[test]
    fn a_stream_key_embedded_in_dest_url_never_reaches_an_event() {
        let config: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "out-1",
            "name": "twitch",
            "dest_url": "rtmp://live.twitch.tv/app/live_000_SUPER_SECRET_KEY",
            "stream_key": "",
        }))
        .expect("minimal RTMP output config deserialises");

        let ev = rtmp_output_event(
            EventSeverity::Critical,
            "rtmp_connect_failed",
            format!(
                "RTMP output '{}' lost its connection to {}",
                config.id,
                redact_dest_url(&config.dest_url)
            ),
            &config,
            "flow-xyz",
            serde_json::json!({}),
        );

        let details = ev.details.clone().expect("details present");
        assert_eq!(details["dest_url"], "rtmp://live.twitch.tv/app");
        let blob = format!(
            "{} {}",
            ev.message,
            serde_json::to_string(&ev.details).unwrap()
        );
        assert!(
            !blob.contains("SUPER_SECRET"),
            "stream key embedded in dest_url leaked into the event: {blob}"
        );
    }

    #[test]
    fn redact_dest_url_keeps_exactly_what_the_connection_uses() {
        // Unchanged when there is nothing to drop — an operator's existing
        // diagnostics must read byte-for-byte as they always have.
        for clean in [
            "rtmp://live.twitch.tv/app",
            "rtmps://host:1936/live",
            "rtmp://[2001:db8::1]:1935/live",
            "rtmp://host",
            "not a url at all",
        ] {
            assert_eq!(redact_dest_url(clean), clean, "must not rewrite {clean}");
        }
        // Everything past the app segment is what `parse_rtmp_url` discards,
        // and is where the credential rides.
        assert_eq!(
            redact_dest_url("rtmp://live.twitch.tv/app/live_000_SECRET"),
            "rtmp://live.twitch.tv/app"
        );
        assert_eq!(
            redact_dest_url("rtmps://a.rtmp.youtube.com:443/live2/abcd-efgh-ijkl"),
            "rtmps://a.rtmp.youtube.com:443/live2"
        );
        assert_eq!(
            redact_dest_url("rtmp://[2001:db8::1]:1935/live/key/extra"),
            "rtmp://[2001:db8::1]:1935/live"
        );
        // A query string is discarded by the connection too, and is where
        // several CDNs put their token.
        assert_eq!(
            redact_dest_url("rtmp://cdn.example.com/live?token=SECRET"),
            "rtmp://cdn.example.com/live"
        );
        assert_eq!(
            redact_dest_url("rtmp://cdn.example.com?token=SECRET"),
            "rtmp://cdn.example.com"
        );
        // `parse_rtmp_url` reads host / port / first path segment only, so
        // userinfo is ignored by the connection exactly like the trailing path
        // — and is a credential in a log line if it survives.
        assert_eq!(
            redact_dest_url("rtmp://user:hunter2@cdn.example.com/live"),
            "rtmp://cdn.example.com/live"
        );
        assert_eq!(
            redact_dest_url("rtmp://user:hunter2@cdn.example.com/live/key"),
            "rtmp://cdn.example.com/live"
        );
        assert_eq!(
            redact_dest_url("rtmps://user:hunter2@[2001:db8::1]:1936"),
            "rtmps://[2001:db8::1]:1936"
        );
        // An `@` in the path is not userinfo and must not be treated as a
        // credential boundary.
        assert_eq!(
            redact_dest_url("rtmp://cdn.example.com/live@2"),
            "rtmp://cdn.example.com/live@2"
        );
    }
}
