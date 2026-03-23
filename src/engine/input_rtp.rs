// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtpInputConfig;
use crate::fec::decoder::FecDecoder;
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::rtp_parse::{is_likely_rtp, parse_rtp_sequence_number, parse_rtp_timestamp};
use crate::util::socket::bind_udp_input;
use crate::util::time::now_us;

use super::packet::{MAX_RTP_PACKET_SIZE, RtpPacket};

/// Spawn a task that receives RTP packets from a UDP socket and
/// publishes them to a broadcast channel for fan-out to outputs.
/// If FEC decode is configured, lost packets may be recovered.
pub fn spawn_rtp_input(
    config: RtpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = rtp_input_loop(config, broadcast_tx, stats, cancel).await {
            tracing::error!("RTP input task exited with error: {e}");
        }
    })
}

/// Simple token-bucket rate limiter.
///
/// Tokens are replenished based on elapsed time. Each byte consumed costs
/// one token. If insufficient tokens are available, the packet is rejected.
/// All operations are integer arithmetic — no system calls or allocations.
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    rate_bytes_per_us: f64,
    last_refill_us: u64,
}

impl TokenBucket {
    fn new(max_bitrate_mbps: f64) -> Self {
        let rate_bytes_per_sec = max_bitrate_mbps * 1_000_000.0 / 8.0;
        let rate_bytes_per_us = rate_bytes_per_sec / 1_000_000.0;
        // Allow burst of up to 10ms worth of data
        let max_tokens = rate_bytes_per_sec * 0.01;
        Self {
            tokens: max_tokens,
            max_tokens,
            rate_bytes_per_us,
            last_refill_us: now_us(),
        }
    }

    /// Returns true if the packet is allowed, false if rate-limited.
    fn try_consume(&mut self, bytes: usize) -> bool {
        let now = now_us();
        let elapsed = now.saturating_sub(self.last_refill_us);
        self.last_refill_us = now;

        // Replenish tokens
        self.tokens += elapsed as f64 * self.rate_bytes_per_us;
        if self.tokens > self.max_tokens {
            self.tokens = self.max_tokens;
        }

        let cost = bytes as f64;
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

/// Core receive loop for RTP input.
///
/// Binds a UDP socket to `config.bind_addr` (with optional multicast join via
/// `config.interface_addr`), then enters a select loop that:
///
/// - Reads datagrams from the socket.
/// - Applies ingress filters (source IP, payload type, rate limit) per RP 2129.
/// - Filters non-RTP packets (e.g., stray RTCP).
/// - Tracks sequence numbers and counts gaps as packet loss.
/// - Optionally passes packets through a [`FecDecoder`] pipeline that can
///   recover lost media packets from FEC (SMPTE 2022-1) redundancy data.
/// - Publishes each resulting [`RtpPacket`] to the broadcast channel.
///
/// The loop exits when the cancellation token fires.
async fn rtp_input_loop(
    config: RtpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let socket = bind_udp_input(&config.bind_addr, config.interface_addr.as_deref()).await?;

    tracing::info!("RTP input started on {}", config.bind_addr);

    // ── Ingress filters (RP 2129) — built once, checked per-packet ──

    // C5: Source IP allow-list (pre-parsed into HashSet for O(1) lookup)
    let source_filter: Option<HashSet<IpAddr>> = config.allowed_sources.as_ref().map(|sources| {
        let set: HashSet<IpAddr> = sources
            .iter()
            .filter_map(|s| s.parse::<IpAddr>().ok())
            .collect();
        tracing::info!("Source IP filter enabled: {} allowed addresses", set.len());
        set
    });

    // U4: RTP payload type allow-list
    let pt_filter: Option<Vec<u8>> = config.allowed_payload_types.clone();
    if let Some(ref pts) = pt_filter {
        tracing::info!("Payload type filter enabled: {:?}", pts);
    }

    // C7: Per-flow ingress rate limiter
    let mut rate_limiter: Option<TokenBucket> = config.max_bitrate_mbps.map(|rate| {
        tracing::info!("Rate limit enabled: {rate} Mbps");
        TokenBucket::new(rate)
    });

    // Optional FEC decoder.
    let mut fec_decoder = config.fec_decode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC decode enabled: L={} D={}",
            fec_config.columns,
            fec_config.rows
        );
        FecDecoder::new(
            fec_config.columns,
            fec_config.rows,
        )
    });

    let mut buf = vec![0u8; MAX_RTP_PACKET_SIZE + 100]; // extra headroom
    let mut last_seq: Option<u16> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTP input on {} stopping (cancelled)", config.bind_addr);
                break;
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, src)) => {
                        let data = &buf[..len];

                        // ── C5: Source IP filter ──
                        if let Some(ref allowed) = source_filter {
                            if !allowed.contains(&src.ip()) {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }

                        if !is_likely_rtp(data) {
                            continue;
                        }

                        // ── U4: Payload type filter ──
                        if let Some(ref pts) = pt_filter {
                            let pt = data[1] & 0x7F;
                            if !pts.contains(&pt) {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }

                        // ── C7: Rate limit ──
                        if let Some(ref mut limiter) = rate_limiter {
                            if !limiter.try_consume(len) {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }

                        let seq = parse_rtp_sequence_number(data).unwrap_or(0);
                        let ts = parse_rtp_timestamp(data).unwrap_or(0);

                        // Detect sequence gaps for loss counting.
                        if let Some(prev) = last_seq {
                            let expected = prev.wrapping_add(1);
                            if seq != expected {
                                let gap = seq.wrapping_sub(expected) as u64;
                                if gap > 0 && gap < 1000 {
                                    stats.input_loss.fetch_add(gap, Ordering::Relaxed);
                                }
                            }
                        }
                        last_seq = Some(seq);

                        // Update stats
                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(len as u64, Ordering::Relaxed);

                        let bytes_data = Bytes::copy_from_slice(data);

                        if let Some(ref mut decoder) = fec_decoder {
                            let packets = decoder.process_media(seq, ts, &bytes_data);
                            for pkt in packets {
                                let _ = broadcast_tx.send(pkt);
                            }
                        } else {
                            let packet = RtpPacket {
                                data: bytes_data,
                                sequence_number: seq,
                                rtp_timestamp: ts,
                                recv_time_us: now_us(),
                                is_raw_ts: false,
                            };
                            let _ = broadcast_tx.send(packet);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("RTP input recv error: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}
