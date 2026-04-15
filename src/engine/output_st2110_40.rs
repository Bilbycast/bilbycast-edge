// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE ST 2110-40 (RFC 8331 ancillary data) output task.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110AncillaryOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_io::run_st2110_anc_output;

pub fn spawn_st2110_40_output(
    config: St2110AncillaryOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    let id = config.id.clone();

    output_stats.set_egress_static(crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some("st2110-40".to_string()),
        video_passthrough: false,
        audio_passthrough: false,
        audio_only: false,
    });

    tokio::spawn(async move {
        if let Err(e) = run_st2110_anc_output(config, &mut rx, output_stats, cancel).await {
            tracing::error!("ST 2110-40 output '{id}' exited with error: {e}");
        }
    })
}
