// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WebRTC output tasks: WHIP client (push to server) and WHEP server (serve viewers).
//!
//! Both modes extract H.264 NALUs from MPEG-TS broadcast channel packets,
//! packetize per RFC 6184, and send via str0m WebRTC sessions.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::WebrtcOutputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;

#[cfg(feature = "webrtc")]
use super::audio_decode::{AacDecoder, DecodeStats, sample_rate_from_index};
#[cfg(feature = "webrtc")]
use super::audio_encode::{AudioCodec, AudioEncoder, AudioEncoderError, EncoderParams};
#[cfg(feature = "webrtc")]
use super::audio_silence::SilenceGenerator;
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
use super::ts_video_replace::VideoEncodeStats;
#[cfg(feature = "webrtc")]
use crate::config::models::VideoEncodeConfig;
use super::packet::RtpPacket;

/// Per-output encoder state for the WebRTC audio_encode bridge. Mirrors the
/// pattern in [`super::output_rtmp`] but tailored to Opus output: there is
/// no Transparent same-codec fast path because the source is always AAC and
/// WebRTC always emits Opus.
#[cfg(feature = "webrtc")]
enum WebrtcEncoderState {
    /// audio_encode unset, or video_only=true. Drop audio frames silently
    /// (preserves the existing pre-Phase B behavior).
    Disabled,
    /// audio_encode set; encoder will be built on first AAC frame.
    Lazy,
    /// Decoder + encoder running; each AAC frame goes through decode → encode
    /// → write_media to the WebRTC audio MID.
    ///
    /// When `silent_fallback` is set, the encoder is built eagerly at
    /// session startup (before any source audio arrives) and the decoder
    /// is filled lazily on the first real AAC frame — hence it is an
    /// `Option`; the stage, pinned to the encoder's format, is built with
    /// the encoder.
    Active {
        /// `None` until the first real AAC frame arrives (silent-fallback
        /// builds the encoder ahead of any source audio).
        decoder: Option<AacDecoder>,
        encoder: AudioEncoder,
        decode_stats: Arc<DecodeStats>,
        /// The channel / rate stage between the decoder and the Opus
        /// encoder (`audio_transcode::encoder_stage`): the `transcode`
        /// block, or `audio_encode.sample_rate` / `channels` alone when
        /// they differ from the source, converting to the format the
        /// encoder was opened at, in streaming mode (its constant delay is
        /// taken off the encoder's stamps). An MP2 / AC-3 / E-AC-3 source
        /// goes through it too.
        stage: super::audio_transcode::EncoderStage,
        /// Silent-PCM generator + audio-drop watchdog. `Some` iff
        /// `audio_encode.silent_fallback = true`.
        silence: Option<SilenceGenerator>,
    },
    /// Decoder or encoder construction failed once. Drop audio for the
    /// rest of the session's lifetime.
    Failed,
}

/// Per-WebRTC-session video transcoding state. Parallels
/// [`WebrtcEncoderState`] but for video: opens the decoder+encoder on
/// the first video access unit and transitions to `Active` for the
/// rest of the session. RTMP's `VideoEncoderState` is the closest
/// analogue, except RTMP builds an out-of-band FLV sequence header
/// (`global_header = true`) while WebRTC emits SPS/PPS inline on every
/// IDR (`global_header = false`) so str0m's packetizer can feed them into
/// RTP as ordinary NAL units.
#[cfg(feature = "webrtc")]
enum WebrtcVideoEncoderState {
    /// `video_encode` unset. Passthrough H.264, drop HEVC (pre-Phase 4d).
    Disabled,
    /// `video_encode` set; decoder+encoder will be built on the first
    /// source access unit that can open them — for H.264, the first that
    /// carries the SPS (`SpsOpenGate`), so the decoder's reorder depth is
    /// seeded from it.
    #[cfg(feature = "media-codecs")]
    Lazy {
        cfg: VideoEncodeConfig,
        sps_gate: crate::engine::video_encode_util::SpsOpenGate,
    },
    /// Decode → re-encode pipeline is live.
    #[cfg(feature = "media-codecs")]
    Active(Box<WebrtcVideoActive>),
    /// Decoder or encoder construction failed once. Drop video for the
    /// rest of the session's lifetime.
    Failed,
}

#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
struct WebrtcVideoActive {
    decoder: video_engine::VideoDecoder,
    /// Shared encoder pipeline — wraps `VideoEncoder` + optional
    /// `VideoScaler`. WebRTC uses `global_header = false` because every
    /// IDR already carries in-band SPS/PPS that the RFC 6184 packetizer
    /// forwards verbatim; no out-of-band codec config channel is
    /// needed. Lazy-opens on the first decoded frame.
    pipeline: crate::engine::video_encode_util::ScaledVideoEncoder,
    /// Monotonic PTS counter in encoder time base. We pass the source
    /// 90 kHz PTS to `write_media` for correct lip-sync, but the encoder
    /// itself gets a monotonic `out_frame_count` so libx264 / NVENC rate
    /// control behaves predictably.
    out_frame_count: i64,
    /// The encoder rate — pinned, or measured from the decoded frames before
    /// the encoder opens (it used to open at a flat 30/1).
    rate: crate::engine::video_encode_util::EncoderRateLock,
    /// The source PTS of each frame in the encoder, by `out_frame_count`:
    /// what an encoded frame's RTP timestamp is.
    in_flight: crate::engine::video_encode_util::EncodedPtsMap,
    /// The source PTS of each decoded picture (see `FramePtsStamper`).
    stamper: crate::engine::video_encode_util::FramePtsStamper,
    stats: Arc<VideoEncodeStats>,
}

/// Resolve a `video_encode.codec` string into a [`VideoEncoderCodec`] for
/// WebRTC. Only H.264 backends are allowed; validation rejects HEVC at
/// config-load, but guard against it at runtime too. Honours `h264_auto`
/// → resolver-picked H.264 backend on the host.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn resolve_webrtc_video_backend(
    cfg: &VideoEncodeConfig,
    output_id: &str,
    flow_id: &str,
    event_sender: &EventSender,
) -> Option<video_codec::VideoEncoderCodec> {
    let codec = cfg.codec.as_str();
    // Auto path: route through the encoder resolver but cap the result
    // at H.264 — if the resolver picked an HEVC backend (because Auto
    // got hevc_auto / auto), reject with the same not-supported event
    // shape. WebRTC browsers only decode H.264.
    if (codec == "h264_auto" || codec == "auto")
        && crate::engine::hardware_probe::static_capabilities().is_some() {
            match crate::engine::hardware_probe::resolve_for_video_encode_config(cfg) {
                Ok(r) if r.family() == crate::engine::hardware_probe::EncoderFamily::H264 => {
                    tracing::info!(
                        "WebRTC output '{}': video_encode auto-resolved '{}' → {}",
                        output_id,
                        codec,
                        r.ffmpeg_name(),
                    );
                    return Some(r.as_video_encoder_codec());
                }
                Ok(other) => {
                    let msg = format!(
                        "WebRTC output '{}': video_encode auto-resolved to {} but WebRTC browsers only decode H.264",
                        output_id,
                        other.ffmpeg_name(),
                    );
                    tracing::error!("{msg}");
                    event_sender.emit_flow(
                        EventSeverity::Critical,
                        category::VIDEO_ENCODE,
                        msg,
                        flow_id,
                    );
                    return None;
                }
                Err(e) => {
                    let msg = format!(
                        "WebRTC output '{}': video_encode unavailable: {}",
                        output_id,
                        e.message()
                    );
                    tracing::error!("{msg}");
                    event_sender.emit_flow(
                        EventSeverity::Critical,
                        category::VIDEO_ENCODE,
                        msg,
                        flow_id,
                    );
                    return None;
                }
            }
        }
        // No probe snapshot → fall through to legacy mapping (which
        // doesn't recognise auto, so reports unknown_codec). Production
        // always has caps installed; this is just a test-path fallback.
    match codec {
        "x264" => Some(video_codec::VideoEncoderCodec::X264),
        "h264_nvenc" => Some(video_codec::VideoEncoderCodec::H264Nvenc),
        "h264_qsv" => Some(video_codec::VideoEncoderCodec::H264Qsv),
        "h264_vaapi" => Some(video_codec::VideoEncoderCodec::H264Vaapi),
        // h264_rkmpp produces baseline/main/high H.264 — browser-decodable.
        // (hevc_rkmpp is intentionally omitted; WebRTC is H.264-only here.)
        "h264_rkmpp" => Some(video_codec::VideoEncoderCodec::H264Rkmpp),
        other => {
            let msg = format!(
                "WebRTC output '{}': video_encode codec '{other}' not supported — WebRTC browsers only decode H.264",
                output_id
            );
            tracing::error!("{msg}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                msg,
                flow_id,
            );
            None
        }
    }
}

/// Initialise the per-session video encoder state. Called once per WHEP
/// viewer loop / WHIP client loop at startup. Returns `Disabled` when
/// `video_encode` is unset so the hot path can passthrough H.264 without
/// any extra branches.
#[cfg(feature = "webrtc")]
fn init_webrtc_video_encoder_state(
    video_encode: Option<&VideoEncodeConfig>,
) -> WebrtcVideoEncoderState {
    match video_encode {
        None => WebrtcVideoEncoderState::Disabled,
        #[cfg(feature = "media-codecs")]
        Some(cfg) => WebrtcVideoEncoderState::Lazy {
            cfg: cfg.clone(),
            sps_gate: crate::engine::video_encode_util::SpsOpenGate::new(),
        },
        #[cfg(not(feature = "media-codecs"))]
        Some(_) => WebrtcVideoEncoderState::Failed,
    }
}

/// Open the decoder+stats for a WebRTC video_encode pipeline and flip
/// the state into `Active`. Mirrors RTMP's `open_video_active` but
/// specialised to WebRTC's H.264-only output constraint.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn open_webrtc_video_active(
    cfg: &VideoEncodeConfig,
    source_codec: video_codec::VideoCodec,
    first_au: &[u8],
    output_id: &str,
    flow_id: &str,
    output_stats: &Arc<OutputStatsAccumulator>,
    event_sender: &EventSender,
) -> WebrtcVideoEncoderState {
    let Some(backend) = resolve_webrtc_video_backend(cfg, output_id, flow_id, event_sender)
    else {
        return WebrtcVideoEncoderState::Failed;
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
                "WebRTC output '{}': video_encode failed to open decoder for {:?}: {e}",
                output_id, source_codec
            );
            tracing::error!("{msg}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::VIDEO_ENCODE,
                msg,
                flow_id,
            );
            return WebrtcVideoEncoderState::Failed;
        }
    };
    let stats_handle = Arc::new(VideoEncodeStats::default());
    let backend_tag = match backend {
        video_codec::VideoEncoderCodec::X264 => "x264",
        video_codec::VideoEncoderCodec::H264Nvenc => "nvenc",
        video_codec::VideoEncoderCodec::H264Qsv => "qsv",
        video_codec::VideoEncoderCodec::H264Vaapi => "vaapi",
        video_codec::VideoEncoderCodec::H264Rkmpp => "rkmpp",
        _ => "unknown",
    };
    output_stats.set_video_encode_stats(
        stats_handle.clone(),
        String::new(),
        "h264".to_string(),
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
        "WebRTC output '{}': video_encode active ({} @ {} kbps, source {:?})",
        output_id,
        backend_tag,
        cfg.bitrate_kbps.unwrap_or(4000),
        source_codec,
    );
    event_sender.emit_flow(
        EventSeverity::Info,
        category::VIDEO_ENCODE,
        format!("Video encoder started: output '{}'", output_id),
        flow_id,
    );
    // Unpinned, the encoder opens at the source's measured rate: the
    // placeholder here is replaced when `EncoderRateLock` locks.
    let (pinned, (fps_num, fps_den)) = match (cfg.fps_num, cfg.fps_den) {
        (Some(n), Some(d)) => (true, (n, d)),
        _ => (false, crate::engine::video_encode_util::RATE_LOCK_FALLBACK),
    };
    let mut pipeline = crate::engine::video_encode_util::ScaledVideoEncoder::new(
        cfg.clone(),
        backend,
        fps_num,
        fps_den,
        false,
        format!("WebRTC output '{}'", output_id),
    );
    pipeline.set_resolved_backend_sink(stats_handle.resolved_backend.clone());
    // An interlaced frame from this decoder is a woven frame (H.264) or a
    // single field (HEVC field_seq) — which sets the geometry its sample
    // aspect ratio describes.
    pipeline.set_source_codec(source_codec);
    WebrtcVideoEncoderState::Active(Box::new(WebrtcVideoActive {
        decoder,
        pipeline,
        out_frame_count: 0,
        rate: crate::engine::video_encode_util::EncoderRateLock::new(pinned),
        in_flight: Default::default(),
        stamper: Default::default(),
        stats: stats_handle,
    }))
}

/// Answer a peer's keyframe request (PLI / FIR) — a viewer that lost
/// packets, or one back from an outage, whose decoder waits for an IDR —
/// by making the encoder's next frame one. `false` when this end does not
/// encode the video (no `video_encode`, or not open yet): passed-through
/// H.264 waits for the source's next IDR, there being nothing to make one
/// from. The request used to be ignored either way.
#[cfg(feature = "webrtc")]
fn force_video_keyframe(video_state: &mut WebrtcVideoEncoderState) -> bool {
    match video_state {
        #[cfg(feature = "media-codecs")]
        WebrtcVideoEncoderState::Active(active) => {
            active.pipeline.force_next_keyframe();
            true
        }
        _ => false,
    }
}

/// Concatenate source NAL units (no start codes) back into an Annex-B
/// byte stream suitable for `VideoDecoder::send_packet`.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn nalus_to_annex_b_webrtc(nalus: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nalu in nalus {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nalu);
    }
    out
}

/// Push one source access unit (stamped `pts`) through the decoder +
/// encoder and return every encoded frame — Annex B, with any emitted
/// SPS/PPS inline on IDRs — with the source PTS of the picture it codes.
/// Flips `video_state` to `Failed` on a terminal error; returns an empty
/// vec while the encoder opens (same convention as RTMP).
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn encode_one_video_frame_webrtc(
    video_state: &mut WebrtcVideoEncoderState,
    annex_b: &[u8],
    pts: u64,
    pts_known: bool,
    output_id: &str,
) -> Vec<(Vec<u8>, u64)> {
    let active = match video_state {
        WebrtcVideoEncoderState::Active(a) => a,
        _ => return Vec::new(),
    };
    let block_result: Result<Vec<(Vec<u8>, u64)>, String> = crate::timed_block_in_place!(
        "output_webrtc.video_encoder",
        crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
        {
            active.stats.input_frames.fetch_add(1, Ordering::Relaxed);
            // The access unit's PTS goes in so each decoded frame carries its
            // own — what the rate lock measures the source's cadence on. One
            // whose PES carried none goes in without (fed as 0, its picture
            // came back stamped 0).
            let fed = if pts_known {
                active.decoder.send_packet_with_pts(annex_b, pts as i64)
            } else {
                active.decoder.send_packet(annex_b)
            };
            if let Err(e) = fed {
                tracing::debug!("WebRTC output '{}': decoder send_packet: {e:?}", output_id);
            }
            let mut out: Vec<(Vec<u8>, u64)> = Vec::new();
            loop {
                let frame = match active.decoder.receive_frame() {
                    Ok(f) => f,
                    Err(_) => break,
                };
                // Until the rate is known the encoder cannot open (its time
                // base is fixed at open): the frame is dropped.
                let admitted = active.rate.admit(frame.pts(), &mut active.pipeline);
                // The picture's own source PTS (display order), or from the
                // pictures before it when its PES carried none.
                let interval =
                    crate::engine::video_encode_util::frame_interval_90k(active.pipeline.fps());
                let stamped = active.stamper.stamp(frame.pts(), interval);
                if !admitted {
                    continue;
                }
                let frame_pts = stamped.unwrap_or(pts);
                active.in_flight.push(active.out_frame_count, Some(frame_pts));
                let encoded_frames = match active.pipeline.encode(&frame, Some(active.out_frame_count)) {
                    Ok(frames) => frames,
                    Err(e) => {
                        active.in_flight.cancel_last();
                        if !active.pipeline.is_open() {
                            return Err(format!("encoder open failed: {e}"));
                        }
                        tracing::debug!("WebRTC output '{}': encode error: {e}", output_id);
                        active.stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                active.out_frame_count += 1;
                for ef in encoded_frames {
                    let coded_pts = active.in_flight.take(ef.pts).unwrap_or(frame_pts);
                    out.push((ef.data, coded_pts));
                    active.stats.output_frames.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok::<_, String>(out)
        }
    );

    // Only encoder *open* failure flips us to Failed. Decoder priming
    // (no frames produced yet on the first few input access units)
    // returns Ok(vec![]) — keep state Active and try again next frame.
    match block_result {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("WebRTC output '{}': video_encode: {e}", output_id);
            *video_state = WebrtcVideoEncoderState::Failed;
            Vec::new()
        }
    }
}

/// Which `codec_needs_encode` Warnings a WebRTC session (a WHEP viewer, or
/// one WHIP connection) has raised — each at most once per session.
#[cfg(feature = "webrtc")]
#[derive(Default)]
struct NeedsEncodeWarned {
    /// MPEG-2 or HEVC video with no `video_encode`.
    video: bool,
    /// Decodable non-Opus audio with no `audio_encode`.
    audio: bool,
    /// Multistream Opus (more than two channels, or dual mono), which an
    /// RTP Opus track cannot carry.
    opus_layout: bool,
}

/// Tell the operator, once per session, that the source's `source_codec`
/// reaches a WebRTC output only through `needs` (WebRTC carries H.264 and
/// Opus), so without it that essence is dropped. Output-scoped, with the
/// flow in `details` — the shape RTMP raises it in.
#[cfg(feature = "webrtc")]
fn warn_codec_needs_encode(
    output_id: &str,
    flow_id: &str,
    events: &EventSender,
    essence: &str,
    source_codec: &str,
    needs: &str,
) {
    let (carries, block) = if essence == "video" {
        ("H.264 only", "`video_encode` (or `webrtc_compatible`)")
    } else {
        ("Opus only", "`audio_encode`")
    };
    let msg = format!(
        "WebRTC output '{output_id}': the source's {source_codec} cannot be carried over WebRTC \
         ({carries}) without {block}; its {essence} is dropped"
    );
    tracing::warn!("{msg}");
    events.emit_output_with_details(
        EventSeverity::Warning,
        category::WEBRTC,
        msg,
        output_id,
        serde_json::json!({
            "error_code": "codec_needs_encode",
            "essence": essence,
            "source_codec": source_codec,
            "needs": needs,
            "flow_id": flow_id,
        }),
    );
}

/// The label a decodable non-Opus source audio frame gets in a
/// `codec_needs_encode` Warning, when the session would carry audio
/// (`audio_negotiated`) but has no `audio_encode` to make Opus of it —
/// `None` when the Warning is not owed. AAC (ADTS) only when it is AAC-LC,
/// the one profile the Opus re-encode takes.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn audio_needs_encode_label(
    frame: &super::ts_demux::DemuxedFrame,
    demuxer: &super::ts_demux::TsDemuxer,
    audio_encode_set: bool,
    audio_negotiated: bool,
) -> Option<String> {
    if audio_encode_set || !audio_negotiated {
        return None;
    }
    match frame {
        super::ts_demux::DemuxedFrame::Aac { .. } => {
            matches!(demuxer.cached_aac_config(), Some((1, _, _))).then(|| "AAC audio".to_string())
        }
        super::ts_demux::DemuxedFrame::OtherAudio { stream_type, .. } => {
            crate::engine::audio_decode::reencodable_audio_label(*stream_type)
        }
        _ => None,
    }
}

/// Tell the operator, once per session, that the source's Opus is a
/// multistream layout (`channel_config_code`, from its PMT) — more than two
/// channels, or dual mono — which an RTP Opus track does not carry, so its
/// audio is dropped. `audio_encode` cannot help: the decoder here takes a
/// single Opus stream (a multistream needs the channel mapping it is not
/// given). Output-scoped, with the flow in `details`.
#[cfg(feature = "webrtc")]
fn warn_opus_layout_not_carried(
    output_id: &str,
    flow_id: &str,
    events: &EventSender,
    channel_config_code: u8,
) {
    let layout = super::webrtc::opus_passthrough::describe(channel_config_code);
    let msg = format!(
        "WebRTC output '{output_id}': the source's {layout} is a multistream Opus layout WebRTC does not \
         carry (one mono or stereo Opus stream only); its audio is dropped"
    );
    tracing::warn!("{msg}");
    events.emit_output_with_details(
        EventSeverity::Warning,
        category::WEBRTC,
        msg,
        output_id,
        serde_json::json!({
            "error_code": "opus_layout_unsupported",
            "channel_config_code": channel_config_code,
            "flow_id": flow_id,
        }),
    );
}

/// What a WebRTC output does with a peer that accepted no H.264 video — the
/// one video codec it sends (`webrtc_no_h264`).
#[cfg(feature = "webrtc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoH264 {
    /// A WHEP viewer that did accept Opus, which this output can send it: it
    /// is sent the audio alone.
    AudioOnly,
    /// A WHEP viewer this output cannot serve: its request is refused, 400.
    Refused(WhepRefusal),
    /// The WHIP endpoint's answer: the resource is deleted and the publish
    /// retried after this many seconds.
    Retrying(u64),
}

/// Why a WHEP viewer is refused at answer time: its request gets 400, with
/// [`Self::reason`] as the body's text.
#[cfg(feature = "webrtc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WhepRefusal {
    /// It accepted no H.264, and no Opus either — or the output is
    /// `video_only`.
    NothingToSend,
    /// It accepted no H.264 but Opus, and this output has no Opus to send:
    /// no `audio_encode`, and the source's audio is not Opus it can pass
    /// through. Such a viewer used to be admitted, then sent nothing.
    NoOpusToSend,
    /// It accepted no H.264 but Opus, the output has no `audio_encode`, and
    /// the source's PMT has not been read yet (the output has just started,
    /// the flow restarted, the source is not flowing), so whether it has
    /// Opus to pass through is not known. Worth retrying: the same offer is
    /// admitted once the PMT says Opus. It used to be refused as
    /// `NoOpusToSend`, the Warning saying the source's audio was not Opus.
    SourceAudioUnknown,
    /// It accepted no H.264, and its video m-line is the offer's BUNDLE tag,
    /// which an answer may not reject (RFC 8843 §7.3.3) — browsers put video
    /// first. Chrome refuses the answer ("Failed to setup RTCP mux"), so this
    /// viewer used to get a 201 it could not apply.
    VideoIsBundleTag,
    /// It accepted H.264, but the BUNDLE tag the answer must keep is an
    /// m-line it accepted nothing on (an audio m-line without Opus).
    BundleTagRejected,
}

