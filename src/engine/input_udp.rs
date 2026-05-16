// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::UdpInputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::FlowStatsAccumulator;
use crate::util::socket::bind_udp_input;
use crate::util::time::now_us;

use super::input_post_process::{InputPostProcess, InputPostProcessConfig};
use super::input_transcode::InputTranscoder;
use super::packet::{MAX_RTP_PACKET_SIZE, RtpPacket};

/// TS packet size used for synthetic timestamp calculation.
const TS_PACKET_SIZE: usize = 188;

/// Spawn a task that receives raw UDP datagrams and publishes them
/// to a broadcast channel for fan-out to outputs.
///
/// Unlike the RTP input, this accepts ALL datagrams without requiring
/// an RTP v2 header. Packets are forwarded with `is_raw_ts: true` and
/// synthetic sequence numbers. Suitable for receiving raw MPEG-TS over
/// UDP from sources like OBS, ffmpeg, or srt-live-transmit.
pub fn spawn_udp_input(
    config: UdpInputConfig,
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
                    tracing::info!("UDP input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("UDP input: transcode setup failed, passthrough: {e}");
                None
            }
        };
        if let (Some(t), Some(p)) = (transcoder.as_mut(), av_sync_pacer.as_ref()) {
            t.set_av_sync_pacer(p.clone());
        }
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_mut(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
        );
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: config.pid_overrides.as_ref(),
            pid_map: config.pid_map.as_ref(),
        });
        if let Some(_) = post.as_ref() {
            tracing::info!(
                "UDP input '{input_id}': ingress post-process active (program_filter={} pid_overrides={} pid_map={})",
                config.program_number.is_some(),
                config.pid_overrides.is_some(),
                config.pid_map.is_some(),
            );
        }
        // Per-input ingress smoothing publisher. When
        // `ingress_smoothing_ms` is unset / 0, the publisher is a
        // zero-overhead pass-through to broadcast_tx. Otherwise it
        // spawns a drainer task that releases packets at
        // recv_time_us + smoothing_ms.
        let publisher = crate::engine::ingress_smoothing::IngressPublisher::new(
            config.ingress_smoothing_ms,
            broadcast_tx,
            &input_id,
            cancel.clone(),
        );
        if let Err(e) = udp_input_loop(config, publisher, stats, cancel, &event_sender, &flow_id, &mut transcoder, &mut post).await {
            tracing::error!("UDP input task exited with error: {e}");
            event_sender.emit_flow(EventSeverity::Critical, category::FLOW, format!("Flow input lost: {e}"), &flow_id);
        }
    })
}

async fn udp_input_loop(
    config: UdpInputConfig,
    publisher: crate::engine::ingress_smoothing::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
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
                category::UDP,
                format!("UDP input listening on {}", config.bind_addr),
                flow_id,
                serde_json::json!({"bind_addr": config.bind_addr}),
            );
            s
        }
        Err(e) => {
            use crate::manager::events::{BindProto, BindScope};
            let scope = BindScope::flow(flow_id);
            if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                events.emit_port_conflict("UDP input", &config.bind_addr, BindProto::Udp, scope, &e);
            } else {
                events.emit_bind_failed("UDP input", &config.bind_addr, BindProto::Udp, scope, &e);
            }
            return Err(e);
        }
    };

    tracing::info!("UDP input started on {}", config.bind_addr);

    let mut buf = vec![0u8; MAX_RTP_PACKET_SIZE + 100];
    let mut seq_counter: u16 = 0;
    let mut ts_counter: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("UDP input on {} stopping (cancelled)", config.bind_addr);
                break;
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, _src)) => {
                        let data = &buf[..len];

                        // Generate synthetic sequence number
                        let seq = seq_counter;
                        seq_counter = seq_counter.wrapping_add(1);

                        // Approximate timestamp: assume 90kHz clock
                        let ts_pkts = (len / TS_PACKET_SIZE) as u32;
                        ts_counter = ts_counter.wrapping_add(ts_pkts.max(1) * 188 * 8);

                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(len as u64, Ordering::Relaxed);

                        // Bandwidth limit enforcement: drop packet if flow is blocked
                        if stats.bandwidth_blocked.load(Ordering::Relaxed) {
                            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }

                        let packet = RtpPacket {
                            data: Bytes::copy_from_slice(data),
                            sequence_number: seq,
                            rtp_timestamp: ts_counter,
                            recv_time_us: now_us(),
                            is_raw_ts: true,
                            upstream_seq: None,
                            upstream_leg_id: None,
                        };

                        crate::engine::input_transcode::publish_input_packet_smoothed(
                            transcoder, post, &publisher, packet,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("UDP input recv error: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}
