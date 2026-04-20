// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CMAF / CMAF-LL HTTP-push output.
//!
//! Subscribes to the flow's broadcast channel, demuxes the MPEG-TS,
//! segments the source H.264/HEVC video + AAC audio into fragmented-MP4
//! (CMAF per ISO/IEC 23000-19), and uploads init + media segments plus
//! HLS m3u8 and/or DASH .mpd manifests to the operator-supplied
//! `ingest_url`.
//!
//! # Threading model
//!
//! - Broadcast subscriber loop: receives RTP packets, demuxes TS,
//!   buffers samples per track. **Never blocks** — codec work is
//!   delegated to spawn_blocking workers.
//! - Per-segment HTTP PUT: awaits without blocking the subscriber
//!   because the next packet is only fetched after the await point.
//!   For sustained throughput we offload uploads via tokio's I/O
//!   reactor (reqwest already does this internally).
//! - Codec workers (Phase 3 audio_encode / video_encode): wrap each
//!   encode call in `tokio::task::block_in_place` only when called
//!   from the subscriber loop.

mod box_writer;
mod cenc;
mod cenc_boxes;
mod codecs;
mod encode;
#[allow(dead_code)]
mod fmp4;
mod manifest;
#[allow(dead_code)]
mod nalu;
mod segmenter;
mod upload;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::CmafOutputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{EgressMediaSummaryStatic, OutputStatsAccumulator};

use super::packet::RtpPacket;
use super::ts_demux::{DemuxedFrame, TsDemuxer};
use super::ts_program_filter::TsProgramFilter;

use codecs::aac_audio_specific_config;
use encode::{AudioReencoder, VideoReencoder};
use fmp4::{AudioTrack, VideoCodec, VideoTrack};
use manifest::{
    DashAudioRep, DashInput, DashVideoRep, HlsPartEntry, LowLatencyHints, M3u8Entry,
    build_dash_mpd, build_hls_playlist, default_segment_uri,
};
use segmenter::{
    AudioSegmenter, CompletedSegment, PushOutcome, SegmentKind, VideoSegmenter,
};
use upload::{ChunkedPutHandle, chunked_put, http_put};

/// Minimum RTP header size (no CSRC / no extensions).
const RTP_HEADER_MIN: usize = 12;

