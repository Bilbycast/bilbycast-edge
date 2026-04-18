// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE ST 2110-23 (one video essence across N sub-streams) output task.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110_23OutputConfig;
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_video_io::run_st2110_23_output;

pub fn spawn_st2110_23_output(
    config: St2110_23OutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    let id = config.id.clone();
    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("st2110-23".to_string()),
        video_passthrough: false,
        audio_passthrough: false,
        audio_only: false,
    });
    tokio::spawn(async move {
        if let Err(e) = run_st2110_23_output(config, rx, output_stats, cancel).await {
            tracing::error!("ST 2110-23 output '{id}' exited with error: {e}");
        }
    })
}
