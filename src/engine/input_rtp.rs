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
        /// What the last packet released, reused from packet to packet so
        /// the per-packet path allocates nothing.
        out: Vec<(ActiveLeg, RtpPacket)>,
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
    ) -> std::vec::Drain<'_, (ActiveLeg, RtpPacket)> {
        match self {
            Self::Bare {
                merger,
                prev_active_leg,
                out,
            } => {
                out.clear();
                let seq = packet.sequence_number;
                if let Some(chosen) = merger.try_merge(seq, leg, packet.recv_time_us) {
                    if *prev_active_leg != ActiveLeg::None && chosen != *prev_active_leg {
                        stats.redundancy_switches.fetch_add(1, Ordering::Relaxed);
                    }
                    *prev_active_leg = chosen;
                    out.push((chosen, packet));
                }
                out.drain(..)
            }
            Self::Buffered(m) => {
                let seq = packet.sequence_number;
                m.ingest(seq, leg, packet, now)
            }
        }
    }

    fn drain_expired(&mut self, now: std::time::Instant) -> std::vec::Drain<'_, (ActiveLeg, RtpPacket)> {
        match self {
            Self::Bare { out, .. } => {
                out.clear();
                out.drain(..)
            }
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

use super::input_post_process::{InputPostProcess, InputPostProcessConfig};
use super::input_transcode::InputTranscoder;
use super::packet::{MAX_RTP_PACKET_SIZE, RtpPacket};

/// The ingress buffering of an RTP input's publisher.
pub(crate) fn ingress_buffering(
    config: &RtpInputConfig,
) -> crate::engine::ingress_publisher::IngressBuffering {
    crate::engine::ingress_publisher::IngressBuffering {
        delay_ms: config.ingress_delay_ms,
        dejitter_ms: config.ingress_dejitter_ms,
        residence_ms: config.ingress_residence_ms,
        honours_node_defaults: true,
    }
}

/// How long, by its configuration, an RTP input holds each packet before
/// publishing it: its 2022-7 merger's `path_differential_ms`, then its
/// ingress buffering, which runs after the merger.
pub fn receive_hold(config: &RtpInputConfig) -> std::time::Duration {
    let path_differential = config.redundancy.as_ref().and_then(|r| r.path_differential_ms);
    std::time::Duration::from_millis(path_differential.unwrap_or(0).into())
        + ingress_buffering(config).hold()
}

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
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
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
            transcoder.as_mut(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
            &event_sender,
        );
        let passthrough_clock = config.passthrough_clock.unwrap_or(false);
        let av_skew_for_post = stats.as_ref().av_skew_reporter_for_input(&input_id);
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: config.pid_overrides.as_ref(),
            pid_map: config.pid_map.as_ref(),
            passthrough_clock,
            av_sync_pacer: av_sync_pacer.as_ref(),
            av_skew: Some(&av_skew_for_post),
        });
        // The muxer-mode clock rewriter reports a source-clock fallback
        // (no PMT learned) as `clock_rewrite_pmt_not_learned`.
        if let Some(p) = post.as_mut() {
            p.set_event_sender(&event_sender, &input_id);
        }
        if let Some(ref p) = post {
            tracing::info!(
                "RTP input '{input_id}': ingress post-process active (program_filter={} pid_overrides={} pid_map={} passthrough_clock={})",
                config.program_number.is_some(),
                config.pid_overrides.is_some(),
                config.pid_map.is_some(),
                passthrough_clock,
            );
            let _ = p;
        }
        // Per-input ingress publisher (fixed-delay + de-jitter modes) — covers both single-
        // leg and 2022-7 dual-leg paths. On a -7 input the publisher sits
        // **downstream** of the merger, not upstream: each leg is fed to
        // `merger.ingest(...)` and only the merged output reaches the
        // publisher (`publish_emitted_redundant_packet`). So the buffer
        // smooths what the merger emitted; it does not present the merger
        // with de-jittered legs, which is what an earlier version of this
        // comment claimed.
        //
        // `dejitter_ms` may also be answered by the node's
        // `tuning.ingress_dejitter_ms` — see `honours_node_defaults`.
        let publisher = crate::engine::ingress_publisher::IngressPublisher::new(
            ingress_buffering(&config),
            broadcast_tx,
            &input_id,
            cancel.clone(),
            stats.clone(),
        );
        let result = if config.redundancy.is_some() {
            rtp_input_redundant_loop(config, publisher, stats, cancel, &event_sender, &flow_id, &input_id, &mut transcoder, &mut post).await
        } else {
            rtp_input_loop(config, publisher, stats, cancel, &event_sender, &flow_id, &input_id, &mut transcoder, &mut post).await
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

/// Whether RTP sequence number `seq` is ahead of `of`: by 1 to 0x7FFF,
/// modulo 2^16.
fn seq_is_ahead(seq: u16, of: u16) -> bool {
    (1..0x8000).contains(&seq.wrapping_sub(of))
}

/// Keeps a single-leg RTP input's FEC repairs from going out behind packets
/// it has already published (edge#206).
///
/// SMPTE 2022-1 column FEC follows the last row of its column, so most
/// packets FEC rebuilds are rebuilt up to (D-1)*L datagrams after newer ones
/// went out. Nothing between this input and the outputs reorders by sequence
/// number, so publishing such a packet would hand every output a datagram
/// out of order -- continuity errors, and a PCR stepping back -- which is
/// worse than the loss it repairs. So a rebuilt packet is published only
/// when it is ahead of every packet already published; one behind stays in
/// the decoder's matrix (where it can still help rebuild others) and is
/// neither published nor counted. A 2022-7 input is not gated: its merger
/// takes the repairs and, with `path_differential_ms`, releases them in
/// order.
///
/// It also remembers the last [`RECENT_REPAIRS`] repairs it let out, so the
/// original of one, arriving late after the decoder has closed its matrix
/// (where [`FecDecoder::holds_payload`] can no longer see it), is not
/// published a second time. Like `holds_payload`, it matches on the bytes
/// as well as the number: a datagram under a recently repaired number with
/// other bytes (a sender that restarted a few numbers back) is new content,
/// and is published like any other received packet.
struct RepairOrder {
    /// The newest sequence number published, as a modular maximum.
    newest: Option<u16>,
    /// The FEC matrix size (L*D). A received packet this far or further
    /// behind `newest` is a stream that moved (a sender restarted on a new
    /// sequence number), not a reorder: the decoder starts a new matrix on
    /// it, and `newest` follows it there.
    window: u16,
    /// Packets published so far: the clock `repairs` are aged by.
    published: u64,
    /// The last repairs published.
    repairs: [Option<PublishedRepair>; RECENT_REPAIRS],
    /// Where the next repair is written in `repairs`.
    next_repair: usize,
}

/// A repair [`RepairOrder`] let out.
struct PublishedRepair {
    seq: u16,
    /// [`RepairOrder::published`] as it went out.
    at: u64,
    /// The datagram as published (a reference to it, not a copy), whose
    /// payload a late original matches.
    datagram: Bytes,
}

/// How many published repairs [`RepairOrder`] remembers. Only a repair
/// ahead of everything published goes out -- in practice the last-row
/// cells of each matrix -- so this covers several matrices' worth.
const RECENT_REPAIRS: usize = 32;

/// The bytes after an RTP datagram's 12-byte fixed header: what the FEC
/// decoder files and rebuilds, and so what a late original is matched on.
fn fec_payload(datagram: &[u8]) -> &[u8] {
    datagram.get(12..).unwrap_or_default()
}

impl RepairOrder {
    fn new(window: u16) -> Self {
        Self {
            newest: None,
            window,
            published: 0,
            repairs: std::array::from_fn(|_| None),
            next_repair: 0,
        }
    }

    /// A received packet, which is published: moves `newest` forward, never
    /// back, except onto a stream that moved.
    fn received(&mut self, seq: u16) {
        self.published += 1;
        match self.newest {
            Some(n) if !seq_is_ahead(seq, n) && n.wrapping_sub(seq) < self.window => {}
            _ => self.newest = Some(seq),
        }
    }

    /// Whether the packet FEC rebuilt as `datagram` under `seq` may be
    /// published: only ahead of every packet already published. Moves
    /// `newest` onto it, and remembers it, when it is.
    fn recovered(&mut self, seq: u16, datagram: &Bytes) -> bool {
        match self.newest {
            Some(n) if !seq_is_ahead(seq, n) => false,
            _ => {
                self.newest = Some(seq);
                self.published += 1;
                self.repairs[self.next_repair] = Some(PublishedRepair {
                    seq,
                    at: self.published,
                    datagram: datagram.clone(),
                });
                self.next_repair = (self.next_repair + 1) % RECENT_REPAIRS;
                true
            }
        }
    }

    /// Whether a received `datagram` under `seq` is the late original of a
    /// repair already published: behind `newest`, within the window (not a
    /// stream that moved), published as a repair within the last two
    /// windows' worth of packets (so not a number from a lap of the
    /// sequence space ago), and carrying that repair's payload. Scans only
    /// for a packet that arrives behind `newest`, and compares bytes only
    /// on a number that matches.
    fn already_repaired(&self, seq: u16, datagram: &[u8]) -> bool {
        let Some(n) = self.newest else {
            return false;
        };
        if seq_is_ahead(seq, n) || n.wrapping_sub(seq) >= self.window {
            return false;
        }
        let horizon = 2 * self.window as u64;
        let payload = fec_payload(datagram);
        !payload.is_empty()
            && self.repairs.iter().flatten().any(|r| {
                r.seq == seq
                    && self.published - r.at < horizon
                    && fec_payload(&r.datagram) == payload
            })
    }
}

/// What a single-leg FEC input publishes of a received media datagram and
/// of the packets FEC rebuilt alongside it, in that order. A datagram whose
/// exact payload the decoder already holds under its sequence number -- a
/// packet FEC rebuilt before it arrived, or a copy of one received -- is a
/// duplicate of something already published or deliberately withheld, and
/// is dropped; so is the late original of a repair published from a matrix
/// the decoder has since closed (matched on its bytes too, so new content
/// under that number is published). Returns the number of rebuilt packets
/// published.
fn publish_fec_media(
    decoder: &mut FecDecoder,
    order: &mut RepairOrder,
    seq: u16,
    ts: u32,
    data: &Bytes,
    mut publish: impl FnMut(RtpPacket),
) -> u64 {
    let duplicate = decoder.holds_payload(seq, data) || order.already_repaired(seq, data);
    let mut packets = decoder.process_media(seq, ts, data).into_iter();
    if let Some(received) = packets.next()
        && !duplicate
    {
        order.received(seq);
        publish(received);
    }
    publish_recovered(order, packets, publish)
}

/// What a single-leg FEC input publishes of the packets one FEC datagram
/// rebuilt. Returns the number published.
fn publish_fec_repair(
    decoder: &mut FecDecoder,
    order: &mut RepairOrder,
    data: &[u8],
    publish: impl FnMut(RtpPacket),
) -> u64 {
    publish_recovered(order, decoder.process_fec(data).into_iter(), publish)
}

fn publish_recovered(
    order: &mut RepairOrder,
    packets: impl Iterator<Item = RtpPacket>,
    mut publish: impl FnMut(RtpPacket),
) -> u64 {
    let mut published = 0;
    for pkt in packets {
        if order.recovered(pkt.sequence_number, &pkt.data) {
            published += 1;
            publish(pkt);
        }
    }
    published
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
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let socket = match bind_udp_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
        config.interface_binding.as_ref(),
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
            let scope = BindScope::flow_input(flow_id, input_id);
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

    // Optional FEC decoder, with the gate that keeps its repairs in
    // sequence order (`RepairOrder`).
    let mut fec_decoder = config.fec_decode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC decode enabled: L={} D={}",
            fec_config.columns,
            fec_config.rows
        );
        (
            FecDecoder::new(fec_config.columns, fec_config.rows),
            RepairOrder::new(fec_config.columns as u16 * fec_config.rows as u16),
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
                        if let Some(ref allowed) = source_filter
                            && !allowed.contains(&src.ip()) {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }

                        // ── FEC repair packet (SMPTE 2022-1, bilbycast framing) ──
                        // Route to the decoder before the RTP-shape check so the
                        // repair datagram isn't silently dropped. Publishes a
                        // reconstructed media packet only when it is ahead of
                        // everything already published (`RepairOrder`).
                        if is_fec_packet(data) {
                            if let Some((ref mut decoder, ref mut order)) = fec_decoder {
                                let recovered = publish_fec_repair(decoder, order, data, |pkt| {
                                    crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, &publisher, pkt);
                                });
                                stats.fec_recovered.fetch_add(recovered, Ordering::Relaxed);
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
                        if let Some(ref mut limiter) = rate_limiter
                            && !limiter.try_consume(len) {
                                stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                                continue;
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

                        if let Some((ref mut decoder, ref mut order)) = fec_decoder {
                            let recovered = publish_fec_media(decoder, order, seq, ts, &bytes_data, |pkt| {
                                crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, &publisher, pkt);
                            });
                            if recovered > 0 {
                                stats.fec_recovered.fetch_add(recovered, Ordering::Relaxed);
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
                                sender_timestamp_us: None,
                            };
                            crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, &publisher, packet);
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
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    let socket1 = match bind_udp_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
        config.interface_binding.as_ref(),
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
            let scope = BindScope::flow_input(flow_id, input_id);
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
        redundancy.interface_binding.as_ref(),
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
            let scope = BindScope::flow_input(flow_id, input_id);
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
            out: Vec::with_capacity(1),
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
                        chosen, pkt, &publisher, &stats, transcoder, post,
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
                        &publisher, &stats, transcoder, post,
                    );
                }
            }
            result = socket2.recv_from(&mut buf2) => {
                if let Ok((len, src)) = result {
                    process_redundant_rtp_packet(
                        &buf2[..len], src.ip(), ActiveLeg::Leg2,
                        &source_filter, &pt_filter, &mut rate_limiter,
                        &mut fec_decoder_leg2, &mut merger,
                        &publisher, &stats, transcoder, post,
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
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    stats: &FlowStatsAccumulator,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
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
    crate::engine::input_transcode::publish_input_packet_smoothed(
        transcoder, post, publisher, packet,
    );
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
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    stats: &FlowStatsAccumulator,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    // C5: Source IP filter
    if let Some(allowed) = source_filter
        && !allowed.contains(&src_ip) {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
            return;
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
                    publish_emitted_redundant_packet(chosen, p, publisher, stats, transcoder, post);
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
    if let Some(limiter) = rate_limiter
        && !limiter.try_consume(data.len()) {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
            return;
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
                publish_emitted_redundant_packet(chosen, p, publisher, stats, transcoder, post);
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
            sender_timestamp_us: None,
        };
        for (chosen, p) in merger.ingest(packet, leg, now, stats) {
            publish_emitted_redundant_packet(chosen, p, publisher, stats, transcoder, post);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::encoder::FecEncoder;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn seq_is_ahead_is_modular() {
        assert!(seq_is_ahead(101, 100));
        assert!(!seq_is_ahead(100, 100));
        assert!(!seq_is_ahead(99, 100));
        assert!(seq_is_ahead(2, 0xFFFE), "across the wrap");
        assert!(!seq_is_ahead(0xFFFE, 2), "behind, across the wrap");
        assert!(seq_is_ahead(100u16.wrapping_add(0x7FFF), 100));
        assert!(!seq_is_ahead(100u16.wrapping_add(0x8000), 100));
    }

    /// A received packet moves `newest` forward and never back -- a
    /// reordered arrival must not re-admit late repairs -- except onto a
    /// stream that moved a whole matrix or more behind.
    #[test]
    fn received_packets_move_newest_forward_or_onto_a_moved_stream() {
        let mut order = RepairOrder::new(16);
        order.received(0xFFF0);
        order.received(3); // across the wrap
        assert_eq!(order.newest, Some(3));
        order.received(1); // reordered, two behind
        assert_eq!(order.newest, Some(3));
        assert!(!order.recovered(2, &datagram(2, 0)), "behind newest");
        assert!(order.recovered(4, &datagram(4, 0)));
        assert_eq!(order.newest, Some(4));
        // A sender restarted 1000 numbers back: a new stream, not a reorder.
        let moved = 4u16.wrapping_sub(1000);
        order.received(moved);
        assert_eq!(order.newest, Some(moved));
        assert!(order.recovered(moved.wrapping_add(1), &datagram(moved.wrapping_add(1), 0)));
    }

    /// A 44-byte RTP datagram under `seq`; `content` picks its payload, so
    /// one number can be sent with other bytes.
    fn datagram(seq: u16, content: usize) -> Bytes {
        let mut dg = vec![0x80, 33, (seq >> 8) as u8, seq as u8, 0, 0, 0, 1, 0, 0, 0, 7];
        dg.extend((0..32).map(|i| (seq as usize * 31 + i * 5 + content) as u8));
        Bytes::from(dg)
    }

    /// The datagrams and FEC an L x D output sends for `seqs`, in order.
    fn sent(l: u8, d: u8, seqs: std::ops::Range<u16>) -> Vec<(Option<u16>, Bytes)> {
        let mut enc = FecEncoder::new(l, d, Arc::new(AtomicU64::new(0)));
        let mut wire = Vec::new();
        for seq in seqs {
            let dg = datagram(seq, 0);
            let fec = enc.process(seq, &dg[12..]);
            wire.push((Some(seq), dg));
            wire.extend(fec.into_iter().map(|f| (None, f)));
        }
        wire
    }

    /// Run `wire` through the single-leg FEC path; returns what it published
    /// (seq, payload) and the count it adds to `fec_recovered`.
    fn single_leg(l: u8, d: u8, wire: &[(Option<u16>, Bytes)]) -> (Vec<(u16, Bytes)>, u64) {
        let mut decoder = FecDecoder::new(l, d);
        let mut order = RepairOrder::new(l as u16 * d as u16);
        let mut published = Vec::new();
        let mut recovered = 0;
        for (seq, data) in wire {
            let mut publish = |p: RtpPacket| published.push((p.sequence_number, p.data));
            recovered += match seq {
                Some(seq) => publish_fec_media(&mut decoder, &mut order, *seq, 0, data, &mut publish),
                None => publish_fec_repair(&mut decoder, &mut order, data, &mut publish),
            };
        }
        (published, recovered)
    }

    fn lose(wire: &[(Option<u16>, Bytes)], seq: u16) -> Vec<(Option<u16>, Bytes)> {
        wire.iter().filter(|(s, _)| *s != Some(seq)).cloned().collect()
    }

    fn assert_in_order(published: &[(u16, Bytes)]) {
        for w in published.windows(2) {
            assert!(seq_is_ahead(w[1].0, w[0].0), "{} published after {}", w[1].0, w[0].0);
        }
    }

    /// A loss in row 0 is rebuilt by its column FEC only after the column's
    /// last row went out -- (D-1)*L packets late. It is withheld (and not
    /// counted), so what the input publishes stays in sequence order.
    #[test]
    fn a_late_repair_is_withheld_and_not_counted() {
        let wire = sent(4, 4, 1000..1032);
        let (published, recovered) = single_leg(4, 4, &lose(&wire, 1017));
        assert_in_order(&published);
        assert!(published.iter().all(|(s, _)| *s != 1017));
        assert_eq!(recovered, 0);
        assert_eq!(published.len(), 31);
    }

    /// A loss in the last row is rebuilt by the column FEC that follows it,
    /// before any newer packet: published, in order, and counted.
    #[test]
    fn a_last_row_repair_is_published_in_order() {
        let wire = sent(4, 4, 1000..1032);
        let (published, recovered) = single_leg(4, 4, &lose(&wire, 1029));
        assert_in_order(&published);
        assert_eq!(recovered, 1);
        assert_eq!(published.len(), 32);
        let original = &wire.iter().find(|(s, _)| *s == Some(1029)).unwrap().1;
        let rebuilt = &published.iter().find(|(s, _)| *s == 1029).unwrap().1;
        assert_eq!(&rebuilt[12..], &original[12..]);
    }

    /// A last-row datagram is delayed past the end of its matrix: its
    /// column FEC rebuilds it in order, the decoder then closes the matrix,
    /// and the original, arriving after that, is not published again.
    #[test]
    fn a_repaired_datagram_arriving_after_its_matrix_closed_is_published_once() {
        let mut wire = sent(4, 4, 1000..1040);
        let i = wire.iter().position(|(s, _)| *s == Some(1029)).unwrap();
        let late = wire.remove(i);
        let next = wire.iter().position(|(s, _)| *s == Some(1032)).unwrap();
        wire.insert(next, late);
        let (published, recovered) = single_leg(4, 4, &wire);
        let seqs: Vec<u16> = published.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, (1000..1040).collect::<Vec<_>>());
        assert_eq!(recovered, 1);
    }

    /// The repairs remembered age out: a packet published as a repair a lap
    /// of the sequence space ago does not hide a reordered packet that
    /// carries the same number now.
    #[test]
    fn a_repair_from_a_lap_ago_hides_nothing() {
        let mut order = RepairOrder::new(16);
        order.received(99);
        assert!(order.recovered(100, &datagram(100, 0)));
        order.received(101);
        assert!(order.already_repaired(100, &datagram(100, 0)), "the late original of a fresh repair");
        for k in 1..=0x1_0000u32 {
            order.received(101u16.wrapping_add(k as u16));
        }
        assert_eq!(order.newest, Some(101));
        assert!(!order.already_repaired(100, &datagram(100, 0)), "a reorder one lap on, not that repair");
    }

    /// A sender restarts a few numbers back just after a repair went out,
    /// and sends the repaired number again with other bytes: new content,
    /// published (behind `newest`, like every received packet), not taken
    /// for the repair's late original.
    #[test]
    fn a_number_resent_with_other_bytes_after_its_repair_is_published() {
        let mut wire = lose(&sent(4, 4, 1000..1032), 1029);
        wire.extend((1026..1032).map(|s| (Some(s), datagram(s, 1))));
        let (published, recovered) = single_leg(4, 4, &wire);
        assert_eq!(recovered, 1, "1029 is rebuilt in order");
        let seqs: Vec<u16> = published.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, (1000..1032).chain(1026..1032).collect::<Vec<_>>());
        for (s, data) in &published[32..] {
            assert_eq!(data, &datagram(*s, 1), "{s} carries the new content");
        }
        // The same bytes under that number are still the late original.
        let mut order = RepairOrder::new(16);
        order.received(1028);
        assert!(order.recovered(1029, &datagram(1029, 0)));
        order.received(1030);
        assert!(order.already_repaired(1029, &datagram(1029, 0)));
        assert!(!order.already_repaired(1029, &datagram(1029, 1)));
        assert!(!order.already_repaired(1029, &datagram(1029, 0).slice(..12)), "header only");
    }

    /// The network swaps a last-row datagram and the column FEC right after
    /// it: FEC rebuilds the datagram first, and the real one, arriving
    /// next, is not published a second time.
    #[test]
    fn a_repair_that_beats_its_datagram_is_published_once() {
        let mut wire = sent(4, 4, 1000..1016);
        let i = wire.iter().position(|(s, _)| *s == Some(1013)).unwrap();
        assert!(wire[i + 1].0.is_none(), "column 1's FEC follows 1013");
        wire.swap(i, i + 1);
        let (published, recovered) = single_leg(4, 4, &wire);
        let seqs: Vec<u16> = published.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, (1000..1016).collect::<Vec<_>>());
        assert_eq!(recovered, 1);
    }
}
