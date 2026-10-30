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
use super::audio_encode::{
    AudioCodec, AudioEncoder, AudioEncoderError, EncoderInputClock, EncoderParams, Placement,
};
use super::audio_silence::SilenceGenerator;
use super::cmaf::codecs::{push_avcc_record, push_hvcc_record};
use super::cmaf::fmp4::VideoCodec as CmafVideoCodec;
use super::cmaf::nalu::{H265_NAL_VPS, h264_is_sei_recovery_point, h265_nal_type, to_length_prefixed};
use super::cmaf::param_sets::{ParamSetCollector, ParamSets};
use super::cmaf::timeline::{CmafTimeline, Track};
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
        /// Where the encoder's input is on the connection's FLV timeline.
        /// The encoder anchors once and counts samples from there, so this
        /// is what re-anchors it where the source leaves it — a loop, a
        /// switch, a gap — and lays silence to the picture (CMAF's rule,
        /// shared). Without it the transcoded audio ran on across every seam
        /// while the video followed the source (edge#211).
        input: EncoderInputClock,
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
    /// Last wire PTS emitted, on the connection's FLV timeline, for the
    /// strictly-increasing guard ([`next_wire_pts_90k`]).
    ///
    /// The encoder's `fps_num`/`fps_den` are deliberately not held here. They
    /// still open the encoder (rate control, VBV, default GOP, SPS VUI) but
    /// they do not reach the FLV timestamp — that comes from the source clock
    /// through the timeline — and keeping a copy invited the next reader to
    /// use it again.
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
    // Every FLV timestamp, audio and video, on one continuous timeline per
    // connection (edge#211).
    let mut timeline = FlvTimeline::new(&config.id);
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
    // `silent_fallback` is active and the eager-build succeeded. The tick is
    // the watchdog, not the meter: each one lays silence up to where the
    // picture says the audio would be (`lay_silence`), so a tick observed
    // late costs nothing and the next makes it up.
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
        // Silence tick → lay silence up to the picture if the watchdog
        // says we should (no real audio within grace), then drain + write
        // FLV audio tags.
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
                    &mut timeline,
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
                    // A receiver can start decoding at an IDR — the demuxer's
                    // flag — or, on an IDR-less open-GOP source, at an intra
                    // picture a recovery-point SEI marks (edge#211). Judged
                    // once per access unit, here, so every use of the flag on
                    // this connection — the FLV frame type, the parameter-set
                    // table's random-access rule, the header re-send — agrees.
                    let is_keyframe = is_keyframe || h264_is_sei_recovery_point(&nalus);
                    let time = timeline.video(pts, dts, pts_known);
                    if process_video_frame(
                        VideoFrameSource::H264 { nalus: &nalus, is_keyframe, pts_known },
                        time,
                        &mut timeline,
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
                    let time = timeline.video(pts, dts, pts_known);
                    if process_video_frame(
                        VideoFrameSource::H265 { nalus: &nalus, is_keyframe, pts_known },
                        time,
                        &mut timeline,
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
                    // On the connection's timeline whether or not it is sent:
                    // a frame dropped below still anchors the epoch, as before.
                    let at = timeline.audio(pts);

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

                    // The FLV AAC sequence header's format, for a frame
                    // passed through. Always AOT=2 (AAC-LC) — even for
                    // HE-AAC output, most RTMP servers (Twitch, YouTube,
                    // nginx-rtmp) expect AOT=2 in the ASC and detect SBR /
                    // PS from the bitstream itself. This matches what
                    // `ffmpeg -c:a aac -profile:a aac_he -f flv` writes.
                    let header_format = demuxer
                        .cached_aac_config()
                        .and_then(|cached| aac_sequence_header_format(&encoder_state, cached));

                    match &mut encoder_state {
                        EncoderState::Disabled | EncoderState::Transparent => {
                            // Passthrough: the raw ADTS-stripped AAC frame as
                            // an FLV audio tag, the sequence header ahead of
                            // the first. A frame at or behind one already sent
                            // (a loop's overlap) is dropped.
                            if !sent_audio_header && header_format.is_none() {
                                continue;
                            }
                            let Some(ts_ms) = timeline.audio_tag_ms(at) else {
                                continue;
                            };
                            if !sent_audio_header
                                && let Some((asc_sr_idx, asc_ch_cfg)) = header_format
                            {
                                let header = build_aac_sequence_header(1, asc_sr_idx, asc_ch_cfg);
                                client.send_audio(&header, ts_ms).await?;
                                sent_audio_header = true;
                                tracing::debug!("RTMP output '{}': sent AAC sequence header", config.id);
                            }
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
                            input,
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
                            // grace-window expiry. And measure the picture's
                            // lead over it, which silence stops short by.
                            note_real_audio(silence.as_mut(), input, at, timeline.newest_picture());
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
                                    submit_source_audio(
                                        encoder,
                                        stage,
                                        input,
                                        silence.is_some(),
                                        &planar,
                                        dec.sample_rate(),
                                        at,
                                        &config.id,
                                    );
                                }
                                Err(e) => {
                                    decode_stats.inc_error();
                                    tracing::debug!(
                                        "RTMP output '{}': AAC decode failed: {e}",
                                        config.id
                                    );
                                }
                            }
                            // Write what the encoder has ready, each frame at
                            // its own stamp, the sequence header (in the
                            // encoder's format) ahead of the first.
                            send_encoded_audio(
                                client,
                                encoder,
                                &mut timeline,
                                &mut sent_audio_header,
                                demuxer.cached_aac_config().map(|c| c.1),
                                stats,
                                Some(recv_time_us),
                            )
                            .await?;
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
                    // On the timeline before the Disabled check: a dropped
                    // MP2 track still times the connection, as it did.
                    let at = timeline.audio(pts);

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
                        encoder, silence, stage, decode_stats, input, ..
                    } = &mut encoder_state
                    else {
                        continue;
                    };
                    note_real_audio(silence.as_mut(), input, at, timeline.newest_picture());

                    // To the encoder's format: MP2 at another rate was
                    // encoded as if at the encoder's (and ran at the wrong
                    // speed), 5.1 AC-3 refused. Each frame of the PES at its
                    // own time — the PES's PTS stamps its first — or every
                    // MP2 frame after the first (2 160 ticks, past the
                    // placement's slack) would read as a departure.
                    let fill_gaps = silence.is_some();
                    let mut frame_at = at;
                    for frame in decoded {
                        decode_stats.inc_output();
                        submit_source_audio(
                            encoder,
                            stage,
                            input,
                            fill_gaps,
                            &frame.planar,
                            frame.sample_rate,
                            frame_at,
                            &config.id,
                        );
                        frame_at = frame_at.saturating_add(audio_ticks(frame.samples_per_channel, frame.sample_rate));
                    }
                    // An MP2 / AC-3 / E-AC-3 source has no ADTS config to
                    // fall back on: the header is the encoder's target
                    // format, which is what every RTMP server expects.
                    send_encoded_audio(
                        client,
                        encoder,
                        &mut timeline,
                        &mut sent_audio_header,
                        None,
                        stats,
                        Some(recv_time_us),
                    )
                    .await?;
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
                    // Re-encode only, which takes the picture's presentation
                    // time: no DTS to read.
                    let time = timeline.video(pts, None, pts_known);
                    process_video_frame(
                        VideoFrameSource::Mpeg2 { es: &es, is_keyframe, pts_known },
                        time,
                        &mut timeline,
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
                // is deliberately not re-anchored: it judges the timestamps
                // themselves, as CMAF's does. A switch on one clock is a gap
                // or nothing, a switch to an unrelated clock a jump it
                // absorbs; re-anchoring here would close real gaps, and put
                // the old feed's last picture — handed out after this frame
                // — on the new feed's clock.
                DemuxedFrame::Discontinuity => passthrough.restart(),
                DemuxedFrame::Scte35(_) => {}
            }
        }

        // Flush after each TS packet batch to keep delivery smooth
        client.flush().await?;
    }
}

/// One lap of the 33-bit PTS clock.
const LAP_90K: u64 = 1 << 33;
const MASK_33: u64 = LAP_90K - 1;

/// One FLV millisecond: how far a held picture goes on past the last.
const FLV_MS_90K: u64 = 90;

/// The longest pause kept on a track with nothing beside it (a video-only
/// source, an audio-only one): a slide feed or VFR camera keeps its timing;
/// a step past it is a jump and is continued one frame on.
const FLV_LONE_GAP_90K: u64 = 10 * 90_000;

/// The most silence one tick lays beside a picture (46 AAC frames at
/// 48 kHz). A pause the timeline keeps (up to [`FLV_LONE_GAP_90K`]) is
/// caught up over the next ticks, at about 46 times real time, rather than
/// encoded in one go: 470 frames at the lone gap.
const SILENCE_PER_TICK_90K: u64 = 90_000;

/// The 64-bit value of the 33-bit `ts` that lies nearest `near`.
fn unwrap_near(ts: u64, near: u64) -> u64 {
    let ahead = ts.wrapping_sub(near) & MASK_33;
    if ahead < LAP_90K / 2 { near.saturating_add(ahead) } else { near.saturating_sub(LAP_90K - ahead) }
}

/// One audio frame's length in 90 kHz ticks.
#[cfg_attr(not(feature = "media-codecs"), allow(dead_code))]
fn audio_ticks(samples: usize, rate: u32) -> u64 {
    samples as u64 * 90_000 / u64::from(rate.max(1))
}

/// A picture's place on the connection's FLV timeline ([`FlvTimeline::video`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PictureTime {
    /// Presentation time, unwrapped, never held: what the re-encoder feeds
    /// its decoder.
    #[cfg_attr(not(feature = "media-codecs"), allow(dead_code))]
    pts: u64,
    /// Decode time, unwrapped, at least a ms past the last picture's.
    dts: u64,
    /// The FLV tag timestamp (the decode time) and composition time.
    ts_ms: u32,
    cts_ms: u32,
}

/// One RTMP connection's FLV timeline: every tag timestamp, audio and video,
/// passthrough and re-encoded, is placed on it (edge#211).
///
/// FLV wants one timeline that never steps back, and the source's need not
/// be one. The connection's epoch used to be set once, from the first
/// essence, and every tag stamped against it from the raw 33-bit PTS: a
/// backward jump — a looped MPTS with no input `program_number`, so the PTS
/// rewriter passes it through, or a switch on a `passthrough_clock` flow —
/// stamped every later tag behind what had gone out, or 0; the 33-bit wrap,
/// every 26.5 h, stamped every tag 0 until the PTS climbed back past the
/// epoch; and a picture whose PES carried no PTS was stamped 0 in mid-stream.
///
/// Every timestamp now goes through a [`CmafTimeline`] — CMAF's, not a
/// second copy of its rules — of the connection's own, which keeps the
/// source's timestamps where they are continuous and continues a jump one
/// frame on, sharing the offset between the tracks so the new feed keeps its
/// own A/V relation; and the result is unwrapped to 64 bits, so the wrap is
/// a lap and not a step. The video is mapped by its decode time, which is
/// monotonic on a clean source, and presented at that plus its composition
/// offset. A track with nothing beside it keeps a pause of up to
/// [`FLV_LONE_GAP_90K`] (`CmafTimeline::with_lone_gap`).
#[derive(Debug)]
struct FlvTimeline {
    source: CmafTimeline,
    /// The last unwrapped value, either track's: what the next is unwrapped
    /// against.
    reference: Option<u64>,
    /// FLV ms 0, unwrapped: the first essence's (a picture's decode time).
    epoch: Option<u64>,
    /// The last picture's time: a picture whose PES carried no PTS takes it,
    /// and the next decode time goes at least a ms past it.
    last_picture: Option<PictureTime>,
    /// The newest presentation time any picture has had: where the picture
    /// is, which silence is laid to.
    newest_picture: Option<u64>,
    /// How far the last picture's decode time ran ahead of the newest audio
    /// while the video was live ([`Self::live_picture`]), negative when it
    /// ran behind: where a picture coming back after the video was taken for
    /// gone would be, beside the audio ([`Self::video`]). Counted as none
    /// when behind, that put it past its place by as far as the audio was
    /// muxed ahead, and a stray picture after a stall opened an offset
    /// there, the video that much late for good.
    picture_lead: i64,
    /// The newest audio time mapped, sent or not, silence on a connection
    /// with no picture included ([`Self::silent_audio`]).
    newest_audio: Option<u64>,
    /// The last real audio frame's source time (33 bits) and where it was
    /// mapped: what silence with no picture beside it is measured from.
    last_real_audio: Option<(u64, u64)>,
    /// The newest audio tag sent: one at or behind it is dropped.
    last_audio: Option<u64>,
    /// Jumps absorbed, and when one was last logged.
    jumps: u64,
    jump_logged: Option<std::time::Instant>,
    output_id: String,
}

impl FlvTimeline {
    fn new(output_id: &str) -> Self {
        Self {
            source: CmafTimeline::with_lone_gap(FLV_LONE_GAP_90K),
            reference: None,
            epoch: None,
            last_picture: None,
            newest_picture: None,
            picture_lead: 0,
            newest_audio: None,
            last_real_audio: None,
            last_audio: None,
            jumps: 0,
            jump_logged: None,
            output_id: output_id.to_string(),
        }
    }

    /// `src` (90 kHz, 33 bits) of `track` on the timeline, unwrapped. The
    /// first value is put a lap up, so a value behind it does not underflow.
    fn map(&mut self, track: Track, src: u64) -> u64 {
        let m = self.source.map(track, src);
        if let Some(jump) = m.jump {
            self.jumps += 1;
            let due = self.jump_logged.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60));
            if due {
                self.jump_logged = Some(std::time::Instant::now());
                tracing::info!(
                    output_id = self.output_id.as_str(),
                    track = ?track,
                    jump_s = jump as f64 / 90_000.0,
                    jumps = self.jumps,
                    "RTMP output: the source's timestamps jumped; the FLV timeline continues across it"
                );
            }
        }
        let at = self.reference.map_or(LAP_90K + m.ts, |r| unwrap_near(m.ts, r));
        self.reference = Some(at);
        at
    }

    /// One picture's time: `pts` and the PES's `dts` as the demuxer handed
    /// them out. A picture whose PES carried no PTS (`pts_known` false — a
    /// PAFF source remuxed one field to a PES) takes the previous picture's
    /// time, decode and composition alike, since it belongs to its frame;
    /// before any picture has had one there is none to take, and `None`.
    ///
    /// The decode time goes at least a ms past the last picture's: a jump
    /// joining the other track's offset can step it back up to the
    /// timeline's 1 s own window, and FLV's never does. The picture still
    /// presents at its own time when that is later; on a clean source the
    /// hold never engages, and the tag is what [`flv_video_time`] made of
    /// the raw timestamps.
    ///
    /// A picture coming back on another clock after the video was taken for
    /// gone — the audio ran on past it alone (a switch to an audio-only
    /// backup, a video PID that stopped), and silence may have followed —
    /// opens an offset where the audio is now, in the relation the picture
    /// last had to it ([`CmafTimeline::continue_at`]), and its audio joins
    /// it beside the audio already sent. Continued from its old picture, a
    /// picture coming back ahead of its audio went one frame past that, as
    /// far behind the audio as the video had been away; its audio joined it
    /// there, and was dropped as behind what had been sent until the source
    /// had run that far again. Audio coming back first was placed beside
    /// the silence, as it still is; and a picture the timeline places — on
    /// the clock it left, or the old picture the demuxer hands out beside
    /// the first after a stall — at its own time.
    fn video(&mut self, pts: u64, dts: Option<u64>, pts_known: bool) -> Option<PictureTime> {
        if !pts_known {
            return self.last_picture;
        }
        if self.live_picture().is_none()
            && let Some(audio) = self.newest_audio
        {
            self.source.continue_at(Track::Video, audio.saturating_add_signed(self.picture_lead) & MASK_33);
        }
        let offset = dts.map_or(0, |d| super::cmaf::composition_offset_90k(pts, d));
        let decoded = self.map(Track::Video, pts.wrapping_sub(offset) & MASK_33);
        let presented = decoded.saturating_add(offset);
        let dts = self.last_picture.map_or(decoded, |p| decoded.max(p.dts.saturating_add(FLV_MS_90K)));
        let (ts_ms, cts_ms) = flv_video_time(presented.max(dts), Some(dts), &mut self.epoch);
        self.newest_picture = Some(self.newest_picture.map_or(presented, |n| n.max(presented)));
        if self.live_picture().is_some()
            && let Some(audio) = self.newest_audio
        {
            self.picture_lead = decoded as i64 - audio as i64;
        }
        let time = PictureTime { pts: presented, dts, ts_ms, cts_ms };
        self.last_picture = Some(time);
        Some(time)
    }

    /// One audio frame's time: mapped, anchoring the epoch when it is the
    /// connection's first essence, whether or not the frame is sent.
    fn audio(&mut self, pts: u64) -> u64 {
        let at = self.map(Track::Audio, pts);
        self.epoch.get_or_insert(at);
        self.newest_audio = Some(self.newest_audio.map_or(at, |n| n.max(at)));
        self.last_real_audio = Some((pts & MASK_33, at));
        at
    }

    /// Silence laid at `at` with no picture to lay it to, put on the audio
    /// track as the source's clock would have stamped it, run on from the
    /// last real frame. The timeline then judges the audio's return against
    /// where the silence ends: on the same clock it lands there, on another
    /// it is continued one frame past it, and a picture coming back ahead of
    /// it is put beside it ([`Self::video`]). Left off the track, a return the
    /// timeline continues (past the lone gap, or onto another clock) would
    /// go one frame past the last *real* frame, behind the silence by as long
    /// as it had run, and every frame of it be held back or dropped as
    /// behind what had been sent. Silence behind what is mapped already is
    /// left off: the timeline is not stepped back for it. Never anchors the
    /// epoch.
    fn silent_audio(&mut self, at: u64) {
        let Some((src, real_at)) = self.last_real_audio else {
            return;
        };
        if self.newest_audio.is_some_and(|n| at <= n) {
            return;
        }
        let mapped = self.map(Track::Audio, src.wrapping_add(at.wrapping_sub(real_at)) & MASK_33);
        self.newest_audio = Some(self.newest_audio.map_or(mapped, |n| n.max(mapped)));
    }

    /// The FLV ms of `at`, a time on this timeline.
    fn tag_ms(&mut self, at: u64) -> u32 {
        rel_ms_monotonic(at, *self.epoch.get_or_insert(at))
    }

    /// The FLV ms of an audio tag at `at`, or `None` to drop it: at or behind
    /// the last audio tag sent (a loop's or a switch's overlap, which would
    /// step the audio track back), or before the epoch. Audio has no
    /// composition time, so a frame before the epoch has no place on the
    /// timeline: stamped 0, as every such frame was, the transcoded audio
    /// of a connection whose first essence was a picture put ~640 ms of
    /// MP2 / AC-3 at one instant. Every audio tag goes through here —
    /// passthrough, transcoded and silent.
    fn audio_tag_ms(&mut self, at: u64) -> Option<u32> {
        if self.last_audio.is_some_and(|l| at <= l) || self.epoch.is_some_and(|e| at < e) {
            return None;
        }
        self.last_audio = Some(at);
        Some(self.tag_ms(at))
    }

    /// Whether an essence has anchored the epoch.
    fn anchored(&self) -> bool {
        self.epoch.is_some()
    }

    fn newest_picture(&self) -> Option<u64> {
        self.newest_picture
    }

    /// The newest picture, unless the audio has run on past it by the
    /// timeline's cross window, which then takes the video for quiet: the
    /// video has gone (a switch to an audio-only backup, a video PID that
    /// stops) while the audio carried on. Silence laid to such a picture
    /// would stop short of where the audio already is, and none be laid at
    /// all. A source stopping whole leaves its audio near its picture, and
    /// that picture still stops the silence.
    fn live_picture(&self) -> Option<u64> {
        let window = super::cmaf::timeline::CROSS_WINDOW_90K as u64;
        self.newest_picture.filter(|&v| self.newest_audio.is_none_or(|a| a <= v.saturating_add(window)))
    }

    fn newest_audio(&self) -> Option<u64> {
        self.newest_audio
    }
}

