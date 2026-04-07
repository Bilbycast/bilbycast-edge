// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! SMPTE ST 2110-31 (AES3 transparent audio) input task.
//!
//! Thin wrapper around [`crate::engine::st2110_io::run_st2110_audio_input`]
//! with `is_aes3 = true`. The wire framing is identical to ST 2110-30; only
//! downstream consumers interpret the bytes as AES3 sub-frames rather than
//! plain linear PCM.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110AudioInputConfig;
use crate::manager::events::EventSender;
use crate::stats::collector::FlowStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_io::run_st2110_audio_input;

pub fn spawn_st2110_31_input(
    config: St2110AudioInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_st2110_audio_input(
            config,
            true,
            broadcast_tx,
            stats,
            cancel,
            Some(event_sender),
            Some(flow_id),
        )
        .await
        {
            tracing::error!("ST 2110-31 input task exited with error: {e}");
        }
    })
}
