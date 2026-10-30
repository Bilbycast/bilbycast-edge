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
use super::cmaf::codecs::{push_avcc_record, push_hvcc_record};
use super::cmaf::fmp4::VideoCodec as CmafVideoCodec;
use super::cmaf::nalu::{H265_NAL_VPS, h265_nal_type, to_length_prefixed};
use super::cmaf::param_sets::{ParamSetCollector, ParamSets};
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
    /// The source PTS of each decoded picture (see `FramePtsStamper`).
    stamper: crate::engine::video_encode_util::FramePtsStamper,
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
    let mut passthrough = PassthroughVideo::default();
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
    // A source codec RTMP cannot carry without a re-encode has been
    // reported (`warn_codec_needs_encode`), once per connection.
    let mut mpeg2_warned = false;
    let mut other_audio_warned = false;
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
                // Every field is named: a `..` here dropped the DTS when the
                // demuxer started surfacing it, and the tags went out in
                // presentation order (edge#205).
                DemuxedFrame::H264 { nalus, pts, is_keyframe, pts_known, dts } => {
                    let (ts_ms, cts_ms) = flv_video_time(pts, dts, &mut base_pts);
                    if process_video_frame(
                        VideoFrameSource::H264 { nalus: &nalus, is_keyframe, pts_known },
                        pts,
                        ts_ms,
                        cts_ms,
                        &mut base_pts,
                        recv_time_us,
                        &mut passthrough,
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
                DemuxedFrame::H265 { nalus, pts, is_keyframe, pts_known, dts } => {
                    let (ts_ms, cts_ms) = flv_video_time(pts, dts, &mut base_pts);
                    if process_video_frame(
                        VideoFrameSource::H265 { nalus: &nalus, is_keyframe, pts_known },
                        pts,
                        ts_ms,
                        cts_ms,
                        &mut base_pts,
                        recv_time_us,
                        &mut passthrough,
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
                        && let Some(cached) = demuxer.cached_aac_config()
                        && let Some((asc_sr_idx, asc_ch_cfg)) =
                            aac_sequence_header_format(&encoder_state, cached)
                    {
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

                    // RTMP carries AAC: MP2 / AC-3 / E-AC-3 / AAC-LATM reach
                    // it only re-encoded (`audio_encode`). A source no
                    // decoder here can read (AC-4) is dropped without the
                    // Warning — naming `audio_encode` would not help it.
                    if matches!(encoder_state, EncoderState::Disabled) {
                        if !other_audio_warned
                            && let Some(what) =
                                crate::engine::audio_decode::reencodable_audio_label(stream_type)
                        {
                            other_audio_warned = true;
                            warn_codec_needs_encode(
                                config,
                                "audio",
                                &what,
                                "audio_encode",
                                flow_id,
                                event_sender,
                            );
                        }
                        continue;
                    }
                    let decoded = decode_other_audio_for_encode(
                        &mut encoder_state,
                        &mut ff_audio_decoder,
                        &mut ff_audio_codec,
                        stream_type,
                        &data,
                        pts,
                        config,
                        cancel,
                        stats,
                        flow_id,
                        event_sender,
                    );
                    let EncoderState::Active {
                        encoder, silence, stage, decode_stats, ..
                    } = &mut encoder_state
                    else {
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

                    for frame in decoded {
                        decode_stats.inc_output();
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
                // RTMP carries H.264 / HEVC: MPEG-2 video goes out only
                // re-encoded (`video_encode`), which the decoder layer
                // does. Without one it is dropped, and the operator told
                // once — it used to be dropped either way, silently.
                DemuxedFrame::Mpeg2 { es, pts, is_keyframe, pts_known } => {
                    if matches!(video_state, VideoEncoderState::Disabled) {
                        if !mpeg2_warned {
                            mpeg2_warned = true;
                            warn_codec_needs_encode(config, "video", "MPEG-2 video", "video_encode", flow_id, event_sender);
                        }
                        continue;
                    }
                    // Re-encode only, which builds its own timeline: no
                    // composition time to carry.
                    let ts_ms = pts_to_ms(pts, &mut base_pts);
                    process_video_frame(
                        VideoFrameSource::Mpeg2 { es: &es, is_keyframe, pts_known },
                        pts,
                        ts_ms,
                        0,
                        &mut base_pts,
                        recv_time_us,
                        &mut passthrough,
                        &mut video_state,
                        client,
                        config,
                        stats,
                        flow_id,
                        event_sender,
                    )
                    .await?;
                }
                // An input switch: passthrough starts its parameter-set
                // table again, so the new source's sets replace the old
                // one's in the sequence header (edge#211). The FLV timeline
                // is not re-anchored here (edge#211 item 3).
                DemuxedFrame::Discontinuity => passthrough.restart(),
                DemuxedFrame::Scte35(_) => {}
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

/// FLV timing of one passed-through H.264 / HEVC picture: `(tag timestamp,
/// composition time)`, in ms on the connection's epoch (edge#205).
///
/// An FLV tag timestamp is the picture's **decode** time, and its composition
/// time how much later it presents. On a B-frame source the tags therefore go
/// out in decode order, and the receiver presents each picture at
/// `timestamp + composition time`. Each picture used to be tagged at its PTS
/// with composition time 0, so the tag stream stepped back at every reordered
/// picture.
///
/// The decode time is the PTS less the composition offset that
/// [`super::cmaf::composition_offset_90k`] reads off the PES's DTS, which is
/// the judgement CMAF passthrough makes: a DTS after the PTS, or more than 2 s
/// before it, counts as none. With no DTS (a stream without B-frames, every
/// non-reference B-picture, a PES with no timestamp) the result is exactly
/// [`pts_to_ms`] with composition time 0, byte for byte what went out before.
///
/// - The first essence anchors the epoch. A picture anchors it on its decode
///   time, the earliest stamp any later picture carries; anchored on its PTS
///   instead, every picture decoding before that PTS would clamp to 0.
/// - The composition time is `pts_ms − dts_ms`, not the offset rounded on its
///   own, so a picture presents at exactly the `floor((pts − epoch) / 90)`
///   audio is mapped with. A/V sync is unchanged; only the decode time moves.
/// - Behind an epoch audio anchored, a picture's decode time clamps to 0 and
///   its composition time carries the whole of its PTS: still presented on
///   time.
/// - The decode time saturates at 0 rather than wrapping. A PTS just past the
///   33-bit wrap, below its offset, would otherwise put the decode time (and,
///   on a connection's first picture, the epoch) near 2^33, which
///   [`rel_ms_wrapping`] reads as ~26.5 h ahead.
fn flv_video_time(pts: u64, dts: Option<u64>, base_pts: &mut Option<u64>) -> (u32, u32) {
    let offset = dts.map_or(0, |d| super::cmaf::composition_offset_90k(pts, d));
    let dts = pts.saturating_sub(offset);
    let base = *base_pts.get_or_insert(dts);
    let dts_ms = rel_ms_wrapping(dts, base);
    let pts_ms = rel_ms_wrapping(pts, base);
    (dts_ms, pts_ms.saturating_sub(dts_ms))
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

/// Build an FLV video tag with length-prefixed NALUs, carrying the picture's
/// composition time (ms after its decode time, the tag timestamp — see
/// [`flv_video_time`]).
fn build_avc_nalu_tag(nalus: &[Vec<u8>], is_keyframe: bool, cts_ms: u32) -> Vec<u8> {
    // Calculate total payload size
    let payload_size: usize = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut buf = BytesMut::with_capacity(5 + payload_size);

    // FLV video tag header
    let frame_type: u8 = if is_keyframe { 0x17 } else { 0x27 }; // keyframe/inter + AVC
    buf.put_u8(frame_type);
    buf.put_u8(0x01); // AVC NALU
    put_si24_composition_time(&mut buf, cts_ms);

    // Length-prefixed NALUs
    for nalu in nalus {
        buf.put_u32(nalu.len() as u32);
        buf.put_slice(nalu);
    }

    buf.to_vec()
}

/// FourCC identifier for HEVC in the Enhanced RTMP v2 extended VideoTagHeader.
const FOURCC_HVC1: [u8; 4] = *b"hvc1";

/// Build an Enhanced RTMP v2 SequenceStart tag payload for HEVC from the
/// re-encoder's extradata.
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
    // otherwise split the Annex-B and build the record passthrough sends,
    // its header read from the SPS RBSP (edge#211).
    if !extradata.is_empty() && extradata[0] == 0x01 {
        let mut buf = BytesMut::with_capacity(5 + extradata.len());
        buf.put_u8(0x80 | (1 << 4)); // Ex | keyframe | PacketType=SequenceStart
        buf.put_slice(&FOURCC_HVC1);
        buf.put_slice(extradata);
        return buf.to_vec();
    }
    match hevc_annex_b_param_sets(extradata) {
        Some(sets) => sequence_header_tag(CmafVideoCodec::H265, &sets),
        None => {
            tracing::warn!(
                "RTMP video_encode: HEVC extradata had no VPS/SPS/PPS NALs ({} bytes)",
                extradata.len()
            );
            Vec::new()
        }
    }
}

/// The first VPS, SPS and PPS of an Annex-B bytestream as emitted by
/// libx265, or `None` unless all three are there.
fn hevc_annex_b_param_sets(extradata: &[u8]) -> Option<ParamSets> {
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
    Some(ParamSets::single(CmafVideoCodec::H265, vps?.to_vec(), sps?.to_vec(), pps?.to_vec()))
}

/// Build an Enhanced RTMP v2 coded-frames tag payload for HEVC.
///
/// A picture that presents at its decode time (`cts_ms` 0) goes out as
/// `PacketType=3` (CodedFramesX), which has no composition-time field — every
/// re-encoded picture (the encoder runs with `max_b_frames = 0`) and every
/// passed-through picture of a source without B-frames. One that presents
/// later (a reference picture of a B-frame source passed through) goes out as
/// `PacketType=1` (CodedFrames) with the SI24 composition time after the
/// FourCC. It used to go out as CodedFramesX too, presenting at its decode
/// time (edge#205). The same choice FFmpeg's FLV muxer makes.
///
/// Layout: `<0x93|0xA3> "hvc1" <AVCC-framed NAL units>`, or
/// `<0x91|0xA1> "hvc1" <SI24 composition time> <AVCC-framed NAL units>`
fn build_hevc_coded_frames_tag(avcc_nalus: &[u8], is_keyframe: bool, cts_ms: u32) -> Vec<u8> {
    let frame_type: u8 = if is_keyframe { 1 } else { 2 };
    // PacketType: 1 = CodedFrames (carries a composition time), 3 = CodedFramesX.
    let packet_type: u8 = if cts_ms == 0 { 3 } else { 1 };
    let mut buf = BytesMut::with_capacity(8 + avcc_nalus.len());
    buf.put_u8(0x80 | (frame_type << 4) | packet_type); // Ex | key/inter | PacketType
    buf.put_slice(&FOURCC_HVC1);
    if packet_type == 1 {
        put_si24_composition_time(&mut buf, cts_ms);
    }
    buf.put_slice(avcc_nalus);
    buf.to_vec()
}

/// Largest composition time an FLV `SI24` field carries. The offset is capped
/// at 2 s upstream ([`super::cmaf::composition_offset_90k`]), so this never
/// binds; it only keeps the field from reading as negative.
const MAX_FLV_COMPOSITION_TIME_MS: u32 = 0x7F_FFFF;

/// Write an FLV composition time as the 24-bit big-endian SI24 field. It is
/// never negative here: a picture presents at or after its decode time.
fn put_si24_composition_time(buf: &mut BytesMut, cts_ms: u32) {
    let cts_ms = cts_ms.min(MAX_FLV_COMPOSITION_TIME_MS);
    buf.put_slice(&cts_ms.to_be_bytes()[1..]);
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
    buf.put_u8(0x00); // composition time 0 (B-frames disabled at encoder)
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
/// the first PPS (type 8), and build the record passthrough sends
/// ([`sequence_header_tag`]). As a fallback, when extradata already looks
/// like a DCR (first byte = `0x01`), we wrap it directly.
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
        (Some(sps), Some(pps)) => sequence_header_tag(
            CmafVideoCodec::H264,
            &ParamSets::single(CmafVideoCodec::H264, Vec::new(), sps.to_vec(), pps.to_vec()),
        ),
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
/// - the Enhanced-RTMP `hvc1` `CodedFrames` / `CodedFramesX` tag body.
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
    H264 { nalus: &'a [Vec<u8>], is_keyframe: bool, pts_known: bool },
    H265 { nalus: &'a [Vec<u8>], is_keyframe: bool, pts_known: bool },
    /// An MPEG-2 video access unit (the ES as the PES carried it): RTMP
    /// carries H.264 / HEVC, so it reaches the wire only re-encoded. It used
    /// to be dropped whatever `video_encode` said, silently.
    #[cfg_attr(not(feature = "media-codecs"), allow(dead_code))]
    Mpeg2 { es: &'a [u8], is_keyframe: bool, pts_known: bool },
}

impl<'a> VideoFrameSource<'a> {
    fn is_keyframe(&self) -> bool {
        match self {
            VideoFrameSource::H264 { is_keyframe, .. }
            | VideoFrameSource::H265 { is_keyframe, .. }
            | VideoFrameSource::Mpeg2 { is_keyframe, .. } => *is_keyframe,
        }
    }
    /// The codec a decoder for this access unit opens.
    #[cfg(feature = "media-codecs")]
    fn codec(&self) -> video_codec::VideoCodec {
        match self {
            VideoFrameSource::H264 { .. } => video_codec::VideoCodec::H264,
            VideoFrameSource::H265 { .. } => video_codec::VideoCodec::Hevc,
            VideoFrameSource::Mpeg2 { .. } => video_codec::VideoCodec::Mpeg2,
        }
    }
    /// The access unit as a decoder takes it: Annex B for H.264 / HEVC, the
    /// ES itself for MPEG-2 (the decoder finds its own start codes).
    #[cfg(feature = "media-codecs")]
    fn decoder_input(&self) -> std::borrow::Cow<'a, [u8]> {
        match self {
            VideoFrameSource::H264 { nalus, .. } | VideoFrameSource::H265 { nalus, .. } => {
                std::borrow::Cow::Owned(nalus_to_annex_b(nalus))
            }
            VideoFrameSource::Mpeg2 { es, .. } => std::borrow::Cow::Borrowed(es),
        }
    }
    /// Whether the access unit's PES carried a PTS.
    #[cfg(feature = "media-codecs")]
    fn pts_known(&self) -> bool {
        match self {
            VideoFrameSource::H264 { pts_known, .. }
            | VideoFrameSource::H265 { pts_known, .. }
            | VideoFrameSource::Mpeg2 { pts_known, .. } => *pts_known,
        }
    }
}

/// Tell the operator, once, that the source's `what` (`kind`: "video" /
/// "audio") reaches this RTMP output only through `block` — RTMP carries
/// H.264 / HEVC and AAC — so without it the essence is dropped. It used to
/// be dropped without a word: an MPEG-2 + AC-3 source (VH1) produced an
/// RTMP publish with nothing in it and no event.
fn warn_codec_needs_encode(
    config: &RtmpOutputConfig,
    kind: &str,
    what: &str,
    block: &str,
    flow_id: &str,
    event_sender: &EventSender,
) {
    let msg = format!(
        "RTMP output '{}': the source's {what} cannot be carried over RTMP (H.264 / HEVC and AAC \
         only) without `{block}`; its {kind} is dropped",
        config.id
    );
    tracing::warn!("{msg}");
    event_sender.emit_output_with_details(
        EventSeverity::Warning,
        category::RTMP,
        msg,
        &config.id,
        serde_json::json!({
            "error_code": "codec_needs_encode",
            "essence": kind,
            "source_codec": what,
            "needs": block,
            "flow_id": flow_id,
        }),
    );
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
    // The picture's FLV decode time and composition time (`flv_video_time`).
    // Passthrough writes both; the re-encode builds its own timeline.
    ts_ms: u32,
    cts_ms: u32,
    base_pts: &mut Option<u64>,
    recv_time_us: u64,
    passthrough: &mut PassthroughVideo,
    video_state: &mut VideoEncoderState,
    client: &mut RtmpClient,
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    event_sender: &EventSender,
) -> anyhow::Result<bool> {
    match video_state {
        VideoEncoderState::Disabled => {
            passthrough_video(&src, ts_ms, cts_ms, recv_time_us, passthrough, client, config, stats).await
        }
        VideoEncoderState::Failed => {
            // Decoder/encoder already failed once — drop video silently
            // so the output keeps running audio.
            Ok(true)
        }
        #[cfg(feature = "media-codecs")]
        VideoEncoderState::Lazy { cfg, sps_gate } => {
            let au = src.decoder_input();
            let codec = src.codec();
            // Nothing decodes before the SPS; opening on it seeds the
            // decoder's reorder depth from it (`SpsOpenGate`).
            if !sps_gate.admits(codec, &au) {
                return Ok(true);
            }
            let cfg = cfg.clone();
            *video_state = open_video_active(
                &cfg,
                codec,
                &au,
                config,
                stats,
                event_sender,
            );
            if matches!(video_state, VideoEncoderState::Failed) {
                return Ok(true);
            }
            // Fall through to Active on the very same frame.
            encode_one_frame(&src, pts_90k, ts_ms, base_pts, recv_time_us, video_state, client, config, stats, flow_id, event_sender).await
        }
        #[cfg(feature = "media-codecs")]
        VideoEncoderState::Active(_) => {
            encode_one_frame(&src, pts_90k, ts_ms, base_pts, recv_time_us, video_state, client, config, stats, flow_id, event_sender).await
        }
    }
}

/// Pictures held for want of a sequence header before the output says why,
/// once per connection: 20 s at 50 fps, 40 s at 25 — past any GOP a
/// broadcast source keeps.
const HELD_PICTURES_WARN: u32 = 1_000;

/// What passthrough has told the receiver about the source's video on this
/// connection. A local of `publish_loop`, so each connection starts afresh.
/// The per-connection state of the other edge#211 passthrough fixes belongs
/// here too.
///
/// The sequence header used to be built from the demuxer's cache, which holds
/// one set of each kind, the last seen, and is read after the whole datagram
/// has been demuxed: an HEVC source whose IRAPs carry PPS0-2 while its slices
/// name PPS0 and PPS1 got a header holding PPS2 alone, its coded frames were
/// stripped of their parameter sets, and no picture decoded at the receiver
/// (edge#211, the RTMP counterpart of edge#204).
#[derive(Default)]
struct PassthroughVideo {
    /// Every parameter set the source has sent, by id, fed one access unit at
    /// a time in decode order.
    collector: ParamSetCollector,
    /// The header last built.
    header: Option<SequenceHeader>,
    /// Rebuilt and not yet sent: it goes out ahead of the next picture sent.
    pending: bool,
    /// A header has gone out on this connection.
    any_sent: bool,
    /// A discontinuity restarted the table: the next complete one replaces the
    /// header.
    rebuild: bool,
    /// Pictures dropped because nothing describes their codec yet.
    held: u32,
}

/// One built sequence header: the codec and sets it describes, and the FLV
/// tag that carries them.
struct SequenceHeader {
    codec: CmafVideoCodec,
    sets: ParamSets,
    tag: Vec<u8>,
}

impl PassthroughVideo {
    /// Take in one access unit, in decode order, before anything is sent for
    /// it. True when the header was rebuilt: nothing described `codec`, the
    /// table restarted at a discontinuity, the table names an id the header
    /// lacks, or — on a key picture only — a set's bytes changed. Between key
    /// pictures a changed set reaches the decoder in-band; renewing on it
    /// would put a header ahead of every picture of a source that re-sends a
    /// varying PPS (CMAF's rule, edge#204).
    fn observe(&mut self, codec: CmafVideoCodec, nalus: &[Vec<u8>], is_keyframe: bool) -> bool {
        self.collector.observe(codec, nalus, is_keyframe);
        let Some(table) = self.collector.complete_sets(codec) else {
            return false;
        };
        let stale = match self.header.as_ref().filter(|h| h.codec == codec) {
            None => true,
            Some(_) if self.rebuild => true,
            Some(h) if is_keyframe => !h.sets.contains_all(table),
            Some(h) => !h.sets.has_every_id(table),
        };
        if stale {
            self.header = Some(SequenceHeader {
                codec,
                tag: sequence_header_tag(codec, table),
                sets: table.clone(),
            });
            self.pending = true;
            self.rebuild = false;
        }
        stale
    }

    /// A stream discontinuity: start the table again. The header stays, so
    /// pictures keep flowing under it until the new source's sets are
    /// complete; the header is then replaced even if it already holds them,
    /// dropping the old source's extra ids.
    fn restart(&mut self) {
        self.collector = ParamSetCollector::default();
        self.rebuild = true;
    }

    /// The header to send ahead of a picture that is about to go out, and how
    /// many copies: two on the connection's first key picture (a header, then
    /// the key-picture re-send, as always), one on any other key picture
    /// (carrying a renewal if one is pending) or ahead of the first picture
    /// after a renewal, else none. `None` while nothing describes `codec`.
    /// Call it only for a picture that will be sent, so a renewal is never
    /// spent on a dropped one.
    fn take_headers(&mut self, codec: CmafVideoCodec, is_keyframe: bool) -> Option<(&[u8], usize)> {
        let header = self.header.as_ref().filter(|h| h.codec == codec)?;
        let copies =
            usize::from(self.pending && (!is_keyframe || !self.any_sent)) + usize::from(is_keyframe);
        self.pending = false;
        self.any_sent |= copies > 0;
        Some((&header.tag, copies))
    }

    /// Count a picture dropped for want of a header, and at
    /// [`HELD_PICTURES_WARN`] say once which kinds of parameter set the source
    /// has not sent.
    fn hold(&mut self, codec: CmafVideoCodec, output_id: &str) {
        self.held = self.held.saturating_add(1);
        if self.held != HELD_PICTURES_WARN {
            return;
        }
        let none = ParamSets::default();
        let have = self.collector.sets(codec).unwrap_or(&none);
        let missing: Vec<&str> = [
            ("VPS", codec == CmafVideoCodec::H265 && have.vps.is_empty()),
            ("SPS", have.sps.is_empty()),
            ("PPS", have.pps.is_empty()),
        ]
        .into_iter()
        .filter_map(|(kind, absent)| absent.then_some(kind))
        .collect();
        tracing::warn!(
            "RTMP output '{output_id}': {HELD_PICTURES_WARN} {} pictures held back with no sequence \
             header to describe them; the source has sent no usable {}",
            codec_label(codec),
            missing.join(" / "),
        );
    }
}

/// How a log line names `codec`.
fn codec_label(codec: CmafVideoCodec) -> &'static str {
    match codec {
        CmafVideoCodec::H264 => "AVC",
        CmafVideoCodec::H265 => "HEVC",
    }
}

/// The FLV sequence header for `codec` carrying every one of `sets`: classic
/// AVC, or Enhanced-RTMP `SequenceStart` whose `hvcC` says sets may also
/// arrive in-band (they do).
fn sequence_header_tag(codec: CmafVideoCodec, sets: &ParamSets) -> Vec<u8> {
    let entries: usize = sets.vps.iter().chain(&sets.sps).chain(&sets.pps).map(|s| 3 + s.nal.len()).sum();
    let mut tag = Vec::with_capacity(64 + entries);
    match codec {
        CmafVideoCodec::H264 => {
            // keyframe (1) + AVC (7), AVC sequence header, composition time 0.
            tag.extend_from_slice(&[0x17, 0x00, 0x00, 0x00, 0x00]);
            push_avcc_record(&mut tag, &sets.sps, &sets.pps);
        }
        CmafVideoCodec::H265 => {
            tag.push(0x80 | (1 << 4)); // Ex | keyframe | PacketType=SequenceStart
            tag.extend_from_slice(&FOURCC_HVC1);
            push_hvcc_record(&mut tag, &sets.vps, &sets.sps, &sets.pps, false);
        }
    }
    tag
}

/// H.264 (classic FLV) or H.265 (Enhanced RTMP) passthrough — no re-encode.
///
/// Each picture goes out at its decode time `ts_ms` with its composition time
/// `cts_ms` ([`flv_video_time`]). A sequence header sent ahead of a picture
/// takes the picture's decode time and composition time 0, as FLV requires.
///
/// The sequence header carries every parameter set the source has sent, by
/// id ([`PassthroughVideo`]), and both codecs keep their parameter sets
/// in-band: the coded frames go out with every NAL the access unit carried,
/// and the HEVC `hvcC` says so (`array_completeness` 0), as FFmpeg's FLV
/// muxer writes it. HEVC used to go out with its parameter sets stripped and
/// a header holding the last one of each kind (edge#211).
#[allow(clippy::too_many_arguments)]
async fn passthrough_video(
    src: &VideoFrameSource<'_>,
    ts_ms: u32,
    cts_ms: u32,
    recv_time_us: u64,
    passthrough: &mut PassthroughVideo,
    client: &mut RtmpClient,
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
) -> anyhow::Result<bool> {
    // One flag drives the FLV frame type, the table's random-access rule and
    // the key-picture re-send.
    let is_keyframe = src.is_keyframe();
    let (codec, nalus) = match src {
        // Not carried without `video_encode` (the loop says so once).
        VideoFrameSource::Mpeg2 { .. } => return Ok(true),
        VideoFrameSource::H264 { nalus, .. } => (CmafVideoCodec::H264, *nalus),
        VideoFrameSource::H265 { nalus, .. } => (CmafVideoCodec::H265, *nalus),
    };
    if passthrough.observe(codec, nalus, is_keyframe)
        && let Some(h) = &passthrough.header
    {
        tracing::debug!(
            "RTMP output '{}': built {} sequence header ({} VPS / {} SPS / {} PPS)",
            config.id,
            codec_label(codec),
            h.sets.vps.len(),
            h.sets.sps.len(),
            h.sets.pps.len(),
        );
    }
    // An HEVC access unit with no picture in it (parameter sets, an AUD, SEI)
    // is not a CodedFrames tag. Its sets are in the table, and a renewal they
    // caused waits for the next picture.
    if codec == CmafVideoCodec::H265 && !nalus.iter().any(|n| h265_nal_type(n) < H265_NAL_VPS) {
        return Ok(true);
    }
    // The first header does not wait for a key picture. Real-world broadcast
    // streams often use open-GOP / recovery-point SEI signalling instead of a
    // true IDR (NAL type 5), so waiting for `is_keyframe` would never fire and
    // video would never start. Receivers gracefully handle the first picture
    // being non-IDR — they treat it as "wait for the next IDR" — but the
    // sequence header is what they need to initialise the decoder. A picture
    // nothing describes yet is dropped.
    let Some((header, copies)) = passthrough.take_headers(codec, is_keyframe) else {
        passthrough.hold(codec, &config.id);
        return Ok(true);
    };
    for _ in 0..copies {
        client.send_video(header, ts_ms).await?;
    }
    let tag = match codec {
        CmafVideoCodec::H264 => build_avc_nalu_tag(nalus, is_keyframe, cts_ms),
        CmafVideoCodec::H265 => build_hevc_coded_frames_tag(&to_length_prefixed(nalus), is_keyframe, cts_ms),
    };
    let tag_len = tag.len();
    client.send_video(&tag, ts_ms).await?;
    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
    stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
    stats.record_latency(recv_time_us);
    Ok(true)
}

#[cfg(feature = "media-codecs")]
fn open_video_active(
    cfg: &VideoEncodeConfig,
    source_codec: video_codec::VideoCodec,
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
    // timestamp, and the re-encode path has none to give it: the encoder hands
    // back no decode times, so its tag writers write composition time 0 (HEVC
    // as CodedFramesX, with no field at all). An encoder that reordered would
    // stamp display-order PTS as DTS and drive it backwards — which most
    // ingests answer by dropping the publisher. It is also the precondition
    // for the source-PTS FIFO below being a plain queue. (Passthrough is
    // unaffected: it carries the source's DTS and composition times,
    // edge#205.) `webrtc_safe_video_encode` pins the same knob for the same
    // class of reason.
    let mut cfg = cfg.clone();
    if cfg.bframes.unwrap_or(0) != 0 {
        tracing::warn!(
            error_code = "rtmp_bframes_unsupported",
            requested = cfg.bframes.unwrap_or(0),
            "RTMP output '{}': video_encode.bframes is not supported on the RTMP path \
             (the re-encode path writes no FLV composition-time offset) — encoding with bframes = 0",
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
        stamper: Default::default(),
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
    let annex_b = src.decoder_input();
    let block_result = crate::timed_block_in_place!(
        "output_rtmp.video_encoder",
        crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
        { transcode_access_unit(active, &annex_b, pts_90k, src.pts_known(), &config.id) }
    );
    // An explicit `scan: interlaced` the encoder could not open for: the
    // pipeline logged it; tell the manager too, once per open.
    if let Some(why) = active.pipeline.take_interlace_notice() {
        crate::engine::video_encode_util::emit_output_interlace_unavailable(
            event_sender,
            "RTMP",
            &config.id,
            &why,
            src.codec(),
        );
    }

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
            // Composition time 0: the encoder runs without B-frames.
            video_codec::VideoCodec::Hevc => build_hevc_coded_frames_tag(&avcc, keyframe, 0),
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
    pts_known: bool,
    output_id: &str,
) -> Result<Vec<(Vec<u8>, bool, i64)>, String> {
    active.stats.input_frames.fetch_add(1, Ordering::Relaxed);
    // Feed the source PTS in so libavcodec can echo it back per
    // decoded frame. `send_packet` (no pts) threw it away at the door,
    // which is why the emit path had nothing but a frame counter to
    // work from. An access unit whose PES carried none goes in without one
    // (fed as 0 it came back stamped 0): its picture is counted towards
    // the rate and stamped from the pictures around it.
    let fed = if pts_known {
        active.decoder.send_packet_with_pts(annex_b, pts_90k as i64)
    } else {
        active.decoder.send_packet(annex_b)
    };
    if let Err(e) = fed {
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
        let admitted = active.rate.admit(frame.pts(), &mut active.pipeline);
        // Display-order source PTS for this frame, straight from the
        // decoder's reorder queue — or, for a picture whose PES carried
        // none, from the pictures before it (`FramePtsStamper`).
        let interval = crate::engine::video_encode_util::frame_interval_90k(active.pipeline.fps());
        let stamped = active.stamper.stamp(frame.pts(), interval);
        if !admitted {
            continue;
        }
        let src_pts_for_frame = stamped.unwrap_or(pts_90k);
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

/// The `(sample_rate_index, channel_config)` the FLV AAC sequence header
/// announces: the re-encoder's output format when audio is re-encoded, the
/// source's ADTS config when it passes through — and nothing while the
/// re-encoder is still waiting for a frame that decodes (`Lazy`): its format
/// is the decoder's, which for HE-AAC is not the header's (24 kHz / mono
/// core), and a header sent from the ADTS config then would announce the
/// wrong rate and channels for the whole connection.
fn aac_sequence_header_format(state: &EncoderState, cached: (u8, u8, u8)) -> Option<(u8, u8)> {
    let (_, sr_idx, ch_cfg) = cached;
    match state {
        EncoderState::Active { encoder, .. } => {
            let p = encoder.params();
            Some((sr_index_from_hz(p.target_sample_rate).unwrap_or(sr_idx), p.target_channels))
        }
        EncoderState::Lazy => None,
        _ => Some((sr_idx, ch_cfg)),
    }
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
    let decoded_as = (decoder.codec_name(), decoder.sample_rate(), decoder.channels());
    build_encoder_for_format(
        config,
        input_sr,
        input_ch,
        Some(decoder),
        decoded_as,
        cancel,
        stats,
        flow_id,
        event_sender,
    )
}

/// Decode one MP2 / AC-3 / E-AC-3 PES (`stream_type`) for the AAC
/// re-encode: with the output's libavcodec decoder (reopened when the codec
/// changes), and — while the encoder is still `Lazy` — building it from the
/// format the source decodes to ([`build_encoder_for_format`]). It used to
/// be built only from the demuxer's AAC config, which such a source never
/// has, so its audio was dropped for good unless `silent_fallback` had
/// built the encoder eagerly. Returns the decoded frames (none when the
/// encoder has failed or the PES decoded to nothing).
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn decode_other_audio_for_encode(
    encoder_state: &mut EncoderState,
    decoder: &mut Option<video_engine::AudioDecoder>,
    decoder_codec: &mut Option<video_codec::AudioDecoderCodec>,
    stream_type: u8,
    data: &[u8],
    pts: u64,
    config: &RtmpOutputConfig,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    event_sender: &EventSender,
) -> Vec<video_engine::DecodedAudioFrame> {
    if matches!(encoder_state, EncoderState::Failed | EncoderState::Disabled) {
        return Vec::new();
    }
    let Some(codec) = crate::engine::audio_decode::ff_codec_for_stream_type(stream_type) else {
        return Vec::new();
    };
    if *decoder_codec != Some(codec) {
        *decoder = crate::engine::audio_decode::open_ff_decoder(codec).ok();
        *decoder_codec = Some(codec);
    }
    let Some(dec) = decoder.as_mut() else {
        return Vec::new();
    };
    let mut decoded = Vec::new();
    for au in crate::engine::audio_decode::split_audio_codec_frames(data, codec) {
        if dec.send_packet(au, pts as i64).is_err() {
            continue;
        }
        while let Ok(frame) = dec.receive_frame() {
            decoded.push(frame);
        }
    }
    if matches!(encoder_state, EncoderState::Lazy)
        && let Some(first) = decoded.first()
    {
        let channels = first.planar.len() as u8;
        *encoder_state = build_encoder_for_format(
            config,
            first.sample_rate,
            channels,
            None,
            (crate::engine::audio_decode::ff_codec_name(codec), first.sample_rate, channels),
            cancel,
            stats,
            flow_id,
            event_sender,
        );
    }
    decoded
}

/// The encoder `audio_encode` asks for, opened for a source that decodes to
/// `input_sr` × `input_ch`: the channel / rate stage in front of it follows
/// the rule every re-encoding output shares (`audio_transcode::EncoderStage`
/// — the `transcode` block with `audio_encode`'s fields folded in, or those
/// two alone when they differ from the source). `decoder` is the AAC
/// source's decoder; an MP2 / AC-3 / E-AC-3 source brings its own
/// libavcodec decoder and passes `None`. `decoded_as` labels the decode
/// stats (codec name, rate, channels).
#[allow(clippy::too_many_arguments)]
fn build_encoder_for_format(
    config: &RtmpOutputConfig,
    input_sr: u32,
    input_ch: u8,
    decoder: Option<AacDecoder>,
    decoded_as: (&str, u32, u8),
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
            config.id,
            enc_cfg.codec
        );
        return EncoderState::Failed;
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
    stats.set_decode_stats(decode_stats.clone(), decoded_as.0, decoded_as.1, decoded_as.2);
    stats.set_encode_stats(
        encoder.stats_handle(),
        encoder.params().codec.as_str().to_string(),
        encoder.params().target_sample_rate,
        encoder.params().target_channels,
        encoder.params().target_bitrate_kbps,
    );

    EncoderState::Active {
        decoder,
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
        let mut state = open_video_active(&cfg, video_codec::VideoCodec::H264, &aus[0].0, &config, &stats, &events);
        let mut encoded = 0;
        for (au, pts) in &aus {
            let VideoEncoderState::Active(active) = &mut state else { panic!("not active") };
            encoded += transcode_access_unit(active, au, *pts, true, "o").unwrap().len();
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

    /// A source stamping only every 12th picture: the access units without
    /// a PTS go to the decoder without one, the rate is measured over their
    /// pictures (25 fps, not the 30/1 fallback after 60 dropped frames), and
    /// each picture is queued on its own time — the last stamped one plus a
    /// frame per picture since — where it used to be queued at 0.
    #[test]
    fn a_sparse_pts_source_is_measured_and_stamped_on_rtmp() {
        let cfg: VideoEncodeConfig =
            serde_json::from_value(serde_json::json!({"codec": "x264", "preset": "veryfast"})).unwrap();
        let config: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k"
        }))
        .unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "rtmp".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let aus = x264_test_source(80, 3_600);
        let mut state = open_video_active(&cfg, video_codec::VideoCodec::H264, &aus[0].0, &config, &stats, &events);
        for (k, (au, pts)) in aus.iter().enumerate() {
            let VideoEncoderState::Active(active) = &mut state else { panic!("not active") };
            let known = k % 12 == 0;
            transcode_access_unit(active, au, if known { *pts } else { 0 }, known, "o").unwrap();
        }
        let VideoEncoderState::Active(active) = &state else { panic!("not active") };
        assert_eq!(active.pipeline.fps(), (25, 1));
        let queued: Vec<u64> = active.src_pts_queue.iter().copied().collect();
        assert!(!queued.is_empty());
        assert!(queued.windows(2).all(|w| w[1] == w[0] + 3_600), "{queued:?}");
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

    /// edge#205 on the re-encode path: a B-frame source re-encoded for RTMP.
    /// The epoch now anchors on the first picture's decode time rather than
    /// its PTS — a constant shift of the connection's whole timeline, so the
    /// first re-encoded picture goes out at its source PTS less that decode
    /// time (it went out at 0). The re-encoded tags keep composition time 0
    /// (the encoder runs without B-frames) and stay strictly increasing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_b_frame_source_re_encoded_for_rtmp_keeps_composition_time_zero() {
        use super::flv_wire_tests::{publish, tag_cts};
        use crate::engine::rtmp::ts_mux::TsMuxer;
        use video_codec::{VideoEncoderCodec, VideoEncoderConfig, VideoPreset};
        let (w, h, n) = (320usize, 240usize, 40usize);
        let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            gop_size: 25,
            max_b_frames: 2,
            // `zerolatency` would turn the B-frames off.
            tune: String::new(),
            preset: VideoPreset::Veryfast,
            global_header: false,
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut aus = Vec::new();
        for i in 0..n {
            let y: Vec<u8> = (0..w * h)
                .map(|k| (((k % w + i) * 7919 + (k / w) * 104_729) % 181) as u8 + 30)
                .collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        assert!(aus.windows(2).any(|a| a[1].pts < a[0].pts), "the source reorders");
        let at = |frames: i64| 900_000 + (frames + 10) as u64 * 3_600;
        let (first_dts, first_pts) = (at(aus[0].dts), at(aus.iter().map(|a| a.pts).min().unwrap()));
        assert!(first_dts < first_pts, "the source decodes ahead of presenting");
        let mut mux = TsMuxer::new();
        let mut ts = Vec::new();
        for au in &aus {
            ts.extend(mux.mux_video(&au.data, at(au.pts), at(au.dts), au.keyframe));
        }
        let end = at(n as i64 + 10);
        ts.extend(mux.mux_video(&aus[n - 1].data, end, end, false));
        let tags = publish(
            ts,
            serde_json::json!({
                "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k",
                "video_encode": { "codec": "x264", "preset": "veryfast", "fps_num": 25, "fps_den": 1 }
            }),
        )
        .await;
        let pictures: Vec<(u32, u32)> =
            tags.iter().filter(|t| t.video).filter_map(|t| Some((t.ts, tag_cts(&t.data)?))).collect();
        assert!(pictures.len() >= n - 10, "{} pictures", pictures.len());
        assert!(pictures.iter().all(|&(_, cts)| cts == 0), "composition time 0: {pictures:?}");
        assert!(pictures.windows(2).all(|p| p[1].0 > p[0].0), "{pictures:?}");
        assert_eq!(pictures[0].0, ((first_pts - first_dts) / 90) as u32, "{pictures:?}");
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

    // ── edge#205: decode time + composition time ──────────────────────────

    /// A source's first decode time, well clear of 0 and of the 33-bit wrap.
    const T0: u64 = 900_000;

    /// `(pts, dts)` of IBBP pictures in decode order on the frame grid `g`
    /// (`g(k)` = the k-th frame instant): each I / P presents three frames
    /// after it decodes and carries its DTS; each B presents as it decodes,
    /// so its PES carries none — the `DemuxedFrame::dts` contract.
    pub(super) fn ibbp(n: u64, g: impl Fn(u64) -> u64) -> Vec<(u64, Option<u64>)> {
        (0..n)
            .map(|j| {
                let shown = if j % 3 == 0 { j + 2 } else { j - 1 };
                let (pts, dts) = (g(shown + 1), g(j));
                (pts, (dts != pts).then_some(dts))
            })
            .collect()
    }

    /// I B B B P with the middle B a reference (b-pyramid), then P B B B, in
    /// decode order at 25 fps: the reference B carries its own DTS, and so
    /// does one of the non-reference Bs (it presents a frame after it
    /// decodes); the other presents as it decodes.
    pub(super) fn b_pyramid() -> Vec<(u64, Option<u64>)> {
        [(2, 0), (6, 1), (4, 2), (3, 3), (5, 4), (10, 5), (8, 6), (7, 7), (9, 8)]
            .map(|(p, d): (u64, u64)| (T0 + p * STEP_25FPS, (p != d).then_some(T0 + d * STEP_25FPS)))
            .to_vec()
    }

    /// The FLV ms a source instant maps to on an epoch at `base` — what
    /// audio at that instant is stamped (`pts_to_ms`).
    fn ms(t: u64, base: u64) -> u32 {
        ((t - base) / 90) as u32
    }

    /// Run `pics` through `flv_video_time` on a fresh connection and check
    /// FLV's contract against them: tags strictly in decode order from 0,
    /// each picture presented (timestamp + composition time) at exactly the
    /// ms its PTS maps to, and a composition time on exactly the pictures
    /// presenting later than they decode.
    fn assert_decode_order(pics: &[(u64, Option<u64>)]) {
        let mut base = None;
        let first_dts = pics[0].1.unwrap_or(pics[0].0);
        let mut last: Option<u32> = None;
        for (k, &(pts, dts)) in pics.iter().enumerate() {
            let (ts, cts) = flv_video_time(pts, dts, &mut base);
            assert_eq!(base, Some(first_dts), "the first decode time anchors the epoch");
            assert_eq!(ts, ms(dts.unwrap_or(pts), first_dts), "picture {k}: tagged at its decode time");
            assert_eq!(ts + cts, ms(pts, first_dts), "picture {k}: presented at its PTS");
            assert_eq!(cts > 0, ms(pts, first_dts) > ms(dts.unwrap_or(pts), first_dts), "picture {k}");
            assert!(last.is_none_or(|l| ts > l), "picture {k}: ts {ts} after {last:?}");
            last = Some(ts);
        }
        assert_eq!(flv_video_time(pics[0].0, pics[0].1, &mut None).0, 0, "the first tag is at 0");
    }

    /// edge#205: on a B-frame source each picture was tagged with its PTS
    /// and composition time 0, so the tags stepped back at every reordered
    /// picture — and, the epoch anchored on the first PTS, the B-pictures
    /// leading it were all stamped 0.
    #[test]
    fn a_b_frame_gop_is_tagged_in_decode_order() {
        let pics = ibbp(12, |k| T0 + k * STEP_25FPS);
        assert_eq!(pics[..4], [
            (T0 + 3 * STEP_25FPS, Some(T0)),
            (T0 + STEP_25FPS, None),
            (T0 + 2 * STEP_25FPS, None),
            (T0 + 6 * STEP_25FPS, Some(T0 + 3 * STEP_25FPS)),
        ]);
        assert_decode_order(&pics);
        let mut base = None;
        let got: Vec<(u32, u32)> = pics[..4].iter().map(|&(p, d)| flv_video_time(p, d, &mut base)).collect();
        assert_eq!(got, [(0, 120), (40, 0), (80, 0), (120, 120)]);
        assert_decode_order(&b_pyramid());
    }

    /// 29.97 fps (3003-tick frames) and 59.94 fps (alternately 1501 and
    /// 1502 ticks): the frame lands on a fraction of a ms, and the tags
    /// still go out strictly increasing, each picture presented on the ms
    /// grid audio is mapped to.
    #[test]
    fn a_b_frame_source_at_a_fractional_rate_stays_in_decode_order() {
        assert_decode_order(&ibbp(60, |k| T0 + k * 3003));
        assert_decode_order(&ibbp(120, |k| T0 + k * 1501 + k / 2));
    }

    /// No DTS (a source without B-frames), or one equal to the PTS: the
    /// timestamp is the PTS mapping it always was and the composition time
    /// 0 — byte for byte what went out before edge#205. So is a picture
    /// whose PES carried no PTS at all (`pts` 0, no DTS).
    #[test]
    fn a_source_without_b_frames_is_tagged_exactly_as_before() {
        for with_dts in [false, true] {
            let (mut base, mut before) = (None, None);
            for k in 0..50u64 {
                let pts = T0 + k * 3003;
                let got = flv_video_time(pts, with_dts.then_some(pts), &mut base);
                assert_eq!(got, (pts_to_ms(pts, &mut before), 0), "frame {k}");
            }
            assert_eq!(base, before);
        }
        assert_eq!(flv_video_time(0, None, &mut None), (0, 0));
        let mut base = Some(T0);
        assert_eq!(flv_video_time(0, None, &mut base), (rel_ms_wrapping(0, T0), 0));
        assert_eq!(base, Some(T0));
    }

    /// A DTS the offset cannot be — after the PTS, or more than the 2 s cap
    /// before it — is judged as CMAF judges it: as none.
    #[test]
    fn a_dts_that_cannot_be_an_offset_is_ignored() {
        let pts = T0 + 180_001;
        for dts in [pts + STEP_25FPS, pts - 180_001, pts - 900_000] {
            let mut base = None;
            assert_eq!(flv_video_time(pts, Some(dts), &mut base), (0, 0), "dts {dts}");
            assert_eq!(base, Some(pts), "anchored on the PTS, as before");
            assert_eq!(flv_video_time(pts + 3600, Some(dts + 3600), &mut base), (40, 0));
        }
        // 2 s exactly is an offset.
        let mut base = None;
        assert_eq!(flv_video_time(pts, Some(pts - 180_000), &mut base), (0, 2000));
    }

    /// The other essence anchored the epoch first (an AAC frame is handed
    /// out the moment it is complete; a video PES only at the next PUSI on
    /// its PID). A picture decoding before that epoch is tagged 0 — the tag
    /// stream never steps back — and its composition time carries the rest,
    /// so it is still presented at the ms audio at its PTS is.
    #[test]
    fn video_behind_an_audio_anchored_epoch_still_presents_at_its_pts() {
        let a0 = T0 + 2 * STEP_25FPS;
        let mut base = Some(a0);
        let pics = ibbp(6, |k| T0 + k * STEP_25FPS);
        let got: Vec<(u32, u32)> = pics.iter().map(|&(p, d)| flv_video_time(p, d, &mut base)).collect();
        // I (decodes before a0, presents a frame after it), then a B before
        // the epoch (clamped, as it was), a B on it, and the rest as usual.
        assert_eq!(got, [(0, 40), (0, 0), (0, 0), (40, 120), (80, 0), (120, 0)]);
        for (&(pts, _), (ts, cts)) in pics.iter().zip(&got) {
            if pts >= a0 {
                assert_eq!(ts + cts, pts_to_ms(pts, &mut Some(a0)));
            }
        }
        assert_eq!(base, Some(a0), "video does not move an epoch audio set");
    }

    /// Anchored on the first picture's decode time, audio at that picture's
    /// PTS maps to the ms the picture is presented at: the shared-epoch A/V
    /// property, unchanged by moving the tag to the decode time.
    #[test]
    fn the_first_picture_anchors_the_epoch_on_its_decode_time() {
        let (pts, dts) = (T0 + 7_200, T0);
        let mut base = None;
        let (ts, cts) = flv_video_time(pts, Some(dts), &mut base);
        assert_eq!(base, Some(dts));
        assert_eq!((ts, cts), (0, 80));
        assert_eq!(pts_to_ms(pts, &mut base), ts + cts, "audio at the same instant");
        assert_eq!(pts_to_ms(pts + 1920, &mut base), 101);
    }

    /// The composition time is `pts_ms − dts_ms`, not the offset rounded on
    /// its own, so an offset under a ms can come out 0 or 1 depending on
    /// where the two fall — either way the picture presents on audio's ms.
    /// At 0 an HEVC picture keeps `CodedFramesX`.
    #[test]
    fn an_offset_under_a_millisecond_rounds_with_the_presentation_time() {
        let mut base = Some(0);
        let (ts, cts) = flv_video_time(170, Some(120), &mut base);
        assert_eq!((ts, cts), (1, 0), "120 and 170 ticks are both in ms 1");
        assert_eq!(build_hevc_coded_frames_tag(&[1, 2], false, cts)[0], 0xA3, "CodedFramesX");
        let (ts, cts) = flv_video_time(100, Some(50), &mut base);
        assert_eq!((ts, cts), (0, 1), "50 ticks in ms 0, 100 in ms 1");
        assert_eq!(build_hevc_coded_frames_tag(&[1, 2], false, cts)[0], 0xA1, "CodedFrames");
    }

    /// The decode time saturates rather than wrapping. A connection whose
    /// first picture presents just past the 33-bit wrap and decodes just
    /// before it (PTS below its offset) would, wrapped, anchor the epoch
    /// near 2^33 — and every later tag, audio and video, would then read as
    /// a wrap and go out at 0 for up to ~26.5 h. The same holds behind an
    /// epoch audio anchored just past the wrap: wrapped, the first picture
    /// would be tagged ~26.5 h ahead and the next one step back.
    #[test]
    fn a_picture_straddling_the_33_bit_wrap_does_not_anchor_near_it() {
        const WRAP: u64 = 1 << 33;
        let mut base = None;
        assert_eq!(flv_video_time(1_000, Some(WRAP - 2_000), &mut base), (0, 11));
        assert_eq!(base, Some(0));
        assert_eq!(flv_video_time(4_600, Some(1_600), &mut base), (17, 34));
        assert_eq!(pts_to_ms(50_000, &mut base), 555, "audio after it is not pinned to 0");

        let mut base = Some(1_000);
        assert_eq!(flv_video_time(3_000, Some(WRAP - 500), &mut base), (0, 22));
        assert_eq!(flv_video_time(6_600, Some(3_100), &mut base), (23, 39));
    }
}

/// edge#205 end to end: MPEG-TS through `publish_loop` — the demuxer, the
/// per-arm `flv_video_time`, `process_video_frame`, `passthrough_video`, the
/// tag writers and the chunk writer — into the edge's own RTMP server, which
/// hands back each tag with the timestamp it read off the wire.
#[cfg(test)]
pub(super) mod flv_wire_tests {
    use super::flv_timestamp_tests::{b_pyramid, ibbp};
    use super::*;
    use crate::engine::cmaf::param_sets::tests::{h264_pps, h770, hevc_pps, hx};
    use crate::engine::rtmp::server::{RtmpMediaMessage, RtmpServerConfig, run_rtmp_server};
    use crate::engine::rtmp::ts_mux::TsMuxer;
    use bytes::Bytes;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    const F: u64 = 3_600; // one frame at 25 fps
    const T0: u64 = 900_000;

    /// One tag as the server read it off the wire.
    pub(super) struct WireTag {
        pub video: bool,
        pub ts: u32,
        pub data: Bytes,
    }

    /// Publish `ts` (one item per broadcast packet) through `publish_loop`
    /// with the RTMP output `output` into a loopback server, and return what
    /// the server read, in order.
    pub(super) async fn publish(ts: Vec<Bytes>, output: serde_json::Value) -> Vec<WireTag> {
        let cancel = CancellationToken::new();
        let (media_tx, mut media_rx) = tokio::sync::mpsc::channel(4_096);
        // A free port, found by binding one and letting it go; another
        // process can take it in between, so a server that fails to bind
        // is retried on a fresh one.
        let mut client = 'connected: {
            for _ in 0..20 {
                let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
                let server = tokio::spawn(run_rtmp_server(
                    RtmpServerConfig {
                        listen_addr: format!("127.0.0.1:{port}"),
                        expected_app: "live".into(),
                        expected_stream_key: Some("k".into()),
                    },
                    media_tx.clone(),
                    Arc::new(AtomicBool::new(false)),
                    cancel.child_token(),
                ));
                for _ in 0..200 {
                    if server.is_finished() {
                        break;
                    }
                    // Bounded: until the server's bind fails, whatever took
                    // the port may answer and never complete a handshake.
                    let url = format!("rtmp://127.0.0.1:{port}/live");
                    if let Ok(Ok(c)) = tokio::time::timeout(Duration::from_secs(2), RtmpClient::connect(&url, "k")).await {
                        break 'connected c;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                server.abort();
            }
            panic!("no loopback RTMP server came up");
        };
        drop(media_tx);
        let config: RtmpOutputConfig = serde_json::from_value(output).unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "rtmp".into()));
        let (events, _events_rx) = crate::manager::events::event_channel();
        let (tx, mut rx) = broadcast::channel(ts.len().max(1));
        for (k, data) in ts.into_iter().enumerate() {
            tx.send(RtpPacket {
                data,
                sequence_number: k as u16,
                rtp_timestamp: 0,
                recv_time_us: 0,
                is_raw_ts: true,
                upstream_seq: None,
                upstream_leg_id: None,
                sender_timestamp_us: None,
            })
            .unwrap();
        }
        drop(tx);
        // Returns once the channel is drained and closed.
        publish_loop(&config, &mut client, &mut rx, &stats, &CancellationToken::new(), false, "f", &events)
            .await
            .expect("publish_loop");
        client.close().await.unwrap();
        let mut tags = Vec::new();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(20), media_rx.recv())
                .await
                .expect("the server hands back every tag, then the unpublish");
            match msg {
                Some(RtmpMediaMessage::Video { data, timestamp_ms }) => {
                    tags.push(WireTag { video: true, ts: timestamp_ms, data })
                }
                Some(RtmpMediaMessage::Audio { data, timestamp_ms }) => {
                    tags.push(WireTag { video: false, ts: timestamp_ms, data })
                }
                Some(RtmpMediaMessage::Metadata { .. }) => {}
                Some(RtmpMediaMessage::Disconnected) | None => break,
            }
        }
        cancel.cancel();
        tags
    }

    /// A plain RTMP output: no `video_encode`, so video passes through.
    fn passthrough_output() -> serde_json::Value {
        serde_json::json!({ "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k" })
    }

    /// The composition time a video tag carries — `None` for a sequence
    /// header. Classic AVC: SI24 at bytes 2..5 of every NALU tag. Enhanced
    /// RTMP: SI24 after the FourCC on `CodedFrames` (1), none on
    /// `CodedFramesX` (3).
    pub(super) fn tag_cts(tag: &[u8]) -> Option<u32> {
        let si24 = |b: &[u8]| {
            let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            assert_eq!(v & 0x80_0000, 0, "a negative composition time");
            v
        };
        if tag[0] & 0x80 != 0 {
            match tag[0] & 0x0F {
                0 => None,
                1 => Some(si24(&tag[5..8])),
                3 => Some(0),
                t => panic!("Enhanced-RTMP PacketType {t}"),
            }
        } else {
            match tag[1] {
                0 => {
                    assert_eq!(&tag[2..5], &[0, 0, 0], "a sequence header's composition time is 0");
                    None
                }
                1 => Some(si24(&tag[2..5])),
                t => panic!("AVCPacketType {t}"),
            }
        }
    }

    /// An H.264 access unit, Annex B: AUD, then SPS + PPS + IDR for a key
    /// picture, a non-IDR slice otherwise. The bytes are only ever passed
    /// through, never decoded.
    fn h264_au(key: bool, k: u8) -> Vec<u8> {
        if key {
            annex_b(&[&H264_AUD, &H264_SPS, &H264_PPS0, &h264_idr(k)])
        } else {
            annex_b(&[&H264_AUD, &h264_slice(k)])
        }
    }

    /// The HEVC counterpart: AUD, then VPS + SPS + PPS + IDR_W_RADL, or a
    /// TRAIL_R slice. The parameter sets are 770_H's VPS, SPS and PPS0: real
    /// ones, which the sequence header is built from.
    fn h265_au(key: bool, k: u8) -> Vec<u8> {
        let (vps, sps, pps) = h770();
        if key {
            annex_b(&[&HEVC_AUD, &vps, &sps, &pps[0], &hevc_idr(k)])
        } else {
            annex_b(&[&HEVC_AUD, &hevc_trail(k)])
        }
    }

    const H264_AUD: [u8; 2] = [0x09, 0xF0];
    const H264_SPS: [u8; 5] = [0x67, 0x42, 0xC0, 0x1E, 0xDA];
    const H264_PPS0: [u8; 4] = [0x68, 0xCE, 0x3C, 0x80];
    const HEVC_AUD: [u8; 3] = [0x46, 0x01, 0x50];

    fn h264_idr(k: u8) -> [u8; 4] {
        [0x65, 0x88, 0x84, k + 1]
    }
    fn h264_slice(k: u8) -> [u8; 4] {
        [0x41, 0x9A, 0x22, k + 1]
    }
    fn hevc_idr(k: u8) -> [u8; 4] {
        [0x26, 0x01, 0xAF, k + 1]
    }
    fn hevc_trail(k: u8) -> [u8; 4] {
        [0x02, 0x01, 0xD0, k + 1]
    }

    /// NAL units as an Annex B access unit, four-byte start codes.
    pub(super) fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut au = Vec::new();
        for n in nals {
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(n);
        }
        au
    }

    /// The parameter sets a sequence header tag carries, read back from its
    /// `avcC` / `hvcC`: `(VPS, SPS, PPS)`, each in the record's order.
    pub(super) fn header_sets(tag: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let record = &tag[5..];
        let mut at;
        let read = |at: &mut usize| {
            let len = u16::from_be_bytes([record[*at], record[*at + 1]]) as usize;
            let nal = record[*at + 2..*at + 2 + len].to_vec();
            *at += 2 + len;
            nal
        };
        let (mut vps, mut sps, mut pps) = (Vec::new(), Vec::new(), Vec::new());
        if tag[0] & 0x80 == 0 {
            assert_eq!(&tag[..2], &[0x17, 0x00], "an AVC sequence header");
            at = 6;
            for _ in 0..record[5] & 0x1F {
                sps.push(read(&mut at));
            }
            let n = record[at];
            at += 1;
            for _ in 0..n {
                pps.push(read(&mut at));
            }
        } else {
            assert_eq!(tag[0], 0x90, "an Enhanced-RTMP SequenceStart");
            at = 23;
            for _ in 0..record[22] {
                let nal_type = record[at] & 0x3F;
                let n = u16::from_be_bytes([record[at + 1], record[at + 2]]);
                at += 3;
                for _ in 0..n {
                    let nal = read(&mut at);
                    match nal_type {
                        32 => vps.push(nal),
                        33 => sps.push(nal),
                        _ => pps.push(nal),
                    }
                }
            }
        }
        assert_eq!(at, record.len(), "nothing after the arrays");
        (vps, sps, pps)
    }

    /// The same TS published three ways: one item per packet, seven packets
    /// to an item as the inputs publish (`packet::bundle_ts_packets`), and
    /// the whole stream as one item — one `demux` call, so an output that
    /// read parameter sets per datagram rather than per access unit sees the
    /// last of them ahead of the first picture.
    fn as_published(ts: Vec<Bytes>) -> [(&'static str, Vec<Bytes>); 3] {
        let whole = Bytes::from(ts.iter().flat_map(|p| p.iter().copied()).collect::<Vec<u8>>());
        let bundled = crate::engine::packet::bundle_ts_packets(ts.clone());
        [("per packet", ts), ("seven to an item", bundled), ("one item", vec![whole])]
    }

    /// `pics` (`(pts, dts)` in decode order; key pictures at the indices in
    /// `keys`, the first among them) muxed as `TsMuxer` muxes any source —
    /// the PES carries the DTS only where it differs from the PTS — plus a
    /// closing picture, since the demuxer hands a video PES out only at the
    /// next one on its PID.
    fn video_ts(mux: &mut TsMuxer, hevc: bool, pics: &[(u64, Option<u64>)], keys: &[usize]) -> Vec<Bytes> {
        let au = |key: bool, k: usize| if hevc { h265_au(key, k as u8) } else { h264_au(key, k as u8) };
        video_ts_with(mux, hevc, pics, keys, &au)
    }

    /// [`video_ts`] with the access units `au(key, k)` builds (Annex B).
    fn video_ts_with(
        mux: &mut TsMuxer,
        hevc: bool,
        pics: &[(u64, Option<u64>)],
        keys: &[usize],
        au: &dyn Fn(bool, usize) -> Vec<u8>,
    ) -> Vec<Bytes> {
        if hevc {
            mux.set_video_stream_type(0x24);
        }
        let mut out = Vec::new();
        for (k, &(pts, dts)) in pics.iter().enumerate() {
            let key = keys.contains(&k);
            out.extend(mux.mux_video(&au(key, k), pts, dts.unwrap_or(pts), key));
        }
        let end = pics.iter().map(|p| p.0).max().unwrap() + F;
        out.extend(mux.mux_video(&au(false, pics.len()), end, end, false));
        out
    }

    /// The video tags of `tags` against the source pictures `pics` on an
    /// epoch at `base`: every picture arrives once, in decode order, tagged
    /// at its decode time (0 when that is before the epoch) and presented
    /// (timestamp + composition time) on the ms its PTS maps to; every
    /// sequence header goes out at the decode time of the key picture it
    /// precedes, with composition time 0; the video timeline never steps
    /// back. Returns how many pictures carried a composition time.
    fn assert_wire_decode_order(tags: &[WireTag], pics: &[(u64, Option<u64>)], base: u64, hevc: bool) -> usize {
        let ms = |t: u64| if t < base { 0 } else { ((t - base) / 90) as u32 };
        let video: Vec<&WireTag> = tags.iter().filter(|t| t.video).collect();
        assert!(video.windows(2).all(|w| w[0].ts <= w[1].ts), "the video timeline steps back");
        let mut pictures = Vec::new();
        for (i, tag) in video.iter().enumerate() {
            assert_eq!(tag.data[0] & 0x80 != 0, hevc, "classic FLV for H.264, Enhanced RTMP for HEVC");
            match tag_cts(&tag.data) {
                Some(cts) => pictures.push((tag.ts, cts, tag.data[0])),
                None => {
                    let picture = video[i + 1..]
                        .iter()
                        .find(|t| tag_cts(&t.data).is_some())
                        .expect("a header ahead of a picture");
                    assert_eq!(tag.ts, picture.ts, "a header takes its picture's decode time");
                }
            }
        }
        assert_eq!(pictures.len(), pics.len(), "every picture once");
        let mut with_cts = 0;
        for (k, (&(pts, dts), &(ts, cts, head))) in pics.iter().zip(&pictures).enumerate() {
            assert_eq!(ts, ms(dts.unwrap_or(pts)), "picture {k}: tagged at its decode time");
            assert_eq!(ts + cts, ms(pts), "picture {k}: presented at its PTS");
            if hevc {
                assert_eq!(head & 0x0F, if cts == 0 { 3 } else { 1 }, "picture {k}: PacketType");
            }
            with_cts += usize::from(cts > 0);
        }
        with_cts
    }

    /// edge#205, the reported failure: a B-frame source passed through to
    /// RTMP — H.264 and HEVC, IBBP and b-pyramid. Every picture was tagged
    /// with its PTS and composition time 0, so the tags went out in
    /// presentation order and stepped back at every reordered picture
    /// (the epoch, anchored on the first PTS, also stamped the B-pictures
    /// leading it 0); HEVC went out as `CodedFramesX` whatever the picture.
    ///
    /// A second key picture mid-stream re-sends the sequence header: at that
    /// picture's decode time, never ahead of the tag after it.
    #[tokio::test]
    async fn a_b_frame_ts_source_reaches_flv_in_decode_order() {
        for hevc in [false, true] {
            for (pics, keys) in [(ibbp(12, |k| T0 + k * F), [0, 6]), (b_pyramid(), [0, 5])] {
                let ts = video_ts(&mut TsMuxer::new(), hevc, &pics, &keys);
                let tags = publish(ts, passthrough_output()).await;
                let base = pics[0].1.unwrap_or(pics[0].0);
                let with_cts = assert_wire_decode_order(&tags, &pics, base, hevc);
                assert!(with_cts >= 3, "hevc {hevc}: the fixture lost its DTS ({with_cts})");
                let headers = tags.iter().filter(|t| t.video && tag_cts(&t.data).is_none()).count();
                assert_eq!(headers, 3, "hevc {hevc}: a header, then a re-send on each key picture");
            }
        }
    }

    /// A source without B-frames goes out on the timeline it always had: each
    /// picture at its PTS, composition time 0, HEVC as `CodedFramesX`. (HEVC
    /// tags now keep their in-band parameter sets, under a corrected `hvcC`:
    /// edge#211.)
    #[tokio::test]
    async fn a_source_without_b_frames_reaches_flv_unchanged() {
        let pics: Vec<(u64, Option<u64>)> = (0..10).map(|k| (T0 + k * 3_003, None)).collect();
        for hevc in [false, true] {
            let tags = publish(video_ts(&mut TsMuxer::new(), hevc, &pics, &[0, 5]), passthrough_output()).await;
            assert_eq!(assert_wire_decode_order(&tags, &pics, T0, hevc), 0, "hevc {hevc}");
        }
    }

    /// Audio anchored the epoch (an ADTS frame is handed out the moment it
    /// is complete; a video PES only at the next PUSI on its PID), and the
    /// first pictures decode before it. They go out at 0 — the timeline does
    /// not step back — and each picture presents on the ms the audio at its
    /// PTS is stamped.
    #[tokio::test]
    async fn video_behind_an_audio_anchored_epoch_presents_with_its_audio() {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let a0 = T0 + 2 * F;
        let mut mux = TsMuxer::new();
        mux.set_has_audio(true);
        let mut ts = Vec::new();
        let (mut off, mut pts) = (0usize, a0);
        for _ in 0..4 {
            let len = (usize::from(ADTS[off + 3] & 0x03) << 11)
                | (usize::from(ADTS[off + 4]) << 3)
                | (usize::from(ADTS[off + 5]) >> 5);
            ts.extend(mux.mux_audio_pre_adts(&ADTS[off..off + len], pts));
            off += len;
            pts += 1_920;
        }
        let pics = ibbp(9, |k| T0 + k * F);
        ts.extend(video_ts(&mut mux, false, &pics, &[0]));
        let tags = publish(ts, passthrough_output()).await;
        let audio: Vec<u32> = tags.iter().filter(|t| !t.video && t.data[1] == 1).map(|t| t.ts).collect();
        assert_eq!(audio, [0, 21, 42, 64], "audio anchors the epoch");
        assert!(assert_wire_decode_order(&tags, &pics, a0, false) > 0);
        let first = tags.iter().find(|t| t.video && tag_cts(&t.data).is_some()).unwrap();
        assert_eq!((first.ts, tag_cts(&first.data)), (0, Some(40)), "the I picture, a frame after the epoch");
    }

    /// Edge to edge: a B-frame H.264 publish into the edge's RTMP input is
    /// muxed with the PTS and DTS its FLV tags carried (`input_rtmp`: DTS
    /// = the tag timestamp, PTS = DTS + composition time, on its fixed
    /// timeline start). Re-published through an RTMP output it carries the
    /// same decode times and composition times out. The test reproduces
    /// that muxing by hand rather than driving `input_rtmp`.
    #[tokio::test]
    async fn an_flv_composition_time_survives_an_edge_to_edge_hop() {
        // input_rtmp's TIMELINE_START_90K.
        const START_90K: u64 = crate::engine::rtmp::ts_mux::PCR_LEAD_90K;
        // (tag timestamp, composition time) as an x264 / OBS publisher
        // sends IBBP at 25 fps: decode order I P B B P B B P B, two frames
        // of decode delay, so every picture — a B too — carries one.
        let flv: [(u64, u64); 9] =
            [(0, 80), (40, 160), (80, 40), (120, 40), (160, 160), (200, 40), (240, 40), (280, 160), (320, 40)];
        let pics: Vec<(u64, Option<u64>)> = flv
            .iter()
            .map(|&(dts, cts)| (START_90K + (dts + cts) * 90, (cts > 0).then_some(START_90K + dts * 90)))
            .collect();
        let tags = publish(video_ts(&mut TsMuxer::new(), false, &pics, &[0]), passthrough_output()).await;
        let out: Vec<(u64, u64)> = tags
            .iter()
            .filter(|t| t.video)
            .filter_map(|t| Some((u64::from(t.ts), u64::from(tag_cts(&t.data)?))))
            .collect();
        assert_eq!(out, flv);
    }

    /// Every video tag that is a sequence header, in order.
    fn headers(tags: &[WireTag]) -> Vec<&WireTag> {
        tags.iter().filter(|t| t.video && tag_cts(&t.data).is_none()).collect()
    }

    /// The index, among the video tags, of each picture's tag.
    fn picture_positions(video: &[&WireTag]) -> Vec<usize> {
        (0..video.len()).filter(|&i| tag_cts(&video[i].data).is_some()).collect()
    }

    /// edge#211, the reported failure: an HEVC source whose IRAPs carry PPS0,
    /// PPS1 and PPS2 while its slices name PPS0 and PPS1 (770_H). The sequence
    /// header carried the last PPS alone and the coded frames none, so no
    /// picture decoded at the receiver ("PPS id out of range"). The header now
    /// carries all three under FFmpeg's header bytes, and every key picture
    /// keeps its parameter sets in-band — however the TS was published.
    #[tokio::test]
    async fn a_multi_pps_hevc_source_reaches_flv_with_every_pps() {
        let (vps, sps, pps) = h770();
        let pics: Vec<(u64, Option<u64>)> = (0..12).map(|k| (T0 + k * F, None)).collect();
        let nals = |key: bool, k: usize| -> Vec<Vec<u8>> {
            let k = k as u8;
            if key {
                let mut au = vec![HEVC_AUD.to_vec(), vps.clone(), sps.clone()];
                au.extend(pps.iter().cloned());
                au.push(hevc_idr(k).to_vec());
                au
            } else {
                vec![HEVC_AUD.to_vec(), hevc_trail(k).to_vec()]
            }
        };
        let au = |key: bool, k: usize| annex_b(&nals(key, k).iter().map(Vec::as_slice).collect::<Vec<_>>());
        let ts = video_ts_with(&mut TsMuxer::new(), true, &pics, &[0, 6], &au);
        for (shape, items) in as_published(ts) {
            let tags = publish(items, passthrough_output()).await;
            let video: Vec<&WireTag> = tags.iter().filter(|t| t.video).collect();
            let first = &video[0].data;
            assert_eq!(first[0], 0x90, "{shape}: the first video tag is a SequenceStart");
            assert_eq!(
                first[5..28],
                hx("0102200000006000000000007bf000fcfdfafa00000f03")[..],
                "{shape}: FFmpeg's hvcC header for 770_H",
            );
            assert_eq!(header_sets(first), (vec![vps.clone()], vec![sps.clone()], pps.to_vec()), "{shape}");
            let headers = headers(&tags);
            assert_eq!(headers.len(), 3, "{shape}");
            assert!(headers.iter().all(|h| h.data == *first), "{shape}: one header, re-sent");
            let keys: Vec<&WireTag> = video.iter().copied().filter(|t| t.data[0] == 0x93).collect();
            assert_eq!(keys.len(), 2, "{shape}");
            for (tag, k) in keys.iter().zip([0, 6]) {
                assert_eq!(
                    tag.data[5..],
                    to_length_prefixed(&nals(true, k))[..],
                    "{shape}: key picture {k} keeps every NAL it carried",
                );
            }
            assert_wire_decode_order(&tags, &pics, T0, true);
        }
    }

    /// The same for H.264: the `avcC` carries every PPS, in id order, and the
    /// NALU tags carry their parameter sets in-band as they always did.
    #[tokio::test]
    async fn a_multi_pps_h264_source_reaches_flv_with_every_pps() {
        let (pps1, pps2) = (h264_pps(1, 1), h264_pps(2, 1));
        let pics: Vec<(u64, Option<u64>)> = (0..8).map(|k| (T0 + k * F, None)).collect();
        let keys = [0, 4];
        let nals = |key: bool, k: usize| -> Vec<Vec<u8>> {
            let k = k as u8;
            if key {
                vec![
                    H264_AUD.to_vec(),
                    H264_SPS.to_vec(),
                    H264_PPS0.to_vec(),
                    pps1.clone(),
                    pps2.clone(),
                    h264_idr(k).to_vec(),
                ]
            } else {
                vec![H264_AUD.to_vec(), h264_slice(k).to_vec()]
            }
        };
        let au = |key: bool, k: usize| annex_b(&nals(key, k).iter().map(Vec::as_slice).collect::<Vec<_>>());
        let tags = publish(video_ts_with(&mut TsMuxer::new(), false, &pics, &keys, &au), passthrough_output()).await;
        let mut want = vec![0x17, 0, 0, 0, 0, 0x01, 0x42, 0xC0, 0x1E, 0xFF, 0xE1, 0x00, 0x05];
        want.extend_from_slice(&H264_SPS);
        want.push(3);
        for p in [&H264_PPS0[..], &pps1, &pps2] {
            want.extend_from_slice(&(p.len() as u16).to_be_bytes());
            want.extend_from_slice(p);
        }
        let headers = headers(&tags);
        assert_eq!(headers.len(), 3);
        assert!(headers.iter().all(|h| h.data[..] == want[..]), "{:02x?}", &headers[0].data[..]);
        let video: Vec<&WireTag> = tags.iter().filter(|t| t.video).collect();
        let pictures = picture_positions(&video);
        assert_eq!(pictures.len(), pics.len());
        for (k, &at) in pictures.iter().enumerate() {
            let key = keys.contains(&k);
            assert_eq!(video[at].data[0], if key { 0x17 } else { 0x27 }, "picture {k}");
            assert_eq!(video[at].data[5..], to_length_prefixed(&nals(key, k))[..], "picture {k}");
        }
        assert_wire_decode_order(&tags, &pics, T0, false);
    }

    /// A parameter set under a new id between key pictures renews the header
    /// at once — immediately ahead of the picture that brought it, at that
    /// picture's decode time — since the next picture may name it. The table
    /// is fed one access unit at a time: built from what a whole datagram
    /// held, the one-item feed's first header would already carry PPS3.
    #[tokio::test]
    async fn a_parameter_set_arriving_mid_gop_renews_the_header_before_its_picture() {
        let pics: Vec<(u64, Option<u64>)> = (0..6).map(|k| (T0 + k * F, None)).collect();
        for hevc in [false, true] {
            let (pps0, pps3) =
                if hevc { (h770().2[0].clone(), hevc_pps(3, 1)) } else { (H264_PPS0.to_vec(), h264_pps(3, 1)) };
            let au = |key: bool, k: usize| match (hevc, k) {
                (true, 3) => annex_b(&[&HEVC_AUD, &pps3, &hevc_trail(3)]),
                (false, 3) => annex_b(&[&H264_AUD, &pps3, &h264_slice(3)]),
                (true, _) => h265_au(key, k as u8),
                (false, _) => h264_au(key, k as u8),
            };
            let ts = video_ts_with(&mut TsMuxer::new(), hevc, &pics, &[0], &au);
            for (shape, items) in as_published(ts) {
                let tags = publish(items, passthrough_output()).await;
                let headers = headers(&tags);
                assert_eq!(headers.len(), 3, "hevc {hevc}, {shape}");
                assert_eq!(header_sets(&headers[0].data).2, vec![pps0.clone()], "hevc {hevc}, {shape}: the first");
                let video: Vec<&WireTag> = tags.iter().filter(|t| t.video).collect();
                let pictures = picture_positions(&video);
                let between = &video[pictures[2] + 1..pictures[3]];
                assert_eq!(between.len(), 1, "hevc {hevc}, {shape}: one header between pictures 2 and 3");
                assert_eq!(between[0].ts, video[pictures[3]].ts, "hevc {hevc}, {shape}: at picture 3's decode time");
                assert_eq!(
                    header_sets(&between[0].data).2,
                    vec![pps0.clone(), pps3.clone()],
                    "hevc {hevc}, {shape}: renewed with PPS3",
                );
                assert_wire_decode_order(&tags, &pics, T0, hevc);
            }
        }
    }

    /// A PPS re-sent under its id with other bytes between key pictures goes
    /// to the decoder in-band and leaves the header alone; the next key
    /// picture renews the header with it. Renewing on every change would put a
    /// header ahead of every picture of a source that alternates its PPS.
    #[tokio::test]
    async fn a_pps_resent_with_new_bytes_mid_gop_waits_for_the_key_picture() {
        let pics: Vec<(u64, Option<u64>)> = (0..10).map(|k| (T0 + k * F, None)).collect();
        let (vps, sps, _) = h770();
        for hevc in [false, true] {
            let pps = |k: usize| {
                let tag = 1 + (k % 2) as u8;
                if hevc { hevc_pps(0, tag) } else { h264_pps(0, tag) }
            };
            let au = |key: bool, k: usize| {
                let (n, p) = (k as u8, pps(k));
                match (hevc, key) {
                    (true, true) => annex_b(&[&HEVC_AUD, &vps, &sps, &p, &hevc_idr(n)]),
                    (true, false) => annex_b(&[&HEVC_AUD, &p, &hevc_trail(n)]),
                    (false, true) => annex_b(&[&H264_AUD, &H264_SPS, &p, &h264_idr(n)]),
                    (false, false) => annex_b(&[&H264_AUD, &p, &h264_slice(n)]),
                }
            };
            let tags = publish(video_ts_with(&mut TsMuxer::new(), hevc, &pics, &[0, 5], &au), passthrough_output()).await;
            let headers = headers(&tags);
            assert_eq!(headers.len(), 3, "hevc {hevc}");
            assert_eq!(headers[0].data, headers[1].data, "hevc {hevc}: a header, then its key-picture re-send");
            assert_eq!(header_sets(&headers[0].data).2, vec![pps(0)], "hevc {hevc}");
            assert_eq!(header_sets(&headers[2].data).2, vec![pps(5)], "hevc {hevc}: renewed at the key picture");
            let ms = |k: usize| ((pics[k].0 - T0) / 90) as u32;
            assert_eq!([headers[0].ts, headers[1].ts, headers[2].ts], [ms(0), ms(0), ms(5)], "hevc {hevc}");
            assert_wire_decode_order(&tags, &pics, T0, hevc);
        }
    }

    /// An HEVC access unit carrying no picture — parameter sets, with or
    /// without an AUD — sends no tag of its own (a CodedFrames tag carries a
    /// picture), and the renewal its new PPS causes goes out immediately
    /// ahead of the next picture, at that picture's decode time.
    #[tokio::test]
    async fn an_hevc_access_unit_without_a_picture_sends_no_tag() {
        let (vps, sps, pps) = h770();
        let pps3 = hevc_pps(3, 1);
        let pics: Vec<(u64, Option<u64>)> = (0..5).map(|k| (T0 + k * F, None)).collect();
        for with_aud in [true, false] {
            let mut sets: Vec<&[u8]> = vec![&vps[..], &sps[..], &pps3[..]];
            if with_aud {
                sets.insert(0, &HEVC_AUD[..]);
            }
            let mut mux = TsMuxer::new();
            mux.set_video_stream_type(0x24);
            let mut ts = Vec::new();
            for (k, &(pts, _)) in pics.iter().enumerate() {
                if k == 2 {
                    let at = T0 + F + F / 2;
                    ts.extend(mux.mux_video(&annex_b(&sets), at, at, false));
                }
                ts.extend(mux.mux_video(&h265_au(k == 0, k as u8), pts, pts, k == 0));
            }
            let end = T0 + 5 * F;
            ts.extend(mux.mux_video(&h265_au(false, 5), end, end, false));
            let tags = publish(ts, passthrough_output()).await;
            let video: Vec<&WireTag> = tags.iter().filter(|t| t.video).collect();
            assert_eq!(headers(&tags).len(), 3, "aud {with_aud}");
            assert_eq!(video.len(), pics.len() + 3, "aud {with_aud}: no tag for the access unit");
            let pictures = picture_positions(&video);
            assert_eq!(pictures[2] - pictures[1], 2, "aud {with_aud}: one header between pictures 1 and 2");
            let renewal = video[pictures[2] - 1];
            assert_eq!(renewal.ts, video[pictures[2]].ts, "aud {with_aud}: at picture 2's decode time");
            assert_eq!(header_sets(&renewal.data).2, vec![pps[0].clone(), pps3.clone()], "aud {with_aud}");
            assert_wire_decode_order(&tags, &pics, T0, true);
        }
    }

    /// Non-regression: a clean single-set H.264 source sends the sequence
    /// header it always sent, byte for byte.
    #[tokio::test]
    async fn a_clean_h264_source_keeps_its_sequence_header_bytes() {
        let pics: Vec<(u64, Option<u64>)> = (0..4).map(|k| (T0 + k * F, None)).collect();
        let tags = publish(video_ts(&mut TsMuxer::new(), false, &pics, &[0]), passthrough_output()).await;
        let first = tags.iter().find(|t| t.video).unwrap();
        assert_eq!(
            first.data[..],
            [
                0x17, 0, 0, 0, 0, 0x01, 0x42, 0xC0, 0x1E, 0xFF, 0xE1, 0x00, 0x05, 0x67, 0x42, 0xC0, 0x1E,
                0xDA, 0x01, 0x00, 0x04, 0x68, 0xCE, 0x3C, 0x80,
            ],
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::flv_wire_tests::{annex_b, header_sets};
    use super::*;
    use crate::engine::cmaf::param_sets::tests::{HevcSpsSpec, h264_pps, h770, hevc_pps, hevc_sps_full, hx};

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

    /// No FLV AAC sequence header goes out while the re-encoder waits for a
    /// frame that decodes: it would announce the ADTS header's format —
    /// for HE-AAC the 24 kHz / mono core — not the 48 kHz stereo the
    /// re-encode then carries. Passthrough announces the source's; a
    /// re-encode, its output.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn the_aac_sequence_header_waits_for_the_resolved_format() {
        let he_aac_v2 = (1u8, 6u8, 1u8);
        assert_eq!(aac_sequence_header_format(&EncoderState::Lazy, he_aac_v2), None);
        assert_eq!(aac_sequence_header_format(&EncoderState::Transparent, he_aac_v2), Some((6, 1)));
        let cfg: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "r1", "name": "r1", "dest_url": "rtmp://127.0.0.1/app", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc", "bitrate_kbps": 128 }
        }))
        .unwrap();
        let ts = crate::engine::ts_test_fixtures::aac_program_ts(
            &crate::engine::ts_test_fixtures::he_aac_adts(true),
        );
        let mut demux = TsDemuxer::new(None);
        let first = first_aac_frame(&mut demux, &ts);
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("r1".into(), "r1".into(), "rtmp".into()));
        let lazy = build_encoder_state(&cfg, &demux, Some(&[]), true, &CancellationToken::new(), &stats, "f", &events);
        assert!(matches!(lazy, EncoderState::Lazy), "a frame that does not decode leaves it waiting");
        assert_eq!(aac_sequence_header_format(&lazy, he_aac_v2), None);
        let active = build_encoder_state(&cfg, &demux, Some(&first), true, &CancellationToken::new(), &stats, "f", &events);
        assert_eq!(aac_sequence_header_format(&active, he_aac_v2), Some((3, 2)), "48 kHz stereo");
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

    /// A PES of MP2 frames at 48 kHz stereo, `n` of them.
    #[cfg(feature = "media-codecs")]
    pub(crate) fn mp2_pes(n: usize) -> Vec<u8> {
        let mut e = video_engine::AudioEncoder::open(&video_codec::AudioEncoderConfig {
            codec: video_codec::AudioCodecType::Mp2,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 192,
        })
        .unwrap();
        let fs = e.frame_size();
        let mut out = Vec::new();
        let mut k = 0usize;
        while out.len() < n * 576 {
            let tone: Vec<Vec<f32>> = (0..2)
                .map(|_| (0..fs).map(|i| 0.25 * ((k + i) as f32 * 0.0654).sin()).collect())
                .collect();
            for f in e.encode_frame(&tone).unwrap() {
                out.extend_from_slice(&f.data);
            }
            k += fs;
        }
        out
    }

    /// An MP2 source builds the AAC encoder from what it decodes to, and
    /// its audio comes out re-encoded. The encoder was built only from the
    /// demuxer's AAC config — which an MP2 / AC-3 / E-AC-3 source never has
    /// — so it stayed `Lazy` and the audio was dropped for good.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn an_mp2_source_builds_the_aac_encoder_from_its_decoded_format() {
        let cfg: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "r1", "name": "r1", "dest_url": "rtmp://127.0.0.1/app", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc", "sample_rate": 44100, "channels": 1 }
        }))
        .unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("r1".into(), "r1".into(), "rtmp".into()));
        let cancel = CancellationToken::new();
        let mut state = EncoderState::Lazy;
        let (mut dec, mut dec_codec) = (None, None);
        let mut aac = 0usize;
        for k in 0..20u64 {
            let decoded = decode_other_audio_for_encode(
                &mut state, &mut dec, &mut dec_codec, 0x03, &mp2_pes(4), 900_000 + k * 8_640,
                &cfg, &cancel, &stats, "f", &events,
            );
            let EncoderState::Active { encoder, stage, .. } = &mut state else {
                panic!("the encoder is built on the first PES that decodes");
            };
            assert_eq!((encoder.params().sample_rate, encoder.params().channels), (44_100, 1));
            for f in decoded {
                encoder.submit_through(stage, &f.planar, f.sample_rate, 900_000 + k * 8_640).unwrap();
            }
            aac += encoder.drain().len();
        }
        assert!(aac > 50, "{aac} AAC frames from 1.9 s of MP2");
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
        // Composition time 0 — every re-encoded picture, every picture of a
        // source without B-frames — keeps the bytes it always had.
        let avcc = [0x01, 0x02];
        let kf = build_hevc_coded_frames_tag(&avcc, true, 0);
        assert_eq!(kf[0], 0x93, "IsEx | keyframe | PacketType=CodedFramesX");
        assert_eq!(&kf[1..5], b"hvc1");
        assert_eq!(&kf[5..], &avcc, "no composition-time field");
        let inter = build_hevc_coded_frames_tag(&avcc, false, 0);
        assert_eq!(inter[0], 0xA3, "IsEx | inter | PacketType=CodedFramesX");
        assert_eq!(&inter[5..], &avcc);
    }

    /// edge#205: a picture that presents after it decodes goes out as
    /// Enhanced-RTMP `CodedFrames` (PacketType 1), whose SI24 composition
    /// time follows the FourCC. It went out as `CodedFramesX`, which has no
    /// such field, so the receiver presented it at its decode time.
    #[test]
    fn an_hevc_picture_with_a_composition_time_is_coded_frames() {
        let avcc = [0x00, 0x00, 0x00, 0x02, 0x26, 0x01];
        let kf = build_hevc_coded_frames_tag(&avcc, true, 80);
        assert_eq!(kf[0], 0x91, "IsEx | keyframe | PacketType=CodedFrames");
        assert_eq!(&kf[1..5], b"hvc1");
        assert_eq!(&kf[5..8], &[0, 0, 80], "SI24 composition time");
        assert_eq!(&kf[8..], &avcc);
        let inter = build_hevc_coded_frames_tag(&avcc, false, 2000);
        assert_eq!(inter[0], 0xA1, "IsEx | inter | PacketType=CodedFrames");
        assert_eq!(&inter[5..8], &[0x00, 0x07, 0xD0]);
        assert_eq!(&inter[8..], &avcc);
    }

    /// edge#205: the classic AVC NALU tag carries the picture's composition
    /// time in its SI24 field, where it hard-coded 0. Composition time 0
    /// writes the bytes it always wrote.
    #[test]
    fn build_avc_nalu_tag_carries_the_composition_time() {
        let nalus = vec![vec![0x41, 0x9A, 0x22]];
        let tag = build_avc_nalu_tag(&nalus, false, 80);
        assert_eq!(&tag[..5], &[0x27, 0x01, 0, 0, 80]);
        assert_eq!(&tag[5..], &[0, 0, 0, 3, 0x41, 0x9A, 0x22]);
        assert_eq!(&build_avc_nalu_tag(&nalus, true, 2000)[..5], &[0x17, 0x01, 0x00, 0x07, 0xD0]);
        assert_eq!(
            build_avc_nalu_tag(&nalus, true, 0),
            vec![0x17, 0x01, 0, 0, 0, 0, 0, 0, 3, 0x41, 0x9A, 0x22],
        );
    }

    const HEVC_IDR: [u8; 4] = [0x26, 0x01, 0xAF, 0x21];
    const HEVC_TRAIL: [u8; 4] = [0x02, 0x01, 0xD0, 0x21];
    /// FFmpeg's FLV muxer's `hvcC` header for 770_H, and numOfArrays.
    const H770_HEADER: &str = "0102200000006000000000007bf000fcfdfafa00000f03";
    const H264: CmafVideoCodec = CmafVideoCodec::H264;
    const H265: CmafVideoCodec = CmafVideoCodec::H265;

    fn au(nals: &[&[u8]]) -> Vec<Vec<u8>> {
        nals.iter().map(|n| n.to_vec()).collect()
    }

    /// The headers ahead of a picture about to go out: the tag, and how many
    /// copies.
    fn take(pv: &mut PassthroughVideo, codec: CmafVideoCodec, key: bool) -> Option<(Vec<u8>, usize)> {
        pv.take_headers(codec, key).map(|(tag, n)| (tag.to_vec(), n))
    }

    /// edge#211: no header until the table holds a VPS, an SPS and a PPS;
    /// then one carrying every PPS the key picture brought, under FFmpeg's
    /// header bytes and with no array marked complete, sent twice ahead of
    /// the connection's first key picture as it always was.
    #[test]
    fn the_sequence_header_waits_for_a_complete_table() {
        let (vps, sps, pps) = h770();
        let mut pv = PassthroughVideo::default();
        assert!(!pv.observe(H265, &au(&[&pps[0], &HEVC_TRAIL]), false));
        assert_eq!(take(&mut pv, H265, false), None);
        assert!(pv.observe(H265, &au(&[&vps, &sps, &pps[0], &pps[1], &pps[2], &HEVC_IDR]), true));
        let mut want = vec![0x90];
        want.extend_from_slice(b"hvc1");
        want.extend(hx(H770_HEADER));
        for (nal_type, list) in [(0x20u8, vec![vps.clone()]), (0x21, vec![sps.clone()]), (0x22, pps.to_vec())] {
            want.push(nal_type);
            want.extend((list.len() as u16).to_be_bytes());
            for nal in &list {
                want.extend((nal.len() as u16).to_be_bytes());
                want.extend_from_slice(nal);
            }
        }
        assert_eq!(take(&mut pv, H265, true), Some((want, 2)));
    }

    /// Between key pictures only a new id renews the header — changed bytes
    /// under a known id reach the decoder in-band — and at a key picture
    /// changed bytes do too. A renewal makes the header the table, and on a
    /// key picture rides its one re-send rather than adding a second.
    #[test]
    fn a_header_is_renewed_by_id_between_key_pictures_and_by_bytes_at_them() {
        let (vps, sps, pps) = h770();
        let key = au(&[&vps, &sps, &pps[0], &pps[1], &pps[2], &HEVC_IDR]);
        let mut pv = PassthroughVideo::default();
        assert!(pv.observe(H265, &key, true));
        let (tag, _) = take(&mut pv, H265, true).unwrap();
        // (a) The same key picture again: one re-send of the same header.
        assert!(!pv.observe(H265, &key, true));
        assert_eq!(take(&mut pv, H265, true), Some((tag.clone(), 1)));
        // (b) PPS0 re-sent as it was: nothing.
        assert!(!pv.observe(H265, &au(&[&pps[0], &HEVC_TRAIL]), false));
        assert_eq!(take(&mut pv, H265, false), Some((tag.clone(), 0)));
        // (c) PPS0 with other bytes between key pictures: in-band only.
        assert!(!pv.observe(H265, &au(&[&hevc_pps(0, 9), &HEVC_TRAIL]), false));
        assert_eq!(header_sets(&pv.header.as_ref().unwrap().tag).2, pps.to_vec());
        // (d) A key picture leaving PPS2 out, PPS0 and PPS1 as the header has
        // them: not a change, and the header keeps all three.
        assert!(!pv.observe(H265, &au(&[&pps[0], &pps[1], &HEVC_IDR]), true));
        assert_eq!(take(&mut pv, H265, true), Some((tag.clone(), 1)));
        assert_eq!(header_sets(&tag).2.len(), 3);
        // (e) A new id between key pictures renews at once, and the header
        // becomes the table: PPS2, which the key picture left out, goes.
        assert!(pv.observe(H265, &au(&[&hevc_pps(3, 1), &HEVC_TRAIL]), false));
        let (renewed, n) = take(&mut pv, H265, false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(header_sets(&renewed).2, vec![pps[0].clone(), pps[1].clone(), hevc_pps(3, 1)]);
        // (f) A key picture with a changed SPS: renewed, one copy, not two.
        let sps150 = hevc_sps_full(HevcSpsSpec { level: 150, ..Default::default() });
        assert!(pv.observe(H265, &au(&[&vps, &sps150, &pps[0], &HEVC_IDR]), true));
        let (changed, n) = take(&mut pv, H265, true).unwrap();
        assert_eq!(n, 1);
        assert_eq!(header_sets(&changed), (vec![vps.clone()], vec![sps150], vec![pps[0].clone()]));
    }

    /// A change of codec starts again: no header describes the new codec's
    /// pictures until its table is complete, and the old codec's header goes.
    #[test]
    fn a_codec_change_starts_the_header_again() {
        let mut pv = PassthroughVideo::default();
        let h264_key = au(&[&[0x67, 0x42, 0xC0, 0x1E, 0xDA], &[0x68, 0xCE, 0x3C, 0x80], &[0x65, 0x88, 0x84, 0x21]]);
        assert!(pv.observe(H264, &h264_key, true));
        assert_eq!(take(&mut pv, H264, true).map(|(_, n)| n), Some(2));
        let (vps, sps, pps) = h770();
        assert!(!pv.observe(H265, &au(&[&pps[0], &HEVC_TRAIL]), false));
        assert_eq!(take(&mut pv, H265, false), None, "nothing describes HEVC yet");
        assert!(pv.observe(H265, &au(&[&vps, &sps, &pps[0], &HEVC_IDR]), true));
        let (tag, n) = take(&mut pv, H265, true).unwrap();
        assert_eq!((tag[0], n), (0x90, 1));
        assert_eq!(take(&mut pv, H264, true), None, "nor H.264 any longer");
    }

    /// At a discontinuity the table starts again. Pictures keep going out
    /// under the old header until the new source's table is complete, which
    /// then replaces it even though the old one holds every set it has —
    /// dropping the old source's extra ids.
    #[test]
    fn a_discontinuity_rebuilds_the_header_from_the_new_source() {
        let sps = [0x67, 0x42, 0xC0, 0x1E, 0xDA];
        let pps0 = [0x68, 0xCE, 0x3C, 0x80];
        let idr = [0x65, 0x88, 0x84, 0x21];
        let mut pv = PassthroughVideo::default();
        assert!(pv.observe(H264, &au(&[&sps, &pps0, &h264_pps(1, 1), &h264_pps(2, 1), &idr]), true));
        let (old, _) = take(&mut pv, H264, true).unwrap();
        assert_eq!(header_sets(&old).2.len(), 3);
        pv.restart();
        assert!(!pv.observe(H264, &au(&[&[0x41, 0x9A, 0x22, 0x21]]), false));
        assert_eq!(take(&mut pv, H264, false), Some((old, 0)), "the old header meanwhile");
        assert!(pv.observe(H264, &au(&[&sps, &pps0, &idr]), true), "replaced, though it held these");
        let (new, n) = take(&mut pv, H264, true).unwrap();
        assert_eq!(n, 1);
        assert_eq!(header_sets(&new).2, vec![pps0.to_vec()]);
    }

    /// edge#211: the re-encoder's `hvcC` header is read from its SPS RBSP as
    /// passthrough's is — it took the raw bytes one early — and says its sets
    /// may arrive in-band too.
    #[test]
    fn the_re_encoders_hvcc_reads_its_sps_rbsp() {
        let (vps, sps, pps) = h770();
        let tag = build_hevc_sequence_header_from_hvcc(&annex_b(&[&vps, &sps, &pps[0]]));
        assert_eq!(&tag[..5], &[0x90, b'h', b'v', b'c', b'1']);
        assert_eq!(tag[5..28], hx(H770_HEADER)[..]);
        let mut at = 28;
        for (nal_type, nal) in [(0x20u8, &vps), (0x21, &sps), (0x22, &pps[0])] {
            assert_eq!(&tag[at..at + 3], &[nal_type, 0, 1], "one entry, not marked complete");
            at += 3 + 2 + nal.len();
        }
        assert_eq!(header_sets(&tag), (vec![vps.clone()], vec![sps.clone()], vec![pps[0].clone()]));
    }

    /// Non-regression: the re-encoder's AVC sequence header from x264's
    /// Annex-B extradata is the record it always sent.
    #[test]
    fn the_re_encoders_avcc_is_unchanged() {
        let sps = hx("6764000d32cd94141fb011000003000100000300320f142996");
        let pps = hx("6831abe3cb22c0");
        let mut want = vec![0x17, 0, 0, 0, 0, 0x01, 0x64, 0x00, 0x0D, 0xFF, 0xE1, 0x00, 0x19];
        want.extend_from_slice(&sps);
        want.extend_from_slice(&[0x01, 0x00, 0x07]);
        want.extend_from_slice(&pps);
        assert_eq!(build_avc_sequence_header_from_avcc(&annex_b(&[&sps, &pps])), want);
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
