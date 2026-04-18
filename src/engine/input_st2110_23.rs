// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE ST 2110-23 (single video essence over multiple -20 sub-streams) input task.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110_23InputConfig;
use crate::stats::collector::FlowStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_video_io::run_st2110_23_input;

pub fn spawn_st2110_23_input(
    config: St2110_23InputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_st2110_23_input(config, input_id, broadcast_tx, stats, cancel).await {
            tracing::error!("ST 2110-23 input task exited with error: {e}");
        }
    })
}
