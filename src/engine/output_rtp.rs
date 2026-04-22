// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::BytesMut;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtpOutputConfig;
use crate::fec::encoder::FecEncoder;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;
use crate::util::time::now_us;

use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::ts_audio_replace::TsAudioReplacer;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;
use super::ts_video_replace::TsVideoReplacer;

/// TS sync byte.
const TS_SYNC_BYTE: u8 = 0x47;

/// TS packet size.
const TS_PACKET_SIZE: usize = 188;

/// Number of TS packets per UDP datagram for raw TS output.
/// 7 × 188 = 1316 bytes — the standard for MPEG-TS over UDP.
const TS_PACKETS_PER_DATAGRAM: usize = 7;

/// Minimum consecutive sync bytes needed to confirm TS alignment.
const TS_SYNC_CONFIRM_COUNT: usize = 3;

/// RTP header size (no CSRC, no extension).
const RTP_HEADER_SIZE: usize = 12;

/// RFC 2250 payload type for MPEG-TS over RTP (PT=33, MP2T).
const RTP_PT_MP2T: u8 = 33;

/// State carried across packets when wrapping raw TS data into RTP.
/// One per output. SSRC is randomised at construction; sequence number and
/// timestamp advance monotonically per emitted RTP datagram.
struct RtpWrapState {
    seq: u16,
    ssrc: u32,
}

impl RtpWrapState {
    fn new() -> Self {
        Self {
            seq: rand::random::<u16>(),
            ssrc: rand::random::<u32>(),
        }
    }

    /// Build a 12-byte RFC 2250 RTP header for an MPEG-TS payload (PT=33).
    /// `rtp_ts` is the 32-bit RTP timestamp at the 90 kHz MPEG clock; pass
    /// the value derived from the upstream `RtpPacket::rtp_timestamp` so the
    /// wire timing reflects the source media clock rather than wall time.
    /// Sequence number advances by 1 per call.
    fn build_header(&mut self, rtp_ts: u32) -> [u8; RTP_HEADER_SIZE] {
        let mut hdr = [0u8; RTP_HEADER_SIZE];
        hdr[0] = 0x80; // V=2, P=0, X=0, CC=0
        hdr[1] = RTP_PT_MP2T; // M=0, PT=33
        hdr[2] = (self.seq >> 8) as u8;
        hdr[3] = (self.seq & 0xFF) as u8;
        hdr[4] = (rtp_ts >> 24) as u8;
        hdr[5] = (rtp_ts >> 16) as u8;
        hdr[6] = (rtp_ts >> 8) as u8;
        hdr[7] = rtp_ts as u8;
        hdr[8] = (self.ssrc >> 24) as u8;
        hdr[9] = (self.ssrc >> 16) as u8;
        hdr[10] = (self.ssrc >> 8) as u8;
        hdr[11] = self.ssrc as u8;
        self.seq = self.seq.wrapping_add(1);
        hdr
    }
}

/// Spawn a task that subscribes to the broadcast channel and sends
/// RTP packets out over a UDP socket. If FEC encode is configured,
/// FEC packets are also generated and sent. If redundancy is configured,
/// packets are duplicated to two independent UDP legs (SMPTE 2022-7).
pub fn spawn_rtp_output(
    config: RtpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    events: EventSender,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("rtp".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
    });

    tokio::spawn(async move {
        let result = if config.redundancy.is_some() {
            rtp_output_redundant_loop(&config, &mut rx, output_stats, cancel, frame_rate_rx).await
        } else {
            rtp_output_loop(&config, &mut rx, output_stats, cancel, frame_rate_rx, &events).await
        };
        if let Err(e) = result {
            tracing::error!("RTP output '{}' exited with error: {e}", config.id);
        }
    })
}

