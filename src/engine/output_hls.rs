// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::VecDeque;
#[cfg(not(feature = "video-thumbnail"))]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
#[cfg(not(feature = "video-thumbnail"))]
use std::time::Duration;

#[cfg(not(feature = "video-thumbnail"))]
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
#[cfg(not(feature = "video-thumbnail"))]
use super::audio_encode::check_ffmpeg_available;
use super::packet::RtpPacket;
use super::ts_program_filter::TsProgramFilter;

/// Maximum time we'll wait for ffmpeg to re-mux a single segment.
/// Segments are typically 2-6 seconds, so 30 s is a generous safety bound.
#[cfg(not(feature = "video-thumbnail"))]
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

    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("hls".to_string()),
        video_passthrough: true,
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
    });

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
    let audio_encode_config: Option<ResolvedAudioEncode> = match &config.audio_encode {
        Some(enc) => match resolve_audio_encode(enc, config.transcode.clone(), &config.id) {
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
                        if let Some(ref mut filter) = program_filter {
                            filter_scratch.clear();
                            filter.filter_into(payload, &mut filter_scratch);
                            if filter_scratch.is_empty() {
                                continue;
                            }
                            segment_buf.extend_from_slice(&filter_scratch);
                        } else {
                            segment_buf.extend_from_slice(payload);
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
                            let segment_data = if let Some(ref enc) = audio_encode_config {
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

/// Parse a URL into (host, port, path) components.
/// Supports http:// and https:// (https falls back to port 443 but no TLS --
/// for production HTTPS a proper TLS library would be needed).
fn parse_url(url: &str) -> anyhow::Result<(String, u16, String)> {
    // Strip scheme
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        ("http", rest)
    } else {
        anyhow::bail!("unsupported URL scheme in: {url}");
    };

    let default_port: u16 = if scheme == "https" { 443 } else { 80 };

    // Split host+port from path
    let (host_port, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };

    let (host, port) = if let Some(colon) = host_port.rfind(':') {
        let h = &host_port[..colon];
        let p: u16 = host_port[colon + 1..].parse().unwrap_or(default_port);
        (h.to_string(), p)
    } else {
        (host_port.to_string(), default_port)
    };

    Ok((host, port, path.to_string()))
}

/// Perform an HTTP PUT request using a raw TCP stream.
///
/// This is a minimal implementation that avoids heavy HTTP client dependencies.
/// It does NOT support TLS (https:// URLs will connect without encryption),
/// redirects, or chunked transfer encoding. For production use, consider
/// switching to `reqwest` once it is added to Cargo.toml.
async fn http_put(
    url: &str,
    body: &[u8],
    content_type: &str,
    auth_token: Option<&str>,
) -> anyhow::Result<()> {
    let (host, port, path) = parse_url(url)?;

    // Defence in depth: even if validation is bypassed, refuse to inline CRLF
    // into the raw HTTP request line or headers.
    let has_crlf = |s: &str| s.bytes().any(|b| b == b'\r' || b == b'\n');
    if has_crlf(&host) || has_crlf(&path) || has_crlf(content_type)
        || auth_token.map_or(false, has_crlf)
    {
        anyhow::bail!("HTTP PUT refused: header injection attempt in URL or headers");
    }

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await
        .map_err(|e| anyhow::anyhow!("connect to {addr}: {e}"))?;

    // Build HTTP/1.1 PUT request
    let mut request = format!(
        "PUT {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len(),
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
        if parts.len() >= 2 {
            if let Ok(status) = parts[1].parse::<u16>() {
                if !(200..300).contains(&status) {
                    anyhow::bail!("HTTP PUT returned status {status}: {status_line}");
                }
            }
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
    /// remux path (`video-thumbnail` feature); the subprocess fallback
    /// logs a warning and ignores it.
    transcode: Option<super::audio_transcode::TranscodeJson>,
    /// Pre-built ffmpeg args (only used for subprocess fallback).
    #[cfg(not(feature = "video-thumbnail"))]
    ffmpeg_args: Vec<String>,
}

/// Resolve and validate audio encode config at output startup.
#[allow(unused_variables)]
fn resolve_audio_encode(
    enc: &AudioEncodeConfig,
    transcode: Option<super::audio_transcode::TranscodeJson>,
    output_id: &str,
) -> Result<ResolvedAudioEncode, String> {
    let codec = AudioCodec::parse(&enc.codec)
        .ok_or_else(|| format!("unknown codec '{}'", enc.codec))?;

    // Opus on HLS-TS is not supported
    if codec == AudioCodec::Opus {
        return Err("Opus is not supported on HLS in this build".into());
    }

    let bitrate_kbps = enc.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

    #[cfg(not(feature = "video-thumbnail"))]
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
                 enable the `video-thumbnail` feature for in-process channel shuffle / SRC"
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

    #[cfg(feature = "video-thumbnail")]
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

        Ok(ResolvedAudioEncode {
            codec,
            bitrate_kbps,
            sample_rate: enc.sample_rate,
            channels: enc.channels,
            transcode,
        })
    }
}

/// Remux a TS segment with audio re-encoding. Dispatches to in-process
/// or subprocess based on the video-thumbnail feature.
async fn remux_segment_audio(
    segment: &[u8],
    enc: &ResolvedAudioEncode,
) -> Result<Vec<u8>, String> {
    #[cfg(feature = "video-thumbnail")]
    {
        // Run in-process on a blocking thread (C codec calls are synchronous)
        let segment_owned = segment.to_vec();
        let codec = enc.codec;
        let bitrate_kbps = enc.bitrate_kbps;
        let sample_rate = enc.sample_rate;
        let channels = enc.channels;
        let transcode = enc.transcode.clone();

        tokio::task::spawn_blocking(move || {
            remux_ts_audio_inprocess(
                &segment_owned,
                codec,
                bitrate_kbps,
                sample_rate,
                channels,
                transcode,
            )
        })
        .await
        .map_err(|e| format!("remux task panicked: {e}"))?
    }

    #[cfg(not(feature = "video-thumbnail"))]
    {
        remux_segment_via_ffmpeg(segment, &enc.ffmpeg_args).await
    }
}

// ════════════════════════════════════════════════════════════════════════
// In-process TS audio remuxer (video-thumbnail feature)
// ════════════════════════════════════════════════════════════════════════

#[cfg(feature = "video-thumbnail")]
fn remux_ts_audio_inprocess(
    segment: &[u8],
    codec: AudioCodec,
    bitrate_kbps: u32,
    sample_rate_override: Option<u32>,
    channels_override: Option<u8>,
    transcode: Option<super::audio_transcode::TranscodeJson>,
) -> Result<Vec<u8>, String> {
    use super::ts_parse::{ts_pid, ts_pusi, ts_has_payload, ts_payload_offset,
                          parse_pat_programs, PAT_PID};

    const TS_PACKET_SIZE: usize = 188;
    const TS_SYNC_BYTE: u8 = 0x47;

    // ── Pass 1: Find PIDs from PAT/PMT ──
    let mut pmt_pid: Option<u16> = None;
    let mut video_pid: Option<u16> = None;
    let mut audio_pid: Option<u16> = None;
    let mut audio_stream_type: u8 = 0;

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
                pmt_pid = Some(programs[0].1);
            }
        }

        if Some(pid) == pmt_pid && ts_pusi(pkt) && video_pid.is_none() {
            // Parse PMT for video + audio PIDs
            if let Some((vpid, apid, ast)) = parse_pmt_av_pids(pkt) {
                video_pid = Some(vpid);
                audio_pid = Some(apid);
                audio_stream_type = ast;
            }
        }

        if video_pid.is_some() && audio_pid.is_some() {
            break;
        }
        offset += TS_PACKET_SIZE;
    }

    let audio_pid = audio_pid.ok_or("no audio PID found in segment")?;
    let _video_pid = video_pid.ok_or("no video PID found in segment")?;
    let pmt_pid = pmt_pid.ok_or("no PMT PID found in segment")?;

    // ── Determine target audio stream type for PMT ──
    let target_stream_type: u8 = match codec {
        AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => 0x0F, // ADTS
        AudioCodec::Mp2 => 0x03, // MPEG audio
        AudioCodec::Ac3 => 0x81, // AC-3
        AudioCodec::Opus => return Err("Opus not supported on HLS".into()),
    };

    // ── Collect audio PES data and determine source format ──
    let mut audio_pes_list: Vec<(Vec<u8>, u64)> = Vec::new(); // (PES data, PTS)
    let mut pes_buffer: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut pes_started = false;

    offset = 0;
    while offset + TS_PACKET_SIZE <= segment.len() {
        let pkt = &segment[offset..offset + TS_PACKET_SIZE];
        offset += TS_PACKET_SIZE;

        if pkt[0] != TS_SYNC_BYTE || ts_pid(pkt) != audio_pid || !ts_has_payload(pkt) {
            continue;
        }

        let pusi = ts_pusi(pkt);
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            continue;
        }
        let payload = &pkt[payload_start..];

        if pusi {
            // Flush previous PES
            if pes_started && !pes_buffer.is_empty() {
                if let Some((es_data, pts)) = extract_pes_audio(&pes_buffer) {
                    audio_pes_list.push((es_data, pts));
                }
            }
            pes_buffer.clear();
            pes_buffer.extend_from_slice(payload);
            pes_started = true;
        } else if pes_started {
            pes_buffer.extend_from_slice(payload);
        }
    }
    // Flush last PES
    if pes_started && !pes_buffer.is_empty() {
        if let Some((es_data, pts)) = extract_pes_audio(&pes_buffer) {
            audio_pes_list.push((es_data, pts));
        }
    }

    if audio_pes_list.is_empty() {
        // No audio to re-encode — return original segment unchanged
        return Ok(segment.to_vec());
    }

    // ── Decode all audio PES → PCM ──
    let decoded_pcm = decode_audio_pes(&audio_pes_list, audio_stream_type)?;

    // ── Re-encode PCM → target codec ──
    let encoded_frames = encode_audio_pcm(
        &decoded_pcm,
        codec,
        bitrate_kbps,
        sample_rate_override,
        channels_override,
        transcode.as_ref(),
    )?;

    // ── Build output segment ──
    // Copy all non-audio TS packets, rewrite PMT, insert new audio packets
    let mut output = Vec::with_capacity(segment.len());
    let mut audio_cc: u8 = 0;
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
                    let pes = build_audio_pes(&ef.data, ef.pts);
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

        if pid == pmt_pid && ts_pusi(pkt) {
            // Rewrite PMT with new audio stream type
            let mut rewritten = pkt.to_vec();
            rewrite_pmt_audio_stream_type(&mut rewritten, audio_pid, target_stream_type);
            output.extend_from_slice(&rewritten);
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
        let pes = build_audio_pes(&ef.data, ef.pts);
        let ts_pkts = packetize_ts(audio_pid, &pes, &mut audio_cc);
        for ts_pkt in &ts_pkts {
            output.extend_from_slice(ts_pkt);
        }
    }

    Ok(output)
}

