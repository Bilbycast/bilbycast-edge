// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::HlsOutputConfig;
use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;

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

    let mut segment_buf: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024); // 2 MB initial
    let mut segment_start_us: Option<u64> = None;
    let mut segment_seq: u64 = 0;

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
                        let payload = if packet.data.len() > RTP_HEADER_MIN {
                            &packet.data[RTP_HEADER_MIN..]
                        } else {
                            // Packet too small to contain TS data, skip.
                            continue;
                        };

                        // Initialise segment timing from the first packet.
                        let start = segment_start_us.get_or_insert(packet.recv_time_us);
                        let elapsed_us = packet.recv_time_us.saturating_sub(*start);

                        segment_buf.extend_from_slice(payload);

                        // Check if we should cut a segment.
                        if elapsed_us >= segment_duration_us && !segment_buf.is_empty() {
                            let duration_secs = elapsed_us as f64 / 1_000_000.0;
                            let seq = segment_seq;
                            segment_seq += 1;

                            // Upload the segment (non-blocking: log and continue on failure).
                            let segment_data = std::mem::replace(
                                &mut segment_buf,
                                Vec::with_capacity(2 * 1024 * 1024),
                            );
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