#[cfg(feature = "webrtc")]
impl WhepRefusal {
    /// The refusal's text, after `WHEP offer refused: ` in the 400's body.
    fn reason(self) -> &'static str {
        match self {
            Self::NothingToSend => "it accepts no H.264 video, and no Opus audio this output sends",
            Self::NoOpusToSend => {
                "it accepts no H.264 video, and this output has no Opus to send it alone: \
                 without an audio_encode it sends audio only from an Opus source"
            }
            Self::SourceAudioUnknown => {
                "it accepts no H.264 video, and this output has not yet read its source's \
                 audio to know whether it has Opus to send it alone: retry in a moment"
            }
            Self::VideoIsBundleTag => {
                "it accepts no H.264 video, and its video m-line is the BUNDLE tag, which an answer \
                 may not reject (RFC 8843 section 7.3.3): offer H.264, or put the audio m-line first"
            }
            Self::BundleTagRejected => {
                "its first bundled m-line accepts nothing this output sends, and an answer may not \
                 reject the BUNDLE tag (RFC 8843 section 7.3.3)"
            }
        }
    }

    /// `details.reason` on the `webrtc_no_h264` Warning.
    fn code(self) -> &'static str {
        match self {
            Self::NothingToSend => "nothing_to_send",
            Self::NoOpusToSend => "no_opus_to_send",
            Self::SourceAudioUnknown => "source_audio_unknown",
            Self::VideoIsBundleTag => "video_is_bundle_tag",
            Self::BundleTagRejected => "bundle_tag_rejected",
        }
    }
}

/// Tell the operator that `peer` (`"WHEP viewer"`, `"WHIP endpoint"`)
/// accepted no H.264 video, and what became of it (`NoH264`). Output-scoped,
/// with the flow in `details`, like `codec_needs_encode`.
#[cfg(feature = "webrtc")]
fn warn_no_h264(output_id: &str, flow_id: &str, events: &EventSender, peer: &str, outcome: NoH264) {
    let (tail, code, retry_secs, reason) = match outcome {
        NoH264::AudioOnly => (
            "; it is sent audio only".to_string(),
            "audio_only",
            None,
            None,
        ),
        NoH264::Refused(refusal) => {
            let why = match refusal {
                WhepRefusal::NothingToSend | WhepRefusal::BundleTagRejected => {
                    ", and no audio this output sends"
                }
                WhepRefusal::NoOpusToSend => {
                    ", and this output has no Opus to send it (no audio_encode, and the source's audio is not Opus)"
                }
                WhepRefusal::SourceAudioUnknown => {
                    ", and this output has not yet read the source's PMT to know whether its audio is Opus (no audio_encode)"
                }
                WhepRefusal::VideoIsBundleTag => {
                    ", and its video m-line leads the offer's BUNDLE group, which an answer may not reject"
                }
            };
            (
                format!("{why}; refused"),
                "refused",
                None,
                Some(refusal.code()),
            )
        }
        NoH264::Retrying(secs) => (
            format!("; retrying in {secs} s"),
            "retrying",
            Some(secs),
            None,
        ),
    };
    let msg = format!("WebRTC output '{output_id}': the {peer} accepted no H.264 video{tail}");
    tracing::warn!("{msg}");
    events.emit_output_with_details(
        EventSeverity::Warning,
        category::WEBRTC,
        msg,
        output_id,
        serde_json::json!({
            "error_code": "webrtc_no_h264",
            "peer": peer,
            "outcome": code,
            "reason": reason,
            "retry_secs": retry_secs,
            "flow_id": flow_id,
        }),
    );
}

/// What a WHEP viewer's answer leaves this output to send, decided before the
/// answer goes back: `Ok(None)` for video (and audio, as negotiated);
/// `Ok(Some(AudioOnly))` for a viewer that accepted no H.264 but Opus, when
/// the output is not `video_only` and has Opus to send (`sends_opus`: an
/// `audio_encode`, or an Opus source passed through — `None` while the
/// source's PMT has not been read, so that is not known yet); else the
/// refusal. `bundle_tag_rejected`: the answer rejects the offer's BUNDLE tag
/// (`session::rejected_bundle_tag`), which no browser applies.
///
/// A viewer without H.264 used to get a 201 and then nothing at all: its
/// task ended at connect for want of a video PT, and the audio it had
/// negotiated went with it.
#[cfg(feature = "webrtc")]
fn whep_viewer_outcome(
    video_pt: Option<str0m::media::Pt>,
    audio_pt: Option<str0m::media::Pt>,
    video_only: bool,
    sends_opus: Option<bool>,
    bundle_tag_rejected: bool,
) -> Result<Option<NoH264>, WhepRefusal> {
    match (video_pt, audio_pt.filter(|_| !video_only)) {
        (Some(_), _) if bundle_tag_rejected => Err(WhepRefusal::BundleTagRejected),
        (Some(_), _) => Ok(None),
        (None, None) => Err(WhepRefusal::NothingToSend),
        (None, Some(_)) if sends_opus == Some(false) => Err(WhepRefusal::NoOpusToSend),
        // Refused whatever the source turns out to be: no retry helps.
        (None, Some(_)) if bundle_tag_rejected => Err(WhepRefusal::VideoIsBundleTag),
        (None, Some(_)) if sends_opus.is_none() => Err(WhepRefusal::SourceAudioUnknown),
        (None, Some(_)) => Ok(Some(NoH264::AudioOnly)),
    }
}

/// An Opus-in-TS source on a session with an `audio_encode` goes through it
/// like any other source: as an `OtherAudio` frame on `stream_type` 0x06,
/// which the Opus decoder takes (`audio_decode::ff_codec_for_stream_type`),
/// then re-encoded at the block's settings. Without one (`Disabled`) the
/// frame is left as it is for the passthrough, and so is a multistream
/// layout the decoder cannot take (the Opus arm drops it, said once).
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
fn opus_through_audio_encode(
    frame: super::ts_demux::DemuxedFrame,
    encoder_state: &WebrtcEncoderState,
    opus_channel_config: Option<u8>,
) -> super::ts_demux::DemuxedFrame {
    use super::ts_demux::DemuxedFrame;
    match frame {
        DemuxedFrame::Opus { data, pts, .. }
            if !matches!(encoder_state, WebrtcEncoderState::Disabled)
                && super::webrtc::opus_passthrough::carries(opus_channel_config) =>
        {
            DemuxedFrame::OtherAudio { stream_type: 0x06, data, pts }
        }
        other => other,
    }
}

/// Count one frame handed to `write_media` as sent — `packets` RTP packets
/// of `bytes`, and its latency from `recv_time_us` when it came from the
/// source — unless ICE was down as it was handed over (`on_wire` false):
/// `WebrtcSession::write_media` then put nothing on the wire. The frame was
/// still encoded, so the encoders keep their timelines (see there).
#[cfg(feature = "webrtc")]
fn count_sent(
    stats: &OutputStatsAccumulator,
    on_wire: bool,
    packets: u64,
    bytes: usize,
    recv_time_us: Option<u64>,
) {
    if !on_wire {
        return;
    }
    stats.packets_sent.fetch_add(packets, Ordering::Relaxed);
    stats.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
    if let Some(recv_time_us) = recv_time_us {
        stats.record_latency(recv_time_us);
    }
}

/// Pass one Opus-in-TS PES through to the session's audio track: each Opus
/// packet it carries written as one frame on the 48 kHz RTP clock, as the
/// re-encode writes its encoder's (`OpusTimeline` places them). Shared by the
/// WHIP client loop and the WHEP per-viewer loop.
#[cfg(feature = "webrtc")]
#[allow(clippy::too_many_arguments)]
async fn write_opus_passthrough(
    timeline: &mut super::webrtc::opus_passthrough::OpusTimeline,
    pes: &[u8],
    pts: Option<u64>,
    recv_time_us: u64,
    session: &mut super::webrtc::session::WebrtcSession,
    audio_mid: str0m::media::Mid,
    audio_pt: str0m::media::Pt,
    stats: &Arc<OutputStatsAccumulator>,
    output_id: &str,
) {
    use str0m::media::{Frequency, MediaTime};
    use std::time::Instant;

    for (packet, rtp_time) in timeline.place(pes, pts) {
        let media_time = MediaTime::new(rtp_time, Frequency::FORTY_EIGHT_KHZ);
        let on_wire = !session.ice_down();
        if let Err(e) = session.write_media(audio_mid, audio_pt, Instant::now(), media_time, packet) {
            tracing::debug!("WebRTC output '{}' Opus passthrough write error: {}", output_id, e);
        }
        // str0m requires poll_output between consecutive writes.
        session.drain_outputs().await;
        count_sent(stats, on_wire, 1, packet.len(), Some(recv_time_us));
    }
}

/// One demuxed video access unit for a WebRTC output.
#[cfg(feature = "webrtc")]
#[derive(Clone, Copy)]
enum WebrtcVideoSource<'a> {
    H264(&'a [Vec<u8>]),
    H265(&'a [Vec<u8>]),
    /// An MPEG-2 access unit (its ES as the PES carried it). WebRTC carries
    /// H.264, so it goes out only re-encoded; it used to be dropped whatever
    /// `video_encode` said.
    #[cfg_attr(not(feature = "media-codecs"), allow(dead_code))]
    Mpeg2(&'a [u8]),
}

#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
impl<'a> WebrtcVideoSource<'a> {
    fn codec(&self) -> video_codec::VideoCodec {
        match self {
            WebrtcVideoSource::H264(_) => video_codec::VideoCodec::H264,
            WebrtcVideoSource::H265(_) => video_codec::VideoCodec::Hevc,
            WebrtcVideoSource::Mpeg2(_) => video_codec::VideoCodec::Mpeg2,
        }
    }

    /// The access unit as a decoder takes it: Annex B, or MPEG-2's ES.
    fn decoder_input(&self) -> std::borrow::Cow<'a, [u8]> {
        match self {
            WebrtcVideoSource::H264(n) | WebrtcVideoSource::H265(n) => {
                std::borrow::Cow::Owned(nalus_to_annex_b_webrtc(n))
            }
            WebrtcVideoSource::Mpeg2(es) => std::borrow::Cow::Borrowed(es),
        }
    }
}

/// Handle one demuxed video access unit: passthrough H.264, encode HEVC
/// (or H.264 if `video_encode` is set), then RFC 6184 packetize and hand
/// to str0m. Shared by the WHIP client loop and the WHEP per-viewer loop.
///
/// `source_is_h264` distinguishes the source codec; HEVC sources without
/// `video_encode` are dropped here (pre-Phase 4d behaviour). With
/// `video_encode`, both source codecs are decoded and re-encoded as
/// H.264 so every WebRTC browser can decode the output.
#[cfg(feature = "webrtc")]
#[allow(clippy::too_many_arguments)]
async fn handle_webrtc_video_frame(
    source: WebrtcVideoSource<'_>,
    pts: u64,
    #[cfg_attr(not(feature = "media-codecs"), allow(unused_variables))]
    pts_known: bool,
    recv_time_us: u64,
    video_state: &mut WebrtcVideoEncoderState,
    session: &mut super::webrtc::session::WebrtcSession,
    video_mid: str0m::media::Mid,
    video_pt: str0m::media::Pt,
    stats: &Arc<OutputStatsAccumulator>,
    output_id: &str,
    #[cfg_attr(not(feature = "media-codecs"), allow(unused_variables))]
    flow_id: &str,
    #[cfg_attr(not(feature = "media-codecs"), allow(unused_variables))]
    events: &EventSender,
) {
    use str0m::media::{Frequency, MediaTime};
    use std::time::Instant;

    // Disabled + HEVC / MPEG-2 source → drop (the loops report it once per
    // session, `codec_needs_encode`).
    let passthrough_nalus = match source {
        WebrtcVideoSource::H264(nalus) => Some(nalus),
        _ => None,
    };
    if matches!(video_state, WebrtcVideoEncoderState::Disabled) && passthrough_nalus.is_none() {
        return;
    }
    // Failed → drop video for the rest of the session.
    if matches!(video_state, WebrtcVideoEncoderState::Failed) {
        return;
    }

    // Lazy-open the decoder + encoder scaffolding on the first video
    // access unit that can open them (an H.264 one waits for the SPS,
    // which seeds the decoder's reorder depth). Falls through to the
    // encode path on the same frame.
    #[cfg(feature = "media-codecs")]
    let au = source.decoder_input();
    #[cfg(feature = "media-codecs")]
    if let WebrtcVideoEncoderState::Lazy { cfg, sps_gate } = video_state {
        let codec = source.codec();
        if !sps_gate.admits(codec, &au) {
            return;
        }
        let cfg = cfg.clone();
        *video_state = open_webrtc_video_active(
            &cfg,
            codec,
            &au,
            output_id,
            flow_id,
            stats,
            events,
        );
        if matches!(video_state, WebrtcVideoEncoderState::Failed) {
            return;
        }
    }

    // The frames to packetize, each with its RTP timestamp — the source
    // NALUs (passthrough), or every frame the encoder handed back, split
    // back into NAL units, each on its own picture's source PTS. They used
    // to be run together as one frame stamped with the access unit being
    // fed: a pipeline's depth late, out of order on a source with B-frames
    // (the receiver saw -80 ms steps on Sky Sports), and one marker bit
    // over several pictures when the encoder handed back more than one.
    #[cfg(feature = "media-codecs")]
    let frames: Vec<(std::borrow::Cow<'_, [Vec<u8>]>, u64)> =
        if matches!(video_state, WebrtcVideoEncoderState::Active(_)) {
            let encoded = encode_one_video_frame_webrtc(video_state, &au, pts, pts_known, output_id);
            if matches!(video_state, WebrtcVideoEncoderState::Failed) {
                return;
            }
            encoded
                .into_iter()
                .map(|(annex_b, frame_pts)| {
                    (std::borrow::Cow::Owned(super::ts_demux::split_annex_b_nalus(&annex_b)), frame_pts)
                })
                .collect()
        } else if let Some(nalus) = passthrough_nalus {
            vec![(std::borrow::Cow::Borrowed(nalus), pts)]
        } else {
            Vec::new()
        };
    #[cfg(not(feature = "media-codecs"))]
    let frames: Vec<(std::borrow::Cow<'_, [Vec<u8>]>, u64)> =
        passthrough_nalus.map(|n| (std::borrow::Cow::Borrowed(n), pts)).into_iter().collect();

    for (send_nalus, frame_pts) in &frames {
        // One access unit per write, Annex B: str0m's writer packetizes a
        // frame itself (RFC 6184 — STAP-A for the SPS / PPS, FU-A past its
        // MTU, the marker bit on the frame's last packet). Each NAL used to
        // be packetized here first and every RTP payload written as a frame
        // of its own: str0m took an FU-A fragment for a NAL and fragmented
        // it again, so a receiver reassembled type-28 "NAL units" out of
        // every IDR and large P slice — no picture decoded from them — and
        // each fragment arrived as its own frame, every one marker-bit.
        let au = annex_b_access_unit(send_nalus);
        let media_time = MediaTime::new(*frame_pts, Frequency::NINETY_KHZ);
        let on_wire = !session.ice_down();
        if let Err(e) = session.write_media(video_mid, video_pt, Instant::now(), media_time, &au) {
            tracing::debug!("WebRTC output '{}' write error: {}", output_id, e);
        }
        // str0m requires poll_output between consecutive writes —
        // drain or the next write_media is silently rejected.
        session.drain_outputs().await;
        // RTP packets, approximately: the 1200-byte payloads a frame splits into.
        count_sent(stats, on_wire, au.len().div_ceil(1_200) as u64, au.len(), Some(recv_time_us));
    }
}

/// An access unit's NAL units (start codes stripped) as one Annex B buffer.
#[cfg(feature = "webrtc")]
fn annex_b_access_unit(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut au = Vec::with_capacity(nalus.iter().map(|n| n.len() + 4).sum());
    for nalu in nalus {
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(nalu);
    }
    au
}

/// Spawn a WebRTC output task (WHIP client or WHEP server depending on config mode).
///
/// For WHEP server mode, `session_rx` must be provided — it receives SDP offers
/// from the HTTP handler (via `WebrtcSessionRegistry`) and spawns per-viewer
/// send tasks. The corresponding sender is stored in `FlowRuntime::whep_session_tx`
/// and registered with the session registry after flow creation.
pub fn spawn_webrtc_output(
    config: WebrtcOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    #[cfg(feature = "webrtc")]
    session_rx: Option<tokio::sync::mpsc::Receiver<crate::api::webrtc::registry::NewSessionMsg>>,
    event_sender: EventSender,
    flow_id: String,
    compressed_audio_input: bool,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();

    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("webrtc".to_string()),
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

    #[cfg(feature = "webrtc")]
    {
        use crate::config::models::WebrtcOutputMode;
        match config.mode {
            WebrtcOutputMode::WhipClient => {
                tokio::spawn(async move {
                    whip_client_loop(config, rx, output_stats, cancel, &event_sender, &flow_id, compressed_audio_input).await;
                })
            }
            WebrtcOutputMode::WhepServer => {
                let broadcast_tx_clone = broadcast_tx.clone();
                tokio::spawn(async move {
                    tracing::info!(
                        "WebRTC/WHEP server output '{}' started, waiting for viewers at /api/v1/flows/.../whep",
                        config.id,
                    );
                    if let Some(session_rx) = session_rx {
                        whep_server_loop(
                            config,
                            broadcast_tx_clone,
                            rx,
                            output_stats,
                            cancel,
                            session_rx,
                            &event_sender,
                            &flow_id,
                            compressed_audio_input,
                            WHEP_SETUP_DEADLINE,
                        )
                        .await;
                    } else {
                        tracing::warn!(
                            "WHEP server output '{}' has no session channel — viewers cannot connect",
                            config.id,
                        );
                        webrtc_stub_loop(&config, rx, output_stats, cancel).await;
                    }
                })
            }
        }
    }

    #[cfg(not(feature = "webrtc"))]
    {
        let _ = (event_sender, flow_id, compressed_audio_input); // Suppress unused warnings
        tokio::spawn(async move {
            tracing::warn!(
                "WebRTC output '{}' is a stub: the `webrtc` cargo feature is not enabled. \
                 Packets will be consumed but not transmitted.",
                config.id,
            );
            webrtc_stub_loop(&config, rx, output_stats, cancel).await;
        })
    }
}

/// Stub receive loop — consumes packets without transmitting.
/// Used when webrtc feature is disabled, or for WHEP server mode
/// (where actual sending happens in per-viewer session tasks).
async fn webrtc_stub_loop(
    config: &WebrtcOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("WebRTC output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(_packet) => {
                        // Consume silently
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("WebRTC output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
        }
    }
}

/// How long an answered WHEP viewer has to complete ICE + DTLS before its
/// session is closed — the relay's `SETUP_DEADLINE`. Ample for any viewer
/// that is going to connect: the output is ICE-Lite, with nothing to gather,
/// and a browser connects in well under a second. Without it a viewer that
/// never connected — an offer POSTed and abandoned, or answered with
/// something its browser could not apply — held its task and UDP socket
/// until the flow stopped.
#[cfg(feature = "webrtc")]
const WHEP_SETUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// How often a WebRTC send loop drives its session when nothing has come to
/// send: answering the peer's STUN consent checks and running str0m's
/// timeouts — ICE giving up on a departed peer among them. A browser checks
/// consent every few seconds, so a second's delay costs nothing. Without it a
/// stalled source left a departed viewer's session undriven, and never let
/// go.
#[cfg(feature = "webrtc")]
const IDLE_DRIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// WHEP server loop — listens for viewer session requests from the HTTP handler
/// and spawns per-viewer send tasks that subscribe to the broadcast channel.
#[cfg(feature = "webrtc")]
#[allow(clippy::too_many_arguments)]
async fn whep_server_loop(
    config: WebrtcOutputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    mut session_rx: tokio::sync::mpsc::Receiver<crate::api::webrtc::registry::NewSessionMsg>,
    events: &EventSender,
    flow_id: &str,
    compressed_audio_input: bool,
    setup_deadline: std::time::Duration,
) {
    use super::webrtc::session::{OfferRefused, SessionConfig, WebrtcSession, rejected_bundle_tag, report_negotiation_panic};

    let public_ip: Option<std::net::IpAddr> = config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    let bind_addr: std::net::SocketAddr = match public_ip {
        Some(ip) => std::net::SocketAddr::new(ip, 0),
        None => "0.0.0.0:0".parse().unwrap(),
    };
    // WHEP output is the server side — ICE-Lite.
    let session_config = SessionConfig { bind_addr, public_ip, ice_lite: true };
    // The source's PSI, read off the packets this loop already consumes: a
    // viewer that accepts no H.264 is sent audio alone only if there is Opus
    // to send it, and without an `audio_encode` that is an Opus source's own.
    let mut source = super::webrtc::ts_demux::TsDemuxer::new(config.program_number);

    loop {
        // Wait for a viewer to connect via WHEP
        let msg = tokio::select! {
            _ = cancel.cancelled() => break,
            msg = session_rx.recv() => match msg {
                Some(m) => m,
                None => break, // Channel closed
            },
            // Keep consuming broadcast packets while waiting so we don't lag
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        source.observe_psi(super::ts_parse::strip_rtp_header(&packet));
                        continue;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };

        tracing::info!("WHEP viewer connecting to output '{}'", config.id);

        // Create WebRTC session for this viewer
        let mut session = match WebrtcSession::new(&session_config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("WHEP output '{}': failed to create session: {}", config.id, e);
                events.emit_flow(EventSeverity::Warning, category::WEBRTC, format!("WebRTC session creation failed: {e}"), flow_id);
                let _ = msg.reply.send(Err(e));
                continue;
            }
        };

        // Accept the viewer's SDP offer (recvonly from viewer's perspective)
        let answer = match session.accept_offer(&msg.offer_sdp) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("WHEP output '{}': failed to accept SDP offer: {}", config.id, e);
                report_negotiation_panic(&e, events, flow_id, "WHEP viewer");
                let _ = msg.reply.send(Err(e));
                continue;
            }
        };

        // What the answer leaves to send, settled before it goes back: a
        // viewer that accepted no H.264 is sent its audio, or refused — as
        // is one whose answer rejects its BUNDLE tag, which it could not
        // apply.
        let (video_pt, audio_pt) = session.answered_pts(&answer);
        // Not known until the source's PMT has been read: a viewer that
        // comes first — at output start, after a flow restart, while the
        // source is not flowing — was refused as if the source were not Opus.
        let sends_opus = if config.audio_encode.is_some() {
            Some(true)
        } else if !source.pmt_seen() {
            None
        } else {
            Some(
                source.audio_is_opus()
                    && super::webrtc::opus_passthrough::carries(source.opus_channel_config()),
            )
        };
        let bundle_tag = rejected_bundle_tag(&msg.offer_sdp, &answer);
        match whep_viewer_outcome(
            video_pt,
            audio_pt,
            config.video_only,
            sends_opus,
            bundle_tag.is_some(),
        ) {
            Ok(None) => {}
            Ok(Some(outcome)) => warn_no_h264(&config.id, flow_id, events, "WHEP viewer", outcome),
            Err(refusal) => {
                if refusal == WhepRefusal::BundleTagRejected {
                    tracing::warn!(
                        "WHEP output '{}': refused a viewer whose BUNDLE tag (mid {}) the answer rejects",
                        config.id,
                        bundle_tag.as_deref().unwrap_or("?"),
                    );
                } else {
                    warn_no_h264(
                        &config.id,
                        flow_id,
                        events,
                        "WHEP viewer",
                        NoH264::Refused(refusal),
                    );
                }
                let _ = msg
                    .reply
                    .send(Err(OfferRefused(refusal.reason().into()).into()));
                continue;
            }
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        // Per-viewer cancel token, rooted at the output task's parent. The
        // API layer holds a clone so DELETE /whep/<session_id> tears down
        // exactly this viewer without affecting other concurrent viewers.
        let viewer_cancel = cancel.child_token();
        let _ = msg.reply.send(Ok((answer, session_id.clone(), viewer_cancel.clone())));

        // Spawn a per-viewer send task
        let viewer_rx = broadcast_tx.subscribe();
        let viewer_stats = stats.clone();
        let output_id = config.id.clone();
        let video_only = config.video_only;
        let viewer_program = config.program_number;
        let viewer_events = events.clone();
        let viewer_flow_id = flow_id.to_string();
        let viewer_audio_encode = config.audio_encode.clone();
        let viewer_transcode = config.transcode.clone();
        let viewer_compressed = compressed_audio_input;
        let viewer_video_encode = config.video_encode.clone();

        tokio::spawn(async move {
            whep_viewer_loop(
                &output_id,
                &session_id,
                session,
                viewer_rx,
                WhepViewerCfg {
                    stats: viewer_stats,
                    cancel: viewer_cancel,
                    video_only,
                    program_number: viewer_program,
                    events: &viewer_events,
                    flow_id: &viewer_flow_id,
                    audio_encode: viewer_audio_encode,
                    transcode: viewer_transcode,
                    compressed_audio_input: viewer_compressed,
                    video_encode: viewer_video_encode,
                    setup_deadline,
                },
            )
            .await;
        });
    }

    tracing::info!("WHEP server output '{}' stopped", config.id);
}

/// Per-viewer send loop: demux TS → packetize H.264 → send via WebRTC to one viewer.
#[cfg(feature = "webrtc")]
/// Fixed per-viewer settings for a WHEP session: identity, the program it
/// carries, and the audio/video transcode selection.
struct WhepViewerCfg<'a> {
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    video_only: bool,
    program_number: Option<u16>,
    events: &'a EventSender,
    flow_id: &'a str,
    audio_encode: Option<crate::config::models::AudioEncodeConfig>,
    transcode: Option<super::audio_transcode::TranscodeJson>,
    compressed_audio_input: bool,
    video_encode: Option<VideoEncodeConfig>,
    /// How long the viewer has to connect (`WHEP_SETUP_DEADLINE`).
    setup_deadline: std::time::Duration,
}