/// Parse PMT to find both video and audio PIDs.
/// Returns (video_pid, audio_pid, audio_stream_type).
#[cfg(feature = "video-thumbnail")]
fn parse_pmt_av_pids(pkt: &[u8]) -> Option<(u16, u16, u8)> {
    use super::ts_parse::ts_has_adaptation;
    const TS_PACKET_SIZE: usize = 188;

    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE { return None; }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 { return None; }

    let section_length = (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length = (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

    let mut video_pid = None;
    let mut audio_pid = None;
    let mut audio_st = 0u8;

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let st = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

        if st == 0x1B || st == 0x24 {
            video_pid = Some(es_pid);
        }
        if st == 0x0F || st == 0x03 || st == 0x04 || st == 0x81 || st == 0x06 {
            audio_pid = Some(es_pid);
            audio_st = st;
        }
        pos += 5 + es_info_len;
    }

    match (video_pid, audio_pid) {
        (Some(v), Some(a)) => Some((v, a, audio_st)),
        _ => None,
    }
}

/// Extract elementary stream data and PTS from a PES packet.
#[cfg(feature = "video-thumbnail")]
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
#[cfg(feature = "video-thumbnail")]
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
#[cfg(feature = "video-thumbnail")]
struct PcmFrame {
    planar: Vec<Vec<f32>>,
    pts: u64,
    sample_rate: u32,
    channels: u8,
}

/// Encoded audio frame ready for TS muxing.
#[cfg(feature = "video-thumbnail")]
struct RemuxEncodedFrame {
    data: Vec<u8>,
    pts: u64,
}

/// Decode audio PES list to PCM frames.
#[cfg(feature = "video-thumbnail")]
fn decode_audio_pes(
    pes_list: &[(Vec<u8>, u64)],
    audio_stream_type: u8,
) -> Result<Vec<PcmFrame>, String> {
    // Currently only AAC (0x0F) input is supported for decoding
    if audio_stream_type != 0x0F {
        return Err(format!(
            "unsupported input audio stream type 0x{audio_stream_type:02X} for re-encoding \
             (only AAC/ADTS 0x0F is supported as input)"
        ));
    }

    #[cfg(feature = "fdk-aac")]
    {
        let mut decoder = aac_audio::AacDecoder::open_adts()
            .map_err(|e| format!("AAC decoder init failed: {e}"))?;

        let mut pcm_frames = Vec::new();

        for (es_data, pts) in pes_list {
            // es_data may contain multiple concatenated ADTS frames
            let mut pos = 0;
            let mut frame_pts = *pts;

            while pos + 7 <= es_data.len() {
                // Check ADTS sync word
                if es_data[pos] != 0xFF || (es_data[pos + 1] & 0xF0) != 0xF0 {
                    break;
                }

                let protection_absent = (es_data[pos + 1] & 0x01) != 0;
                let header_len = if protection_absent { 7 } else { 9 };
                if pos + header_len > es_data.len() { break; }

                // Frame length from ADTS header
                let frame_len = (((es_data[pos + 3] & 0x03) as usize) << 11)
                    | ((es_data[pos + 4] as usize) << 3)
                    | ((es_data[pos + 5] as usize) >> 5);

                if frame_len < header_len || pos + frame_len > es_data.len() {
                    break;
                }

                let adts_frame = &es_data[pos..pos + frame_len];
                match decoder.decode_frame(adts_frame) {
                    Ok(decoded) => {
                        let sr = decoder.sample_rate().unwrap_or(48000);
                        let ch = decoder.channels().unwrap_or(2);
                        pcm_frames.push(PcmFrame {
                            planar: decoded.planar,
                            pts: frame_pts,
                            sample_rate: sr,
                            channels: ch,
                        });
                        // Advance PTS
                        if sr > 0 {
                            frame_pts += (decoded.frame_size as u64) * 90_000 / sr as u64;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("AAC decode error in HLS remux: {e}");
                    }
                }

                pos += frame_len;
            }
        }

        Ok(pcm_frames)
    }

    #[cfg(not(feature = "fdk-aac"))]
    {
        Err("AAC decoding requires the fdk-aac feature".into())
    }
}

/// Re-encode PCM frames to the target codec.
#[cfg(feature = "video-thumbnail")]
fn encode_audio_pcm(
    pcm_frames: &[PcmFrame],
    codec: AudioCodec,
    bitrate_kbps: u32,
    sample_rate_override: Option<u32>,
    channels_override: Option<u8>,
    transcode: Option<&super::audio_transcode::TranscodeJson>,
) -> Result<Vec<RemuxEncodedFrame>, String> {
    if pcm_frames.is_empty() {
        return Ok(Vec::new());
    }

    let source_sr = pcm_frames[0].sample_rate;
    let source_ch = pcm_frames[0].channels;

    // Resolve the encoder target. When transcode is set, it wins and folds
    // any audio_encode overrides as fallbacks; the planar transcoder then
    // aligns every decoded frame to (target_sr, target_ch) before the
    // encoder runs.
    let (target_sr, target_ch, mut transcoder) = if let Some(tj_in) = transcode {
        let merged = super::audio_transcode::TranscodeJson {
            sample_rate: tj_in.sample_rate.or(sample_rate_override),
            channels: tj_in.channels.or(channels_override),
            ..tj_in.clone()
        };
        let tc = super::audio_transcode::PlanarAudioTranscoder::new(
            source_sr, source_ch, &merged,
        )
        .map_err(|e| format!("transcode build failed: {e}"))?;
        (tc.out_sample_rate(), tc.out_channels(), Some(tc))
    } else {
        (
            sample_rate_override.unwrap_or(source_sr),
            channels_override.unwrap_or(source_ch),
            None,
        )
    };

    // For AAC codecs, use fdk-aac directly
    #[cfg(feature = "fdk-aac")]
    if matches!(codec, AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2) {
        return encode_audio_pcm_aac(
            pcm_frames,
            codec,
            bitrate_kbps,
            target_sr,
            target_ch,
            transcoder.as_mut(),
        );
    }

    // For Opus/MP2/AC-3, use the video-engine AudioEncoder
    let codec_type = match codec {
        AudioCodec::Mp2 => video_codec::AudioCodecType::Mp2,
        AudioCodec::Ac3 => video_codec::AudioCodecType::Ac3,
        AudioCodec::Opus => return Err("Opus not supported on HLS".into()),
        _ => return Err(format!("unsupported codec for in-process HLS remux: {}", codec.as_str())),
    };

    let config = video_codec::AudioEncoderConfig {
        codec: codec_type,
        sample_rate: target_sr,
        channels: target_ch,
        bitrate_kbps,
    };

    let mut encoder = video_engine::AudioEncoder::open(&config)
        .map_err(|e| format!("audio encoder open failed: {e}"))?;

    let frame_size = encoder.frame_size();
    let mut encoded_frames = Vec::new();
    let mut accumulator: Vec<Vec<f32>> = vec![Vec::new(); target_ch as usize];
    let mut pts_90k = pcm_frames.first().map(|f| f.pts).unwrap_or(0);

    for pcm in pcm_frames {
        // Apply the optional planar transcoder first; without one the
        // source PCM is fed in directly (matching the pre-transcode path).
        let planar_for_encoder: Vec<Vec<f32>> =
            if let Some(tc) = transcoder.as_mut() {
                match tc.process(&pcm.planar) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!("transcode failed in HLS remux: {e}");
                        continue;
                    }
                }
            } else {
                pcm.planar.clone()
            };
        // Accumulate samples (handle channel count mismatch by truncating/padding)
        for ch in 0..target_ch as usize {
            if ch < planar_for_encoder.len() {
                accumulator[ch].extend_from_slice(&planar_for_encoder[ch]);
            } else if !planar_for_encoder.is_empty() {
                // Pad missing channels with silence
                accumulator[ch].extend(
                    std::iter::repeat(0.0f32).take(planar_for_encoder[0].len()),
                );
            }
        }

        // Encode complete frames
        while accumulator[0].len() >= frame_size {
            let frame_planar: Vec<Vec<f32>> = accumulator
                .iter_mut()
                .map(|ch| ch.drain(..frame_size).collect())
                .collect();

            match encoder.encode_frame(&frame_planar) {
                Ok(frames) => {
                    for ef in frames {
                        encoded_frames.push(RemuxEncodedFrame {
                            data: ef.data.to_vec(),
                            pts: pts_90k,
                        });
                        let sr = encoder.sample_rate() as u64;
                        if sr > 0 {
                            pts_90k += (ef.num_samples as u64) * 90_000 / sr;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("audio encode error in HLS remux: {e}");
                }
            }
        }
    }

    // Flush encoder
    if let Ok(frames) = encoder.flush() {
        for ef in frames {
            encoded_frames.push(RemuxEncodedFrame {
                data: ef.data.to_vec(),
                pts: pts_90k,
            });
            let sr = encoder.sample_rate() as u64;
            if sr > 0 {
                pts_90k += (ef.num_samples as u64) * 90_000 / sr;
            }
        }
    }

    Ok(encoded_frames)
}

/// Re-encode PCM to AAC using fdk-aac.
#[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
fn encode_audio_pcm_aac(
    pcm_frames: &[PcmFrame],
    codec: AudioCodec,
    bitrate_kbps: u32,
    target_sr: u32,
    target_ch: u8,
    mut transcoder: Option<&mut super::audio_transcode::PlanarAudioTranscoder>,
) -> Result<Vec<RemuxEncodedFrame>, String> {
    let profile = match codec {
        AudioCodec::AacLc => aac_codec::AacProfile::AacLc,
        AudioCodec::HeAacV1 => aac_codec::AacProfile::HeAacV1,
        AudioCodec::HeAacV2 => aac_codec::AacProfile::HeAacV2,
        _ => unreachable!(),
    };

    let config = aac_codec::EncoderConfig {
        profile,
        sample_rate: target_sr,
        channels: target_ch,
        bitrate: bitrate_kbps * 1000,
        afterburner: true,
        sbr_signaling: aac_codec::SbrSignaling::default(),
        transport: aac_codec::TransportType::Adts,
    };

    let mut encoder = aac_audio::AacEncoder::open(&config)
        .map_err(|e| format!("AAC encoder open failed: {e}"))?;

    let frame_size = encoder.frame_size() as usize;
    let mut encoded_frames = Vec::new();
    let mut accumulator: Vec<Vec<f32>> = vec![Vec::new(); target_ch as usize];
    let mut pts_90k = pcm_frames.first().map(|f| f.pts).unwrap_or(0);

    for pcm in pcm_frames {
        let planar_for_encoder: Vec<Vec<f32>> =
            if let Some(tc) = transcoder.as_deref_mut() {
                match tc.process(&pcm.planar) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!("transcode failed in HLS AAC remux: {e}");
                        continue;
                    }
                }
            } else {
                pcm.planar.clone()
            };
        for ch in 0..target_ch as usize {
            if ch < planar_for_encoder.len() {
                accumulator[ch].extend_from_slice(&planar_for_encoder[ch]);
            } else if !planar_for_encoder.is_empty() {
                accumulator[ch].extend(
                    std::iter::repeat(0.0f32).take(planar_for_encoder[0].len()),
                );
            }
        }

        while accumulator[0].len() >= frame_size {
            let frame_planar: Vec<Vec<f32>> = accumulator
                .iter_mut()
                .map(|ch| ch.drain(..frame_size).collect())
                .collect();

            match encoder.encode_frame(&frame_planar) {
                Ok(encoded) => {
                    encoded_frames.push(RemuxEncodedFrame {
                        data: encoded.bytes,
                        pts: pts_90k,
                    });
                    let sr = target_sr as u64;
                    if sr > 0 {
                        pts_90k += (encoded.num_samples as u64) * 90_000 / sr;
                    }
                }
                Err(e) => {
                    tracing::debug!("AAC encode error in HLS remux: {e}");
                }
            }
        }
    }

    Ok(encoded_frames)
}

