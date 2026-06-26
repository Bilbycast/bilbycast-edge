// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RIST Simple Profile (TR-06-1:2020) output.
//!
//! Subscribes to the flow's broadcast channel, runs the optional program
//! filter / audio encode / video encode / delay-buffer stages (identical
//! to the UDP and RTP outputs), and transmits aligned 7×188-byte MPEG-TS
//! datagrams via [`rist_transport::RistSocket::sender`]. The underlying
//! RistSocket wraps each payload in RTP, performs NACK-driven
//! retransmission, and exchanges RTCP with the peer.
//!
//! Optional SMPTE 2022-7 redundancy: a second `RistSocket::sender` is
//! instantiated with a separate local bind and a separate remote address;
//! every outbound datagram is duplicated to both legs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use rist_transport::{RistSocket, RistSocketConfig};

use crate::config::models::{RistOutputConfig, RistOutputRedundancyConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::time::now_us;

use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::ts_pid_overrides_rewriter::TsPidOverridesRewriter;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

const TS_SYNC_BYTE: u8 = 0x47;
const TS_PACKET_SIZE: usize = 188;
const TS_PACKETS_PER_DATAGRAM: usize = 7;
const TS_SYNC_CONFIRM_COUNT: usize = 3;
const RTP_HEADER_MIN_SIZE: usize = 12;

/// Spawn a RIST output task.
pub fn spawn_rist_output(
    config: RistOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    event_sender: EventSender,
    flow_id: String,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> JoinHandle<()> {
    // Apply interface_binding (loose only on RIST in Phase 1) for the
    // primary leg + 2022-7 redundancy leg. Strict deferred (librist
    // SRTO_BINDTODEVICE plumbing).
    let mut config = config;
    match crate::util::socket::srt_local_addr_from_binding(
        config.interface_binding.as_ref(),
        config.local_addr.as_deref(),
    ) {
        Ok(Some(addr)) => { config.local_addr = Some(addr); }
        Ok(None) => {}
        Err(e) => {
            tracing::error!("RIST output '{}': interface_binding failed: {e}", config.id);
            event_sender.emit_output_with_details(
                EventSeverity::Critical,
                crate::manager::events::category::RIST,
                format!("RIST output '{}': interface_binding rejected: {e}", config.id),
                &config.id,
                serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
            );
            return tokio::spawn(async {});
        }
    }
    if let Some(red) = config.redundancy.as_mut() {
        match crate::util::socket::srt_local_addr_from_binding(
            red.interface_binding.as_ref(),
            red.local_addr.as_deref(),
        ) {
            Ok(Some(addr)) => { red.local_addr = Some(addr); }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("RIST output '{}' leg2: interface_binding failed: {e}", config.id);
                event_sender.emit_output_with_details(
                    EventSeverity::Critical,
                    crate::manager::events::category::RIST,
                    format!("RIST output '{}' leg2: interface_binding rejected: {e}", config.id),
                    &config.id,
                    serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
                );
                return tokio::spawn(async {});
            }
        }
    }

    let mut rx = broadcast_tx.subscribe();

    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("ts".to_string()),
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
        if let Err(e) = rist_output_loop(
            &config,
            &mut rx,
            output_stats,
            cancel,
            frame_rate_rx,
            &event_sender,
            &flow_id,
            av_sync_pacer,
            active_input_rx,
        )
        .await
        {
            tracing::error!("RIST output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::RIST,
                format!("RIST output '{}' exited with error: {e}", config.id),
                &flow_id,
            );
        }
    })
}

fn socket_config_for_sender(
    cname: Option<&str>,
    buffer_ms: Option<u32>,
    rtcp_interval_ms: Option<u32>,
    retransmit_buffer_capacity: Option<usize>,
    local_addr: SocketAddr,
) -> RistSocketConfig {
    let mut sc = RistSocketConfig::default();
    sc.local_addr = local_addr;
    if let Some(ms) = buffer_ms {
        sc.buffer_size = Duration::from_millis(ms as u64);
    }
    if let Some(ms) = rtcp_interval_ms {
        sc.rtcp_interval = Duration::from_millis(ms as u64);
    }
    if let Some(cap) = retransmit_buffer_capacity {
        sc.retransmit_buffer_capacity = cap;
    }
    sc.cname = cname.map(|s| s.to_string());
    sc
}

