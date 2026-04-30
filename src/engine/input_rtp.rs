// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtpInputConfig;
use crate::fec::decoder::{is_fec_packet, FecDecoder};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::redundancy::merger::{ActiveLeg, BufferedHitlessMerger, HitlessMerger};

/// Merger strategy for the 2022-7 redundant RTP path. The legacy bare
/// path is kept as the default for backwards compatibility — operators
/// opt into the industry-standard buffered path by setting
/// `path_differential_ms` on the `RtpRedundancyConfig`.
enum RedundantMerger {
    /// Stateless dedup (`HitlessMerger`). Lower latency on symmetric
    /// paths but degrades on asymmetric paths because the slower leg
    /// can't fill a gap that the faster leg has already advanced past.
    Bare {
        merger: HitlessMerger,
        prev_active_leg: ActiveLeg,
    },
    /// Buffered SMPTE 2022-7 (`BufferedHitlessMerger`). Constant
    /// `max_path_diff` emission latency in exchange for proper
    /// hitless behaviour across asymmetric paths.
    Buffered(BufferedHitlessMerger<RtpPacket>),
}

impl RedundantMerger {
    fn ingest(
        &mut self,
        packet: RtpPacket,
        leg: ActiveLeg,
        now: std::time::Instant,
        stats: &FlowStatsAccumulator,
    ) -> Vec<(ActiveLeg, RtpPacket)> {
        match self {
            Self::Bare {
                merger,
                prev_active_leg,
            } => {
                if let Some(chosen) = merger.try_merge(packet.sequence_number, leg) {
                    if *prev_active_leg != ActiveLeg::None && chosen != *prev_active_leg {
                        stats.redundancy_switches.fetch_add(1, Ordering::Relaxed);
                    }
                    *prev_active_leg = chosen;
                    vec![(chosen, packet)]
                } else {
                    Vec::new()
                }
            }
            Self::Buffered(m) => {
                let seq = packet.sequence_number;
                m.ingest(seq, leg, packet, now)
            }
        }
    }

    fn drain_expired(&mut self, now: std::time::Instant) -> Vec<(ActiveLeg, RtpPacket)> {
        match self {
            Self::Bare { .. } => Vec::new(),
            Self::Buffered(m) => m.drain_expired(now),
        }
    }

    fn next_deadline(&self) -> Option<std::time::Instant> {
        match self {
            Self::Bare { .. } => None,
            Self::Buffered(m) => m.next_deadline(),
        }
    }

    fn observe_ssrc(&mut self, leg: ActiveLeg, ssrc: u32) {
        if let Self::Buffered(m) = self {
            m.observe_ssrc(leg, ssrc);
        }
    }
}
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::rtp_parse::{is_likely_rtp, parse_rtp_sequence_number, parse_rtp_timestamp};
use crate::util::socket::bind_udp_input;
use crate::util::time::now_us;

use super::input_transcode::{publish_input_packet, InputTranscoder};
use super::packet::{MAX_RTP_PACKET_SIZE, RtpPacket};

