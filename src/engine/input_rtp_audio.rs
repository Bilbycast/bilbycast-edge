// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Generic RFC 3551 PCM-over-RTP audio input.
//!
//! Wire-identical to ST 2110-30 but with relaxed constraints (no PTP, no
//! RFC 7273 timing reference, no NMOS clock_domain advertising). The runtime
//! is delegated to the existing `st2110_io::run_st2110_audio_input` helper:
//! we synthesize an [`St2110AudioInputConfig`] with `clock_domain = None` so
//! the PTP reporter is not spawned, and pass it through. This keeps a single
//! battle-tested input loop for both essence types.
//!
//! Use cases: radio contribution feeds over the public internet, talkback
//! between studios without a shared PTP fabric, ffmpeg / OBS / GStreamer
//! interoperability, hardware encoders that don't speak ST 2110.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{RtpAudioInputConfig, St2110AudioInputConfig};
use crate::manager::events::EventSender;
use crate::stats::collector::FlowStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_io::run_st2110_audio_input;

/// Spawn a generic RTP audio input task.
///
/// Internally delegates to the ST 2110-30 input runtime with PTP /
/// `clock_domain` disabled. The wire format is identical (RFC 3551 RTP +
/// big-endian L16/L24 PCM payload), so the existing `PcmDepacketizer`
/// validation applies unchanged.
pub fn spawn_rtp_audio_input(
    config: RtpAudioInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: Option<EventSender>,
    flow_id: Option<String>,
) -> JoinHandle<()> {
    let synthesized = St2110AudioInputConfig {
        bind_addr: config.bind_addr.clone(),
        interface_addr: config.interface_addr.clone(),
        source_addr: config.source_addr.clone(),
        redundancy: config.redundancy.clone(),
        sample_rate: config.sample_rate,
        bit_depth: config.bit_depth,
        channels: config.channels,
        packet_time_us: config.packet_time_us,
        payload_type: config.payload_type,
        // Force PTP off — `rtp_audio` is explicitly the no-PTP variant.
        clock_domain: None,
        allowed_sources: config.allowed_sources.clone(),
        max_bitrate_mbps: None,
        transcode: config.transcode.clone(),
        audio_encode: config.audio_encode.clone(),
    };

    tokio::spawn(async move {
        if let Err(e) = run_st2110_audio_input(
            synthesized,
            false,
            broadcast_tx,
            flow_stats,
            cancel,
            event_sender,
            flow_id,
        )
        .await
        {
            tracing::error!("rtp_audio input exited with error: {e}");
        }
    })
}