/// Core send loop for a single RTP output.
///
/// Subscribes to the flow's broadcast channel and, for every received
/// [`RtpPacket`], sends it as a UDP datagram to the configured destination
/// address. If FEC encoding is enabled, additional FEC repair packets are
/// generated and sent after each media packet.
///
/// **Broadcast lag handling**: if this output cannot keep up with the input
/// rate (e.g., due to transient network stalls), the broadcast receiver
/// will report `Lagged(n)`. The output counts those dropped packets in
/// `stats.packets_dropped` and continues — it never blocks the input or
/// other outputs.
///
/// The loop exits when the cancellation token fires or the broadcast
/// channel is closed (input task exited).
async fn rtp_output_loop(
    config: &RtpOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    events: &EventSender,
) -> anyhow::Result<()> {
    let (socket, dest) =
        create_udp_output(&config.dest_addr, config.bind_addr.as_deref(), config.interface_addr.as_deref(), config.dscp).await?;

    tracing::info!(
        "RTP output '{}' started -> {}",
        config.id,
        dest
    );

    // Optional FEC encoder (SMPTE 2022-1).
    //
    // Like the input-side FEC decoder, we use a shared `Arc<AtomicU64>`
    // counter so the encoder can increment it internally and we periodically
    // copy the value back to the stats accumulator without borrow conflicts.
    let fec_sent_counter = Arc::new(AtomicU64::new(0));
    let mut fec_encoder = config.fec_encode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC encode enabled for output '{}': L={} D={}",
            config.id,
            fec_config.columns,
            fec_config.rows
        );
        FecEncoder::new(
            fec_config.columns,
            fec_config.rows,
            fec_sent_counter.clone(),
        )
    });

    // Buffer for re-aligning raw TS data into 188-byte aligned datagrams.
    // SRT may deliver data in chunks that don't align to TS packet boundaries
    // (e.g., 1456-byte byte-stream chunks). This buffer accumulates incoming
    // data, finds the TS sync byte (0x47) boundary, and sends properly aligned
    // chunks (7 × 188 = 1316 bytes) — the standard for MPEG-TS over UDP.
    let mut ts_realign_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut is_raw_ts_stream = false;
    let mut ts_sync_found = false;

    // Optional MPTS → SPTS program filter.
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "RTP output '{}': program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    // Optional PID remapper. Runs on every packet after the program filter
    // and preserves the RTP header for wrapped packets (zero-copy passthrough).
    let mut pid_remapper = config.pid_map.as_ref().and_then(|m| {
        let r = TsPidRemapper::new(m);
        if r.is_active() {
            tracing::info!(
                "RTP output '{}': pid_map active ({} entries)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut remap_scratch: Vec<u8> = Vec::new();

    // Optional audio ES replacement. When set, all outgoing packets are
    // forced through the raw-TS path (the original RTP framing is
    // discarded and rebuilt) because the replacer rewrites payload bytes.
    let mut audio_replacer = match config.audio_encode.as_ref() {
        Some(enc) => match TsAudioReplacer::new(enc, config.transcode.clone()) {
            Ok(r) => {
                tracing::info!(
                    "RTP output '{}': audio_encode active ({})",
                    config.id,
                    r.target_description()
                );
                events.emit_output_with_details(
                    EventSeverity::Info,
                    category::AUDIO_ENCODE,
                    format!("TS audio encoder started: output '{}'", config.id),
                    &config.id,
                    serde_json::json!({ "codec": enc.codec }),
                );
                Some(r)
            }
            Err(e) => {
                tracing::error!(
                    "RTP output '{}': audio_encode rejected: {e}; audio will be left untouched",
                    config.id
                );
                events.emit_output_with_details(
                    EventSeverity::Critical,
                    category::AUDIO_ENCODE,
                    format!("TS audio encoder failed: output '{}': {e}", config.id),
                    &config.id,
                    serde_json::json!({ "error": e.to_string() }),
                );
                None
            }
        },
        None => None,
    };
    let mut replace_scratch: Vec<u8> = Vec::new();

    // Optional video ES replacement.
    let mut video_replacer = match config.video_encode.as_ref() {
        Some(enc) => match TsVideoReplacer::new(enc, None) {
            Ok(r) => {
                let backend = match enc.codec.as_str() {
                    "x264" | "x265" => enc.codec.clone(),
                    "h264_nvenc" | "hevc_nvenc" => "nvenc".to_string(),
                    other => other.to_string(),
                };
                let target_codec = match enc.codec.as_str() {
                    "x264" | "h264_nvenc" => "h264",
                    "x265" | "hevc_nvenc" => "hevc",
                    other => other,
                };
                stats.set_video_encode_stats(
                    r.stats_handle(),
                    String::new(),
                    target_codec.to_string(),
                    enc.width.unwrap_or(0),
                    enc.height.unwrap_or(0),
                    match (enc.fps_num, enc.fps_den) {
                        (Some(n), Some(d)) if d > 0 => n as f32 / d as f32,
                        _ => 0.0,
                    },
                    enc.bitrate_kbps.unwrap_or(0),
                    backend,
                );
                tracing::info!(
                    "RTP output '{}': video_encode active ({})",
                    config.id,
                    r.target_description()
                );
                events.emit_output_with_details(
                    EventSeverity::Info,
                    category::VIDEO_ENCODE,
                    format!("Video encoder started: output '{}'", config.id),
                    &config.id,
                    serde_json::json!({ "codec": enc.codec }),
                );
                Some(r)
            }
            Err(e) => {
                tracing::error!(
                    "RTP output '{}': video_encode rejected: {e}; video will be left untouched",
                    config.id
                );
                events.emit_output_with_details(
                    EventSeverity::Critical,
                    category::VIDEO_ENCODE,
                    format!("Video encoder failed: output '{}': {e}", config.id),
                    &config.id,
                    serde_json::json!({ "error": e.to_string() }),
                );
                None
            }
        },
        None => None,
    };
    let mut video_replace_scratch: Vec<u8> = Vec::new();

    // RTP wrap state for raw-TS payloads. Random SSRC + monotonic seq so this
    // output emits RFC 2250-compliant RTP/MPEG-TS even when its upstream
    // input was raw TS (UDP, RTMP, RTSP, etc.). The 90 kHz timestamp is
    // carried from the inbound packet so the wire reflects the source media
    // clock.
    let mut rtp_wrap = RtpWrapState::new();
    // Reusable scratch buffer for [12-byte RTP header || 1316-byte TS] datagrams.
    let mut rtp_send_buf = Vec::with_capacity(RTP_HEADER_SIZE + TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE);

    // Optional output delay buffer for stream synchronization.
    let resolved = if let Some(ref delay) = config.delay {
        resolve_output_delay(delay, &config.id, frame_rate_rx, &cancel).await
    } else {
        None
    };
    let (mut delay_buf, _frame_rate_watch) = match resolved {
        Some((buf, watch)) => (Some(buf), watch),
        None => (None, None),
    };

    // Pinned sleep for delay buffer drain timing. Reset each iteration
    // from the front packet's release time. Starts with a distant deadline
    // (effectively infinite) so it never fires when no delay is configured.
    let delay_sleep = tokio::time::sleep(Duration::from_secs(86400));
    tokio::pin!(delay_sleep);

    loop {
        // Reset the delay timer to fire when the oldest buffered packet
        // is due for release. Only meaningful when delay is active.
        if let Some(ref db) = delay_buf {
            if let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }
        }

        // Collect packets to send after the select (avoids duplicating
        // the send logic between the recv and delay-drain branches).
        let mut packets_to_send: Vec<RtpPacket> = Vec::new();

        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(ref db) = delay_buf {
                    if db.len() > 0 {
                        tracing::debug!(
                            "RTP output '{}' cancelled with {} buffered packets",
                            config.id, db.len()
                        );
                    }
                }
                tracing::info!("RTP output '{}' stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Apply program filter (if enabled). For RTP-wrapped
                        // packets the filter strips the RTP header, filters
                        // the TS payload, then re-wraps with the original
                        // RTP header. For raw TS the filtered bytes feed the
                        // re-alignment buffer below. If the filter eats the
                        // entire packet (no target-program TS in this datagram)
                        // we skip both the send AND the FEC update.
                        let packet = if let Some(ref mut filter) = program_filter {
                            match filter.filter_packet(&packet, &mut filter_scratch) {
                                Some(filtered) => RtpPacket {
                                    data: filtered,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                },
                                None => continue,
                            }
                        } else {
                            packet
                        };

                        let packet = if let Some(ref mut remapper) = pid_remapper {
                            match remapper.process_packet(&packet, &mut remap_scratch) {
                                Some(remapped) => RtpPacket {
                                    data: remapped,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                },
                                None => continue,
                            }
                        } else {
                            packet
                        };

                        if let Some(ref mut db) = delay_buf {
                            db.push(packet);
                            // Don't send yet — the delay_sleep branch will drain
                        } else {
                            packets_to_send.push(packet);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "RTP output '{}' lagged, dropped {n} packets",
                            config.id
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("RTP output '{}' broadcast channel closed", config.id);
                        break;
                    }
                }
            }
            _ = &mut delay_sleep, if delay_buf.as_ref().map_or(false, |db| db.len() > 0) => {
                let db = delay_buf.as_mut().unwrap();
                let now = now_us();
                for packet in db.drain_ready(now) {
                    packets_to_send.push(packet);
                }
            }
        }

        // Process all packets collected from either the immediate recv path
        // or the delay buffer drain. This shared code path avoids duplicating
        // the raw-TS re-alignment, RTP wrapping, FEC, and socket send logic.
        for packet in &packets_to_send {
            // When audio_encode or video_encode is active we must rewrite
            // TS bytes, so even RTP-wrapped inputs get stripped and forced
            // through the raw-TS path (output is then re-wrapped with
            // fresh RTP framing).
            let force_raw_ts = audio_replacer.is_some() || video_replacer.is_some();

            if packet.is_raw_ts || force_raw_ts {
                if !is_raw_ts_stream {
                    is_raw_ts_stream = true;
                    tracing::info!(
                        "RTP output '{}': raw TS detected, enabling TS re-alignment ({} TS pkts/datagram)",
                        config.id,
                        TS_PACKETS_PER_DATAGRAM
                    );
                }

                // Extract TS payload: strip RTP header if forced-raw and
                // the input was RTP-wrapped; otherwise use packet.data as-is.
                let ts_input: &[u8] = if !packet.is_raw_ts && force_raw_ts {
                    if packet.data.len() > RTP_HEADER_SIZE {
                        &packet.data[RTP_HEADER_SIZE..]
                    } else {
                        continue;
                    }
                } else {
                    &packet.data[..]
                };

                // Chain: ts_input → audio_replacer → video_replacer → ts_realign_buf.
                let after_audio: &[u8] = if let Some(ref mut replacer) = audio_replacer {
                    replace_scratch.clear();
                    tokio::task::block_in_place(|| {
                        replacer.process(ts_input, &mut replace_scratch);
                    });
                    &replace_scratch
                } else {
                    ts_input
                };
                if let Some(ref mut vreplacer) = video_replacer {
                    video_replace_scratch.clear();
                    tokio::task::block_in_place(|| {
                        vreplacer.process(after_audio, &mut video_replace_scratch);
                    });
                    ts_realign_buf.extend_from_slice(&video_replace_scratch);
                } else {
                    ts_realign_buf.extend_from_slice(after_audio);
                }

                if !ts_sync_found {
                    let min_bytes = TS_SYNC_CONFIRM_COUNT * TS_PACKET_SIZE;
                    if ts_realign_buf.len() >= min_bytes {
                        let mut found_offset = None;
                        for offset in 0..TS_PACKET_SIZE {
                            let all_sync = (0..TS_SYNC_CONFIRM_COUNT).all(|i| {
                                let pos = offset + i * TS_PACKET_SIZE;
                                pos < ts_realign_buf.len()
                                    && ts_realign_buf[pos] == TS_SYNC_BYTE
                            });
                            if all_sync {
                                found_offset = Some(offset);
                                break;
                            }
                        }

                        if let Some(offset) = found_offset {
                            if offset > 0 {
                                tracing::info!(
                                    "RTP output '{}': TS sync found at offset {}, discarding {} leading bytes",
                                    config.id,
                                    offset,
                                    offset
                                );
                                let _ = ts_realign_buf.split_to(offset);
                            } else {
                                tracing::info!(
                                    "RTP output '{}': TS sync found at offset 0 (already aligned)",
                                    config.id
                                );
                            }
                            ts_sync_found = true;
                        } else {
                            tracing::warn!(
                                "RTP output '{}': no TS sync found in {} bytes, discarding",
                                config.id,
                                ts_realign_buf.len()
                            );
                            ts_realign_buf.clear();
                        }
                    }
                }

                if ts_sync_found {
                    while ts_realign_buf.len() >= ts_datagram_size {
                        let datagram = ts_realign_buf.split_to(ts_datagram_size);
                        // PID-bus Phase 8: per-output PCR trust sampler.
                        let pcr_sample =
                            crate::engine::ts_parse::first_pcr_in_ts_buffer(&datagram);
                        let header = rtp_wrap.build_header(packet.rtp_timestamp);
                        rtp_send_buf.clear();
                        rtp_send_buf.extend_from_slice(&header);
                        rtp_send_buf.extend_from_slice(&datagram);
                        match socket.send_to(&rtp_send_buf, dest).await {
                            Ok(sent) => {
                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                stats.record_latency(packet.recv_time_us);
                                if let Some(pcr) = pcr_sample {
                                    stats.record_pcr_egress(
                                        pcr,
                                        crate::util::time::now_us(),
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!("RTP output '{}' send error: {e}", config.id);
                            }
                        }
                    }
                }
            } else {
                // RTP-wrapped data: send as-is (already properly framed).
                // Sample PCR from the payload (skip the 12-byte RTP header).
                let pcr_sample = if packet.data.len() > crate::engine::ts_parse::RTP_HEADER_MIN_SIZE {
                    crate::engine::ts_parse::first_pcr_in_ts_buffer(
                        &packet.data[crate::engine::ts_parse::RTP_HEADER_MIN_SIZE..],
                    )
                } else {
                    None
                };
                match socket.send_to(&packet.data, dest).await {
                    Ok(sent) => {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                        stats.record_latency(packet.recv_time_us);
                        if let Some(pcr) = pcr_sample {
                            stats.record_pcr_egress(pcr, crate::util::time::now_us());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("RTP output '{}' send error: {e}", config.id);
                    }
                }
            }

            // Generate and send FEC packets if enabled.
            if let Some(ref mut encoder) = fec_encoder {
                let rtp_header_len = 12;
                let payload = if packet.data.len() > rtp_header_len {
                    &packet.data[rtp_header_len..]
                } else {
                    &packet.data[..]
                };

                let fec_packets = encoder.process(packet.sequence_number, payload);
                for fec_pkt in fec_packets {
                    match socket.send_to(&fec_pkt, dest).await {
                        Ok(sent) => {
                            stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "RTP output '{}' FEC send error: {e}",
                                config.id
                            );
                        }
                    }
                }
                let sent = fec_sent_counter.load(Ordering::Relaxed);
                stats.fec_packets_sent.store(sent, Ordering::Relaxed);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// SMPTE 2022-7 dual-leg RTP output
// ---------------------------------------------------------------------------

/// Dual-leg RTP output with SMPTE 2022-7 duplication.
///
/// Sends every packet to two independent UDP destinations. If FEC encoding
/// is enabled, FEC packets are also sent to both legs. Only leg 1 increments
/// `packets_sent`/`bytes_sent` to avoid double-counting.
async fn rtp_output_redundant_loop(
    config: &RtpOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Leg 1
    let (socket1, dest1) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
    )
    .await?;

    // Leg 2
    let leg2_dscp = redundancy.dscp.unwrap_or(config.dscp);
    let (socket2, dest2) = create_udp_output(
        &redundancy.dest_addr,
        redundancy.bind_addr.as_deref(),
        redundancy.interface_addr.as_deref(),
        leg2_dscp,
    )
    .await?;

    tracing::info!(
        "RTP output '{}' started with 2022-7 redundancy: leg1={} leg2={}",
        config.id,
        dest1,
        dest2
    );

    // FEC encoder (shared — same repair data sent to both legs)
    let fec_sent_counter = Arc::new(AtomicU64::new(0));
    let mut fec_encoder = config.fec_encode.as_ref().map(|fec_config| {
        tracing::info!(
            "FEC encode enabled for output '{}': L={} D={}",
            config.id,
            fec_config.columns,
            fec_config.rows
        );
        FecEncoder::new(
            fec_config.columns,
            fec_config.rows,
            fec_sent_counter.clone(),
        )
    });

    // Raw TS re-alignment buffer (shared for both legs)
    let mut ts_realign_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut is_raw_ts_stream = false;
    let mut ts_sync_found = false;

    // Optional MPTS → SPTS program filter (shared by both legs).
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "RTP output '{}' (2022-7): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    // Optional PID remapper (shared by both legs — both legs MUST emit
    // identical payloads so the receiver merger can dedupe by seq).
    let mut pid_remapper = config.pid_map.as_ref().and_then(|m| {
        let r = TsPidRemapper::new(m);
        if r.is_active() {
            tracing::info!(
                "RTP output '{}' (2022-7): pid_map active ({} entries)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut remap_scratch: Vec<u8> = Vec::new();

    // RTP wrap state shared by both legs — both legs MUST emit identical
    // sequence numbers and SSRC so the receiver-side hitless merger can
    // dedupe by sequence number per SMPTE 2022-7.
    let mut rtp_wrap = RtpWrapState::new();
    let mut rtp_send_buf = Vec::with_capacity(RTP_HEADER_SIZE + TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE);

    // Optional output delay buffer for stream synchronization.
    let resolved = if let Some(ref delay) = config.delay {
        resolve_output_delay(delay, &config.id, frame_rate_rx, &cancel).await
    } else {
        None
    };
    let (mut delay_buf, _frame_rate_watch) = match resolved {
        Some((buf, watch)) => (Some(buf), watch),
        None => (None, None),
    };

    let delay_sleep = tokio::time::sleep(Duration::from_secs(86400));
    tokio::pin!(delay_sleep);

    loop {
        if let Some(ref db) = delay_buf {
            if let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }
        }

        let mut packets_to_send: Vec<RtpPacket> = Vec::new();

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RTP output '{}' (2022-7) stopping (cancelled)", config.id);
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        let packet = if let Some(ref mut filter) = program_filter {
                            match filter.filter_packet(&packet, &mut filter_scratch) {
                                Some(filtered) => RtpPacket {
                                    data: filtered,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                },
                                None => continue,
                            }
                        } else {
                            packet
                        };

                        let packet = if let Some(ref mut remapper) = pid_remapper {
                            match remapper.process_packet(&packet, &mut remap_scratch) {
                                Some(remapped) => RtpPacket {
                                    data: remapped,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                },
                                None => continue,
                            }
                        } else {
                            packet
                        };

                        if let Some(ref mut db) = delay_buf {
                            db.push(packet);
                        } else {
                            packets_to_send.push(packet);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "RTP output '{}' (2022-7) lagged, dropped {n} packets",
                            config.id
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("RTP output '{}' (2022-7) broadcast channel closed", config.id);
                        break;
                    }
                }
            }
            _ = &mut delay_sleep, if delay_buf.as_ref().map_or(false, |db| db.len() > 0) => {
                let db = delay_buf.as_mut().unwrap();
                let now = now_us();
                for packet in db.drain_ready(now) {
                    packets_to_send.push(packet);
                }
            }
        }

        for packet in &packets_to_send {
            if packet.is_raw_ts {
                if !is_raw_ts_stream {
                    is_raw_ts_stream = true;
                    tracing::info!(
                        "RTP output '{}': raw TS detected, enabling TS re-alignment ({} TS pkts/datagram)",
                        config.id,
                        TS_PACKETS_PER_DATAGRAM
                    );
                }

                ts_realign_buf.extend_from_slice(&packet.data);

                if !ts_sync_found {
                    let min_bytes = TS_SYNC_CONFIRM_COUNT * TS_PACKET_SIZE;
                    if ts_realign_buf.len() >= min_bytes {
                        let mut found_offset = None;
                        for offset in 0..TS_PACKET_SIZE {
                            let all_sync = (0..TS_SYNC_CONFIRM_COUNT).all(|i| {
                                let pos = offset + i * TS_PACKET_SIZE;
                                pos < ts_realign_buf.len()
                                    && ts_realign_buf[pos] == TS_SYNC_BYTE
                            });
                            if all_sync {
                                found_offset = Some(offset);
                                break;
                            }
                        }

                        if let Some(offset) = found_offset {
                            if offset > 0 {
                                let _ = ts_realign_buf.split_to(offset);
                            }
                            ts_sync_found = true;
                        } else {
                            ts_realign_buf.clear();
                        }
                    }
                }

                if ts_sync_found {
                    while ts_realign_buf.len() >= ts_datagram_size {
                        let datagram = ts_realign_buf.split_to(ts_datagram_size);
                        let header = rtp_wrap.build_header(packet.rtp_timestamp);
                        rtp_send_buf.clear();
                        rtp_send_buf.extend_from_slice(&header);
                        rtp_send_buf.extend_from_slice(&datagram);
                        if let Ok(sent) = socket1.send_to(&rtp_send_buf, dest1).await {
                            stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                            stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                            stats.record_latency(packet.recv_time_us);
                        }
                        let _ = socket2.send_to(&rtp_send_buf, dest2).await;
                    }
                }
            } else {
                if let Ok(sent) = socket1.send_to(&packet.data, dest1).await {
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    stats.record_latency(packet.recv_time_us);
                }
                let _ = socket2.send_to(&packet.data, dest2).await;
            }

            // FEC: generate and send to both legs
            if let Some(ref mut encoder) = fec_encoder {
                let rtp_header_len = 12;
                let payload = if packet.data.len() > rtp_header_len {
                    &packet.data[rtp_header_len..]
                } else {
                    &packet.data[..]
                };

                let fec_packets = encoder.process(packet.sequence_number, payload);
                for fec_pkt in &fec_packets {
                    if let Ok(sent) = socket1.send_to(fec_pkt, dest1).await {
                        stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    }
                    let _ = socket2.send_to(fec_pkt, dest2).await;
                }
                let sent = fec_sent_counter.load(Ordering::Relaxed);
                stats.fec_packets_sent.store(sent, Ordering::Relaxed);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 2250 RTP header validation: V=2, PT=33 (MP2T), monotonic seq,
    /// stable SSRC across calls.
    #[test]
    fn rtp_wrap_state_builds_rfc2250_header() {
        let mut s = RtpWrapState::new();
        let initial_ssrc = s.ssrc;
        let h1 = s.build_header(0x12345678);
        let h2 = s.build_header(0x12345679);

        // Byte 0: V=2 (top 2 bits = 10), P=0, X=0, CC=0  → 0x80
        assert_eq!(h1[0], 0x80);
        assert_eq!(h2[0], 0x80);

        // Byte 1: M=0, PT=33 (MP2T)  → 0x21
        assert_eq!(h1[1], 33);
        assert_eq!(h2[1], 33);

        // Sequence number must advance by exactly 1
        let seq1 = u16::from_be_bytes([h1[2], h1[3]]);
        let seq2 = u16::from_be_bytes([h2[2], h2[3]]);
        assert_eq!(seq2.wrapping_sub(seq1), 1);

        // Timestamp written as big-endian u32
        assert_eq!(u32::from_be_bytes([h1[4], h1[5], h1[6], h1[7]]), 0x12345678);
        assert_eq!(u32::from_be_bytes([h2[4], h2[5], h2[6], h2[7]]), 0x12345679);

        // SSRC stable across calls and big-endian encoded
        let ssrc1 = u32::from_be_bytes([h1[8], h1[9], h1[10], h1[11]]);
        let ssrc2 = u32::from_be_bytes([h2[8], h2[9], h2[10], h2[11]]);
        assert_eq!(ssrc1, initial_ssrc);
        assert_eq!(ssrc2, initial_ssrc);
    }

    /// Sequence wraps cleanly past 0xFFFF.
    #[test]
    fn rtp_wrap_state_sequence_wraps() {
        let mut s = RtpWrapState::new();
        s.seq = 0xFFFF;
        let h1 = s.build_header(0);
        let h2 = s.build_header(0);
        assert_eq!(u16::from_be_bytes([h1[2], h1[3]]), 0xFFFF);
        assert_eq!(u16::from_be_bytes([h2[2], h2[3]]), 0x0000);
    }
}