/// Build a PES packet wrapping an audio frame.
#[cfg(feature = "video-thumbnail")]
fn build_audio_pes(audio_data: &[u8], pts: u64) -> Vec<u8> {
    // PES header: 0x000001 + stream_id(0xC0) + length + flags + PTS
    let pes_header_len = 14; // 3 + 1 + 2 + 2 + 1 + 5
    let pes_len = 3 + 5 + audio_data.len(); // optional header + PTS + payload

    let mut pes = Vec::with_capacity(pes_header_len + audio_data.len());
    // Start code
    pes.push(0x00);
    pes.push(0x00);
    pes.push(0x01);
    // Stream ID: audio
    pes.push(0xC0);
    // PES packet length (0 = unbounded for video, but for audio we set it)
    let pkt_len = pes_len as u16;
    pes.push((pkt_len >> 8) as u8);
    pes.push(pkt_len as u8);
    // Flags: marker bits (10), PTS present
    pes.push(0x80); // 10 00 0000
    pes.push(0x80); // PTS_DTS_flags = 10 (PTS only)
    // PES header data length
    pes.push(5); // 5 bytes for PTS

    // PTS (5 bytes)
    let pts = pts & 0x1_FFFF_FFFF; // 33 bits
    pes.push(0x21 | (((pts >> 30) as u8) & 0x0E));
    pes.push((pts >> 22) as u8);
    pes.push(0x01 | (((pts >> 15) as u8) & 0xFE));
    pes.push((pts >> 7) as u8);
    pes.push(0x01 | (((pts as u8) & 0x7F) << 1));

    // Audio payload
    pes.extend_from_slice(audio_data);

    pes
}