async fn whep_viewer_loop(
    output_id: &str,
    session_id: &str,
    mut session: super::webrtc::session::WebrtcSession,
    mut rx: broadcast::Receiver<RtpPacket>,
    params: WhepViewerCfg<'_>,
) {
    // Destructured back into the original bindings so the body is unchanged.
    let WhepViewerCfg {
        stats,
        cancel,
        video_only,
        program_number,
        events,
        flow_id,
        audio_encode,
        transcode,
        compressed_audio_input,
        video_encode,
        setup_deadline,
    } = params;

    use super::ts_parse::strip_rtp_header;
    use super::webrtc::ts_demux::TsDemuxer;
    use super::webrtc::session::SessionEvent;
    use str0m::media::MediaTime;
    use std::time::Instant;

    // However the viewer ends — a DELETE, the setup deadline, ICE giving up,
    // a DTLS close — its token is cancelled on the way out, which is what
    // takes its entry out of the session registry (`handle_whep_offer`).
    // Only a DELETE removed it before: every viewer that left without one
    // stayed listed until the flow stopped.
    let _cancel_on_exit = cancel.clone().drop_guard();

    // Wait for ICE+DTLS to complete — for `setup_deadline` at most.
    let answered_at = Instant::now();
    let setup = tokio::time::timeout(setup_deadline, async {
        loop {
            match session.poll_event(&cancel).await {
                SessionEvent::Connected => return true,
                SessionEvent::Disconnected => return false,
                _ => continue,
            }
        }
    })
    .await;
    match setup {
        Ok(true) => {
            tracing::info!(
                "WHEP viewer '{}' connected on output '{}'",
                session_id,
                output_id
            );
            events.emit_flow(
                EventSeverity::Info,
                category::WEBRTC,
                "WHEP viewer connected",
                flow_id,
            );
        }
        // A DELETE (or the output stopping) during setup.
        Ok(false) if cancel.is_cancelled() => {
            tracing::info!("WHEP viewer '{}' disconnected during setup", session_id);
            events.emit_flow(
                EventSeverity::Info,
                category::WEBRTC,
                "WHEP viewer disconnected",
                flow_id,
            );
            return;
        }
        // The session ended before it connected: a setup that failed as
        // surely as one the deadline closes. An offer carrying a usable
        // candidate (an IP host, srflx or relay one — any player with a STUN
        // or TURN server) gets its pairs made at once, and ICE gives up on
        // them 15 s after nothing came — before the deadline. That ended
        // the setup with an Info alone, so a viewer that was answered and
        // could not connect was never reported.
        Ok(false) => {
            let (secs, after) = (setup_deadline.as_secs(), answered_at.elapsed().as_secs());
            let (reason, why) = if session.ice_down() {
                ("ice_failed", "ICE failed")
            } else {
                ("session_failed", "its session failed")
            };
            tracing::warn!(
                "WHEP viewer '{}' on output '{}' did not connect: {} {} s after its answer; closing it",
                session_id,
                output_id,
                why,
                after,
            );
            events.emit_flow_with_details(
                EventSeverity::Warning,
                category::WEBRTC,
                format!("WHEP viewer did not connect: {why} {after} s after its answer; its session is closed"),
                flow_id,
                serde_json::json!({
                    "error_code": "webrtc_setup_timeout",
                    "peer": "WHEP viewer",
                    "reason": reason,
                    "timeout_secs": secs,
                    "output_id": output_id,
                }),
            );
            return;
        }
        Err(_) => {
            let secs = setup_deadline.as_secs();
            tracing::warn!(
                "WHEP viewer '{}' on output '{}' did not connect within {} s of its answer; closing it",
                session_id,
                output_id,
                secs,
            );
            events.emit_flow_with_details(
                EventSeverity::Warning,
                category::WEBRTC,
                format!("WHEP viewer did not connect within {secs} s of its answer; its session is closed"),
                flow_id,
                serde_json::json!({
                    "error_code": "webrtc_setup_timeout",
                    "peer": "WHEP viewer",
                    "reason": "deadline",
                    "timeout_secs": secs,
                    "output_id": output_id,
                }),
            );
            return;
        }
    }

    // str0m may emit MediaAdded *after* Connected. Flush any pending
    // events so video_mid / audio_mid are populated before we read them.
    session.drain_pending_events();

    // The video MID and PT: none for a viewer that accepted no H.264, which
    // was admitted for its audio alone (`whep_viewer_outcome`) — its video
    // frames go nowhere.
    let video = session.video_mid.and_then(|mid| Some((mid, session.get_pt(mid)?)));

    // Extract the audio MID + payload type if SDP negotiated audio.
    // video_only=true skips this entirely (no audio MID was negotiated).
    let (audio_mid, audio_pt) = if !video_only {
        let mid = session.audio_mid;
        let pt = mid.and_then(|m| session.get_pt(m));
        (mid, pt)
    } else {
        (None, None)
    };
    if video.is_none() && audio_pt.is_none() {
        tracing::error!("WHEP viewer '{}': neither H.264 video nor Opus audio negotiated", session_id);
        return;
    }

    // Build encoder state lazily on first AAC frame. The Lazy state only
    // makes sense when audio_encode is set AND audio MID was negotiated.
    let mut encoder_state: WebrtcEncoderState = match audio_encode.as_ref() {
        Some(cfg) if cfg.silent_fallback && audio_mid.is_some() && audio_pt.is_some() => {
            build_webrtc_encoder_state_eager_for_silent_fallback(
                audio_encode.as_ref(),
                transcode.as_ref(),
                &cancel,
                &stats,
                flow_id,
                output_id,
                events,
            )
        }
        Some(_) if audio_mid.is_some() && audio_pt.is_some() => WebrtcEncoderState::Lazy,
        _ => WebrtcEncoderState::Disabled,
    };
    let mut silence_interval: Option<tokio::time::Interval> =
        if let WebrtcEncoderState::Active { silence: Some(sg), .. } = &encoder_state {
            let mut iv = tokio::time::interval(sg.chunk_duration());
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            Some(iv)
        } else {
            None
        };
    // Lazy FFmpeg-backed decoder for non-AAC sources (MP2 / AC-3 /
    // E-AC-3). Mirrors `decoder` inside `WebrtcEncoderState::Active`
    // for the AAC path.
    #[cfg(feature = "media-codecs")]
    let mut ff_audio_decoder: Option<video_engine::AudioDecoder> = None;
    #[cfg(feature = "media-codecs")]
    let mut ff_audio_codec: Option<video_codec::AudioDecoderCodec> = None;
    let mut video_encoder_state: WebrtcVideoEncoderState =
        init_webrtc_video_encoder_state(video_encode.as_ref());
    let mut needs_encode = NeedsEncodeWarned::default();

    // Send loop: demux TS → packetize H.264 → send via str0m.
    // Also processes incoming RTCP/STUN via drive_udp_io() to keep
    // the session alive (same pattern as whip_client_loop).
    let mut demuxer = TsDemuxer::new(program_number);
    // The 48 kHz RTP timeline of an Opus source passed through.
    let mut opus_timeline = super::webrtc::opus_passthrough::OpusTimeline::default();
    // A viewer that leaves without a DELETE — `pc.close()`, a closed tab, a
    // lost network — surfaces only inside the session: DTLS closed, or ICE
    // giving up, from whichever drain ran str0m's timeouts. So the session is
    // driven when no packet comes too (a stalled source), and checked on
    // every pass. The event used to be dropped by the drains, and nothing
    // drove an idle session: a departed viewer was sent the stream until the
    // flow stopped.
    let mut idle = tokio::time::interval(IDLE_DRIVE_INTERVAL);
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if session.is_disconnected() {
            tracing::info!("WHEP viewer '{}' disconnected during send", session_id);
            break;
        }
        let silence_tick = async {
            match silence_interval.as_mut() {
                Some(iv) => { iv.tick().await; }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => break,

            _ = idle.tick() => {
                session.drive_udp_io().await;
                if session.take_keyframe_request() {
                    force_video_keyframe(&mut video_encoder_state);
                }
            }

            _ = silence_tick => {
                emit_webrtc_silence_if_needed(
                    &mut encoder_state,
                    &mut session,
                    audio_mid,
                    audio_pt,
                    &stats,
                    output_id,
                ).await;
                continue;
            }

            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        let recv_time_us = packet.recv_time_us;
                        let payload = strip_rtp_header(&packet);
                        if payload.is_empty() { continue; }

                        let frames = demuxer.demux(payload);
                        for frame in frames {
                            // While ICE is down (the viewer has gone quiet;
                            // the grace is running) every frame is still
                            // decoded and encoded, and `write_media` drops
                            // it: str0m would send it to the last nominated
                            // address. The encoders' timelines stay whole, so
                            // a viewer that comes back is sent audio and video
                            // on one clock. Frames used to be skipped here,
                            // and an Opus encoder resumed where it had
                            // stopped: audio late by the whole outage.
                            // Opus with an `audio_encode` is re-encoded
                            // through it like any other source.
                            #[cfg(feature = "media-codecs")]
                            let frame = opus_through_audio_encode(frame, &encoder_state, demuxer.opus_channel_config());
                            // Source audio this viewer would hear, dropped
                            // for want of `audio_encode`: say so once.
                            #[cfg(feature = "media-codecs")]
                            if !needs_encode.audio
                                && let Some(what) = audio_needs_encode_label(
                                    &frame,
                                    &demuxer,
                                    audio_encode.is_some(),
                                    audio_mid.is_some() && audio_pt.is_some(),
                                )
                            {
                                needs_encode.audio = true;
                                warn_codec_needs_encode(output_id, flow_id, events, "audio", &what, "audio_encode");
                            }
                            match frame {
                                super::webrtc::ts_demux::DemuxedFrame::H264 { nalus, pts, pts_known, .. } => {
                                    let Some((video_mid, video_pt)) = video else { continue };
                                    handle_webrtc_video_frame(
                                        WebrtcVideoSource::H264(&nalus),
                                        pts,
                                        pts_known,
                                        recv_time_us,
                                        &mut video_encoder_state,
                                        &mut session,
                                        video_mid,
                                        video_pt,
                                        &stats,
                                        output_id,
                                        flow_id,
                                        events,
                                    ).await;
                                }
                                super::webrtc::ts_demux::DemuxedFrame::H265 { nalus, pts, pts_known, .. } => {
                                    let Some((video_mid, video_pt)) = video else { continue };
                                    // WebRTC carries H.264: HEVC goes out
                                    // re-encoded or not at all — said once.
                                    if matches!(video_encoder_state, WebrtcVideoEncoderState::Disabled) {
                                        if !needs_encode.video {
                                            needs_encode.video = true;
                                            warn_codec_needs_encode(output_id, flow_id, events, "video", "HEVC video", "video_encode");
                                        }
                                        continue;
                                    }
                                    handle_webrtc_video_frame(
                                        WebrtcVideoSource::H265(&nalus),
                                        pts,
                                        pts_known,
                                        recv_time_us,
                                        &mut video_encoder_state,
                                        &mut session,
                                        video_mid,
                                        video_pt,
                                        &stats,
                                        output_id,
                                        flow_id,
                                        events,
                                    ).await;
                                }
                                super::webrtc::ts_demux::DemuxedFrame::Opus { data, pts, pts_known } => {
                                    // Opus-in-TS (a WHIP input, ffmpeg
                                    // libopus): its packets go out as they
                                    // are. A multistream layout cannot —
                                    // said once — and with an `audio_encode`
                                    // the frame was re-encoded above, or (a
                                    // build without the decoder) is dropped.
                                    let (Some(audio_mid), Some(audio_pt)) = (audio_mid, audio_pt) else {
                                        continue;
                                    };
                                    let layout = demuxer.opus_channel_config();
                                    if !super::webrtc::opus_passthrough::carries(layout) {
                                        if !needs_encode.opus_layout
                                            && let Some(code) = layout
                                        {
                                            needs_encode.opus_layout = true;
                                            warn_opus_layout_not_carried(output_id, flow_id, events, code);
                                        }
                                        continue;
                                    }
                                    if !matches!(encoder_state, WebrtcEncoderState::Disabled) {
                                        continue;
                                    }
                                    write_opus_passthrough(
                                        &mut opus_timeline,
                                        &data,
                                        pts_known.then_some(pts),
                                        recv_time_us,
                                        &mut session,
                                        audio_mid,
                                        audio_pt,
                                        &stats,
                                        output_id,
                                    ).await;
                                }
                                super::webrtc::ts_demux::DemuxedFrame::Aac { data, pts } => {
                                    if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                        encoder_state = build_webrtc_encoder_state(
                                            audio_encode.as_ref(),
                                            transcode.as_ref(),
                                            &demuxer,
                                            Some(&data),
                                            compressed_audio_input,
                                            &cancel,
                                            &stats,
                                            flow_id,
                                            output_id,
                                            events,
                                        );
                                    }
                                    if let (
                                        WebrtcEncoderState::Active {
                                            decoder,
                                            encoder,
                                            decode_stats,
                                            stage,
                                            silence,
                                        },
                                        Some(audio_mid),
                                        Some(audio_pt),
                                    ) = (&mut encoder_state, audio_mid, audio_pt)
                                    {
                                        if decoder.is_none()
                                            && let Some(c) = demuxer.cached_aac_config() {
                                                webrtc_lazy_build_decoder(decoder, c, output_id);
                                            }
                                        if let Some(sg) = silence.as_mut() {
                                            sg.mark_real_audio(pts);
                                        }
                                        let Some(dec) = decoder.as_mut() else {
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
                                                            "WHEP viewer '{}' transcode failed: {}",
                                                            session_id, e
                                                        );
                                                    }
                                                }
                                            }
                                            Err(_) => {
                                                decode_stats.inc_error();
                                            }
                                        }
                                        for frame in encoder.drain() {
                                            // Convert 90k PTS → 48k for Opus
                                            let media_time = MediaTime::new(
                                                frame.pts * 48_000 / 90_000,
                                                str0m::media::Frequency::FORTY_EIGHT_KHZ,
                                            );
                                            let on_wire = !session.ice_down();
                                            if let Err(e) = session.write_media(
                                                audio_mid,
                                                audio_pt,
                                                Instant::now(),
                                                media_time,
                                                &frame.data,
                                            ) {
                                                tracing::debug!(
                                                    "WHEP viewer '{}' audio write error: {}",
                                                    session_id, e
                                                );
                                            }
                                            session.drain_outputs().await;
                                            count_sent(&stats, on_wire, 1, frame.data.len(), Some(recv_time_us));
                                        }
                                    }
                                }
                                #[cfg(feature = "media-codecs")]
                                super::webrtc::ts_demux::DemuxedFrame::OtherAudio {
                                    stream_type, data, pts,
                                } => {
                                    let decoded = decode_other_audio_for_encode(
                                        &mut encoder_state,
                                        &mut ff_audio_decoder,
                                        &mut ff_audio_codec,
                                        stream_type,
                                        &data,
                                        pts,
                                        audio_encode.as_ref(),
                                        transcode.as_ref(),
                                        &cancel,
                                        &stats,
                                        flow_id,
                                        output_id,
                                        events,
                                    );
                                    let (
                                        WebrtcEncoderState::Active {
                                            encoder, silence, stage, ..
                                        },
                                        Some(audio_mid),
                                        Some(audio_pt),
                                    ) = (&mut encoder_state, audio_mid, audio_pt)
                                    else {
                                        continue;
                                    };
                                    if let Some(sg) = silence.as_mut() {
                                        sg.mark_real_audio(pts);
                                    }
                                    for frame in &decoded {
                                        // To the encoder's format (a 5.1
                                        // AC-3 source was refused, MP2 at
                                        // another rate mislabelled).
                                        let _ = encoder.submit_through(stage, &frame.planar, frame.sample_rate, pts);
                                    }
                                    for frame in encoder.drain() {
                                        let media_time = MediaTime::new(
                                            frame.pts * 48_000 / 90_000,
                                            str0m::media::Frequency::FORTY_EIGHT_KHZ,
                                        );
                                        let on_wire = !session.ice_down();
                                        if let Err(e) = session.write_media(
                                            audio_mid,
                                            audio_pt,
                                            Instant::now(),
                                            media_time,
                                            &frame.data,
                                        ) {
                                            tracing::debug!(
                                                "WHEP viewer '{}' audio write error: {}",
                                                session_id, e
                                            );
                                        }
                                        session.drain_outputs().await;
                                        count_sent(&stats, on_wire, 1, frame.data.len(), Some(recv_time_us));
                                    }
                                }
                                #[cfg(not(feature = "media-codecs"))]
                                super::webrtc::ts_demux::DemuxedFrame::OtherAudio { .. } => {}
                                // WebRTC carries H.264: MPEG-2 video goes out
                                // re-encoded (`video_encode`), which the decoder
                                // layer does; without one it is dropped and the
                                // operator told once — it used to be dropped
                                // either way, silently.
                                super::webrtc::ts_demux::DemuxedFrame::Mpeg2 { es, pts, pts_known, .. } => {
                                    let Some((video_mid, video_pt)) = video else { continue };
                                    if matches!(video_encoder_state, WebrtcVideoEncoderState::Disabled) {
                                        if !needs_encode.video {
                                            needs_encode.video = true;
                                            warn_codec_needs_encode(output_id, flow_id, events, "video", "MPEG-2 video", "video_encode");
                                        }
                                        continue;
                                    }
                                    handle_webrtc_video_frame(
                                        WebrtcVideoSource::Mpeg2(&es),
                                        pts,
                                        pts_known,
                                        recv_time_us,
                                        &mut video_encoder_state,
                                        &mut session,
                                        video_mid,
                                        video_pt,
                                        &stats,
                                        output_id,
                                        flow_id,
                                        events,
                                    ).await;
                                }
                                // Stream discontinuity is metadata for stateful
                                // decoders; the WebRTC packetizer re-anchors on
                                // the next IDR independently.
                                super::webrtc::ts_demux::DemuxedFrame::Discontinuity
                                | super::webrtc::ts_demux::DemuxedFrame::Scte35(_) => {}
                            }
                        }

                        // Drive str0m: process incoming RTCP/STUN + send
                        // queued output. Whether that ended the session is
                        // checked at the top of the loop.
                        session.drive_udp_io().await;
                        if session.take_keyframe_request() {
                            force_video_keyframe(&mut video_encoder_state);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    tracing::info!("WHEP viewer '{}' disconnected from output '{}'", session_id, output_id);
    events.emit_flow(EventSeverity::Info, category::WEBRTC, "WHEP viewer disconnected", flow_id);
}

