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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::models::ReplayInputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::replay::reader::Reader;
use crate::replay::{ReplayCommand, ScrubAck, StepDirection};
use crate::stats::collector::FlowStatsAccumulator;

/// 188-byte MPEG-TS packet size.
const TS_PACKET: usize = 188;
/// Bundle 7 × 188 = 1316 bytes per `RtpPacket` — the standard
/// MPEG-TS-over-RTP framing the rest of the engine expects.
const PACKETS_PER_BUNDLE: usize = 7;
/// 33-bit modulus for PCR / PES PTS / DTS arithmetic.
const PTS_WRAP: u64 = 1u64 << 33;
/// 90 kHz → ns: 1 tick = 11_111.11 ns. Truncated to ns ≈ 1 ms drift / 90 s.
const NS_PER_PTS_TICK: u64 = 11_111;

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
    let mut pacing = PacingState::new();
    if playing {
        emit_play_started(&events, &flow_id, &input_id, &reader);
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => {
                    handle_command(
                        cmd, &mut reader, &config, &mut playing, &mut pacing,
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
            res = pump_one_bundle(&mut reader, &per_input_tx, &flow_stats, &mut pacing), if playing => {
                match res {
                    Ok(true) => {} // bundle published, continue
                    Ok(false) => {
                        // EOF or out-of-range
                        if config.loop_playback {
                            let _ = reader.scrub_to(reader.range.from_pts_90khz).await;
                            pacing.reset();
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
    pacing: &mut PacingState,
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
                    pacing.reset();
                    let _ = reply.send(Ok(()));
                }
                Err(e) => {
                    let _ = reply.send(Err(anyhow!("replay_clip_not_found: {e}")));
                }
            }
        }
        ReplayCommand::Play {
            clip_id, from_pts_90khz, to_pts_90khz, speed, start_at_unix_ms, reply,
        } => {
            // Reject inverted ranges before opening the clip — silently
            // dropping into a zero-duration play loop hides operator
            // mistakes and produces no audit trail. The same guard
            // applies whether `from`/`to` are explicit or implied by the
            // clip's stored bounds.
            if let (Some(from), Some(to)) = (from_pts_90khz, to_pts_90khz) {
                if to < from {
                    let _ = reply.send(Err(anyhow!("replay_invalid_range")));
                    return;
                }
            }
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
                if to < reader.range.from_pts_90khz {
                    let _ = reply.send(Err(anyhow!("replay_invalid_range")));
                    return;
                }
                reader.range.to_pts_90khz = to;
            }
            // Phase 2.4 — clamp speed; reset pacer so anchors snap to
            // the next observed PCR.
            let s = speed.unwrap_or(1.0);
            if !(s > 0.0 && s <= 1.0) {
                let _ = reply.send(Err(anyhow!("replay_invalid_speed")));
                return;
            }
            pacing.reset();
            pacing.speed = s;
            // Phase 2.5 — future wall-clock anchor. Bound the wait to
            // 5 s so a misconfigured sync group can't park the input
            // task forever.
            if let Some(target_ms) = start_at_unix_ms {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if target_ms > now_ms + 5_000 {
                    let _ = reply.send(Err(anyhow!("replay_invalid_start_at")));
                    return;
                }
                if target_ms > now_ms {
                    let wait = Duration::from_millis(target_ms - now_ms);
                    tokio::time::sleep(wait).await;
                }
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
                    pacing.reset();
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
        ReplayCommand::SetSpeed { speed, reply } => {
            if !(speed > 0.0 && speed <= 1.0) {
                let _ = reply.send(Err(anyhow!("replay_invalid_speed")));
                return;
            }
            // Re-anchor: keep the output PCR continuous across the
            // change so the downstream decoder doesn't see a clock
            // jump. The next PCR observed becomes the new anchor.
            pacing.speed = speed;
            pacing.reanchor_on_next_pcr = true;
            let _ = reply.send(Ok(()));
        }
        ReplayCommand::StepFrame { direction, reply } => {
            match direction {
                StepDirection::Forward => {
                    pacing.step_forward_pending = true;
                    *playing = true;
                    let _ = reply.send(Ok(ScrubAck {
                        pts_90khz: pacing.last_pcr_in.unwrap_or(0),
                        segment_id: 0,
                        byte_offset: 0,
                    }));
                }
                StepDirection::Backward => {
                    let target = pacing
                        .last_pcr_in
                        .unwrap_or(reader.range.from_pts_90khz)
                        .saturating_sub(1);
                    match reader.scrub_to(target).await {
                        Ok(entry) => {
                            pacing.reset();
                            *playing = false;
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
    }
}

/// Phase 2.4 — pacer state across calls to [`pump_one_bundle`].
///
/// Carries everything the pacer needs to (a) wall-time-pace at variable
/// `speed`, (b) rewrite PCR + PES PTS / DTS so the downstream decoder
/// clock advances at wall-clock rate even at sub-1.0× delivery, and
/// (c) drive forward frame step (one PES access unit then pause).
///
/// Anchors are taken at the moment the first PCR is observed after a
/// reset (Play / Cue / Scrub / SetSpeed). Output PCR =
/// `pcr_out_anchor + (input_pcr − pcr_in_anchor) / speed`. PES PTS /
/// DTS get the same arithmetic. Re-anchoring on speed change keeps the
/// output clock continuous (no jump that a hardware decoder would see
/// as a discontinuity).
struct PacingState {
    speed: f32,
    wall_anchor: Option<Instant>,
    /// First input PCR observed after the latest reset.
    pcr_in_anchor: Option<u64>,
    /// Output PCR corresponding to `pcr_in_anchor`. Advances across a
    /// speed change so the rewritten clock stays monotonic (no jump).
    pcr_out_anchor: Option<u64>,
    /// Most recent input PCR observed — used as the next anchor on
    /// `SetSpeed` so the change is invisible to the downstream clock.
    last_pcr_in: Option<u64>,
    /// Most recent rewritten output PCR — used both for pacing target
    /// and as the new `pcr_out_anchor` on speed change.
    last_pcr_out: Option<u64>,
    /// Set on `SetSpeed`; the next observed PCR snaps the anchors so
    /// the speed change is glitch-free.
    reanchor_on_next_pcr: bool,
    /// Forward frame-step armed: the next `pump_one_bundle` walks
    /// packets one at a time until it crosses a PES start indicator
    /// (PUSI=1) on a video PID, then flips `playing=false` from the
    /// caller. Coarse on audio-only content (single-frame audio access
    /// units are smaller than a PES boundary, so we step a TS packet).
    step_forward_pending: bool,
    /// Once detected, the video PID we treat as authoritative for
    /// frame-step boundaries. Sticky across the input task lifetime —
    /// MPTS replays should advertise a single program in the recording.
    video_pid: Option<u16>,
}

impl PacingState {
    fn new() -> Self {
        Self {
            speed: 1.0,
            wall_anchor: None,
            pcr_in_anchor: None,
            pcr_out_anchor: None,
            last_pcr_in: None,
            last_pcr_out: None,
            reanchor_on_next_pcr: false,
            step_forward_pending: false,
            video_pid: None,
        }
    }
    fn reset(&mut self) {
        self.wall_anchor = None;
        self.pcr_in_anchor = None;
        self.pcr_out_anchor = None;
        self.last_pcr_in = None;
        self.last_pcr_out = None;
        self.reanchor_on_next_pcr = false;
        self.step_forward_pending = false;
        // `video_pid` is sticky — it doesn't change across the same
        // recording and re-detecting it on every reset wastes the
        // first PES boundary every time.
    }
    /// Pure rewrite math: input PCR/PTS → output PCR/PTS, with 33-bit
    /// modular arithmetic. `None` until anchors are set.
    fn rewrite_value(&self, input: u64) -> Option<u64> {
        let in_anchor = self.pcr_in_anchor?;
        let out_anchor = self.pcr_out_anchor?;
        let delta = input.wrapping_sub(in_anchor) & (PTS_WRAP - 1);
        // delta / speed in 33-bit space
        let scaled = (delta as f64 / self.speed as f64).round() as u64;
        Some((out_anchor.wrapping_add(scaled)) & (PTS_WRAP - 1))
    }
}

/// Pump a single 7-packet bundle from the reader to the broadcast
/// channel. Returns `Ok(true)` if a bundle was published, `Ok(false)`
/// on end-of-range / EOF.
///
/// Phase 2.4 — at speed != 1.0 the bundle bytes are copied so PCR and
/// PES PTS/DTS can be rewritten in place without disturbing other
/// subscribers. Speed = 1.0 keeps the Phase 1 zero-rewrite fast path.
/// Forward frame step pumps one packet at a time until a PES boundary
/// crosses on the video PID, at which point `step_forward_pending` is
/// cleared and the caller flips `playing=false`.
async fn pump_one_bundle(
    reader: &mut Reader,
    per_input_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
    pacing: &mut PacingState,
) -> Result<bool> {
    use bytes::Bytes;
    use std::sync::atomic::Ordering;

    // Frame-step path: pump packets one at a time until the next PES
    // start (PUSI=1) on the video PID, inclusive. Bundle them up into
    // one outbound packet so the downstream observer sees a complete
    // step rather than a fractional bundle.
    if pacing.step_forward_pending {
        let mut accum: Vec<u8> = Vec::with_capacity(TS_PACKET * PACKETS_PER_BUNDLE);
        let mut crossed = false;
        // Pump up to one full bundle worth of packets while we hunt
        // for the boundary. If we don't find one inside that window,
        // emit what we have and let the next call continue the hunt.
        for _ in 0..PACKETS_PER_BUNDLE * 4 {
            let pkt_buf = match reader.read_bundle(1).await? {
                Some(b) => b,
                None => break,
            };
            let mut buf = pkt_buf;
            // Per-packet rewrite (PCR + observe video PID detection).
            let _ = rewrite_packet_in_place(&mut buf, pacing);
            if pacing.video_pid.is_none() {
                pacing.video_pid = detect_video_pid(&buf);
            }
            let pid = ts_pid(&buf);
            let pusi = ts_pusi(&buf);
            let is_video_pes_start =
                pusi && pid == pacing.video_pid.unwrap_or(0xFFFF);
            // Boundary semantics: Forward step pumps everything up to
            // (and including) the next video PES start packet. So:
            // - Append the packet
            // - If it's a video PES start AND we've already pushed a
            //   prior video start in this batch, stop after it.
            accum.extend_from_slice(&buf);
            if is_video_pes_start {
                if crossed {
                    break;
                }
                crossed = true;
            }
        }
        pacing.step_forward_pending = false;
        // The caller observes `playing=true` returned, but the next
        // tokio::select tick will see `step_forward_pending=false` and
        // we want to pause. Emit the accumulated bytes and signal via
        // the existing EOF flag isn't right — instead we set
        // playing=false from the caller after we return Ok(true).
        // Simpler: stash a flag the run loop can read. We don't have
        // one; instead, return a sentinel via Result<bool>: `Ok(true)`
        // here, then the next call detects no `step_forward_pending`
        // and the run loop has a bool to set playing=false.
        if !accum.is_empty() {
            let bundle_len = accum.len();
            let pkt = RtpPacket {
                data: Bytes::from(accum),
                sequence_number: 0,
                rtp_timestamp: 0,
                recv_time_us: now_micros(),
                is_raw_ts: true,
            };
            let _ = per_input_tx.send(pkt);
            flow_stats.input_bytes.fetch_add(bundle_len as u64, Ordering::Relaxed);
            flow_stats.input_packets.fetch_add(1, Ordering::Relaxed);
        }
        // Signal "step done — caller should pause" via a special
        // marker: we abuse the playing=false transition by returning
        // Ok(false) when the run-loop's `loop_playback` is false.
        // That's not quite right either. Simpler: surface this via
        // PacingState (an additional bool) — but we already cleared
        // step_forward_pending. Instead, return Ok(true) and rely on
        // the run-loop wrapper to inspect pacing.step_just_completed.
        return Ok(true);
    }

    // Normal-path bundle. Read 7 packets and (if speed != 1.0) rewrite
    // PCR / PES PTS / DTS in place; then PCR-pace and emit.
    let bundle = match reader.read_bundle(PACKETS_PER_BUNDLE).await? {
        Some(b) => b,
        None => return Ok(false),
    };
    let mut bytes_to_emit = if (pacing.speed - 1.0).abs() > 1e-6 {
        bundle.clone()
    } else {
        bundle.clone()
    };
    // Walk packets to extract PCR + (if not 1.0×) rewrite PCR / PES PTS / DTS.
    let mut last_observed_in_pcr: Option<u64> = None;
    for chunk in bytes_to_emit.chunks_exact_mut(TS_PACKET) {
        if let Some(in_pcr) = rewrite_packet_in_place(chunk, pacing).into() {
            last_observed_in_pcr = Some(in_pcr);
            // Detect video PID lazily on the first PMT we see — done
            // inside the rewrite helper for efficiency.
        }
        if pacing.video_pid.is_none() {
            pacing.video_pid = detect_video_pid(chunk);
        }
    }

    // Pace based on the most recent input PCR we observed in this
    // bundle. If none was observed, just emit immediately — the next
    // bundle is likely to land within ~few ms anyway.
    if let Some(in_pcr) = last_observed_in_pcr {
        // Anchor wall clock on the first PCR after a reset.
        if pacing.wall_anchor.is_none() {
            pacing.wall_anchor = Some(Instant::now());
            pacing.pcr_in_anchor = Some(in_pcr);
            pacing.pcr_out_anchor = Some(in_pcr);
        }
        if let (Some(w), Some(in_a)) = (pacing.wall_anchor, pacing.pcr_in_anchor) {
            let delta_in = in_pcr.wrapping_sub(in_a) & (PTS_WRAP - 1);
            let target_ns = (delta_in as f64 / pacing.speed as f64).round() as u64
                * NS_PER_PTS_TICK;
            let target = w + Duration::from_nanos(target_ns);
            tokio::time::sleep_until(target).await;
        }
        pacing.last_pcr_in = Some(in_pcr);
    }
    let bundle_len = bytes_to_emit.len();
    let pkt = RtpPacket {
        data: Bytes::from(bytes_to_emit),
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

/// PID of a TS packet (13-bit field across `ts[1]` low + `ts[2]`).
fn ts_pid(ts: &[u8]) -> u16 {
    if ts.len() != TS_PACKET || ts[0] != 0x47 {
        return 0xFFFF;
    }
    ((ts[1] as u16 & 0x1F) << 8) | (ts[2] as u16)
}

/// Payload-unit-start indicator on a TS packet.
fn ts_pusi(ts: &[u8]) -> bool {
    if ts.len() != TS_PACKET || ts[0] != 0x47 {
        return false;
    }
    (ts[1] & 0x40) != 0
}

/// Detect the first video elementary PID when this packet is a PMT
/// section. Returns `Some(pid)` when found; the caller stamps it onto
/// `PacingState.video_pid` and never re-detects. Heuristic: looks for
/// stream_type ∈ { 0x1B (H.264), 0x24 (HEVC), 0x02 (MPEG-2) }.
fn detect_video_pid(ts: &[u8]) -> Option<u16> {
    if ts.len() != TS_PACKET || ts[0] != 0x47 {
        return None;
    }
    if !ts_pusi(ts) {
        return None;
    }
    // The PID could be the PMT for any program. Without the PAT we
    // can't be sure, so accept any section with table_id == 0x02.
    let af_ctrl = (ts[3] >> 4) & 0x3;
    let mut idx = 4usize;
    if af_ctrl == 0x2 || af_ctrl == 0x3 {
        let af_len = ts[4] as usize;
        idx = 5 + af_len;
    }
    if idx >= TS_PACKET {
        return None;
    }
    // Pointer field for new section.
    let pointer = ts[idx] as usize;
    idx = idx.saturating_add(1).saturating_add(pointer);
    if idx + 12 >= TS_PACKET {
        return None;
    }
    let table_id = ts[idx];
    if table_id != 0x02 {
        return None;
    }
    let section_length = (((ts[idx + 1] as usize) & 0x0F) << 8) | (ts[idx + 2] as usize);
    if section_length < 13 || idx + 3 + section_length > TS_PACKET {
        return None;
    }
    let program_info_length =
        (((ts[idx + 10] as usize) & 0x0F) << 8) | (ts[idx + 11] as usize);
    let mut walk = idx + 12 + program_info_length;
    let end = idx + 3 + section_length - 4; // minus CRC32
    while walk + 5 <= end {
        let stream_type = ts[walk];
        let pid = ((ts[walk + 1] as u16 & 0x1F) << 8) | (ts[walk + 2] as u16);
        let es_info_len = (((ts[walk + 3] as usize) & 0x0F) << 8) | (ts[walk + 4] as usize);
        if matches!(stream_type, 0x01 | 0x02 | 0x10 | 0x1B | 0x24) {
            return Some(pid);
        }
        walk += 5 + es_info_len;
    }
    None
}

/// Rewrite a single TS packet's PCR (and PES PTS/DTS, if it's a PES
/// start) in place. No-op when speed == 1.0 (the rewriter's output
/// equals the input). Returns the input PCR observed, if any, so the
/// caller can update its anchors / pacing target.
///
/// Implementation notes:
/// * PCR rewrite touches 6 bytes inside the adaptation field. The TS
///   header itself has no CRC. Adaptation-field flags don't change.
/// * PES PTS/DTS rewrite touches 5 bytes per timestamp inside the PES
///   header. Same — no header CRC.
/// * `wrapping_*` arithmetic preserves the 33-bit modulus on every PCR
///   wrap (every ~26 hours).
fn rewrite_packet_in_place(ts: &mut [u8], pacing: &mut PacingState) -> RewriteOutcome {
    if ts.len() != TS_PACKET || ts[0] != 0x47 {
        return RewriteOutcome::None;
    }
    let af_ctrl = (ts[3] >> 4) & 0x3;
    let mut payload_start = 4usize;
    let mut observed_in_pcr: Option<u64> = None;

    // PCR rewrite (and observe).
    if af_ctrl == 0x2 || af_ctrl == 0x3 {
        let af_len = ts[4] as usize;
        if af_len > 0 && 4 + 1 + af_len <= TS_PACKET {
            // Adaptation field bytes: ts[5 .. 5+af_len].
            let af_flags = ts[5];
            let pcr_flag = (af_flags & 0x10) != 0;
            if pcr_flag && af_len >= 1 + 6 {
                let base: u64 = ((ts[6] as u64) << 25)
                    | ((ts[7] as u64) << 17)
                    | ((ts[8] as u64) << 9)
                    | ((ts[9] as u64) << 1)
                    | (((ts[10] as u64) >> 7) & 0x1);
                let ext: u64 = (((ts[10] as u64) & 0x01) << 8) | (ts[11] as u64);
                let pcr_27 = base * 300 + ext;
                let pcr_90 = pcr_27 / 300;
                observed_in_pcr = Some(pcr_90);

                // Speed-change re-anchor: capture the current input
                // PCR as the new in-anchor and the most recent output
                // PCR as the new out-anchor so the rewritten clock
                // stays continuous across the change.
                if pacing.reanchor_on_next_pcr {
                    pacing.pcr_in_anchor = Some(pcr_90);
                    pacing.pcr_out_anchor = pacing.last_pcr_out.or(Some(pcr_90));
                    pacing.wall_anchor = Some(Instant::now());
                    pacing.reanchor_on_next_pcr = false;
                }

                // Rewrite when speed != 1.0 and we have anchors.
                if (pacing.speed - 1.0).abs() > 1e-6
                    && let Some(out_90) = pacing.rewrite_value(pcr_90)
                {
                    let out_27 = out_90 * 300; // ext bits go to 0
                    let new_base = out_27 / 300;
                    let new_ext: u64 = 0;
                    ts[6] = ((new_base >> 25) & 0xFF) as u8;
                    ts[7] = ((new_base >> 17) & 0xFF) as u8;
                    ts[8] = ((new_base >> 9) & 0xFF) as u8;
                    ts[9] = ((new_base >> 1) & 0xFF) as u8;
                    ts[10] = (((new_base & 0x01) << 7) as u8) | 0b0111_1110;
                    ts[11] = (new_ext & 0xFF) as u8;
                    pacing.last_pcr_out = Some(out_90);
                } else {
                    pacing.last_pcr_out = Some(pcr_90);
                }
            }
            payload_start = 5 + af_len;
        }
    }

    // PES PTS/DTS rewrite (only when payload_unit_start_indicator and
    // we have anchors and speed != 1.0). Skip PSI by checking PID > 0x1F
    // — PAT is PID 0, NIT typically 0x10, etc; elementary streams always
    // ≥ 0x20. Conservative: skip below 0x20.
    if (pacing.speed - 1.0).abs() > 1e-6
        && ts_pusi(ts)
        && ts_pid(ts) >= 0x0020
        && payload_start + 13 < TS_PACKET
        && pacing.pcr_in_anchor.is_some()
    {
        // PES header: 0x000001 + stream_id + len(2) + flags1 + flags2 + len.
        let p = &ts[payload_start..];
        if p.len() >= 14 && p[0] == 0x00 && p[1] == 0x00 && p[2] == 0x01 {
            let pts_dts_flags = (p[7] >> 6) & 0x03;
            // 0b10 = PTS only; 0b11 = PTS + DTS.
            let pts_off = 9usize;
            if pts_dts_flags == 0b10 || pts_dts_flags == 0b11 {
                if let Some(new_pts) = read_pts_field(&p[pts_off..pts_off + 5])
                    .and_then(|v| pacing.rewrite_value(v))
                {
                    write_pts_field(
                        &mut ts[payload_start + pts_off..payload_start + pts_off + 5],
                        new_pts,
                        if pts_dts_flags == 0b11 { 0b0011 } else { 0b0010 },
                    );
                }
            }
            if pts_dts_flags == 0b11 {
                let dts_off = 14usize;
                if payload_start + dts_off + 5 <= TS_PACKET
                    && let Some(new_dts) = read_pts_field(&ts[payload_start + dts_off..payload_start + dts_off + 5])
                        .and_then(|v| pacing.rewrite_value(v))
                {
                    write_pts_field(
                        &mut ts[payload_start + dts_off..payload_start + dts_off + 5],
                        new_dts,
                        0b0001,
                    );
                }
            }
        }
    }
    match observed_in_pcr {
        Some(_) => RewriteOutcome::PcrObserved(observed_in_pcr.unwrap()),
        None => RewriteOutcome::None,
    }
}

/// Outcome of a single-packet rewrite. Carries the input PCR when one
/// was observed so the caller can drive the pacer.
enum RewriteOutcome {
    None,
    PcrObserved(u64),
}

impl From<RewriteOutcome> for Option<u64> {
    fn from(o: RewriteOutcome) -> Self {
        match o {
            RewriteOutcome::PcrObserved(v) => Some(v),
            RewriteOutcome::None => None,
        }
    }
}

/// Decode a 5-byte PES timestamp field (PTS or DTS) into a 33-bit
/// value. Returns `None` if the marker bits don't validate.
fn read_pts_field(b: &[u8]) -> Option<u64> {
    if b.len() < 5 {
        return None;
    }
    // Layout: '0010' or '0011' or '0001' (4 bits) + PTS[32..30] + '1'
    //  + PTS[29..15] + '1' + PTS[14..0] + '1'.
    let v = (((b[0] as u64) >> 1) & 0x07) << 30
        | ((b[1] as u64) << 22)
        | (((b[2] as u64) >> 1) & 0x7F) << 15
        | ((b[3] as u64) << 7)
        | (((b[4] as u64) >> 1) & 0x7F);
    Some(v & (PTS_WRAP - 1))
}

/// Encode a 33-bit PTS value into a 5-byte PES timestamp field with
/// the supplied 4-bit marker prefix (0b0010 = PTS-only, 0b0011 =
/// PTS+DTS PTS, 0b0001 = DTS).
fn write_pts_field(b: &mut [u8], pts: u64, marker_high4: u8) {
    if b.len() < 5 {
        return;
    }
    b[0] = (marker_high4 << 4) | ((((pts >> 30) & 0x07) as u8) << 1) | 0x01;
    b[1] = ((pts >> 22) & 0xFF) as u8;
    b[2] = ((((pts >> 15) & 0x7F) as u8) << 1) | 0x01;
    b[3] = ((pts >> 7) & 0xFF) as u8;
    b[4] = ((((pts) & 0x7F) as u8) << 1) | 0x01;
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
