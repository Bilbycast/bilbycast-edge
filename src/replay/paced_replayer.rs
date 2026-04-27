// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! PTS-paced TS publisher for the replay input.
//!
//! Reads TS packets from a [`Reader`] and publishes them onto the
//! per-input broadcast channel paced by their embedded PCRs. Falls back
//! to a fixed-bitrate clock when PCRs are sparse.
//!
//! Mirrors the rate logic in [`crate::engine::input_media_player`] —
//! specifically the PCR-paced TS-file path — so the replay output
//! "feels" exactly like a live source from the downstream perspective.
//!
//! Phase 1 uses an inline pump inside `engine::input_replay` so each
//! iteration can be `tokio::select!`-ed against the per-input command
//! channel. This module remains as a standalone helper for future
//! callers that want a non-interruptible pump (e.g. a render-to-file
//! offline export); marked `#[allow(dead_code)]` until then.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::engine::packet::RtpPacket;
use crate::stats::collector::FlowStatsAccumulator;

use super::reader::Reader;

/// 188-byte MPEG-TS packet size.
const TS_PACKET: usize = 188;
/// Bundle 7 × 188 = 1316 bytes per `RtpPacket` — the standard
/// MPEG-TS-over-RTP framing the rest of the engine expects.
const PACKETS_PER_BUNDLE: usize = 7;
const BUNDLE_SIZE: usize = TS_PACKET * PACKETS_PER_BUNDLE;
/// Fallback bitrate when no PCR is observed in the first second of
/// reading. Matches `input_media_player`'s default.
const DEFAULT_FALLBACK_BPS: u64 = 4_000_000;

/// Pump a `Reader` to a broadcast channel, paced by PCR. Returns when
/// the reader hits end-of-range, end-of-recording, or `cancel` fires.
pub async fn pump(
    mut reader: Reader,
    per_input_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    loop_playback: bool,
) -> Result<()> {
    let mut wall_anchor: Option<Instant> = None;
    let mut pcr_anchor: Option<u64> = None;
    let mut accumulated_pts: u64 = reader.range.start_entry.pts_90khz;
    let mut last_pcr: Option<u64> = None;
    let mut bytes_since_pcr: u64 = 0;
    let mut sequence_number: u16 = 0;
    let mut rtp_timestamp: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let bundle = match reader.read_bundle(PACKETS_PER_BUNDLE).await? {
            Some(b) => b,
            None => {
                if loop_playback {
                    let entry = reader.scrub_to(reader.range.from_pts_90khz).await?;
                    accumulated_pts = entry.pts_90khz;
                    wall_anchor = None;
                    pcr_anchor = None;
                    last_pcr = None;
                    continue;
                }
                return Ok(());
            }
        };
        // Walk packets to extract PCR + check for early termination at
        // the clip's out_pts.
        for chunk in bundle.chunks_exact(TS_PACKET) {
            if let Some(pcr) = extract_pcr_90khz(chunk) {
                let wrap_modulus: u64 = 1u64 << 33;
                let delta = match last_pcr {
                    None => 0u64,
                    Some(prev) => pcr.wrapping_sub(prev) & (wrap_modulus - 1),
                };
                accumulated_pts = accumulated_pts.wrapping_add(delta);
                last_pcr = Some(pcr);
                bytes_since_pcr = 0;
            } else {
                bytes_since_pcr += TS_PACKET as u64;
            }
        }
        // Hard stop at clip out point.
        if accumulated_pts >= reader.range.to_pts_90khz {
            return Ok(());
        }
        // Pacing: aim wall time relative to the recording PTS span.
        if wall_anchor.is_none() {
            wall_anchor = Some(Instant::now());
            pcr_anchor = Some(accumulated_pts);
        }
        if let (Some(anchor), Some(pcr_start)) = (wall_anchor, pcr_anchor) {
            let elapsed_pts = accumulated_pts.saturating_sub(pcr_start);
            // 90 kHz → ns: * 1_000_000_000 / 90_000 = * 11111.11
            let elapsed_ns: u64 = elapsed_pts.saturating_mul(11_111);
            let target = anchor + Duration::from_nanos(elapsed_ns);
            sleep_until(target).await;
        }
        // Fallback paint: when no PCR has been observed, pace by
        // `DEFAULT_FALLBACK_BPS`.
        if last_pcr.is_none() && bytes_since_pcr > 0 {
            let pace_ns = (bytes_since_pcr * 8 * 1_000_000_000) / DEFAULT_FALLBACK_BPS;
            sleep_until(Instant::now() + Duration::from_nanos(pace_ns)).await;
        }
        sequence_number = sequence_number.wrapping_add(1);
        rtp_timestamp = rtp_timestamp.wrapping_add(1);
        let bundle_len = bundle.len();
        let pkt = RtpPacket {
            data: Bytes::from(bundle),
            sequence_number,
            rtp_timestamp,
            recv_time_us: now_micros(),
            is_raw_ts: true,
        };
        let _ = per_input_tx.send(pkt);
        flow_stats.input_bytes.fetch_add(bundle_len as u64, Ordering::Relaxed);
        flow_stats.input_packets.fetch_add(1, Ordering::Relaxed);
    }
}

/// Extract the 90 kHz PCR from a TS packet's adaptation field, if
/// present.
fn extract_pcr_90khz(ts: &[u8]) -> Option<u64> {
    if ts.len() != TS_PACKET || ts[0] != 0x47 {
        return None;
    }
    let adaptation_field_control = (ts[3] >> 4) & 0x3;
    if adaptation_field_control != 0x2 && adaptation_field_control != 0x3 {
        return None;
    }
    let af_len = ts[4] as usize;
    if af_len == 0 || 5 + af_len > TS_PACKET {
        return None;
    }
    let af = &ts[5..5 + af_len];
    if af.is_empty() {
        return None;
    }
    let pcr_flag = (af[0] & 0x10) != 0;
    if !pcr_flag || af.len() < 7 {
        return None;
    }
    let base: u64 = ((af[1] as u64) << 25)
        | ((af[2] as u64) << 17)
        | ((af[3] as u64) << 9)
        | ((af[4] as u64) << 1)
        | (((af[5] as u64) >> 7) & 0x1);
    let ext: u64 = (((af[5] as u64) & 0x01) << 8) | (af[6] as u64);
    let pcr_27 = base * 300 + ext;
    Some(pcr_27 / 300)
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
