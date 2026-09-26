// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::VecDeque;
#[cfg(not(feature = "media-codecs"))]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
#[cfg(not(feature = "media-codecs"))]
use std::time::Duration;

#[cfg(not(feature = "media-codecs"))]
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{AudioEncodeConfig, HlsOutputConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;

use super::audio_encode::AudioCodec;
#[cfg(not(feature = "media-codecs"))]
use super::audio_encode::check_ffmpeg_available;
use super::packet::RtpPacket;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

/// Maximum time we'll wait for ffmpeg to re-mux a single segment.
/// Segments are typically 2-6 seconds, so 30 s is a generous safety bound.
#[cfg(not(feature = "media-codecs"))]
const HLS_REMUX_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimum RTP header size (no CSRC or extensions).
const RTP_HEADER_MIN: usize = 12;

/// Spawn an HLS ingest output task.
///
/// Subscribes to the broadcast channel, strips RTP headers to recover raw
/// MPEG-TS payload, segments the stream by wall-clock duration, and uploads
/// each completed segment plus a rolling M3U8 playlist via HTTP PUT.
///
/// The upload uses a minimal async HTTP/1.1 client built on `TcpStream` to
/// avoid pulling in heavy HTTP client dependencies (e.g. reqwest/hyper).
///
/// On upload failure the segment is skipped with a warning log -- the output
/// never blocks the input or other outputs.
pub fn spawn_hls_output(
    config: HlsOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("hls".to_string()),
        video_passthrough: true,
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
        ..Default::default()
    };
    if let Some(ae) = config.audio_encode.as_ref() {
        egress_static = egress_static.with_audio_encode_target(ae);
    }
    output_stats.set_egress_static(egress_static);

    tokio::spawn(async move {
        if let Err(e) = hls_output_loop(&config, &mut rx, output_stats, cancel, &event_sender, &flow_id).await {
            tracing::error!("HLS output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::HLS,
                format!("HLS output '{}' error: {e}", config.id),
                &flow_id,
            );
        }
    })
}