/// Spawn a task that receives RTP packets from a UDP socket and
/// publishes them to a broadcast channel for fan-out to outputs.
/// If FEC decode is configured, lost packets may be recovered.
/// If redundancy is configured, two UDP legs are merged using SMPTE 2022-7.
pub fn spawn_rtp_input(
    config: RtpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
    force_idr: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("RTP input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("RTP input: transcode setup failed, passthrough: {e}");
                None
            }
        };
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_ref(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
        );
        let result = if config.redundancy.is_some() {
            rtp_input_redundant_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder).await
        } else {
            rtp_input_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder).await
        };
        if let Err(e) = result {
            tracing::error!("RTP input task exited with error: {e}");
            event_sender.emit_flow(EventSeverity::Critical, category::FLOW, format!("Flow input lost: {e}"), &flow_id);
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
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    let socket = match bind_udp_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
    )
    .await
    {
        Ok(s) => {
            events.emit_flow_with_details(
                EventSeverity::Info,
                category::RTP,
                format!("RTP input listening on {}", config.bind_addr),
                flow_id,
                serde_json::json!({"bind_addr": config.bind_addr}),
            );
            s
        }
        Err(e) => {
            use crate::manager::events::{BindProto, BindScope};
            let scope = BindScope::flow(flow_id);
            if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                events.emit_port_conflict("RTP input", &config.bind_addr, BindProto::Udp, scope, &e);
            } else {
                events.emit_bind_failed("RTP input", &config.bind_addr, BindProto::Udp, scope, &e);
            }
            return Err(e);
        }
    };

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
        FecDecoder::new(fec_config.columns, fec_config.rows)
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

                        // ── FEC repair packet (SMPTE 2022-1, bilbycast framing) ──
                        // Route to the decoder before the RTP-shape check so the
                        // repair datagram isn't silently dropped. Emits any
                        // reconstructed media packets directly to broadcast.
                        if is_fec_packet(data) {
                            if let Some(ref mut decoder) = fec_decoder {
                                let recovered = decoder.process_fec(data);
                                for pkt in recovered {
                                    stats.fec_recovered.fetch_add(1, Ordering::Relaxed);
                                    publish_input_packet(transcoder, &broadcast_tx, pkt);
                                }
                            }
                            continue;
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

                        // Update stats (always counted so bandwidth monitor sees real traffic)
                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(len as u64, Ordering::Relaxed);

                        // Bandwidth limit enforcement: drop packet if flow is blocked
                        if stats.bandwidth_blocked.load(Ordering::Relaxed) {
                            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }

                        let bytes_data = Bytes::copy_from_slice(data);

                        if let Some(ref mut decoder) = fec_decoder {
                            let packets = decoder.process_media(seq, ts, &bytes_data);
                            for pkt in packets {
                                publish_input_packet(transcoder, &broadcast_tx, pkt);
                            }
                        } else {
                            let packet = RtpPacket {
                                data: bytes_data,
                                sequence_number: seq,
                                rtp_timestamp: ts,
                                recv_time_us: now_us(),
                                is_raw_ts: false,
                                // Single-leg RTP carries a wire seq even
                                // outside 2022-7. Stamp it so a downstream
                                // Hitless slot that pairs this input with
                                // a peer (also wire-seq-bearing) can run
                                // the seq-aware path.
                                upstream_seq: Some(seq),
                                upstream_leg_id: None,
                            };
                            publish_input_packet(transcoder, &broadcast_tx, packet);
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

// ---------------------------------------------------------------------------
// SMPTE 2022-7 dual-leg RTP input
// ---------------------------------------------------------------------------

/// Dual-leg RTP input with SMPTE 2022-7 hitless merge.
///
/// Binds two UDP sockets (leg 1 and leg 2), applies the same ingress filters
/// to both, optionally runs per-leg FEC decoding, then merges de-duplicated
/// packets via [`HitlessMerger`] before publishing to the broadcast channel.
///
/// This supports all RTP-based formats: RTP/MPEG-TS (SMPTE 2022-2),
/// VSF TR-07, and generic RTP streams. The merge uses the native RTP
/// sequence number present in every packet header.
async fn rtp_input_redundant_loop(
    config: RtpInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    let socket1 = match bind_udp_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
    )
    .await
    {
        Ok(s) => {
            events.emit_flow_with_details(
                EventSeverity::Info,
                category::RTP,
                format!("RTP input leg 1 listening on {}", config.bind_addr),
                flow_id,
                serde_json::json!({"bind_addr": config.bind_addr, "leg": 1}),
            );
            s
        }
        Err(e) => {
            use crate::manager::events::{BindProto, BindScope};
            let scope = BindScope::flow(flow_id);
            if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                events.emit_port_conflict("RTP input leg 1", &config.bind_addr, BindProto::Udp, scope, &e);
            } else {
                events.emit_bind_failed("RTP input leg 1", &config.bind_addr, BindProto::Udp, scope, &e);
            }
            return Err(e);
        }
    };
    let socket2 = match bind_udp_input(
        &redundancy.bind_addr,
        redundancy.interface_addr.as_deref(),
        redundancy.source_addr.as_deref(),
    )
    .await
    {
        Ok(s) => {
            events.emit_flow_with_details(
                EventSeverity::Info,
                category::RTP,
                format!("RTP input leg 2 listening on {}", redundancy.bind_addr),
                flow_id,
                serde_json::json!({"bind_addr": redundancy.bind_addr, "leg": 2}),
            );
            s
        }
        Err(e) => {
            use crate::manager::events::{BindProto, BindScope};
            let scope = BindScope::flow(flow_id);
            if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                events.emit_port_conflict("RTP input leg 2", &redundancy.bind_addr, BindProto::Udp, scope, &e);
            } else {
                events.emit_bind_failed("RTP input leg 2", &redundancy.bind_addr, BindProto::Udp, scope, &e);
            }
            return Err(e);
        }
    };

    tracing::info!(
        "RTP input started with 2022-7 redundancy: leg1={} leg2={}",
        config.bind_addr,
        redundancy.bind_addr
    );

    // ── Shared ingress filters (applied to both legs) ──

    let source_filter: Option<HashSet<IpAddr>> = config.allowed_sources.as_ref().map(|sources| {
        let set: HashSet<IpAddr> = sources
            .iter()
            .filter_map(|s| s.parse::<IpAddr>().ok())
            .collect();
        tracing::info!("Source IP filter enabled: {} allowed addresses", set.len());
        set
    });

    let pt_filter: Option<Vec<u8>> = config.allowed_payload_types.clone();
    if let Some(ref pts) = pt_filter {
        tracing::info!("Payload type filter enabled: {:?}", pts);
    }

    // Rate limiter is shared across both legs (combined rate)
    let mut rate_limiter: Option<TokenBucket> = config.max_bitrate_mbps.map(|rate| {
        tracing::info!("Rate limit enabled: {rate} Mbps (combined across both legs)");
        TokenBucket::new(rate)
    });

    // Per-leg FEC decoders (each leg has independent FEC repair data)
    let mut fec_decoder_leg1 = config.fec_decode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC decode enabled (leg 1): L={} D={}",
            fec_config.columns,
            fec_config.rows
        );
        FecDecoder::new(fec_config.columns, fec_config.rows)
    });
    let mut fec_decoder_leg2 = config.fec_decode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC decode enabled (leg 2): L={} D={}",
            fec_config.columns,
            fec_config.rows
        );
        FecDecoder::new(fec_config.columns, fec_config.rows)
    });

    // Pick merger strategy. When `path_differential_ms` is set on the
    // redundancy config, use the buffered SMPTE 2022-7 merger (industry
    // standard — emits at constant `pd_ms` latency, hitlessly fills
    // single-leg loss within that window). Otherwise fall back to the
    // legacy bare dedup path for backwards compat.
    let mut merger = match config.redundancy.as_ref().and_then(|r| r.path_differential_ms) {
        Some(pd_ms) => {
            tracing::info!(
                "RTP input (2022-7): buffered merger enabled (path_differential = {pd_ms} ms)"
            );
            RedundantMerger::Buffered(BufferedHitlessMerger::new(
                std::time::Duration::from_millis(pd_ms as u64),
            ))
        }
        None => RedundantMerger::Bare {
            merger: HitlessMerger::new(),
            prev_active_leg: ActiveLeg::None,
        },
    };

    let mut buf1 = vec![0u8; MAX_RTP_PACKET_SIZE + 100];
    let mut buf2 = vec![0u8; MAX_RTP_PACKET_SIZE + 100];
    // Far-future placeholder so the drain-timer arm doesn't fire when
    // the buffer is empty (or when in Bare mode).
    let far_future = std::time::Instant::now() + std::time::Duration::from_secs(86_400);

    // Track the previous per-leg health so we can fire transition events
    // (`leg_dropped` / `leg_recovered`) when state changes. Also track
    // the previously-observed SSRC mismatch counter so we fire one
    // event per new mismatch event, not on every drain.
    let mut prev_leg_health = [
        crate::redundancy::merger::LegHealth::Unknown,
        crate::redundancy::merger::LegHealth::Unknown,
    ];
    let mut prev_ssrc_mismatch: u64 = 0;

    loop {
        let drain_at = merger.next_deadline().unwrap_or(far_future);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!("RTP input (2022-7) stopping (cancelled)");
                break;
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(drain_at)) => {
                let now = std::time::Instant::now();
                for (chosen, pkt) in merger.drain_expired(now) {
                    publish_emitted_redundant_packet(
                        chosen, pkt, &broadcast_tx, &stats, transcoder,
                    );
                }
                refresh_buffered_hitless_snapshot(&merger, &stats);
                // Health-transition + SSRC-mismatch events. Fired only
                // on edge transitions so the manager UI gets one event
                // per state change rather than a per-tick storm.
                if let RedundantMerger::Buffered(ref m) = merger {
                    let now_health = m.leg_health();
                    for (idx, (prev, new)) in
                        prev_leg_health.iter().zip(now_health.iter()).enumerate()
                    {
                        if prev != new {
                            emit_leg_health_event(
                                events,
                                flow_id,
                                idx,
                                *prev,
                                *new,
                                &now_health,
                            );
                        }
                    }
                    prev_leg_health = now_health;
                    let mismatch =
                        m.stats.ssrc_mismatch.load(std::sync::atomic::Ordering::Relaxed);
                    if mismatch > prev_ssrc_mismatch {
                        events.emit_flow_with_details(
                            crate::manager::events::EventSeverity::Warning,
                            crate::manager::events::category::REDUNDANCY,
                            "SMPTE 2022-7: leg SSRC mismatch — both legs must \
                             carry byte-identical RTP including SSRC",
                            flow_id,
                            serde_json::json!({
                                "error_code": "redundancy_ssrc_mismatch",
                                "mismatch_count": mismatch,
                            }),
                        );
                        prev_ssrc_mismatch = mismatch;
                    }
                }
            }
            result = socket1.recv_from(&mut buf1) => {
                if let Ok((len, src)) = result {
                    process_redundant_rtp_packet(
                        &buf1[..len], src.ip(), ActiveLeg::Leg1,
                        &source_filter, &pt_filter, &mut rate_limiter,
                        &mut fec_decoder_leg1, &mut merger,
                        &broadcast_tx, &stats, transcoder,
                    );
                }
            }
            result = socket2.recv_from(&mut buf2) => {
                if let Ok((len, src)) = result {
                    process_redundant_rtp_packet(
                        &buf2[..len], src.ip(), ActiveLeg::Leg2,
                        &source_filter, &pt_filter, &mut rate_limiter,
                        &mut fec_decoder_leg2, &mut merger,
                        &broadcast_tx, &stats, transcoder,
                    );
                }
            }
        }
    }

    Ok(())
}

