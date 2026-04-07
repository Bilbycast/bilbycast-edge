// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE ST 2110-30 (linear PCM audio) output task.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110AudioOutputConfig;
use crate::stats::collector::OutputStatsAccumulator;

use super::audio_transcode::InputFormat;
use super::packet::RtpPacket;
use super::st2110_io::run_st2110_audio_output;

pub fn spawn_st2110_30_output(
    config: St2110AudioOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    input_format: Option<InputFormat>,
    flow_id: &str,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    let id = config.id.clone();
    let flow_id = flow_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = run_st2110_audio_output(
            config,
            false,
            &mut rx,
            output_stats,
            cancel,
            input_format,
            &flow_id,
        )
        .await
        {
            tracing::error!("ST 2110-30 output '{id}' exited with error: {e}");
        }
    })
}