/// Core loop for the HLS ingest output.
///
/// Accumulates raw TS bytes (RTP payload with header stripped) into a segment
/// buffer. When the wall-clock duration of packets in the current segment
/// exceeds `segment_duration_secs`, the segment is uploaded and a new one
/// starts.
async fn hls_output_loop(
    config: &HlsOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
) -> anyhow::Result<()> {
    tracing::info!(
        "HLS output '{}' started -> {} (segment={}s, max_segments={})",
        config.id,
        config.ingest_url,
        config.segment_duration_secs,
        config.max_segments,
    );

    let segment_duration_us = (config.segment_duration_secs * 1_000_000.0) as u64;

    // Resolve audio_encode at startup.
    let mut audio_encode_config: Option<ResolvedAudioEncode> = match &config.audio_encode {
        Some(enc) => match resolve_audio_encode(
            enc,
            config.transcode.clone(),
            &config.id,
            EncoderCtxParts { cancel: &cancel, stats: &stats, flow_id, events: Some(event_sender) },
        ) {
            Ok(resolved) => {
                tracing::info!(
                    "HLS output '{}': audio_encode active codec={} bitrate={:?}k sr={:?} ch={:?}",
                    config.id, enc.codec, enc.bitrate_kbps, enc.sample_rate, enc.channels
                );
                event_sender.emit_flow_with_details(
                    EventSeverity::Info,
                    crate::manager::events::category::AUDIO_ENCODE,
                    format!(
                        "HLS output '{}': audio encoder started (codec={})",
                        config.id, enc.codec
                    ),
                    flow_id,
                    serde_json::json!({
                        "output_id": config.id,
                        "codec": enc.codec,
                        "bitrate_kbps": enc.bitrate_kbps,
                        "sample_rate": enc.sample_rate,
                        "channels": enc.channels,
                    }),
                );
                Some(resolved)
            }
            Err(e) => {
                let msg = format!(
                    "HLS output '{}': audio_encode rejected: {e}",
                    config.id
                );
                tracing::error!("{msg}");
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    crate::manager::events::category::AUDIO_ENCODE,
                    msg,
                    flow_id,
                );
                return Ok(());
            }
        },
        None => None,
    };

    let mut segment_buf: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024); // 2 MB initial
    let mut segment_start_us: Option<u64> = None;
    let mut segment_seq: u64 = 0;

    // Optional MPTS → SPTS program filter. When set, every TS chunk is
    // filtered before it joins the segment buffer, so the resulting `.ts`
    // segments carry only the selected program. PAT/PMT state survives
    // across packets so version bumps are handled.
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "HLS output '{}': program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    let mut pid_remapper = config.pid_map.as_ref().and_then(|m| {
        let r = TsPidRemapper::new(m);
        if r.is_active() {
            tracing::info!(
                "HLS output '{}': pid_map active ({} entries)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut remap_scratch: Vec<u8> = Vec::new();

    // Rolling playlist: (sequence_number, duration_secs)
    let mut playlist_entries: VecDeque<(u64, f64)> = VecDeque::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("HLS output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Strip RTP header to get raw TS payload.
                        let payload = if packet.is_raw_ts {
                            &packet.data[..]
                        } else if packet.data.len() > RTP_HEADER_MIN {
                            &packet.data[RTP_HEADER_MIN..]
                        } else {
                            // Packet too small to contain TS data, skip.
                            continue;
                        };

                        // Initialise segment timing from the first packet.
                        let start = segment_start_us.get_or_insert(packet.recv_time_us);
                        let elapsed_us = packet.recv_time_us.saturating_sub(*start);

                        // Apply program filter if configured: feed only the
                        // selected program's TS bytes into the segment buffer.
                        // Skip the packet entirely when the filter eats it.
                        let filtered: &[u8] = if let Some(ref mut filter) = program_filter {
                            filter_scratch.clear();
                            filter.filter_into(payload, &mut filter_scratch);
                            if filter_scratch.is_empty() {
                                continue;
                            }
                            &filter_scratch
                        } else {
                            payload
                        };

                        if let Some(ref mut remapper) = pid_remapper {
                            remap_scratch.clear();
                            remapper.process(filtered, &mut remap_scratch);
                            segment_buf.extend_from_slice(&remap_scratch);
                        } else {
                            segment_buf.extend_from_slice(filtered);
                        }

                        // Check if we should cut a segment.
                        if elapsed_us >= segment_duration_us && !segment_buf.is_empty() {
                            let duration_secs = elapsed_us as f64 / 1_000_000.0;
                            let seq = segment_seq;
                            segment_seq += 1;

                            // Upload the segment (non-blocking: log and continue on failure).
                            let raw_segment = std::mem::replace(
                                &mut segment_buf,
                                Vec::with_capacity(2 * 1024 * 1024),
                            );

                            // Run the segment through the audio remuxer if
                            // audio_encode is configured. Copies the video
                            // stream and re-encodes the audio per the
                            // configured codec/bitrate. On failure we skip
                            // this segment — the next segment may succeed.
                            let segment_data = if let Some(ref mut enc) = audio_encode_config {
                                match remux_segment_audio(&raw_segment, enc).await {
                                    Ok(data) => data,
                                    Err(e) => {
                                        tracing::warn!(
                                            "HLS output '{}': segment {} audio remux failed: {e}; skipping",
                                            config.id, segment_seq
                                        );
                                        event_sender.emit_flow(
                                            EventSeverity::Warning,
                                            crate::manager::events::category::AUDIO_ENCODE,
                                            format!("HLS output '{}': segment {} remux failed: {e}", config.id, segment_seq),
                                            flow_id,
                                        );
                                        segment_start_us = None;
                                        continue;
                                    }
                                }
                            } else {
                                raw_segment
                            };
                            let segment_bytes = segment_data.len() as u64;

                            let segment_url = format!(
                                "{}/segment_{}.ts",
                                config.ingest_url.trim_end_matches('/'),
                                seq,
                            );

                            match http_put(&segment_url, &segment_data, "video/mp2t", config.auth_token.as_deref()).await {
                                Ok(_) => {
                                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                    stats.bytes_sent.fetch_add(segment_bytes, Ordering::Relaxed);
                                    // Use the segment start time as the latency base — this
                                    // captures both the segment accumulation time and the upload time.
                                    if let Some(seg_start) = segment_start_us {
                                        stats.record_latency(seg_start);
                                    }
                                    tracing::debug!(
                                        "HLS output '{}': uploaded segment_{}.ts ({} bytes, {:.2}s)",
                                        config.id,
                                        seq,
                                        segment_bytes,
                                        duration_secs,
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "HLS output '{}': failed to upload segment_{}.ts: {e}",
                                        config.id,
                                        seq,
                                    );
                                    event_sender.emit_flow(
                                        EventSeverity::Warning,
                                        category::HLS,
                                        format!("HLS output '{}': segment upload failed: {e}", config.id),
                                        flow_id,
                                    );
                                }
                            }

                            // Update rolling playlist.
                            playlist_entries.push_back((seq, duration_secs));
                            while playlist_entries.len() > config.max_segments {
                                playlist_entries.pop_front();
                            }

                            // Generate and upload the M3U8 playlist.
                            let playlist = generate_m3u8(
                                &playlist_entries,
                                config.segment_duration_secs,
                            );

                            let playlist_url = format!(
                                "{}/playlist.m3u8",
                                config.ingest_url.trim_end_matches('/'),
                            );

                            if let Err(e) = http_put(
                                &playlist_url,
                                playlist.as_bytes(),
                                "application/vnd.apple.mpegurl",
                                config.auth_token.as_deref(),
                            ).await {
                                tracing::warn!(
                                    "HLS output '{}': failed to upload playlist.m3u8: {e}",
                                    config.id,
                                );
                            }

                            // Reset for the next segment.
                            segment_start_us = None;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "HLS output '{}' lagged, dropped {n} packets",
                            config.id,
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("HLS output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Generate an HLS M3U8 playlist from the rolling segment list.
fn generate_m3u8(
    entries: &VecDeque<(u64, f64)>,
    target_duration: f64,
) -> String {
    let target_dur_int = target_duration.ceil() as u64;

    // Media sequence is the sequence number of the first entry in the playlist.
    let media_seq = entries.front().map(|(seq, _)| *seq).unwrap_or(0);

    let mut m3u8 = String::with_capacity(512);
    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:3\n");
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur_int}\n"));
    m3u8.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{media_seq}\n"));

    for (seq, duration) in entries {
        m3u8.push_str(&format!("#EXTINF:{duration:.3},\n"));
        m3u8.push_str(&format!("segment_{seq}.ts\n"));
    }

    m3u8
}

// ── Minimal async HTTP PUT client ──────────────────────────────────────────

/// Base network time budget for a single segment/playlist PUT. Without a bound
/// a destination that accepts the TCP connection but never responds (or
/// black-holes SYNs) would wedge the HLS output task for the OS TCP timeout
/// (~2 min) or indefinitely, silently taking the leg dark. The effective
/// budget scales with body size (see `http_put`) so a large high-bitrate
/// segment on a slow-but-working link isn't clipped.
const HLS_PUT_TIMEOUT_BASE: std::time::Duration = std::time::Duration::from_secs(30);

/// Perform an HTTP PUT request using a raw TCP stream.
///
/// This is a minimal implementation that avoids heavy HTTP client dependencies.
/// It does NOT support TLS — `https://` URLs are rejected at config validation
/// (they would connect in cleartext and leak the bearer token), so this path
/// only ever sees `http://`. No redirects or chunked transfer encoding. For
/// TLS, terminate at a local reverse proxy or switch to `reqwest`.
async fn http_put(
    url: &str,
    body: &[u8],
    content_type: &str,
    auth_token: Option<&str>,
) -> anyhow::Result<()> {
    // 30s base + ~1s per MiB of body, so a large segment (e.g. 10s @ 40 Mbps
    // ≈ 50 MiB) on a slow link gets a proportionate budget instead of being
    // clipped by a flat timeout — while a stalled/black-holed ingest is still
    // bounded (drop-and-continue) rather than wedging the leg for ~2 min.
    let timeout = HLS_PUT_TIMEOUT_BASE
        .saturating_add(std::time::Duration::from_secs((body.len() / (1024 * 1024)) as u64));
    tokio::time::timeout(timeout, http_put_inner(url, body, content_type, auth_token))
        .await
        .map_err(|_| anyhow::anyhow!("HLS PUT to {url} timed out after {timeout:?}"))?
}

async fn http_put_inner(
    url: &str,
    body: &[u8],
    content_type: &str,
    auth_token: Option<&str>,
) -> anyhow::Result<()> {
    let target = crate::util::url_parse::parse_http_target(url)?;

    // No TLS support here — refuse https:// BEFORE connecting or building the
    // request, so the bearer token is never transmitted in cleartext. This is a
    // runtime refusal (the config still loads) rather than a validation error,
    // so one misconfigured HLS output can't brick the whole edge.
    if target.scheme.eq_ignore_ascii_case("https") {
        anyhow::bail!(
            "HLS output to {url}: https:// is not supported (the HLS uploader has no TLS) — \
             refusing to send the auth_token in cleartext. Use http:// to a trusted network \
             or a TLS-terminating reverse proxy."
        );
    }

    let has_crlf = |s: &str| s.bytes().any(|b| b == b'\r' || b == b'\n');
    if has_crlf(&target.host) || has_crlf(&target.path_and_query) || has_crlf(content_type)
        || auth_token.is_some_and(has_crlf)
    {
        anyhow::bail!("HTTP PUT refused: header injection attempt in URL or headers");
    }

    let mut stream = TcpStream::connect(&target.connect_target).await
        .map_err(|e| anyhow::anyhow!("connect to {}: {e}", target.connect_target))?;

    let host_header = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else if (target.scheme == "http" && target.port == 80)
        || (target.scheme == "https" && target.port == 443)
    {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };

    let mut request = format!(
        "PUT {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len(),
        path = target.path_and_query,
        host = host_header,
    );

    if let Some(token) = auth_token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }

    request.push_str("\r\n");

    stream.write_all(request.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    // Read the response status line (we don't need the full body).
    let mut response_buf = vec![0u8; 1024];
    let n = stream.read(&mut response_buf).await?;
    let response_str = String::from_utf8_lossy(&response_buf[..n]);

    // Check for 2xx status
    if let Some(status_line) = response_str.lines().next() {
        // e.g. "HTTP/1.1 200 OK"
        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        if parts.len() >= 2
            && let Ok(status) = parts[1].parse::<u16>()
                && !(200..300).contains(&status) {
                    anyhow::bail!("HTTP PUT returned status {status}: {status_line}");
                }
    }

    Ok(())
}

// ════════════════════════════════════════════════════════════════════════
// Audio remux dispatch
// ════════════════════════════════════════════════════════════════════════

/// Resolved audio encode configuration ready for segment remuxing.
#[allow(dead_code)]
struct ResolvedAudioEncode {
    codec: AudioCodec,
    bitrate_kbps: u32,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    /// Optional planar PCM shuffle / resample block, applied between the
    /// AAC decoder and the target encoder. Only honoured on the in-process
    /// remux path (`media-codecs` feature); the subprocess fallback
    /// logs a warning and ignores it.
    transcode: Option<super::audio_transcode::TranscodeJson>,
    /// The re-encode chain the output's segments run through, one after the
    /// other (`HlsAudioChain`); taken out while a segment is on the
    /// blocking pool. Its stage fixes the rendition's (rate, channels) at the
    /// first frame, so the rendition keeps one format when its source
    /// changes format in-band (a playlist has no way to signal a change of
    /// channel count or rate between segments).
    #[cfg(feature = "media-codecs")]
    chain: Option<HlsAudioChain>,
    /// Pre-built ffmpeg args (only used for subprocess fallback).
    #[cfg(not(feature = "media-codecs"))]
    ffmpeg_args: Vec<String>,
}

/// Resolve and validate audio encode config at output startup.
#[allow(unused_variables)]
fn resolve_audio_encode(
    enc: &AudioEncodeConfig,
    transcode: Option<super::audio_transcode::TranscodeJson>,
    output_id: &str,
    #[cfg_attr(not(feature = "media-codecs"), allow(unused_variables))] ctx: EncoderCtxParts<'_>,
) -> Result<ResolvedAudioEncode, String> {
    let codec = AudioCodec::parse(&enc.codec)
        .ok_or_else(|| format!("unknown codec '{}'", enc.codec))?;

    // Opus on HLS-TS is not supported
    if codec == AudioCodec::Opus {
        return Err("Opus is not supported on HLS in this build".into());
    }

    let bitrate_kbps = enc.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    #[cfg(not(feature = "media-codecs"))]
    {
        // Subprocess fallback: need ffmpeg in PATH
        if !check_ffmpeg_available() {
            return Err(format!(
                "HLS output '{output_id}': audio_encode requires ffmpeg in PATH but it is not installed"
            ));
        }
        if transcode.is_some() {
            tracing::warn!(
                "HLS output '{output_id}': transcode block ignored in ffmpeg subprocess fallback — \
                 enable the `media-codecs` feature for in-process channel shuffle / SRC"
            );
        }
        let ffmpeg_args = build_remux_args(enc)?;
        Ok(ResolvedAudioEncode {
            codec,
            bitrate_kbps,
            sample_rate: enc.sample_rate,
            channels: enc.channels,
            transcode,
            ffmpeg_args,
        })
    }

    #[cfg(feature = "media-codecs")]
    {
        // Validate HE-AAC variants have fdk-aac available
        #[cfg(feature = "fdk-aac")]
        if matches!(codec, AudioCodec::HeAacV1 | AudioCodec::HeAacV2) {
            // OK — fdk-aac handles these in-process
        }
        #[cfg(not(feature = "fdk-aac"))]
        if matches!(codec, AudioCodec::HeAacV1 | AudioCodec::HeAacV2) {
            return Err(
                "HE-AAC v1/v2 requires the fdk-aac feature to be enabled".into()
            );
        }

        let mut resolved = ResolvedAudioEncode {
            codec,
            bitrate_kbps,
            sample_rate: enc.sample_rate,
            channels: enc.channels,
            transcode,
            chain: None,
        };
        resolved.chain = Some(HlsAudioChain::new(
            &resolved,
            HlsEncoderCtx {
                cancel: ctx.cancel.clone(),
                stats: ctx.stats.clone(),
                flow_id: ctx.flow_id.to_string(),
                output_id: output_id.to_string(),
                events: ctx.events.cloned(),
            },
        ));
        Ok(resolved)
    }
}

/// What an HLS output's loop hands [`resolve_audio_encode`] for the encoder
/// it opens: its cancel token, stats, flow and events.
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "media-codecs"), allow(dead_code))]
struct EncoderCtxParts<'a> {
    cancel: &'a CancellationToken,
    stats: &'a Arc<OutputStatsAccumulator>,
    flow_id: &'a str,
    events: Option<&'a EventSender>,
}