/// Emit a `leg_dropped` / `leg_recovered` event when a leg's health
/// transitions in the buffered SMPTE 2022-7 merger.
fn emit_leg_health_event(
    events: &EventSender,
    flow_id: &str,
    leg_idx: usize,
    prev: crate::redundancy::merger::LegHealth,
    new: crate::redundancy::merger::LegHealth,
    overall: &[crate::redundancy::merger::LegHealth; 2],
) {
    use crate::manager::events::{EventSeverity, category};
    use crate::redundancy::merger::LegHealth;
    let leg_label = match leg_idx {
        0 => "leg 1",
        _ => "leg 2",
    };
    let both_down =
        overall.iter().all(|h| matches!(h, LegHealth::Down | LegHealth::Unknown));
    let (severity, error_code, message) = match (prev, new) {
        (_, LegHealth::Down) if both_down => (
            EventSeverity::Critical,
            "redundancy_both_legs_down",
            format!(
                "SMPTE 2022-7: both legs lost (last transition: {leg_label} → down) — \
                 input has no source"
            ),
        ),
        (_, LegHealth::Down) => (
            EventSeverity::Warning,
            "redundancy_leg_dropped",
            format!("SMPTE 2022-7: {leg_label} dropped — surviving on the other leg"),
        ),
        (LegHealth::Down, LegHealth::Up) => (
            EventSeverity::Info,
            "redundancy_leg_recovered",
            format!("SMPTE 2022-7: {leg_label} recovered — both legs healthy"),
        ),
        _ => return,
    };
    events.emit_flow_with_details(
        severity,
        category::REDUNDANCY,
        message,
        flow_id,
        serde_json::json!({
            "error_code": error_code,
            "leg": leg_idx + 1,
            "prev_health": format!("{prev:?}"),
            "new_health": format!("{new:?}"),
            "leg1_health": format!("{:?}", overall[0]),
            "leg2_health": format!("{:?}", overall[1]),
        }),
    );
}

