// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE ST 2110-20 (uncompressed video) input task.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110VideoInputConfig;
use crate::stats::collector::FlowStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_video_io::run_st2110_20_input;

pub fn spawn_st2110_20_input(
    config: St2110VideoInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_st2110_20_input(config, input_id, broadcast_tx, stats, cancel).await {
            tracing::error!("ST 2110-20 input task exited with error: {e}");
        }
    })
}