/// Remux a TS segment with audio re-encoding. Dispatches to in-process
/// or subprocess based on the media-codecs feature.
async fn remux_segment_audio(
    segment: &[u8],
    enc: &mut ResolvedAudioEncode,
) -> Result<Vec<u8>, String> {
    #[cfg(feature = "media-codecs")]
    {
        // Run in-process on a blocking thread (C codec calls are
        // synchronous), the chain moved there and back.
        let segment_owned = segment.to_vec();
        let Some(mut chain) = enc.chain.take() else {
            return Err("the audio re-encode chain was lost with a panicked segment".into());
        };
        let (chain, data) = tokio::task::spawn_blocking(move || {
            let data = chain.remux(&segment_owned);
            (chain, data)
        })
        .await
        .map_err(|e| format!("remux task panicked: {e}"))?;
        enc.chain = Some(chain);
        data
    }

    #[cfg(not(feature = "media-codecs"))]
    {
        remux_segment_via_ffmpeg(segment, &enc.ffmpeg_args).await
    }
}

// ════════════════════════════════════════════════════════════════════════
// In-process TS audio remuxer (media-codecs feature)
// ════════════════════════════════════════════════════════════════════════

/// What an HLS output's audio re-encode needs from its loop to open an
/// encoder: the same accounting every other re-encoding output registers.
#[cfg(feature = "media-codecs")]
struct HlsEncoderCtx {
    cancel: CancellationToken,
    stats: Arc<OutputStatsAccumulator>,
    flow_id: String,
    output_id: String,
    events: Option<EventSender>,
}

/// How far (90 kHz) a decoded frame's PTS may sit from where the encoder's
/// input is before the encoder's timeline is re-anchored on it: one 48 kHz
/// frame plus rounding, as on CMAF (`SOURCE_PTS_SLACK_TICKS`).
#[cfg(feature = "media-codecs")]
const HLS_SOURCE_PTS_SLACK_90K: u64 = 2_000;

/// An HLS output's audio re-encode, kept across its segments.
///
/// Each segment used to be re-encoded by a fresh chain: a new encoder whose
/// priming (2048 samples of AAC-LC) opened every segment, stamped at the
/// segment's first source PTS so the content presented that late; its delay
/// line was never flushed, so the tail of every segment's audio (the
/// priming's worth, plus the partial frame) was never emitted; a new AU
/// cutter lost an AU straddling two segments; and the audio PID's continuity
/// counter restarted at 0 in every segment. One chain now runs across them:
/// the cutter, the decoder, the channel / rate stage (streaming — its
/// constant delay comes off the stamps) and the encoder (the one every
/// re-encoding output uses, `audio_encode::AudioEncoder`: content-true
/// anchoring, codec delay off the stamps, one anchor and a running sample
/// count) all carry on, so the audio of consecutive segments is one
/// continuous stream. The frames the encoder hands back while a segment is
/// processed go into that segment; the last ~2048 samples of it are still in
/// the encoder's delay line and come out in the next segment — each on its
/// own PTS, so the rendition's audio timeline has no gap and no overlap.
/// A source whose PTS jumps (a splice, a loop) re-anchors the encoder on
/// the new PTS, as on CMAF.
#[cfg(feature = "media-codecs")]
struct HlsAudioChain {
    codec: AudioCodec,
    bitrate_kbps: u32,
    stage: super::audio_transcode::EncoderStage,
    ctx: HlsEncoderCtx,
    /// The source audio PID and stream type the cutter and decoder follow.
    source: Option<(u16, u8)>,
    cutter: Option<super::audio_au::AuCutter>,
    #[cfg(feature = "fdk-aac")]
    aac_decoder: Option<aac_audio::AacDecoder>,
    ff_decoder: Option<(video_codec::AudioDecoderCodec, video_engine::AudioDecoder)>,
    /// PTS the next decoded frame follows on from when its AU carried none.
    running_pts: Option<u64>,
    encoder: Option<super::audio_encode::AudioEncoder>,
    /// Where the encoder's input is on the source timeline: the PTS it was
    /// (re-)anchored at and the samples it has taken since, at `in_rate`.
    in_anchor: Option<u64>,
    in_samples: u64,
    in_rate: u32,
    /// The source's AC-3 / E-AC-3 dialogue level last carried into an
    /// AC-3 re-encode.
    encoder_dialnorm: Option<i8>,
    source_dialnorm: Option<i8>,
    /// Continuity counter of the audio PID across segments.
    audio_cc: u8,
}

#[cfg(feature = "media-codecs")]
impl HlsAudioChain {
    fn new(enc: &ResolvedAudioEncode, ctx: HlsEncoderCtx) -> Self {
        Self {
            codec: enc.codec,
            bitrate_kbps: enc.bitrate_kbps,
            stage: super::audio_transcode::EncoderStage::new(enc.transcode.clone(), enc.sample_rate, enc.channels),
            ctx,
            source: None,
            cutter: None,
            #[cfg(feature = "fdk-aac")]
            aac_decoder: None,
            ff_decoder: None,
            running_pts: None,
            encoder: None,
            in_anchor: None,
            in_samples: 0,
            in_rate: 0,
            encoder_dialnorm: None,
            source_dialnorm: None,
            audio_cc: 0,
        }
    }