/// Leave a WHIP session the output will not publish on: DELETE its
/// `resource` (when there is one to delete) and wait out `backoff_secs`.
/// `false` when `cancel` fired meanwhile — the output is stopping.
#[cfg(feature = "webrtc")]
async fn whip_leave_and_back_off(
    resource: Option<&str>,
    config: &WebrtcOutputConfig,
    tls: &crate::util::tls::TlsTrust,
    backoff_secs: u64,
    cancel: &CancellationToken,
) -> bool {
    if let Some(resource) = resource {
        tokio::select! {
            _ = cancel.cancelled() => return false,
            r = super::webrtc::signaling::delete_session(resource, config.bearer_token.as_deref(), tls) => {
                if let Err(e) = r {
                    tracing::warn!("WHIP client '{}': DELETE {} failed: {}", config.id, resource, e);
                }
            }
        }
    }
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => true,
    }
}

/// WHIP client output loop — pushes media to an external WHIP endpoint.
#[cfg(feature = "webrtc")]
async fn whip_client_loop(
    config: WebrtcOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    compressed_audio_input: bool,
) {
    use std::time::Instant;
    use super::ts_parse::strip_rtp_header;
    use super::webrtc::session::{
        BACKOFF_RESET_AFTER, SessionConfig, SessionEvent, WebrtcSession, report_negotiation_panic,
    };
    use super::webrtc::ts_demux::TsDemuxer;
    use str0m::media::MediaTime;

    let whip_url = match &config.whip_url {
        Some(url) => url.clone(),
        None => {
            tracing::error!("WebRTC output '{}': no whip_url configured", config.id);
            return;
        }
    };

    let public_ip: Option<std::net::IpAddr> = config.public_ip.as_ref().and_then(|ip| ip.parse().ok());
    // When public_ip is pinned we also bind the UDP socket to that address,
    // so the destination IP on every incoming packet matches the local ICE
    // candidate. Without this, str0m's ICE state machine discards STUN
    // binding requests as "unknown interface" and the connection silently
    // fails to complete. Same fix as input_webrtc.
    let bind_addr: std::net::SocketAddr = match public_ip {
        Some(ip) => std::net::SocketAddr::new(ip, 0),
        None => "0.0.0.0:0".parse().unwrap(),
    };
    // WHIP output is the client side — full ICE (not ICE-Lite).
    let session_config = SessionConfig { bind_addr, public_ip, ice_lite: false };
    let mut backoff_secs = 1u64;

    'outer: loop {
        // Create session
        let mut session = match WebrtcSession::new(&session_config).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("WHIP client '{}': session error: {}", config.id, e);
                events.emit_flow(EventSeverity::Warning, category::WEBRTC, format!("WebRTC session creation failed: {e}"), flow_id);
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        // Create SDP offer (sendonly video + optional audio)
        let (offer_sdp, pending) = match session.create_offer(true, !config.video_only, true) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("WHIP client '{}': SDP offer error: {}", config.id, e);
                report_negotiation_panic(&e, events, flow_id, "WHIP endpoint");
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        // POST to WHIP endpoint
        let tls = crate::util::tls::TlsTrust {
            accept_self_signed: config.accept_self_signed_cert.unwrap_or(true),
            fingerprint: config.cert_fingerprint.clone(),
        };
        // Cancel-guard the signaling POST: with a bounded connect_timeout it
        // can still take ~10 s against a filtered endpoint, and without this
        // guard a flow-stop would stall on that connect. The tokio::select!
        // evaluates to the whip_post result, or diverges out of the loop on
        // cancellation.
        let post_result = tokio::select! {
            _ = cancel.cancelled() => break 'outer,
            r = super::webrtc::signaling::whip_post(
                &whip_url,
                &offer_sdp,
                config.bearer_token.as_deref(),
                &tls,
            ) => r,
        };
        let (answer_sdp, resource_url) = match post_result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("WHIP signaling '{}' failed: {}", config.id, e);
                // Surface the failure in the manager UI — the error string
                // carries the HTTP status + body from whip_post, or the
                // connect error for a filtered/blackholed endpoint. Without
                // this the output sits silently in the benign "waiting" state.
                events.emit_flow(
                    EventSeverity::Warning,
                    category::WEBRTC,
                    format!("WHIP signaling failed: {e}"),
                    flow_id,
                );
                tokio::select! {
                    _ = cancel.cancelled() => break 'outer,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        if let Err(e) = session.apply_answer(&answer_sdp, pending) {
            tracing::error!("WHIP client '{}': SDP answer error: {}", config.id, e);
            report_negotiation_panic(&e, events, flow_id, "WHIP endpoint");
            // Back off before retrying — a bare `continue` here spins the
            // session/offer/POST loop with no delay on a persistent SDP fault.
            tokio::select! {
                _ = cancel.cancelled() => break 'outer,
                _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
            }
            backoff_secs = (backoff_secs * 2).min(30);
            continue;
        }

        // The answer settles the PTs written on (`get_pt`: H.264, Opus). An
        // endpoint that accepted no H.264 gets no publish — there is no other
        // video to send it: its resource is deleted and the publish retried
        // after the backoff. It used to connect over the audio alone, find no
        // video PT, and start over at once — a new session, POST and ICE/DTLS
        // each time, the resource never deleted, and no Warning.
        let Some((video_mid, video_pt)) =
            session.video_mid.and_then(|mid| Some((mid, session.get_pt(mid)?)))
        else {
            warn_no_h264(&config.id, flow_id, events, "WHIP endpoint", NoH264::Retrying(backoff_secs));
            if !whip_leave_and_back_off(resource_url.as_deref(), &config, &tls, backoff_secs, &cancel).await {
                break 'outer;
            }
            backoff_secs = (backoff_secs * 2).min(30);
            continue;
        };

        // Optional audio MID + PT (only present when video_only=false).
        let (audio_mid, audio_pt) = if !config.video_only {
            let mid = session.audio_mid;
            let pt = mid.and_then(|m| session.get_pt(m));
            (mid, pt)
        } else {
            (None, None)
        };

        tracing::info!("WHIP client '{}' signaling complete, waiting for ICE/DTLS", config.id);

        // Wait for ICE+DTLS to complete
        let child_cancel = cancel.child_token();

        // Wait for connected event before sending media
        loop {
            let event = session.poll_event(&child_cancel).await;
            match event {
                SessionEvent::Connected => {
                    tracing::info!("WHIP client '{}' session established", config.id);
                    // Positive lifecycle signal, symmetric with the WHEP
                    // client input's "WHEP connected" event.
                    events.emit_flow(
                        EventSeverity::Info,
                        category::WEBRTC,
                        "WHIP session established",
                        flow_id,
                    );
                    break;
                }
                SessionEvent::Disconnected => {
                    if cancel.is_cancelled() {
                        break 'outer;
                    }
                    // ICE or DTLS failed. The session is left — its resource
                    // deleted, as for an endpoint without H.264 — and the
                    // publish retried after the backoff. It used to go
                    // straight back to a new session and POST: against an
                    // endpoint whose handshake always fails, without end.
                    tracing::warn!(
                        "WHIP client '{}' disconnected during setup; retrying in {} s",
                        config.id,
                        backoff_secs,
                    );
                    if !whip_leave_and_back_off(
                        resource_url.as_deref(),
                        &config,
                        &tls,
                        backoff_secs,
                        &cancel,
                    )
                    .await
                    {
                        break 'outer;
                    }
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue 'outer;
                }
                _ => continue,
            }
        }
        // Connected. The backoff starts over only if this session lasts
        // (`BACKOFF_RESET_AFTER`, judged when it ends). It was reset here, so
        // an endpoint that accepts each publish and closes it was published
        // to about once a second for good. (Never when signaling succeeds: a
        // handshake that keeps failing backs off as a POST that keeps
        // failing does.)
        let connected_at = Instant::now();

        // str0m may emit MediaAdded *after* Connected. Flush those queued
        // events (the tracks are this end's own offer's, read above).
        session.drain_pending_events();

        let audio_encode = config.audio_encode.clone();
        let transcode = config.transcode.clone();
        let mut encoder_state: WebrtcEncoderState = match audio_encode.as_ref() {
            Some(cfg) if cfg.silent_fallback && audio_mid.is_some() && audio_pt.is_some() => {
                build_webrtc_encoder_state_eager_for_silent_fallback(
                    audio_encode.as_ref(),
                    transcode.as_ref(),
                    &cancel,
                    &stats,
                    flow_id,
                    &config.id,
                    events,
                )
            }
            Some(_) if audio_mid.is_some() && audio_pt.is_some() => WebrtcEncoderState::Lazy,
            _ => WebrtcEncoderState::Disabled,
        };
        // Lazy FFmpeg-backed decoder for non-AAC sources on the WHIP
        // path. Sibling to `decoder` inside `WebrtcEncoderState::Active`.
        #[cfg(feature = "media-codecs")]
        let mut ff_audio_decoder: Option<video_engine::AudioDecoder> = None;
        #[cfg(feature = "media-codecs")]
        let mut ff_audio_codec: Option<video_codec::AudioDecoderCodec> = None;
        let mut silence_interval: Option<tokio::time::Interval> =
            if let WebrtcEncoderState::Active { silence: Some(sg), .. } = &encoder_state {
                let mut iv = tokio::time::interval(sg.chunk_duration());
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                Some(iv)
            } else {
                None
            };
        let mut video_encoder_state: WebrtcVideoEncoderState =
            init_webrtc_video_encoder_state(config.video_encode.as_ref());
        let mut needs_encode = NeedsEncodeWarned::default();

        // Send loop: demux TS → packetize H.264 → send via str0m.
        //
        // We must also process incoming UDP (RTCP receiver reports, STUN
        // keepalives) so str0m can maintain the connection. Without this
        // the remote peer never receives RTCP feedback, timers expire,
        // and the session silently dies.
        let mut demuxer = TsDemuxer::new(config.program_number);
        // The 48 kHz RTP timeline of an Opus source passed through.
        let mut opus_timeline = super::webrtc::opus_passthrough::OpusTimeline::default();
        // As on the WHEP viewer loop: the session is driven when no packet
        // comes too, and checked on every pass — an endpoint that closed the
        // session (DTLS close_notify) or stopped answering ICE used to go
        // unnoticed, and nothing was published again until the flow
        // restarted.
        let mut idle = tokio::time::interval(IDLE_DRIVE_INTERVAL);
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            if session.is_disconnected() {
                if connected_at.elapsed() >= BACKOFF_RESET_AFTER {
                    backoff_secs = 1;
                }
                tracing::warn!(
                    "WHIP client '{}' disconnected during send; publishing again in {} s",
                    config.id,
                    backoff_secs,
                );
                events.emit_flow(
                    EventSeverity::Info,
                    category::WEBRTC,
                    "WHIP client disconnected",
                    flow_id,
                );
                // A floor under the republish, doubling from one session to
                // the next unless one lasted: an endpoint that closes each
                // session as it opens would otherwise be published to in a
                // tight loop.
                if !whip_leave_and_back_off(None, &config, &tls, backoff_secs, &cancel).await {
                    break 'outer;
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue 'outer;
            }
            let silence_tick = async {
                match silence_interval.as_mut() {
                    Some(iv) => { iv.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => break 'outer,

                _ = idle.tick() => {
                    session.drive_udp_io().await;
                    if session.take_keyframe_request() && !force_video_keyframe(&mut video_encoder_state) {
                        tracing::debug!("WHIP client '{}': received PLI/FIR (ignored, passthrough mode)", config.id);
                    }
                }

                _ = silence_tick => {
                    emit_webrtc_silence_if_needed(
                        &mut encoder_state,
                        &mut session,
                        audio_mid,
                        audio_pt,
                        &stats,
                        &config.id,
                    ).await;
                    continue;
                }

                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            let recv_time_us = packet.recv_time_us;
                            let payload = strip_rtp_header(&packet);
                            if payload.is_empty() { continue; }

                            let frames = demuxer.demux(payload);
                            for frame in frames {
                                // Nothing is written while ICE is down, as
                                // on the WHEP viewer loop (`write_media`
                                // drops it). This full-ICE session ends at
                                // that report (at the top of the loop); what
                                // is left of the batch does not go to the
                                // dead address first.
                                // Opus with an `audio_encode` is re-encoded
                                // through it like any other source.
                                #[cfg(feature = "media-codecs")]
                                let frame = opus_through_audio_encode(frame, &encoder_state, demuxer.opus_channel_config());
                                // Source audio the peer would hear, dropped
                                // for want of `audio_encode`: say so once.
                                #[cfg(feature = "media-codecs")]
                                if !needs_encode.audio
                                    && let Some(what) = audio_needs_encode_label(
                                        &frame,
                                        &demuxer,
                                        audio_encode.is_some(),
                                        audio_mid.is_some() && audio_pt.is_some(),
                                    )
                                {
                                    needs_encode.audio = true;
                                    warn_codec_needs_encode(&config.id, flow_id, events, "audio", &what, "audio_encode");
                                }
                                match frame {
                                    super::webrtc::ts_demux::DemuxedFrame::H264 { nalus, pts, pts_known, .. } => {
                                        handle_webrtc_video_frame(
                                            WebrtcVideoSource::H264(&nalus),
                                            pts,
                                            pts_known,
                                            recv_time_us,
                                            &mut video_encoder_state,
                                            &mut session,
                                            video_mid,
                                            video_pt,
                                            &stats,
                                            &config.id,
                                            flow_id,
                                            events,
                                        ).await;
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::H265 { nalus, pts, pts_known, .. } => {
                                        // WebRTC carries H.264: HEVC goes out
                                        // re-encoded or not at all — said once.
                                        if matches!(video_encoder_state, WebrtcVideoEncoderState::Disabled) {
                                            if !needs_encode.video {
                                                needs_encode.video = true;
                                                warn_codec_needs_encode(&config.id, flow_id, events, "video", "HEVC video", "video_encode");
                                            }
                                            continue;
                                        }
                                        handle_webrtc_video_frame(
                                            WebrtcVideoSource::H265(&nalus),
                                            pts,
                                            pts_known,
                                            recv_time_us,
                                            &mut video_encoder_state,
                                            &mut session,
                                            video_mid,
                                            video_pt,
                                            &stats,
                                            &config.id,
                                            flow_id,
                                            events,
                                        ).await;
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Opus { data, pts, pts_known } => {
                                        // Passed through, as on the WHEP
                                        // viewer loop.
                                        let (Some(audio_mid), Some(audio_pt)) = (audio_mid, audio_pt) else {
                                            continue;
                                        };
                                        let layout = demuxer.opus_channel_config();
                                        if !super::webrtc::opus_passthrough::carries(layout) {
                                            if !needs_encode.opus_layout
                                                && let Some(code) = layout
                                            {
                                                needs_encode.opus_layout = true;
                                                warn_opus_layout_not_carried(&config.id, flow_id, events, code);
                                            }
                                            continue;
                                        }
                                        if !matches!(encoder_state, WebrtcEncoderState::Disabled) {
                                            continue;
                                        }
                                        write_opus_passthrough(
                                            &mut opus_timeline,
                                            &data,
                                            pts_known.then_some(pts),
                                            recv_time_us,
                                            &mut session,
                                            audio_mid,
                                            audio_pt,
                                            &stats,
                                            &config.id,
                                        ).await;
                                    }
                                    super::webrtc::ts_demux::DemuxedFrame::Aac { data, pts } => {
                                        if matches!(encoder_state, WebrtcEncoderState::Lazy) {
                                            encoder_state = build_webrtc_encoder_state(
                                                audio_encode.as_ref(),
                                                transcode.as_ref(),
                                                &demuxer,
                                                Some(&data),
                                                compressed_audio_input,
                                                &cancel,
                                                &stats,
                                                flow_id,
                                                &config.id,
                                                events,
                                            );
                                        }
                                        if let (
                                            WebrtcEncoderState::Active {
                                                decoder,
                                                encoder,
                                                decode_stats,
                                                stage,
                                                silence,
                                            },
                                            Some(audio_mid),
                                            Some(audio_pt),
                                        ) = (&mut encoder_state, audio_mid, audio_pt)
                                        {
                                            if decoder.is_none()
                                                && let Some(c) = demuxer.cached_aac_config() {
                                                    webrtc_lazy_build_decoder(decoder, c, &config.id);
                                                }
                                            if let Some(sg) = silence.as_mut() {
                                                sg.mark_real_audio(pts);
                                            }
                                            let Some(dec) = decoder.as_mut() else {
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
                                                                "WHIP '{}' transcode failed: {}",
                                                                config.id, e
                                                            );
                                                        }
                                                    }
                                                }
                                                Err(_) => {
                                                    decode_stats.inc_error();
                                                }
                                            }
                                            for frame in encoder.drain() {
                                                let media_time = MediaTime::new(
                                                    frame.pts * 48_000 / 90_000,
                                                    str0m::media::Frequency::FORTY_EIGHT_KHZ,
                                                );
                                                let on_wire = !session.ice_down();
                                                if let Err(e) = session.write_media(
                                                    audio_mid,
                                                    audio_pt,
                                                    Instant::now(),
                                                    media_time,
                                                    &frame.data,
                                                ) {
                                                    tracing::debug!(
                                                        "WHIP audio write error: {}", e
                                                    );
                                                }
                                                session.drain_outputs().await;
                                                count_sent(&stats, on_wire, 1, frame.data.len(), Some(recv_time_us));
                                            }
                                        }
                                    }
                                    #[cfg(feature = "media-codecs")]
                                    super::webrtc::ts_demux::DemuxedFrame::OtherAudio {
                                        stream_type, data, pts,
                                    } => {
                                        let decoded = decode_other_audio_for_encode(
                                            &mut encoder_state,
                                            &mut ff_audio_decoder,
                                            &mut ff_audio_codec,
                                            stream_type,
                                            &data,
                                            pts,
                                            audio_encode.as_ref(),
                                            transcode.as_ref(),
                                            &cancel,
                                            &stats,
                                            flow_id,
                                            &config.id,
                                            events,
                                        );
                                        let (
                                            WebrtcEncoderState::Active {
                                                encoder, silence, stage, ..
                                            },
                                            Some(audio_mid),
                                            Some(audio_pt),
                                        ) = (&mut encoder_state, audio_mid, audio_pt)
                                        else {
                                            continue;
                                        };
                                        if let Some(sg) = silence.as_mut() {
                                            sg.mark_real_audio(pts);
                                        }
                                        for frame in &decoded {
                                            let _ = encoder.submit_through(stage, &frame.planar, frame.sample_rate, pts);
                                        }
                                        for frame in encoder.drain() {
                                            let media_time = MediaTime::new(
                                                frame.pts * 48_000 / 90_000,
                                                str0m::media::Frequency::FORTY_EIGHT_KHZ,
                                            );
                                            let on_wire = !session.ice_down();
                                            if let Err(e) = session.write_media(
                                                audio_mid,
                                                audio_pt,
                                                Instant::now(),
                                                media_time,
                                                &frame.data,
                                            ) {
                                                tracing::debug!(
                                                    "WHIP '{}' audio write error: {}",
                                                    config.id, e
                                                );
                                            }
                                            count_sent(&stats, on_wire, 1, frame.data.len(), Some(recv_time_us));
                                        }
                                    }
                                    #[cfg(not(feature = "media-codecs"))]
                                    super::webrtc::ts_demux::DemuxedFrame::OtherAudio { .. } => {}
                                    // MPEG-2 video: re-encoded, or dropped
                                    // and reported once (as on WHEP).
                                    super::webrtc::ts_demux::DemuxedFrame::Mpeg2 { es, pts, pts_known, .. } => {
                                        if matches!(video_encoder_state, WebrtcVideoEncoderState::Disabled) {
                                            if !needs_encode.video {
                                                needs_encode.video = true;
                                                warn_codec_needs_encode(&config.id, flow_id, events, "video", "MPEG-2 video", "video_encode");
                                            }
                                            continue;
                                        }
                                        handle_webrtc_video_frame(
                                            WebrtcVideoSource::Mpeg2(&es),
                                            pts,
                                            pts_known,
                                            recv_time_us,
                                            &mut video_encoder_state,
                                            &mut session,
                                            video_mid,
                                            video_pt,
                                            &stats,
                                            &config.id,
                                            flow_id,
                                            events,
                                        ).await;
                                    }
                                    // Stream discontinuity is metadata for
                                    // stateful decoders; the WebRTC packetizer
                                    // re-anchors on the next IDR independently.
                                    super::webrtc::ts_demux::DemuxedFrame::Discontinuity
                                    | super::webrtc::ts_demux::DemuxedFrame::Scte35(_) => {}
                                }
                            }

                            // Drive str0m: process incoming RTCP/STUN + send
                            // queued output. Whether that ended the session
                            // is checked at the top of the loop.
                            session.drive_udp_io().await;
                            if session.take_keyframe_request() && !force_video_keyframe(&mut video_encoder_state) {
                                tracing::debug!("WHIP client '{}': received PLI/FIR (ignored, passthrough mode)", config.id);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(broadcast::error::RecvError::Closed) => break 'outer,
                    }
                }
            }
        }
    }

    tracing::info!("WebRTC output '{}' stopped", config.id);
}

/// Lazy build of the WebRTC encoder state on the first AAC frame. Mirrors
/// `super::output_rtmp::build_encoder_state` but specialised for the
/// AAC → Opus path. Returns Failed (with a clear log) if the input is
/// non-AAC-LC, ffmpeg is missing, or anything in the decode/encode chain
/// rejects the source.
#[cfg(feature = "webrtc")]
fn build_webrtc_encoder_state(
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    transcode: Option<&super::audio_transcode::TranscodeJson>,
    demuxer: &super::ts_demux::TsDemuxer,
    first_frame: Option<&[u8]>,
    compressed_audio_input: bool,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    output_id: &str,
    events: &EventSender,
) -> WebrtcEncoderState {
    if audio_encode.is_none() {
        return WebrtcEncoderState::Disabled;
    }

    if !compressed_audio_input {
        let msg = format!(
            "WebRTC output '{}': audio_encode is set but the flow input cannot carry TS audio (PCM-only source); audio will be dropped",
            output_id
        );
        tracing::error!("{msg}");
        events.emit_flow(
            EventSeverity::Critical,
            crate::manager::events::category::AUDIO_ENCODE,
            msg,
            flow_id,
        );
        return WebrtcEncoderState::Failed;
    }

    let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() else {
        return WebrtcEncoderState::Lazy;
    };

    if profile != 1 {
        let msg = format!(
            "WebRTC output '{}': audio_encode requires AAC-LC input (ADTS profile=1, AOT=2), got profile={profile} (AOT={}); audio will be dropped",
            output_id,
            profile + 1
        );
        tracing::error!("{msg}");
        events.emit_flow(
            EventSeverity::Critical,
            crate::manager::events::category::AUDIO_ENCODE,
            msg,
            flow_id,
        );
        return WebrtcEncoderState::Failed;
    }

    if sample_rate_from_index(sr_idx).is_none() {
        tracing::error!(
            "WebRTC output '{}': audio_encode rejected unsupported AAC sample_rate_index={sr_idx}",
            output_id
        );
        return WebrtcEncoderState::Failed;
    }
    if ch_cfg == 0 || ch_cfg > 2 {
        tracing::error!(
            "WebRTC output '{}': audio_encode rejected unsupported AAC channel_config={ch_cfg}",
            output_id
        );
        return WebrtcEncoderState::Failed;
    }
    // What the decoder really hands out — see the RTMP output's
    // `build_encoder_state`: an HE-AAC header gives the core's rate and
    // v2's mono core, which SBR and PS double.
    let Some((input_sr, input_ch)) = first_frame
        .and_then(|f| super::audio_decode::aac_decoded_format(profile, sr_idx, ch_cfg, f))
    else {
        return WebrtcEncoderState::Lazy;
    };

    let decoder = match AacDecoder::from_adts_config(profile, sr_idx, ch_cfg) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                "WebRTC output '{}': audio_encode AacDecoder build failed: {e}",
                output_id
            );
            return WebrtcEncoderState::Failed;
        }
    };
    let decoded_as = (decoder.codec_name(), decoder.sample_rate(), decoder.channels());
    build_webrtc_encoder_for_format(
        audio_encode,
        transcode,
        input_sr,
        input_ch,
        Some(decoder),
        decoded_as,
        cancel,
        stats,
        flow_id,
        output_id,
        events,
    )
}