async fn build_sender(
    remote: SocketAddr,
    local_addr: Option<&str>,
    cname: Option<&str>,
    buffer_ms: Option<u32>,
    rtcp_interval_ms: Option<u32>,
    retransmit_buffer_capacity: Option<usize>,
) -> anyhow::Result<RistSocket> {
    // RistChannel::bind computes RTCP port = rtp_port + 1 using the REQUESTED
    // port, not the OS-assigned port, so passing port 0 makes RTCP land on
    // port 1 (a privileged port on Linux). Pick an explicit even port in the
    // dynamic range and retry on collision.
    if let Some(addr) = local_addr {
        let sc = socket_config_for_sender(
            cname,
            buffer_ms,
            rtcp_interval_ms,
            retransmit_buffer_capacity,
            addr.parse()?,
        );
        return RistSocket::sender(sc, remote)
            .await
            .map_err(|e| anyhow::anyhow!("RIST sender bind/connect failed: {e}"));
    }

    let bind_ip = if remote.is_ipv4() { "0.0.0.0" } else { "[::]" };
    for _ in 0..32 {
        // Even port in [49152, 65534] (IANA dynamic/private range).
        let port: u16 = (49152 + (rand::random::<u16>() % 8192) * 2).min(65534);
        let local_str = format!("{}:{}", bind_ip, port & !1);
        let local: SocketAddr = local_str.parse().unwrap();
        let sc = socket_config_for_sender(
            cname,
            buffer_ms,
            rtcp_interval_ms,
            retransmit_buffer_capacity,
            local,
        );
        match RistSocket::sender(sc, remote).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                tracing::debug!(
                    "RIST sender bind on {local_str} failed ({e}), retrying with a different port"
                );
                continue;
            }
        }
    }
    Err(anyhow::anyhow!(
        "RIST sender: exhausted 32 bind attempts for ephemeral even port; set local_addr explicitly"
    ))
}