/// Refresh the per-flow `buffered_hitless` stats slot. Called on every
/// drain firing — cheap (single write-lock + atomic snapshot) and gives
/// the manager UI a 1-Hz-fresh view of path differential + per-leg
/// health without polling.
fn refresh_buffered_hitless_snapshot(
    merger: &RedundantMerger,
    stats: &FlowStatsAccumulator,
) {
    if let RedundantMerger::Buffered(m) = merger {
        let snap = m.snapshot();
        if let Ok(mut g) = stats.buffered_hitless_snapshot.write() {
            *g = Some(snap);
        }
    }
}

/// Publish a packet emitted by the merger — handles bandwidth gating
/// + stats increment + final upstream-seq stamping for the PID-bus.
fn publish_emitted_redundant_packet(
    chosen_leg: ActiveLeg,
    mut packet: RtpPacket,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &FlowStatsAccumulator,
    transcoder: &mut Option<InputTranscoder>,
) {
    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let upstream_leg_id = match chosen_leg {
        ActiveLeg::Leg1 => Some(0u8),
        ActiveLeg::Leg2 => Some(1u8),
        ActiveLeg::None => None,
    };
    if packet.upstream_seq.is_none() {
        packet.upstream_seq = Some(packet.sequence_number);
    }
    if packet.upstream_leg_id.is_none() {
        packet.upstream_leg_id = upstream_leg_id;
    }
    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(packet.data.len() as u64, Ordering::Relaxed);
    publish_input_packet(transcoder, broadcast_tx, packet);
}