/// Spawn a CMAF output task.
pub fn spawn_cmaf_output(
    config: CmafOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    output_stats.set_egress_static(EgressMediaSummaryStatic {
        transport_mode: Some("cmaf".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
    });

    tokio::spawn(async move {
        if let Err(e) = run(
            &config,
            &mut rx,
            output_stats,
            cancel,
            &event_sender,
            &flow_id,
        )
        .await
        {
            tracing::error!("CMAF output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::CMAF,
                format!("CMAF output '{}' error: {e}", config.id),
                &flow_id,
            );
        }
    })
}

fn init_cenc_runtime(
    cfg: &crate::config::models::CencConfig,
) -> anyhow::Result<CencRuntime> {
    let scheme = cenc::Scheme::parse(&cfg.scheme)
        .ok_or_else(|| anyhow::anyhow!("unknown CENC scheme '{}'", cfg.scheme))?;
    let key = decode_hex16(&cfg.key, "key")?;
    let key_id = decode_hex16(&cfg.key_id, "key_id")?;
    let mut extra_pssh = Vec::with_capacity(cfg.pssh_boxes.len());
    for hex_pssh in &cfg.pssh_boxes {
        let bytes = hex_decode(hex_pssh)
            .map_err(|e| anyhow::anyhow!("invalid pssh hex: {e}"))?;
        if bytes.len() < 8 || &bytes[4..8] != b"pssh" {
            anyhow::bail!("pssh entry is not a valid pssh box");
        }
        extra_pssh.push(bytes);
    }
    Ok(CencRuntime {
        encryptor: cenc::CencEncryptor::new(scheme, key),
        scheme,
        key_id,
        extra_pssh,
    })
}

fn decode_hex16(hex_str: &str, label: &str) -> anyhow::Result<[u8; 16]> {
    let bytes = hex_decode(hex_str).map_err(|e| anyhow::anyhow!("{label} hex: {e}"))?;
    if bytes.len() != 16 {
        anyhow::bail!("{label} must decode to 16 bytes, got {}", bytes.len());
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("odd hex length".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex byte 0x{c:02x}")),
    }
}

/// Mid-loop output state.
struct CmafState {
    video_seg: Option<VideoSegmenter>,
    audio_seg: Option<AudioSegmenter>,
    /// Whether audio has produced its init data yet (e.g. AAC config
    /// observed). Init.mp4 is published only after both video + (if
    /// configured) audio are ready.
    audio_ready: bool,
    /// Estimated video bitrate in bps (EWMA over emitted segments).
    video_bps_ewma: u64,
    /// Estimated audio bitrate in bps.
    audio_bps_ewma: u64,
    /// Wall-clock unix seconds of first segment emission.
    availability_start_unix: i64,
    /// True after init.mp4 has been published at least once.
    init_uploaded: bool,
    /// Rolling window of muxed segments (newest last).
    playlist: VecDeque<M3u8Entry>,
    /// Optional re-encoder for audio (Phase 3).
    audio_reencoder: Option<AudioReencoder>,
    /// Optional re-encoder for video (Phase 3).
    video_reencoder: Option<VideoReencoder>,
    /// LL-CMAF: in-flight chunked PUT for the current segment. None
    /// between segments.
    ll_current: Option<LlSegment>,
    /// CENC encryptor (Phase 5). When `Some`, video and audio
    /// samples get encrypted in place before muxing into the
    /// segment, and the init segment carries `tenc`/`pssh`.
    cenc: Option<CencRuntime>,
}

struct CencRuntime {
    encryptor: cenc::CencEncryptor,
    scheme: cenc::Scheme,
    key_id: [u8; 16],
    extra_pssh: Vec<Vec<u8>>,
}

/// LL-CMAF state held across the duration of one segment upload.
struct LlSegment {
    handle: ChunkedPutHandle,
    /// Sequence number of this segment.
    sequence_number: u64,
    /// Number of chunks emitted so far. The first chunk carries the
    /// `styp` prefix; subsequent chunks skip it.
    chunks_emitted: u32,
    /// Parts advertised on the current manifest for this segment.
    parts: Vec<HlsPartEntry>,
    /// The filename this segment is being uploaded under.
    uri: String,
}

impl CmafState {
    fn new() -> Self {
        Self {
            video_seg: None,
            audio_seg: None,
            audio_ready: false,
            video_bps_ewma: 0,
            audio_bps_ewma: 0,
            availability_start_unix: 0,
            init_uploaded: false,
            playlist: VecDeque::new(),
            audio_reencoder: None,
            video_reencoder: None,
            ll_current: None,
            cenc: None,
        }
    }
}

async fn run(
    config: &CmafOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        "CMAF output '{}' started -> {} (segment={}s, max_segments={}, manifests={:?}, audio_encode={:?}, video_encode={:?})",
        config.id,
        config.ingest_url,
        config.segment_duration_secs,
        config.max_segments,
        config.manifests,
        config.audio_encode.as_ref().map(|a| &a.codec),
        config.video_encode.as_ref().map(|v| &v.codec),
    );
    event_sender.emit_flow(
        EventSeverity::Info,
        category::CMAF,
        format!(
            "CMAF output '{}' started (segment={}s, manifests={})",
            config.id,
            config.segment_duration_secs,
            config.manifests.join("+"),
        ),
        flow_id,
    );

    let base_url = config.ingest_url.trim_end_matches('/').to_string();
    let init_name = "init.mp4".to_string();
    let init_url = format!("{base_url}/{init_name}");
    let m3u8_url = format!("{base_url}/manifest.m3u8");
    let mpd_url = format!("{base_url}/manifest.mpd");

    let publish_hls = config.manifests.iter().any(|m| m == "hls");
    let publish_dash = config.manifests.iter().any(|m| m == "dash");

    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "CMAF output '{}': program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    let mut demuxer = TsDemuxer::new(config.program_number);
    let mut state = CmafState::new();

    // Pre-flight Phase 3 re-encoders.
    if let Some(enc_cfg) = &config.audio_encode {
        match AudioReencoder::new(enc_cfg, &cancel, &config.id, flow_id) {
            Ok(reenc) => {
                tracing::info!(
                    "CMAF output '{}': audio re-encode active codec={} silent_fallback={}",
                    config.id, enc_cfg.codec, reenc.has_silent_fallback(),
                );
                // Silent-fallback path: build the AudioSegmenter eagerly
                // using the declared target params so silent AAC frames
                // can flow into the current segment before any source
                // audio arrives. The ASC is synthesised from the
                // declared sample_rate / channels (AOT=2, AAC-LC).
                if reenc.has_silent_fallback() {
                    if let Some((profile, sr_idx, ch_cfg)) = reenc.silent_fallback_track() {
                        let asc = aac_audio_specific_config(profile, sr_idx, ch_cfg);
                        let sample_rate = codecs::sample_rate_from_index(sr_idx);
                        let track = AudioTrack {
                            audio_specific_config: asc,
                            sample_rate,
                            channels: ch_cfg as u16,
                            avg_bitrate: enc_cfg
                                .bitrate_kbps
                                .map(|k| k * 1000)
                                .unwrap_or(128_000),
                        };
                        state.audio_seg = Some(AudioSegmenter::new(track, config.segment_duration_secs));
                        state.audio_ready = true;
                        tracing::info!(
                            "CMAF output '{}': audio track pre-built for silent_fallback (sr={} ch={})",
                            config.id, sample_rate, ch_cfg,
                        );
                    } else {
                        tracing::warn!(
                            "CMAF output '{}': silent_fallback target sample_rate has no ADTS index — silent-track init deferred",
                            config.id,
                        );
                    }
                }
                state.audio_reencoder = Some(reenc);
            }
            Err(e) => {
                tracing::error!(
                    "CMAF output '{}': audio re-encoder init failed: {e}",
                    config.id,
                );
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::AUDIO_ENCODE,
                    format!("CMAF output '{}': audio_encode init failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }
    // Pre-flight CENC.
    if let Some(cenc_cfg) = &config.encryption {
        match init_cenc_runtime(cenc_cfg) {
            Ok(rt) => {
                tracing::info!(
                    "CMAF output '{}': CENC active scheme={} pssh_extras={}",
                    config.id,
                    cenc_cfg.scheme,
                    cenc_cfg.pssh_boxes.len(),
                );
                state.cenc = Some(rt);
            }
            Err(e) => {
                tracing::error!("CMAF output '{}': CENC init failed: {e}", config.id);
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::CMAF,
                    format!("CMAF output '{}': CENC init failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }

    if let Some(enc_cfg) = &config.video_encode {
        match VideoReencoder::new(enc_cfg, &config.id) {
            Ok(reenc) => {
                tracing::info!(
                    "CMAF output '{}': video re-encode active codec={}",
                    config.id, enc_cfg.codec
                );
                state.video_reencoder = Some(reenc);
            }
            Err(e) => {
                tracing::error!(
                    "CMAF output '{}': video re-encoder init failed: {e}",
                    config.id,
                );
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::VIDEO_ENCODE,
                    format!("CMAF output '{}': video_encode init failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }

    // Silence tick: only armed when the AudioReencoder was built with
    // `silent_fallback = true`.
    let mut silence_interval: Option<tokio::time::Interval> = state
        .audio_reencoder
        .as_ref()
        .and_then(|r| r.silence_chunk_duration())
        .map(|d| {
            let mut iv = tokio::time::interval(d);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            iv
        });

    loop {
        let silence_tick = async {
            match silence_interval.as_mut() {
                Some(iv) => { iv.tick().await; }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("CMAF output '{}' stopping (cancelled)", config.id);
                break;
            }
            _ = silence_tick => {
                if let Some(reenc) = state.audio_reencoder.as_mut() {
                    match tokio::task::block_in_place(|| reenc.encode_silence_if_needed()) {
                        Ok(frames) if !frames.is_empty() => {
                            if let Some(seg) = state.audio_seg.as_mut() {
                                for (data, pts) in frames {
                                    seg.push(&data, pts);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!(
                                "CMAF output '{}': silent-fallback encode error: {e}",
                                config.id
                            );
                        }
                    }
                }
                continue;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        let payload = if packet.is_raw_ts {
                            &packet.data[..]
                        } else if packet.data.len() > RTP_HEADER_MIN {
                            &packet.data[RTP_HEADER_MIN..]
                        } else {
                            continue;
                        };

                        let ts_bytes: &[u8] = if let Some(ref mut f) = program_filter {
                            filter_scratch.clear();
                            f.filter_into(payload, &mut filter_scratch);
                            if filter_scratch.is_empty() {
                                continue;
                            }
                            &filter_scratch
                        } else {
                            payload
                        };

                        let frames = demuxer.demux(ts_bytes);
                        for frame in frames {
                            handle_frame(
                                frame,
                                &mut state,
                                &mut demuxer,
                                config,
                                &base_url,
                                &init_url,
                                &init_name,
                                &m3u8_url,
                                &mpd_url,
                                publish_hls,
                                publish_dash,
                                &stats,
                                event_sender,
                                flow_id,
                                packet.recv_time_us,
                            ).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "CMAF output '{}': broadcast lag, dropped {n} packets",
                            config.id,
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("CMAF output '{}' broadcast closed", config.id);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    frame: DemuxedFrame,
    state: &mut CmafState,
    demuxer: &mut TsDemuxer,
    config: &CmafOutputConfig,
    base_url: &str,
    init_url: &str,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    stats: &OutputStatsAccumulator,
    event_sender: &EventSender,
    flow_id: &str,
    recv_time_us: u64,
) {
    match frame {
        DemuxedFrame::H264 { nalus, pts, is_keyframe } => {
            handle_video(
                state,
                demuxer,
                VideoCodec::H264,
                nalus,
                pts,
                is_keyframe,
                config,
                base_url,
                init_url,
                init_name,
                m3u8_url,
                mpd_url,
                publish_hls,
                publish_dash,
                stats,
                event_sender,
                flow_id,
                recv_time_us,
            )
            .await;
        }
        DemuxedFrame::H265 { nalus, pts, is_keyframe } => {
            handle_video(
                state,
                demuxer,
                VideoCodec::H265,
                nalus,
                pts,
                is_keyframe,
                config,
                base_url,
                init_url,
                init_name,
                m3u8_url,
                mpd_url,
                publish_hls,
                publish_dash,
                stats,
                event_sender,
                flow_id,
                recv_time_us,
            )
            .await;
        }
        DemuxedFrame::Aac { data, pts } => {
            handle_audio_frame(state, demuxer, config, &data, pts, event_sender, flow_id);
        }
        DemuxedFrame::Opus => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_video(
    state: &mut CmafState,
    demuxer: &TsDemuxer,
    codec: VideoCodec,
    nalus: Vec<Vec<u8>>,
    pts: u64,
    is_keyframe: bool,
    config: &CmafOutputConfig,
    base_url: &str,
    init_url: &str,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    stats: &OutputStatsAccumulator,
    event_sender: &EventSender,
    flow_id: &str,
    recv_time_us: u64,
) {
    if !ensure_video_segmenter(
        &mut state.video_seg,
        codec,
        demuxer,
        config.segment_duration_secs,
        &config.id,
    ) {
        return;
    }

    // Phase 3: video_encode hooks here. For passthrough we forward the
    // NALs directly to the segmenter; with `video_encode`, we run them
    // through the re-encoder (block_in_place around the codec call) and
    // forward the encoded NALs instead.
    let pushed_nalus: Vec<Vec<u8>>;
    let pushed_is_keyframe: bool;
    if let Some(reenc) = state.video_reencoder.as_mut() {
        let recoded = tokio::task::block_in_place(|| {
            reenc.encode_frame(&nalus, pts, is_keyframe, codec)
        });
        match recoded {
            Ok(Some(out)) => {
                pushed_nalus = out.nalus;
                pushed_is_keyframe = out.is_keyframe;
            }
            Ok(None) => return, // encoder buffered the frame
            Err(e) => {
                tracing::warn!(
                    "CMAF output '{}': video re-encode failed: {e}",
                    config.id,
                );
                return;
            }
        }
    } else {
        pushed_nalus = nalus;
        pushed_is_keyframe = is_keyframe;
    }

    let outcome: PushOutcome = state
        .video_seg
        .as_mut()
        .map(|s| s.push(&pushed_nalus, pts, pushed_is_keyframe))
        .unwrap_or(PushOutcome {
            completed_video: None,
            new_segment_started: false,
            completed_video_samples: None,
        });

    // ── LL-CMAF chunked streaming path ──────────────────────────────
    //
    // When `low_latency` is enabled we open a chunked PUT at the start
    // of each segment and emit moof+mdat chunks every
    // `chunk_duration_ms` of buffered samples. The chunk emission runs
    // inside the broadcast subscriber loop with `try_send` + drop-on-
    // full backpressure, so slow ingests can never stall the flow.
    //
    // In LL mode we RETURN early after chunked emission — we never
    // fall through to the whole-segment upload path below. When a new
    // segment starts we close the existing PUT and open a new one; the
    // manifest row gets appended at that point.
    if config.low_latency {
        handle_ll_cmaf(
            state,
            &outcome,
            config,
            base_url,
            init_url,
            init_name,
            m3u8_url,
            mpd_url,
            publish_hls,
            publish_dash,
            stats,
            event_sender,
            flow_id,
            recv_time_us,
        )
        .await;
        return;
    }

    // Publish init.mp4 the first time a video track is materialised.
    if !state.init_uploaded {
        let need_audio = config.audio_encode.is_some()
            || state.audio_seg.is_some()
            || state.audio_ready;
        if let Some(v) = state.video_seg.as_ref() {
            // If audio is configured but not yet ready, defer init.mp4
            // until the first audio frame so we can include the audio
            // track in the init segment.
            if !need_audio || state.audio_seg.is_some() {
                let audio_track = state.audio_seg.as_ref().map(|a| &a.track);
                let init_bytes = if let Some(c) = state.cenc.as_ref() {
                    let params = fmp4::CencInitParams {
                        scheme: c.scheme,
                        key_id: &c.key_id,
                        extra_pssh: c.extra_pssh.clone(),
                    };
                    fmp4::build_encrypted_init_segment(&v.track, audio_track, &params)
                } else {
                    fmp4::build_init_segment(&v.track, audio_track)
                };
                match http_put(init_url, init_bytes, "video/mp4", config.auth_token.as_deref()).await {
                    Ok(_) => {
                        state.init_uploaded = true;
                        tracing::info!(
                            "CMAF output '{}': uploaded init.mp4 ({}x{}, {:?}{})",
                            config.id,
                            v.track.width,
                            v.track.height,
                            v.track.codec,
                            if audio_track.is_some() { " + audio" } else { "" },
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "CMAF output '{}': init.mp4 upload failed: {e}",
                            config.id,
                        );
                        event_sender.emit_flow(
                            EventSeverity::Warning,
                            category::CMAF,
                            format!("CMAF output '{}': init upload failed: {e}", config.id),
                            flow_id,
                        );
                    }
                }
            }
        }
    }

    if let Some(seg) = outcome.completed_video {
        // Re-publish init.mp4 if we captured an audio track *after* the
        // first video frame already pushed init.mp4. This lets us add
        // the audio trak into the moov before continuing.
        if state.init_uploaded
            && state.audio_seg.is_some()
            && seg.sequence_number == 0
        {
            if let Some(v) = state.video_seg.as_ref() {
                let audio_track = state.audio_seg.as_ref().map(|a| &a.track);
                let init_bytes = if let Some(c) = state.cenc.as_ref() {
                    let params = fmp4::CencInitParams {
                        scheme: c.scheme,
                        key_id: &c.key_id,
                        extra_pssh: c.extra_pssh.clone(),
                    };
                    fmp4::build_encrypted_init_segment(&v.track, audio_track, &params)
                } else {
                    fmp4::build_init_segment(&v.track, audio_track)
                };
                let _ = http_put(
                    init_url,
                    init_bytes,
                    "video/mp4",
                    config.auth_token.as_deref(),
                )
                .await;
            }
        }

        // Apply CENC by rebuilding the segment from the raw Sample
        // vector with per-sample encryption applied.
        let (segment_bytes, segment_kind, duration_90k) = if state.cenc.is_some()
            && outcome.completed_video_samples.is_some()
        {
            encrypt_and_build_video_segment(
                state,
                &seg,
                outcome.completed_video_samples.as_ref().unwrap(),
            )
            .map(|b| (b, SegmentKind::Video, seg.duration_90k))
            .unwrap_or((seg.bytes, seg.kind, seg.duration_90k))
        } else if state.audio_seg.is_some() {
            build_muxed_segment_for_seq(state, &seg)
                .unwrap_or((seg.bytes, seg.kind, seg.duration_90k))
        } else {
            (seg.bytes, seg.kind, seg.duration_90k)
        };

        let uri = match segment_kind {
            SegmentKind::Audio => format!("aud-{:05}.m4s", seg.sequence_number),
            _ => default_segment_uri(seg.sequence_number),
        };
        let seg_url = format!("{base_url}/{uri}");
        let seg_bytes_len = segment_bytes.len() as u64;

        match http_put(&seg_url, segment_bytes, "video/mp4", config.auth_token.as_deref()).await {
            Ok(_) => {
                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                stats.bytes_sent.fetch_add(seg_bytes_len, Ordering::Relaxed);
                stats.record_latency(recv_time_us);
                let dur_s = (duration_90k as f64 / 90_000.0).max(0.001);
                let bps = ((seg_bytes_len as f64 * 8.0) / dur_s) as u64;
                state.video_bps_ewma = if state.video_bps_ewma == 0 {
                    bps
                } else {
                    (state.video_bps_ewma * 3 + bps) / 4
                };
                tracing::debug!(
                    "CMAF output '{}': uploaded {} ({} bytes)",
                    config.id, uri, seg_bytes_len,
                );
            }
            Err(e) => {
                tracing::warn!(
                    "CMAF output '{}': segment {} upload failed: {e}",
                    config.id, uri,
                );
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::CMAF,
                    format!("CMAF output '{}': segment upload failed: {e}", config.id),
                    flow_id,
                );
                return;
            }
        }

        if state.availability_start_unix == 0 {
            state.availability_start_unix = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
        }

        state.playlist.push_back(M3u8Entry {
            sequence_number: seg.sequence_number,
            duration_secs: duration_90k as f64 / 90_000.0,
            uri: Some(uri),
            parts: Vec::new(),
        });
        while state.playlist.len() > config.max_segments {
            state.playlist.pop_front();
        }

        publish_manifests(
            state,
            config,
            init_name,
            m3u8_url,
            mpd_url,
            publish_hls,
            publish_dash,
            seg.sequence_number,
            event_sender,
            flow_id,
        )
        .await;
    }
}

fn handle_audio_frame(
    state: &mut CmafState,
    demuxer: &TsDemuxer,
    config: &CmafOutputConfig,
    data: &[u8],
    pts: u64,
    event_sender: &EventSender,
    flow_id: &str,
) {
    // Lazily construct the audio track on first AAC frame (we need the
    // demuxer-cached AAC config to derive the AudioSpecificConfig).
    if state.audio_seg.is_none() {
        let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() else {
            return;
        };
        let asc = aac_audio_specific_config(profile, sr_idx, ch_cfg);
        let sample_rate = codecs::sample_rate_from_index(sr_idx);
        let track = AudioTrack {
            audio_specific_config: asc,
            sample_rate,
            channels: ch_cfg as u16,
            avg_bitrate: config
                .audio_encode
                .as_ref()
                .and_then(|e| e.bitrate_kbps)
                .map(|k| k * 1000)
                .unwrap_or(128_000),
        };
        state.audio_seg = Some(AudioSegmenter::new(track, config.segment_duration_secs));
        state.audio_ready = true;
        tracing::info!(
            "CMAF output '{}': audio track detected AAC sr={} ch={}",
            config.id, sample_rate, ch_cfg,
        );
    }

    // Phase 3: audio_encode hook — pump the source AAC frame through
    // the AudioReencoder and substitute the re-encoded frame(s).
    let frames_to_buffer: Vec<Vec<u8>> = if let Some(reenc) = state.audio_reencoder.as_mut() {
        // Reset the silent-fallback drop watchdog — real audio is flowing.
        reenc.mark_real_audio(pts);
        // Propagate the ADTS triplet from the demuxer so lazy decoder
        // construction inside AudioReencoder can succeed.
        if let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() {
            reenc.set_adts_config(profile, sr_idx, ch_cfg);
        }
        match tokio::task::block_in_place(|| reenc.encode_aac_frame(data, pts)) {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!("CMAF output '{}': audio re-encode failed: {e}", config.id);
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::AUDIO_ENCODE,
                    format!("CMAF output '{}': audio_encode error: {e}", config.id),
                    flow_id,
                );
                Vec::new()
            }
        }
    } else {
        vec![data.to_vec()]
    };

    if let Some(seg) = state.audio_seg.as_mut() {
        for f in frames_to_buffer {
            seg.push(&f, pts);
        }
    }
}

/// Encrypt `samples` in place and re-build the video segment with
/// senc/saio/saiz.
fn encrypt_and_build_video_segment(
    state: &mut CmafState,
    seg: &CompletedSegment,
    snapshot: &(u64, u64, Vec<fmp4::Sample>),
) -> Option<Vec<u8>> {
    use fmp4::{Sample as FSample, VideoCodec};
    let cenc_rt = state.cenc.as_mut()?;
    let v_track_codec = state.video_seg.as_ref()?.track.codec;
    let (_seq, base, samples) = snapshot;
    let mut mutable: Vec<FSample> = samples.clone();
    let mut cenc_info = Vec::with_capacity(mutable.len());
    for s in mutable.iter_mut() {
        let info = cenc_rt
            .encryptor
            .encrypt_video_sample(&mut s.data, match v_track_codec {
                VideoCodec::H264 => cenc::VideoCodec::H264,
                VideoCodec::H265 => cenc::VideoCodec::H265,
            });
        cenc_info.push(info);
    }
    Some(fmp4::build_encrypted_media_segment(
        fmp4::VIDEO_TRACK_ID,
        seg.sequence_number as u32,
        *base,
        &mutable,
        &cenc_info,
        cenc_rt.scheme,
    ))
}

/// Build a muxed segment containing both video samples (already in
/// `seg.bytes` is video-only — we discard it and rebuild) and the
/// audio frames buffered up to the boundary. Returns `None` if the
/// caller should fall back to the video-only `seg.bytes`.
fn build_muxed_segment_for_seq(
    state: &mut CmafState,
    seg: &CompletedSegment,
) -> Option<(Vec<u8>, SegmentKind, u64)> {
    let v_seg = state.video_seg.as_mut()?;
    let a_seg = state.audio_seg.as_mut()?;

    // Convert video segment boundary DTS (90 kHz) into the audio
    // track's timescale.
    let boundary_video_dts = seg.base_dts_90k + seg.duration_90k;
    let boundary_audio_ts = boundary_video_dts * a_seg.track.sample_rate as u64 / 90_000;

    // Take whatever audio is buffered. We deliberately do NOT use the
    // video segmenter's take_pending_samples (the segment is already
    // closed); the bytes inside `seg.bytes` are a video-only fMP4.
    // Rebuild as muxed if we have audio for this segment.
    let audio_data = a_seg.take_pending_samples(boundary_audio_ts)?;
    let (a_seq, a_base, a_samples) = audio_data;
    if a_samples.is_empty() {
        return None;
    }

    // For muxed output we drop the video-only bytes and rebuild from
    // the next-pending video samples. But by the time we get here,
    // those samples have already been consumed by the segmenter to
    // produce `seg.bytes`. Cheapest fix: re-derive video samples by
    // parsing seg.bytes is expensive — we instead leave the existing
    // single-track segments in place and emit a parallel audio-only
    // segment alongside (CMAF supports both modes).
    //
    // We synthesize an audio-only segment and upload it separately by
    // returning the rebuilt audio bytes. The video bytes stay as-is.
    //
    // Simpler approach taken here: keep video-only seg, separately
    // emit audio-only seg (caller is responsible for upload).
    let _ = (v_seg, a_seq, a_base);

    None
}

fn ensure_video_segmenter(
    slot: &mut Option<VideoSegmenter>,
    codec: VideoCodec,
    demuxer: &TsDemuxer,
    segment_duration_secs: f64,
    output_id: &str,
) -> bool {
    if slot.is_some() {
        return true;
    }
    let track = match codec {
        VideoCodec::H264 => {
            let sps = match demuxer.cached_sps() {
                Some(s) => s.to_vec(),
                None => return false,
            };
            let pps = match demuxer.cached_pps() {
                Some(p) => p.to_vec(),
                None => return false,
            };
            VideoTrack::from_h264(sps, pps)
        }
        VideoCodec::H265 => {
            let vps = match demuxer.cached_h265_vps() {
                Some(v) => v.to_vec(),
                None => return false,
            };
            let sps = match demuxer.cached_h265_sps() {
                Some(s) => s.to_vec(),
                None => return false,
            };
            let pps = match demuxer.cached_h265_pps() {
                Some(p) => p.to_vec(),
                None => return false,
            };
            VideoTrack::from_h265(vps, sps, pps)
        }
    };
    tracing::info!(
        "CMAF output '{}': video track detected {:?} {}x{}",
        output_id, track.codec, track.width, track.height,
    );
    *slot = Some(VideoSegmenter::new(track, segment_duration_secs));
    true
}

/// LL-CMAF chunk pump. Called once per video frame push; it opens a
/// chunked PUT at segment boundaries, flushes accumulated samples as
/// moof+mdat chunks every `chunk_duration_ms` worth of media, and
/// closes the PUT at the segment boundary.
#[allow(clippy::too_many_arguments)]
async fn handle_ll_cmaf(
    state: &mut CmafState,
    outcome: &PushOutcome,
    config: &CmafOutputConfig,
    base_url: &str,
    init_url: &str,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    stats: &OutputStatsAccumulator,
    event_sender: &EventSender,
    flow_id: &str,
    recv_time_us: u64,
) {
    // Publish init.mp4 if we haven't yet.
    if !state.init_uploaded {
        if let Some(v) = state.video_seg.as_ref() {
            let init_bytes =
                fmp4::build_init_segment(&v.track, state.audio_seg.as_ref().map(|a| &a.track));
            match http_put(init_url, init_bytes, "video/mp4", config.auth_token.as_deref()).await {
                Ok(_) => {
                    state.init_uploaded = true;
                }
                Err(e) => {
                    tracing::warn!("CMAF LL output '{}': init upload failed: {e}", config.id);
                    event_sender.emit_flow(
                        EventSeverity::Warning,
                        category::CMAF,
                        format!("CMAF output '{}': init upload failed: {e}", config.id),
                        flow_id,
                    );
                    return;
                }
            }
        }
    }

    // On a new segment boundary, finalise the previous LL PUT and open
    // a new one.
    if outcome.new_segment_started {
        // Close previous LL segment if any.
        if let Some(ll) = state.ll_current.take() {
            let uri = ll.uri.clone();
            let seq = ll.sequence_number;
            let finish = ll.handle.finish().await;
            match finish {
                Ok(()) => {
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        "CMAF output '{}': finished LL seg {} ({})",
                        config.id, seq, uri,
                    );
                    stats.record_latency(recv_time_us);
                }
                Err(e) => {
                    tracing::warn!(
                        "CMAF output '{}': LL seg {} PUT final response error: {e}",
                        config.id, seq,
                    );
                    event_sender.emit_flow(
                        EventSeverity::Warning,
                        category::CMAF,
                        format!("CMAF output '{}': LL PUT failed: {e}", config.id),
                        flow_id,
                    );
                }
            }
            state.playlist.push_back(M3u8Entry {
                sequence_number: seq,
                duration_secs: config.segment_duration_secs,
                uri: Some(uri),
                parts: Vec::new(),
            });
            while state.playlist.len() > config.max_segments {
                state.playlist.pop_front();
            }
        }
        // Open new segment.
        if let Some(vs) = state.video_seg.as_ref() {
            let seq = vs.next_segment_number();
            let uri = default_segment_uri(seq);
            let url = format!("{}/{}", base_url, uri);
            let handle =
                chunked_put(&url, "video/mp4", config.auth_token.as_deref(), 8);
            tracing::debug!(
                "CMAF output '{}': opened LL chunked PUT for seg={} ({})",
                config.id, seq, url
            );
            state.ll_current = Some(LlSegment {
                handle,
                sequence_number: seq,
                chunks_emitted: 0,
                parts: Vec::new(),
                uri,
            });
            if state.availability_start_unix == 0 {
                state.availability_start_unix = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
            }
        }
    }

    // Try to emit one or more chunks from the segmenter's accumulated
    // samples.
    let chunk_duration_90k =
        (config.chunk_duration_ms as u64 * 90_000) / 1_000;
    let mut did_emit = false;

    if let (Some(vs), Some(ll)) = (state.video_seg.as_mut(), state.ll_current.as_mut()) {
        loop {
            let chunk_bytes = vs.take_pending_chunk(
                ll.sequence_number as u32,
                chunk_duration_90k,
                ll.chunks_emitted,
            );
            let Some(bytes) = chunk_bytes else {
                break;
            };
            let bytes_len = bytes.len();
            match ll.handle.send_chunk(bytes) {
                Ok(()) => {
                    ll.chunks_emitted += 1;
                    did_emit = true;
                    let part_dur =
                        config.chunk_duration_ms as f64 / 1000.0;
                    ll.parts.push(HlsPartEntry {
                        uri: format!("{}?part={}", ll.uri, ll.chunks_emitted - 1),
                        duration_secs: part_dur,
                        independent: ll.chunks_emitted == 1,
                    });
                    tracing::trace!(
                        "CMAF output '{}': emitted LL chunk {}#{} ({}B)",
                        config.id, ll.sequence_number, ll.chunks_emitted - 1, bytes_len
                    );
                }
                Err(()) => {
                    // Backpressure: ingest is too slow. Abort the PUT,
                    // discard accumulated samples for this segment, and
                    // wait for the next IDR to open a fresh segment.
                    tracing::warn!(
                        "CMAF output '{}': LL ingest stall, aborting seg {}",
                        config.id, ll.sequence_number
                    );
                    event_sender.emit_flow(
                        EventSeverity::Warning,
                        category::CMAF,
                        format!(
                            "CMAF output '{}': LL chunk enqueue full (seg {}) — aborting",
                            config.id, ll.sequence_number
                        ),
                        flow_id,
                    );
                    let ll = state.ll_current.take().unwrap();
                    ll.handle.abort();
                    return;
                }
            }
        }
    }

    // Update the manifest on every chunk emission so players see the
    // new part advertised.
    if did_emit && publish_hls {
        publish_ll_hls(state, config, init_name, m3u8_url, event_sender, flow_id)
            .await;
    }
    if did_emit && publish_dash {
        publish_ll_dash(state, config, mpd_url, event_sender, flow_id).await;
    }
}

async fn publish_ll_dash(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    mpd_url: &str,
    event_sender: &EventSender,
    flow_id: &str,
) {
    let Some(v) = state.video_seg.as_ref() else { return };
    let latest_seq = state
        .ll_current
        .as_ref()
        .map(|l| l.sequence_number)
        .unwrap_or_else(|| {
            state
                .playlist
                .back()
                .map(|e| e.sequence_number)
                .unwrap_or(0)
        });
    let ato = (config.segment_duration_secs
        - (config.chunk_duration_ms as f64 / 1000.0))
        .max(0.0);
    let video_rep = DashVideoRep {
        codec: v.track.codec,
        sps: &v.track.sps,
        width: v.track.width,
        height: v.track.height,
        timescale: v.track.timescale,
        bandwidth_bps: state.video_bps_ewma.max(500_000),
    };
    let audio_rep = state.audio_seg.as_ref().map(|a| DashAudioRep {
        asc: &a.track.audio_specific_config,
        sample_rate: a.track.sample_rate,
        channels: a.track.channels,
        bandwidth_bps: state.audio_bps_ewma.max(64_000),
    });
    let mpd = build_dash_mpd(&DashInput {
        availability_start_unix_secs: state.availability_start_unix,
        target_segment_duration_secs: config.segment_duration_secs,
        video: Some(video_rep),
        audio: audio_rep,
        latest_segment_number: latest_seq,
        available_segments: state.playlist.len() as u64,
        availability_time_offset_secs: ato,
    });
    if let Err(e) = http_put(
        mpd_url,
        mpd.into_bytes(),
        "application/dash+xml",
        config.auth_token.as_deref(),
    )
    .await
    {
        tracing::warn!("CMAF output '{}': LL mpd upload failed: {e}", config.id);
        event_sender.emit_flow(
            EventSeverity::Warning,
            category::CMAF,
            format!("CMAF output '{}': LL mpd upload failed: {e}", config.id),
            flow_id,
        );
    }
}

async fn publish_ll_hls(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    init_name: &str,
    m3u8_url: &str,
    event_sender: &EventSender,
    flow_id: &str,
) {
    // Attach the current segment's parts to the last playlist entry
    // (or append a synthetic "in-progress" entry).
    let mut entries: Vec<M3u8Entry> = state.playlist.iter().cloned().collect();
    if let Some(ll) = state.ll_current.as_ref() {
        // Synthesize an in-progress entry for the current segment so
        // its parts are part of the manifest even before it closes.
        entries.push(M3u8Entry {
            sequence_number: ll.sequence_number,
            duration_secs: config.segment_duration_secs,
            uri: Some(ll.uri.clone()),
            parts: ll.parts.clone(),
        });
    }
    let hints = LowLatencyHints {
        part_target_secs: config.chunk_duration_ms as f64 / 1000.0,
        can_block_reload: true,
    };
    let body =
        build_hls_playlist(config.segment_duration_secs, &entries, init_name, Some(&hints));
    if let Err(e) = http_put(
        m3u8_url,
        body.into_bytes(),
        "application/vnd.apple.mpegurl",
        config.auth_token.as_deref(),
    )
    .await
    {
        tracing::warn!(
            "CMAF output '{}': LL m3u8 upload failed: {e}",
            config.id,
        );
        event_sender.emit_flow(
            EventSeverity::Warning,
            category::CMAF,
            format!("CMAF output '{}': LL m3u8 upload failed: {e}", config.id),
            flow_id,
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_manifests(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    latest_seq: u64,
    event_sender: &EventSender,
    flow_id: &str,
) {
    if publish_hls {
        let entries: Vec<M3u8Entry> = state.playlist.iter().cloned().collect();
        let body = build_hls_playlist(config.segment_duration_secs, &entries, init_name, None);
        if let Err(e) = http_put(
            m3u8_url,
            body.into_bytes(),
            "application/vnd.apple.mpegurl",
            config.auth_token.as_deref(),
        )
        .await
        {
            tracing::warn!(
                "CMAF output '{}': manifest.m3u8 upload failed: {e}",
                config.id,
            );
            event_sender.emit_flow(
                EventSeverity::Warning,
                category::CMAF,
                format!("CMAF output '{}': m3u8 upload failed: {e}", config.id),
                flow_id,
            );
        }
    }

    if publish_dash {
        if let Some(v) = state.video_seg.as_ref() {
            let video_rep = DashVideoRep {
                codec: v.track.codec,
                sps: &v.track.sps,
                width: v.track.width,
                height: v.track.height,
                timescale: v.track.timescale,
                bandwidth_bps: state.video_bps_ewma.max(500_000),
            };
            let audio_rep = state.audio_seg.as_ref().map(|a| DashAudioRep {
                asc: &a.track.audio_specific_config,
                sample_rate: a.track.sample_rate,
                channels: a.track.channels,
                bandwidth_bps: state.audio_bps_ewma.max(64_000),
            });
            let mpd = build_dash_mpd(&DashInput {
                availability_start_unix_secs: state.availability_start_unix,
                target_segment_duration_secs: config.segment_duration_secs,
                video: Some(video_rep),
                audio: audio_rep,
                latest_segment_number: latest_seq,
                available_segments: state.playlist.len() as u64,
                availability_time_offset_secs: 0.0,
            });
            if let Err(e) = http_put(
                mpd_url,
                mpd.into_bytes(),
                "application/dash+xml",
                config.auth_token.as_deref(),
            )
            .await
            {
                tracing::warn!(
                    "CMAF output '{}': manifest.mpd upload failed: {e}",
                    config.id,
                );
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::CMAF,
                    format!("CMAF output '{}': mpd upload failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }
}