/// The Opus encoder `audio_encode` asks for, opened for a source that
/// decodes to `input_sr` × `input_ch`, behind the channel / rate stage every
/// re-encoding output shares (`audio_transcode::EncoderStage`). `decoder` is
/// an AAC source's; an MP2 / AC-3 / E-AC-3 source brings its own libavcodec
/// decoder and passes `None`. `decoded_as` labels the decode stats.
#[cfg(feature = "webrtc")]
#[allow(clippy::too_many_arguments)]
fn build_webrtc_encoder_for_format(
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    transcode: Option<&super::audio_transcode::TranscodeJson>,
    input_sr: u32,
    input_ch: u8,
    decoder: Option<AacDecoder>,
    decoded_as: (&str, u32, u8),
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    output_id: &str,
    events: &EventSender,
) -> WebrtcEncoderState {
    let Some(enc_cfg) = audio_encode else {
        return WebrtcEncoderState::Disabled;
    };
    // Validation guarantees codec=opus for WebRTC; no need to handle others.
    let Some(codec) = AudioCodec::parse(&enc_cfg.codec) else {
        tracing::error!(
            "WebRTC output '{}': audio_encode unknown codec '{}'",
            output_id, enc_cfg.codec
        );
        return WebrtcEncoderState::Failed;
    };
    if codec != AudioCodec::Opus {
        tracing::error!(
            "WebRTC output '{}': audio_encode codec must be 'opus', got '{}'",
            output_id, enc_cfg.codec
        );
        return WebrtcEncoderState::Failed;
    }

    // The channel / rate stage converts the source to what the Opus encoder
    // ingests: the transcode block when set (audio_encode's sample_rate /
    // channels folded in for the fields it leaves unset — transcode.channels
    // wins over the Opus encoder's channel count, transcode.sample_rate
    // chooses the PCM rate it ingests), otherwise those two alone. Without a
    // block, audio_encode.channels used to open the encoder at a channel
    // count the decoded PCM did not have, and every frame was dropped.
    // Opus on the wire is always 48 kHz regardless of either block.
    let mut stage = super::audio_transcode::EncoderStage::new(
        transcode.cloned(),
        enc_cfg.sample_rate,
        enc_cfg.channels,
    );
    let (enc_in_sr, target_ch) = match stage.prepare(input_sr, input_ch) {
        Ok(out) => out,
        Err(e) => {
            let msg = format!(
                "WebRTC output '{output_id}': audio_encode transcode build failed: {e}"
            );
            tracing::error!("{msg}");
            events.emit_flow(
                EventSeverity::Critical,
                crate::manager::events::category::AUDIO_ENCODE,
                msg,
                flow_id,
            );
            return WebrtcEncoderState::Failed;
        }
    };
    let target_br = enc_cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    let params = EncoderParams {
        codec,
        sample_rate: enc_in_sr,
        channels: target_ch,
        target_bitrate_kbps: target_br,
        // Opus is always 48 kHz on the wire regardless of operator input.
        target_sample_rate: 48_000,
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
        output_id.to_string(),
        stats.clone(),
        Some(events.clone()),
    ) {
        Ok(e) => e,
        Err(AudioEncoderError::FfmpegNotFound) => {
            let msg = format!(
                "WebRTC output '{}': audio_encode requires ffmpeg in PATH but it is not installed; audio will be dropped",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(
                EventSeverity::Critical,
                crate::manager::events::category::AUDIO_ENCODE,
                msg,
                flow_id,
            );
            return WebrtcEncoderState::Failed;
        }
        Err(e) => {
            let msg = format!(
                "WebRTC output '{}': audio_encode spawn failed: {e}",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(
                EventSeverity::Critical,
                crate::manager::events::category::AUDIO_ENCODE,
                msg,
                flow_id,
            );
            return WebrtcEncoderState::Failed;
        }
    };

    // The stage's resampler delay comes off the stamps with the codec's.
    encoder.set_upstream_delay(stage.delay(), enc_in_sr);

    tracing::info!(
        "WebRTC output '{}': audio_encode active (Opus, source {} Hz {} ch -> 48000 Hz {} ch, {} kbps)",
        output_id, input_sr, input_ch, target_ch, target_br,
    );

    // Register decode + encode stats with the shared per-output accumulator.
    let decode_stats = Arc::new(DecodeStats::new());
    stats.set_decode_stats(decode_stats.clone(), decoded_as.0, decoded_as.1, decoded_as.2);
    stats.set_encode_stats(
        encoder.stats_handle(),
        encoder.params().codec.as_str().to_string(),
        encoder.params().target_sample_rate,
        encoder.params().target_channels,
        encoder.params().target_bitrate_kbps,
    );

    WebrtcEncoderState::Active {
        decoder,
        encoder,
        decode_stats,
        stage,
        silence: None,
    }
}

/// Decode one MP2 / AC-3 / E-AC-3 PES (`stream_type`) for the Opus
/// re-encode: with the session's libavcodec decoder (reopened when the codec
/// changes), and — while the encoder is still `Lazy` — building it from the
/// format the source decodes to ([`build_webrtc_encoder_for_format`]). It
/// used to wait for an AAC config such a source never has, so its audio was
/// dropped for good unless `silent_fallback` had built the encoder eagerly.
/// Returns the decoded frames.
#[cfg(all(feature = "webrtc", feature = "media-codecs"))]
#[allow(clippy::too_many_arguments)]
fn decode_other_audio_for_encode(
    encoder_state: &mut WebrtcEncoderState,
    decoder: &mut Option<video_engine::AudioDecoder>,
    decoder_codec: &mut Option<video_codec::AudioDecoderCodec>,
    stream_type: u8,
    data: &[u8],
    pts: u64,
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    transcode: Option<&super::audio_transcode::TranscodeJson>,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    output_id: &str,
    events: &EventSender,
) -> Vec<video_engine::DecodedAudioFrame> {
    if matches!(encoder_state, WebrtcEncoderState::Failed | WebrtcEncoderState::Disabled) {
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
    let mut out = Vec::new();
    for au in crate::engine::audio_decode::split_audio_codec_frames(data, codec) {
        if dec.send_packet(au, pts as i64).is_err() {
            continue;
        }
        while let Ok(frame) = dec.receive_frame() {
            out.push(frame);
        }
    }
    if matches!(encoder_state, WebrtcEncoderState::Lazy)
        && let Some(first) = out.first()
    {
        let channels = first.planar.len() as u8;
        *encoder_state = build_webrtc_encoder_for_format(
            audio_encode,
            transcode,
            first.sample_rate,
            channels,
            None,
            (crate::engine::audio_decode::ff_codec_name(codec), first.sample_rate, channels),
            cancel,
            stats,
            flow_id,
            output_id,
            events,
        );
    }
    out
}

/// Eager encoder construction for `silent_fallback = true` on a WebRTC
/// session. Mirrors [`super::output_rtmp::build_encoder_state_eager_for_silent_fallback`]
/// but tailored to Opus (48 kHz, stereo only).
///
/// Fine details:
/// - WebRTC always emits Opus on the wire; the encoder is opened with
///   input PCM at 48 kHz (the Opus internal clock) and the declared
///   channel count (defaulting to stereo).
/// - A real source AAC frame arriving later triggers lazy construction
///   of the AAC decoder ([`webrtc_lazy_build_decoder`]); the encoder's
///   stage, pinned to 48 kHz and the declared channel count, converts its
///   native format so the encoder sees a consistent PCM format.
#[cfg(feature = "webrtc")]
fn build_webrtc_encoder_state_eager_for_silent_fallback(
    audio_encode: Option<&crate::config::models::AudioEncodeConfig>,
    pending_transcode_cfg: Option<&super::audio_transcode::TranscodeJson>,
    cancel: &CancellationToken,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    output_id: &str,
    events: &EventSender,
) -> WebrtcEncoderState {
    let Some(enc_cfg) = audio_encode else {
        return WebrtcEncoderState::Disabled;
    };

    let codec = match AudioCodec::parse(&enc_cfg.codec) {
        Some(c) => c,
        None => {
            tracing::error!(
                "WebRTC output '{}': audio_encode unknown codec '{}'", output_id, enc_cfg.codec
            );
            return WebrtcEncoderState::Failed;
        }
    };

    // Browsers require Opus; the validator catches bad configs, but
    // guard again here so an eager build doesn't start a non-Opus
    // encoder on the back of a misconfigured silent_fallback block.
    if !matches!(codec, AudioCodec::Opus) {
        tracing::error!(
            "WebRTC output '{}': silent_fallback requires codec=opus (got {})",
            output_id, enc_cfg.codec
        );
        return WebrtcEncoderState::Failed;
    }

    // Opus is always 48 kHz on the wire regardless of the declared
    // sample_rate; the channel count is honoured — a transcode block's
    // first, as on every other build of this stage. Taken from
    // `audio_encode` alone, a block's `channels: 1` met an encoder opened in
    // stereo and the pinned stage fell back to the default conversion,
    // dropping the block's routing.
    let target_sr = 48_000_u32;
    let target_ch = pending_transcode_cfg
        .and_then(|b| b.channels)
        .or(enc_cfg.channels)
        .unwrap_or(2)
        .clamp(1, 2);
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
        output_id.to_string(),
        stats.clone(),
        Some(events.clone()),
    ) {
        Ok(e) => e,
        Err(AudioEncoderError::FfmpegNotFound) => {
            let msg = format!(
                "WebRTC output '{}': audio_encode(silent_fallback) requires ffmpeg in PATH but it is not installed",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(EventSeverity::Critical, category::AUDIO_ENCODE, msg, flow_id);
            return WebrtcEncoderState::Failed;
        }
        Err(e) => {
            let msg = format!(
                "WebRTC output '{}': audio_encode(silent_fallback) encoder spawn failed: {e}",
                output_id
            );
            tracing::error!("{msg}");
            events.emit_flow(EventSeverity::Critical, category::AUDIO_ENCODE, msg, flow_id);
            return WebrtcEncoderState::Failed;
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
        "WebRTC output '{}': audio_encode(silent_fallback) active (Opus 48000 Hz {} ch, {} kbps)",
        output_id, target_ch, target_br,
    );

    // Real audio, when it arrives, is converted to the format the silence
    // opened the encoder at (48 kHz, the declared channel count).
    let mut stage = super::audio_transcode::EncoderStage::new(
        pending_transcode_cfg.cloned(),
        enc_cfg.sample_rate,
        enc_cfg.channels,
    );
    stage.pin_output(target_sr, target_ch);

    WebrtcEncoderState::Active {
        decoder: None,
        encoder,
        decode_stats,
        stage,
        silence: Some(silence),
    }
}

/// Shared silence-tick handler for WebRTC sessions. Submits one
/// zero-filled planar chunk to the encoder, drains any encoded Opus
/// frames, and writes them to the audio MID. Early-returns when:
/// - the encoder state is not Active with a silence generator, or
/// - real audio has arrived within the grace window, or
/// - the session has no audio MID / payload type (SDP negotiated
///   video-only).
#[cfg(feature = "webrtc")]
async fn emit_webrtc_silence_if_needed(
    state: &mut WebrtcEncoderState,
    session: &mut super::webrtc::session::WebrtcSession,
    audio_mid: Option<str0m::media::Mid>,
    audio_pt: Option<str0m::media::Pt>,
    stats: &Arc<OutputStatsAccumulator>,
    output_id: &str,
) {
    use std::time::Instant;
    use str0m::media::MediaTime;

    let (Some(audio_mid), Some(audio_pt)) = (audio_mid, audio_pt) else {
        return;
    };
    let WebrtcEncoderState::Active {
        encoder,
        silence: Some(sg),
        ..
    } = state
    else {
        return;
    };
    if !sg.should_emit() {
        return;
    }
    if !sg.is_emitting() {
        tracing::info!(
            "WebRTC output '{}': source audio absent/stalled — starting silent-Opus injection",
            output_id
        );
    }
    let (planar, pts) = sg.next_chunk();
    encoder.submit_planar(planar, pts);
    for frame in encoder.drain() {
        let media_time = MediaTime::new(
            frame.pts * 48_000 / 90_000,
            str0m::media::Frequency::FORTY_EIGHT_KHZ,
        );
        let on_wire = !session.ice_down();
        if let Err(e) = session.write_media(
            audio_mid,
            audio_pt,
            Instant::now(),
            media_time,
            &frame.data,
        ) {
            tracing::debug!("WebRTC output '{}' silent audio write error: {}", output_id, e);
            continue;
        }
        session.drain_outputs().await;
        count_sent(stats, on_wire, 1, frame.data.len(), None);
    }
}

/// Lazy build of the AAC decoder on the first real source AAC frame
/// observed while the WebRTC encoder is already running (the
/// silent-fallback path). The encoder's stage, pinned to its format (48 kHz
/// Opus input, the declared channel count), converts whatever the source
/// turns out to be.
#[cfg(feature = "webrtc")]
fn webrtc_lazy_build_decoder(
    decoder: &mut Option<AacDecoder>,
    cached_aac: (u8, u8, u8),
    output_id: &str,
) {
    let (profile, sr_idx, ch_cfg) = cached_aac;
    if profile != 1 {
        tracing::warn!(
            "WebRTC output '{}': silent-fallback ignoring non-AAC-LC source frame (profile={profile})",
            output_id
        );
        return;
    }
    match AacDecoder::from_adts_config(profile, sr_idx, ch_cfg) {
        Ok(d) => *decoder = Some(d),
        Err(e) => {
            tracing::warn!(
                "WebRTC output '{}': silent-fallback AacDecoder build failed: {e}",
                output_id
            );
        }
    }
}

#[cfg(all(test, feature = "webrtc", feature = "fdk-aac"))]
mod stage_tests {
    use super::*;

    /// **B1.** `audio_encode.channels` without a `transcode` block on a
    /// WebRTC output goes through the shared channel / rate stage. The Opus
    /// encoder used to be opened at the override's channel count while the
    /// decoded stereo PCM went to it unconverted, and every frame was
    /// dropped (`planar channel count != configured`).
    #[test]
    fn a_channel_override_without_a_transcode_block_converts() {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let enc: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus", "channels": 1 })).unwrap();
        let mut demux = super::super::ts_demux::TsDemuxer::new(None);
        demux.demux(&crate::engine::ts_test_fixtures::aac_program_ts(ADTS));
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let first = demux
            .demux(&crate::engine::ts_test_fixtures::aac_program_ts(ADTS))
            .into_iter()
            .find_map(|f| match f {
                super::super::ts_demux::DemuxedFrame::Aac { data, .. } => Some(data),
                _ => None,
            })
            .unwrap();
        let st = build_webrtc_encoder_state(
            Some(&enc),
            None,
            &demux,
            Some(&first),
            true,
            &CancellationToken::new(),
            &stats,
            "f",
            "w1",
            &events,
        );
        let WebrtcEncoderState::Active { mut encoder, mut stage, .. } = st else {
            panic!("the session must re-encode");
        };
        assert_eq!((encoder.params().channels, encoder.params().target_channels), (1, 1));
        let pcm = stage.process(&[vec![0.1f32; 1024], vec![0.1f32; 1024]], 48_000).unwrap();
        assert_eq!(pcm.len(), 1, "the stage mixes to mono");
        assert!(encoder.submit_planar(&pcm, 90_000), "and the encoder takes it");
    }

    /// An HE-AAC v2 source (header: 24 kHz mono core) goes to the Opus
    /// encoder at what it decodes to, 48 kHz stereo — not converted down to
    /// the core's format the header describes.
    #[test]
    fn an_he_aac_v2_source_is_encoded_at_its_decoded_format() {
        let enc: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus" })).unwrap();
        let ts = crate::engine::ts_test_fixtures::aac_program_ts(
            &crate::engine::ts_test_fixtures::he_aac_adts(true),
        );
        let mut demux = super::super::ts_demux::TsDemuxer::new(None);
        let first = demux
            .demux(&ts)
            .into_iter()
            .find_map(|f| match f {
                super::super::ts_demux::DemuxedFrame::Aac { data, .. } => Some(data),
                _ => None,
            })
            .unwrap();
        assert_eq!(demux.cached_aac_config(), Some((1, 6, 1)));
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let st = build_webrtc_encoder_state(
            Some(&enc),
            None,
            &demux,
            Some(&first),
            true,
            &CancellationToken::new(),
            &stats,
            "f",
            "w1",
            &events,
        );
        let WebrtcEncoderState::Active { encoder, stage, .. } = st else {
            panic!("the session must re-encode");
        };
        assert_eq!((encoder.params().sample_rate, encoder.params().channels), (48_000, 2));
        assert_eq!(stage.output(), Some((48_000, 2)));
    }

    /// The silent-fallback Opus encoder takes the transcode block's channel
    /// count (and so keeps its routing) before `audio_encode`'s.
    /// An MP2 source builds the Opus encoder from what it decodes to (it
    /// waited for an AAC config the source never has, and its audio was
    /// dropped for good).
    #[cfg(feature = "media-codecs")]
    #[test]
    fn an_mp2_source_builds_the_opus_encoder_from_its_decoded_format() {
        let ae: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus" })).unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let cancel = CancellationToken::new();
        let mut state = WebrtcEncoderState::Lazy;
        let (mut dec, mut dec_codec) = (None, None);
        let pes = crate::engine::output_rtmp::tests::mp2_pes(4);
        let mut opus = 0usize;
        for k in 0..20u64 {
            let decoded = decode_other_audio_for_encode(
                &mut state, &mut dec, &mut dec_codec, 0x03, &pes, 900_000 + k * 8_640,
                Some(&ae), None, &cancel, &stats, "f", "w1", &events,
            );
            let WebrtcEncoderState::Active { encoder, stage, .. } = &mut state else {
                panic!("the encoder is built on the first PES that decodes");
            };
            for f in decoded {
                encoder.submit_through(stage, &f.planar, f.sample_rate, 900_000 + k * 8_640).unwrap();
            }
            opus += encoder.drain().len();
        }
        assert!(opus > 50, "{opus} Opus frames from 1.9 s of MP2");
    }

    #[test]
    fn a_silent_fallback_session_takes_the_transcode_blocks_channels() {
        let enc: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus", "silent_fallback": true })).unwrap();
        let block: super::super::audio_transcode::TranscodeJson =
            serde_json::from_value(serde_json::json!({ "channels": 1, "channel_map_with_gain": [[[0, 1.0]]] }))
                .unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let st = build_webrtc_encoder_state_eager_for_silent_fallback(
            Some(&enc),
            Some(&block),
            &CancellationToken::new(),
            &stats,
            "f",
            "w1",
            &events,
        );
        let WebrtcEncoderState::Active { encoder, mut stage, .. } = st else {
            panic!("silent fallback builds eagerly");
        };
        assert_eq!(encoder.params().channels, 1);
        let out = stage.process(&[vec![0.5f32; 1024], vec![0.0f32; 1024]], 48_000).unwrap();
        assert_eq!(out.len(), 1);
        assert!((out[0][512] - 0.5).abs() < 1e-6, "left only: {}", out[0][512]);
    }
}

#[cfg(all(test, feature = "webrtc", feature = "video-encoder-x264"))]
mod rate_tests {
    use super::*;

    /// A peer's keyframe request makes the encoder's next frame an IDR, deep
    /// inside its GOP: a viewer that lost packets, or one back from an
    /// outage, used to wait for the GOP to end (the request was ignored).
    /// The same frame of the same source is not one unrequested.
    #[test]
    fn a_keyframe_request_makes_the_next_frame_an_idr() {
        let idrs = |request_at: Option<usize>| {
            let cfg: VideoEncodeConfig = serde_json::from_value(
                serde_json::json!({"codec": "x264", "preset": "veryfast", "gop_size": 250}),
            )
            .unwrap();
            let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "webrtc".into()));
            let (events, _rx) = crate::manager::events::event_channel();
            let aus = crate::engine::output_rtmp::x264_test_source(40, 3_600);
            let mut state =
                open_webrtc_video_active(&cfg, video_codec::VideoCodec::H264, &aus[0].0, "o", "f", &stats, &events);
            let (mut idrs, mut out, mut requested) = (Vec::new(), 0usize, None);
            for (k, (au, pts)) in aus.iter().enumerate() {
                if Some(k) == request_at {
                    assert!(force_video_keyframe(&mut state), "no encoder to ask");
                    requested = Some(out);
                }
                let nalus = crate::engine::ts_demux::split_annex_b_nalus(au);
                for (frame, _) in encode_one_video_frame_webrtc(&mut state, &nalus_to_annex_b_webrtc(&nalus), *pts, true, "o") {
                    let idr = crate::engine::ts_demux::split_annex_b_nalus(&frame)
                        .iter()
                        .any(|n| n.first().is_some_and(|b| b & 0x1f == 5));
                    if idr {
                        idrs.push(out);
                    }
                    out += 1;
                }
            }
            (idrs, requested)
        };
        let (unrequested, _) = idrs(None);
        let (requested, at) = idrs(Some(12));
        let at = at.unwrap();
        assert!(at > 0 && !unrequested.contains(&at), "frame {at} is an IDR anyway: {unrequested:?}");
        assert!(requested.contains(&at), "requested before frame {at}; IDRs {requested:?}");
    }

    /// Unpinned, the WebRTC encoder opens at the source's measured rate — it
    /// used to open at a flat 30/1 — which needs the decoder to be handed
    /// each access unit's PTS (it used to get none).
    #[test]
    fn an_unpinned_webrtc_encode_opens_at_the_source_rate() {
        let cfg: VideoEncodeConfig =
            serde_json::from_value(serde_json::json!({"codec": "x264", "preset": "veryfast"})).unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "webrtc".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let aus = crate::engine::output_rtmp::x264_test_source(40, 3_600);
        let mut state = open_webrtc_video_active(&cfg, video_codec::VideoCodec::H264, &aus[0].0, "o", "f", &stats, &events);
        let mut sps = None;
        let mut frames = 0;
        for (au, pts) in &aus {
            let nalus = crate::engine::ts_demux::split_annex_b_nalus(au);
            for (out, _) in encode_one_video_frame_webrtc(&mut state, &nalus_to_annex_b_webrtc(&nalus), *pts, true, "o") {
                frames += 1;
                sps = sps.or_else(|| video_engine::find_h264_sps(&out));
            }
        }
        let WebrtcVideoEncoderState::Active(active) = &state else { panic!("not active") };
        assert_eq!(active.pipeline.fps(), (25, 1));
        assert_eq!(sps.and_then(|s| s.timing).map(|(n, t, _)| (n, t)), Some((1, 50)), "VUI 25 fps");
        assert_eq!(frames, 40 - 4);
    }

    /// A source stamping only every 12th picture: its unstamped access
    /// units go to the decoder without a PTS, the rate is measured over
    /// their pictures (25 fps), and each encoded frame's RTP time is its
    /// picture's — the last stamped one plus a frame per picture since. Fed
    /// PTS 0 they came back stamped 0 and the rate was never measured.
    #[test]
    fn a_sparse_pts_source_is_measured_and_stamped_on_webrtc() {
        let cfg: VideoEncodeConfig =
            serde_json::from_value(serde_json::json!({"codec": "x264", "preset": "veryfast"})).unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "webrtc".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let aus = crate::engine::output_rtmp::x264_test_source(80, 3_600);
        let mut state = open_webrtc_video_active(&cfg, video_codec::VideoCodec::H264, &aus[0].0, "o", "f", &stats, &events);
        let mut stamps = Vec::new();
        for (k, (au, pts)) in aus.iter().enumerate() {
            let nalus = crate::engine::ts_demux::split_annex_b_nalus(au);
            let known = k % 12 == 0;
            let fed = if known { *pts } else { 0 };
            stamps.extend(
                encode_one_video_frame_webrtc(&mut state, &nalus_to_annex_b_webrtc(&nalus), fed, known, "o").into_iter().map(|(_, p)| p),
            );
        }
        let WebrtcVideoEncoderState::Active(active) = &state else { panic!("not active") };
        assert_eq!(active.pipeline.fps(), (25, 1));
        assert!(stamps.len() >= 20, "{} frames", stamps.len());
        assert!(stamps.windows(2).all(|w| w[1] == w[0] + 3_600), "{stamps:?}");
    }

    /// Every encoded frame goes out on its own picture's source PTS. A
    /// source with B-frames arrives I P B B (decode order); the decoder
    /// hands pictures back I B B P, and they used to be stamped with the
    /// access unit being fed — RTP timestamps stepping back and forth.
    #[test]
    fn encoded_frames_keep_their_own_pictures_pts() {
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
            // A texture panning a pixel a frame: x264 codes it with B-frames.
            let y: Vec<u8> = (0..w * h)
                .map(|k| (((k % w + i) * 7919 + (k / w) * 104_729) % 181) as u8 + 30)
                .collect();
            let c = vec![128u8; w / 2 * h / 2];
            aus.extend(enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i as i64)).unwrap());
        }
        aus.extend(enc.flush().unwrap());
        assert!(aus.windows(2).any(|a| a[1].pts < a[0].pts), "the source reorders");
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "preset": "veryfast", "fps_num": 25, "fps_den": 1
        }))
        .unwrap();
        let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "webrtc".into()));
        let (events, _rx) = crate::manager::events::event_channel();
        let mut state = open_webrtc_video_active(&cfg, video_codec::VideoCodec::H264, &aus[0].data, "o", "f", &stats, &events);
        let mut stamps = Vec::new();
        for au in &aus {
            let nalus = crate::engine::ts_demux::split_annex_b_nalus(&au.data);
            let pts = 900_000 + au.pts as u64 * 3_600;
            stamps.extend(encode_one_video_frame_webrtc(&mut state, &nalus_to_annex_b_webrtc(&nalus), pts, true, "o").into_iter().map(|(_, p)| p));
        }
        assert!(stamps.len() >= n - 3, "{} frames", stamps.len());
        assert!(stamps.windows(2).all(|w| w[1] == w[0] + 3_600), "{stamps:?}");
    }
}