async fn rist_output_loop(
    config: &RistOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    events: &EventSender,
    flow_id: &str,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    let remote: SocketAddr = config.remote_addr.parse()?;

    let socket_leg1 = build_sender(
        remote,
        config.local_addr.as_deref(),
        config.cname.as_deref(),
        config.buffer_ms,
        config.rtcp_interval_ms,
        config.retransmit_buffer_capacity,
    )
    .await?;
    stats.set_rist_stats(socket_leg1.stats());

    let socket_leg2: Option<RistSocket> = if let Some(ref red) = config.redundancy {
        let s = build_sender_redundancy(red, config).await?;
        stats.set_rist_leg2_stats(s.stats());
        Some(s)
    } else {
        None
    };

    tracing::info!(
        "RIST output '{}' started -> {}{}",
        config.id,
        remote,
        socket_leg2
            .as_ref()
            .map(|_| format!(" (+2022-7 leg2 -> {})", config.redundancy.as_ref().unwrap().remote_addr))
            .unwrap_or_default()
    );
    events.emit_flow(
        EventSeverity::Info,
        category::RIST,
        format!("RIST output '{}' connected -> {}", config.id, remote),
        flow_id,
    );

    let mut program_filter = config.program_number.map(TsProgramFilter::new);
    let mut filter_scratch: Vec<u8> = Vec::new();

    let mut pid_remapper = config.pid_map.as_ref().and_then(|m| {
        let r = TsPidRemapper::new(m);
        if r.is_active() {
            tracing::info!(
                "RIST output '{}': pid_map active ({} entries)",
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
                "RIST output '{}': pid_overrides rewriter active ({} programs)",
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
        av_sync_pacer.as_ref(),
        None, // RIST backpressure not yet wired
        Some(events),
    ) {
        Ok(chain) => {
            if let Some(ref c) = chain {
                tracing::info!(
                    "RIST output '{}': transcode chain active ({})",
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
                "RIST output '{}': transcode chain rejected: {e}; transcoding disabled",
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

    // Per-output input-switch watcher.
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

    let mut ts_buf = BytesMut::new();
    let ts_datagram_size = TS_PACKETS_PER_DATAGRAM * TS_PACKET_SIZE;
    let mut ts_sync_found = false;

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
                delay_sleep
                    .as_mut()
                    .reset(Instant::now() + Duration::from_micros(wait));
            }
        }

        let mut packets_to_send: Vec<RtpPacket> = Vec::new();

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RIST output '{}' stopping (cancelled)", config.id);
                socket_leg1.close();
                if let Some(s) = socket_leg2 { s.close(); }
                return Ok(());
            }
            result = rx.recv() => match result {
                Ok(packet) => {
                    if let Some(ref mut db) = delay_buf {
                        db.push(packet);
                    } else {
                        packets_to_send.push(packet);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    tracing::warn!("RIST output '{}' lagged, dropped {n} packets", config.id);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("RIST output '{}' broadcast channel closed", config.id);
                    break;
                }
            },
            _ = &mut delay_sleep, if delay_buf.as_ref().map_or(false, |db| db.len() > 0) => {
                let db = delay_buf.as_mut().unwrap();
                let now = now_us();
                for packet in db.drain_ready(now) {
                    packets_to_send.push(packet);
                }
            }
        }

        for packet in &packets_to_send {
            let ts_data = if packet.is_raw_ts {
                &packet.data[..]
            } else if packet.data.len() > RTP_HEADER_MIN_SIZE {
                &packet.data[RTP_HEADER_MIN_SIZE..]
            } else {
                continue;
            };

            let filtered_bytes: &[u8] = if let Some(ref mut filter) = program_filter {
                filter_scratch.clear();
                filter.filter_into(ts_data, &mut filter_scratch);
                if filter_scratch.is_empty() {
                    continue;
                }
                &filter_scratch
            } else {
                ts_data
            };

            // If transcode chain is active, submit + drain transcoded
            // bytes for downstream pipeline. Otherwise filtered_bytes
            // flows directly to the downstream pipeline.
            let transcoded_holder: Option<Bytes>;
            let downstream_input: &[u8] = if let Some(ref chain) = transcode_chain {
                if chain.try_submit(Bytes::copy_from_slice(filtered_bytes)).is_err() {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                }
                // Drain any transcoded chunks emitted by the codec
                // thread since last poll. If at least one chunk is
                // ready we splice it through; otherwise this iteration
                // contributes nothing (the encoder's still pipelining).
                match transcode_chain.as_mut().and_then(|c| c.try_recv()) {
                    Some(t) => {
                        transcoded_holder = Some(t);
                        transcoded_holder.as_ref().unwrap()
                    }
                    None => continue,
                }
            } else {
                transcoded_holder = None;
                filtered_bytes
            };

            // Per-program role-keyed PID rewrite for passthrough flows.
            let after_overrides: &[u8] = if let Some(ref mut rw) = pid_overrides_rewriter {
                overrides_scratch.clear();
                rw.process(downstream_input, &mut overrides_scratch);
                &overrides_scratch
            } else {
                downstream_input
            };
            let _ = transcoded_holder; // keep transcoded bytes alive through use

            let after_remap: &[u8] = if let Some(ref mut remapper) = pid_remapper {
                remap_scratch.clear();
                remapper.process(after_overrides, &mut remap_scratch);
                &remap_scratch
            } else {
                after_overrides
            };
            ts_buf.extend_from_slice(after_remap);

            if !ts_sync_found {
                let min_bytes = TS_SYNC_CONFIRM_COUNT * TS_PACKET_SIZE;
                if ts_buf.len() >= min_bytes {
                    let mut found_offset = None;
                    for offset in 0..TS_PACKET_SIZE {
                        let all_sync = (0..TS_SYNC_CONFIRM_COUNT).all(|i| {
                            let pos = offset + i * TS_PACKET_SIZE;
                            pos < ts_buf.len() && ts_buf[pos] == TS_SYNC_BYTE
                        });
                        if all_sync {
                            found_offset = Some(offset);
                            break;
                        }
                    }
                    if let Some(offset) = found_offset {
                        if offset > 0 {
                            let _ = ts_buf.split_to(offset);
                        }
                        ts_sync_found = true;
                    } else {
                        ts_buf.clear();
                    }
                }
            }

            if ts_sync_found {
                while ts_buf.len() >= ts_datagram_size {
                    let datagram: Bytes = ts_buf.split_to(ts_datagram_size).freeze();
                    if let Err(e) = socket_leg1.send(datagram.clone()).await {
                        tracing::warn!("RIST output '{}' leg 1 send error: {e}", config.id);
                    } else {
                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes_sent
                            .fetch_add(datagram.len() as u64, Ordering::Relaxed);
                        stats.record_latency(packet.recv_time_us);
                        // A/V interleave: feed every TS packet.
                        let mut off = 0;
                        while off + 188 <= datagram.len() {
                            stats.observe_av_interleave_packet(&datagram[off..off + 188]);
                            off += 188;
                        }
                    }
                    if let Some(ref s2) = socket_leg2 {
                        if let Err(e) = s2.send(datagram).await {
                            tracing::warn!(
                                "RIST output '{}' leg 2 send error: {e}",
                                config.id
                            );
                        }
                    }
                }
            }
        }
    }

    socket_leg1.close();
    if let Some(s) = socket_leg2 {
        s.close();
    }
    Ok(())
}

async fn build_sender_redundancy(
    red: &RistOutputRedundancyConfig,
    parent: &RistOutputConfig,
) -> anyhow::Result<RistSocket> {
    let remote: SocketAddr = red.remote_addr.parse()?;
    build_sender(
        remote,
        red.local_addr.as_deref(),
        parent.cname.as_deref(),
        parent.buffer_ms,
        parent.rtcp_interval_ms,
        parent.retransmit_buffer_capacity,
    )
    .await
}
