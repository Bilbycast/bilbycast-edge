// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE ST 2110-20 (uncompressed video) output task.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110VideoOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_video_io::run_st2110_20_output;

pub fn spawn_st2110_20_output(
    config: St2110VideoOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    let id = config.id.clone();
    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("st2110-20".to_string()),
        video_passthrough: false,
        audio_passthrough: false,
        audio_only: false,
    });
    tokio::spawn(async move {
        if let Err(e) = run_st2110_20_output(config, rx, output_stats, cancel).await {
            tracing::error!("ST 2110-20 output '{id}' exited with error: {e}");
        }
    })
}