    /// Re-encode one segment's audio (see the type doc).
    fn remux(&mut self, segment: &[u8]) -> Result<Vec<u8>, String> {
    use super::ts_parse::{ts_pid, ts_pusi, ts_has_payload, ts_payload_offset,
                          parse_pat_programs, PAT_PID};

    const TS_PACKET_SIZE: usize = 188;
    const TS_SYNC_BYTE: u8 = 0x47;

    use super::ts_pmt_edit::{
        parse_pmt, pmt_index, rebuild_pmt_section_fitting, AudioTarget, EsEdit, PmtEdit, PsiUnitStage,
        TsFlavour,
    };

    // ── Pass 1: Find PIDs from PAT/PMT ──
    //
    // PMT sections are reassembled, so a PMT spanning packets or sitting
    // behind a user-private table on its PID (ATSC / DigiCipher 0xC0) is
    // found — the pointer-target-only read used to fail the whole segment
    // remux ("no audio PID found") on such streams.
    let mut pmt_pid: Option<u16> = None;
    let mut program: Option<(u16, bool)> = None; // (program_number, pid_shared)
    let mut audio_pid: Option<u16> = None;
    let mut audio_stream_type: u8 = 0;
    let mut pmt_asm = super::ts_parse::SectionAssembler::new();

    let mut offset = 0;
    while offset + TS_PACKET_SIZE <= segment.len() {
        let pkt = &segment[offset..offset + TS_PACKET_SIZE];
        if pkt[0] != TS_SYNC_BYTE {
            offset += TS_PACKET_SIZE;
            continue;
        }

        let pid = ts_pid(pkt);

        if pid == PAT_PID && ts_pusi(pkt) && pmt_pid.is_none() {
            let mut programs = parse_pat_programs(pkt);
            if !programs.is_empty() {
                programs.sort_by_key(|(num, _)| *num);
                let (pn, ppid) = programs[0];
                pmt_pid = Some(ppid);
                program = Some((pn, programs.iter().filter(|(_, p)| *p == ppid).count() > 1));
            }
        }

        if Some(pid) == pmt_pid && audio_pid.is_none() {
            let sections: Vec<Vec<u8>> = pmt_asm.push_packet(pkt).map(|s| s.to_vec()).collect();
            if let Some((pn, shared)) = program
                && let Some(i) = pmt_index(&sections, pn, shared)
                && let Some(view) = parse_pmt(&sections[i])
                && let Some((apid, ast)) = pmt_audio_pid(&view)
            {
                audio_pid = Some(apid);
                audio_stream_type = ast;
            }
        }

        if audio_pid.is_some() {
            break;
        }
        offset += TS_PACKET_SIZE;
    }

    let audio_pid = audio_pid.ok_or("no audio PID found in segment")?;
    let pmt_pid = pmt_pid.ok_or("no PMT PID found in segment")?;
    let (program_number, pmt_pid_shared) = program.ok_or("no program in the PAT")?;
    let codec = self.codec;
    if matches!(codec, AudioCodec::Opus) {
        return Err("Opus not supported on HLS".into());
    }
    #[cfg(not(feature = "fdk-aac"))]
    if matches!(codec, AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2) {
        return Err(format!("unsupported codec for in-process HLS remux: {}", codec.as_str()));
    }

    // A new audio PID or codec: the cutter and decoder start afresh, and
    // the encoder re-anchors on the first frame they decode.
    if self.source != Some((audio_pid, audio_stream_type)) {
        self.source = Some((audio_pid, audio_stream_type));
        self.cutter = super::audio_au::AuFormat::for_stream_type(audio_stream_type)
            .map(super::audio_au::AuCutter::new);
        #[cfg(feature = "fdk-aac")]
        {
            self.aac_decoder = None;
        }
        self.ff_decoder = None;
        self.running_pts = None;
        self.in_anchor = None;
    }

    // ── Cut the audio into access units ──
    //
    // Across PES boundaries and across segments: the cutter runs on, so
    // an AU that straddles two PES (legal whenever data_alignment_indicator
    // is 0, and done by some broadcast muxers) or two segments is cut
    // whole. The cutter is the TS audio replacer's (`engine::audio_au`).
    let mut aus: Vec<(Vec<u8>, Option<u64>)> = Vec::new();
    match self.cutter.as_mut() {
        Some(cutter) => {
            offset = 0;
            while offset + TS_PACKET_SIZE <= segment.len() {
                let pkt = &segment[offset..offset + TS_PACKET_SIZE];
                offset += TS_PACKET_SIZE;
                if pkt[0] != TS_SYNC_BYTE || ts_pid(pkt) != audio_pid {
                    continue;
                }
                cutter.push_packet(pkt);
                while let Some(au) = cutter.next(false) {
                    aus.push((au.data, au.pts.filter(|_| au.pes_start)));
                }
            }
        }
        None => {
            // A format the cutter does not frame (Opus on 0x06): whole PES,
            // split per codec frame, the PES PTS on the first.
            let mut pes_buffer: Vec<u8> = Vec::with_capacity(16 * 1024);
            let mut pes_list: Vec<(Vec<u8>, u64)> = Vec::new();
            let mut pes_started = false;
            offset = 0;
            while offset + TS_PACKET_SIZE <= segment.len() {
                let pkt = &segment[offset..offset + TS_PACKET_SIZE];
                offset += TS_PACKET_SIZE;
                if pkt[0] != TS_SYNC_BYTE || ts_pid(pkt) != audio_pid || !ts_has_payload(pkt) {
                    continue;
                }
                let payload_start = ts_payload_offset(pkt);
                if payload_start >= TS_PACKET_SIZE {
                    continue;
                }
                let payload = &pkt[payload_start..];
                if ts_pusi(pkt) {
                    if pes_started
                        && let Some(p) = extract_pes_audio(&pes_buffer)
                    {
                        pes_list.push(p);
                    }
                    pes_buffer.clear();
                    pes_buffer.extend_from_slice(payload);
                    pes_started = true;
                } else if pes_started {
                    pes_buffer.extend_from_slice(payload);
                }
            }
            if pes_started
                && let Some(p) = extract_pes_audio(&pes_buffer)
            {
                pes_list.push(p);
            }
            if let Some(codec) = crate::engine::audio_decode::ff_codec_for_stream_type(audio_stream_type) {
                for (es, pts) in &pes_list {
                    for (k, f) in crate::engine::audio_decode::split_audio_codec_frames(es, codec)
                        .into_iter()
                        .enumerate()
                    {
                        aus.push((f.to_vec(), (k == 0).then_some(*pts)));
                    }
                }
            }
        }
    }


    // The source's AC-3 / E-AC-3 dialogue level, carried into an AC-3
    // re-encode (libavcodec writes -31 otherwise).
    if let Some(d) = aus.iter().rev().find_map(|(au, _)| crate::engine::audio_decode::ac3_dialnorm(au)) {
        self.source_dialnorm = Some(d);
    }

    // ── Decode → stage → encode, one chain across segments ──
    let decoded = self.decode(&aus, audio_stream_type)?;
    for frame in decoded {
        self.submit(frame)?;
    }
    let encoded_frames: Vec<RemuxEncodedFrame> = self
        .encoder
        .as_mut()
        .map(|e| e.drain().into_iter().map(|f| RemuxEncodedFrame { data: f.data.to_vec(), pts: f.pts }).collect())
        .unwrap_or_default();
    let out_sr = self.stage.output().map_or(0, |f| f.0);
    let mut audio_cc = self.audio_cc;

    // ── Target signalling for the PMT ──
    //
    // HLS is always ATSC-flavoured for AC-3 (0x81 + "AC-3" registration):
    // that is what Apple HLS and hls.js expect in TS segments, whatever the
    // source's convention. MP2 at 16 / 22.05 / 24 kHz is MPEG-2 LSF (0x04).
    let target = match codec {
        AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => AudioTarget::Aac,
        AudioCodec::Mp2 => AudioTarget::Mp2 { lsf: matches!(out_sr, 16_000 | 22_050 | 24_000) },
        AudioCodec::Ac3 => AudioTarget::Ac3 { flavour: TsFlavour::Atsc },
        AudioCodec::Opus => unreachable!("rejected above"),
    };
    let edit = [EsEdit::Audio { pid: audio_pid, target }];
    let mut pmt_stage = PsiUnitStage::new("hls_audio_remux");

    // ── Build output segment ──
    // Copy all non-audio TS packets, rewrite PMT, insert new audio packets
    let mut output = Vec::with_capacity(segment.len());
    let mut encoded_frame_idx = 0;
    let mut last_audio_position = false;

    offset = 0;
    while offset + TS_PACKET_SIZE <= segment.len() {
        let pkt = &segment[offset..offset + TS_PACKET_SIZE];
        offset += TS_PACKET_SIZE;

        if pkt[0] != TS_SYNC_BYTE {
            output.extend_from_slice(pkt);
            continue;
        }

        let pid = ts_pid(pkt);

        if pid == audio_pid {
            // Replace first audio packet position with re-encoded audio
            if !last_audio_position {
                last_audio_position = true;
                // Insert all remaining encoded frames here
                while encoded_frame_idx < encoded_frames.len() {
                    let ef = &encoded_frames[encoded_frame_idx];
                    encoded_frame_idx += 1;

                    // Build PES packet for this audio frame
                    let pes = build_audio_pes(codec.ts_pes_stream_id(), &ef.data, ef.pts);
                    // Packetize PES into TS packets
                    let ts_pkts = packetize_ts(audio_pid, &pes, &mut audio_cc);
                    for ts_pkt in &ts_pkts {
                        output.extend_from_slice(ts_pkt);
                    }
                }
            }
            // Skip original audio packet (already replaced)
            continue;
        }

        if pid == pmt_pid {
            // Rewrite the program's PMT: new audio stream_type and the
            // target's descriptor policy (source codec descriptors dropped).
            if let Some(mut unit) = pmt_stage.push(pkt, &mut output) {
                if let Some(i) = pmt_index(unit.sections(), program_number, pmt_pid_shared) {
                    let rebuilt = rebuild_pmt_section_fitting(
                        &unit,
                        i,
                        &PmtEdit { es: &edit, ..Default::default() },
                    );
                    if let Some(new_section) = rebuilt {
                        unit.replace_section(i, new_section);
                    }
                }
                pmt_stage.emit(unit, &mut output);
            }
            continue;
        }

        // Copy all other packets (video, PAT, null, etc.)
        output.extend_from_slice(pkt);
    }

    // If there are still encoded frames that weren't inserted (e.g., no audio
    // packets were found in the segment after the first pass), append them
    while encoded_frame_idx < encoded_frames.len() {
        let ef = &encoded_frames[encoded_frame_idx];
        encoded_frame_idx += 1;
        let pes = build_audio_pes(codec.ts_pes_stream_id(), &ef.data, ef.pts);
        let ts_pkts = packetize_ts(audio_pid, &pes, &mut audio_cc);
        for ts_pkt in &ts_pkts {
            output.extend_from_slice(ts_pkt);
        }
    }

    self.audio_cc = audio_cc;
    Ok(output)
    }

    /// Decode the segment's AUs with the chain's decoder: AAC (0x0F) via
    /// fdk-aac, MP2 / AC-3 / E-AC-3 / LATM via libavcodec. An AU carrying its
    /// PES's PTS is timed by it; the rest follow on from the samples decoded
    /// before them — across segments too.
    fn decode(&mut self, aus: &[(Vec<u8>, Option<u64>)], stream_type: u8) -> Result<Vec<PcmFrame>, String> {
        let mut out = Vec::new();
        if stream_type == 0x0F {
            #[cfg(feature = "fdk-aac")]
            {
                if self.aac_decoder.is_none() {
                    self.aac_decoder = Some(
                        aac_audio::AacDecoder::open_adts().map_err(|e| format!("AAC decoder init failed: {e}"))?,
                    );
                }
                let decoder = self.aac_decoder.as_mut().expect("opened above");
                for (au, au_pts) in aus {
                    let pts = next_pts(*au_pts, &mut self.running_pts);
                    match decoder.decode_frame(au) {
                        Ok(decoded) => {
                            let sr = decoder.sample_rate().unwrap_or(48_000);
                            if let Some(p) = pts {
                                if sr > 0 {
                                    self.running_pts = Some(p + (decoded.frame_size as u64) * 90_000 / sr as u64);
                                }
                                out.push(PcmFrame { planar: decoded.planar, pts: p, sample_rate: sr });
                            }
                        }
                        Err(e) => tracing::debug!("AAC decode error in HLS remux: {e}"),
                    }
                }
                return Ok(out);
            }
            #[cfg(not(feature = "fdk-aac"))]
            return Err("AAC decoding requires the fdk-aac feature".into());
        }
        let Some(codec) = crate::engine::audio_decode::ff_codec_for_stream_type(stream_type) else {
            return Err(format!("unsupported input audio stream type 0x{stream_type:02X} for re-encoding"));
        };
        if self.ff_decoder.as_ref().is_none_or(|(c, _)| *c != codec) {
            let d = crate::engine::audio_decode::open_ff_decoder(codec)
                .map_err(|e| format!("FFmpeg audio decoder init failed: {e}"))?;
            self.ff_decoder = Some((codec, d));
        }
        let (_, decoder) = self.ff_decoder.as_mut().expect("opened above");
        for (au, au_pts) in aus {
            next_pts(*au_pts, &mut self.running_pts);
            if decoder.send_packet(au, 0).is_err() {
                continue;
            }
            while let Ok(frame) = decoder.receive_frame() {
                let Some(p) = self.running_pts else {
                    continue;
                };
                let sr = frame.sample_rate;
                let n = frame.planar.first().map_or(0, |c| c.len());
                if sr > 0 {
                    self.running_pts = Some(p + n as u64 * 90_000 / sr as u64);
                }
                out.push(PcmFrame { planar: frame.planar, pts: p, sample_rate: sr });
            }
        }
        Ok(out)
    }