/// Real audio at mapped time `at` reached the output: the silence watchdog
/// is told, so no silence is laid while it flows, and the picture's lead
/// over it measured for silence to stop short by — unless the encoder's
/// input is already past it, a frame [`submit_source_audio`] holds back.
/// The last MP2 / AC-3 PES before an outage, handed out beside the first
/// after it, sat the outage's length behind the picture: taken for the
/// lead, it held the next outage's silence that far short of the picture,
/// an unfilled hole as long again.
fn note_real_audio(
    silence: Option<&mut SilenceGenerator>,
    input: &mut EncoderInputClock,
    at: u64,
    picture: Option<u64>,
) {
    if let Some(sg) = silence {
        sg.mark_real_audio(at);
    }
    if !input.passed(at) {
        input.mark_real_audio(at, picture);
    }
}

/// Submit one decoded source frame (`planar` at `rate`, from mapped time
/// `at`) at its place on the encoder's input timeline: re-anchoring the
/// encoder where the source left it, covering a gap with zeros under
/// `silent_fallback` (`fill_gaps`), holding back a frame the input is
/// already past — the rule CMAF's re-encoder applies
/// ([`EncoderInputClock::place`]), but for a frame behind the input by
/// more than the fill cap, which is held back too rather than re-anchored
/// on ([`EncoderInputClock::place_forward_only`]): FLV's audio never steps
/// back. Beside a picture that is the last MP2 / AC-3 PES before an
/// outage, which the demuxer hands out only beside the first after it, its
/// place long since laid with silence. The encoder used to anchor on its first
/// frame and count samples for the life of the connection, so a loop
/// whose audio and video lengths differ drifted A/V by the difference every
/// lap, and a switch kept the old feed's A/V relation.
#[allow(clippy::too_many_arguments)]
fn submit_source_audio(
    encoder: &mut AudioEncoder,
    stage: &mut super::audio_transcode::EncoderStage,
    input: &mut EncoderInputClock,
    fill_gaps: bool,
    planar: &[Vec<f32>],
    rate: u32,
    at: u64,
    output_id: &str,
) {
    let n = planar.first().map_or(0, Vec::len) as u64;
    if n == 0 {
        return;
    }
    input.set_rate(rate);
    match input.place_forward_only(at, fill_gaps) {
        Placement::Skip => return,
        Placement::Submit { reanchor: true } => {
            encoder.reanchor_pts();
            input.anchor(at, rate);
        }
        Placement::FillThenSubmit { samples } => {
            // Up to the fill cap (4 s, ~190 AAC frames) encoded at once: off
            // the async task, as CMAF's encoding is.
            let zeros = vec![vec![0.0f32; samples as usize]; planar.len()];
            let filled = crate::timed_block_in_place!(
                "output_rtmp.audio_gap_fill",
                crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
                { encoder.submit_through(stage, &zeros, rate, input.position().unwrap_or(at)) }
            );
            match filled {
                Ok(true) => input.took(samples),
                _ => input.refused(),
            }
        }
        Placement::Submit { reanchor: false } => {}
    }
    match encoder.submit_through(stage, planar, rate, input.position().unwrap_or(at)) {
        Ok(true) => input.took(n),
        Ok(false) => input.refused(),
        Err(e) => {
            tracing::debug!("RTMP output '{output_id}': transcode failed: {e}");
            input.refused();
        }
    }
}

/// Lay silence while the watchdog says real audio is absent: up to the
/// newest picture less the video's lead over the audio, as CMAF's
/// `encode_silence_if_needed` does. The tick is only the watchdog, so a late
/// tick costs nothing and a stalled picture stops the silence; at most
/// [`SILENCE_PER_TICK_90K`] a tick, so a long pause is caught up over a few.
/// The fill reaches as far as the timeline keeps a lone track's pause
/// ([`FLV_LONE_GAP_90K`]), not CMAF's 4 s: a kept step past the fill's
/// reach would re-anchor the silence at the picture with nothing laid, and
/// a video-only source with a picture every 5 s would send no audio at
/// all. With no picture, or one the audio has run on past
/// ([`FlvTimeline::live_picture`]), one chunk a tick from the encoder's
/// input, else from the newest audio mapped, and put on the timeline's audio
/// track ([`FlvTimeline::silent_audio`]) so the audio returning is placed
/// after it. Never before an essence has anchored the FLV epoch.
///
/// Silence laid beside a picture stays off the audio track: the picture
/// already says where the audio is. Audio coming back behind it — the last
/// MP2 / AC-3 PES before an outage, which the demuxer hands out only beside
/// the first after it — is mapped at its own time and held back however
/// far behind ([`submit_source_audio`]). Behind the silence on the track,
/// with no picture, that PES cannot be told from audio whose timestamps
/// close the outage (the input's PTS rewriter bridges an audio-only feed's
/// gap the wall clock did not see), which must not be held back: it is
/// continued one frame past the silence and sent, and audio returning after
/// it on its own clock goes back there, at its own time (a lone track's
/// gap, `CmafTimeline::map`).
///
/// The generator's own PTS starts at 0 and is not used: submitted at it, the
/// first silent chunk anchored the encoder at 0 once a picture had anchored
/// the epoch, and every silent tag went out at 0 for as long as the source
/// PTS stood above 0 — on a video-only source always.
fn lay_silence(
    encoder: &mut AudioEncoder,
    sg: &mut SilenceGenerator,
    input: &mut EncoderInputClock,
    timeline: &mut FlvTimeline,
    output_id: &str,
) {
    if !timeline.anchored() || !sg.should_emit() {
        return;
    }
    let rate = encoder.params().sample_rate;
    let chunk = sg.chunk_samples() as u64;
    let starting = |sg: &SilenceGenerator| {
        if !sg.is_emitting() {
            tracing::info!(
                "RTMP output '{output_id}': source audio absent/stalled — starting silent-AAC injection"
            );
        }
    };
    match timeline.live_picture() {
        Some(video) => {
            let target = input.silence_target(video);
            if input.silence_start_within(target, rate, FLV_LONE_GAP_90K) {
                encoder.reanchor_pts();
            }
            let end = input.position().map_or(target, |at| target.min(at.saturating_add(SILENCE_PER_TICK_90K)));
            while let Some(at) = input.next_silence(chunk, end) {
                starting(sg);
                let (planar, _) = sg.next_chunk();
                if !encoder.submit_planar(planar, at) {
                    input.refused();
                    break;
                }
                input.took(chunk);
                input.mark_silence();
            }
        }
        None => {
            let Some(start) = input.position().or(timeline.newest_audio()) else {
                return;
            };
            if input.silence_start(start, rate) {
                encoder.reanchor_pts();
            }
            let Some(at) = input.position() else {
                return;
            };
            starting(sg);
            let (planar, _) = sg.next_chunk();
            if encoder.submit_planar(planar, at) {
                input.took(chunk);
                input.mark_silence();
                timeline.silent_audio(at);
            } else {
                input.refused();
            }
        }
    }
}

