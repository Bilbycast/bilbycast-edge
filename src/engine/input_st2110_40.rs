// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SMPTE ST 2110-40 (RFC 8331 ancillary data) input task.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::St2110AncillaryInputConfig;
use crate::manager::events::EventSender;
use crate::stats::collector::FlowStatsAccumulator;

use super::packet::RtpPacket;
use super::st2110_io::run_st2110_anc_input;

pub fn spawn_st2110_40_input(
    config: St2110AncillaryInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_st2110_anc_input(
            config,
            broadcast_tx,
            stats,
            cancel,
            Some(event_sender),
            Some(flow_id),
        )
        .await
        {
            tracing::error!("ST 2110-40 input task exited with error: {e}");
        }
    })
}