    /// One decoded frame into the encoder, building it on the first (at the
    /// format the stage resolves — the rendition keeps it: a playlist cannot
    /// signal a change of rate or channel count between segments) and
    /// re-anchoring it when the source's PTS is not where its input is.
    fn submit(&mut self, frame: PcmFrame) -> Result<(), String> {
        let n = frame.planar.first().map_or(0, |c| c.len()) as u64;
        if n == 0 || frame.sample_rate == 0 {
            return Ok(());
        }
        if self.encoder.is_none() {
            let (sr, ch) = self.stage.prepare(frame.sample_rate, frame.planar.len() as u8)?;
            let params = super::audio_encode::EncoderParams {
                codec: self.codec,
                sample_rate: sr,
                channels: ch,
                target_bitrate_kbps: self.bitrate_kbps,
                target_sample_rate: sr,
                target_channels: ch,
                opus_vbr_mode: None,
                opus_fec: false,
                opus_dtx: false,
                opus_frame_duration_ms: None,
            };
            let encoder = super::audio_encode::AudioEncoder::spawn(
                params,
                self.ctx.cancel.child_token(),
                self.ctx.flow_id.clone(),
                self.ctx.output_id.clone(),
                self.ctx.stats.clone(),
                self.ctx.events.clone(),
            )
            .map_err(|e| format!("audio encoder open failed: {e}"))?;
            self.encoder = Some(encoder);
            self.encoder_dialnorm = None;
        }
        // An AC-3 re-encode carries the source's dialogue level, and follows
        // a change of it.
        if self.codec == AudioCodec::Ac3
            && let Some(d) = self.source_dialnorm
            && self.encoder_dialnorm != Some(d)
            && let Some(enc) = self.encoder.as_mut()
        {
            if let Err(e) = enc.set_codec_option("dialnorm", &d.to_string()) {
                tracing::warn!("HLS output '{}': could not carry dialnorm {d} dB: {e}", self.ctx.output_id);
            }
            self.encoder_dialnorm = Some(d);
        }
        let expected = self
            .in_anchor
            .filter(|_| self.in_rate == frame.sample_rate)
            .map(|a| a + self.in_samples * 90_000 / self.in_rate.max(1) as u64);
        let enc = self.encoder.as_mut().expect("built above");
        if expected.is_none_or(|x| x.abs_diff(frame.pts) > HLS_SOURCE_PTS_SLACK_90K) {
            if self.in_anchor.is_some() {
                tracing::debug!(
                    "HLS output '{}': audio source PTS {} is not where the encoder's input is \
                     ({expected:?}); re-anchoring",
                    self.ctx.output_id, frame.pts,
                );
                enc.reanchor_pts();
            }
            self.in_anchor = Some(frame.pts);
            self.in_samples = 0;
            self.in_rate = frame.sample_rate;
        }
        enc.submit_through(&mut self.stage, &frame.planar, frame.sample_rate, frame.pts)?;
        self.in_samples += n;
        Ok(())
    }
}

/// The audio ES of a parsed PMT this remux re-encodes — the last one it can
/// decode, as it always picked — and the stream type it decodes as: an AAC
/// ADTS / LATM, MPEG audio, ATSC AC-3 / E-AC-3 stream type, or a private
/// (0x06) ES whose descriptors name AC-3, E-AC-3, LATM or Opus (decoded as
/// 0x81 / 0x87 / 0x11 / 0x06).
///
/// It used to require an H.264 / HEVC video ES beside it, which the remux
/// never uses: an MPEG-2 video source (VH1) failed every segment with "no
/// audio PID found" and the output published nothing. And it took every
/// 0x06 ES for audio, so a later teletext or subtitle PID was re-encoded in
/// place of the programme's sound.
#[cfg(feature = "media-codecs")]
fn pmt_audio_pid(view: &super::ts_pmt_edit::PmtView<'_>) -> Option<(u16, u8)> {
    use super::ts_parse::{descriptor_audio_kind, PrivateEsAudioKind};
    let mut audio: Option<(u16, u8)> = None;
    for es in &view.es {
        let decode_as = match es.stream_type {
            st @ (0x0F | 0x11 | 0x03 | 0x04 | 0x80 | 0x81 | 0xC1 | 0x87 | 0xC2) => Some(st),
            0x06 => match descriptor_audio_kind(view.es_info(es)) {
                Some(PrivateEsAudioKind::Ac3) => Some(0x81),
                Some(PrivateEsAudioKind::Eac3) => Some(0x87),
                Some(PrivateEsAudioKind::AacLatm) => Some(0x11),
                Some(PrivateEsAudioKind::Opus) => Some(0x06),
                _ => None,
            },
            _ => None,
        };
        if let Some(st) = decode_as {
            audio = Some((es.pid, st));
        }
    }
    audio
}

/// Extract elementary stream data and PTS from a PES packet.
#[cfg(feature = "media-codecs")]
fn extract_pes_audio(pes: &[u8]) -> Option<(Vec<u8>, u64)> {
    if pes.len() < 9 || pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
        return None;
    }
    let header_data_len = pes[8] as usize;
    let es_start = 9 + header_data_len;
    if es_start >= pes.len() { return None; }

    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    let pts = if pts_dts_flags >= 2 && pes.len() >= 14 {
        parse_pts(&pes[9..14])
    } else {
        0
    };

    Some((pes[es_start..].to_vec(), pts))
}

/// Parse PTS from 5 bytes of PES header.
#[cfg(feature = "media-codecs")]
fn parse_pts(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;
    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) << 15)
        | (b3 << 7)
        | (b4 >> 1)
}

/// Decoded PCM audio frame.
#[cfg(feature = "media-codecs")]
struct PcmFrame {
    /// One plane per channel — the frame's channel count.
    planar: Vec<Vec<f32>>,
    pts: u64,
    sample_rate: u32,
}

/// Encoded audio frame ready for TS muxing.
#[cfg(feature = "media-codecs")]
struct RemuxEncodedFrame {
    data: Vec<u8>,
    pts: u64,
}

/// The PTS of the next decoded frame: its AU's own, or where the previous
/// frame ended.
#[cfg(feature = "media-codecs")]
fn next_pts(au_pts: Option<u64>, running: &mut Option<u64>) -> Option<u64> {
    if au_pts.is_some() {
        *running = au_pts;
    }
    *running
}

/// Build a PES packet wrapping an audio frame.
#[cfg(feature = "media-codecs")]
fn build_audio_pes(stream_id: u8, audio_data: &[u8], pts: u64) -> Vec<u8> {
    // PES header: 0x000001 + stream_id + length + flags + PTS. stream_id is
    // 0xC0 for MP2 / AAC and 0xBD (private_stream_1) for AC-3 — see
    // `AudioCodec::ts_pes_stream_id`.
    let pes_header_len = 14; // 3 + 1 + 2 + 2 + 1 + 5
    let pes_len = 3 + 5 + audio_data.len(); // optional header + PTS + payload

    let mut pes = Vec::with_capacity(pes_header_len + audio_data.len());
    // Start code
    pes.push(0x00);
    pes.push(0x00);
    pes.push(0x01);
    pes.push(stream_id);
    // PES packet length (0 = unbounded for video, but for audio we set it)
    let pkt_len = pes_len as u16;
    pes.push((pkt_len >> 8) as u8);
    pes.push(pkt_len as u8);
    // Flags: marker bits (10), PTS present
    pes.push(0x80); // 10 00 0000
    pes.push(0x80); // PTS_DTS_flags = 10 (PTS only)
    // PES header data length
    pes.push(5); // 5 bytes for PTS

    // PTS (5 bytes): '0010', PTS[32..30], marker, PTS[29..15], marker,
    // PTS[14..0], marker. Bits 32..30 and 29..15 were each written one
    // place short (shifted by 30 and 15 where the field needs 29 and 14),
    // so every re-encoded audio PES carried a PTS that read back
    // 2^15-granular garbage: 900 000 read 441 248, 5.1 s early.
    let pts = pts & 0x1_FFFF_FFFF; // 33 bits
    pes.push(0x21 | (((pts >> 29) as u8) & 0x0E));
    pes.push((pts >> 22) as u8);
    pes.push(0x01 | (((pts >> 14) as u8) & 0xFE));
    pes.push((pts >> 7) as u8);
    pes.push(0x01 | (((pts as u8) & 0x7F) << 1));

    // Audio payload
    pes.extend_from_slice(audio_data);

    pes
}

/// Packetize a PES payload into 188-byte TS packets.
#[cfg(feature = "media-codecs")]
fn packetize_ts(pid: u16, pes: &[u8], cc: &mut u8) -> Vec<[u8; 188]> {
    const TS_PACKET_SIZE: usize = 188;
    let mut packets = Vec::new();
    let mut offset = 0;
    let mut is_first = true;

    while offset < pes.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];

        let pusi: u8 = if is_first { 1 } else { 0 };
        let current_cc = *cc;
        *cc = (*cc + 1) & 0x0F;

        // Header (4 bytes)
        pkt[0] = 0x47; // sync byte
        pkt[1] = (pusi << 6) | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = pid as u8;

        let remaining = pes.len() - offset;
        let payload_capacity = TS_PACKET_SIZE - 4;

        if remaining >= payload_capacity {
            // Full payload, no adaptation field
            pkt[3] = 0x10 | current_cc; // AFC=01 (payload only) + CC
            pkt[4..TS_PACKET_SIZE].copy_from_slice(&pes[offset..offset + payload_capacity]);
            offset += payload_capacity;
        } else {
            // Need stuffing via adaptation field
            let stuff_len = payload_capacity - remaining;
            if stuff_len == 1 {
                // Adaptation field with length 0
                pkt[3] = 0x30 | current_cc; // AFC=11 + CC
                pkt[4] = 0; // adaptation_field_length = 0
                pkt[5..5 + remaining].copy_from_slice(&pes[offset..]);
            } else {
                pkt[3] = 0x30 | current_cc; // AFC=11 + CC
                pkt[4] = (stuff_len - 1) as u8; // adaptation_field_length
                if stuff_len > 1 {
                    pkt[5] = 0x00; // flags
                    // Fill stuffing bytes
                    for i in 6..4 + stuff_len {
                        pkt[i] = 0xFF;
                    }
                }
                pkt[4 + stuff_len..4 + stuff_len + remaining].copy_from_slice(&pes[offset..]);
            }
            offset += remaining;
        }

        is_first = false;
        packets.push(pkt);
    }

    packets
}

