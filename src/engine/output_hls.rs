// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{AudioEncodeConfig, HlsOutputConfig};
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::OutputStatsAccumulator;

use super::audio_encode::{AudioCodec, check_ffmpeg_available};
use super::packet::RtpPacket;
use super::ts_program_filter::TsProgramFilter;

/// Maximum time we'll wait for ffmpeg to re-mux a single segment.
/// Segments are typically 2-6 seconds, so 30 s is a generous safety bound.
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

    tokio::spawn(async move {
        if let Err(e) = hls_output_loop(&config, &mut rx, output_stats, cancel, &event_sender, &flow_id).await {
            tracing::error!("HLS output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                "hls",
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

    // Resolve audio_encode at startup. When set, ffmpeg must be present in
    // PATH or the output refuses to start (the operator asked for re-encode
    // and we can't deliver). When unset, behaviour is unchanged.
    let remux_args: Option<Vec<String>> = match &config.audio_encode {
        Some(enc) => match build_remux_args(enc) {
            Ok(args) => {
                if !check_ffmpeg_available() {
                    let msg = format!(
                        "HLS output '{}': audio_encode requires ffmpeg in PATH but it is not installed; refusing to start output",
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
                Some(args)
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

                            // Run the segment through ffmpeg if audio_encode
                            // is configured. ffmpeg copies the video stream
                            // and re-encodes the audio per the configured
                            // codec/bitrate, then re-muxes a fresh TS.
                            // On ffmpeg failure we skip this segment with a
                            // warning — the next segment may succeed.
                            let segment_data = if let Some(ref args) = remux_args {
                                match remux_segment_via_ffmpeg(&raw_segment, args).await {
                                    Ok(data) => data,
                                    Err(e) => {
                                        tracing::warn!(
                                            "HLS output '{}': segment {} ffmpeg remux failed: {e}; skipping",
                                            config.id, segment_seq
                                        );
                                        event_sender.emit_flow(
                                            EventSeverity::Warning,
                                            crate::manager::events::category::AUDIO_ENCODE,
                                            format!("HLS output '{}': segment {} remux failed: {e}", config.id, segment_seq),
                                            flow_id,
                                        );
                                        // Reset for the next segment but
                                        // don't bump segment_seq (we'll
                                        // re-use it next iteration).
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
                                        "hls",
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

// ── audio_encode: per-segment ffmpeg remux ─────────────────────────────────

/// Build the ffmpeg argv for the HLS audio re-encode case. The encoder
/// reads MPEG-TS bytes on stdin, copies all video streams as-is, re-encodes
/// the audio per the configured codec/bitrate/SR/channels, and writes a
/// fresh MPEG-TS file to stdout. ffmpeg-as-remuxer rather than wiring the
/// engine::audio_encode::AudioEncoder PCM pipeline avoids re-implementing a
/// TS muxer for MP2/AC-3 (the existing engine/rtmp/ts_mux.rs only knows
/// AAC). Per-segment fork is acceptable because HLS segments are 2-6 s
/// long and ffmpeg startup is small relative to that.
fn build_remux_args(enc: &AudioEncodeConfig) -> Result<Vec<String>, String> {
    let codec = AudioCodec::parse(&enc.codec)
        .ok_or_else(|| format!("unknown codec '{}'", enc.codec))?;

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostats".into(),
        "-loglevel".into(),
        "warning".into(),
        // Read MPEG-TS bytes from stdin.
        "-f".into(),
        "mpegts".into(),
        "-i".into(),
        "pipe:0".into(),
        // Copy video streams unchanged.
        "-c:v".into(),
        "copy".into(),
    ];

    let bitrate = format!("{}k",
        enc.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps()));

    match codec {
        AudioCodec::AacLc => {
            args.extend([
                "-c:a".into(), "aac".into(),
                "-profile:a".into(), "aac_low".into(),
                "-b:a".into(), bitrate,
            ]);
        }
        AudioCodec::HeAacV1 => {
            // HE-AAC v1 / v2 encoding is only reliably supported by
            // `libfdk_aac`. ffmpeg's native `aac` encoder hard-rejects
            // these profiles with "Profile not supported!" and `aac_at`
            // (Apple AudioToolbox) silently downgrades to LC, neither of
            // which is acceptable for a broadcast contribution chain. We
            // refuse to build args when libfdk_aac isn't present so the
            // operator gets a clear error at output startup instead of
            // cryptic per-segment ffmpeg `-22` failures.
            if !crate::engine::audio_encode::check_libfdk_aac_available() {
                return Err(
                    "he_aac_v1 requires an ffmpeg build with libfdk_aac (the native ffmpeg \
                     `aac` encoder does not support the aac_he profile). Install ffmpeg with \
                     `--enable-libfdk-aac` (e.g. via the homebrew-ffmpeg/ffmpeg tap on macOS) \
                     and restart the edge node, or switch this output to `aac_lc`."
                        .into(),
                );
            }
            args.extend([
                "-c:a".into(), "libfdk_aac".into(),
                "-profile:a".into(), "aac_he".into(),
                "-b:a".into(), bitrate,
            ]);
        }
        AudioCodec::HeAacV2 => {
            if !crate::engine::audio_encode::check_libfdk_aac_available() {
                return Err(
                    "he_aac_v2 requires an ffmpeg build with libfdk_aac (the native ffmpeg \
                     `aac` encoder does not support the aac_he_v2 profile). Install ffmpeg with \
                     `--enable-libfdk-aac` (e.g. via the homebrew-ffmpeg/ffmpeg tap on macOS) \
                     and restart the edge node, or switch this output to `aac_lc`."
                        .into(),
                );
            }
            args.extend([
                "-c:a".into(), "libfdk_aac".into(),
                "-profile:a".into(), "aac_he_v2".into(),
                "-b:a".into(), bitrate,
            ]);
        }
        AudioCodec::Mp2 => {
            args.extend([
                "-c:a".into(), "mp2".into(),
                "-b:a".into(), bitrate,
            ]);
        }
        AudioCodec::Ac3 => {
            args.extend([
                "-c:a".into(), "ac3".into(),
                "-b:a".into(), bitrate,
            ]);
        }
        AudioCodec::Opus => {
            // Validation should have rejected this — Opus on HLS-TS is
            // out of scope for v1.
            return Err("Opus is not supported on HLS in this build".into());
        }
    }

    if let Some(sr) = enc.sample_rate {
        args.extend(["-ar".into(), sr.to_string()]);
    }
    if let Some(ch) = enc.channels {
        args.extend(["-ac".into(), ch.to_string()]);
    }

    args.extend([
        // Output: MPEG-TS to stdout.
        "-f".into(),
        "mpegts".into(),
        "pipe:1".into(),
    ]);

    Ok(args)
}

/// Run ffmpeg as a one-shot remuxer over a single HLS segment. Pipes
/// `segment` to stdin and reads the re-muxed TS from stdout. Times out
/// after [`HLS_REMUX_TIMEOUT`] to catch a hung ffmpeg.
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
        return Err(format!(
            "ffmpeg exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    if output.stdout.is_empty() {
        return Err("ffmpeg produced no output".to_string());
    }

    Ok(output.stdout)
}

#[cfg(test)]
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
