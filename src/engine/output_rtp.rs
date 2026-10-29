// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::wire_emit::{AnchorSource, WireDatagram, WirePacingClass, WireTxHandle, spawn_wire_emitter};

use crate::config::models::{FecConfig, RtpOutputConfig};
use crate::fec::encoder::{FEC_INDEXED_GROUPS, FecEncoder};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;
use crate::util::time::now_us;

use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::ts_pid_overrides_rewriter::TsPidOverridesRewriter;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

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
    /// Last non-zero RTP timestamp emitted. Edge-generated filler bundles
    /// (dead-input NULL keepalives, partial-bundle flush padding, injected
    /// PSI) carry `rtp_timestamp: 0` on their `RtpPacket` — stamping that
    /// raw onto the wire reads as a ±300 s clock jump to receivers that
    /// de-jitter on the RTP timestamp (observed live 2026-06-06: 30 ts=0
    /// datagrams in 180 s flushed downstream jitter buffers). RFC 2250's
    /// timestamp is "target transmission time", so carrying the previous
    /// value forward for filler keeps the wire clock monotonic and honest.
    last_ts: u32,
}

impl RtpWrapState {
    fn new() -> Self {
        Self {
            seq: rand::random::<u16>(),
            ssrc: rand::random::<u32>(),
            last_ts: 0,
        }
    }