#[cfg(all(test, feature = "webrtc", feature = "media-codecs"))]
mod needs_encode_tests {
    use super::*;
    use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};

    /// `codec_needs_encode` on WebRTC is output-scoped with the flow in
    /// `details` — the shape RTMP raises it in (it was flow-scoped, the
    /// output only in `details`).
    #[test]
    fn the_warning_is_output_scoped() {
        let (tx, mut rx) = crate::manager::events::event_channel();
        warn_codec_needs_encode("out-w", "flow-w", &tx, "video", "HEVC video", "video_encode");
        let ev = rx.try_recv().expect("event");
        assert_eq!(ev.output_id.as_deref(), Some("out-w"));
        assert!(ev.flow_id.is_none());
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::WEBRTC);
        let d = ev.details.unwrap();
        assert_eq!(d["error_code"], "codec_needs_encode");
        assert_eq!(d["essence"], "video");
        assert_eq!(d["source_codec"], "HEVC video");
        assert_eq!(d["needs"], "video_encode");
        assert_eq!(d["flow_id"], "flow-w");
        assert!(d.get("output_id").is_none());
    }

    /// Audio is reported only where `audio_encode` would help: a decodable
    /// non-Opus source, a session that negotiated audio, no `audio_encode`.
    #[test]
    fn only_audio_an_opus_re_encode_could_carry_is_reported() {
        let demuxer = TsDemuxer::new(None);
        let other = |st: u8| DemuxedFrame::OtherAudio { stream_type: st, data: Vec::new(), pts: 0 };
        let label = |f: &DemuxedFrame, set: bool, negotiated: bool| {
            audio_needs_encode_label(f, &demuxer, set, negotiated)
        };
        assert_eq!(label(&other(0x81), false, true).as_deref(), Some("AC-3 audio (stream_type 0x81)"));
        assert_eq!(label(&other(0x03), false, true).as_deref(), Some("MP2 audio (stream_type 0x03)"));
        // AC-4: no decoder, so `audio_encode` could not help.
        assert_eq!(label(&other(0xAC), false, true), None);
        // `audio_encode` set, or a session with no audio (video_only): none.
        assert_eq!(label(&other(0x81), true, true), None);
        assert_eq!(label(&other(0x81), false, false), None);
        // AAC whose profile the demuxer has not read: not claimed.
        let aac = DemuxedFrame::Aac { data: Vec::new(), pts: 0 };
        assert_eq!(label(&aac, false, true), None);
        // Opus passes through: nothing for `audio_encode` to do.
        let opus = DemuxedFrame::Opus { data: Vec::new(), pts: 0, pts_known: true };
        assert_eq!(label(&opus, false, true), None);
    }

    /// The multistream-Opus Warning is output-scoped, names the layout and
    /// carries its `channel_config_code`.
    #[test]
    fn the_opus_layout_warning_names_the_layout() {
        let (tx, mut rx) = crate::manager::events::event_channel();
        warn_opus_layout_not_carried("out-w", "flow-w", &tx, 6);
        let ev = rx.try_recv().expect("event");
        assert_eq!(ev.output_id.as_deref(), Some("out-w"));
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::WEBRTC);
        assert!(ev.message.contains("6-channel Opus"), "{}", ev.message);
        let d = ev.details.unwrap();
        assert_eq!(d["error_code"], "opus_layout_unsupported");
        assert_eq!(d["channel_config_code"], 6);
        assert_eq!(d["flow_id"], "flow-w");
    }

    /// An Opus source on a session with an `audio_encode` is decoded and
    /// re-encoded through it, like any other source — it used to be dropped
    /// whatever the block said. Without one it stays Opus for the
    /// passthrough, and so does a multistream layout the decoder cannot
    /// take.
    #[test]
    fn an_opus_source_goes_through_audio_encode_only_when_one_is_set() {
        const STEREO: &[u8] = include_bytes!("testdata/sine1k_opus_48k_stereo.ts");
        let mut demux = TsDemuxer::new(None);
        let pes: Vec<DemuxedFrame> = demux
            .demux(&[STEREO, STEREO].concat())
            .into_iter()
            .filter(|f| matches!(f, DemuxedFrame::Opus { .. }))
            .collect();
        assert_eq!(demux.opus_channel_config(), Some(2));
        let is_opus = |f: &DemuxedFrame| matches!(f, DemuxedFrame::Opus { .. });
        let opus = || DemuxedFrame::Opus { data: vec![0x7F, 0xE0, 0x01, 0xFC], pts: 0, pts_known: true };

        assert!(is_opus(&opus_through_audio_encode(opus(), &WebrtcEncoderState::Disabled, Some(2))));
        assert!(is_opus(&opus_through_audio_encode(opus(), &WebrtcEncoderState::Lazy, Some(6))));

        let ae: crate::config::models::AudioEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "opus", "bitrate_kbps": 32 })).unwrap();
        let (events, _rx) = crate::manager::events::event_channel();
        let stats = Arc::new(OutputStatsAccumulator::new("w1".into(), "w1".into(), "webrtc".into()));
        let cancel = CancellationToken::new();
        let mut state = WebrtcEncoderState::Lazy;
        let (mut dec, mut dec_codec) = (None, None);
        let mut encoded = 0usize;
        for frame in pes.into_iter().take(5) {
            let DemuxedFrame::OtherAudio { stream_type, data, pts } =
                opus_through_audio_encode(frame, &state, demux.opus_channel_config())
            else {
                panic!("routed to the re-encode");
            };
            assert_eq!(stream_type, 0x06);
            let decoded = decode_other_audio_for_encode(
                &mut state, &mut dec, &mut dec_codec, stream_type, &data, pts,
                Some(&ae), None, &cancel, &stats, "f", "w1", &events,
            );
            let WebrtcEncoderState::Active { encoder, stage, .. } = &mut state else {
                panic!("the encoder is built on the first PES that decodes");
            };
            assert_eq!(encoder.params().target_bitrate_kbps, 32, "at the block's settings");
            for f in decoded {
                encoder.submit_through(stage, &f.planar, f.sample_rate, pts).unwrap();
            }
            encoded += encoder.drain().len();
        }
        assert!(encoded >= 15, "{encoded} Opus frames re-encoded from 21 packets");
    }
}

#[cfg(all(test, feature = "webrtc"))]
mod whep_server_tests {
    use super::*;
    use crate::api::webrtc::registry::NewSessionMsg;

    /// The real Chrome 124 WHEP offer that panicked str0m inside
    /// `whep_server_loop` in the 2026-10-07 interop run.
    const CHROME_WHEP_OFFER: &str = include_str!("webrtc/testdata/chrome124-whep-recvonly.sdp");
    /// An offer str0m 0.24.1 still panics on, whatever H.264 set is
    /// registered: one RTX PT repairing two H.264 PTs.
    const RTX_REPAIRING_TWO_PTS: &str = include_str!("webrtc/testdata/rtx-repairing-two-pts.sdp");