// ════════════════════════════════════════════════════════════════════════
// ffmpeg subprocess fallback (when media-codecs feature is disabled)
// ════════════════════════════════════════════════════════════════════════

#[cfg(not(feature = "media-codecs"))]
fn build_remux_args(enc: &AudioEncodeConfig) -> Result<Vec<String>, String> {
    let codec = AudioCodec::parse(&enc.codec)
        .ok_or_else(|| format!("unknown codec '{}'", enc.codec))?;

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-loglevel".into(),
        "warning".into(),
        "-f".into(),
        "mpegts".into(),
        "-i".into(),
        "pipe:0".into(),
        "-c:v".into(),
        "copy".into(),
    ];

    let bitrate = format!("{}k",
        enc.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps()));

    match codec {
        AudioCodec::AacLc => {
            args.extend(["-c:a".into(), "aac".into(), "-profile:a".into(), "aac_low".into(), "-b:a".into(), bitrate]);
        }
        AudioCodec::HeAacV1 => {
            if !crate::engine::audio_encode::check_libfdk_aac_available() {
                return Err("he_aac_v1 requires libfdk_aac in ffmpeg".into());
            }
            args.extend(["-c:a".into(), "libfdk_aac".into(), "-profile:a".into(), "aac_he".into(), "-b:a".into(), bitrate]);
        }
        AudioCodec::HeAacV2 => {
            if !crate::engine::audio_encode::check_libfdk_aac_available() {
                return Err("he_aac_v2 requires libfdk_aac in ffmpeg".into());
            }
            args.extend(["-c:a".into(), "libfdk_aac".into(), "-profile:a".into(), "aac_he_v2".into(), "-b:a".into(), bitrate]);
        }
        AudioCodec::Mp2 => {
            args.extend(["-c:a".into(), "mp2".into(), "-b:a".into(), bitrate]);
        }
        AudioCodec::Ac3 => {
            args.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), bitrate]);
        }
        AudioCodec::Opus => {
            return Err("Opus is not supported on HLS in this build".into());
        }
    }

    if let Some(sr) = enc.sample_rate {
        args.extend(["-ar".into(), sr.to_string()]);
    }
    if let Some(ch) = enc.channels {
        args.extend(["-ac".into(), ch.to_string()]);
    }

    args.extend(["-f".into(), "mpegts".into(), "pipe:1".into()]);
    Ok(args)
}

/// Run ffmpeg as a one-shot remuxer over a single HLS segment.
#[cfg(not(feature = "media-codecs"))]
async fn remux_segment_via_ffmpeg(
    segment: &[u8],
    args: &[String],
) -> Result<Vec<u8>, String> {
    let mut child = tokio::process::Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let bytes = Bytes::copy_from_slice(segment);
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }

    let output = tokio::time::timeout(HLS_REMUX_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "ffmpeg remux timed out".to_string())?
        .map_err(|e| format!("ffmpeg wait failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg exited with {}: {}", output.status, stderr.trim()));
    }

    if output.stdout.is_empty() {
        return Err("ffmpeg produced no output".to_string());
    }

    Ok(output.stdout)
}

#[cfg(test)]
#[cfg(not(feature = "media-codecs"))]
mod tests {
    use super::*;

    fn make_enc(codec: &str) -> AudioEncodeConfig {
        AudioEncodeConfig {
            codec: codec.into(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
            silent_fallback: false,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
            source_audio_pid: None,
            ts_signalling: None,
        }
    }

    #[test]
    fn build_remux_args_aac_lc_minimal() {
        let args = build_remux_args(&make_enc("aac_lc")).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-i pipe:0"));
        assert!(joined.contains("-c:v copy"));
        assert!(joined.contains("-c:a aac"));
        assert!(joined.contains("-profile:a aac_low"));
        assert!(joined.contains("-b:a 128k"));
        assert!(joined.contains("-f mpegts"));
        assert!(joined.ends_with("pipe:1"));
    }

    #[test]
    fn build_remux_args_he_aac_requires_libfdk_or_errors_clearly() {
        // HE-AAC v1/v2 must request libfdk_aac (the native ffmpeg `aac`
        // encoder hard-rejects the aac_he profiles). When libfdk_aac IS
        // present we expect a libfdk_aac arg list with the right profile;
        // when it's NOT present we expect an Err with a message that
        // mentions libfdk_aac so the operator knows what to install.
        for (codec, profile) in [
            ("he_aac_v1", "aac_he"),
            ("he_aac_v2", "aac_he_v2"),
        ] {
            match build_remux_args(&make_enc(codec)) {
                Ok(args) => {
                    let joined = args.join(" ");
                    assert!(
                        joined.contains("-c:a libfdk_aac"),
                        "{codec}: expected libfdk_aac, got {joined}"
                    );
                    assert!(joined.contains(&format!("-profile:a {profile}")));
                }
                Err(msg) => {
                    assert!(
                        msg.contains("libfdk_aac"),
                        "{codec}: error message should mention libfdk_aac, got: {msg}"
                    );
                }
            }
        }
    }

    #[test]
    fn build_remux_args_mp2_and_ac3() {
        let args = build_remux_args(&make_enc("mp2")).unwrap();
        let j = args.join(" ");
        assert!(j.contains("-c:a mp2"));
        assert!(j.contains("-b:a 192k"));
        let args = build_remux_args(&make_enc("ac3")).unwrap();
        let j = args.join(" ");
        assert!(j.contains("-c:a ac3"));
        assert!(j.contains("-b:a 192k"));
    }

    #[test]
    fn build_remux_args_opus_rejected() {
        assert!(build_remux_args(&make_enc("opus")).is_err());
    }

    #[test]
    fn build_remux_args_overrides_propagate() {
        let mut enc = make_enc("aac_lc");
        enc.bitrate_kbps = Some(96);
        enc.sample_rate = Some(44_100);
        enc.channels = Some(1);
        let args = build_remux_args(&enc).unwrap();
        let j = args.join(" ");
        assert!(j.contains("-b:a 96k"));
        assert!(j.contains("-ar 44100"));
        assert!(j.contains("-ac 1"));
    }
}

#[cfg(all(test, feature = "media-codecs", feature = "fdk-aac"))]
mod pmt_remux_tests {
    use super::*;
    use crate::engine::ts_pmt_edit::{descriptors, is_pmt_for, parse_pmt};
    use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};

    /// A segment whose PMT PID carries a short-form 0xC0 section ahead of
    /// the PMT (ATSC / DigiCipher shape) and an AAC ES with an AAC
    /// descriptor. The AC-3 remux must find the PMT, signal ATSC (0x81 +
    /// "AC-3" registration — what Apple HLS / hls.js expect, whatever the
    /// source flavour), drop the stale AAC descriptor, and keep the 0xC0
    /// section byte-identical.
    /// A chain as an HLS output builds it, for `codec` with these overrides.
    fn chain(
        codec: AudioCodec,
        bitrate_kbps: u32,
        sample_rate: Option<u32>,
        channels: Option<u8>,
        transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    ) -> HlsAudioChain {
        let resolved = ResolvedAudioEncode { codec, bitrate_kbps, sample_rate, channels, transcode, chain: None };
        HlsAudioChain::new(
            &resolved,
            HlsEncoderCtx {
                cancel: CancellationToken::new(),
                stats: Arc::new(OutputStatsAccumulator::new("hls".into(), "hls".into(), "hls".into())),
                flow_id: "f".into(),
                output_id: "hls".into(),
                events: None,
            },
        )
    }

    /// One segment through a fresh chain.
    fn remux_ts_audio_inprocess(
        segment: &[u8],
        codec: AudioCodec,
        bitrate_kbps: u32,
        sample_rate: Option<u32>,
        channels: Option<u8>,
        transcode: Option<crate::engine::audio_transcode::TranscodeJson>,
    ) -> Result<Vec<u8>, String> {
        chain(codec, bitrate_kbps, sample_rate, channels, transcode).remux(segment)
    }