/// Send what `encoder` has encoded, each frame at its own stamp on the
/// connection's timeline ([`FlvTimeline::audio_tag_ms`], which drops a frame
/// at or behind one already sent). The AAC sequence header — the encoder's
/// target format, its rate's ADTS index, else `fallback_sr_index` — goes out
/// with the first frame, at its timestamp: the encoder's first frame is
/// stamped its codec delay before the source time that produced it, so a
/// header sent at that source time stepped the audio track back.
async fn send_encoded_audio(
    client: &mut RtmpClient,
    encoder: &mut AudioEncoder,
    timeline: &mut FlvTimeline,
    sent_audio_header: &mut bool,
    fallback_sr_index: Option<u8>,
    stats: &OutputStatsAccumulator,
    recv_time_us: Option<u64>,
) -> anyhow::Result<()> {
    let header = (!*sent_audio_header)
        .then(|| {
            let p = encoder.params();
            sr_index_from_hz(p.target_sample_rate)
                .or(fallback_sr_index)
                .map(|i| build_aac_sequence_header(1, i, p.target_channels))
        })
        .flatten();
    // The encoder's stamps are already on the timeline: it was anchored on
    // mapped times.
    for frame in encoder.drain() {
        if !*sent_audio_header && header.is_none() {
            // No header to announce the format, so no frame, as before.
            continue;
        }
        let Some(ts_ms) = timeline.audio_tag_ms(frame.pts) else {
            continue;
        };
        if !*sent_audio_header && let Some(h) = &header {
            client.send_audio(h, ts_ms).await?;
            *sent_audio_header = true;
            tracing::debug!("RTMP output '{}': sent AAC sequence header (re-encoded)", timeline.output_id);
        }
        let tag = build_aac_raw_tag(&frame.data);
        let tag_len = tag.len();
        client.send_audio(&tag, ts_ms).await?;
        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        stats.bytes_sent.fetch_add(tag_len as u64, Ordering::Relaxed);
        if let Some(r) = recv_time_us {
            stats.record_latency(r);
        }
    }
    Ok(())
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
/// The inputs come from [`FlvTimeline::video`], already unwrapped. The decode
/// time is the PTS less the composition offset that
/// [`super::cmaf::composition_offset_90k`] reads off the PES's DTS, which is
/// the judgement CMAF passthrough makes: a DTS after the PTS, or more than 2 s
/// before it, counts as none. With no DTS (a stream without B-frames, every
/// non-reference B-picture) the result is exactly the PTS mapping, with
/// composition time 0, byte for byte what went out before.
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
/// - The decode time saturates at 0 rather than wrapping, a second line of
///   defence behind the unwrapped timeline. The subtraction is in 64 bits, so
///   a picture straddling the FLV clock's own 2^32 ms wrap keeps its
///   composition time.
fn flv_video_time(pts: u64, dts: Option<u64>, base_pts: &mut Option<u64>) -> (u32, u32) {
    let offset = dts.map_or(0, |d| super::cmaf::composition_offset_90k(pts, d));
    let dts = pts.saturating_sub(offset);
    let base = *base_pts.get_or_insert(dts);
    let dts_ms = dts.saturating_sub(base) / 90;
    let pts_ms = pts.saturating_sub(base) / 90;
    (dts_ms as u32, pts_ms.saturating_sub(dts_ms) as u32)
}

/// A time on the connection's [`FlvTimeline`] (unwrapped, 64 bits) → FLV
/// milliseconds relative to the epoch.
///
/// No wrap heuristic is wanted here: the timeline has already unwrapped the
/// source's 33-bit clock. Applying one would cap the output at 2^33 ticks
/// (~26.5 h) and then pin every later tag to 0. A value behind the epoch (the
/// other essence anchored first) clamps to 0. The `as u32` truncates modulo
/// 2^32 ms (49.7 days), as FLV's own clock wraps.
fn rel_ms_monotonic(v: u64, base: u64) -> u32 {
    (v.saturating_sub(base) / 90) as u32
}

/// Wire PTS (90 kHz) for one encoded video frame.
///
/// FLV carries DTS in the tag timestamp and RTMP servers drop a publisher whose
/// DTS goes backwards, so the result is forced strictly increasing. Its input
/// is on the connection's FLV timeline, where a forward step is a gap both
/// tracks keep, a video pause the audio ran through, or a lone track's pause,
/// so it is honoured: the transcoded audio follows the same timeline. A step
/// back (an offset join inside the timeline's 1 s window) or a repeated stamp
/// is held a millisecond on per frame until the source passes it, which takes
/// ⌈Δ / (frame − 1 ms)⌉ frames. Substituting a whole frame (`last + step`), as
/// this used to, kept the video a step ahead of the audio for good, and
/// clamping a forward step to one split them at every gap.
fn next_wire_pts_90k(queue: &mut std::collections::VecDeque<u64>, last: Option<u64>) -> u64 {
    let raw = queue.pop_front().unwrap_or(0);
    last.map_or(raw, |l| raw.max(l.saturating_add(FLV_MS_90K)))
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
    /// `is_keyframe`: the demuxer's IDR flag, widened in `publish_loop` to an
    /// intra picture with a recovery-point SEI (edge#211).
    H264 { nalus: &'a [Vec<u8>], is_keyframe: bool, pts_known: bool },
    H265 { nalus: &'a [Vec<u8>], is_keyframe: bool, pts_known: bool },
    /// An MPEG-2 video access unit (the ES as the PES carried it): RTMP
    /// carries H.264 / HEVC, so it reaches the wire only re-encoded. It used
    /// to be dropped whatever `video_encode` said, silently.
    #[cfg_attr(not(feature = "media-codecs"), allow(dead_code))]
    Mpeg2 { es: &'a [u8], is_keyframe: bool, pts_known: bool },
}

impl<'a> VideoFrameSource<'a> {
    /// Whether a receiver can start decoding at this access unit: what the FLV
    /// frame type says and where passthrough re-sends the sequence header. For
    /// H.264 that includes recovery points (see `publish_loop`); read once per
    /// access unit, by `passthrough_video`. The re-encode takes keys from its
    /// encoder.
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
    // The picture's time on the connection's FLV timeline
    // (`FlvTimeline::video`): `None` for an access unit whose PES carried no
    // PTS, before any picture that did. Passthrough writes its decode and
    // composition times; the re-encode feeds its presentation time to the
    // decoder and stamps what comes out on `timeline`.
    time: Option<PictureTime>,
    timeline: &mut FlvTimeline,
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
            passthrough_video(&src, time, recv_time_us, passthrough, client, config, stats).await
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
            encode_one_frame(&src, time, timeline, recv_time_us, video_state, client, config, stats, flow_id, event_sender).await
        }
        #[cfg(feature = "media-codecs")]
        VideoEncoderState::Active(_) => {
            encode_one_frame(&src, time, timeline, recv_time_us, video_state, client, config, stats, flow_id, event_sender).await
        }
    }
}

/// Pictures held for want of a sequence header before the output says why,
/// once per connection: 20 s at 50 fps, 40 s at 25 — past any GOP a
/// broadcast source keeps.
const HELD_PICTURES_WARN: u32 = 1_000;

/// What passthrough has told the receiver about the source's video on this
/// connection. A local of `publish_loop`, so each connection starts afresh.
/// The connection's timing is not here: every tag's time comes off its
/// [`FlvTimeline`], which the re-encode shares.
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
    time: Option<PictureTime>,
    recv_time_us: u64,
    passthrough: &mut PassthroughVideo,
    client: &mut RtmpClient,
    config: &RtmpOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
) -> anyhow::Result<bool> {
    // One flag drives the FLV frame type, the table's random-access rule and
    // the key-picture re-send: the demuxer's, which for H.264 `publish_loop`
    // has widened to recovery points.
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
    // A picture whose PES carried no PTS, before any picture that did, has no
    // time to go out at (edge#211: it went out at 0). Its parameter sets are
    // in the table now; the picture is dropped, before a header is taken for
    // it.
    let Some(PictureTime { ts_ms, cts_ms, .. }) = time else {
        return Ok(true);
    };
    // The first sequence header goes out ahead of the first picture whose
    // parameter sets are in the table, not on the first keyframe: a source
    // that marks random access in neither way `is_keyframe` reads — an
    // intra-refresh encoder, or an open GOP whose recovery-point SEIs were
    // stripped upstream — would otherwise never start video. A receiver given
    // a non-key first picture waits for the next key; the header is what it
    // needs to open its decoder. A picture nothing describes yet is dropped.
    let Some((header, copies)) = passthrough.take_headers(codec, is_keyframe) else {
        passthrough.hold(codec, &config.id);
        return Ok(true);
    };
    // The header is re-sent at each random-access point — an IDR or IRAP, or
    // an H.264 intra picture a recovery-point SEI marks (edge#211) — where a
    // receiver joins, cuts or opens a GOP-cache entry. On an open GOP the
    // leading pictures after it still reference the previous GOP; a receiver
    // that re-opened its decoder on every header would lose them, but ffmpeg
    // keeps an identical SPS (its decoded output is unchanged by the re-send),
    // and HEVC CRA has had the same re-send all along.
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
    time: Option<PictureTime>,
    // The connection's FLV timeline, shared with the audio: the source
    // picture's time comes off it, and each encoded frame is stamped on it.
    timeline: &mut FlvTimeline,
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
        {
            transcode_access_unit(
                active,
                &annex_b,
                time.map_or(0, |t| t.pts),
                src.pts_known() && time.is_some(),
                &config.id,
            )
        }
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

    // FLV timestamps come from the SOURCE clock, on the connection's
    // timeline, popped one per emitted frame from the FIFO the decode loop
    // filled. An older form derived them from the encoder's frame index and
    // the *declared* rate, so video ran on the configured fps while
    // transcoded audio ran on the source clock — the two diverged at the rate
    // ratio, unbounded and with no resync.
    //
    // Monotonicity, which that form got for free, is kept by
    // `next_wire_pts_90k`: DTS is forced strictly increasing (RTMP servers
    // drop a publisher whose DTS goes backwards). A forward step is kept: the
    // timeline has judged it, and the audio follows the same timeline.
    for (annex_b_encoded, keyframe, _enc_pts) in encoded {
        let wire_pts = next_wire_pts_90k(&mut active.src_pts_queue, active.last_wire_pts_90k);
        active.last_wire_pts_90k = Some(wire_pts);
        // Anchors the shared FLV epoch if this is the first essence to arrive.
        let frame_ts_ms = timeline.tag_ms(wire_pts);
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
        // On the connection's timeline: `pts_90k` is the picture's
        // presentation time there (unwrapped), or the previous picture's for
        // an access unit whose PES carried none, and the stamper's values —
        // masked to 33 bits — are unwrapped against it.
        let src_pts_for_frame = match stamped {
            Some(s) => unwrap_near(s, pts_90k),
            None if pts_known => pts_90k,
            // No picture has carried a time yet: nothing to stamp it with.
            None => continue,
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
        input: EncoderInputClock::default(),
    }
}

/// Silence-tick handler: if the drop watchdog says real audio is missing,
/// lay silence up to where the audio would be beside the picture
/// ([`lay_silence`]), then send what the encoder has ready as FLV audio
/// tags, the AAC sequence header ahead of the first.
///
/// This is idempotent and cheap when no silence is needed: it
/// short-circuits on `should_emit() == false`. Silence never *anchors* the
/// connection's epoch, only reads it: until a real essence has, there is
/// nothing for silence to be in sync with, so an output connecting before
/// any essence arrives emits no audio tag until the first access unit — the
/// right trade, since a video-only opening is universally tolerated and a
/// split A/V timeline is not.
async fn emit_silence_if_needed(
    config: &RtmpOutputConfig,
    client: &mut RtmpClient,
    encoder_state: &mut EncoderState,
    sent_audio_header: &mut bool,
    stats: &Arc<OutputStatsAccumulator>,
    timeline: &mut FlvTimeline,
) -> anyhow::Result<()> {
    let EncoderState::Active {
        encoder,
        silence: Some(sg),
        input,
        ..
    } = encoder_state
    else {
        return Ok(());
    };
    // Up to a second of AAC encoded at once, off the async task as CMAF's
    // silence is; skipped while real audio is flowing, so that the hand-off
    // is paid only on the ticks that may lay something.
    if sg.should_emit() {
        crate::timed_block_in_place!(
            "output_rtmp.audio_silence_encode",
            crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
            { lay_silence(encoder, sg, input, timeline, &config.id) }
        );
    }
    send_encoded_audio(client, encoder, timeline, sent_audio_header, None, stats, None).await
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
        input: EncoderInputClock::default(),
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

    /// The re-encoder is fed each picture's time on the connection's FLV
    /// timeline, unwrapped, which runs on past 2^33 as the connection does:
    /// the decoder echoes it, the stamper masks it to 33 bits, and the queue
    /// unwraps it against the picture's own time again. Each queued time
    /// steps +3 600 across 2^34 (a 33-bit wrap); taken masked, the wire
    /// time would step back 26.5 h there.
    #[test]
    fn a_re_encode_across_the_33_bit_wrap_queues_unwrapped_times() {
        let cfg: VideoEncodeConfig =
            serde_json::from_value(serde_json::json!({"codec": "x264", "preset": "veryfast"})).unwrap();
        let config: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k"
        }))
        .unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "rtmp".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let aus = x264_test_source(40, 3_600);
        let mut state = open_video_active(&cfg, video_codec::VideoCodec::H264, &aus[0].0, &config, &stats, &events);
        const UP: u64 = 1 << 34;
        for (k, (au, _)) in aus.iter().enumerate() {
            let VideoEncoderState::Active(active) = &mut state else { panic!("not active") };
            transcode_access_unit(active, au, UP - 72_000 + k as u64 * 3_600, true, "o").unwrap();
        }
        let VideoEncoderState::Active(active) = &state else { panic!("not active") };
        let queued: Vec<u64> = active.src_pts_queue.iter().copied().collect();
        assert!(queued.windows(2).all(|w| w[1] == w[0] + 3_600), "{queued:?}");
        assert!(queued[0] < UP && *queued.last().unwrap() > UP, "across the wrap: {queued:?}");
    }

    /// A re-encoded loop whose audio (85 frames, 1 813 ms) is shorter than
    /// its video (48 pictures, 1 920 ms), the audio meeting the loop first:
    /// the audio goes on one frame on, and the video joins it 6 000 ticks
    /// behind its last picture. The re-encode's wire time is held a ms on
    /// for its first two pictures and then rejoins the source's: from the
    /// third picture of the second lap every one is on its source time. The wire
    /// time used to substitute a whole frame at a step back and run on from
    /// there, a frame ahead of the audio for good.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_re_encode_rejoins_the_source_clock_after_a_loop_step_back() {
        use super::flv_wire_tests::{Lap, programme_ts, publish, tag_cts};
        use crate::engine::rtmp::ts_mux::TsMuxer;
        const T0: u64 = 900_000;
        let aus = x264_test_source(48, 3_600);
        assert_eq!(aus[0].1, T0);
        let pics: Vec<(u64, Option<u64>)> = aus.iter().map(|a| (a.1, None)).collect();
        // Audio first: its first frame is muxed ahead of the second picture,
        // and anchors the epoch.
        let a0 = T0 + 1_800;
        let audio: Vec<u64> = (0..85).map(|j| a0 + j * 1_920).collect();
        let laps = [Lap::new(pics.clone(), audio.clone()), Lap::new(pics.clone(), audio.clone())];
        let au = |_: bool, k: usize| aus[k].0.clone();
        let tags = publish(
            programme_ts(&mut TsMuxer::new(), &au, &laps, &[0]),
            serde_json::json!({
                "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k",
                "video_encode": { "codec": "x264", "preset": "veryfast", "fps_num": 25, "fps_den": 1 }
            }),
        )
        .await;
        let pictures: Vec<u32> =
            tags.iter().filter(|t| t.video).filter_map(|t| Some(t.ts + tag_cts(&t.data)?)).collect();
        assert_eq!(pictures.len(), 96, "every picture re-encoded");
        assert!(pictures.windows(2).all(|p| p[1] > p[0]), "{pictures:?}");
        let ms = |t: u64| (t.saturating_sub(a0) / 90) as u32;
        let shift = 85 * 1_920;
        for (k, &(pts, _)) in pics.iter().enumerate() {
            assert_eq!(pictures[k], ms(pts), "lap 1, picture {k}");
            if k >= 2 {
                assert_eq!(pictures[48 + k], ms(pts + shift), "lap 2, picture {k}: on the source's time");
            }
        }
        assert_eq!(pictures[48], ms(pics[47].0) + 1, "lap 2's first picture held a ms on");
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
            let p = next_wire_pts_90k(&mut q, last);
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
            let p = next_wire_pts_90k(&mut q, last);
            if i > 0 {
                assert!(p > prev, "DTS must not stall or go backwards");
            }
            prev = p;
            last = Some(p);
        }
    }

    /// `next_wire_pts_90k` over `src`, from a fresh connection.
    fn wire_pts(src: &[u64]) -> Vec<u64> {
        let mut q: VecDeque<u64> = src.iter().copied().collect();
        let mut last = None;
        (0..src.len())
            .map(|_| {
                let p = next_wire_pts_90k(&mut q, last);
                last = Some(p);
                p
            })
            .collect()
    }

    /// A source stamp behind the last one sent (an offset join inside the
    /// timeline's 1 s own window), or repeating it, is held a millisecond on
    /// per frame until the source passes it, and the source's own stamps
    /// then go out as they are. Substituting a whole frame, as this did, kept
    /// the video a frame ahead of the audio for good.
    #[test]
    fn a_backwards_source_pts_is_held_until_the_source_passes() {
        assert_eq!(wire_pts(&[100_000, 50_000]), [100_000, 100_090], "never emit a backwards DTS");
        const X: u64 = 900_000;
        assert_eq!(wire_pts(&[X, X - 2_400, X + 1_200, X + 4_800]), [X, X + 90, X + 1_200, X + 4_800]);
        assert_eq!(wire_pts(&[X, X, X + 3_600]), [X, X + 90, X + 3_600], "a repeated stamp");
    }

    /// A forward step is kept, however long. Its input is on the FLV
    /// timeline, which has already judged it: a gap both tracks keep, a
    /// video pause the audio ran through, or a lone track's pause — and the
    /// transcoded audio now follows that timeline. Clamping it to one step,
    /// as this did while the audio could not follow a jump, split the video
    /// from the audio at every such gap.
    #[test]
    fn a_forward_step_on_the_flv_timeline_is_kept() {
        assert_eq!(wire_pts(&[0, 90_000 * 60]), [0, 90_000 * 60]);
    }

    #[test]
    fn a_gap_within_tolerance_is_honoured() {
        // A real one-second gap passes through.
        assert_eq!(wire_pts(&[0, 90_000]), [0, 90_000]);
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
    /// audio at that instant is stamped (`FlvTimeline::tag_ms`).
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
    /// 0 — byte for byte what went out before edge#205. A picture whose PES
    /// carried no PTS no longer reaches `flv_video_time` (it takes its
    /// predecessor's time, `FlvTimeline::video`); a PTS of 0 behind an epoch
    /// clamps to 0.
    #[test]
    fn a_source_without_b_frames_is_tagged_exactly_as_before() {
        for with_dts in [false, true] {
            let (mut base, mut before) = (None, None);
            for k in 0..50u64 {
                let pts = T0 + k * 3003;
                let got = flv_video_time(pts, with_dts.then_some(pts), &mut base);
                assert_eq!(got, (rel_ms_monotonic(pts, *before.get_or_insert(pts)), 0), "frame {k}");
            }
            assert_eq!(base, before);
        }
        assert_eq!(flv_video_time(0, None, &mut None), (0, 0));
        let mut base = Some(T0);
        assert_eq!(flv_video_time(0, None, &mut base), (0, 0));
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
                assert_eq!(ts + cts, rel_ms_monotonic(pts, a0));
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
        assert_eq!(rel_ms_monotonic(pts, base.unwrap()), ts + cts, "audio at the same instant");
        assert_eq!(rel_ms_monotonic(pts + 1920, base.unwrap()), 101);
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
        assert_eq!(rel_ms_monotonic(50_000, base.unwrap()), 555, "audio after it is not pinned to 0");

        let mut base = Some(1_000);
        assert_eq!(flv_video_time(3_000, Some(WRAP - 500), &mut base), (0, 22));
        assert_eq!(flv_video_time(6_600, Some(3_100), &mut base), (23, 39));
    }

    /// FLV's own clock wraps at 2^32 ms (49.7 days). A picture decoding just
    /// before that and presenting just after keeps its composition time: the
    /// subtraction is done before the truncation, where it used to give 0.
    #[test]
    fn a_picture_straddling_the_u32_ms_wrap_keeps_its_composition_time() {
        const E: u64 = 1 << 34;
        let mut base = Some(E);
        let (pts, dts) = (E + ((1 << 32) + 23) * 90, E + ((1 << 32) - 10) * 90);
        assert_eq!(flv_video_time(pts, Some(dts), &mut base), (u32::MAX - 9, 33));
    }

    // ── edge#211: one FLV timeline per connection ─────────────────────────

    const WRAP: u64 = 1 << 33;

    /// `pics` (decode order) and `audio` stamps in arrival order — each
    /// picture at its decode time, each audio frame at its stamp, a picture
    /// first at a tie: `(is_video, index)`.
    fn interleave(pics: &[(u64, Option<u64>)], audio: &[u64]) -> Vec<(bool, usize)> {
        let mut ev: Vec<(u64, bool, usize)> = pics
            .iter()
            .enumerate()
            .map(|(k, &(p, d))| (d.unwrap_or(p), true, k))
            .chain(audio.iter().enumerate().map(|(j, &a)| (a, false, j)))
            .collect();
        ev.sort_by_key(|&(t, video, _)| (t, !video));
        ev.into_iter().map(|(_, video, k)| (video, k)).collect()
    }

    /// An AAC frame's FLV ms through the timeline, `None` when dropped.
    fn audio_ms(tl: &mut FlvTimeline, a: u64) -> Option<u32> {
        let at = tl.audio(a);
        tl.audio_tag_ms(at)
    }

    /// A clean source — B-frames, AAC beside them, whichever essence
    /// anchors the epoch — goes out exactly as `flv_video_time` and the
    /// shared epoch tagged it before the timeline: the timeline adds an
    /// offset of 0 and a lap, and differences are all FLV reads.
    #[test]
    fn a_clean_source_is_tagged_exactly_as_before() {
        let pics = ibbp(60, |k| T0 + k * STEP_25FPS);
        for a0 in [T0 - 3_000, T0 + 1_000] {
            let audio: Vec<u64> = (0..120).map(|j| a0 + j * 1_920).collect();
            let mut tl = FlvTimeline::new("o");
            let mut raw = None;
            for (video, k) in interleave(&pics, &audio) {
                if video {
                    let (pts, dts) = pics[k];
                    let t = tl.video(pts, dts, true).expect("a timed picture");
                    assert_eq!((t.ts_ms, t.cts_ms), flv_video_time(pts, dts, &mut raw), "a0 {a0}: picture {k}");
                } else {
                    let a = audio[k];
                    let e = *raw.get_or_insert(a);
                    assert_eq!(audio_ms(&mut tl, a), Some(((a - e) / 90) as u32), "a0 {a0}: audio {k}");
                }
            }
        }
    }

    /// edge#211 item 4: the source's 33-bit PTS wraps every 26.5 h, and every
    /// tag after it went out at 0 until the PTS climbed back past the epoch.
    /// The timeline unwraps it: audio counts on, B-frames stay in decode
    /// order with their composition times, and a picture decoding just
    /// behind an epoch audio set just past the wrap is still at 0.
    #[test]
    fn the_33_bit_wrap_keeps_counting() {
        let mut tl = FlvTimeline::new("o");
        let got: Vec<Option<u32>> = [WRAP - 90_000, 0, 90_000].iter().map(|&a| audio_ms(&mut tl, a)).collect();
        assert_eq!(got, [Some(0), Some(1_000), Some(2_000)]);

        let at = |k: u64| WRAP - 6 * STEP_25FPS + k * STEP_25FPS;
        let pics = ibbp(24, at);
        let mut tl = FlvTimeline::new("o");
        let d0 = pics[0].1.unwrap();
        let mut last = None;
        for (k, &(pts, dts)) in pics.iter().enumerate() {
            let t = tl.video(pts & MASK_33, dts.map(|d| d & MASK_33), true).unwrap();
            let decode = dts.unwrap_or(pts);
            assert_eq!(t.ts_ms, ((decode - d0) / 90) as u32, "picture {k}: its decode time");
            assert_eq!(t.ts_ms + t.cts_ms, ((pts - d0) / 90) as u32, "picture {k}: presented at its PTS");
            assert!(last.is_none_or(|l| t.ts_ms > l), "picture {k}");
            last = Some(t.ts_ms);
        }
        assert!(pics.iter().any(|p| p.0 > WRAP) && pics.iter().any(|p| p.0 < WRAP), "across the wrap");

        let mut tl = FlvTimeline::new("o");
        assert_eq!(audio_ms(&mut tl, 1_000), Some(0));
        let dts = (1_000 + WRAP - STEP_25FPS) & MASK_33;
        let t = tl.video((dts + 3 * STEP_25FPS) & MASK_33, Some(dts), true).unwrap();
        assert_eq!((t.ts_ms, t.cts_ms), (0, 80), "decoding before the epoch, presented on its audio's ms");
    }

    /// The picture straddling the wrap that `flv_video_time` alone could only
    /// saturate (0, 11): on the unwrapped timeline its decode time is just
    /// before the wrap, and its composition time whole.
    #[test]
    fn a_picture_straddling_the_33_bit_wrap_is_tagged_at_its_true_decode_time() {
        let mut tl = FlvTimeline::new("o");
        let t = tl.video(1_000, Some(WRAP - 2_000), true).unwrap();
        assert_eq!((t.ts_ms, t.cts_ms), (0, 33));
        let t = tl.video(4_600, Some(1_600), true).unwrap();
        assert_eq!((t.ts_ms, t.cts_ms), (40, 33));
        assert_eq!(audio_ms(&mut tl, 50_000), Some(577));
    }

    /// A 25 fps picture (no B-frames) every 3 600 ticks of programme time
    /// and an AAC frame every 1 920, over `len` ticks, in arrival order (a
    /// picture first at a tie): `(track, programme time)`.
    fn programme(len: u64) -> Vec<(Track, u64)> {
        let mut ev: Vec<(Track, u64)> = (0..len)
            .step_by(STEP_25FPS as usize)
            .map(|t| (Track::Video, t))
            .chain((0..len).step_by(1_920).map(|t| (Track::Audio, t)))
            .collect();
        ev.sort_by_key(|&(track, t)| (t, track == Track::Audio));
        ev
    }

    /// `programme(len)` through a fresh timeline with each stamp `src(t)`:
    /// `(track, programme time, FLV ms)` — a dropped audio frame `None`.
    fn tag_programme(len: u64, src: impl Fn(Track, u64) -> u64) -> Vec<(Track, u64, Option<u32>)> {
        let mut tl = FlvTimeline::new("o");
        programme(len)
            .into_iter()
            .map(|(track, t)| {
                let ms = match track {
                    Track::Video => tl.video(src(track, t), None, true).map(|p| p.ts_ms),
                    Track::Audio => audio_ms(&mut tl, src(track, t)),
                };
                (track, t, ms)
            })
            .collect()
    }

    /// edge#211 item 3: both tracks jumping 90 s back (a looped MPTS with no
    /// input `program_number`), the video first. Every tag after it used to
    /// step back, or go out at 0. The video goes on one frame on and the
    /// audio takes the same offset: the tags are what a source that never
    /// jumped gets, A/V together.
    #[test]
    fn a_backward_jump_goes_on_one_frame_on_with_the_audio_beside_it() {
        const B: u64 = 200 * 90_000;
        let jump_at = 64 * STEP_25FPS;
        let jumped = tag_programme(10 * 90_000, |_, t| B + t - if t >= jump_at { 90 * 90_000 } else { 0 });
        let clean = tag_programme(10 * 90_000, |_, t| B + t);
        assert_eq!(jumped, clean);
        let video: Vec<u32> = jumped.iter().filter(|e| e.0 == Track::Video).filter_map(|e| e.2).collect();
        assert_eq!(video[64], video[63] + 40, "the first picture after the jump, one frame on");
        assert!(jumped.iter().all(|e| e.2.is_some()), "nothing dropped");
    }

    /// edge#211 item 5: a picture whose PES carried no PTS (a PAFF source
    /// remuxed one field to a PES) was stamped 0 in mid-stream. It takes the
    /// previous picture's time, decode and composition alike; before any
    /// picture has had a time there is none to take.
    #[test]
    fn a_picture_without_a_pts_takes_the_previous_pictures_time() {
        let mut tl = FlvTimeline::new("o");
        assert_eq!(tl.video(0, None, false), None, "nothing to take yet");
        assert!(!tl.anchored(), "and nothing anchored by it");
        let pics = ibbp(6, |k| T0 + k * STEP_25FPS);
        let mut prev = None;
        for &(pts, dts) in &pics {
            let t = tl.video(pts, dts, true);
            assert!(t.is_some());
            assert_eq!(tl.video(0, None, false), t, "the field after it");
            if let (Some(p), Some(t)) = (prev, t) {
                let p: PictureTime = p;
                assert!(t.ts_ms > p.ts_ms, "the next timed picture unaffected");
            }
            prev = t;
        }
        assert_eq!(prev.map(|t| (t.ts_ms, t.cts_ms)), Some((200, 0)));
    }

    /// Both tracks stepping 0.5 s back, inside the timeline's 1 s own window
    /// (a B-frame source's reordering, to the timeline): the decode time
    /// goes a ms on per picture until the source passes it, each picture
    /// presented at the later of its own time and that, then at the source's
    /// times again; the audio behind what was sent is dropped.
    #[test]
    fn a_decode_time_behind_the_last_picture_is_held_a_millisecond_on() {
        let back_at = 50 * STEP_25FPS;
        let out = tag_programme(5 * 90_000, |_, t| T0 + t - if t >= back_at { 45_000 } else { 0 });
        let video: Vec<u32> = out.iter().filter(|e| e.0 == Track::Video).map(|e| e.2.unwrap()).collect();
        assert_eq!(video[49], 1_960);
        // 12 pictures held, then the 13th past the hold at its source time.
        for k in 50..62 {
            assert_eq!(video[k], 1_960 + (k as u32 - 49), "picture {k} held a ms on");
        }
        for k in 62..video.len() {
            assert_eq!(video[k], (k as u32) * 40 - 500, "picture {k} at its source time");
        }
        let audio: Vec<(u64, Option<u32>)> =
            out.iter().filter(|e| e.0 == Track::Audio).map(|e| (e.1, e.2)).collect();
        let last_before = audio.iter().rev().find(|a| a.0 < back_at).unwrap().0;
        let mut dropped = 0;
        for &(t, ms) in &audio {
            let src = t - if t >= back_at { 45_000 } else { 0 };
            if t >= back_at && src <= last_before {
                assert_eq!(ms, None, "audio at {t}: behind what was sent");
                dropped += 1;
            } else {
                assert_eq!(ms, Some((src / 90) as u32), "audio at {t}");
            }
        }
        assert_eq!(dropped, 23, "0.5 s of audio, less the frame it ends inside");
        // The presentation of a held picture: its decode ms, composition 0.
        let mut tl = FlvTimeline::new("o");
        tl.video(T0, None, true);
        let held = tl.video(T0 - 45_000, None, true).unwrap();
        assert_eq!((held.ts_ms, held.cts_ms), (1, 0));
    }

    /// An AAC frame at or behind the last one sent — a loop's or a switch's
    /// overlap — is dropped rather than stepping the audio track back.
    #[test]
    fn audio_at_or_behind_what_was_sent_is_dropped() {
        let mut tl = FlvTimeline::new("o");
        assert_eq!(audio_ms(&mut tl, T0), Some(0));
        assert_eq!(audio_ms(&mut tl, T0 + 1_920), Some(21));
        assert_eq!(audio_ms(&mut tl, T0 + 1_920), None, "repeated");
        assert_eq!(audio_ms(&mut tl, T0), None, "behind");
        assert_eq!(audio_ms(&mut tl, T0 + 3_840), Some(42));
    }

    /// The addendum to edge#211's timeline: a frame of audio stamped before
    /// the epoch has no place on the timeline and is dropped. Every such
    /// frame used to go out at 0: a connection whose first essence was a
    /// picture stacked ~640 ms of transcoded MP2 / AC-3 at one instant. And
    /// the encoder's first frame, stamped its codec delay before the source
    /// frame that anchored the epoch, is dropped rather than put at 0.
    #[test]
    fn audio_before_the_epoch_is_dropped_not_stacked_at_zero() {
        let mut tl = FlvTimeline::new("o");
        tl.video(T0, None, true);
        let e = tl.epoch.unwrap();
        let got: Vec<Option<u32>> =
            [T0 - 3 * 1_920, T0 - 1_920, T0, T0 + 1_920].iter().map(|&a| audio_ms(&mut tl, a)).collect();
        assert_eq!(got, [None, None, Some(0), Some(21)]);
        assert_eq!(tl.epoch, Some(e), "audio does not move the epoch");

        // fdk AAC-LC's 2 048-sample delay at 48 kHz is two frames: those two
        // are dropped, and the third is the source frame's own time.
        let mut tl = FlvTimeline::new("o");
        let a = tl.audio(T0);
        let first = a - 2_048 * 90_000 / 48_000;
        let got: Vec<Option<u32>> = (0..4).map(|n| tl.audio_tag_ms(first + n * 1_920)).collect();
        assert_eq!(got, [None, None, Some(0), Some(21)]);
    }

    /// FLV's clock wraps at 2^32 ms, as any long-running publisher's does,
    /// and the guards compare the timeline's 64-bit values, so the wrap is
    /// no step back to them.
    #[test]
    fn the_flv_clock_wraps_at_2_32_ms_without_tripping_the_guards() {
        let mut tl = FlvTimeline::new("o");
        let e = tl.audio(T0);
        assert_eq!(tl.audio_tag_ms(e + ((1 << 32) - 10) * 90), Some(u32::MAX - 9));
        assert_eq!(tl.audio_tag_ms(e + ((1 << 32) + 5) * 90), Some(5));
    }

    /// A video-only source (nothing else maps a timestamp) at 1 fps keeps its
    /// pauses up to the lone gap — 3 s and 9 s — and continues a step past
    /// it 3 003 ticks on: no step under 500 ms has been seen to continue by.
    #[test]
    fn a_video_only_source_keeps_pauses_up_to_the_lone_gap() {
        let mut tl = FlvTimeline::new("o");
        let mut ts = |t: u64| tl.video(T0 + t, None, true).unwrap().ts_ms;
        for k in 0..5u64 {
            assert_eq!(ts(k * 90_000), k as u32 * 1_000);
        }
        assert_eq!(ts(7 * 90_000), 7_000, "a 3 s pause kept");
        assert_eq!(ts(16 * 90_000), 16_000, "a 9 s pause kept");
        assert_eq!(ts(27 * 90_000), 16_033, "an 11 s step continued 3 003 ticks on");
        assert_eq!(ts(28 * 90_000), 17_033, "and the source's timing after it");
    }

    /// `(handed out at, picture, decode time)` in hand-out order, a picture
    /// first at a tie, as the demuxer hands them out: a picture when the PES
    /// `held` pictures on begins (at its decode time) — 1 for the demuxer's
    /// own hold, one more for every remuxer upstream — an AAC frame on
    /// arrival, `ahead(pts)` before its own time.
    fn hand_out(
        dts: &[u64],
        held: usize,
        audio: impl Iterator<Item = u64>,
        ahead: impl Fn(u64) -> i64,
    ) -> Vec<(i64, bool, u64)> {
        let mut ev: Vec<(i64, bool, u64)> = dts.windows(held + 1).map(|w| (w[held] as i64, true, w[0])).collect();
        ev.extend(audio.map(|a| (a as i64 - ahead(a), false, a)));
        ev.sort_by_key(|&(at, picture, _)| (at, !picture));
        ev
    }

    /// An A/V source with a picture every 2 to 8 s, its AAC running on
    /// beside it, muxed at its own time or 1.1 s either side of the
    /// pictures: each picture handed out when the next one's PES begins,
    /// with the audio up to there already mapped, so the audio has run over
    /// 3 s past the picture before it — the video taken for gone. Each
    /// picture still goes out at its own time, as a pause of the video's
    /// the audio ran through. Moving the video's line on to where the audio
    /// was put every other picture 2 s apart on an offset of its own, 2 s
    /// late, a millisecond before the next; and pictures handed out over
    /// 3 s from the audio were continued one step past the last picture,
    /// or beside the audio, an interval late.
    #[test]
    fn slow_pictures_beside_running_audio_keep_their_own_time() {
        for interval in [180_000, 225_000, 450_000, 720_000] {
            for ahead in [0, 99_000, -99_000] {
                let what = format!("a picture every {interval} ticks, audio {ahead} ahead");
                let mut tl = FlvTimeline::new("o");
                let dts: Vec<u64> = (0..12).map(|k| T0 + k * interval).collect();
                let audio = (0..11 * interval).step_by(1_920).map(|a| T0 + a);
                for (_, picture, t) in hand_out(&dts, 1, audio, |_| ahead) {
                    if picture {
                        let p = tl.video(t, None, true).unwrap();
                        assert_eq!((p.pts, p.dts), (LAP_90K + t, LAP_90K + t), "{what}: the picture at {t}");
                    } else {
                        assert_eq!(tl.audio(t), LAP_90K + t, "{what}: the audio at {t}");
                    }
                }
                assert_eq!(tl.jumps, 0, "{what}: the source never jumped");
            }
        }
    }

    /// A 25 fps source decoding 80 ms before it presents whose video PID
    /// stops for 4 s or 12 s on its own clock while its AAC runs on, muxed
    /// at its own time, 300 ms or 1.1 s ahead of the pictures, or 1.1 s
    /// behind them before the stall and at its own time after it. The
    /// demuxer hands the last picture before the stall out only beside the
    /// first after it, and a remuxer upstream with the same hold one more;
    /// they, and every picture after, go out at their own time. Moving the
    /// video's line on to where the audio was took the old picture for a
    /// jump: an offset of its own, presented after returning pictures
    /// squashed a millisecond apart behind it — and with the audio over a
    /// second off the pictures, the video late for good. With two held back
    /// the audio's run, measured from the pictures before, looked a few
    /// frames long, and the first picture back was put one frame past the
    /// old ones, 4 s early.
    #[test]
    fn a_video_stall_beside_running_audio_keeps_every_picture_at_its_own_time() {
        const STALL: u64 = T0 + 100 * STEP_25FPS;
        for held in [1, 2] {
            for stall in [4 * 90_000, 12 * 90_000] {
                for (what, ahead) in [
                    ("own time", (|_| 0) as fn(u64) -> i64),
                    ("300 ms ahead", |_| 27_000),
                    ("1.1 s ahead", |_| 99_000),
                    ("1.1 s behind, then own time", |a| if a < STALL { -99_000 } else { 0 }),
                ] {
                    let what = format!("{} s stall, {held} held, audio {what}", stall / 90_000);
                    let mut tl = FlvTimeline::new("o");
                    let dts: Vec<u64> = (0..300)
                        .map(|k| T0 + k * STEP_25FPS)
                        .filter(|&d| !(STALL..STALL + stall).contains(&d))
                        .collect();
                    let audio = (0..300 * STEP_25FPS).step_by(1_920).map(|a| T0 + a);
                    for (_, picture, t) in hand_out(&dts, held, audio, ahead) {
                        if picture {
                            let p = tl.video(t + 7_200, Some(t), true).unwrap();
                            let want = (LAP_90K + t + 7_200, LAP_90K + t);
                            assert_eq!((p.pts, p.dts), want, "{what}: the picture decoding at {t}");
                        } else {
                            assert_eq!(tl.audio(t), LAP_90K + t, "{what}: the audio at {t}");
                        }
                    }
                    assert_eq!(tl.jumps, 0, "{what}: the source never jumped");
                }
            }
        }
    }

    /// A source whose video stops while its audio runs on 5 s on the same
    /// clock — one input whose video PID stopped, or an audio-only backup
    /// the ingress rewriter put on the main's clock — and then its audio
    /// stops too, for 1.1 s to 2.9 s with no silence laid, before the whole
    /// source comes back on that clock picture first. Every frame of its
    /// audio is sent at its own time; its first picture goes out where the
    /// audio stopped (in doubt: as far as the timeline can tell, a stray
    /// timestamp), and the rest at their own time. The picture used to be
    /// placed one step past its old one, 6 s behind the audio; its audio
    /// joined it there and was dropped as behind what had been sent.
    #[test]
    fn a_picture_back_after_a_hole_in_the_audio_keeps_its_audio() {
        for hole in [99_000, 135_000, 180_000, 261_000] {
            let mut tl = FlvTimeline::new("o");
            let play = |tl: &mut FlvTimeline, from: u64, first: Option<u64>| {
                for (track, t) in programme(90_000) {
                    let src = from + t;
                    if track == Track::Video {
                        let p = tl.video(src, None, true).unwrap();
                        let want = first.filter(|_| t == 0).unwrap_or(src);
                        assert_eq!(p.dts, LAP_90K + want, "{hole}: the picture at {src}");
                    } else {
                        let at = tl.audio(src);
                        assert_eq!(at, LAP_90K + src, "{hole}: the audio at {src}");
                        assert!(tl.audio_tag_ms(at).is_some(), "{hole}: the audio at {src} sent");
                    }
                }
            };
            play(&mut tl, T0, None);
            let mut a = T0 + 90_000;
            while a < T0 + 6 * 90_000 {
                let at = tl.audio(a);
                assert_eq!(at, LAP_90K + a, "{hole}: the audio alone at {a}");
                assert!(tl.audio_tag_ms(at).is_some());
                a += 1_920;
            }
            play(&mut tl, a + hole, Some(a));
        }
    }

    /// The same, but the source comes back with its clock 4 s to 9.5 s
    /// behind where it was — the main's encoder restarted, the ingress
    /// rewriter bridging the step it did not see — while an audio-only
    /// backup ran 12 s on in its place on that clock; and with pictures lost
    /// before the outage, one step of 0.68 s. The picture goes beside the
    /// audio, its audio joins it, and every frame is sent in the source's
    /// A/V relation. Kept as a gap at its own time, behind the audio, the
    /// picture left its audio to open an offset of its own beside the audio
    /// sent: the video 4 s or more behind its audio for good — after the
    /// lost pictures too, while one long step took the video for a slide
    /// feed's.
    #[test]
    fn a_picture_back_on_a_clock_behind_the_audio_goes_beside_it() {
        for (behind, lost) in [360_000, 540_000, 855_000].into_iter().flat_map(|b| [(b, false), (b, true)]) {
            let mut tl = FlvTimeline::new("o");
            for (track, t) in programme(90_000) {
                // Pictures lost before the outage: one step of 0.68 s.
                if track == Track::Video && lost && (25_200..86_400).contains(&t) {
                    continue;
                }
                if track == Track::Video {
                    tl.video(T0 + t, None, true);
                } else {
                    tl.audio(T0 + t);
                }
            }
            let mut a = T0 + 90_000;
            while a < T0 + 13 * 90_000 {
                let at = tl.audio(a);
                assert!(tl.audio_tag_ms(at).is_some());
                a += 1_920;
            }
            for (track, t) in programme(90_000) {
                let src = a - behind + t;
                if track == Track::Video {
                    tl.video(src, None, true);
                } else {
                    let at = tl.audio(src);
                    assert!(tl.audio_tag_ms(at).is_some(), "{behind}: the audio at {src} sent");
                }
            }
            // The last frame at 88 320, the last picture at 86 400.
            let (a, v) = (tl.newest_audio().unwrap(), tl.newest_picture().unwrap());
            assert_eq!(a as i64 - v as i64, 1_920, "{behind}, lost {lost}: the source's A/V relation");
        }
    }

    /// A 25 fps source whose video PID stops for 4 s while its AAC runs on,
    /// muxed at its own time or 1.1 s ahead of the pictures, the first
    /// picture back stamped 1.5 s to 2.9 s ahead of its time, or 2 s behind.
    /// The pictures after it go out at their own time — after a stray behind,
    /// from the second on: the first, a step as far forward again, is in
    /// doubt, as it always was. Kept as a gap, the stray ahead left every
    /// picture after it on an offset of its own, as late;
    /// and opened beside the audio as far past its place as the audio was
    /// muxed ahead, it left them 1.1 s late.
    #[test]
    fn a_stray_picture_after_a_video_stall_does_not_hold_the_video_late() {
        const STALL: u64 = T0 + 100 * STEP_25FPS;
        for (stray, ahead) in [135_000i64, 180_000, 261_000, -180_000].into_iter().flat_map(|s| [(s, 0), (s, 99_000)]) {
            let what = format!("stray {stray}, audio {ahead} ahead");
            let mut tl = FlvTimeline::new("o");
            let dts: Vec<u64> = (0..300)
                .map(|k| T0 + k * STEP_25FPS)
                .filter(|&d| !(STALL..STALL + 4 * 90_000).contains(&d))
                .collect();
            let audio = (0..300 * STEP_25FPS).step_by(1_920).map(|a| T0 + a);
            for (_, picture, t) in hand_out(&dts, 1, audio, |_| ahead) {
                if !picture {
                    assert_eq!(tl.audio(t), LAP_90K + t, "{what}: the audio at {t}");
                } else if t == STALL + 4 * 90_000 {
                    tl.video(t.saturating_add_signed(stray), None, true);
                } else if t > STALL + 4 * 90_000 + if stray < 0 { STEP_25FPS } else { 0 } {
                    let p = tl.video(t, None, true).unwrap();
                    assert_eq!(p.dts, LAP_90K + t, "{what}: the picture at {t}");
                } else {
                    tl.video(t, None, true);
                }
            }
        }
    }

    /// A source stamping only its I pictures (every 12th): each picture
    /// without a PTS takes the I picture's time, so each GOP goes out on one
    /// timestamp. That is the rule edge#211 asked for; spreading a GOP needs
    /// lookahead passthrough does not have.
    #[test]
    fn a_source_stamping_only_its_i_pictures_puts_each_gop_on_one_timestamp() {
        let mut tl = FlvTimeline::new("o");
        for g in 0..5u64 {
            let gop: Vec<(u32, u32)> = (0..12)
                .map(|k| {
                    let t = tl.video(T0 + g * 12 * STEP_25FPS, None, k == 0).unwrap();
                    (t.ts_ms, t.cts_ms)
                })
                .collect();
            assert!(gop.iter().all(|&p| p == (g as u32 * 480, 0)), "GOP {g}: {gop:?}");
        }
    }

    /// A feed on the wire, as `cmaf::timeline`'s tests have it: `frames`
    /// IBBP pictures at 25 fps and the audio beside them in 1 920-tick
    /// frames, stamped `base` plus the arrival time, the video `lead` ticks
    /// ahead of the audio in the mux.
    #[derive(Clone, Copy)]
    struct Feed {
        frames: u64,
        base: i64,
        lead: i64,
    }

    /// One essence of `feeds` as it arrives: a picture's `(pts, dts)` or an
    /// AAC frame's stamp, each source value unmasked.
    enum Essence {
        Picture(u64, Option<u64>),
        Audio(u64),
    }

    /// Every essence of `feeds`, one feed after another, in arrival order —
    /// a picture at its decode time, the audio first at a tie when
    /// `audio_first`.
    fn wire(feeds: &[Feed], audio_first: bool) -> Vec<Essence> {
        let mut ev: Vec<(i64, bool, Essence)> = Vec::new();
        let mut t0 = 0i64;
        for f in feeds {
            let len = f.frames as i64 * STEP_25FPS as i64;
            let at = |k: u64| (f.base + t0 + k as i64 * STEP_25FPS as i64 + f.lead) as u64;
            for (k, (pts, dts)) in ibbp(f.frames, at).into_iter().enumerate() {
                ev.push((t0 + k as i64 * STEP_25FPS as i64, true, Essence::Picture(pts, dts)));
            }
            for t in (0..len).step_by(1_920) {
                ev.push((t0 + t, false, Essence::Audio((f.base + t0 + t) as u64)));
            }
            t0 += len;
        }
        ev.sort_by_key(|&(t, video, _)| (t, video == audio_first));
        ev.into_iter().map(|(_, _, e)| e).collect()
    }

    /// `feeds` through a fresh timeline, checking FLV's contract on the way:
    /// the video's tags never step back (a timed picture's strictly
    /// forward), the audio sent strictly forward. Returns the last picture's
    /// and the last audio's offsets from their source — equal when the last
    /// feed's A/V relation is published as it came.
    fn play(feeds: &[Feed], audio_first: bool) -> (u64, u64) {
        let mut tl = FlvTimeline::new("o");
        let (mut video, mut audio) = (None::<u32>, None::<u32>);
        let mut offsets = (0, 0);
        for e in wire(feeds, audio_first) {
            match e {
                Essence::Picture(pts, dts) => {
                    let t = tl.video(pts & MASK_33, dts.map(|d| d & MASK_33), true).unwrap();
                    assert!(video.is_none_or(|v| t.ts_ms > v), "the video steps back: {} after {video:?}", t.ts_ms);
                    video = Some(t.ts_ms);
                    offsets.0 = t.pts.wrapping_sub(pts);
                }
                Essence::Audio(a) => {
                    let at = tl.audio(a & MASK_33);
                    if let Some(ms) = tl.audio_tag_ms(at) {
                        assert!(audio.is_none_or(|l| ms > l), "the audio steps back: {ms} after {audio:?}");
                        audio = Some(ms);
                    }
                    offsets.1 = at.wrapping_sub(a);
                }
            }
        }
        offsets
    }

    /// The switches and jumps `cmaf::timeline` takes up, on an FLV timeline
    /// carrying B-frames — the video mapped by decode time, so a composition
    /// offset rides beside it — each in both arrival orders where the order
    /// is the point: the tags never step back, and the new feed's A/V
    /// relation is what goes out.
    #[test]
    fn feed_switches_with_b_frames_keep_tags_forward_and_take_the_new_relation() {
        let scenarios: [(&str, [Feed; 2], bool); 4] = [
            (
                "a 2 s outage of both",
                [Feed { frames: 150, base: 900_000, lead: 45_000 }, Feed { frames: 200, base: 1_080_000, lead: 45_000 }],
                false,
            ),
            (
                "a switch moving the video 1.1 s and the audio 0.5 s",
                [Feed { frames: 150, base: 900_000, lead: -45_000 }, Feed { frames: 200, base: 945_000, lead: 9_000 }],
                false,
            ),
            (
                "a switch the audio meets first",
                [Feed { frames: 150, base: 900_000, lead: 99_000 }, Feed { frames: 200, base: 5_000_000_000, lead: 0 }],
                true,
            ),
            (
                "both tracks jumping 2 s",
                [Feed { frames: 100, base: 900_000, lead: 45_000 }, Feed { frames: 300, base: 1_080_000, lead: 45_000 }],
                false,
            ),
        ];
        for (what, feeds, audio_first) in scenarios {
            let (v, a) = play(&feeds, audio_first);
            assert_eq!(v, a, "{what}: the new feed's A/V relation");
        }
    }

    /// An outage of both tracks past the timeline's 3 s cross window is a
    /// jump nothing on the other track vouches for: both tracks go on one
    /// step on, together, whichever meets it first — the FLV clock falls
    /// behind the wall clock by the outage. A 2 s outage is kept as a gap.
    #[test]
    fn an_outage_of_both_tracks_longer_than_the_cross_window_is_closed_together() {
        for audio_first in [false, true] {
            let feeds = [Feed { frames: 150, base: 900_000, lead: 0 }, Feed { frames: 200, base: 1_350_000, lead: 0 }];
            let (v, a) = play(&feeds, audio_first);
            assert_eq!(v, a, "audio first {audio_first}: together");
            // Closed one step on from the track that met it. The feeds are
            // back to back on the wire: the video's next picture lands where
            // it would have with no outage, and the audio's — 480 ticks
            // after its last frame on the wire — a whole frame on.
            let closed = if audio_first { 450_000 - 1_440 } else { 450_000 };
            assert_eq!(LAP_90K - v, closed, "audio first {audio_first}: closed, one step on");
        }
        let feeds = [Feed { frames: 150, base: 900_000, lead: 45_000 }, Feed { frames: 200, base: 1_080_000, lead: 45_000 }];
        assert_eq!(play(&feeds, false), (LAP_90K, LAP_90K), "a 2 s outage kept as a gap");
    }

    // ── edge#211: the transcoded audio on the timeline ────────────────────

    /// One audio frame's length, whatever the codec, and no panic on a rate
    /// of 0.
    #[test]
    fn audio_ticks_steps_by_the_frame() {
        assert_eq!(audio_ticks(1_152, 48_000), 2_160, "MP2");
        assert_eq!(audio_ticks(1_536, 48_000), 2_880, "AC-3");
        assert_eq!(audio_ticks(1_024, 48_000), 1_920, "AAC");
        assert_eq!(audio_ticks(1_024, 0), 92_160_000);
    }

    /// The eager encoder a `silent_fallback` output builds before any audio.
    #[cfg(feature = "fdk-aac")]
    fn silent_fallback_state() -> EncoderState {
        let cfg: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc", "silent_fallback": true }
        }))
        .unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "rtmp".into()));
        build_encoder_state_eager_for_silent_fallback(&cfg, &CancellationToken::new(), &stats, "f", &events)
    }

    /// The stamps of what `encoder` has encoded that the timeline sends
    /// (`send_encoded_audio` without the client).
    #[cfg(feature = "fdk-aac")]
    fn sent(encoder: &mut AudioEncoder, tl: &mut FlvTimeline) -> Vec<u64> {
        encoder.drain().into_iter().filter(|f| tl.audio_tag_ms(f.pts).is_some()).map(|f| f.pts).collect()
    }

    /// A video-only source (the case `silent_fallback` exists for) whose PTS
    /// is far from 0. The silence used to be submitted at the generator's
    /// own PTS, 0, and once the first picture had anchored the epoch every
    /// silent tag went out at 0. It starts at the picture now — nothing laid
    /// until the picture moves on — and is laid up to it, a second of
    /// pictures bringing a second of silence however few ticks noticed.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn silence_on_a_video_only_source_starts_at_the_picture_not_at_zero() {
        let mut st = silent_fallback_state();
        let EncoderState::Active { encoder, silence: Some(sg), input, .. } = &mut st else {
            panic!("silent fallback builds eagerly");
        };
        const P0: u64 = 4_000_000_000;
        let mut tl = FlvTimeline::new("o");
        for k in 0..25 {
            tl.video(P0 + k * STEP_25FPS, None, true);
        }
        let p = tl.newest_picture().unwrap();
        lay_silence(encoder, sg, input, &mut tl, "o");
        assert!(encoder.drain().is_empty(), "nothing to lay yet");
        assert_eq!(input.position(), Some(p), "the silence starts at the picture");
        for k in 25..50 {
            tl.video(P0 + k * STEP_25FPS, None, true);
        }
        lay_silence(encoder, sg, input, &mut tl, "o");
        let frames = encoder.drain();
        assert_eq!(frames.len(), 46, "46 chunks of 1 920 ticks fit in the second");
        let delay = p - frames[0].pts;
        assert!(delay > 0 && delay < 9_000, "the codec's delay: {delay}");
        let first = tl.audio_tag_ms(frames[0].pts).unwrap();
        assert_eq!(first, tl.tag_ms(p - delay));
        assert!(first > 900, "the first silent tag at {first} ms, by the picture (it was 0)");
        assert!(frames.windows(2).all(|w| w[1].pts - w[0].pts == 1_920));
    }

    /// A video-only source pausing 3 s (a pause the lone gap keeps): the
    /// silence stands still with the picture and is laid across the pause
    /// when the picture comes back — one contiguous track, ending within a
    /// chunk of the picture less the codec's delay. Ticked by the clock it
    /// ran on through a stall and fell behind at every tick observed late.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn silence_stays_with_a_video_only_source_across_a_pause() {
        let mut st = silent_fallback_state();
        let EncoderState::Active { encoder, silence: Some(sg), input, .. } = &mut st else {
            panic!("silent fallback builds eagerly");
        };
        let mut tl = FlvTimeline::new("o");
        let mut stamps = Vec::new();
        let mut start = None;
        for k in 0..100u64 {
            let pause = if k >= 50 { 3 * 90_000 } else { 0 };
            tl.video(T0 + k * STEP_25FPS + pause, None, true);
            lay_silence(encoder, sg, input, &mut tl, "o");
            start = start.or(input.position());
            stamps.extend(encoder.drain().into_iter().map(|f| f.pts));
            if k == 49 {
                // The pause: the picture stands still, and so does the silence.
                let before = input.position();
                lay_silence(encoder, sg, input, &mut tl, "o");
                assert_eq!(input.position(), before);
            }
        }
        assert!(stamps.windows(2).all(|w| w[1] - w[0] == 1_920), "one contiguous track");
        let delay = start.unwrap() - stamps[0];
        let end = stamps.last().unwrap() + 1_920 + delay;
        let newest = tl.newest_picture().unwrap();
        assert!(end <= newest && newest - end < 1_920, "laid to the picture: {end} vs {newest}");
        assert!(stamps.len() > 300, "{} frames for 7 s", stamps.len());
    }

    /// Real audio, then none past the watchdog's grace, then real audio
    /// again: the silence continues the real audio exactly, real audio
    /// returning where the silence ends continues it exactly, and audio
    /// behind it is held back — the audio track never steps back.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn silence_and_real_audio_hand_over_without_stepping_back() {
        let mut st = silent_fallback_state();
        let EncoderState::Active { encoder, silence: Some(sg), input, stage, .. } = &mut st else {
            panic!("silent fallback builds eagerly");
        };
        let pcm = vec![vec![0.1f32; 1_024]; 2];
        let mut tl = FlvTimeline::new("o");
        let mut stamps = Vec::new();
        for (track, t) in programme(90_000) {
            match track {
                Track::Video => {
                    tl.video(T0 + t, None, true);
                }
                Track::Audio => {
                    let at = tl.audio(T0 + t);
                    sg.mark_real_audio(at);
                    input.mark_real_audio(at, tl.newest_picture());
                    submit_source_audio(encoder, stage, input, true, &pcm, 48_000, at, "o");
                    stamps.extend(sent(encoder, &mut tl));
                }
            }
        }
        let real = stamps.len();
        assert!(real > 40);
        // The audio stops; past the grace, a second more of pictures.
        lay_silence(encoder, sg, input, &mut tl, "o");
        assert!(encoder.drain().is_empty(), "inside the grace");
        std::thread::sleep(std::time::Duration::from_millis(600));
        for k in 0..25 {
            tl.video(T0 + 90_000 + k * STEP_25FPS, None, true);
            lay_silence(encoder, sg, input, &mut tl, "o");
            stamps.extend(sent(encoder, &mut tl));
        }
        assert!(stamps.len() > real + 40, "a second of silence");
        assert_eq!(stamps[real], stamps[real - 1] + 1_920, "the silence continues the real audio");
        // Real audio back where the silence ends: continued.
        let laid = stamps.len();
        let back = input.position().unwrap() - LAP_90K;
        for j in 0..3 {
            let at = tl.audio(back + j * 1_920);
            assert_eq!(at, input.position().unwrap(), "frame {j} where the input is");
            sg.mark_real_audio(at);
            input.mark_real_audio(at, tl.newest_picture());
            submit_source_audio(encoder, stage, input, true, &pcm, 48_000, at, "o");
            stamps.extend(sent(encoder, &mut tl));
        }
        assert_eq!(stamps.len(), laid + 3);
        assert_eq!(stamps[laid], stamps[laid - 1] + 1_920, "the real audio continues the silence");
        // Audio behind the input is held back until it catches up.
        let position = input.position();
        let behind = input.position().unwrap() - LAP_90K - 4 * 1_920;
        for j in 0..2 {
            let at = tl.audio(behind + j * 1_920);
            submit_source_audio(encoder, stage, input, true, &pcm, 48_000, at, "o");
        }
        assert!(encoder.drain().is_empty(), "held back");
        assert_eq!(input.position(), position);
        assert!(stamps.windows(2).all(|w| w[1] > w[0]), "the audio never steps back");
    }

    /// `n` real AAC frames from source time `from`, placed as the publish
    /// loop's AAC arm places them under `silent_fallback`: on the timeline,
    /// the watchdog and the lead told, onto the encoder's input.
    #[cfg(feature = "fdk-aac")]
    fn feed_real(st: &mut EncoderState, tl: &mut FlvTimeline, from: u64, n: u64) {
        let EncoderState::Active { encoder, silence: Some(sg), input, stage, .. } = st else {
            panic!("silent fallback builds eagerly");
        };
        let pcm = vec![vec![0.1f32; 1_024]; 2];
        for j in 0..n {
            let at = tl.audio((from + j * 1_920) & MASK_33);
            note_real_audio(Some(sg), input, at, tl.newest_picture());
            submit_source_audio(encoder, stage, input, true, &pcm, 48_000, at, "o");
        }
    }

    /// `ticks` silence ticks, `lay_silence` on each.
    #[cfg(feature = "fdk-aac")]
    fn tick_silence(st: &mut EncoderState, tl: &mut FlvTimeline, ticks: usize) {
        let EncoderState::Active { encoder, silence: Some(sg), input, .. } = st else {
            panic!("silent fallback builds eagerly");
        };
        for _ in 0..ticks {
            lay_silence(encoder, sg, input, tl, "o");
        }
    }

    /// The stamps of what the encoder has encoded, and of those the timeline
    /// sends.
    #[cfg(feature = "fdk-aac")]
    fn drain_sent(st: &mut EncoderState, tl: &mut FlvTimeline) -> (Vec<u64>, Vec<u64>) {
        let EncoderState::Active { encoder, .. } = st else {
            panic!("silent fallback builds eagerly");
        };
        let drained: Vec<u64> = encoder.drain().into_iter().map(|f| f.pts).collect();
        let sent = drained.iter().copied().filter(|&p| tl.audio_tag_ms(p).is_some()).collect();
        (drained, sent)
    }

    /// An audio-only source under `silent_fallback` — no picture to lay the
    /// silence to, so it goes in a chunk a tick from the encoder's input —
    /// whose audio stops for 11 s and comes back on the same clock, then
    /// for 3 s and comes back on another (a backup, a publisher reconnecting
    /// at a reset PTS). The returning audio continues the silence, zero-filled
    /// up to it on the same clock and one frame on across the jump, and
    /// nothing after either return is dropped. Were the silence left off the
    /// timeline, it would continue the return one frame past the last *real*
    /// frame, behind the silence by as long as it ran: every frame held back
    /// or dropped, and no audio sent for as long again.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn audio_returning_to_an_audio_only_source_continues_its_silence() {
        let mut st = silent_fallback_state();
        let mut tl = FlvTimeline::new("o");
        feed_real(&mut st, &mut tl, T0, 50);
        let (_, mut stamps) = drain_sent(&mut st, &mut tl);
        // 11 s without audio: the grace, then a chunk on each tick of the rest.
        std::thread::sleep(std::time::Duration::from_millis(600));
        tick_silence(&mut st, &mut tl, 492);
        let (drained, sent) = drain_sent(&mut st, &mut tl);
        assert_eq!(sent, drained, "the silence sent whole");
        assert!(sent.len() > 480, "{} silent frames", sent.len());
        stamps.extend(sent);
        // Back on the same clock, 11 s on: past the lone gap from the last
        // real frame, inside the timeline's window from the silence's end.
        feed_real(&mut st, &mut tl, T0 + 50 * 1_920 + 11 * 90_000, 50);
        let (drained, sent) = drain_sent(&mut st, &mut tl);
        assert_eq!(sent, drained, "nothing after the return dropped");
        assert!(sent.len() > 70, "the fill to the return, and the return: {}", sent.len());
        stamps.extend(sent);
        // 3 s without audio, back on an unrelated clock.
        std::thread::sleep(std::time::Duration::from_millis(600));
        tick_silence(&mut st, &mut tl, 117);
        let (drained, sent) = drain_sent(&mut st, &mut tl);
        assert_eq!(sent, drained, "the silence sent whole");
        stamps.extend(sent);
        feed_real(&mut st, &mut tl, 5_000_000_000, 50);
        let (drained, sent) = drain_sent(&mut st, &mut tl);
        assert_eq!(sent, drained, "nothing after the jump dropped");
        assert!(sent.len() >= 49, "{} frames after the jump", sent.len());
        stamps.extend(sent);
        assert!(stamps.windows(2).all(|w| w[1] - w[0] == 1_920), "one contiguous track");
    }

    /// The video stops while the audio runs on (a switch to an audio-only
    /// backup, a video PID that stops), and then the audio stops too: the
    /// silence follows the audio a chunk a tick, as on a source that never
    /// had a picture. Laid to the newest picture, which the audio has long
    /// passed, none would be laid at all. A source stopping whole — both
    /// tracks together — still lays none past its stalled picture.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn silence_follows_the_audio_once_the_video_has_gone() {
        let mut st = silent_fallback_state();
        let mut tl = FlvTimeline::new("o");
        for (track, t) in programme(90_000) {
            match track {
                Track::Video => {
                    tl.video(T0 + t, None, true);
                }
                Track::Audio => feed_real(&mut st, &mut tl, T0 + t, 1),
            }
        }
        let (_, mut stamps) = drain_sent(&mut st, &mut tl);
        // Both tracks stop: nothing past the stalled picture.
        std::thread::sleep(std::time::Duration::from_millis(600));
        tick_silence(&mut st, &mut tl, 25);
        assert!(drain_sent(&mut st, &mut tl).0.is_empty(), "a stalled picture stops the silence");
        // The audio comes back alone, where it stopped, and runs 5 s on.
        feed_real(&mut st, &mut tl, T0 + 47 * 1_920, 235);
        stamps.extend(drain_sent(&mut st, &mut tl).1);
        // And stops: past the grace, a chunk a tick.
        std::thread::sleep(std::time::Duration::from_millis(600));
        tick_silence(&mut st, &mut tl, 50);
        let (drained, sent) = drain_sent(&mut st, &mut tl);
        assert_eq!(sent, drained);
        assert!(sent.len() >= 48, "{} silent frames on 50 ticks", sent.len());
        stamps.extend(sent);
        assert!(stamps.windows(2).all(|w| w[1] - w[0] == 1_920), "the silence continues the audio");
    }

    /// After its video had gone the audio ran on alone — a switch to an
    /// audio-only backup on another clock — and then stopped, and silence
    /// was laid a chunk a tick. The main feed comes back, on its own clock
    /// run on in real time or on a third, picture first or audio first:
    /// either way its audio is sent whole, continuing the silence, in the
    /// source's own A/V relation. A picture returning first used to be
    /// continued one frame past its old picture, as far behind the audio as
    /// it had been away, and its audio joined it there: behind everything
    /// sent, every frame of it dropped until the source had run that far
    /// again — 8.5 s of the main feed's audio here.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn a_picture_returning_after_the_video_had_gone_is_put_beside_the_audio() {
        const B: u64 = 5_000_000_000;
        // The main's second, the backup's 5 s, the grace and the silence
        // after it: where the main's clock is when it comes back.
        let away = 90_000 + 235 * 1_920 + 45_000 + 140 * 1_920;
        for (clock, picture_first) in
            [(T0 + away, true), (T0 + away, false), (7_000_000_000, true), (7_000_000_000, false)]
        {
            let what = format!("back at {clock}, picture first {picture_first}");
            let mut st = silent_fallback_state();
            let mut tl = FlvTimeline::new("o");
            for (track, t) in programme(90_000) {
                match track {
                    Track::Video => {
                        tl.video(T0 + t, None, true);
                    }
                    Track::Audio => feed_real(&mut st, &mut tl, T0 + t, 1),
                }
            }
            // The video stops, and the audio goes on 5 s on the backup's clock.
            feed_real(&mut st, &mut tl, B, 235);
            let (_, mut stamps) = drain_sent(&mut st, &mut tl);
            assert!(tl.live_picture().is_none(), "the video taken for gone");
            // The audio stops: past the grace, 3 s of silence.
            std::thread::sleep(std::time::Duration::from_millis(600));
            tick_silence(&mut st, &mut tl, 140);
            stamps.extend(drain_sent(&mut st, &mut tl).1);
            // A second of the main feed, a tick after each essence.
            let mut back = programme(90_000);
            if !picture_first {
                back.sort_by_key(|&(track, t)| (t, track == Track::Video));
            }
            let (mut drained, mut sent) = (Vec::new(), Vec::new());
            for (track, t) in back {
                match track {
                    Track::Video => {
                        tl.video(clock + t, None, true);
                    }
                    Track::Audio => feed_real(&mut st, &mut tl, clock + t, 1),
                }
                tick_silence(&mut st, &mut tl, 1);
                let (d, s) = drain_sent(&mut st, &mut tl);
                drained.extend(d);
                sent.extend(s);
            }
            let dropped: Vec<u64> = drained.iter().copied().filter(|p| !sent.contains(p)).collect();
            assert!(dropped.is_empty(), "{what}: dropped after the return: {dropped:?}");
            assert!(sent.len() > 40, "{what}: {} frames after the return", sent.len());
            stamps.extend(sent);
            assert!(stamps.windows(2).all(|w| w[1] - w[0] == 1_920), "{what}: one contiguous track");
            // The last frame at 88 320, the last picture at 86 400.
            let (a, v) = (tl.newest_audio().unwrap(), tl.newest_picture().unwrap());
            assert_eq!(a as i64 - v as i64, 1_920, "{what}: the source's A/V relation");
        }
    }

    /// One PES of `frames` MP2 frames from source time `pts`, placed as the
    /// publish loop's `OtherAudio` arm places it under `silent_fallback`:
    /// the PES on the timeline, the watchdog and the lead told, each frame
    /// of it 2 160 ticks on from the one before. Returns where it was mapped.
    #[cfg(feature = "fdk-aac")]
    fn feed_pes(st: &mut EncoderState, tl: &mut FlvTimeline, pts: u64, frames: u64) -> u64 {
        let EncoderState::Active { encoder, silence: Some(sg), input, stage, .. } = st else {
            panic!("silent fallback builds eagerly");
        };
        let pcm = vec![vec![0.1f32; 1_152]; 2];
        let at = tl.audio(pts & MASK_33);
        note_real_audio(Some(sg), input, at, tl.newest_picture());
        for j in 0..frames {
            submit_source_audio(encoder, stage, input, true, &pcm, 48_000, at + j * 2_160, "o");
        }
        at
    }

    /// A source whose audio stops for 5 s while its pictures run on, under
    /// `silent_fallback` — the live case: H.264 decoding 80 ms before it
    /// presents, MP2 in PES of four frames, its audio PID dropped. The
    /// demuxer hands a picture out when the next picture's PES begins, and
    /// an MP2 PES's frames when the next audio PES begins, so the last PES
    /// before the outage comes out only beside the first after it, 5 s
    /// late, its place long since laid with silence. It is dropped, and the
    /// audio returning continues the silence, the stretch up to it
    /// zero-filled: one contiguous track, the return at its own time. That
    /// PES used to re-anchor the encoder 4.8 s back, every frame of it then
    /// dropped as behind what had been sent, and the return re-anchored it
    /// forward with nothing filled: no audio tag between the silence's end
    /// and the return, the picture's lead over the audio (0.4 s live). A
    /// second outage is laid as the first: that PES, 5 s behind the picture,
    /// was also taken for the picture's lead over the audio, and held the
    /// next outage's silence 5 s short of the picture — a 5 s hole. With the
    /// audio muxed at its own time, and 300 ms ahead of the pictures, so that
    /// it comes back ahead of the newest one.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn a_pes_handed_out_after_an_outage_beside_the_picture_leaves_no_hole() {
        const PES: u64 = 4 * 2_160;
        // Two outages: the second laid as the first.
        let out = |p: u64| [(3, 8), (11, 16)].iter().any(|&(a, b)| (T0 + a * 90_000..T0 + b * 90_000).contains(&p));
        for audio_ahead in [0, 27_000] {
            let mut st = silent_fallback_state();
            let mut tl = FlvTimeline::new("o");
            // (handed out at, a picture, its decode time / the PES's PTS):
            // a picture's PES arrives at its decode time, an audio PES
            // `audio_ahead` before its PTS.
            let mut ev: Vec<(u64, bool, u64)> =
                (0..500).map(|k| T0 + k * STEP_25FPS).map(|dts| (dts + STEP_25FPS, true, dts)).collect();
            let pes: Vec<u64> = (0..230).map(|j| T0 + j * PES).filter(|&p| !out(p)).collect();
            ev.extend(pes.windows(2).map(|w| (w[1] - audio_ahead, false, w[0])));
            ev.sort_by_key(|&(at, picture, _)| (at, !picture));
            let (mut stamps, mut drained, mut sent, mut moved) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let (mut last_real, mut slept, mut first_back, mut last_back) = (None, None, None, None);
            let mut outages = 0;
            for (w, picture, t) in ev {
                if picture {
                    tl.video(t + 2 * STEP_25FPS, Some(t), true);
                } else {
                    let at = feed_pes(&mut st, &mut tl, t, 4);
                    if at != LAP_90K + t {
                        moved.push(t);
                    }
                    last_real = Some(w);
                    if t >= T0 + 8 * 90_000 {
                        first_back = first_back.or(Some((t, at)));
                        last_back = Some(at);
                    }
                }
                // Into an outage, past the watchdog's grace.
                if last_real.is_some_and(|r| w > r + 45_000) && slept != last_real {
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    slept = last_real;
                    outages += 1;
                }
                tick_silence(&mut st, &mut tl, 1);
                let (d, s) = drain_sent(&mut st, &mut tl);
                if slept.is_some() {
                    drained.extend(d);
                    sent.extend(s.iter().copied());
                }
                stamps.extend(s);
            }
            let what = format!("audio {audio_ahead} ticks ahead");
            assert_eq!(outages, 2, "{what}: two outages");
            // The late PES included: continued past the silence, it would go
            // out 5 s from its place.
            assert!(moved.is_empty(), "{what}: PES moved off the source's clock: {moved:?}");
            assert_eq!(tl.jumps, 0, "{what}: the source never jumped");
            let dropped: Vec<u64> = drained.iter().copied().filter(|p| !sent.contains(p)).collect();
            assert!(dropped.is_empty(), "{what}: encoded from the outage on and dropped: {dropped:?}");
            let holes: Vec<i64> =
                stamps.windows(2).map(|w| w[1] as i64 - w[0] as i64).filter(|&d| d != 1_920).collect();
            assert!(holes.is_empty(), "{what}: steps other than a frame in the audio tags: {holes:?}");
            let (src, at) = first_back.unwrap();
            assert_eq!(at, LAP_90K + src, "{what}: the return on the source's clock");
            // Submitted where the timeline put it, within the placement's slack.
            let EncoderState::Active { input, .. } = &st else { unreachable!() };
            let off = input.position().unwrap() as i64 - (last_back.unwrap() + PES) as i64;
            let slack = crate::engine::audio_encode::SOURCE_PTS_SLACK_TICKS;
            assert!(off.unsigned_abs() <= slack, "{what}: the return {off} ticks off its own time");
            assert_eq!(tl.newest_picture(), Some(LAP_90K + T0 + 501 * STEP_25FPS), "{what}: the pictures too");
        }
    }

    /// An audio-only MP2 source under `silent_fallback`, its silence on the
    /// timeline's audio track a chunk a tick, and the last PES before an
    /// outage handed out only beside the first after it, its place long
    /// gone: that PES is continued one frame past the silence, and the audio
    /// returning after it lands at its own time on its own clock, the
    /// stretch up to it filled — one contiguous track. That PES's offset
    /// used to take the return with it as a lone track's gap: after a 3 s
    /// outage every frame from then on went out 2.5 s late; after 6 s, past
    /// the fill's reach, with a 6 s hole as well. A source whose timestamps
    /// close the outage (the input's PTS rewriter bridges an audio-only
    /// feed's gap the wall clock did not see) runs on past the silence, none
    /// of it held back.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn audio_returning_after_an_outage_with_no_picture_lands_at_its_own_time() {
        const PES: u64 = 4 * 2_160;
        for (outage, closed) in [(3u64, false), (6, false), (11, false), (6, true)] {
            let what = format!("a {outage} s outage, closed {closed}");
            let mut st = silent_fallback_state();
            let mut tl = FlvTimeline::new("o");
            let gone = T0 + 2 * 90_000;
            let back = if closed { gone } else { gone + outage * 90_000 };
            let pes: Vec<u64> = (0..250)
                .map(|j| T0 + j * PES)
                .filter(|&p| p < gone || p >= back)
                .take_while(|&p| p < back + 90_000)
                .collect();
            let (mut stamps, mut drained, mut sent, mut moved) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let (mut silence_end, mut after) = (None, 0);
            // Each PES handed out when the next begins.
            for w in pes.windows(2) {
                if silence_end.is_none() && w[1] >= back {
                    // The outage: the grace, then a chunk a tick.
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    tick_silence(&mut st, &mut tl, ((outage * 90_000 - 45_000) / 1_920) as usize);
                    let EncoderState::Active { input, .. } = &st else { unreachable!() };
                    silence_end = input.position();
                }
                let at = feed_pes(&mut st, &mut tl, w[0], 4);
                if !closed && w[0] >= back && at != LAP_90K + w[0] {
                    moved.push(w[0]);
                }
                let (d, s) = drain_sent(&mut st, &mut tl);
                if silence_end.is_some() {
                    after += 1;
                    drained.extend(d);
                    sent.extend(s.iter().copied());
                }
                stamps.extend(s);
            }
            let dropped: Vec<u64> = drained.iter().copied().filter(|p| !sent.contains(p)).collect();
            assert!(dropped.is_empty(), "{what}: encoded from the outage on and dropped: {dropped:?}");
            let holes: Vec<i64> =
                stamps.windows(2).map(|w| w[1] as i64 - w[0] as i64).filter(|&d| d != 1_920).collect();
            assert!(holes.is_empty(), "{what}: steps other than a frame in the audio tags: {holes:?}");
            assert!(moved.is_empty(), "{what}: returning PES off the source's clock: {moved:?}");
            // Where the input ends: the source's own time, or with the
            // outage closed every PES after the silence, none held back.
            let EncoderState::Active { input, .. } = &st else { unreachable!() };
            let end = if closed { silence_end.unwrap() + after * PES } else { LAP_90K + pes[pes.len() - 2] + PES };
            let off = input.position().unwrap() as i64 - end as i64;
            let slack = crate::engine::audio_encode::SOURCE_PTS_SLACK_TICKS;
            assert!(off.unsigned_abs() <= slack, "{what}: the input {off} ticks off where it belongs");
        }
    }

    /// A video-only source with a picture every 5 s (a slide feed, a
    /// timelapse camera): the FLV timeline keeps each step (a lone track's
    /// gap, up to 10 s), and the silence fills it to the picture. Filled no
    /// further than the 4 s a CMAF fill covers, each picture would re-anchor
    /// the silence at itself with nothing laid, and the connection never
    /// send an audio tag, the sequence header included.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn silence_fills_a_video_only_source_whose_pictures_are_seconds_apart() {
        let mut st = silent_fallback_state();
        let mut tl = FlvTimeline::new("o");
        let (mut drained, mut sent) = (Vec::new(), Vec::new());
        let mut start = None;
        for k in 0..6u64 {
            tl.video(T0 + k * 5 * 90_000, None, true);
            tick_silence(&mut st, &mut tl, 10);
            let EncoderState::Active { input, .. } = &st else { unreachable!() };
            start = start.or(input.position());
            let (d, s) = drain_sent(&mut st, &mut tl);
            drained.extend(d);
            sent.extend(s);
        }
        assert!(sent.len() > 1_100, "{} silent frames for 25 s", sent.len());
        assert!(drained.windows(2).all(|w| w[1] - w[0] == 1_920), "one contiguous track");
        let delay = start.unwrap() - drained[0];
        let end = drained.last().unwrap() + 1_920 + delay;
        let newest = tl.newest_picture().unwrap();
        assert!(end <= newest && newest - end < 1_920, "laid to the picture: {end} vs {newest}");
    }

    /// One tick lays at most a second of silence: a pause the timeline keeps
    /// (3 s here; up to 10 s on a video-only source) is caught up over the
    /// next few ticks, each a bounded piece of work, rather than encoded at
    /// once — 140 AAC frames on one tick here, 470 at the lone gap.
    #[cfg(feature = "fdk-aac")]
    #[test]
    fn one_tick_lays_at_most_a_second_of_silence() {
        let mut st = silent_fallback_state();
        let mut tl = FlvTimeline::new("o");
        for k in 0..25 {
            tl.video(T0 + k * STEP_25FPS, None, true);
        }
        tick_silence(&mut st, &mut tl, 1);
        tl.video(T0 + 24 * STEP_25FPS + 3 * 90_000, None, true);
        let mut per_tick = Vec::new();
        let mut stamps = Vec::new();
        for _ in 0..5 {
            tick_silence(&mut st, &mut tl, 1);
            let (drained, _) = drain_sent(&mut st, &mut tl);
            per_tick.push(drained.len());
            stamps.extend(drained);
        }
        assert!(per_tick.iter().all(|&n| n <= 47), "frames encoded per tick: {per_tick:?}");
        assert!(stamps.windows(2).all(|w| w[1] - w[0] == 1_920), "one contiguous track");
        let EncoderState::Active { input, .. } = &st else { unreachable!() };
        let (at, newest) = (input.position().unwrap(), tl.newest_picture().unwrap());
        assert!(at <= newest && newest - at < 1_920, "caught up: {at} vs {newest}");
    }

    /// MP2 packs several frames into a PES (four here, 8 640 ticks), each
    /// stepped on from the PES's PTS by its own length: placed at the PES's
    /// PTS, every frame after the first (2 160 ticks on, past the 2 000-tick
    /// slack) would read as a departure. The encoder is anchored once and
    /// never again, and its frames run on contiguous.
    #[cfg(all(feature = "media-codecs", feature = "fdk-aac"))]
    #[test]
    fn transcoded_mp2_with_several_frames_per_pes_never_reanchors() {
        let cfg: RtmpOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc" }
        }))
        .unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "rtmp".into()));
        let cancel = CancellationToken::new();
        let mut state = EncoderState::Lazy;
        let (mut dec, mut dec_codec) = (None, None);
        let mut tl = FlvTimeline::new("o");
        let mut stamps = Vec::new();
        let mut first = None;
        for k in 0..20u64 {
            let pts = 900_000 + k * 8_640;
            let at = tl.audio(pts);
            first = first.or(Some(at));
            let decoded = decode_other_audio_for_encode(
                &mut state, &mut dec, &mut dec_codec, 0x03, &super::tests::mp2_pes(4), pts,
                &cfg, &cancel, &stats, "f", &events,
            );
            let EncoderState::Active { encoder, stage, input, .. } = &mut state else {
                panic!("the encoder is built on the first PES that decodes");
            };
            assert_eq!(encoder.params().sample_rate, 48_000);
            let mut frame_at = at;
            for f in decoded {
                submit_source_audio(encoder, stage, input, false, &f.planar, f.sample_rate, frame_at, "o");
                frame_at += audio_ticks(f.samples_per_channel, f.sample_rate);
            }
            if k == 0 {
                assert_eq!(input.position(), Some(frame_at), "anchored on the first PES");
            }
            stamps.extend(encoder.drain().into_iter().map(|f| f.pts));
        }
        let EncoderState::Active { input, .. } = &state else { unreachable!() };
        assert_eq!(input.position(), Some(first.unwrap() + 20 * 8_640), "never re-anchored");
        assert!(stamps.len() > 70, "{} AAC frames", stamps.len());
        assert!(stamps.windows(2).all(|w| w[1] - w[0] == 1_920), "contiguous");
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
        // Returns once the channel is drained and closed. The input can
        // carry TS audio (it only matters with `audio_encode`).
        publish_loop(&config, &mut client, &mut rx, &stats, &CancellationToken::new(), true, "f", &events)
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

    // ── edge#211: H.264 recovery points ──────────────────────────────────

    /// The SEIs of broadcast-spts-h264-1080p25-ac3.ts: buffering_period,
    /// recovery_point and pic_timing at a random-access point; pic_timing
    /// alone elsewhere.
    const H264_SEI_RP: [u8; 11] = [0x06, 0x00, 0x01, 0xC0, 0x06, 0x01, 0x84, 0x01, 0x01, 0x32, 0x80];
    const H264_SEI_PT: [u8; 5] = [0x06, 0x01, 0x01, 0x32, 0x80];

    /// What an H.264 test picture is.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Pic {
        /// [`h264_au`]'s key picture: SPS, PPS, IDR.
        Idr,
        /// An open-GOP random-access point as the DVB sources send it: SPS,
        /// PPS, a recovery-point SEI, a non-IDR I picture.
        RecoveryPoint,
        /// The same with a P field after it in the same PES.
        RecoveryPointFieldPair,
        /// A non-IDR I picture with SPS / PPS and no recovery point.
        Intra,
        /// [`h264_au`]'s P picture.
        Inter,
        /// A P picture repeating SPS and PPS.
        InterWithSets,
        /// A recovery-point SEI on a P picture: gradual decoding refresh.
        GdrP,
    }

    impl Pic {
        /// Whether a muxer marks it random access (RAI, PSI ahead of it), as
        /// the capture's did.
        fn keyed(self) -> bool {
            matches!(self, Pic::Idr | Pic::RecoveryPoint | Pic::RecoveryPointFieldPair)
        }
    }

    /// Picture `k` of kind `p`, Annex B.
    fn h264_pic(p: Pic, k: u8) -> Vec<u8> {
        let i_slice = [0x41, 0x88, 0x84, k + 1]; // slice_type 7
        let p_field = [0x41, 0x9A, 0x22, k + 2]; // slice_type 5
        match p {
            Pic::Idr => h264_au(true, k),
            Pic::Inter => h264_au(false, k),
            Pic::RecoveryPoint => annex_b(&[&H264_AUD, &H264_SPS, &H264_PPS0, &H264_SEI_RP, &i_slice]),
            Pic::RecoveryPointFieldPair => annex_b(&[
                &H264_AUD,
                &H264_SPS,
                &H264_PPS0,
                &H264_SEI_RP,
                &i_slice,
                &H264_AUD,
                &H264_SEI_PT,
                &p_field,
            ]),
            Pic::Intra => annex_b(&[&H264_AUD, &H264_SPS, &H264_PPS0, &H264_SEI_PT, &i_slice]),
            Pic::InterWithSets => annex_b(&[&H264_AUD, &H264_SPS, &H264_PPS0, &h264_slice(k)]),
            Pic::GdrP => annex_b(&[&H264_AUD, &H264_SEI_RP, &h264_slice(k)]),
        }
    }

    /// [`video_ts`] for H.264 pictures of the kinds `kind(k)` gives, the
    /// muxer marking the keyed ones random access; `kind(0)` must be one, so
    /// PSI precedes the first picture. It ends with a closing `Inter`
    /// picture, since the demuxer releases a PES only at the next one.
    fn h264_pics_ts(mux: &mut TsMuxer, pics: &[(u64, Option<u64>)], kind: impl Fn(usize) -> Pic) -> Vec<Bytes> {
        assert!(kind(0).keyed(), "PSI ahead of the first picture");
        let keys: Vec<usize> = (0..pics.len()).filter(|&k| kind(k).keyed()).collect();
        let au = |_: bool, k: usize| h264_pic(if k < pics.len() { kind(k) } else { Pic::Inter }, k as u8);
        video_ts_with(mux, false, pics, &keys, &au)
    }

    /// Byte 0 of each picture's tag: 0x17 keyed, 0x27 not.
    fn picture_frame_types(tags: &[WireTag]) -> Vec<u8> {
        tags.iter().filter(|t| t.video && tag_cts(&t.data).is_some()).map(|t| t.data[0]).collect()
    }

    /// The index, among the video tags, of each sequence header.
    fn header_positions(tags: &[WireTag]) -> Vec<usize> {
        let video: Vec<&WireTag> = tags.iter().filter(|t| t.video).collect();
        (0..video.len()).filter(|&i| tag_cts(&video[i].data).is_none()).collect()
    }

    const KEYS_AT_0_AND_6: [u8; 12] = [0x17, 0x27, 0x27, 0x27, 0x27, 0x27, 0x17, 0x27, 0x27, 0x27, 0x27, 0x27];

    /// edge#211, the reported failure: an IDR-less open-GOP source marks
    /// random access with an intra picture behind a recovery-point SEI. Every
    /// tag went out an inter frame and the sequence header was sent once. Such
    /// a picture is now keyed and the header re-sent ahead of it, as at an IDR
    /// — on a source with no IDR at all, one mixing IDRs among its recovery
    /// points, and one whose recovery point is an I / P field pair in one PES.
    /// Pictures 7 and 8 are picture 6's open-GOP leading pictures; the
    /// edge#205 timing is unchanged.
    #[tokio::test]
    async fn an_h264_recovery_point_is_keyed_like_an_idr() {
        let pics = ibbp(12, |k| T0 + k * F);
        let base = pics[0].1.unwrap_or(pics[0].0);
        for (case, first, sixth) in [
            ("no IDR", Pic::RecoveryPoint, Pic::RecoveryPoint),
            ("an IDR first", Pic::Idr, Pic::RecoveryPoint),
            ("a field pair", Pic::RecoveryPoint, Pic::RecoveryPointFieldPair),
        ] {
            let kind = |k: usize| match k {
                0 => first,
                6 => sixth,
                _ => Pic::Inter,
            };
            let tags = publish(h264_pics_ts(&mut TsMuxer::new(), &pics, kind), passthrough_output()).await;
            assert_wire_decode_order(&tags, &pics, base, false);
            assert_eq!(picture_frame_types(&tags), KEYS_AT_0_AND_6, "{case}");
            assert_eq!(header_positions(&tags), [0, 1, 8], "{case}: a header, then a re-send on each key");
            let headers = headers(&tags);
            assert!(headers.iter().all(|h| h.data == headers[0].data), "{case}: one header, re-sent");
        }
    }

    /// What the rule leaves out: an access unit carrying SPS / PPS, an intra
    /// picture with no recovery point, and a recovery point on a P picture
    /// (gradual decoding refresh) are not keyed and re-send no header. If GDR
    /// keying is adopted later, picture 6's expectation changes deliberately.
    #[tokio::test]
    async fn parameter_sets_an_intra_picture_or_a_gdr_point_alone_do_not_make_a_keyframe() {
        let pics = ibbp(12, |k| T0 + k * F);
        let kind = |k: usize| match k {
            0 => Pic::RecoveryPoint,
            3 | 9 => Pic::Intra,
            6 => Pic::GdrP,
            _ => Pic::InterWithSets,
        };
        let tags = publish(h264_pics_ts(&mut TsMuxer::new(), &pics, kind), passthrough_output()).await;
        let mut want = [0x27; 12];
        want[0] = 0x17;
        assert_eq!(picture_frame_types(&tags), want);
        assert_eq!(header_positions(&tags), [0, 1]);
        assert_wire_decode_order(&tags, &pics, pics[0].1.unwrap_or(pics[0].0), false);
    }

    /// A source with no recovery points — IDRs, an intra picture without the
    /// SEI, a P picture repeating its sets — goes out as it did before: the
    /// keyframe flag is the demuxer's IDR flag. Pinned literally and against
    /// a recomputation of every video tag (the first header taken from the
    /// wire, so the test does not depend on the avcC bytes); the byte-identity
    /// evidence for clean sources is the edge#205 wire tests passing
    /// unchanged.
    #[tokio::test]
    async fn an_idr_source_reaches_flv_byte_for_byte_as_before() {
        let pics = ibbp(12, |k| T0 + k * F);
        let kind = |k: usize| match k {
            0 | 6 => Pic::Idr,
            3 => Pic::Intra,
            9 => Pic::InterWithSets,
            _ => Pic::Inter,
        };
        let tags = publish(h264_pics_ts(&mut TsMuxer::new(), &pics, kind), passthrough_output()).await;
        assert_eq!(picture_frame_types(&tags), KEYS_AT_0_AND_6);
        assert_eq!(header_positions(&tags), [0, 1, 8]);

        let header = headers(&tags)[0].data.to_vec();
        let mut base = None;
        let mut want = Vec::new();
        for (k, &(pts, dts)) in pics.iter().enumerate() {
            let (ts, cts) = flv_video_time(pts, dts, &mut base);
            let nalus = crate::engine::ts_demux::split_annex_b_nalus(&h264_pic(kind(k), k as u8));
            let idr = kind(k) == Pic::Idr;
            if k == 0 {
                want.push((ts, header.clone()));
            }
            if idr {
                want.push((ts, header.clone()));
            }
            want.push((ts, build_avc_nalu_tag(&nalus, idr, cts)));
        }
        let got: Vec<(u32, Vec<u8>)> = tags.iter().filter(|t| t.video).map(|t| (t.ts, t.data.to_vec())).collect();
        assert_eq!(got, want);
    }

    // ── edge#211: one FLV timeline per connection, end to end ─────────────

    /// One lap of the source, F = 25 fps: 48 IBBP pictures (1 920 ms).
    const LAP_PICTURES: u64 = 48;

    /// The ADTS frames of the 1 kHz test tone, whole (header included),
    /// reused cyclically by the A/V fixtures.
    pub(super) fn adts_frames() -> Vec<&'static [u8]> {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let mut out = Vec::new();
        let mut off = 0;
        while off + 7 <= ADTS.len() {
            let len = (usize::from(ADTS[off + 3] & 0x03) << 11)
                | (usize::from(ADTS[off + 4]) << 3)
                | (usize::from(ADTS[off + 5]) >> 5);
            out.push(&ADTS[off..off + len]);
            off += len;
        }
        out
    }

    /// One stretch of a programme: pictures `(pts, dts)` in decode order and
    /// an AAC frame at each of `audio`; `pmt_bump` bumps the PMT version
    /// (a `Discontinuity`) ahead of it, and `audio_pid` moves the audio to
    /// another PID there. `stray` is a picture `(index, ticks)` whose PES is
    /// stamped that far ahead of its place, muxed where it belongs.
    pub(super) struct Lap {
        pub pics: Vec<(u64, Option<u64>)>,
        pub audio: Vec<u64>,
        pub pmt_bump: bool,
        pub audio_pid: Option<u16>,
        pub stray: Option<(usize, u64)>,
    }

    impl Lap {
        pub(super) fn new(pics: Vec<(u64, Option<u64>)>, audio: Vec<u64>) -> Self {
            Self { pics, audio, pmt_bump: false, audio_pid: None, stray: None }
        }
    }

    /// `laps` muxed one after another as one programme: in each, an AAC
    /// frame goes ahead of picture k iff it is stamped at or before k's
    /// decode time, the rest after the lap's last picture; key pictures at
    /// `keys` in each lap; a closing picture after the last lap, since the
    /// demuxer hands a video PES out only at the next one on its PID. Lap L's
    /// values go in plus L · 2^33, which the PES masks away: the muxer clamps
    /// its audio PTS monotonic, and would otherwise not let a loop through.
    pub(super) fn programme_ts(
        mux: &mut TsMuxer,
        au: &dyn Fn(bool, usize) -> Vec<u8>,
        laps: &[Lap],
        keys: &[usize],
    ) -> Vec<Bytes> {
        let frames = adts_frames();
        mux.set_has_audio(true);
        let mut out = Vec::new();
        let (mut version, mut a, mut end) = (0u8, 0usize, 0u64);
        for (l, lap) in laps.iter().enumerate() {
            let up = l as u64 * WRAP;
            if lap.pmt_bump {
                version += 1;
                mux.set_pmt_version(version);
            }
            if lap.audio_pid.is_some() {
                mux.set_pids(None, None, lap.audio_pid, None);
            }
            let mut audio = lap.audio.iter().peekable();
            for (k, &(pts, dts)) in lap.pics.iter().enumerate() {
                let decode = dts.unwrap_or(pts);
                while let Some(&&at) = audio.peek() {
                    if at > decode {
                        break;
                    }
                    out.extend(mux.mux_audio_pre_adts(frames[a % frames.len()], at + up));
                    a += 1;
                    audio.next();
                }
                let key = keys.contains(&k);
                let ahead = lap.stray.filter(|s| s.0 == k).map_or(0, |s| s.1);
                out.extend(mux.mux_video(&au(key, k), pts + up + ahead, decode + up + ahead, key));
            }
            for &at in audio {
                out.extend(mux.mux_audio_pre_adts(frames[a % frames.len()], at + up));
                a += 1;
            }
            end = lap.pics.iter().map(|p| p.0).max().unwrap() + F + up;
        }
        out.extend(mux.mux_video(&au(false, 0), end, end, false));
        out
    }

    const WRAP: u64 = 1 << 33;

    /// H.264 access units for [`programme_ts`].
    fn h264(key: bool, k: usize) -> Vec<u8> {
        h264_au(key, k as u8)
    }

    /// HEVC access units for [`programme_ts`].
    fn h265(key: bool, k: usize) -> Vec<u8> {
        h265_au(key, k as u8)
    }

    /// `ts` with the PTS (and DTS) of the PES starting on `pid` cleared from
    /// its header — `PTS_DTS_flags` 0, the bytes left for
    /// `PES_header_data_length` to skip — as a muxer that puts a field in a
    /// PES of its own without a time writes it.
    fn without_pts(mut ts: Vec<Bytes>, pid: u16) -> Vec<Bytes> {
        let at = ts
            .iter()
            .position(|p| p[1] & 0x40 != 0 && (u16::from(p[1] & 0x1F) << 8 | u16::from(p[2])) == pid)
            .expect("a PES start on the PID");
        let mut p = ts[at].to_vec();
        let payload = 4 + if p[3] & 0x20 != 0 { 1 + usize::from(p[4]) } else { 0 };
        assert_eq!(&p[payload..payload + 3], &[0, 0, 1], "a PES header");
        p[payload + 7] &= 0x3F;
        ts[at] = Bytes::from(p);
        ts
    }

    /// The video tags of `tags` that are pictures: `(timestamp, timestamp +
    /// composition time)` — decode ms and presentation ms.
    fn picture_times(tags: &[WireTag]) -> Vec<(u32, u32)> {
        tags.iter().filter(|t| t.video).filter_map(|t| Some((t.ts, t.ts + tag_cts(&t.data)?))).collect()
    }

    /// The timestamps of the AAC frames of `tags` (not the sequence header).
    fn audio_times(tags: &[WireTag]) -> Vec<u32> {
        tags.iter().filter(|t| !t.video && t.data[1] == 1).map(|t| t.ts).collect()
    }

    /// FLV's contract on the timeline: the video's tags never step back, the
    /// audio's strictly forward.
    fn assert_monotonic(tags: &[WireTag], what: &str) {
        let video: Vec<u32> = tags.iter().filter(|t| t.video).map(|t| t.ts).collect();
        assert!(video.windows(2).all(|w| w[0] <= w[1]), "{what}: the video steps back: {video:?}");
        let audio = audio_times(tags);
        assert!(audio.windows(2).all(|w| w[0] < w[1]), "{what}: the audio steps back: {audio:?}");
    }

    /// The first essence the demuxer hands out, which anchors the epoch:
    /// the first AAC frame when it is muxed ahead of the second picture —
    /// the first picture is handed out only when the second's PES begins —
    /// else the first picture's decode time.
    fn epoch(pics: &[(u64, Option<u64>)], a0: u64) -> u64 {
        let decode = |p: &(u64, Option<u64>)| p.1.unwrap_or(p.0);
        if a0 <= decode(&pics[1]) { a0 } else { decode(&pics[0]) }
    }

    /// edge#211 item 3: a looped source the PTS rewriter passes through (an
    /// MPTS input with no `program_number`). At each loop every tag stepped
    /// back to the epoch or below it and went out at 0, and the receiver's
    /// log filled with non-monotonic DTS. Each lap now continues the last,
    /// both tracks on one offset — the lap's own A/V relation — whichever
    /// meets the loop first and whatever the audio's length against the
    /// video's (85, 90 or 95 frames to a 1 920 ms lap): the audio meeting it
    /// first continues one frame on, and the video joins it (85 frames: 6
    /// 000 ticks back, two pictures held a ms on); the video first
    /// continues one picture on, and the audio joins it, its overlap dropped.
    #[tokio::test]
    async fn a_looped_source_keeps_both_tracks_monotonic_and_together() {
        let pics = ibbp(LAP_PICTURES, |k| T0 + k * F);
        let cases = [(false, T0 + F, 85u64), (false, T0 + F, 90), (false, T0 + F, 95)]
            .into_iter()
            .chain([(false, T0 + 3 * F, 85), (false, T0 + 3 * F, 90), (false, T0 + 3 * F, 95)])
            .chain([(true, T0 + F, 85)]);
        for (hevc, a0, m) in cases {
            let audio_first = a0 == T0 + F;
            let what = format!("hevc {hevc}, audio first {audio_first}, {m} audio frames a lap");
            let audio: Vec<u64> = (0..m).map(|j| a0 + j * 1_920).collect();
            let laps: Vec<Lap> = (0..3).map(|_| Lap::new(pics.clone(), audio.clone())).collect();
            let au: &dyn Fn(bool, usize) -> Vec<u8> = if hevc { &h265 } else { &h264 };
            let mut mux = TsMuxer::new();
            if hevc {
                mux.set_video_stream_type(0x24);
            }
            let tags = publish(programme_ts(&mut mux, au, &laps, &[0]), passthrough_output()).await;
            assert_monotonic(&tags, &what);

            let e = epoch(&pics, a0);
            let ms = |t: u64| (t.saturating_sub(e) / 90) as u32;
            let shift = |l: u64| if audio_first { l * m * 1_920 } else { l * LAP_PICTURES * F };
            let (mut want_pics, mut last_decode) = (Vec::new(), None::<u64>);
            let (mut want_audio, mut last_audio) = (Vec::new(), None::<u64>);
            for l in 0..3 {
                for &(pts, dts) in &pics {
                    let decode = dts.unwrap_or(pts) + shift(l);
                    let decode = last_decode.map_or(decode, |d| decode.max(d + 90));
                    last_decode = Some(decode);
                    want_pics.push((ms(decode), ms((pts + shift(l)).max(decode))));
                }
                for &a in &audio {
                    let a = a + shift(l);
                    if last_audio.is_none_or(|la| a > la) && a >= e {
                        want_audio.push(ms(a));
                        last_audio = Some(a);
                    }
                }
            }
            assert_eq!(picture_times(&tags), want_pics, "{what}: pictures");
            assert_eq!(audio_times(&tags), want_audio, "{what}: audio");
        }
    }

    /// edge#211 item 4, end to end: B-frames and AAC across the 33-bit wrap,
    /// the values masked into the PES as any source's are. Every tag lands
    /// at its unwrapped time; it used to go out at 0 for up to 26.5 h.
    #[tokio::test]
    async fn a_33_bit_wrap_mid_connection_keeps_counting() {
        let start = WRAP - 3 * 90_000;
        let pics = ibbp(150, |k| start + k * F);
        let audio: Vec<u64> = (0..280).map(|j| start + 3 * F + j * 1_920).collect();
        let lap = Lap::new(pics.clone(), audio.clone());
        let tags = publish(programme_ts(&mut TsMuxer::new(), &h264, &[lap], &[0, 75]), passthrough_output()).await;
        let d0 = pics[0].1.unwrap();
        assert!(assert_wire_decode_order(&tags, &pics, d0, false) > 0);
        let want: Vec<u32> = audio.iter().map(|&a| ((a - d0) / 90) as u32).collect();
        assert_eq!(audio_times(&tags), want);
        assert!(want.last().unwrap() > &5_000, "past the wrap");
    }

    /// An input switch is a PMT bump, and a `Discontinuity`, which the FLV
    /// timeline does not need: (a) onto a feed 5 000 s on, whose audio runs
    /// 5 frames behind its video where the first's ran one — the new feed
    /// goes on one picture on, in its own A/V relation; (b) onto a feed on
    /// the same clock — every tag at its source time, nothing re-anchored.
    #[tokio::test]
    async fn a_discontinuity_needs_no_re_anchor_with_or_without_a_jump() {
        let feed = |base: u64, lead: u64, frames: u64, pmt_bump: bool| Lap {
            pmt_bump,
            ..Lap::new(ibbp(LAP_PICTURES, |k| base + k * F), (0..frames).map(|j| base + lead + j * 1_920).collect())
        };
        // (a) a jump: feed 2's picture 13 presents at the instant its audio
        // frame 15 is stamped, and goes out on the same ms.
        let far = T0 + 5_000 * 90_000;
        let laps = [feed(T0, F, 85, false), feed(far, 5 * F, 85, true)];
        let tags = publish(programme_ts(&mut TsMuxer::new(), &h264, &laps, &[0]), passthrough_output()).await;
        assert_monotonic(&tags, "a jump");
        assert_eq!(laps[1].pics[13].0, laps[1].audio[15], "the fixture's instant");
        let pictures = picture_times(&tags);
        assert_eq!(pictures.len(), 2 * LAP_PICTURES as usize);
        // Strictly forward past the first, which decodes before the epoch
        // the audio set and goes out at 0 with the second.
        assert!(pictures[1..].windows(2).all(|w| w[0].0 < w[1].0), "every picture timed, strictly forward");
        let audio = audio_times(&tags);
        assert_eq!(audio.len(), 2 * 85, "no audio dropped");
        assert_eq!(pictures[LAP_PICTURES as usize + 13].1, audio[85 + 15], "feed 2's A/V relation");
        assert_eq!(
            pictures[LAP_PICTURES as usize].0,
            pictures[LAP_PICTURES as usize - 1].0 + 40,
            "feed 2 one picture on",
        );

        // (b) no jump: the second feed continues the first's clock.
        let laps = [feed(T0, F, 90, false), feed(T0 + LAP_PICTURES * F, F, 90, true)];
        assert_eq!(laps[1].audio[0], laps[0].audio[89] + 1_920, "the fixture's audio is contiguous");
        let tags = publish(programme_ts(&mut TsMuxer::new(), &h264, &laps, &[0]), passthrough_output()).await;
        let pics: Vec<(u64, Option<u64>)> = laps.iter().flat_map(|l| l.pics.iter().copied()).collect();
        let e = epoch(&pics, laps[0].audio[0]);
        assert_wire_decode_order(&tags, &pics, e, false);
        let want: Vec<u32> = laps.iter().flat_map(|l| l.audio.iter()).map(|&a| ((a - e) / 90) as u32).collect();
        assert_eq!(audio_times(&tags), want, "every frame at its source time");
    }

    /// edge#211 item 5: a PAFF source remuxed with each second field in a PES
    /// of its own and no PTS. The demuxer hands such a field over as PTS 0,
    /// and passthrough stamped it 0 — a decode time stepping back to the
    /// epoch every other picture. Each field now takes its first field's
    /// time.
    #[tokio::test]
    async fn a_picture_whose_pes_carried_no_pts_takes_its_predecessors_time() {
        let mut mux = TsMuxer::new();
        let mut ts = Vec::new();
        for f in 0..20u64 {
            let at = T0 + f * F;
            ts.extend(mux.mux_video(&h264_au(f == 0, (2 * f) as u8), at, at, f == 0));
            let second = mux.mux_video(&h264_au(false, (2 * f + 1) as u8), at + F / 2, at + F / 2, false);
            ts.extend(without_pts(second, 0x100));
        }
        ts.extend(mux.mux_video(&h264_au(false, 40), T0 + 20 * F, T0 + 20 * F, false));
        let tags = publish(ts, passthrough_output()).await;
        let pictures = picture_times(&tags);
        assert_eq!(pictures.len(), 40);
        for (f, pair) in pictures.chunks(2).enumerate() {
            assert_eq!(pair[0], pair[1], "frame {f}: the second field on its first's time");
            assert_eq!(pair[0], (f as u32 * 40, f as u32 * 40), "frame {f}");
        }
        assert!(pictures[2..].iter().all(|p| p.0 > 0), "nothing at 0 after the first frame");
    }

    /// One picture stamped 2 s ahead (PTS and DTS): placed one frame on —
    /// where it belongs — and the pictures after it back on the programme's
    /// clock, so every tag lands at its unshifted time. Taken as a gap it
    /// would have put the video 2 s off the audio for good.
    #[tokio::test]
    async fn a_stray_picture_two_seconds_ahead_is_placed_one_frame_on() {
        let pics = ibbp(100, |k| T0 + k * F);
        let audio: Vec<u64> = (0..185).map(|j| T0 + F + j * 1_920).collect();
        let lap = Lap { stray: Some((50, 180_000)), ..Lap::new(pics.clone(), audio.clone()) };
        let tags = publish(programme_ts(&mut TsMuxer::new(), &h264, &[lap], &[0]), passthrough_output()).await;
        let e = epoch(&pics, audio[0]);
        assert_wire_decode_order(&tags, &pics, e, false);
        // Strictly forward past the first, which decodes before the epoch
        // the audio set and goes out at 0 with the second.
        let pictures = picture_times(&tags);
        assert!(pictures[1..].windows(2).all(|w| w[0].0 < w[1].0));
        let want: Vec<u32> = audio.iter().map(|&a| ((a - e) / 90) as u32).collect();
        assert_eq!(audio_times(&tags), want);
    }

    /// Transcoded audio beside passed-through video across loops of a
    /// source whose audio (85 frames, 1 813 ms) is shorter than its video
    /// (1 920 ms). The encoder anchored once and counted samples, so it ran
    /// 107 ms further from the video every lap; it is now re-anchored at each
    /// seam, where the video continues: the audio's first frame of lap L is
    /// L × 1 920 ms after its first. The seam is the only place the audio
    /// steps by more than a frame.
    ///
    /// The source's audio starting just past the video's second picture, the
    /// encoder's first frame — stamped its codec delay before that — falls
    /// before the epoch the first picture anchored: it is dropped, where it
    /// used to go out at 0.
    #[cfg(feature = "fdk-aac")]
    #[tokio::test]
    async fn transcoded_audio_stays_with_passed_through_video_across_loops() {
        let pics = ibbp(LAP_PICTURES, |k| T0 + k * F);
        let output = serde_json::json!({
            "id": "o", "name": "o", "dest_url": "rtmp://127.0.0.1/live", "stream_key": "k",
            "audio_encode": { "codec": "aac_lc", "bitrate_kbps": 96 }
        });
        // Video first both ways: 120 ms of audio lead, past the codec's
        // 42.7 ms delay, and 41 ms, inside it.
        for a0 in [T0 + 3 * F, T0 + F + 90] {
            let audio: Vec<u64> = (0..85).map(|j| a0 + j * 1_920).collect();
            assert_eq!(epoch(&pics, a0), T0, "video first");
            let laps: Vec<Lap> = (0..4).map(|_| Lap::new(pics.clone(), audio.clone())).collect();
            let tags = publish(programme_ts(&mut TsMuxer::new(), &h264, &laps, &[0]), output.clone()).await;
            assert_monotonic(&tags, "transcoded");
            let audio = audio_times(&tags);
            assert!(audio.len() > 300, "{} AAC frames", audio.len());
            assert!(!audio.contains(&0), "no audio stacked at 0: {:?}", &audio[..4]);
            let seams: Vec<usize> = (1..audio.len()).filter(|&i| audio[i] - audio[i - 1] > 32).collect();
            assert_eq!(seams.len(), 3, "a0 {a0}: one step a seam: {seams:?}");
            if a0 == T0 + 3 * F {
                assert!((70..120).contains(&audio[0]), "the first frame, its codec delay early: {}", audio[0]);
                for (l, &i) in seams.iter().enumerate() {
                    let lap = (l as i64 + 1) * 1_920;
                    let got = i64::from(audio[i]) - i64::from(audio[0]);
                    assert!((got - lap).abs() <= 1, "lap {}: {got} ms after the first, not {lap}", l + 1);
                }
            }
            assert_eq!(picture_times(&tags).len(), 4 * LAP_PICTURES as usize);
        }
    }

    /// A switch to a feed on another audio PID re-creates the demuxer's
    /// audio framer, and the new feed's first audio PES carries no PTS: its
    /// frame is handed out stamped 0. The timeline takes that for a jump and
    /// continues the audio one frame on, and the next frame brings it back to
    /// its source time: the tags go on forward, none at 0, and at most one
    /// frame is lost.
    #[tokio::test]
    async fn an_audio_pes_without_a_pts_after_a_discontinuity_costs_at_most_one_frame() {
        let pics = ibbp(2 * LAP_PICTURES, |k| T0 + k * F);
        let audio: Vec<u64> = (0..180).map(|j| T0 + F + j * 1_920).collect();
        let laps = [
            Lap::new(pics[..48].to_vec(), audio[..90].to_vec()),
            Lap {
                pmt_bump: true,
                audio_pid: Some(0x0102),
                ..Lap::new(pics[48..].to_vec(), audio[90..].to_vec())
            },
        ];
        let ts = without_pts(programme_ts(&mut TsMuxer::new(), &h264, &laps, &[0]), 0x0102);
        let tags = publish(ts, passthrough_output()).await;
        assert_monotonic(&tags, "a PTS-less audio PES");
        let got = audio_times(&tags);
        assert!(got.len() + 1 >= audio.len(), "{} of {} frames", got.len(), audio.len());
        assert!(got[1..].iter().all(|&t| t > 0), "none at 0 after the start");
        assert_wire_decode_order(&tags, &pics, epoch(&pics, audio[0]), false);
    }

    /// The lone gap end to end: a video-only source pausing 3 s keeps the
    /// pause, as passthrough always did, where a timeline closing every
    /// pause past a second would have run its clock behind the source's.
    #[tokio::test]
    async fn a_video_only_source_pausing_three_seconds_keeps_the_pause() {
        let pics: Vec<(u64, Option<u64>)> =
            (0..50).map(|k| (T0 + k * F + if k >= 25 { 3 * 90_000 } else { 0 }, None)).collect();
        let tags = publish(video_ts(&mut TsMuxer::new(), false, &pics, &[0, 25]), passthrough_output()).await;
        assert_wire_decode_order(&tags, &pics, T0, false);
        let pictures = picture_times(&tags);
        assert_eq!(pictures[25].0 - pictures[24].0, 3_040, "the pause kept");
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