    /// The WHEP output negotiates every viewer inline, in one task: a viewer
    /// whose negotiation fails — str0m panicking on its offer included — must
    /// cost that viewer its answer and nothing more. A Chrome offer used to
    /// panic str0m there, which ended the task: every later viewer's request
    /// found the reply dropped until the flow restarted.
    #[tokio::test]
    async fn a_failed_viewer_negotiation_leaves_the_whep_output_serving() {
        let config: WebrtcOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "whep-out",
            "name": "WHEP",
            "mode": "whep_server",
            "public_ip": "127.0.0.1",
        }))
        .unwrap();
        let (broadcast_tx, _keep) = broadcast::channel::<RtpPacket>(16);
        let stats = Arc::new(OutputStatsAccumulator::new("whep-out".into(), "WHEP".into(), "webrtc".into()));
        let cancel = CancellationToken::new();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
        let (events, mut raised) = crate::manager::events::event_channel();
        let task = spawn_webrtc_output(
            config, &broadcast_tx, stats, cancel.clone(), Some(session_rx), events, "flow-w".into(), false,
        );

        // A viewer whose offer panics str0m, a Chrome viewer, a viewer whose
        // offer is unusable, another Chrome viewer.
        let offers = [RTX_REPAIRING_TWO_PTS, CHROME_WHEP_OFFER, "v=0\r\nnot an offer\r\n", CHROME_WHEP_OFFER];
        for (viewer, offer) in offers.into_iter().enumerate() {
            let (reply, answer) = tokio::sync::oneshot::channel();
            session_tx
                .send(NewSessionMsg { offer_sdp: offer.to_string(), reply })
                .await
                .unwrap_or_else(|_| panic!("viewer {viewer}: the WHEP output task has ended"));
            let answer = tokio::time::timeout(std::time::Duration::from_secs(10), answer)
                .await
                .unwrap_or_else(|_| panic!("viewer {viewer}: no reply"))
                .unwrap_or_else(|_| panic!("viewer {viewer}: the WHEP output task dropped the reply"));
            match viewer {
                0 => {
                    let err = answer.expect_err("str0m cannot negotiate this offer");
                    assert!(err.to_string().contains("str0m panicked during SDP offer"), "{err}");
                }
                2 => assert!(answer.is_err(), "garbage was answered"),
                _ => {
                    let (sdp, _session, _cancel) = answer.unwrap();
                    assert!(sdp.contains("a=rtpmap:102 H264/90000"), "viewer {viewer}: {sdp}");
                }
            }
        }
        assert!(!task.is_finished());

        // The panic, and only the panic, raised a Warning.
        let mut panics = Vec::new();
        while let Ok(ev) = raised.try_recv() {
            if let Some(d) = ev.details.filter(|d| d["error_code"] == "webrtc_negotiation_panic") {
                assert_eq!(ev.severity, EventSeverity::Warning);
                assert_eq!(ev.flow_id.as_deref(), Some("flow-w"));
                panics.push(d);
            }
        }
        assert_eq!(panics.len(), 1, "{panics:?}");
        assert_eq!(panics[0]["peer"], "WHEP viewer");
        assert_eq!(panics[0]["step"], "SDP offer");
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// A WHEP output on loopback, `video_only` or not.
    fn whep_output(
        video_only: bool,
    ) -> (
        broadcast::Sender<RtpPacket>,
        tokio::sync::mpsc::Sender<NewSessionMsg>,
        tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        CancellationToken,
        JoinHandle<()>,
    ) {
        whep_output_with(
            serde_json::json!({ "video_only": video_only }),
            WHEP_SETUP_DEADLINE,
        )
    }

    /// A WHEP output on loopback with `extra` config fields, its viewers
    /// given `setup_deadline` to connect. The server loop is run as
    /// `spawn_webrtc_output` runs it.
    fn whep_output_with(
        extra: serde_json::Value,
        setup_deadline: std::time::Duration,
    ) -> (
        broadcast::Sender<RtpPacket>,
        tokio::sync::mpsc::Sender<NewSessionMsg>,
        tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        CancellationToken,
        JoinHandle<()>,
    ) {
        let (broadcast_tx, session_tx, raised, cancel, task, _stats) =
            whep_output_with_stats(extra, setup_deadline);
        (broadcast_tx, session_tx, raised, cancel, task)
    }

    /// [`whep_output_with`], and the output's stats.
    #[allow(clippy::type_complexity)]
    fn whep_output_with_stats(
        extra: serde_json::Value,
        setup_deadline: std::time::Duration,
    ) -> (
        broadcast::Sender<RtpPacket>,
        tokio::sync::mpsc::Sender<NewSessionMsg>,
        tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        CancellationToken,
        JoinHandle<()>,
        Arc<OutputStatsAccumulator>,
    ) {
        spawn_whep_output(extra, setup_deadline, false)
    }

    /// [`whep_output_with_stats`] on a flow whose input carries TS audio
    /// (`compressed_audio_input`), so an `audio_encode` opens: without it
    /// the encoder state is `Failed` (a PCM-only source).
    #[allow(clippy::type_complexity)]
    fn whep_output_with_ts_audio(
        extra: serde_json::Value,
    ) -> (
        broadcast::Sender<RtpPacket>,
        tokio::sync::mpsc::Sender<NewSessionMsg>,
        tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        CancellationToken,
        JoinHandle<()>,
        Arc<OutputStatsAccumulator>,
    ) {
        spawn_whep_output(extra, WHEP_SETUP_DEADLINE, true)
    }

    /// The WHEP output the helpers above describe, run as
    /// `spawn_webrtc_output` runs it.
    #[allow(clippy::type_complexity)]
    fn spawn_whep_output(
        extra: serde_json::Value,
        setup_deadline: std::time::Duration,
        compressed_audio_input: bool,
    ) -> (
        broadcast::Sender<RtpPacket>,
        tokio::sync::mpsc::Sender<NewSessionMsg>,
        tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        CancellationToken,
        JoinHandle<()>,
        Arc<OutputStatsAccumulator>,
    ) {
        let mut config = serde_json::json!({
            "id": "whep-out",
            "name": "WHEP",
            "mode": "whep_server",
            "public_ip": "127.0.0.1",
        });
        config.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let config: WebrtcOutputConfig = serde_json::from_value(config).unwrap();
        let (broadcast_tx, rx) = broadcast::channel::<RtpPacket>(1024);
        let stats = Arc::new(OutputStatsAccumulator::new("whep-out".into(), "WHEP".into(), "webrtc".into()));
        let cancel = CancellationToken::new();
        let (session_tx, session_rx) = tokio::sync::mpsc::channel(4);
        let (events, raised) = crate::manager::events::event_channel();
        let task = tokio::spawn({
            let (broadcast_tx, cancel, stats) = (broadcast_tx.clone(), cancel.clone(), stats.clone());
            async move {
                whep_server_loop(
                    config,
                    broadcast_tx,
                    rx,
                    stats,
                    cancel,
                    session_rx,
                    &events,
                    "flow-w",
                    compressed_audio_input,
                    setup_deadline,
                )
                .await;
            }
        });
        (broadcast_tx, session_tx, raised, cancel, task, stats)
    }

    /// An Opus-in-TS source: a 1 kHz tone, fed into `tx` until `stop`.
    fn feed_opus(tx: broadcast::Sender<RtpPacket>, stop: CancellationToken) -> JoinHandle<()> {
        const OPUS_TS: &[u8] = include_bytes!("testdata/sine1k_opus_48k_stereo.ts");
        feed_ts(tx, stop, OPUS_TS.to_vec())
    }

    /// A source whose audio is AAC (H.264 + AAC-LC in its PMT), fed into `tx`
    /// until `stop`.
    fn feed_aac(tx: broadcast::Sender<RtpPacket>, stop: CancellationToken) -> JoinHandle<()> {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        feed_ts(tx, stop, crate::engine::ts_test_fixtures::aac_program_ts(ADTS))
    }

    /// `ts`, fed into `tx` over and over until `stop`.
    fn feed_ts(tx: broadcast::Sender<RtpPacket>, stop: CancellationToken, ts: Vec<u8>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut seq = 0u16;
            while !stop.is_cancelled() {
                for chunk in ts.chunks(7 * 188) {
                    let _ = tx.send(RtpPacket {
                        data: bytes::Bytes::copy_from_slice(chunk),
                        sequence_number: seq,
                        rtp_timestamp: 0,
                        recv_time_us: 0,
                        is_raw_ts: true,
                        upstream_seq: None,
                        upstream_leg_id: None,
                        sender_timestamp_us: None,
                    });
                    seq = seq.wrapping_add(1);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    }

    /// A viewer's peer on loopback — a browser with H.264 and Opus, as this
    /// edge's own codec set is.
    async fn viewer() -> super::super::webrtc::session::WebrtcSession {
        use super::super::webrtc::session::{SessionConfig, WebrtcSession};
        let config = SessionConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            public_ip: None,
            ice_lite: false,
        };
        WebrtcSession::new(&config).await.unwrap()
    }

    /// A viewer's peer, on loopback, offering VP8 for its video — a browser
    /// without H.264 — with Opus for its audio or no audio at all.
    async fn vp8_viewer() -> super::super::webrtc::session::WebrtcSession {
        use super::super::webrtc::session::{SessionConfig, WebrtcSession};
        let vp8 = str0m::Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false);
        let config = SessionConfig { bind_addr: "127.0.0.1:0".parse().unwrap(), public_ip: None, ice_lite: false };
        WebrtcSession::with_rtc_config(&config, vp8).await.unwrap()
    }

    /// Offer `offer` to the output; its reply.
    async fn offer_to(
        session_tx: &tokio::sync::mpsc::Sender<NewSessionMsg>,
        offer: String,
    ) -> anyhow::Result<(String, String, CancellationToken)> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        session_tx
            .send(NewSessionMsg {
                offer_sdp: offer,
                reply,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), answer)
            .await
            .expect("no reply")
            .expect("the WHEP output task dropped the reply")
    }

    /// `viewer` joins the output with a `recvonly` offer of video and audio,
    /// connects, and hears the output's audio.
    async fn join_and_listen(
        session_tx: &tokio::sync::mpsc::Sender<NewSessionMsg>,
        viewer: &mut super::super::webrtc::session::WebrtcSession,
    ) {
        use super::super::webrtc::session::SessionEvent;
        let (offer, pending) = viewer.create_offer(true, true, false).unwrap();
        let (answer, _session, _cancel) = offer_to(session_tx, offer).await.expect("admitted");
        viewer.apply_answer(&answer, pending).unwrap();
        let audio_mid = viewer.audio_mid.unwrap();
        let cancel = CancellationToken::new();
        let heard = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match viewer.poll_event(&cancel).await {
                    SessionEvent::MediaData { mid, data, .. }
                        if mid == audio_mid && !data.is_empty() =>
                    {
                        break;
                    }
                    SessionEvent::Disconnected => panic!("the viewer was disconnected"),
                    _ => {}
                }
            }
        })
        .await;
        assert!(heard.is_ok(), "the viewer heard nothing in 10 s");
    }

    /// Wait up to `within` for an event whose message is `message`: how long
    /// it took, `None` if it did not come.
    async fn event_within(
        raised: &mut tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        message: &str,
        within: std::time::Duration,
    ) -> Option<std::time::Duration> {
        let start = std::time::Instant::now();
        tokio::time::timeout(within, async {
            while let Some(ev) = raised.recv().await {
                if ev.message == message {
                    return Some(start.elapsed());
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
    }

    /// The `webrtc_no_h264` Warnings raised so far, as their details.
    fn no_h264_warnings(raised: &mut tokio::sync::mpsc::Receiver<crate::manager::events::Event>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(ev) = raised.try_recv() {
            if let Some(d) = ev.details.filter(|d| d["error_code"] == "webrtc_no_h264") {
                assert_eq!(ev.severity, EventSeverity::Warning);
                assert_eq!(ev.output_id.as_deref(), Some("whep-out"));
                assert!(ev.message.contains("accepted no H.264 video"), "{}", ev.message);
                out.push(d);
            }
        }
        out
    }

    /// A viewer that offers no H.264 but Opus — its audio m-line first, the
    /// BUNDLE tag — is sent its audio when the source's is Opus. It used to
    /// get its 201, connect, and then nothing at all — its task ended for
    /// want of a video PT, the audio it negotiated with it — and nothing but
    /// a log line said why.
    #[tokio::test]
    async fn a_viewer_without_h264_is_sent_its_audio() {
        use super::super::webrtc::session::SessionEvent;
        use str0m::media::MediaKind;
        let (broadcast_tx, session_tx, mut raised, cancel, task) = whep_output(false);

        // An Opus-in-TS source, fed on until the viewer has heard it: the
        // output has read its PMT by the time the viewer comes.
        let feed_cancel = cancel.child_token();
        let feed = feed_opus(broadcast_tx.clone(), feed_cancel.clone());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let mut viewer = vp8_viewer().await;
        let (offer, pending) = viewer
            .offer_media(&[MediaKind::Audio, MediaKind::Video], false)
            .unwrap();
        let (answer, _session, _viewer_cancel) = offer_to(&session_tx, offer)
            .await
            .expect("admitted for its audio");
        viewer.apply_answer(&answer, pending).unwrap();
        let audio_mid = viewer.audio_mid.unwrap();

        // Decided at answer time, and said once.
        let warned = no_h264_warnings(&mut raised);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert_eq!(warned[0]["peer"], "WHEP viewer");
        assert_eq!(warned[0]["outcome"], "audio_only");
        assert_eq!(warned[0]["flow_id"], "flow-w");

        let heard = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                match viewer.poll_event(&cancel).await {
                    SessionEvent::MediaData { mid, data, .. } if mid == audio_mid && !data.is_empty() => break,
                    SessionEvent::Disconnected => panic!("the viewer was disconnected"),
                    _ => {}
                }
            }
        })
        .await;
        feed_cancel.cancel();
        let _ = feed.await;
        assert!(heard.is_ok(), "the viewer heard no audio in 15 s");
        assert!(!task.is_finished());
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// A real Chrome 124 WHEP offer with VP8 for its video and Opus, the
    /// audio m-line first — the BUNDLE tag. Answered with the video
    /// rejected; the 2026-10 live run heard its Opus.
    const CHROME_VP8_AUDIO_FIRST: &str =
        include_str!("webrtc/testdata/chrome124-whep-vp8-audiofirst.sdp");
    /// The same viewer with its video m-line first, as browsers order them.
    const CHROME_VP8_VIDEO_FIRST: &str =
        include_str!("webrtc/testdata/chrome124-whep-vp8-videofirst.sdp");

    /// A viewer offered nothing this output sends is refused at answer time,
    /// with an error its request is answered 400 on, its reason, and a
    /// Warning. The output keeps serving. Every case but the first two used
    /// to be admitted, 201, to nothing:
    ///
    /// * no H.264 and no Opus, or only Opus to a `video_only` output;
    /// * no H.264 but Opus, to an output with no Opus to send — no
    ///   `audio_encode`, and a source whose audio is AAC;
    /// * no H.264 but Opus, the video m-line first: rejecting it rejects the
    ///   offer's BUNDLE tag, and Chrome cannot apply the answer ("Failed to
    ///   setup RTCP mux") — whether the output has Opus to send or not.
    ///
    /// A viewer refused because the source's audio is not known yet is
    /// `a_viewer_before_the_sources_pmt_is_told_to_retry`.
    #[tokio::test]
    async fn a_viewer_with_nothing_to_be_sent_is_refused() {
        use super::super::webrtc::session::offer_was_at_fault;
        use str0m::media::{MediaKind, MediaKind::*};
        enum Offer {
            Vp8(&'static [MediaKind]),
            Chrome(&'static str),
        }
        enum Source {
            None,
            Opus,
            Aac,
        }
        let opus_encode = serde_json::json!({ "audio_encode": { "codec": "opus" } });
        let cases = [
            (
                serde_json::json!({}),
                Source::None,
                Offer::Vp8(&[Video]),
                "nothing_to_send",
                "and no Opus audio this output sends",
            ),
            (
                serde_json::json!({ "video_only": true }),
                Source::None,
                Offer::Vp8(&[Audio, Video]),
                "nothing_to_send",
                "and no Opus audio",
            ),
            (
                serde_json::json!({}),
                Source::Aac,
                Offer::Vp8(&[Audio, Video]),
                "no_opus_to_send",
                "without an audio_encode",
            ),
            (
                serde_json::json!({}),
                Source::Opus,
                Offer::Chrome(CHROME_VP8_VIDEO_FIRST),
                "video_is_bundle_tag",
                "BUNDLE tag",
            ),
            (
                opus_encode,
                Source::None,
                Offer::Chrome(CHROME_VP8_VIDEO_FIRST),
                "video_is_bundle_tag",
                "put the audio m-line first",
            ),
        ];
        for (n, (extra, source, offer, code, reason)) in cases.into_iter().enumerate() {
            let (broadcast_tx, session_tx, mut raised, cancel, task) =
                whep_output_with(extra, WHEP_SETUP_DEADLINE);
            let feed = match source {
                Source::None => None,
                Source::Opus => Some(feed_opus(broadcast_tx.clone(), cancel.child_token())),
                Source::Aac => Some(feed_aac(broadcast_tx.clone(), cancel.child_token())),
            };
            if feed.is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let offer = match offer {
                Offer::Vp8(kinds) => vp8_viewer().await.offer_media(kinds, false).unwrap().0,
                Offer::Chrome(sdp) => sdp.to_string(),
            };
            let err = offer_to(&session_tx, offer)
                .await
                .expect_err("nothing to send this viewer");
            assert!(offer_was_at_fault(&err), "case {n}: {err}");
            assert!(
                err.to_string().contains("no H.264 video") && err.to_string().contains(reason),
                "case {n}: {err}"
            );

            let warned = no_h264_warnings(&mut raised);
            assert_eq!(warned.len(), 1, "case {n}: {warned:?}");
            assert_eq!(warned[0]["outcome"], "refused", "case {n}");
            assert_eq!(warned[0]["reason"], code, "case {n}");

            // The next viewer, a Chrome one, is answered.
            assert!(
                offer_to(&session_tx, CHROME_WHEP_OFFER.to_string())
                    .await
                    .is_ok(),
                "case {n}"
            );
            assert!(no_h264_warnings(&mut raised).is_empty());
            cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
    }

    /// A Chrome viewer without H.264 whose audio m-line leads its BUNDLE
    /// group is admitted for its audio — when the output has Opus to send:
    /// an Opus source, or an `audio_encode`. The answer keeps the tag (the
    /// audio) and rejects only the video, which Chrome applies.
    #[tokio::test]
    async fn a_chrome_viewer_whose_audio_leads_is_admitted_for_it() {
        for (extra, opus_source) in [
            (serde_json::json!({}), true),
            (
                serde_json::json!({ "audio_encode": { "codec": "opus" } }),
                false,
            ),
        ] {
            let (broadcast_tx, session_tx, mut raised, cancel, task) =
                whep_output_with(extra, WHEP_SETUP_DEADLINE);
            let _feed = opus_source.then(|| feed_opus(broadcast_tx.clone(), cancel.child_token()));
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let (answer, _session, _cancel) =
                offer_to(&session_tx, CHROME_VP8_AUDIO_FIRST.to_string())
                    .await
                    .expect("admitted for its audio");
            assert!(
                answer.contains("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n"),
                "{answer}"
            );
            assert!(
                answer.contains("m=video 0 UDP/TLS/RTP/SAVPF 0\r\n"),
                "{answer}"
            );
            assert!(answer.contains("a=group:BUNDLE 0\r\n"), "{answer}");
            let warned = no_h264_warnings(&mut raised);
            assert_eq!(warned.len(), 1, "{warned:?}");
            assert_eq!(warned[0]["outcome"], "audio_only");
            cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
    }

    /// A viewer without H.264 that comes before the output has read its
    /// source's PMT — at output start, after a flow restart, while the
    /// source is not flowing — is refused for now (`source_audio_unknown`)
    /// and told to retry, not that the source's audio is not Opus; the same
    /// offer is admitted once the PMT says Opus. It used to be refused
    /// `no_opus_to_send`, its Warning saying the source's audio was not Opus.
    #[tokio::test]
    async fn a_viewer_before_the_sources_pmt_is_told_to_retry() {
        use super::super::webrtc::session::offer_was_at_fault;
        let (broadcast_tx, session_tx, mut raised, cancel, task) = whep_output(false);

        let err = offer_to(&session_tx, CHROME_VP8_AUDIO_FIRST.to_string())
            .await
            .expect_err("no PMT read yet");
        assert!(offer_was_at_fault(&err), "{err}");
        let text = err.to_string();
        assert!(text.contains("not yet read its source's audio") && text.contains("retry"), "{text}");
        let warned = no_h264_warnings(&mut raised);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert_eq!(warned[0]["outcome"], "refused");
        assert_eq!(warned[0]["reason"], "source_audio_unknown");

        // The source starts: the same viewer, a moment later, is sent its
        // audio.
        let _feed = feed_opus(broadcast_tx.clone(), cancel.child_token());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        offer_to(&session_tx, CHROME_VP8_AUDIO_FIRST.to_string())
            .await
            .expect("admitted once the PMT says Opus");
        let warned = no_h264_warnings(&mut raised);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert_eq!(warned[0]["outcome"], "audio_only");
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// A viewer whose offer is answered and which then never connects holds
    /// its session only until the setup deadline, with a Warning. It used to
    /// hold its task and UDP socket until the flow stopped.
    #[tokio::test]
    async fn a_viewer_that_never_connects_is_closed_at_the_setup_deadline() {
        let deadline = std::time::Duration::from_secs(1);
        let (_broadcast_tx, session_tx, mut raised, cancel, task) =
            whep_output_with(serde_json::json!({}), deadline);
        // A Chrome viewer that is nowhere: nothing ever reaches its session.
        offer_to(&session_tx, CHROME_WHEP_OFFER.to_string())
            .await
            .expect("answered");
        let message = "WHEP viewer did not connect within 1 s of its answer; its session is closed";
        let at = event_within(&mut raised, message, std::time::Duration::from_secs(5)).await;
        let at = at.expect("no webrtc_setup_timeout within 5 s");
        assert!(
            at >= std::time::Duration::from_millis(900),
            "closed after {at:?}"
        );
        assert!(!task.is_finished());
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// The setup deadline's Warning, as raised.
    #[tokio::test]
    async fn the_setup_deadline_warning_names_the_viewer_and_output() {
        let deadline = std::time::Duration::from_secs(1);
        let (_broadcast_tx, session_tx, mut raised, cancel, task) =
            whep_output_with(serde_json::json!({}), deadline);
        offer_to(&session_tx, CHROME_WHEP_OFFER.to_string())
            .await
            .expect("answered");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let ev = raised.recv().await.unwrap();
                if ev
                    .details
                    .as_ref()
                    .is_some_and(|d| d["error_code"] == "webrtc_setup_timeout")
                {
                    return ev;
                }
            }
        })
        .await
        .expect("no webrtc_setup_timeout");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert_eq!(ev.category, category::WEBRTC);
        assert_eq!(ev.flow_id.as_deref(), Some("flow-w"));
        let d = ev.details.unwrap();
        assert_eq!(d["peer"], "WHEP viewer");
        assert_eq!(d["reason"], "deadline");
        assert_eq!(d["timeout_secs"], 1);
        assert_eq!(d["output_id"], "whep-out");
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// A viewer whose offer carries a candidate the output can pair with (an
    /// IP host candidate here; a srflx or relay one from any player with a
    /// STUN or TURN server) and that then never answers a check fails its
    /// setup when ICE gives up on it, 15 s after its answer — before the
    /// deadline — and that is reported as the failed setup it is: a Warning
    /// `webrtc_setup_timeout`, reason `ice_failed`. It used to end with an
    /// Info "WHEP viewer disconnected" alone; only an offer with no usable
    /// candidate (mDNS only) reached the deadline and its Warning.
    #[tokio::test]
    async fn a_viewer_whose_ice_fails_during_setup_is_reported() {
        let deadline = std::time::Duration::from_secs(25);
        let (_broadcast_tx, session_tx, mut raised, cancel, task) =
            whep_output_with(serde_json::json!({}), deadline);
        // A viewer on loopback that offers its host candidate, then is never
        // driven: not one check reaches the output.
        let mut silent = viewer().await;
        let (offer, _pending) = silent.create_offer(true, true, false).unwrap();
        assert!(offer.contains(" 127.0.0.1 "), "{offer}");
        offer_to(&session_tx, offer).await.expect("answered");
        let started = std::time::Instant::now();

        let ev = tokio::time::timeout(deadline, async {
            loop {
                let ev = raised.recv().await.unwrap();
                assert_ne!(ev.message, "WHEP viewer disconnected", "an Info alone");
                if ev
                    .details
                    .as_ref()
                    .is_some_and(|d| d["error_code"] == "webrtc_setup_timeout")
                {
                    return ev;
                }
            }
        })
        .await
        .expect("no webrtc_setup_timeout before the deadline");
        assert!(started.elapsed() < deadline, "the deadline, not ICE, ended it");
        assert_eq!(ev.severity, EventSeverity::Warning);
        assert!(ev.message.starts_with("WHEP viewer did not connect: ICE failed "), "{}", ev.message);
        let d = ev.details.unwrap();
        assert_eq!(d["peer"], "WHEP viewer");
        assert_eq!(d["reason"], "ice_failed");
        assert_eq!(d["timeout_secs"], 25);
        assert_eq!(d["output_id"], "whep-out");
        drop(silent);
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// However a viewer ends — the setup deadline, its DTLS close, a DELETE
    /// — its entry leaves the session registry. Only a DELETE removed one
    /// before, and departed viewers are the ones that send none: each held
    /// its entry (and a child token) until the flow stopped.
    #[tokio::test]
    async fn a_viewer_that_ends_leaves_the_session_registry() {
        use crate::api::webrtc::registry::WebrtcSessionRegistry;
        let (broadcast_tx, session_tx, _raised, cancel, task) =
            whep_output_with(serde_json::json!({}), std::time::Duration::from_secs(1));
        let _feed = feed_opus(broadcast_tx.clone(), cancel.child_token());
        let registry = WebrtcSessionRegistry::new();
        registry.register_whep_output("flow-w", session_tx, None);
        /// The flow's sessions are all gone within 5 s.
        async fn emptied(registry: &WebrtcSessionRegistry) -> bool {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while registry.session_count("flow-w") > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .is_ok()
        }

        // A viewer that never connects: closed at the 1 s deadline.
        registry
            .handle_whep_offer("flow-w", CHROME_WHEP_OFFER)
            .await
            .expect("answered");
        assert_eq!(registry.session_count("flow-w"), 1);
        assert!(emptied(&registry).await, "the timed-out viewer is still listed");

        // A viewer that connects, then closes its session.
        let mut closing = viewer().await;
        let (offer, pending) = closing.create_offer(true, true, false).unwrap();
        let (answer, _session_id) = registry
            .handle_whep_offer("flow-w", &offer)
            .await
            .expect("admitted");
        closing.apply_answer(&answer, pending).unwrap();
        let viewer_cancel = CancellationToken::new();
        let connected = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !matches!(
                closing.poll_event(&viewer_cancel).await,
                super::super::webrtc::session::SessionEvent::Connected
            ) {}
        })
        .await;
        assert!(connected.is_ok(), "the viewer never connected");
        assert_eq!(registry.session_count("flow-w"), 1);
        closing.close().await;
        assert!(emptied(&registry).await, "the closed viewer is still listed");

        // A DELETE still ends one, and removes it.
        let (_answer, session_id) = registry
            .handle_whep_offer("flow-w", CHROME_WHEP_OFFER)
            .await
            .expect("answered");
        registry.remove_session("flow-w", &session_id);
        assert_eq!(registry.session_count("flow-w"), 0);

        assert!(!task.is_finished());
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// While ICE is down — a viewer gone quiet, the grace running — nothing
    /// is written to it: str0m keeps the last nominated address, and a
    /// departed viewer was sent its full bitrate (2.3 Mbit/s, measured) for
    /// the whole grace. Media flows while the viewer is heard from, and stops
    /// once ICE has given up on it (15 s after its last check) — while the
    /// session is still held for the grace, not only when it ends.
    #[tokio::test]
    async fn a_viewer_whose_ice_is_down_is_sent_nothing() {
        use std::time::{Duration, Instant};
        let (broadcast_tx, session_tx, mut raised, cancel, task, stats) =
            whep_output_with_stats(serde_json::json!({}), WHEP_SETUP_DEADLINE);
        let _feed = feed_opus(broadcast_tx.clone(), cancel.child_token());
        let mut viewer = viewer().await;
        join_and_listen(&session_tx, &mut viewer).await;
        let sent = || stats.packets_sent.load(Ordering::Relaxed);

        // The viewer stops being driven: no more checks, nothing read. Wait
        // for the output to stop sending: 3 s without one packet.
        let paused_at = Instant::now();
        let (mut last, mut changed_at, mut grew) = (sent(), paused_at, false);
        while changed_at.elapsed() < Duration::from_secs(3) {
            assert!(
                paused_at.elapsed() < Duration::from_secs(45),
                "the output never stopped sending to the departed viewer"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
            if sent() != last {
                (last, changed_at, grew) = (sent(), Instant::now(), true);
            }
        }
        assert!(grew, "nothing was sent while ICE was up");
        assert!(
            changed_at - paused_at >= Duration::from_secs(5),
            "stopped {:?} after the pause, before ICE could have given up",
            changed_at - paused_at
        );
        // It stopped because ICE is down, while the grace runs: the viewer
        // has not been let go. (It used to stop only then, at about 30 s.)
        let mut gone = false;
        while let Ok(ev) = raised.try_recv() {
            gone |= ev.message == "WHEP viewer disconnected";
        }
        assert!(
            !gone,
            "sent to until it was let go, {:?} after the pause",
            changed_at - paused_at
        );
        drop(viewer);
        assert!(!task.is_finished());
        cancel.cancel();
    }

    /// A viewer that leaves without a DELETE — its peer simply gone, as when
    /// a tab is killed — is let go once ICE has given up on it (15 s after
    /// its last check) and the grace has run (`ICE_DISCONNECT_GRACE`): about
    /// 30 s, the browser's own consent window. Both with the source flowing,
    /// where ICE's giving up surfaced in a drain that threw it away, and with
    /// the source stalled, where nothing drove the session at all. Either way
    /// the viewer was sent the stream until the flow stopped.
    #[tokio::test]
    async fn a_viewer_that_leaves_without_a_delete_is_reaped() {
        use super::super::webrtc::session::ICE_DISCONNECT_GRACE;
        let (fed_tx, fed_offers, mut fed_raised, fed_cancel, fed_task) = whep_output(false);
        let _feed = feed_opus(fed_tx.clone(), fed_cancel.child_token());
        let (_stalled_tx, stalled_offers, mut stalled_raised, stalled_cancel, stalled_task) =
            whep_output(false);

        let mut fed_viewer = viewer().await;
        join_and_listen(&fed_offers, &mut fed_viewer).await;
        // The stalled output's viewer connects and is then sent nothing.
        let mut stalled_viewer = viewer().await;
        let (offer, pending) = stalled_viewer.create_offer(true, true, false).unwrap();
        let (answer, _session, _cancel) = offer_to(&stalled_offers, offer).await.expect("admitted");
        stalled_viewer.apply_answer(&answer, pending).unwrap();
        let viewer_cancel = CancellationToken::new();
        let connected = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !matches!(
                stalled_viewer.poll_event(&viewer_cancel).await,
                super::super::webrtc::session::SessionEvent::Connected
            ) {}
        })
        .await;
        assert!(
            connected.is_ok(),
            "the stalled output's viewer never connected"
        );
        assert!(
            event_within(
                &mut stalled_raised,
                "WHEP viewer connected",
                std::time::Duration::from_secs(10)
            )
            .await
            .is_some(),
            "the stalled output's viewer never connected"
        );

        // Both peers vanish without a word.
        drop((fed_viewer, stalled_viewer));
        let bound = std::time::Duration::from_secs(15)
            + ICE_DISCONNECT_GRACE
            + std::time::Duration::from_secs(10);
        let (fed, stalled) = tokio::join!(
            event_within(&mut fed_raised, "WHEP viewer disconnected", bound),
            event_within(&mut stalled_raised, "WHEP viewer disconnected", bound),
        );
        for (which, after) in [("fed", fed), ("stalled", stalled)] {
            let after = after.unwrap_or_else(|| {
                panic!("the {which} output's viewer was not let go within {bound:?}")
            });
            assert!(
                after >= ICE_DISCONNECT_GRACE,
                "the {which} viewer was let go after {after:?}, before the grace"
            );
        }
        assert!(!fed_task.is_finished() && !stalled_task.is_finished());
        fed_cancel.cancel();
        stalled_cancel.cancel();
    }

    /// A viewer that closes its session — DTLS close_notify, what a
    /// browser's `pc.close()` sends — is let go at once.
    /// str0m goes inert on it, so no ICE timeout ever followed: the viewer
    /// was waited on, and sent the stream, until the flow stopped.
    #[tokio::test]
    async fn a_viewer_that_closes_its_session_is_let_go_at_once() {
        let (broadcast_tx, session_tx, mut raised, cancel, task) = whep_output(false);
        let _feed = feed_opus(broadcast_tx.clone(), cancel.child_token());
        let mut viewer = viewer().await;
        join_and_listen(&session_tx, &mut viewer).await;
        viewer.close().await;
        let after = event_within(
            &mut raised,
            "WHEP viewer disconnected",
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(
            after.is_some(),
            "a viewer that closed its session was not let go within 5 s"
        );
        assert!(!task.is_finished());
        cancel.cancel();
    }

    /// A viewer whose checks pause for 18 s — past the 15 s after which ICE
    /// reports it `Disconnected` — and then resume is kept: it is sent the
    /// stream again, past the moment the grace would have run out. A session
    /// ended at ICE's first `Disconnected` (bilbycast-relay 9836299) closed it
    /// at about 15 s, and with no reconnect in the page, its viewer stayed
    /// black.
    #[tokio::test]
    async fn a_viewer_that_pauses_and_comes_back_is_kept() {
        use super::super::webrtc::session::SessionEvent;
        let (broadcast_tx, session_tx, mut raised, cancel, task) = whep_output(false);
        let _feed = feed_opus(broadcast_tx.clone(), cancel.child_token());
        let mut viewer = viewer().await;
        join_and_listen(&session_tx, &mut viewer).await;
        let audio_mid = viewer.audio_mid.unwrap();

        // The viewer is not driven for 18 s: no checks, nothing read.
        let paused_at = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs(18)).await;

        // Back: what it hears from 28 s on — well after the buffered frames
        // are read, and after a session counted over at ~15 s plus the grace
        // would have ended — is what the output sends it now.
        let heard_from = paused_at + std::time::Duration::from_secs(28);
        let until = paused_at + std::time::Duration::from_secs(33);
        let mut late = 0;
        let viewer_cancel = CancellationToken::new();
        let _ = tokio::time::timeout_at(until.into(), async {
            loop {
                match viewer.poll_event(&viewer_cancel).await {
                    SessionEvent::MediaData { mid, .. }
                        if mid == audio_mid && std::time::Instant::now() >= heard_from =>
                    {
                        late += 1;
                    }
                    SessionEvent::Disconnected => panic!("the viewer's own session ended"),
                    _ => {}
                }
            }
        })
        .await;
        let mut gone = false;
        while let Ok(ev) = raised.try_recv() {
            gone |= ev.message == "WHEP viewer disconnected";
        }
        assert!(!gone, "the viewer was let go");
        assert!(
            late > 100,
            "{late} frames heard 28-33 s after the pause began"
        );
        assert!(!task.is_finished());
        cancel.cancel();
    }

    /// A live source — H.264 and AAC-LC, an access unit every 40 ms with the
    /// audio frames that cover it — whose PTS follows the wall clock, fed
    /// into `tx` until `stop`. (`feed_aac` repeats its file every 50 ms, so
    /// its PTS bears no relation to the clock.) The H.264 is an SPS, a PPS
    /// and an IDR slice of filler: enough to be passed through, not decoded.
    #[cfg(all(feature = "fdk-aac", feature = "media-codecs"))]
    fn feed_live_aac(tx: broadcast::Sender<RtpPacket>, stop: CancellationToken) -> JoinHandle<()> {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pes_packets, pmt_section};
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let mut frames = Vec::new();
        let mut off = 0usize;
        while off + 7 <= ADTS.len() {
            let len = (((ADTS[off + 3] as usize) & 0x03) << 11)
                | ((ADTS[off + 4] as usize) << 3)
                | ((ADTS[off + 5] as usize) >> 5);
            if len < 7 || off + len > ADTS.len() {
                break;
            }
            frames.push(ADTS[off..off + len].to_vec());
            off += len;
        }
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
        let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x01, 0x41, 0xfb, 0x01, 0x10];
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);
        au.extend(std::iter::repeat_n(0x5a, 600));
        tokio::spawn(async move {
            let (mut cc_v, mut cc_a, mut cc_pat, mut cc_pmt) = (0u8, 0u8, 0u8, 0u8);
            let (mut pts_v, mut pts_a) = (90_000u64, 90_000u64);
            let (mut frame, mut seq, mut n) = (0usize, 0u16, 0u64);
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(40));
            while !stop.is_cancelled() {
                tick.tick().await;
                let mut ts = Vec::new();
                if n % 10 == 0 {
                    ts.extend_from_slice(&pat_packet(&[(1, 0x1000)], 0, cc_pat));
                    cc_pat = (cc_pat + 1) & 0x0F;
                    ts.extend_from_slice(&packetize_sections(0x1000, &[&pmt], cc_pmt)[0]);
                    cc_pmt = (cc_pmt + 1) & 0x0F;
                }
                n += 1;
                ts.extend(pes_packets(0x100, 0xE0, &au, pts_v, &mut cc_v));
                while pts_a < pts_v + 3600 {
                    ts.extend(pes_packets(0x101, 0xC0, &frames[frame % frames.len()], pts_a, &mut cc_a));
                    frame += 1;
                    pts_a += 1920;
                }
                pts_v += 3600;
                for chunk in ts.chunks(7 * 188) {
                    let _ = tx.send(RtpPacket {
                        data: bytes::Bytes::copy_from_slice(chunk),
                        sequence_number: seq,
                        rtp_timestamp: 0,
                        recv_time_us: 0,
                        is_raw_ts: true,
                        upstream_seq: None,
                        upstream_leg_id: None,
                        sender_timestamp_us: None,
                    });
                    seq = seq.wrapping_add(1);
                }
            }
        })
    }

    /// A viewer of a live AAC source re-encoded to Opus (`audio_encode`, the
    /// output's config) pauses for 18 s — ICE goes down at 15 s — and comes
    /// back. Each stream's lag (arrival time less RTP time) is measured over
    /// the 2 s before the pause and from 5 s after it (once what the
    /// viewer's socket held is read); both streams follow the source's PTS,
    /// which follows the wall clock, so both lags should move by the same
    /// amount. Returns how far audio's moved against video's, in seconds.
    #[cfg(all(feature = "fdk-aac", feature = "media-codecs"))]
    async fn audio_slip_against_video_across_an_outage(audio_encode: serde_json::Value) -> f64 {
        use super::super::webrtc::session::{SessionEvent, WebrtcSession};
        use std::time::{Duration, Instant};

        /// (arrival time, is audio, lag) of every media frame heard for `dur`.
        async fn listen(
            viewer: &mut WebrtcSession,
            (audio_mid, video_mid): (str0m::media::Mid, str0m::media::Mid),
            t0: Instant,
            dur: Duration,
        ) -> Vec<(f64, bool, f64)> {
            let mut heard = Vec::new();
            let cancel = CancellationToken::new();
            let _ = tokio::time::timeout(dur, async {
                loop {
                    match viewer.poll_event(&cancel).await {
                        SessionEvent::MediaData { mid, rtp_time, data, .. }
                            if !data.is_empty() && (mid == audio_mid || mid == video_mid) =>
                        {
                            let t = t0.elapsed().as_secs_f64();
                            heard.push((t, mid == audio_mid, t - rtp_time.as_seconds()));
                        }
                        SessionEvent::Disconnected => panic!("the viewer's own session ended"),
                        _ => {}
                    }
                }
            })
            .await;
            heard
        }
        /// The median lag of one stream's frames heard between `from` and `to`.
        fn lag(heard: &[(f64, bool, f64)], audio: bool, from: f64, to: f64) -> f64 {
            let mut lags: Vec<f64> = heard
                .iter()
                .filter(|h| h.1 == audio && h.0 >= from && h.0 <= to)
                .map(|h| h.2)
                .collect();
            assert!(
                !lags.is_empty(),
                "no {} heard {from:.1}-{to:.1} s",
                if audio { "audio" } else { "video" }
            );
            lags.sort_by(f64::total_cmp);
            lags[lags.len() / 2]
        }

        let (broadcast_tx, session_tx, mut raised, cancel, task, stats) =
            whep_output_with_ts_audio(serde_json::json!({ "audio_encode": audio_encode }));
        let _feed = feed_live_aac(broadcast_tx.clone(), cancel.child_token());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut viewer = viewer().await;
        join_and_listen(&session_tx, &mut viewer).await;
        let mids = (viewer.audio_mid.unwrap(), viewer.video_mid.unwrap());
        let t0 = Instant::now();
        let before = listen(&mut viewer, mids, t0, Duration::from_secs(4)).await;

        // The viewer is not driven for 18 s: no checks, nothing read. From
        // 15 s ICE is down, and nothing is sent.
        let paused_at = t0.elapsed().as_secs_f64();
        tokio::time::sleep(Duration::from_millis(16_500)).await;
        let sent_at_16_5 = stats.packets_sent.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(
            stats.packets_sent.load(Ordering::Relaxed),
            sent_at_16_5,
            "sent to 16.5-18 s into the pause: ICE was not down, and the pause proves nothing"
        );

        let resumed_at = t0.elapsed().as_secs_f64();
        let after = listen(&mut viewer, mids, t0, Duration::from_secs(10)).await;
        let (audio_before, video_before) =
            (lag(&before, true, paused_at - 2.0, paused_at), lag(&before, false, paused_at - 2.0, paused_at));
        let (audio_after, video_after) =
            (lag(&after, true, resumed_at + 5.0, f64::MAX), lag(&after, false, resumed_at + 5.0, f64::MAX));
        let mut gone = false;
        while let Ok(ev) = raised.try_recv() {
            gone |= ev.message == "WHEP viewer disconnected";
        }
        assert!(!gone, "the viewer was let go");
        assert!(!task.is_finished());
        cancel.cancel();
        (audio_after - audio_before) - (video_after - video_before)
    }

    /// A viewer of an `audio_encode` output that comes back from an outage
    /// hears its audio on its video's clock. The send loop used to skip every
    /// frame while ICE was down, and the Opus encoder — anchored on the
    /// source's PTS once, then run on from there — resumed where it had
    /// stopped: the viewer's audio was late by the whole outage for the rest
    /// of the session (3.8 s, measured).
    #[cfg(all(feature = "fdk-aac", feature = "media-codecs"))]
    #[tokio::test]
    async fn an_encoded_viewers_audio_keeps_time_with_its_video_across_an_outage() {
        let slip = audio_slip_against_video_across_an_outage(serde_json::json!({ "codec": "opus" })).await;
        assert!(slip.abs() < 0.5, "audio slipped {slip:.3} s against video across the outage");
    }

    /// The same with `silent_fallback`, whose encoder is opened at once and
    /// fed silence whenever the source's audio stops.
    #[cfg(all(feature = "fdk-aac", feature = "media-codecs"))]
    #[tokio::test]
    async fn a_silent_fallback_viewers_audio_keeps_time_with_its_video_across_an_outage() {
        let slip = audio_slip_against_video_across_an_outage(
            serde_json::json!({ "codec": "opus", "silent_fallback": true }),
        )
        .await;
        assert!(slip.abs() < 0.5, "audio slipped {slip:.3} s against video across the outage");
    }
}

