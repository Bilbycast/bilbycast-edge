// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RIST Simple Profile (TR-06-1:2020) input.
//!
//! Binds a dual-port UDP channel (RTP on even port P, RTCP on P+1) via
//! [`rist_transport::RistSocket::receiver`], consumes the decoded RTP
//! payload stream, and publishes each datagram onto the flow's per-input
//! broadcast channel. RIST's NACK-based retransmission and jitter buffer
//! happen inside the `RistSocket`, so by the time bytes reach this module
//! they are already in delivery order.
//!
//! Payload handling mirrors the UDP input: we treat every datagram as raw
//! MPEG-TS (`is_raw_ts: true`) and synthesise sequence numbers / RTP
//! timestamps for the downstream 2022-7 merger and stats counters.
//!
//! Optional SMPTE 2022-7 redundancy: two `RistSocket::receiver` instances
//! bound on separate even ports feed a shared [`HitlessMerger`] using
//! synthetic per-leg sequence counters.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use rist_transport::{RistSocket, RistSocketConfig};

use crate::config::models::RistInputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::time::now_us;

use super::input_post_process::{InputPostProcess, InputPostProcessConfig};
use super::input_transcode::{publish_input_packet_with_post, InputTranscoder};
use super::packet::RtpPacket;

#[allow(dead_code)]
const TS_PACKET_SIZE: usize = 188;

/// Spawn a RIST input task. Produces `RtpPacket { is_raw_ts: true, .. }`.
pub fn spawn_rist_input(
    config: RistInputConfig,
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
        // Apply interface_binding (loose only on RIST in Phase 1):
        // rewrite the bind_addr's IP to the resolved NIC's primary IPv4
        // when bind_addr is currently 0.0.0.0. Strict requires librist
        // SO_BINDTODEVICE plumbing — Phase 2.
        let mut config = config;
        if let Some(b) = config.interface_binding.clone() {
            if b.strict {
                tracing::error!(
                    "RIST input '{input_id}': strict interface_binding not supported in Phase 1"
                );
                event_sender.emit_flow_with_details(
                    EventSeverity::Critical,
                    crate::manager::events::category::RIST,
                    format!("RIST input '{input_id}': strict interface_binding rejected (Phase 1)"),
                    &flow_id,
                    serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
                );
                return;
            }
            match crate::util::socket::resolve_interface_binding(Some(&b)) {
                Ok(Some(r)) => {
                    if let Some(ip) = r.ipv4_for_loose {
                        // Replace the host part of bind_addr if it's
                        // currently 0.0.0.0. Operator-specified IPs win.
                        if let Ok(addr) = config.bind_addr.parse::<std::net::SocketAddr>() {
                            if addr.ip().is_unspecified() {
                                let new_bind = format!("{ip}:{}", addr.port());
                                tracing::info!(
                                    "RIST input '{}': interface_binding {} → bind_addr {}",
                                    input_id, b.name, new_bind,
                                );
                                config.bind_addr = new_bind;
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("RIST input '{input_id}': interface_binding failed: {e}");
                    event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        crate::manager::events::category::RIST,
                        format!("RIST input '{input_id}': interface_binding rejected: {e}"),
                        &flow_id,
                        serde_json::json!({"error_code": "interface_not_found"}),
                    );
                    return;
                }
            }
        }
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("RIST input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("RIST input: transcode setup failed, passthrough: {e}");
                None
            }
        };
        if let (Some(t), Some(p)) = (transcoder.as_mut(), av_sync_pacer.as_ref()) {
            t.set_av_sync_pacer(p.clone());
        }
        // **Per-input** PCR forward-jump signal — same shape as input_srt.
        let pcr_jump_signal: std::sync::Arc<std::sync::atomic::AtomicI64> =
            std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
        if let Some(t) = transcoder.as_mut() {
            t.set_pcr_jump_signal(pcr_jump_signal.clone());
        }
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_mut(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
        );
        let passthrough_clock = config.passthrough_clock.unwrap_or(false);
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: config.pid_overrides.as_ref(),
            pid_map: config.pid_map.as_ref(),
            passthrough_clock,
            av_sync_pacer: av_sync_pacer.as_ref(),
            pcr_jump_signal: Some(&pcr_jump_signal),
        });
        if let Some(ref _p) = post {
            tracing::info!(
                "RIST input '{input_id}': ingress post-process active (program_filter={} pid_overrides={} pid_map={} passthrough_clock={})",
                config.program_number.is_some(),
                config.pid_overrides.is_some(),
                config.pid_map.is_some(),
                passthrough_clock,
            );
        }
        let result = if config.redundancy.is_some() {
            rist_input_redundant_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder, &mut post)
                .await
        } else {
            rist_input_loop(config, broadcast_tx, stats, cancel, &event_sender, &flow_id, &mut transcoder, &mut post).await
        };
        if let Err(e) = result {
            tracing::error!("RIST input task exited with error: {e}");
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::RIST,
                format!("RIST input lost: {e}"),
                &flow_id,
            );
        }
    })
}