    /// Build a 12-byte RFC 2250 RTP header for an MPEG-TS payload (PT=33).
    /// `rtp_ts` is the 32-bit RTP timestamp at the 90 kHz MPEG clock; pass
    /// the value derived from the upstream `RtpPacket::rtp_timestamp` so the
    /// wire timing reflects the source media clock rather than wall time.
    /// A zero `rtp_ts` (timestamp-less filler) reuses the previous packet's
    /// timestamp instead — see `last_ts`. Sequence number advances by 1 per
    /// call.
    fn build_header(&mut self, rtp_ts: u32) -> [u8; RTP_HEADER_SIZE] {
        let rtp_ts = if rtp_ts == 0 { self.last_ts } else { rtp_ts };
        self.last_ts = rtp_ts;
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

/// The FEC encoder an output with `fec_encode` runs, with a warning when
/// its matrix is wider or deeper than the FEC header can name: columns and
/// rows from [`FEC_INDEXED_GROUPS`] up carry no FEC of their own.
fn new_fec_encoder(
    output_id: &str,
    fec: &FecConfig,
    fec_sent_counter: Arc<AtomicU64>,
) -> FecEncoder {
    tracing::info!(
        "FEC encode enabled for output '{}': L={} D={}",
        output_id,
        fec.columns,
        fec.rows
    );
    if let Some(lost) = fec_groups_past_the_index(fec.columns, fec.rows) {
        tracing::warn!(
            "RTP output '{}': FEC {}x{} is larger than the FEC header can name ({n} columns, \
             {n} rows): {lost}; use at most {n} columns and {n} rows",
            output_id,
            fec.columns,
            fec.rows,
            n = FEC_INDEXED_GROUPS,
        );
    }
    FecEncoder::new(fec.columns, fec.rows, fec_sent_counter)
}

/// What an L x D matrix loses to the 3-bit FEC header index, or `None` when
/// every column and row it sends FEC for can be named. A one-column matrix
/// sends no row FEC at all, so its row count costs nothing.
fn fec_groups_past_the_index(columns: u8, rows: u8) -> Option<&'static str> {
    let cols_lost = columns > FEC_INDEXED_GROUPS;
    let rows_lost = rows > FEC_INDEXED_GROUPS && columns > 1;
    match (cols_lost, rows_lost) {
        (true, true) => Some(
            "columns from 8 up get no column FEC and rows from 8 up no row FEC, \
             so a packet in both is unprotected",
        ),
        (true, false) => Some("columns from 8 up get no column FEC, so their packets are protected by their row only"),
        (false, true) => Some("rows from 8 up get no row FEC, so their packets are protected by their column only"),
        (false, false) => None,
    }
}

/// SMPTE 2022-1 protection for one media datagram, exactly as it goes to
/// the wire.
///
/// `fec::decoder` files a received datagram under the sequence number in
/// its RTP header and XORs every byte after the first 12, so the encoder is
/// given those two things -- not the broadcast item the datagram was made
/// from, whose sequence number is the input's and whose bytes need not map
/// one to one onto datagrams (edge#206). A header-only datagram is never
/// filed by the decoder, so it is left out here too: its column and row go
/// unprotected for that matrix on both ends.
fn fec_for_datagram(encoder: &mut FecEncoder, datagram: &[u8]) -> Vec<Bytes> {
    if datagram.len() <= RTP_HEADER_SIZE {
        return Vec::new();
    }
    let seq = u16::from_be_bytes([datagram[2], datagram[3]]);
    encoder.process(seq, &datagram[RTP_HEADER_SIZE..])
}

/// A datagram for the wire-emit queue. FEC rides the same queue: its bytes
/// past the RTP-sized offset are XOR parity, so the pacer's PCR scan finds
/// nothing and treats it as a between-PCR datagram (emit ASAP, riding the
/// previous PCR anchor).
fn wire_datagram(bytes: Bytes, recv_time_us: u64) -> WireDatagram {
    WireDatagram {
        bytes,
        recv_time_us,
        enqueue_us: 0,
        target_tx_time_ns: None,
        ts_offset: RTP_HEADER_SIZE,
    }
}

/// Send one media datagram on a single-leg output, then the FEC it
/// completes. The FEC is computed before the send, so a datagram the queue
/// then drops is still protected: the receiver sees a loss FEC can repair,
/// and every later cell keeps its place. A drop of either counts in
/// `packets_dropped`.
fn send_single(
    wire_tx: &WireTxHandle,
    fec_encoder: Option<&mut FecEncoder>,
    bytes: Bytes,
    recv_time_us: u64,
    stats: &OutputStatsAccumulator,
) {
    let repair = fec_encoder.map(|e| fec_for_datagram(e, &bytes));
    if wire_tx.try_send(wire_datagram(bytes, recv_time_us)).is_err() {
        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
    }
    for fec_pkt in repair.into_iter().flatten() {
        if wire_tx.try_send(wire_datagram(fec_pkt, recv_time_us)).is_err() {
            stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Send one media datagram on both legs of a 2022-7 output (the same bytes,
/// a refcount bump), then the FEC it completes on both. Only a leg-1 media
/// drop counts in `packets_dropped`, as leg 1 alone reports.
fn send_dual(
    leg1: &WireTxHandle,
    leg2: &WireTxHandle,
    fec_encoder: Option<&mut FecEncoder>,
    bytes: Bytes,
    recv_time_us: u64,
    stats: &OutputStatsAccumulator,
) {
    let repair = fec_encoder.map(|e| fec_for_datagram(e, &bytes));
    if leg1.try_send(wire_datagram(bytes.clone(), recv_time_us)).is_err() {
        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
    }
    let _ = leg2.try_send(wire_datagram(bytes, recv_time_us));
    for fec_pkt in repair.into_iter().flatten() {
        let _ = leg1.try_send(wire_datagram(fec_pkt.clone(), recv_time_us));
        let _ = leg2.try_send(wire_datagram(fec_pkt, recv_time_us));
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
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("rtp".to_string()),
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

    tokio::spawn(async move {
        let result = if config.redundancy.is_some() {
            rtp_output_redundant_loop(&config, &mut rx, output_stats, cancel, frame_rate_rx, active_input_rx).await
        } else {
            rtp_output_loop(&config, &mut rx, output_stats, cancel, frame_rate_rx, &events, active_input_rx).await
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
/// address. If FEC encoding is enabled, each FEC repair packet is generated
/// from the datagrams as sent and follows the datagram that completes it.
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
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    let (socket, dest) =
        create_udp_output(&config.dest_addr, config.bind_addr.as_deref(), config.interface_addr.as_deref(), config.dscp, config.interface_binding.as_ref()).await?;

    tracing::info!(
        "RTP output '{}' started -> {}",
        config.id,
        dest
    );

    // ── PCR-anchored wire pacing (engine::wire_emit) ─────────────────
    //
    // The encoder task wraps each datagram with an RTP header (using
    // the local `RtpWrapState` below) and hands the wrapped Bytes to
    // a dedicated wire-emit thread for paced delivery. Single emitter
    // owns the std blocking socket — FEC packets and media packets
    // flow through the same emitter on the same socket.
    let std_socket = socket
        .into_std()
        .map_err(|e| anyhow::anyhow!("convert tokio UdpSocket -> std: {e}"))?;
    std_socket.set_nonblocking(false)?;
    // Epoch-locked egress: ms -> ns once, here, so the hot path never
    // divides. `None` leaves wire_emit on its classic relative anchor.
    let epoch_lock_params = config.epoch_lock.as_ref().map(|e| {
        crate::engine::wire_emit::EpochLockParams {
            offset_ns: e.egress_offset_ms as u64 * 1_000_000,
            group_label: e.group_label.clone(),
            anchor: e.source_anchor.map(Into::into),
            anchor_cell: Some(stats.epoch_anchor_cell.clone()),
        }
    });
    let wire_tx = spawn_wire_emitter(
        config.id.clone(),
        std_socket,
        dest,
        AnchorSource::Pcr,
        WirePacingClass::Lossless,
        config.egress_pacing.unwrap_or_default(),
        config.egress_buffer_ms,
        epoch_lock_params.clone(),
        stats.clone(),
        cancel.clone(),
    );

    // RTP wrap state lives on the encoder task. Sequence + SSRC are
    // owned here; FEC is computed over the sequence numbers and bytes of
    // the datagrams this builds (`send_single`), never the input's.
    let mut rtp_wrap = RtpWrapState::new();

    // Optional FEC encoder (SMPTE 2022-1).
    //
    // Like the input-side FEC decoder, we use a shared `Arc<AtomicU64>`
    // counter so the encoder can increment it internally and we periodically
    // copy the value back to the stats accumulator without borrow conflicts.
    let fec_sent_counter = Arc::new(AtomicU64::new(0));
    let mut fec_encoder = config
        .fec_encode
        .as_ref()
        .map(|fec| new_fec_encoder(&config.id, fec, fec_sent_counter.clone()));

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

    // Per-program role-keyed PID rewriter. Single owner of PID remapping
    // on both passthrough and transcoded paths — the transcode replacers
    // re-encode on the source audio / video PID and let this stage handle
    // every PAT/PMT/ES PID rename, including for programs other than 1.
    let mut pid_overrides_rewriter = config.pid_overrides.as_ref().and_then(|m| {
        let r = TsPidOverridesRewriter::new(m);
        if r.is_active() {
            tracing::info!(
                "RTP output '{}': pid_overrides rewriter active ({} programs)",
                config.id,
                m.len()
            );
            Some(r)
        } else {
            None
        }
    });
    let mut overrides_scratch: Vec<u8> = Vec::new();

    // Transcoding chain on a dedicated codec thread. See output_udp.rs
    // for full rationale.
    let mut transcode_chain = match crate::engine::transcode_chain::build_for_output(
        &config.id,
        config.audio_encode.as_ref(),
        config.video_encode.as_ref(),
        config.transcode.clone(),
        &stats,
        None, // RTP backpressure not yet wired
        Some(events),
    ) {
        Ok(chain) => {
            if let Some(ref c) = chain {
                tracing::info!(
                    "RTP output '{}': transcode chain active ({})",
                    config.id,
                    c.target_description()
                );
                if config.audio_encode.is_some() {
                    events.emit_output_with_details(
                        EventSeverity::Info,
                        category::AUDIO_ENCODE,
                        format!("TS audio encoder started: output '{}'", config.id),
                        &config.id,
                        serde_json::json!({
                            "codec": config.audio_encode.as_ref().map(|e| &e.codec)
                        }),
                    );
                }
                if config.video_encode.is_some() {
                    events.emit_output_with_details(
                        EventSeverity::Info,
                        category::VIDEO_ENCODE,
                        format!("Video encoder started: output '{}'", config.id),
                        &config.id,
                        serde_json::json!({
                            "codec": config.video_encode.as_ref().map(|e| &e.codec)
                        }),
                    );
                }
            }
            chain
        }
        Err(e) => {
            tracing::error!(
                "RTP output '{}': transcode chain rejected: {e}; transcoding disabled",
                config.id
            );
            let (cat, label) = match e {
                crate::engine::transcode_chain::TranscodeChainError::Audio(_) => {
                    (category::AUDIO_ENCODE, "TS audio encoder")
                }
                crate::engine::transcode_chain::TranscodeChainError::Video(_) => {
                    (category::VIDEO_ENCODE, "Video encoder")
                }
            };
            events.emit_output_with_details(
                EventSeverity::Critical,
                cat,
                format!("{label} failed: output '{}': {e}", config.id),
                &config.id,
                serde_json::json!({ "error": e.to_string() }),
            );
            None
        }
    };

    // Optional CBR null padder. See `engine::ts_null_padder`.
    let mut null_padder = config
        .cbr_pad_to_kbps
        .map(crate::engine::ts_null_padder::TsNullPadder::new);
    let mut pad_scratch: Vec<u8> = Vec::new();

    // Per-output input-switch watcher. See output_udp.rs for the
    // detailed rationale.
    let mut switch_handles: Vec<Arc<std::sync::atomic::AtomicBool>> = Vec::new();
    if let Some(ref chain) = transcode_chain {
        if let Some(h) = chain.audio_external_reset_handle() {
            switch_handles.push(h);
        }
        if let Some(h) = chain.video_external_reset_handle() {
            switch_handles.push(h);
        }
    }
    crate::engine::input_switch_watcher::spawn(
        config.id.clone(),
        active_input_rx,
        switch_handles,
        cancel.clone(),
    );

    // RTP wrap state lives in the wire task now (it builds RFC 2250
    // headers at emit time so the timestamp tracks the actual wire
    // egress moment, and it pads with null packets at CBR before
    // wrapping each datagram).

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

    // Reused per-iteration send scratch. Hoisted out of the loop so the
    // per-packet hot path does not allocate a fresh Vec on every select
    // branch (at 50 Mbps that would be ~3k allocs/sec per output).
    let mut packets_to_send: Vec<RtpPacket> = Vec::with_capacity(4);

    loop {
        // Reset the delay timer to fire when the oldest buffered packet
        // is due for release. Only meaningful when delay is active.
        if let Some(ref db) = delay_buf
            && let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }

        packets_to_send.clear();

        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(ref db) = delay_buf
                    && db.len() > 0 {
                        tracing::debug!(
                            "RTP output '{}' cancelled with {} buffered packets",
                            config.id, db.len()
                        );
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
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
                                },
                                None => continue,
                            }
                        } else {
                            packet
                        };

                        // Per-program role-keyed PID rewrite (passthrough only).
                        let packet = if let Some(ref mut rw) = pid_overrides_rewriter {
                            match rw.process_packet(&packet, &mut overrides_scratch) {
                                Some(rewritten) => RtpPacket {
                                    data: rewritten,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
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
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
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
            _ = &mut delay_sleep, if delay_buf.as_ref().is_some_and(|db| db.len() > 0) => {
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
            let force_raw_ts = transcode_chain.is_some();

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

                // If transcode chain is active, submit the ts_input
                // bytes (non-blocking) and drain any ready transcoded
                // bytes back into the realign buffer through the same
                // padder. The transcoded output may arrive across one
                // or more iterations; this loop continues to consume
                // from ts_realign_buf normally.
                if let Some(ref chain) = transcode_chain {
                    if chain.try_submit(Bytes::copy_from_slice(ts_input)).is_err() {
                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    while let Some(transcoded) = transcode_chain.as_mut().and_then(|c| c.try_recv())
                    {
                        if let Some(ref mut padder) = null_padder {
                            pad_scratch.clear();
                            padder.process(&transcoded, &mut pad_scratch);
                            ts_realign_buf.extend_from_slice(&pad_scratch);
                        } else {
                            ts_realign_buf.extend_from_slice(&transcoded);
                        }
                    }
                } else {
                    if let Some(ref mut padder) = null_padder {
                        pad_scratch.clear();
                        padder.process(ts_input, &mut pad_scratch);
                        ts_realign_buf.extend_from_slice(&pad_scratch);
                    } else {
                        ts_realign_buf.extend_from_slice(ts_input);
                    }
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
                        let header = rtp_wrap.build_header(packet.rtp_timestamp);
                        let mut buf = Vec::with_capacity(RTP_HEADER_SIZE + ts_datagram_size);
                        buf.extend_from_slice(&header);
                        buf.extend_from_slice(&datagram);
                        send_single(
                            &wire_tx,
                            fec_encoder.as_mut(),
                            Bytes::from(buf),
                            packet.recv_time_us,
                            &stats,
                        );
                    }
                }
            } else {
                // Already-RTP-wrapped data: strip the upstream header,
                // wrap with our local SSRC + seq so receivers see a
                // coherent stream from us regardless of upstream
                // synthesis quirks.
                let inner_ts = if packet.data.len() > crate::engine::ts_parse::RTP_HEADER_MIN_SIZE {
                    &packet.data[crate::engine::ts_parse::RTP_HEADER_MIN_SIZE..]
                } else {
                    &[][..]
                };
                let header = rtp_wrap.build_header(packet.rtp_timestamp);
                let mut buf = Vec::with_capacity(RTP_HEADER_SIZE + inner_ts.len());
                buf.extend_from_slice(&header);
                buf.extend_from_slice(inner_ts);
                send_single(
                    &wire_tx,
                    fec_encoder.as_mut(),
                    Bytes::from(buf),
                    packet.recv_time_us,
                    &stats,
                );
            }

            // FEC went out with each datagram it protects (`send_single`);
            // publish the running count. FEC packets carry the BCFE magic,
            // not an RTP header, and ride the same wire-emit instance as the
            // media -- single socket owner, paced as non-PCR datagrams.
            if fec_encoder.is_some() {
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
    // Redundant 2022-7 path is passthrough-only (no replacers); the
    // input-switch watcher would have nothing to flip. Accepted for
    // signature parity with `rtp_output_loop`.
    _active_input_rx: tokio::sync::watch::Receiver<String>,
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
        config.interface_binding.as_ref(),
    )
    .await?;

    // Leg 2
    let leg2_dscp = redundancy.dscp.unwrap_or(config.dscp);
    let (socket2, dest2) = create_udp_output(
        &redundancy.dest_addr,
        redundancy.bind_addr.as_deref(),
        redundancy.interface_addr.as_deref(),
        leg2_dscp,
        redundancy.interface_binding.as_ref(),
    )
    .await?;

    tracing::info!(
        "RTP output '{}' started with 2022-7 redundancy: leg1={} leg2={}",
        config.id,
        dest1,
        dest2
    );

    // ── PCR-anchored wire pacing for both legs ───────────────────────
    //
    // Both legs receive byte-identical RTP datagrams (shared
    // RtpWrapState below) and pace independently. PCR-anchored
    // closed-loop on observed inter-PCR rate; SO_TXTIME on Linux
    // ≥ 4.19, clock_nanosleep fallback otherwise. Same input cadence
    // → same observed rate → same per-packet wallclock target. Sub-µs
    // cross-leg scheduling skew is well within 2022-7 receiver dedup
    // tolerance.
    //
    // Leg 2's stats are private and never surfaced — keeps the
    // existing observable semantics (only leg 1 contributes to
    // packets_sent / bytes_sent).
    let std_socket1 = socket1
        .into_std()
        .map_err(|e| anyhow::anyhow!("convert leg1 tokio UdpSocket -> std: {e}"))?;
    std_socket1.set_nonblocking(false)?;
    let std_socket2 = socket2
        .into_std()
        .map_err(|e| anyhow::anyhow!("convert leg2 tokio UdpSocket -> std: {e}"))?;
    std_socket2.set_nonblocking(false)?;
    let leg2_stats = Arc::new(OutputStatsAccumulator::new(
        format!("{}-leg2", config.id),
        format!("{}-leg2", config.id),
        "rtp_2022_7_leg2".to_string(),
    ));
    // Epoch-locked egress: ms -> ns once, here, so the hot path never
    // divides. `None` leaves wire_emit on its classic relative anchor.
    let epoch_lock_params = config.epoch_lock.as_ref().map(|e| {
        crate::engine::wire_emit::EpochLockParams {
            offset_ns: e.egress_offset_ms as u64 * 1_000_000,
            group_label: e.group_label.clone(),
            anchor: e.source_anchor.map(Into::into),
            anchor_cell: Some(stats.epoch_anchor_cell.clone()),
        }
    });
    let wire_tx_leg1 = spawn_wire_emitter(
        format!("{}-leg1", config.id),
        std_socket1,
        dest1,
        AnchorSource::Pcr,
        WirePacingClass::Lossless,
        config.egress_pacing.unwrap_or_default(),
        config.egress_buffer_ms,
        epoch_lock_params.clone(),
        stats.clone(),
        cancel.clone(),
    );
    let wire_tx_leg2 = spawn_wire_emitter(
        format!("{}-leg2", config.id),
        std_socket2,
        dest2,
        AnchorSource::Pcr,
        WirePacingClass::Lossless,
        config.egress_pacing.unwrap_or_default(),
        config.egress_buffer_ms,
        epoch_lock_params.clone(),
        leg2_stats,
        cancel.clone(),
    );

    // FEC encoder (shared — same repair data sent to both legs)
    let fec_sent_counter = Arc::new(AtomicU64::new(0));
    let mut fec_encoder = config
        .fec_encode
        .as_ref()
        .map(|fec| new_fec_encoder(&config.id, fec, fec_sent_counter.clone()));

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

    // Reused per-iteration send scratch — see rtp_output_loop for rationale.
    let mut packets_to_send: Vec<RtpPacket> = Vec::with_capacity(4);

    loop {
        if let Some(ref db) = delay_buf
            && let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }

        packets_to_send.clear();

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
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
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
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
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
            _ = &mut delay_sleep, if delay_buf.as_ref().is_some_and(|db| db.len() > 0) => {
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
                        let mut buf = Vec::with_capacity(RTP_HEADER_SIZE + ts_datagram_size);
                        buf.extend_from_slice(&header);
                        buf.extend_from_slice(&datagram);
                        // Both legs receive the same Bytes (refcount
                        // bump only — no copy).
                        send_dual(
                            &wire_tx_leg1,
                            &wire_tx_leg2,
                            fec_encoder.as_mut(),
                            Bytes::from(buf),
                            packet.recv_time_us,
                            &stats,
                        );
                    }
                }
            } else {
                // Already-RTP data goes out as it came, upstream header
                // and all, so FEC is over the upstream sequence numbers --
                // which may skip, repeat or step back; the encoder files
                // each datagram by its number, as the decoder does.
                send_dual(
                    &wire_tx_leg1,
                    &wire_tx_leg2,
                    fec_encoder.as_mut(),
                    Bytes::from(packet.data.to_vec()),
                    packet.recv_time_us,
                    &stats,
                );
            }

            // FEC went out on both legs with each datagram it protects
            // (`send_dual`); publish the running count.
            if fec_encoder.is_some() {
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

    fn ts_of(h: &[u8; RTP_HEADER_SIZE]) -> u32 {
        u32::from_be_bytes([h[4], h[5], h[6], h[7]])
    }

    /// Timestamp-less filler (`rtp_ts == 0` from NULL keepalives /
    /// partial-bundle flush padding) must not stamp a raw 0 onto the wire
    /// mid-stream — it reuses the previous packet's timestamp so the wire
    /// clock stays monotonic (observed live 2026-06-06: ts=0 datagrams
    /// read as ±300 s clock jumps to RFC 3550 jitter estimators).
    #[test]
    fn rtp_wrap_state_carries_timestamp_forward_for_filler() {
        let mut s = RtpWrapState::new();
        // Mid-stream filler inherits the previous timestamp.
        assert_eq!(ts_of(&s.build_header(27_000_000)), 27_000_000);
        assert_eq!(ts_of(&s.build_header(0)), 27_000_000);
        assert_eq!(ts_of(&s.build_header(0)), 27_000_000);
        // Real timestamps resume unchanged.
        assert_eq!(ts_of(&s.build_header(27_003_600)), 27_003_600);
        // Before any real timestamp has been seen, 0 still emits 0
        // (cold start — nothing better to claim).
        let mut cold = RtpWrapState::new();
        assert_eq!(ts_of(&cold.build_header(0)), 0);
    }

    // ── edge#206: FEC over the datagrams as sent ────────────────────────

    use crate::fec::decoder::{FecDecoder, is_fec_packet};
    use crate::fec::{FEC_HEADER_LEN, FEC_MAGIC};

    /// One RTP datagram the way the output builds it.
    fn datagram(wrap: &mut RtpWrapState, payload: &[u8]) -> Vec<u8> {
        let mut dg = wrap.build_header(90_000).to_vec();
        dg.extend_from_slice(payload);
        dg
    }

    fn seq_of(dg: &[u8]) -> u16 {
        u16::from_be_bytes([dg[2], dg[3]])
    }

    /// (SNBase, is_column, index, parity) of a FEC datagram.
    fn fec_fields(f: &[u8]) -> (u16, bool, u8, &[u8]) {
        let b = &f[FEC_MAGIC.len()..];
        (u16::from_be_bytes([b[0], b[1]]), b[8] & 0x40 != 0, b[8] & 0x07, &b[FEC_HEADER_LEN..])
    }

    /// The start-up warning names only what a shape actually loses to the
    /// 3-bit header index.
    #[test]
    fn the_index_warning_names_only_what_is_lost() {
        assert_eq!(fec_groups_past_the_index(8, 8), None);
        assert_eq!(fec_groups_past_the_index(1, 20), None, "L=1 sends no row FEC");
        assert!(fec_groups_past_the_index(10, 10).unwrap().contains("a packet in both is unprotected"));
        assert!(fec_groups_past_the_index(10, 4).unwrap().contains("by their row only"));
        assert!(fec_groups_past_the_index(4, 10).unwrap().contains("by their column only"));
    }

    /// The FEC goes out under the wire sequence number and covers the bytes
    /// after the RTP header -- not the input item's number or bytes.
    #[test]
    fn fec_for_datagram_uses_the_wire_seq_and_bytes() {
        let mut wrap = RtpWrapState::new();
        wrap.seq = 0xFFFE; // across the wrap
        let mut enc = FecEncoder::new(1, 4, Arc::new(AtomicU64::new(0)));
        let mut sent = Vec::new();
        let mut fec = Vec::new();
        for i in 0..4u8 {
            let dg = datagram(&mut wrap, &[i.wrapping_mul(37) ^ 0x47; 1316]);
            fec.extend(fec_for_datagram(&mut enc, &dg));
            sent.push(dg);
        }
        assert_eq!(fec.len(), 1);
        let (sn_base, is_column, index, parity) = fec_fields(&fec[0]);
        assert_eq!(sn_base, 0xFFFE);
        assert!(is_column);
        assert_eq!(index, 0);
        let mut xor = vec![0u8; 1316];
        for dg in &sent {
            for (x, b) in xor.iter_mut().zip(&dg[RTP_HEADER_SIZE..]) {
                *x ^= b;
            }
        }
        assert_eq!(parity, &xor[..]);
    }

    /// A header-only datagram is never filed by the decoder, so the encoder
    /// leaves it out too: its matrix sends nothing that covers it, and the
    /// next matrix starts at the same number on both ends.
    #[test]
    fn fec_for_datagram_leaves_header_only_datagrams_unprotected() {
        let mut wrap = RtpWrapState::new();
        let first = wrap.seq;
        let mut enc = FecEncoder::new(1, 4, Arc::new(AtomicU64::new(0)));
        let mut fec = Vec::new();
        for i in 0..8u16 {
            let payload: &[u8] = if i == 1 { &[] } else { &[0x47; 188] };
            fec.extend(fec_for_datagram(&mut enc, &datagram(&mut wrap, payload)));
        }
        assert_eq!(fec.len(), 1, "only the second matrix is protected");
        assert_eq!(fec_fields(&fec[0]).0, first.wrapping_add(4));
    }

    // Loopback runs of the real output loops: what goes on the wire, then a
    // FEC decoder replaying it with one datagram lost.

    /// `n` 188-byte TS packets on PID 0x100, each with distinct payload bytes.
    fn ts_stream(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n * TS_PACKET_SIZE);
        for k in 0..n {
            out.extend_from_slice(&[TS_SYNC_BYTE, 0x01, 0x00, 0x10 | (k & 0x0F) as u8]);
            out.extend((0..184).map(|j| (k * 7 + j * 13) as u8 ^ (k >> 8) as u8));
        }
        out
    }

    fn item(data: Vec<u8>, seq: u16, is_raw_ts: bool) -> RtpPacket {
        RtpPacket {
            data: Bytes::from(data),
            sequence_number: seq,
            rtp_timestamp: 90_000 + seq as u32 * 3_000,
            recv_time_us: now_us(),
            is_raw_ts,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        }
    }

    /// Raw-TS items cut from `ts` at the sizes `sizes` cycles through, with
    /// an input sequence counter that has nothing to do with the wire's.
    fn raw_ts_items(ts: &[u8], sizes: &[usize]) -> Vec<RtpPacket> {
        let mut items = Vec::new();
        let mut at = 0;
        for k in 0.. {
            if at >= ts.len() {
                break;
            }
            let end = (at + sizes[k % sizes.len()]).min(ts.len());
            items.push(item(ts[at..end].to_vec(), 5_000 + k as u16, true));
            at = end;
        }
        items
    }

    /// An RTP datagram from upstream: a foreign SSRC and its own numbers.
    fn upstream_rtp(seq: u16, payload: &[u8]) -> RtpPacket {
        let mut dg = vec![0x80, 33, (seq >> 8) as u8, seq as u8, 0, 0, 0x10, 0, 0xDE, 0xAD, 0xBE, 0xEF];
        dg.extend_from_slice(payload);
        item(dg, seq, false)
    }

    /// Read one leg until `expect_media` media datagrams and the FEC after
    /// them have arrived.
    fn drain_leg(sock: &std::net::UdpSocket, expect_media: usize) -> Vec<Vec<u8>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut buf = vec![0u8; 4096];
        let (mut out, mut media) = (Vec::new(), 0);
        loop {
            let quiet = if media >= expect_media { 300 } else { 1_000 };
            sock.set_read_timeout(Some(Duration::from_millis(quiet))).unwrap();
            match sock.recv(&mut buf) {
                Ok(n) => {
                    if !is_fec_packet(&buf[..n]) {
                        media += 1;
                    }
                    out.push(buf[..n].to_vec());
                }
                Err(_) if media >= expect_media || std::time::Instant::now() > deadline => break,
                Err(_) => {}
            }
        }
        out
    }

    /// Run `items` through a real RTP output with `fec_encode` L x D
    /// (2022-7 with `dual`) on loopback; returns each leg's datagrams in
    /// arrival order.
    async fn capture(l: u8, d: u8, dual: bool, items: Vec<RtpPacket>, expect_media: usize) -> Vec<Vec<Vec<u8>>> {
        let legs: Vec<std::net::UdpSocket> = (0..if dual { 2 } else { 1 })
            .map(|_| {
                let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                socket2::SockRef::from(&s).set_recv_buffer_size(32 << 20).unwrap();
                s
            })
            .collect();
        let mut config = serde_json::json!({
            "id": "fec-206",
            "name": "fec-206",
            "dest_addr": legs[0].local_addr().unwrap().to_string(),
            "fec_encode": { "columns": l, "rows": d },
        });
        if dual {
            config["redundancy"] = serde_json::json!({ "dest_addr": legs[1].local_addr().unwrap().to_string() });
        }
        let config: RtpOutputConfig = serde_json::from_value(config).unwrap();
        let (tx, _rx) = broadcast::channel(8192);
        let stats = Arc::new(OutputStatsAccumulator::new("fec-206".into(), "fec-206".into(), "rtp".into()));
        let cancel = CancellationToken::new();
        let (events, _events_rx) = crate::manager::events::event_channel();
        let (_active_tx, active_rx) = tokio::sync::watch::channel(String::new());
        let task = spawn_rtp_output(config, &tx, stats, cancel.clone(), None, events, active_rx);
        // Every leg is read from before the first item goes in, both at
        // once, and the items go in a few at a time, so no burst waits in a
        // socket buffer: on a host with the stock rmem_max (a ~416 KiB
        // buffer, about 180 of these datagrams) a burst sent ahead of its
        // reader, or faster than a reader sharing its CPU gets to run,
        // overflows it.
        let leg_count = legs.len();
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let readers = tokio::task::spawn_blocking(move || {
            std::thread::scope(|scope| {
                let readers: Vec<_> = legs
                    .iter()
                    .map(|s| {
                        let ready = ready_tx.clone();
                        scope.spawn(move || {
                            let _ = ready.send(());
                            drain_leg(s, expect_media)
                        })
                    })
                    .collect();
                readers.into_iter().map(|r| r.join().unwrap()).collect::<Vec<_>>()
            })
        });
        for _ in 0..leg_count {
            ready_rx.recv().await.expect("a leg reader started");
        }
        for (i, it) in items.into_iter().enumerate() {
            tx.send(it).unwrap();
            if i % 4 == 3 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        let captured = readers.await.unwrap();
        cancel.cancel();
        let _ = task.await;
        captured
    }

    /// Replay a capture through a fresh decoder with the media datagram
    /// `lose` left out. Asserts every packet it rebuilds carries exactly
    /// the bytes sent under that number; returns their numbers.
    fn replay(capture: &[Vec<u8>], l: u8, d: u8, lose: Option<u16>) -> Vec<u16> {
        let sent: std::collections::HashMap<u16, &Vec<u8>> = capture
            .iter()
            .filter(|dg| !is_fec_packet(dg))
            .map(|dg| (seq_of(dg), dg))
            .collect();
        let mut dec = FecDecoder::new(l, d);
        let mut rebuilt = Vec::new();
        for dg in capture {
            let out = if is_fec_packet(dg) {
                dec.process_fec(dg)
            } else if Some(seq_of(dg)) == lose {
                continue;
            } else {
                let mut out = dec.process_media(seq_of(dg), 0, &Bytes::copy_from_slice(dg));
                out.remove(0);
                out
            };
            for p in out {
                let original = sent[&p.sequence_number];
                assert_eq!(
                    &p.data[RTP_HEADER_SIZE..],
                    &original[RTP_HEADER_SIZE..],
                    "seq {} rebuilt with other bytes than were sent",
                    p.sequence_number,
                );
                rebuilt.push(p.sequence_number);
            }
        }
        rebuilt
    }

    /// The checks every contiguous capture passes: contiguous numbering,
    /// each FEC right after the datagram completing its group with the
    /// group's true parity, the expected FEC count, and single losses
    /// rebuilt byte for byte wherever a column or row FEC covers them.
    fn check_contiguous(capture: &[Vec<u8>], l: u8, d: u8, matrices: usize) {
        let (lu, du) = (l as u16, d as u16);
        let media: Vec<&Vec<u8>> = capture.iter().filter(|dg| !is_fec_packet(dg)).collect();
        assert_eq!(media.len(), matrices * lu as usize * du as usize);
        let first = seq_of(media[0]);
        for (i, dg) in media.iter().enumerate() {
            assert_eq!(seq_of(dg), first.wrapping_add(i as u16), "wire numbering is contiguous");
        }
        let by_seq: std::collections::HashMap<u16, &Vec<u8>> = media.iter().map(|dg| (seq_of(dg), *dg)).collect();
        let mut last_media = None;
        let mut fec_count = 0;
        for dg in capture {
            if !is_fec_packet(dg) {
                last_media = Some(seq_of(dg));
                continue;
            }
            fec_count += 1;
            let (base, is_column, index, parity) = fec_fields(dg);
            assert_eq!(base.wrapping_sub(first) % (lu * du), 0, "SNBase starts a matrix");
            let members: Vec<u16> = if is_column {
                (0..du).map(|r| base.wrapping_add(r * lu + index as u16)).collect()
            } else {
                (0..lu).map(|c| base.wrapping_add(index as u16 * lu + c)).collect()
            };
            let completes = if is_column { *members.last().unwrap() } else { base.wrapping_add(lu * du - 1) };
            assert_eq!(last_media, Some(completes), "FEC follows the datagram completing it");
            let mut xor = vec![0u8; parity.len()];
            for m in &members {
                for (x, b) in xor.iter_mut().zip(&by_seq[m][RTP_HEADER_SIZE..]) {
                    *x ^= b;
                }
            }
            assert_eq!(parity, &xor[..], "parity of the datagrams as sent");
        }
        let rows_fec = if l > 1 { d.min(8) as usize } else { 0 };
        assert_eq!(fec_count, matrices * (l.min(8) as usize + rows_fec));

        assert!(replay(capture, l, d, None).is_empty(), "no loss, nothing rebuilt");
        let m = first.wrapping_add(lu * du); // the second matrix
        for (r, c) in [(0, 0), (du - 1, 1.min(lu - 1)), (du - 1, lu - 1), (2, lu - 1)] {
            let lost = m.wrapping_add(r * lu + c);
            let covered = c < 8 || (r < 8 && l > 1);
            let expect = if covered { vec![lost] } else { vec![] };
            assert_eq!(replay(capture, l, d, Some(lost)), expect, "{l}x{d} loss at row {r} column {c}");
        }
    }

    /// Single leg, raw TS in items that are not datagram-sized (SRT's
    /// 1456 bytes, and odd sizes): FEC protects the realigned datagrams.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_leg_raw_ts_fec_repairs_a_lost_datagram_byte_exact() {
        for (l, d, matrices, sizes) in [
            (4u8, 4u8, 3usize, &[1456usize][..]),
            (4, 4, 3, &[1000, 1456, 777, 2048][..]),
            (10, 10, 2, &[1456][..]),
        ] {
            let dgs = matrices * l as usize * d as usize;
            let items = raw_ts_items(&ts_stream(dgs * TS_PACKETS_PER_DATAGRAM), sizes);
            let legs = capture(l, d, false, items, dgs).await;
            check_contiguous(&legs[0], l, d, matrices);
        }
    }

    /// Single leg, RTP in (an upstream SSRC and numbers): re-wrapped with
    /// this output's numbers, which the FEC follows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_leg_rtp_fec_repairs_a_lost_datagram_byte_exact() {
        let ts = ts_stream(48 * TS_PACKETS_PER_DATAGRAM);
        let items = ts
            .chunks(TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE)
            .enumerate()
            .map(|(i, p)| upstream_rtp(30_000 + i as u16, p))
            .collect();
        let legs = capture(4, 4, false, items, 48).await;
        assert!(legs[0].iter().filter(|dg| !is_fec_packet(dg)).all(|dg| dg[8..12] != [0xDE, 0xAD, 0xBE, 0xEF]));
        check_contiguous(&legs[0], 4, 4, 3);
    }

    /// 2022-7, raw TS: both legs carry the same datagrams and FEC.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dual_leg_raw_ts_fec_repairs_a_lost_datagram_byte_exact() {
        let items = raw_ts_items(&ts_stream(48 * TS_PACKETS_PER_DATAGRAM), &[1456]);
        let legs = capture(4, 4, true, items, 48).await;
        assert_eq!(legs[0], legs[1], "both legs byte-identical");
        check_contiguous(&legs[0], 4, 4, 3);
    }

    /// 2022-7, RTP forwarded unchanged with an upstream gap and one short
    /// datagram: groups holding a number never sent, or datagrams of two
    /// sizes, get no FEC; every other loss is rebuilt byte for byte, and
    /// nothing is ever made up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dual_leg_rtp_forward_fec_follows_upstream_numbers() {
        let full = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
        let ts = ts_stream(48 * TS_PACKETS_PER_DATAGRAM);
        let seqs: Vec<u16> = (40_000u16..40_048).filter(|s| !(40_018..=40_020).contains(s)).collect();
        let items: Vec<RtpPacket> = seqs
            .iter()
            .map(|&s| {
                let i = (s - 40_000) as usize;
                let len = if s == 40_034 { 3 * TS_PACKET_SIZE } else { full };
                upstream_rtp(s, &ts[i * full..i * full + len])
            })
            .collect();
        let legs = capture(4, 4, true, items, seqs.len()).await;
        assert_eq!(legs[0], legs[1], "both legs byte-identical");
        let cap = &legs[0];
        let wire: Vec<u16> = cap.iter().filter(|dg| !is_fec_packet(dg)).map(|dg| seq_of(dg)).collect();
        assert_eq!(wire, seqs, "upstream numbers forwarded unchanged");
        assert!(replay(cap, 4, 4, None).is_empty());
        // Column 1 of the gapped matrix, and a loss in the third matrix.
        assert_eq!(replay(cap, 4, 4, Some(40_025)), vec![40_025]);
        assert_eq!(replay(cap, 4, 4, Some(40_047)), vec![40_047]);
        // Column 2 and row 1 both hold a number upstream never sent.
        assert!(replay(cap, 4, 4, Some(40_022)).is_empty());
        // The short datagram: its column and row are of mixed sizes.
        assert!(replay(cap, 4, 4, Some(40_034)).is_empty());
        // Same column as the short one, but its row is uniform.
        assert_eq!(replay(cap, 4, 4, Some(40_046)), vec![40_046]);
    }

    /// L=1: column FEC only, so a receiver publishes each datagram once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_column_output_is_never_duplicated_by_a_receiver() {
        let items = raw_ts_items(&ts_stream(12 * TS_PACKETS_PER_DATAGRAM), &[1316]);
        let legs = capture(1, 4, false, items, 12).await;
        check_contiguous(&legs[0], 1, 4, 3);
        let mut dec = FecDecoder::new(1, 4);
        let mut published = 0;
        for dg in &legs[0] {
            published += if is_fec_packet(dg) {
                dec.process_fec(dg).len()
            } else {
                dec.process_media(seq_of(dg), 0, &Bytes::copy_from_slice(dg)).len()
            };
        }
        assert_eq!(published, 12);
    }
}