/// Packetize a PES payload into 188-byte TS packets.
#[cfg(feature = "video-thumbnail")]
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

/// Rewrite the audio stream_type in a PMT TS packet and recalculate CRC.
#[cfg(feature = "video-thumbnail")]
fn rewrite_pmt_audio_stream_type(pkt: &mut [u8], audio_pid: u16, new_stream_type: u8) {
    use super::ts_parse::{ts_has_adaptation, mpeg2_crc32};
    const TS_PACKET_SIZE: usize = 188;

    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE { return; }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;

    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 { return; }

    let section_start = offset;
    let section_length = (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length = (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length).min(TS_PACKET_SIZE).saturating_sub(4);

    // Find and rewrite the audio stream entry
    let mut pos = data_start;
    while pos + 5 <= data_end {
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

        if es_pid == audio_pid {
            pkt[pos] = new_stream_type;
        }

        pos += 5 + es_info_len;
    }

    // Recalculate CRC32 over the section (excluding the CRC bytes themselves)
    let crc_offset = section_start + 3 + section_length - 4;
    if crc_offset + 4 <= TS_PACKET_SIZE {
        let crc = mpeg2_crc32(&pkt[section_start..crc_offset]);
        pkt[crc_offset] = (crc >> 24) as u8;
        pkt[crc_offset + 1] = (crc >> 16) as u8;
        pkt[crc_offset + 2] = (crc >> 8) as u8;
        pkt[crc_offset + 3] = crc as u8;
    }
}

// ════════════════════════════════════════════════════════════════════════
// ffmpeg subprocess fallback (when video-thumbnail feature is disabled)
// ════════════════════════════════════════════════════════════════════════

#[cfg(not(feature = "video-thumbnail"))]
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
#[cfg(not(feature = "video-thumbnail"))]
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
#[cfg(not(feature = "video-thumbnail"))]
mod tests {
    use super::*;

    fn make_enc(codec: &str) -> AudioEncodeConfig {
        AudioEncodeConfig {
            codec: codec.into(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
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