fn socket_config_for(config: &RistInputConfig, local_addr: SocketAddr) -> RistSocketConfig {
    let mut sc = RistSocketConfig::default();
    sc.local_addr = local_addr;
    if let Some(ms) = config.buffer_ms {
        sc.buffer_size = Duration::from_millis(ms as u64);
    }
    if let Some(r) = config.max_nack_retries {
        sc.max_nack_retries = r;
    }
    if let Some(ms) = config.rtcp_interval_ms {
        sc.rtcp_interval = Duration::from_millis(ms as u64);
    }
    sc.cname = config.cname.clone();
    sc
}

async fn rist_input_loop(
    config: RistInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let bind: SocketAddr = config.bind_addr.parse()?;
    let sc = socket_config_for(&config, bind);

    let mut socket = RistSocket::receiver(sc).await.map_err(|e| {
        use crate::manager::events::{BindProto, BindScope};
        let scope = BindScope::flow(flow_id);
        let component = "RIST input";
        let addr = config.bind_addr.clone();
        let err_str = format!("{e}");
        if err_str.to_lowercase().contains("address") && err_str.to_lowercase().contains("use") {
            events.emit_port_conflict(component, &addr, BindProto::Udp, scope, &err_str);
        } else {
            events.emit_bind_failed(component, &addr, BindProto::Udp, scope, &err_str);
        }
        anyhow::anyhow!("RIST receiver bind failed: {e}")
    })?;

    stats.set_input_rist_stats(socket.stats());

    tracing::info!("RIST input started on {}", bind);
    events.emit_flow(
        EventSeverity::Info,
        category::RIST,
        format!("RIST input listening on {}", bind),
        flow_id,
    );

    let mut seq_counter: u16 = 0;
    let mut ts_counter: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RIST input on {} stopping (cancelled)", bind);
                socket.close();
                return Ok(());
            }
            recv = socket.recv() => {
                match recv {
                    Some(data) => {
                        publish(
                            &data,
                            &broadcast_tx,
                            &stats,
                            &mut seq_counter,
                            &mut ts_counter,
                            transcoder,
                            post,
                        );
                    }
                    None => {
                        // Socket closed — exit cleanly.
                        tracing::info!("RIST input recv channel closed on {}", bind);
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn publish(
    data: &Bytes,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
    seq_counter: &mut u16,
    ts_counter: &mut u32,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    let seq = *seq_counter;
    *seq_counter = seq_counter.wrapping_add(1);
    // RFC 2250 §3.5 — 90 kHz media-clock RTP timestamp from PCR;
    // falls back to last-seen value when this datagram doesn't carry a
    // PCR-bearing TS packet.
    if let Some(pcr_27mhz) =
        crate::engine::input_srt::scan_first_pcr_in_datagram(&data)
    {
        *ts_counter = (pcr_27mhz / 300) as u32;
    }

    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let packet = RtpPacket {
        data: data.clone(),
        sequence_number: seq,
        rtp_timestamp: *ts_counter,
        recv_time_us: now_us(),
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
    };

    publish_input_packet_with_post(transcoder, post, broadcast_tx, packet);
}

async fn rist_input_redundant_loop(
    config: RistInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    let leg1_addr: SocketAddr = config.bind_addr.parse()?;
    let leg2_addr: SocketAddr = redundancy.bind_addr.parse()?;

    let mut socket_leg1 = RistSocket::receiver(socket_config_for(&config, leg1_addr))
        .await
        .map_err(|e| {
            use crate::manager::events::{BindProto, BindScope};
            let scope = BindScope::flow(flow_id);
            let err_str = format!("{e}");
            if err_str.to_lowercase().contains("address") && err_str.to_lowercase().contains("use") {
                events.emit_port_conflict("RIST input leg 1", &config.bind_addr, BindProto::Udp, scope, &err_str);
            } else {
                events.emit_bind_failed("RIST input leg 1", &config.bind_addr, BindProto::Udp, scope, &err_str);
            }
            anyhow::anyhow!("RIST input leg 1 bind failed: {e}")
        })?;
    let mut socket_leg2 = RistSocket::receiver(socket_config_for(&config, leg2_addr))
        .await
        .map_err(|e| {
            use crate::manager::events::{BindProto, BindScope};
            let scope = BindScope::flow(flow_id);
            let err_str = format!("{e}");
            if err_str.to_lowercase().contains("address") && err_str.to_lowercase().contains("use") {
                events.emit_port_conflict("RIST input leg 2", &redundancy.bind_addr, BindProto::Udp, scope, &err_str);
            } else {
                events.emit_bind_failed("RIST input leg 2", &redundancy.bind_addr, BindProto::Udp, scope, &err_str);
            }
            anyhow::anyhow!("RIST input leg 2 bind failed: {e}")
        })?;

    stats.set_input_rist_stats(socket_leg1.stats());
    stats.set_input_rist_leg2_stats(socket_leg2.stats());

    tracing::info!(
        "RIST input started with 2022-7 redundancy: leg1={} leg2={}",
        leg1_addr,
        leg2_addr
    );
    events.emit_flow(
        EventSeverity::Info,
        category::RIST,
        format!(
            "RIST input listening (2022-7): leg1={} leg2={}",
            leg1_addr, leg2_addr
        ),
        flow_id,
    );

    let mut merger = HitlessMerger::new();
    let mut seq_counter_leg1: u16 = 0;
    let mut seq_counter_leg2: u16 = 0;
    let mut ts_counter: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("RIST redundant input stopping (cancelled)");
                socket_leg1.close();
                socket_leg2.close();
                return Ok(());
            }
            recv = socket_leg1.recv() => match recv {
                Some(data) => handle_redundant_leg(
                    &data, ActiveLeg::Leg1, &mut merger,
                    &mut seq_counter_leg1, &mut ts_counter,
                    &broadcast_tx, &stats, transcoder, post,
                ),
                None => {
                    tracing::warn!("RIST input leg 1 closed");
                    return Ok(());
                }
            },
            recv = socket_leg2.recv() => match recv {
                Some(data) => handle_redundant_leg(
                    &data, ActiveLeg::Leg2, &mut merger,
                    &mut seq_counter_leg2, &mut ts_counter,
                    &broadcast_tx, &stats, transcoder, post,
                ),
                None => {
                    tracing::warn!("RIST input leg 2 closed");
                    return Ok(());
                }
            },
        }
    }
}

fn handle_redundant_leg(
    data: &Bytes,
    leg: ActiveLeg,
    merger: &mut HitlessMerger,
    leg_seq: &mut u16,
    ts_counter: &mut u32,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: &Arc<FlowStatsAccumulator>,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    let seq = *leg_seq;
    *leg_seq = leg_seq.wrapping_add(1);

    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

    if merger.try_merge(seq, leg).is_none() {
        return;
    }

    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // RFC 2250 §3.5 — 90 kHz media-clock RTP timestamp from PCR;
    // falls back to last-seen value on datagrams without PCR.
    if let Some(pcr_27mhz) =
        crate::engine::input_srt::scan_first_pcr_in_datagram(&data)
    {
        *ts_counter = (pcr_27mhz / 300) as u32;
    }

    let packet = RtpPacket {
        data: data.clone(),
        sequence_number: seq,
        rtp_timestamp: *ts_counter,
        recv_time_us: now_us(),
        is_raw_ts: true,
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
    };

    publish_input_packet_with_post(transcoder, post, broadcast_tx, packet);
}