    #[test]
    fn ac3_remux_is_atsc_signalled_behind_a_private_section() {
        const ADTS: &[u8] = include_bytes!("testdata/sine1k_aac_lc_48k_stereo.adts");
        let c0 = vec![0xC0, 0x00, 0x05, 1, 2, 3, 4, 5];
        let pmt = pmt_section(
            1,
            0,
            0x100,
            &[],
            &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[0x0A, 0x04, b'e', b'n', b'g', 0, 0x7C, 0x01, 0x58])],
        );
        let mut seg = pat_packet(&[(1, 0x1000)], 0, 0).to_vec();
        seg.extend_from_slice(&packetize_sections(0x1000, &[&c0, &pmt], 0)[0]);
        let mut cc = 0u8;
        let mut off = 0usize;
        let mut pts = 90_000u64;
        while off + 7 <= ADTS.len() {
            let len = (((ADTS[off + 3] as usize) & 0x03) << 11)
                | ((ADTS[off + 4] as usize) << 3)
                | ((ADTS[off + 5] as usize) >> 5);
            if len == 0 || off + len > ADTS.len() {
                break;
            }
            let pes = build_audio_pes(0xC0, &ADTS[off..off + len], pts);
            for p in packetize_ts(0x101, &pes, &mut cc) {
                seg.extend_from_slice(&p);
            }
            pts += 1920;
            off += len;
        }
        let out = remux_ts_audio_inprocess(&seg, AudioCodec::Ac3, 192, None, None, None)
            .expect("remux");
        let pmt_pkt = out
            .chunks(188)
            .find(|p| crate::engine::ts_parse::ts_pid(p) == 0x1000)
            .expect("PMT packet in the output");
        assert_eq!(&pmt_pkt[5..5 + c0.len()], &c0[..], "0xC0 section byte-identical");
        let s = crate::engine::ts_parse::find_section_in_packet(pmt_pkt, 0x02, Some(1)).unwrap();
        let sec = &pmt_pkt[s.start..s.end()];
        assert!(is_pmt_for(sec, 1));
        assert_eq!(crate::engine::ts_parse::mpeg2_crc32(sec), 0);
        let v = parse_pmt(sec).unwrap();
        let a = v.es.iter().find(|e| e.pid == 0x101).unwrap();
        assert_eq!(a.stream_type, 0x81);
        let tags: Vec<u8> = descriptors(v.es_info(a)).map(|(t, _)| t).collect();
        assert_eq!(tags, vec![0x0A, 0x05], "7C dropped, AC-3 registration added");
        // AC-3 rides private_stream_1 (0xBD), as A/52 Annex A and TS 101
        // 154 require — the remux used to write the MPEG audio id 0xC0.
        let ids: Vec<u8> = out
            .chunks(188)
            .filter(|p| crate::engine::ts_parse::ts_pid(p) == 0x101 && crate::engine::ts_parse::ts_pusi(p))
            .map(|p| p[crate::engine::ts_parse::ts_payload_offset(p) + 3])
            .collect();
        assert!(!ids.is_empty(), "re-encoded audio in the segment");
        assert!(ids.iter().all(|&id| id == 0xBD), "AC-3 PES stream_id: {ids:02X?}");
    }

    /// A 2 s 48 kHz stereo segment (PAT, PMT with H.264 + AAC, AAC-LC ADTS
    /// with a 1 kHz burst at sample `at`) whose audio PES are cut every 700
    /// bytes — through AUs — each stamped with the PTS of the first AU that
    /// commences in it. `(segment, source AU count, samples per AU)`.
    fn burst_segment(at: usize) -> Vec<u8> {
        let rate = 48_000usize;
        let n = rate / 100;
        let mut pcm = vec![0.0f32; rate * 2];
        for k in 0..n {
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / (n - 1) as f32).cos();
            pcm[at + k] = 0.5 * w * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / rate as f32).sin();
        }
        let mut e = aac_audio::AacEncoder::open(&aac_codec::EncoderConfig {
            profile: aac_codec::AacProfile::AacLc,
            sample_rate: 48_000,
            channels: 2,
            bitrate: 128_000,
            afterburner: true,
            sbr_signaling: aac_codec::SbrSignaling::default(),
            transport: aac_codec::TransportType::Adts,
        })
        .unwrap();
        let delay = e.codec_delay_samples() as i64;
        let mut aus: Vec<(Vec<u8>, u64)> = Vec::new();
        for (i, c) in pcm.chunks(1024).enumerate() {
            let ed = e.encode_frame(&[c.to_vec(), c.to_vec()]).unwrap();
            if !ed.bytes.is_empty() {
                let first = aus.len() as i64 * 1024 - delay;
                let _ = i;
                aus.push((ed.bytes, (900_000 + first * 90_000 / 48_000) as u64));
            }
        }
        let starts: Vec<usize> = aus
            .iter()
            .scan(0usize, |o, (b, _)| {
                let s = *o;
                *o += b.len();
                Some(s)
            })
            .collect();
        let es: Vec<u8> = aus.iter().flat_map(|(b, _)| b.clone()).collect();
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
        let mut seg = pat_packet(&[(1, 0x1000)], 0, 0).to_vec();
        seg.extend_from_slice(&packetize_sections(0x1000, &[&pmt], 0)[0]);
        let mut cc = 0u8;
        for (k, chunk) in es.chunks(700).enumerate() {
            let off = k * 700;
            let first = starts.iter().position(|&s| s >= off).unwrap_or(aus.len() - 1);
            seg.extend(crate::engine::ts_test_fixtures::pes_packets(0x101, 0xC0, chunk, aus[first].1, &mut cc));
        }
        seg
    }

    /// The re-encoded audio of `out`: sample rate and channel-0 PCM.
    fn decode_audio(out: &[u8]) -> (u32, Vec<f32>) {
        let mut pes: Vec<Vec<u8>> = Vec::new();
        for p in out.chunks(188) {
            if crate::engine::ts_parse::ts_pid(p) != 0x101 || !crate::engine::ts_parse::ts_has_payload(p) {
                continue;
            }
            let payload = &p[crate::engine::ts_parse::ts_payload_offset(p)..];
            if crate::engine::ts_parse::ts_pusi(p) {
                pes.push(payload.to_vec());
            } else if let Some(l) = pes.last_mut() {
                l.extend_from_slice(payload);
            }
        }
        let mut d = aac_audio::AacDecoder::open_adts().unwrap();
        let mut pcm = Vec::new();
        for b in &pes {
            let es = &b[9 + b[8] as usize..];
            pcm.extend_from_slice(&d.decode_frame(es).unwrap().planar[0]);
        }
        (d.sample_rate().unwrap(), pcm)
    }

    /// Where the burst is in `pcm` (a 10 ms 1 kHz burst at `rate`).
    fn burst_index(pcm: &[f32], rate: u32) -> usize {
        let n = (rate / 100) as usize;
        let b: Vec<f32> = (0..n)
            .map(|k| {
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / (n - 1) as f32).cos();
                w * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / rate as f32).sin()
            })
            .collect();
        (0..pcm.len() - n)
            .max_by(|&x, &y| {
                let s = |k: usize| -> f32 { b.iter().zip(&pcm[k..]).map(|(p, q)| p * q).sum() };
                s(x).total_cmp(&s(y))
            })
            .unwrap()
    }

    /// **B1 / B2 / B3 on HLS.** `audio_encode.sample_rate` with no
    /// `transcode` block resamples, and the audio PES that cut through AUs
    /// lose none of them: the burst comes out at its scaled position (plus
    /// the stream's one encoder priming and the resampler's constant delay,
    /// both taken off the stamps), in 44.1 kHz ADTS. Before, the encoder was
    /// opened at 44.1 kHz and fed the 48 kHz PCM as it was — the burst at its
    /// unscaled index, the audio 8.8 % slow — and each straddling AU (with
    /// the rest of its next PES) was lost, moving the burst earlier.
    #[test]
    fn a_rate_override_without_a_transcode_block_resamples_every_au() {
        let at = 48_000 + 333;
        let seg = burst_segment(at);
        let out = remux_ts_audio_inprocess(&seg, AudioCodec::AacLc, 128, Some(44_100), None, None)
            .expect("remux");
        let (rate, pcm) = decode_audio(&out);
        assert_eq!(rate, 44_100, "the stream is 44.1 kHz");
        // 2 s of source → ~88 200 samples (whole encoder frames), less what
        // the chain holds for the next segment: the encoder's delay line and
        // a partial frame.
        assert!((88_200 - 4 * 1024..=88_200).contains(&(pcm.len() as i64)), "{} samples: not the 48 kHz count", pcm.len());
        // The source's own AAC priming is decoded too (2048 samples at
        // 48 kHz, before the scaling), then the resampler's delay and the
        // re-encode's priming (2048 at 44.1).
        let mut stage = crate::engine::audio_transcode::EncoderStage::new(None, Some(44_100), None);
        stage.prepare(48_000, 2).unwrap();
        let expected = (at as f64 + 2048.0) * 44_100.0 / 48_000.0 + stage.delay() as f64 + 2048.0;
        let got = burst_index(&pcm, 44_100) as f64;
        assert!((got - expected).abs() <= 3.0, "burst at {got}, expected {expected:.1}");
    }

    /// The same segment without an override: 48 kHz, every AU, the burst at
    /// its own index plus the priming.
    #[test]
    fn a_straddling_au_is_re_encoded_on_hls() {
        let at = 48_000 + 333;
        let seg = burst_segment(at);
        let out = remux_ts_audio_inprocess(&seg, AudioCodec::AacLc, 128, None, None, None).expect("remux");
        let (rate, pcm) = decode_audio(&out);
        assert_eq!(rate, 48_000);
        let got = burst_index(&pcm, 48_000) as i64;
        // The source's priming and the re-encode's, 2048 samples each.
        assert!((got - (at as i64 + 4096)).abs() <= 2, "burst at {got}");
    }

    /// A re-encoded audio PES reads back the PTS it was built with, across
    /// the whole 33-bit range.
    #[test]
    fn an_audio_pes_carries_its_pts() {
        for pts in [0u64, 900_000, 1 << 15, (1 << 30) + 12_345, (1 << 33) - 1] {
            let pes = build_audio_pes(0xC0, &[0xFF, 0xF1, 0, 0], pts);
            let mut cc = 0;
            let pkt = packetize_ts(0x101, &pes, &mut cc).remove(0);
            assert_eq!(crate::engine::ts_parse::extract_pes_pts(&pkt), Some(pts));
        }
    }

    /// The PES PTS and continuity counters of the re-encoded audio in `out`.
    fn audio_pes_pts_and_ccs(out: &[u8]) -> (Vec<u64>, Vec<u8>) {
        let mut pts = Vec::new();
        let mut ccs = Vec::new();
        for p in out.chunks(188) {
            if crate::engine::ts_parse::ts_pid(p) != 0x101 {
                continue;
            }
            ccs.push(p[3] & 0x0F);
            if crate::engine::ts_parse::ts_pusi(p)
                && let Some(v) = crate::engine::ts_parse::extract_pes_pts(p)
            {
                pts.push(v);
            }
        }
        (pts, ccs)
    }

    /// Consecutive segments run through one encoder: the audio of the
    /// second picks up exactly where the first's ended — every frame one
    /// 1024-sample step after the one before, across the cut — with one
    /// priming for the stream (stamped ahead of the content, 2048 samples
    /// before the first source PTS) and the continuity counter carried on.
    /// Each segment used to get a fresh encoder: its own priming, stamped at
    /// the segment's first source PTS (the content presented 42.7 ms late),
    /// the tail in the delay line never emitted (a gap at every cut), and
    /// the counter back at 0.
    #[test]
    fn consecutive_segments_are_one_continuous_encode() {
        let aus = aac_frames(48_000, 2, 2.0, 900_000);
        let half = aus.len() / 2;
        let mut c = chain(AudioCodec::AacLc, 128, None, None, None);
        let one = c.remux(&aac_segment(&aus[..half])).expect("remux 1");
        let two = c.remux(&aac_segment(&aus[half..])).expect("remux 2");
        let (p1, cc1) = audio_pes_pts_and_ccs(&one);
        let (p2, cc2) = audio_pes_pts_and_ccs(&two);
        assert!(!p1.is_empty() && !p2.is_empty());
        // The source's own AAC priming decodes too, so the source content
        // starts at its first PTS; the re-encode's priming sits before it.
        assert_eq!(p1[0], 900_000 - 2048 * 90_000 / 48_000, "one priming, ahead of the content");
        let all: Vec<u64> = p1.iter().chain(&p2).copied().collect();
        for (k, w) in all.windows(2).enumerate() {
            assert!(w[1].abs_diff(w[0]) <= 1_921 && w[1] > w[0], "step {k}: {} -> {}", w[0], w[1]);
        }
        let ccs: Vec<u8> = cc1.iter().chain(&cc2).copied().collect();
        for w in ccs.windows(2) {
            assert_eq!(w[1], (w[0] + 1) & 0x0F, "the audio CC runs on across the cut");
        }
    }

    /// `secs` of a tone as AAC-LC ADTS frames at `rate` × `channels`, each
    /// with its PTS, the first at `pts0`.
    fn aac_frames(rate: u32, channels: u8, secs: f64, pts0: u64) -> Vec<(Vec<u8>, u64)> {
        let mut e = aac_audio::AacEncoder::open(&aac_codec::EncoderConfig {
            profile: aac_codec::AacProfile::AacLc,
            sample_rate: rate,
            channels,
            bitrate: 64_000 * channels as u32,
            afterburner: true,
            sbr_signaling: aac_codec::SbrSignaling::default(),
            transport: aac_codec::TransportType::Adts,
        })
        .unwrap();
        let n = (rate as f64 * secs) as usize;
        let tone: Vec<f32> =
            (0..n).map(|k| 0.3 * (2.0 * std::f32::consts::PI * 440.0 * k as f32 / rate as f32).sin()).collect();
        let mut aus = Vec::new();
        for c in tone.chunks(1024) {
            let ed = e.encode_frame(&vec![c.to_vec(); channels as usize]).unwrap();
            if !ed.bytes.is_empty() {
                aus.push((ed.bytes, pts0 + aus.len() as u64 * 1024 * 90_000 / rate as u64));
            }
        }
        aus
    }

    /// A segment (PAT, PMT: H.264 0x100 + AAC 0x101) carrying `aus`, one
    /// PES each.
    fn aac_segment(aus: &[(Vec<u8>, u64)]) -> Vec<u8> {
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
        let mut seg = pat_packet(&[(1, 0x1000)], 0, 0).to_vec();
        seg.extend_from_slice(&packetize_sections(0x1000, &[&pmt], 0)[0]);
        let mut cc = 0u8;
        for (au, pts) in aus {
            seg.extend(crate::engine::ts_test_fixtures::pes_packets(0x101, 0xC0, au, *pts, &mut cc));
        }
        seg
    }

    /// The re-encoded audio of `out`: sample rate, channels, samples a
    /// channel.
    fn decoded_shape(out: &[u8]) -> (u32, u8, usize) {
        let mut pes: Vec<Vec<u8>> = Vec::new();
        for p in out.chunks(188) {
            if crate::engine::ts_parse::ts_pid(p) != 0x101 || !crate::engine::ts_parse::ts_has_payload(p) {
                continue;
            }
            let payload = &p[crate::engine::ts_parse::ts_payload_offset(p)..];
            if crate::engine::ts_parse::ts_pusi(p) {
                pes.push(payload.to_vec());
            } else if let Some(l) = pes.last_mut() {
                l.extend_from_slice(payload);
            }
        }
        let mut d = aac_audio::AacDecoder::open_adts().unwrap();
        let mut samples = 0;
        let mut channels = 0;
        for b in &pes {
            let f = d.decode_frame(&b[9 + b[8] as usize..]).unwrap();
            channels = f.planar.len() as u8;
            samples += f.planar[0].len();
        }
        (d.sample_rate().unwrap(), channels, samples)
    }

    /// A source that changes format in-band mid-segment is converted to the
    /// segment's output format like the rest: a 5.1 programme going to a
    /// stereo break (with `audio_encode.channels: 2`) keeps its second half
    /// — it was dropped, every stereo frame refused by the 6-channel stage
    /// ("expected 6 input channels, got 2") — and a 48 → 44.1 kHz splice
    /// with no override is resampled to the 48 kHz the encoder opened at —
    /// its 44.1 kHz PCM was encoded as 48 kHz, 8.1 % short and fast.
    #[test]
    fn an_in_band_format_change_mid_segment_is_converted() {
        let first = aac_frames(48_000, 6, 1.0, 900_000);
        let at = first.last().unwrap().1 + 1_920;
        let mut aus = first;
        aus.extend(aac_frames(48_000, 2, 1.0, at));
        let out = remux_ts_audio_inprocess(&aac_segment(&aus), AudioCodec::AacLc, 128, None, Some(2), None)
            .expect("remux");
        let (rate, channels, samples) = decoded_shape(&out);
        assert_eq!((rate, channels), (48_000, 2));
        assert!((samples as i64 - 96_000).abs() <= 4 * 1024, "{samples} samples: both halves");

        let first = aac_frames(48_000, 2, 1.0, 900_000);
        let at = first.last().unwrap().1 + 1_920;
        let mut aus = first;
        aus.extend(aac_frames(44_100, 2, 1.0, at));
        let out = remux_ts_audio_inprocess(&aac_segment(&aus), AudioCodec::AacLc, 128, None, None, None)
            .expect("remux");
        let (rate, _, samples) = decoded_shape(&out);
        assert_eq!(rate, 48_000);
        assert!((samples as i64 - 96_000).abs() <= 4 * 1024, "{samples} samples: 44.1 kHz resampled");
    }

    /// The audio is found beside an MPEG-2 video ES, and a private-data
    /// (0x06) ES after it is not taken for it. The remux required an H.264
    /// / HEVC video ES — an MPEG-2 source (VH1) failed every segment with
    /// "no audio PID found" and the output published nothing — and took the
    /// last 0x06 ES (teletext here) for the audio.
    #[test]
    fn the_audio_is_found_beside_mpeg2_video_and_private_data() {
        let aus = aac_frames(48_000, 2, 0.5, 900_000);
        let pmt = pmt_section(
            1,
            0,
            0x100,
            &[],
            &[(0x02, 0x100, &[]), (0x0F, 0x101, &[]), (0x06, 0x102, &[0x56, 0x05, b'e', b'n', b'g', 0x09, 0x00])],
        );
        let mut seg = pat_packet(&[(1, 0x1000)], 0, 0).to_vec();
        seg.extend_from_slice(&packetize_sections(0x1000, &[&pmt], 0)[0]);
        let mut cc = 0u8;
        for (au, pts) in &aus {
            seg.extend(crate::engine::ts_test_fixtures::pes_packets(0x101, 0xC0, au, *pts, &mut cc));
        }
        let out = remux_ts_audio_inprocess(&seg, AudioCodec::Ac3, 192, None, None, None).expect("remux");
        // The AAC ES is the one re-encoded (to AC-3), and the PMT says so;
        // the teletext ES is left alone.
        let pmt_pkt = out.chunks(188).find(|p| crate::engine::ts_parse::ts_pid(p) == 0x1000).expect("PMT");
        let s = crate::engine::ts_parse::find_section_in_packet(pmt_pkt, 0x02, Some(1)).unwrap();
        let v = parse_pmt(&pmt_pkt[s.start..s.end()]).unwrap();
        let types: Vec<(u16, u8)> = v.es.iter().map(|e| (e.pid, e.stream_type)).collect();
        assert_eq!(types, vec![(0x100, 0x02), (0x101, 0x81), (0x102, 0x06)]);
        let ac3 = out
            .chunks(188)
            .filter(|p| crate::engine::ts_parse::ts_pid(p) == 0x101 && crate::engine::ts_parse::ts_pusi(p))
            .filter(|p| {
                let o = crate::engine::ts_parse::ts_payload_offset(p);
                p[o + 9 + p[o + 8] as usize..].starts_with(&[0x0B, 0x77])
            })
            .count();
        assert!(ac3 > 10, "{ac3} AC-3 PES on the audio PID");
    }

    /// The rendition keeps the format its first segment resolved: a later
    /// segment whose source is at 44.1 kHz is converted to 48 kHz rather
    /// than changing the rendition's rate between segments, which a
    /// playlist cannot signal.
    #[test]
    fn a_later_segment_keeps_the_renditions_format() {
        let mut c = chain(AudioCodec::AacLc, 128, None, None, None);
        c.remux(&aac_segment(&aac_frames(48_000, 2, 0.5, 900_000))).unwrap();
        assert_eq!(c.stage.output(), Some((48_000, 2)));
        let out = c.remux(&aac_segment(&aac_frames(44_100, 2, 1.0, 950_000))).unwrap();
        assert_eq!(c.stage.output(), Some((48_000, 2)));
        let (rate, _, samples) = decoded_shape(&out);
        assert_eq!(rate, 48_000);
        // Its second of audio at 48 kHz, plus the first segment's tail from
        // the delay line, less this one's.
        assert!((samples as i64 - 48_000).abs() <= 4 * 1024, "{samples} samples");
    }

}
