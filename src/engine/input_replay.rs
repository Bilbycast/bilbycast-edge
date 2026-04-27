// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Replay input task: pumps a previously-recorded flow's TS segments
//! back onto the per-input broadcast channel, paced by PCR.
//!
//! Phase 1 behaviour:
//!
//! * On spawn, opens a [`crate::replay::reader::Reader`] for the
//!   configured `recording_id` (and optional `clip_id`).
//! * If `start_paused`, idles until a `Play` command arrives.
//! * Otherwise begins paced playback immediately.
//! * Responds to mark/cue/play/stop/scrub commands routed via the
//!   per-input [`mpsc::Receiver<ReplayCommand>`] held by the
//!   FlowRuntime.
//!
//! When playback hits EOF and `loop_playback = false`, the input goes
//! idle (NULL-PID padding via the per-input forwarder is handled
//! upstream — this task simply stops publishing). Subsequent Play
//! commands restart from the beginning.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::ReplayInputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::replay::reader::Reader;
use crate::replay::{ReplayCommand, ScrubAck};
use crate::stats::collector::FlowStatsAccumulator;

/// Public entry point. Mirrors the signature shape of
/// `input_test_pattern` and `input_media_player` so the flow runtime
/// calls it through the same per-input forwarder pipeline.
pub fn spawn_replay_input(
    config: ReplayInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
    cmd_rx: mpsc::Receiver<ReplayCommand>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(
            config, per_input_tx, flow_stats, cancel, events, flow_id, input_id, cmd_rx,
        ).await {
            tracing::error!(target: "replay", "replay input exited: {e:#}");
        }
    })
}

async fn run(
    config: ReplayInputConfig,
    per_input_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    events: EventSender,
    flow_id: String,
    input_id: String,
    mut cmd_rx: mpsc::Receiver<ReplayCommand>,
) -> Result<()> {
    let mut reader = open_reader(&config).await?;
    let mut playing = !config.start_paused && config.clip_id.is_some()
        || (!config.start_paused && config.clip_id.is_none());
    if playing {
        emit_play_started(&events, &flow_id, &input_id, &reader);
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => {
                    handle_command(
                        cmd, &mut reader, &config, &mut playing,
                        &events, &flow_id, &input_id,
                    ).await;
                }
                None => return Ok(()),
            },
            // Pump one bundle when in playing state. We don't drive the
            // entire `paced_replayer::pump` here because we need the
            // command channel to interrupt mid-playback (cue / scrub /
            // stop) — embed the loop body inline so each iteration is
            // tokio::select-able.
            res = pump_one_bundle(&mut reader, &per_input_tx, &flow_stats), if playing => {
                match res {
                    Ok(true) => {} // bundle published, continue
                    Ok(false) => {
                        // EOF or out-of-range
                        if config.loop_playback {
                            let _ = reader.scrub_to(reader.range.from_pts_90khz).await;
                        } else {
                            playing = false;
                            emit_eof(&events, &flow_id, &input_id);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "replay", "playback read error: {e:#}");
                        playing = false;
                    }
                }
            }
        }
    }
}

async fn open_reader(config: &ReplayInputConfig) -> Result<Reader> {
    if let Some(ref clip_id) = config.clip_id {
        Reader::open_clip(&config.recording_id, clip_id).await
    } else {
        Reader::open_recording(&config.recording_id).await
    }
}

async fn handle_command(
    cmd: ReplayCommand,
    reader: &mut Reader,
    config: &ReplayInputConfig,
    playing: &mut bool,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
) {
    match cmd {
        ReplayCommand::Cue { clip_id, reply } => {
            // Re-open scoped to the requested clip.
            match Reader::open_clip(&config.recording_id, &clip_id).await {
                Ok(new_reader) => {
                    *reader = new_reader;
                    *playing = false;
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(anyhow!("replay_clip_not_found: {e}")));
                }
            }
        }
        ReplayCommand::Play { clip_id, from_pts_90khz, to_pts_90khz, reply } => {
            // Re-scope if the operator overrode the range.
            if let Some(cid) = clip_id {
                match Reader::open_clip(&config.recording_id, &cid).await {
                    Ok(new_reader) => *reader = new_reader,
                    Err(e) => {
                        let _ = reply.send(Err(anyhow!("replay_clip_not_found: {e}")));
                        return;
                    }
                }
            }
            if let Some(from) = from_pts_90khz {
                if let Err(e) = reader.scrub_to(from).await {
                    let _ = reply.send(Err(e));
                    return;
                }
            }
            if let Some(to) = to_pts_90khz {
                reader.range.to_pts_90khz = to;
            }
            *playing = true;
            emit_play_started(events, flow_id, input_id, reader);
            let _ = reply.send(Ok(()));
        }
        ReplayCommand::Stop { reply } => {
            *playing = false;
            emit_play_stopped(events, flow_id, input_id);
            let _ = reply.send(Ok(()));
        }
        ReplayCommand::Scrub { pts_90khz, reply } => {
            match reader.scrub_to(pts_90khz).await {
                Ok(entry) => {
                    let _ = reply.send(Ok(ScrubAck {
                        pts_90khz: entry.pts_90khz,
                        segment_id: entry.segment_id,
                        byte_offset: entry.byte_offset,
                    }));
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
        }
    }
}

/// Pump a single 7-packet bundle from the reader to the broadcast
/// channel. Returns `Ok(true)` if a bundle was published, `Ok(false)`
/// on end-of-range / EOF.
async fn pump_one_bundle(
    reader: &mut Reader,
    per_input_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
) -> Result<bool> {
    use bytes::Bytes;
    use std::sync::atomic::Ordering;
    const PACKETS_PER_BUNDLE: usize = 7;

    let bundle = match reader.read_bundle(PACKETS_PER_BUNDLE).await? {
        Some(b) => b,
        None => return Ok(false),
    };
    let bundle_len = bundle.len();
    let pkt = RtpPacket {
        data: Bytes::from(bundle),
        sequence_number: 0,
        rtp_timestamp: 0,
        recv_time_us: now_micros(),
        is_raw_ts: true,
    };
    let _ = per_input_tx.send(pkt);
    flow_stats.input_bytes.fetch_add(bundle_len as u64, Ordering::Relaxed);
    flow_stats.input_packets.fetch_add(1, Ordering::Relaxed);
    Ok(true)
}

fn emit_play_started(events: &EventSender, flow_id: &str, input_id: &str, reader: &Reader) {
    events.emit_with_details(
        EventSeverity::Info,
        category::REPLAY,
        format!("Replay input '{input_id}' started playback"),
        Some(flow_id),
        serde_json::json!({
            "replay_event": "playback_started",
            "input_id": input_id,
            "from_pts_90khz": reader.range.from_pts_90khz,
            "to_pts_90khz": reader.range.to_pts_90khz,
        }),
    );
}

fn emit_play_stopped(events: &EventSender, flow_id: &str, input_id: &str) {
    events.emit_with_details(
        EventSeverity::Info,
        category::REPLAY,
        format!("Replay input '{input_id}' stopped"),
        Some(flow_id),
        serde_json::json!({
            "replay_event": "playback_stopped",
            "input_id": input_id,
        }),
    );
}

fn emit_eof(events: &EventSender, flow_id: &str, input_id: &str) {
    events.emit_with_details(
        EventSeverity::Info,
        category::REPLAY,
        format!("Replay input '{input_id}' reached end of range"),
        Some(flow_id),
        serde_json::json!({
            "replay_event": "playback_eof",
            "input_id": input_id,
        }),
    );
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
