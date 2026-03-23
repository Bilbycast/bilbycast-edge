// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! RTMP output task — publishes demuxed H.264/AAC to an RTMP server.
//!
//! Subscribes to the flow's broadcast channel, demuxes MPEG-TS into
//! H.264 + AAC elementary streams, and publishes them via the RTMP
//! protocol implementation in [`super::rtmp`].

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::RtmpOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;

/// Spawn an async task that consumes RTP packets from the broadcast channel,
/// demuxes H.264/AAC from the MPEG-TS payload, and publishes to an RTMP server.
///
/// This is a placeholder implementation that logs a warning.
/// Full integration with the TS demuxer and RTMP session will be added later.
pub fn spawn_rtmp_output(
    config: RtmpOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    tokio::spawn(async move {
        tracing::info!(
            "RTMP output '{}' started -> {}",
            config.id,
            config.dest_url,
        );

        // TODO: integrate with super::rtmp::RtmpSession and ts_demux
        // to extract H.264/AAC from the TS stream and publish via RTMP.
        //
        // High-level flow:
        // 1. RtmpSession::connect(&config.dest_url, &config.stream_key, is_tls)
        // 2. For each RtpPacket received on `rx`:
        //    a. Feed TS payload into ts_demux
        //    b. On complete H.264 access unit: session.send_video_tag(...)
        //    c. On complete AAC frame: session.send_audio_tag(...)
        // 3. On cancel: session.close()

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("RTMP output '{}' cancelled", config.id);
                    break;
                }
                result = rx.recv() => {
                    match result {
                        Ok(_packet) => {
                            output_stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                            output_stats.bytes_sent.fetch_add(188, Ordering::Relaxed);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                "RTMP output '{}' lagged by {n} packets",
                                config.id,
                            );
                            output_stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!("RTMP output '{}' channel closed", config.id);
                            break;
                        }
                    }
                }
            }
        }
    })
}