/// Process a single packet from one leg of a 2022-7 redundant RTP input.
/// Applies ingress filters, optional FEC decode, and merger deduplication.
#[allow(clippy::too_many_arguments)]
fn process_redundant_rtp_packet(
    data: &[u8],
    src_ip: IpAddr,
    leg: ActiveLeg,
    source_filter: &Option<HashSet<IpAddr>>,
    pt_filter: &Option<Vec<u8>>,
    rate_limiter: &mut Option<TokenBucket>,
    fec_decoder: &mut Option<FecDecoder>,
    merger: &mut RedundantMerger,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &FlowStatsAccumulator,
    transcoder: &mut Option<InputTranscoder>,
) {
    // C5: Source IP filter
    if let Some(allowed) = source_filter {
        if !allowed.contains(&src_ip) {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }

    // FEC repair packet on this leg. Recovery feeds straight through the
    // merger like any other media packet so 2022-7 dedup still applies.
    if is_fec_packet(data) {
        if let Some(decoder) = fec_decoder {
            let recovered = decoder.process_fec(data);
            let now = std::time::Instant::now();
            for pkt in recovered {
                stats.fec_recovered.fetch_add(1, Ordering::Relaxed);
                for (chosen, p) in merger.ingest(pkt, leg, now, stats) {
                    publish_emitted_redundant_packet(chosen, p, broadcast_tx, stats, transcoder);
                }
            }
        }
        return;
    }

    if !is_likely_rtp(data) {
        return;
    }

    // U4: Payload type filter
    if let Some(pts) = pt_filter {
        let pt = data[1] & 0x7F;
        if !pts.contains(&pt) {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }

    // C7: Rate limit
    if let Some(limiter) = rate_limiter {
        if !limiter.try_consume(data.len()) {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }

    let seq = parse_rtp_sequence_number(data).unwrap_or(0);
    let ts = parse_rtp_timestamp(data).unwrap_or(0);
    // SSRC validation: 2022-7 mandates byte-identical RTP on both legs,
    // including the same SSRC. Mismatch lights the `ssrc_mismatch`
    // counter on the buffered merger so the manager can fire an event.
    if data.len() >= 12 {
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        merger.observe_ssrc(leg, ssrc);
    }
    let bytes_data = Bytes::copy_from_slice(data);
    let now = std::time::Instant::now();

    // FEC decode (per-leg), then merge each resulting packet.
    if let Some(decoder) = fec_decoder {
        let packets = decoder.process_media(seq, ts, &bytes_data);
        for pkt in packets {
            for (chosen, p) in merger.ingest(pkt, leg, now, stats) {
                publish_emitted_redundant_packet(chosen, p, broadcast_tx, stats, transcoder);
            }
        }
    } else {
        let packet = RtpPacket {
            data: bytes_data,
            sequence_number: seq,
            rtp_timestamp: ts,
            recv_time_us: now_us(),
            is_raw_ts: false,
            upstream_seq: Some(seq),
            upstream_leg_id: None, // filled in by publish_emitted_*
        };
        for (chosen, p) in merger.ingest(packet, leg, now, stats) {
            publish_emitted_redundant_packet(chosen, p, broadcast_tx, stats, transcoder);
        }
    }
}