#[cfg(all(test, feature = "webrtc"))]
mod whip_client_tests {
    use super::*;
    use axum::extract::Path;
    use axum::http::{StatusCode, header};

    /// A WHIP endpoint whose answer accepts no H.264 — one that takes VP8 and
    /// Opus only. Each publish's resource is deleted and the publish retried
    /// after an exponential backoff, with a Warning. It used to connect over
    /// the audio alone, find no video PT and start over at once — a new
    /// session, POST and ICE / DTLS each time, the resource never deleted,
    /// and no Warning.
    #[tokio::test]
    async fn an_endpoint_without_h264_is_left_and_retried_with_backoff() {
        use std::sync::Mutex;
        use std::sync::atomic::AtomicUsize;
        // What `main` installs before any TLS client is built.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let posts = Arc::new(AtomicUsize::new(0));
        let deleted = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = axum::Router::new()
            .route(
                "/whip",
                axum::routing::post({
                    let posts = posts.clone();
                    move |offer: String| async move {
                        let n = posts.fetch_add(1, Ordering::SeqCst);
                        let mut rtc = str0m::Rtc::builder()
                            .set_ice_lite(true)
                            .clear_codecs()
                            .enable_vp8(true)
                            .enable_opus(true, false)
                            .build(std::time::Instant::now());
                        let offer = str0m::change::SdpOffer::from_sdp_string(&offer).unwrap();
                        let answer = rtc.sdp_api().accept_offer(offer).unwrap().to_sdp_string();
                        // The video rejected as a server outside str0m
                        // rejects it (Pion: port 0, format 0); str0m's own
                        // empty format list is not SDP.
                        let rejected = "m=video 0 UDP/TLS/RTP/SAVPF \r\n";
                        assert!(answer.contains(rejected), "{answer}");
                        let answer = answer.replace(rejected, "m=video 0 UDP/TLS/RTP/SAVPF 0\r\n");
                        (
                            StatusCode::CREATED,
                            [(header::LOCATION, format!("/whip/res-{n}")), (header::CONTENT_TYPE, "application/sdp".to_string())],
                            answer,
                        )
                    }
                }),
            )
            .route(
                "/whip/{resource}",
                axum::routing::delete({
                    let deleted = deleted.clone();
                    move |Path(resource): Path<String>| async move {
                        deleted.lock().unwrap().push(resource);
                        StatusCode::OK
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config: WebrtcOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "whip-out",
            "name": "WHIP",
            "mode": "whip_client",
            "whip_url": format!("http://{addr}/whip"),
            "public_ip": "127.0.0.1",
        }))
        .unwrap();
        let (broadcast_tx, _keep) = broadcast::channel::<RtpPacket>(16);
        let stats = Arc::new(OutputStatsAccumulator::new("whip-out".into(), "WHIP".into(), "webrtc".into()));
        let cancel = CancellationToken::new();
        let (events, mut raised) = crate::manager::events::event_channel();
        let task = spawn_webrtc_output(config, &broadcast_tx, stats, cancel.clone(), None, events, "flow-w".into(), false);

        // POSTs at about 0, 1 and 3 s; the fourth would be at 7 s.
        tokio::time::sleep(std::time::Duration::from_millis(4_500)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        server.abort();

        let posts = posts.load(Ordering::SeqCst);
        assert_eq!(posts, 3, "{posts} publishes in 4.5 s");
        assert_eq!(*deleted.lock().unwrap(), ["res-0", "res-1", "res-2"]);
        let mut retries = Vec::new();
        while let Ok(ev) = raised.try_recv() {
            if let Some(d) = ev.details.filter(|d| d["error_code"] == "webrtc_no_h264") {
                assert_eq!(ev.severity, EventSeverity::Warning);
                assert_eq!(ev.output_id.as_deref(), Some("whip-out"));
                assert_eq!(d["peer"], "WHIP endpoint");
                assert_eq!(d["outcome"], "retrying");
                assert_eq!(d["flow_id"], "flow-w");
                retries.push(d["retry_secs"].as_u64().unwrap());
            }
        }
        assert_eq!(retries, [1, 2, 4]);
    }

    /// What [`webrtc_endpoint`]'s WHIP endpoint does with each publish.
    #[derive(Clone, Copy)]
    enum Endpoint {
        /// Answers with a DTLS fingerprint that is not its own: ICE
        /// completes and the handshake then fails, every time.
        WrongFingerprint,
        /// Connects, then closes the session (DTLS close_notify), as a
        /// server dropping its publisher does.
        ClosesOnConnect,
    }

    /// A WHIP endpoint on loopback answering every publish with a real
    /// ICE-Lite session of this edge's own (`WebrtcSession`), behaving as
    /// `mode` says. Its address, and the POSTs and DELETEs it has had.
    async fn webrtc_endpoint(
        mode: Endpoint,
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::Mutex<Vec<String>>>,
        JoinHandle<()>,
    ) {
        use super::super::webrtc::session::{SessionConfig, SessionEvent, WebrtcSession};
        use std::sync::Mutex;
        use std::sync::atomic::AtomicUsize;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let posts = Arc::new(AtomicUsize::new(0));
        let deleted = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = axum::Router::new()
            .route(
                "/whip",
                axum::routing::post({
                    let posts = posts.clone();
                    move |offer: String| async move {
                        let n = posts.fetch_add(1, Ordering::SeqCst);
                        let config = SessionConfig {
                            bind_addr: "127.0.0.1:0".parse().unwrap(),
                            public_ip: None,
                            ice_lite: true,
                        };
                        let mut server = WebrtcSession::new(&config).await.unwrap();
                        let mut answer = server.accept_offer(&offer).unwrap();
                        if let Endpoint::WrongFingerprint = mode {
                            let theirs = answer
                                .lines()
                                .find_map(|l| l.strip_prefix("a=fingerprint:sha-256 "))
                                .unwrap()
                                .trim_end()
                                .to_string();
                            let wrong = ["00"; 32].join(":");
                            assert_ne!(theirs, wrong);
                            answer = answer.replace(&theirs, &wrong);
                        }
                        // Drive the session for as long as the test runs.
                        tokio::spawn(async move {
                            let cancel = CancellationToken::new();
                            let _ =
                                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                                    loop {
                                        match server.poll_event(&cancel).await {
                                            SessionEvent::Connected => {
                                                if let Endpoint::ClosesOnConnect = mode {
                                                    server.close().await;
                                                }
                                            }
                                            SessionEvent::Disconnected => break,
                                            _ => {}
                                        }
                                    }
                                })
                                .await;
                        });
                        (
                            StatusCode::CREATED,
                            [
                                (header::LOCATION, format!("/whip/res-{n}")),
                                (header::CONTENT_TYPE, "application/sdp".to_string()),
                            ],
                            answer,
                        )
                    }
                }),
            )
            .route(
                "/whip/{resource}",
                axum::routing::delete({
                    let deleted = deleted.clone();
                    move |Path(resource): Path<String>| async move {
                        deleted.lock().unwrap().push(resource);
                        StatusCode::OK
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, posts, deleted, server)
    }

    /// A WHIP output publishing to `addr`, its source silent (kept open by
    /// the sender returned: a closed one stops the output).
    fn whip_output(
        addr: std::net::SocketAddr,
    ) -> (
        broadcast::Sender<RtpPacket>,
        CancellationToken,
        tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        JoinHandle<()>,
    ) {
        let config: WebrtcOutputConfig = serde_json::from_value(serde_json::json!({
            "id": "whip-out",
            "name": "WHIP",
            "mode": "whip_client",
            "whip_url": format!("http://{addr}/whip"),
            "public_ip": "127.0.0.1",
        }))
        .unwrap();
        let (broadcast_tx, _) = broadcast::channel::<RtpPacket>(16);
        let stats = Arc::new(OutputStatsAccumulator::new(
            "whip-out".into(),
            "WHIP".into(),
            "webrtc".into(),
        ));
        let cancel = CancellationToken::new();
        let (events, raised) = crate::manager::events::event_channel();
        let task = spawn_webrtc_output(
            config,
            &broadcast_tx,
            stats,
            cancel.clone(),
            None,
            events,
            "flow-w".into(),
            false,
        );
        (broadcast_tx, cancel, raised, task)
    }

    /// An endpoint whose every ICE / DTLS handshake fails is published to
    /// again only after the backoff — 1 s, then doubling, as for any other
    /// failed attempt — and each failed session's resource is deleted. It
    /// used to go straight back to a new session and POST, without end (the
    /// backoff was reset as soon as signaling succeeded), leaving every
    /// resource behind.
    #[tokio::test]
    async fn a_handshake_that_fails_is_left_and_retried_with_backoff() {
        let (addr, posts, deleted, server) = webrtc_endpoint(Endpoint::WrongFingerprint).await;
        let (_source, cancel, _raised, task) = whip_output(addr);

        // POSTs at about 0, 1 and 3 s; the fourth would be at 7 s.
        tokio::time::sleep(std::time::Duration::from_millis(4_500)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        server.abort();

        let posts = posts.load(Ordering::SeqCst);
        assert_eq!(posts, 3, "{posts} publishes in 4.5 s");
        assert_eq!(*deleted.lock().unwrap(), ["res-0", "res-1", "res-2"]);
    }

    /// An endpoint that closes the session it accepted (DTLS close_notify)
    /// is published to again, after the backoff's floor. str0m goes inert
    /// on the close, and the output used to wait on the dead session for
    /// good: nothing was published again until the flow restarted.
    #[tokio::test]
    async fn an_endpoint_that_closes_the_session_is_published_to_again() {
        let (addr, posts, _deleted, server) = webrtc_endpoint(Endpoint::ClosesOnConnect).await;
        let (_source, cancel, mut raised, task) = whip_output(addr);

        // Connects at once, is closed at once, noticed within the second
        // (no media: the idle drive), published again a second later.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        while posts.load(Ordering::SeqCst) < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        server.abort();

        let posts = posts.load(Ordering::SeqCst);
        assert!(posts >= 2, "published {posts} time(s) in 6 s");
        let mut established = 0;
        let mut lost = 0;
        while let Ok(ev) = raised.try_recv() {
            established += usize::from(ev.message == "WHIP session established");
            lost += usize::from(ev.message == "WHIP client disconnected");
        }
        assert!(
            established >= 1 && lost >= 1,
            "{established} established, {lost} lost"
        );
    }

    /// An endpoint that accepts each publish and closes it as it connects is
    /// published to again after a backoff that doubles, as for any other
    /// failed attempt: the backoff starts over only after a session that
    /// lasted (`BACKOFF_RESET_AFTER`). It was reset on every connect, so such
    /// an endpoint was published to about once a second for good, two Info
    /// events each time.
    #[tokio::test]
    async fn an_endpoint_that_closes_each_session_is_backed_off_from() {
        let (addr, posts, _deleted, server) = webrtc_endpoint(Endpoint::ClosesOnConnect).await;
        let (_source, cancel, mut raised, task) = whip_output(addr);

        // When each publish was made.
        let start = tokio::time::Instant::now();
        let mut at = Vec::new();
        while at.len() < 4 && start.elapsed() < std::time::Duration::from_secs(20) {
            let n = posts.load(Ordering::SeqCst);
            while at.len() < n {
                at.push(start.elapsed());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        server.abort();

        assert_eq!(at.len(), 4, "published at {at:?} in 20 s");
        let gaps: Vec<_> = at.windows(2).map(|w| w[1] - w[0]).collect();
        // Waits of 1, 2 and 4 s (each plus the connect and the close's
        // noticing); a backoff reset on connect waited 1 s every time.
        assert!(gaps[1] >= std::time::Duration::from_millis(1_900), "gaps {gaps:?}");
        assert!(gaps[2] >= std::time::Duration::from_millis(3_900), "gaps {gaps:?}");
        let mut lost = 0;
        while let Ok(ev) = raised.try_recv() {
            lost += usize::from(ev.message == "WHIP client disconnected");
        }
        assert!(lost >= 3, "{lost} sessions lost: not the connected path");
    }
}
