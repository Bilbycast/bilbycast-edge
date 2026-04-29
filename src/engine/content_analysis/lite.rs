// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lite-tier (compressed-domain) content analyser.
//!
//! Subscribes to a flow's broadcast channel as an independent consumer and
//! runs a handful of cheap parsers on every MPEG-TS packet: GOP frame-type
//! cadence, container/codec signalling, SMPTE timecode, CEA-608/708 caption
//! presence, SCTE-35 cue presence, and Media Delivery Index (RFC 4445).
//!
//! Like the TR-101290 analyser, this task **cannot** block the hot path —
//! if it falls behind, it receives `broadcast::RecvError::Lagged(n)` and
//! silently skips packets, bumping a `lite_drops` counter on the shared
//! accumulator. All parsing is pure Rust; no new external dependencies.
//!
//! **Scope is deliberate**: the heavy lifting (GOP frame-type cadence,
//! full SPS/VUI HDR metadata, pic_timing SEI timecode decode, CEA-708
//! cc_data decode) is factored into the per-sub-module files so a
//! follow-up commit can deepen individual checks without touching this
//! task loop.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::manager::events::{EventSender, EventSeverity};
use crate::stats::collector::ContentAnalysisAccumulator;
use crate::stats::models::ContentAnalysisLiteStats;

use super::captions::CaptionsParser;
use super::gop::GopTracker;
use super::mdi::MdiSampler;
use super::scte35::Scte35Tracker;
use super::signalling::SignallingTracker;
use super::timecode::TimecodeTracker;

use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{
    ts_adaptation_field_control, ts_cc, ts_discontinuity_indicator, ts_has_payload, ts_pid,
    ts_pusi, RTP_HEADER_MIN_SIZE, TS_PACKET_SIZE, TS_SYNC_BYTE,
};

// PID 0x0000 is the PAT. Kept local to avoid an extra import.
const PAT_PID: u16 = 0x0000;

/// Sample-publish interval. Holding the snapshot for 250 ms bounds the
/// time between an in-memory state change and the next 1 Hz manager
/// snapshot picking it up.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(250);

/// Rolling window used by the MDI sampler. RFC 4445 is defined over 1 s.
const MDI_WINDOW: Duration = Duration::from_secs(1);

/// NDF threshold above which we consider the window "impaired" for the
/// `windows_above_threshold` counter and the `mdi_above_threshold` event.
/// 50 ms is the boundary most probe vendors use for `OK` → `Warning`.
const MDI_NDF_ALARM_MS: f32 = 50.0;

/// Minimum interval between two `content_analysis_caption_lost` events,
/// so a single flapping caption source can't flood the manager.
const EVENT_RATELIMIT: Duration = Duration::from_secs(30);

// ── Public API ─────────────────────────────────────────────────────────────

/// Spawn the Lite (compressed-domain) content-analysis task.
///
/// `flow_id` is carried so the event emitter can scope caption-lost /
/// SCTE-35 / MDI-threshold events back to the owning flow. `cancel`
/// lives below the flow's runtime token — a flow stop or a tier toggle
/// off aborts the task within one broadcast packet.
///
/// `frame_rate_rx` is the flow's frame-rate watch channel (published by
/// the `MediaAnalyzer`). Forwarded into the SMPTE timecode tracker so
/// it can strictly bound the `pic_timing` SEI `n_frames` field
/// (`FF < fps`). `None` keeps the validator on its loose fallback.
pub fn spawn_content_analysis_lite(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<ContentAnalysisAccumulator>,
    event_sender: EventSender,
    flow_id: String,
    cancel: CancellationToken,
    frame_rate_rx: Option<watch::Receiver<Option<f64>>>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(lite_loop(rx, stats, event_sender, flow_id, cancel, frame_rate_rx))
}

// ── Task Loop ──────────────────────────────────────────────────────────────

async fn lite_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<ContentAnalysisAccumulator>,
    events: EventSender,
    flow_id: String,
    cancel: CancellationToken,
    frame_rate_rx: Option<watch::Receiver<Option<f64>>>,
) {
    tracing::info!(flow_id = %flow_id, "content-analysis Lite analyser started");

    let mut state = LiteState::new();
    let mut publish_interval = tokio::time::interval(PUBLISH_INTERVAL);
    publish_interval.tick().await;
    let start = Instant::now();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(flow_id = %flow_id, "content-analysis Lite analyser stopping (cancelled)");
                break;
            }

            _ = publish_interval.tick() => {
                // Refresh the SMPTE timecode validator's fps from the
                // MediaAnalyzer watch channel. 250 ms cadence is plenty
                // fresh — fps changes once per encoder restart at most.
                if let Some(rx) = &frame_rate_rx {
                    state.timecode.set_frame_rate(*rx.borrow());
                }
                state.tick(start.elapsed(), &flow_id, &events);
                let snap = state.snapshot();
                stats.publish_lite(snap);
            }

            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        state.process_packet(&packet, &flow_id, &events);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.lite_drops.fetch_add(n, Ordering::Relaxed);
                        tracing::debug!(
                            flow_id = %flow_id,
                            dropped = n,
                            "content-analysis Lite lagged"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            flow_id = %flow_id,
                            "content-analysis Lite: broadcast closed"
                        );
                        break;
                    }
                }
            }
        }
    }
}

// ── Per-task State ─────────────────────────────────────────────────────────

/// Running state owned by the Lite analyser task. No locks — the task is
/// the single writer. Wire-shaped output is assembled on every tick and
/// pushed to the shared accumulator via `publish_lite`.
struct LiteState {
    // Discovered topology
    pmt_pids: HashMap<u16, u16>, // program_number -> PMT PID
    video_pid: Option<u16>,
    video_stream_type: Option<u8>,
    scte35_pids: HashSet<u16>,

    // Per-sub-check running state
    gop: GopTracker,
    signalling: SignallingTracker,
    timecode: TimecodeTracker,
    captions: CaptionsParser,
    scte35: Scte35Tracker,
    mdi: MdiSampler,

    /// Per-PID last continuity counter value — drives MDI MLR by
    /// counting unexpected CC steps on every TS-carrying PID we walk.
    cc_tracker: HashMap<u16, u8>,

    // Published-state memo for event emission
    captions_was_present: bool,
    last_caption_event_at: Option<Instant>,
    last_mdi_event_at: Option<Instant>,
    last_scte35_cue_count: u64,
}

impl LiteState {
    fn new() -> Self {
        Self {
            pmt_pids: HashMap::new(),
            video_pid: None,
            video_stream_type: None,
            scte35_pids: HashSet::new(),
            gop: GopTracker::new(),
            signalling: SignallingTracker::new(),
            timecode: TimecodeTracker::new(),
            captions: CaptionsParser::new(),
            scte35: Scte35Tracker::new(),
            mdi: MdiSampler::new(MDI_WINDOW),
            cc_tracker: HashMap::new(),
            captions_was_present: false,
            last_caption_event_at: None,
            last_mdi_event_at: None,
            last_scte35_cue_count: 0,
        }
    }

    /// Process a single packet off the broadcast channel. Strips RTP if
    /// present, then walks 188-byte TS packets.
    fn process_packet(&mut self, packet: &RtpPacket, flow_id: &str, events: &EventSender) {
        let payload = strip_rtp_header(packet);
        if payload.is_empty() {
            return;
        }

        // MDI is per-RTP-packet (not per-TS) since it's measuring network-level
        // delivery regularity. Keep the sampling at broadcast-tap granularity
        // so post-recovered SRT / RIST streams feed in too.
        self.mdi
            .observe_packet(packet.recv_time_us, payload.len());

        // TS-level processing
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= payload.len() {
            let ts = &payload[offset..offset + TS_PACKET_SIZE];
            if ts[0] == TS_SYNC_BYTE {
                self.process_ts_packet(ts, flow_id, events);
            }
            offset += TS_PACKET_SIZE;
        }
    }

    fn process_ts_packet(&mut self, pkt: &[u8], flow_id: &str, events: &EventSender) {
        let pid = ts_pid(pkt);
        let pusi = ts_pusi(pkt);
        let has_payload = ts_has_payload(pkt);
        let adaptation = ts_adaptation_field_control(pkt);
        let cc = ts_cc(pkt);
        let discontinuity = ts_discontinuity_indicator(pkt);

        // ── MDI MLR: per-PID CC tracking ──
        // PID 0x1FFF is the null PID — it has undefined CC behaviour per
        // MPEG-TS spec, so we skip it to avoid phantom alarms on muxer
        // stuffing patterns.
        if pid != 0x1FFF && !discontinuity {
            match self.cc_tracker.get(&pid).copied() {
                None => {
                    self.cc_tracker.insert(pid, cc);
                }
                Some(prev) => {
                    let expected = if has_payload { (prev + 1) & 0x0F } else { prev };
                    if cc != expected && cc != prev {
                        // cc == prev is a legitimate duplicate (spec allows
                        // one); anything else is a discontinuity we count
                        // toward MLR.
                        self.mdi.observe_cc_error();
                    }
                    self.cc_tracker.insert(pid, cc);
                }
            }
        }

        if !has_payload {
            return;
        }

        let payload_offset = if adaptation & 0x02 != 0 {
            // Adaptation field present — skip it
            5 + pkt[4] as usize
        } else {
            4
        };
        if payload_offset >= TS_PACKET_SIZE {
            return;
        }
        let payload = &pkt[payload_offset..];

        // PSI handling first — catalogues video PID and SCTE-35 PIDs.
        if pid == PAT_PID {
            if pusi {
                self.parse_pat(payload);
            }
            return;
        }

        if self.pmt_pids.values().any(|&p| p == pid) {
            if pusi {
                self.parse_pmt(payload, flow_id, events);
            }
            return;
        }

        // SCTE-35 section pickup. Stream_type 0x86 PIDs carry
        // splice_info_section directly (table_id = 0xFC).
        if self.scte35_pids.contains(&pid) {
            self.scte35.observe(pusi, payload, flow_id, events);
            return;
        }

        // Video PID: feed GOP / signalling / timecode / captions parsers.
        if Some(pid) == self.video_pid {
            self.gop.observe_ts(pusi, payload);
            self.signalling.observe_ts(pusi, payload);
            self.timecode.observe_ts(pusi, payload);
            self.captions.observe_ts(pusi, payload);
            return;
        }
    }

    /// Parse the PAT section to discover PMT PIDs. Minimal — skips CRC
    /// validation (TR-101290 already catches CRC errors on this PID).
    fn parse_pat(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        if 1 + pointer + 8 > payload.len() {
            return;
        }
        let section = &payload[1 + pointer..];
        if section.is_empty() || section[0] != 0x00 {
            return;
        }
        let section_length = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
        if 3 + section_length > section.len() {
            return;
        }
        // Skip through transport_stream_id(2) + version/current(1) + section_number(1) + last_section_number(1)
        // Loop over programs in section[8..(section_length - 1 - 4)] (subtract CRC + last fields)
        if section_length < 9 {
            return;
        }
        let body_end = 3 + section_length - 4; // exclude CRC32
        let mut i = 8;
        self.pmt_pids.clear();
        while i + 4 <= body_end {
            let program_number = ((section[i] as u16) << 8) | section[i + 1] as u16;
            let pmt_pid = (((section[i + 2] as u16) & 0x1F) << 8) | section[i + 3] as u16;
            if program_number != 0 {
                self.pmt_pids.insert(program_number, pmt_pid);
            }
            i += 4;
        }
    }

    /// Parse a PMT section to discover the first video PID and any SCTE-35
    /// PIDs (stream_type = 0x86). Caption detection is done inside video
    /// SEI; we only use the PMT to spot the PID we should be watching.
    fn parse_pmt(&mut self, payload: &[u8], flow_id: &str, events: &EventSender) {
        if payload.is_empty() {
            return;
        }
        let pointer = payload[0] as usize;
        if 1 + pointer + 12 > payload.len() {
            return;
        }
        let section = &payload[1 + pointer..];
        if section.is_empty() || section[0] != 0x02 {
            return;
        }
        let section_length = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
        if 3 + section_length > section.len() {
            return;
        }
        // Skip 9-byte fixed fields + program_info_length
        if section_length < 13 {
            return;
        }
        let program_info_length =
            (((section[10] as usize) & 0x0F) << 8) | section[11] as usize;
        let es_start = 12 + program_info_length;
        let body_end = 3 + section_length - 4;
        if es_start >= body_end {
            return;
        }

        let mut i = es_start;
        let mut first_video: Option<(u16, u8)> = None;
        let mut new_scte35: HashSet<u16> = HashSet::new();
        while i + 5 <= body_end {
            let stream_type = section[i];
            let es_pid = (((section[i + 1] as u16) & 0x1F) << 8) | section[i + 2] as u16;
            let es_info_length =
                (((section[i + 3] as usize) & 0x0F) << 8) | section[i + 4] as usize;

            // First video stream wins. Accept common TS video stream types.
            if first_video.is_none() && is_video_stream_type(stream_type) {
                first_video = Some((es_pid, stream_type));
            }
            if stream_type == 0x86 {
                new_scte35.insert(es_pid);
            }
            i += 5 + es_info_length;
        }

        if let Some((pid, stype)) = first_video {
            let is_change = self.video_pid != Some(pid) || self.video_stream_type != Some(stype);
            self.video_pid = Some(pid);
            self.video_stream_type = Some(stype);
            if is_change {
                self.gop.set_codec_from_stream_type(stype);
                self.signalling.set_codec_from_stream_type(stype);
            }
        }

        // Emit an Info event the first time we notice a SCTE-35 PID on the
        // stream. Subsequent PMT refreshes are quiet.
        if !new_scte35.is_subset(&self.scte35_pids) {
            let first_new: Vec<u16> = new_scte35
                .difference(&self.scte35_pids)
                .copied()
                .collect();
            if !first_new.is_empty() {
                events.send(crate::manager::events::Event {
                    severity: EventSeverity::Info,
                    category: "content_analysis_scte35_pid".into(),
                    message: format!(
                        "SCTE-35 PID discovered: {}",
                        first_new
                            .iter()
                            .map(|p| format!("0x{:04X}", p))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    details: Some(serde_json::json!({
                        "pids": first_new,
                    })),
                    flow_id: Some(flow_id.into()),
                    input_id: None,
                    output_id: None,
                });
            }
        }
        self.scte35_pids = new_scte35;
        self.scte35.set_pids(self.scte35_pids.iter().copied().collect());
    }

    /// Periodic housekeeping: compute MDI, emit threshold-crossing events,
    /// and detect caption loss.
    fn tick(&mut self, now_since_start: Duration, flow_id: &str, events: &EventSender) {
        // MDI rollover: if the sampler's window elapsed, publish new NDF/MLR.
        self.mdi.maybe_finalise_window();
        let ndf = self.mdi.last_delay_factor_ms();
        if ndf > MDI_NDF_ALARM_MS {
            let fire = self
                .last_mdi_event_at
                .map_or(true, |t| t.elapsed() >= EVENT_RATELIMIT);
            if fire {
                events.send(crate::manager::events::Event {
                    severity: EventSeverity::Warning,
                    category: "content_analysis_mdi_above_threshold".into(),
                    message: format!(
                        "MDI delay factor {:.1} ms above {:.0} ms threshold (MLR {:.1} pps)",
                        ndf,
                        MDI_NDF_ALARM_MS,
                        self.mdi.last_loss_rate_pps()
                    ),
                    details: Some(serde_json::json!({
                        "error_code": "content_analysis_mdi_above_threshold",
                        "delay_factor_ms": ndf,
                        "loss_rate_pps": self.mdi.last_loss_rate_pps(),
                        "threshold_ms": MDI_NDF_ALARM_MS,
                    })),
                    flow_id: Some(flow_id.into()),
                    input_id: None,
                    output_id: None,
                });
                self.last_mdi_event_at = Some(Instant::now());
            }
        }

        // Caption-loss detection — only valid after the flow has been
        // running long enough to have seen at least one caption packet.
        let captions_now_present = self.captions.is_present();
        if self.captions_was_present && !captions_now_present {
            let fire = self
                .last_caption_event_at
                .map_or(true, |t| t.elapsed() >= EVENT_RATELIMIT);
            if fire {
                events.send(crate::manager::events::Event {
                    severity: EventSeverity::Warning,
                    category: "content_analysis_caption_lost".into(),
                    message: "Closed-caption presence dropped off the video stream".into(),
                    details: Some(serde_json::json!({
                        "error_code": "content_analysis_caption_lost",
                    })),
                    flow_id: Some(flow_id.into()),
                    input_id: None,
                    output_id: None,
                });
                self.last_caption_event_at = Some(Instant::now());
            }
        }
        self.captions_was_present = captions_now_present;

        // SCTE-35 cue event — fire one Info event per new cue observed, but
        // debounce so a bursty ad-break doesn't flood the feed.
        let cue_count = self.scte35.cue_count();
        if cue_count > self.last_scte35_cue_count {
            events.send(crate::manager::events::Event {
                severity: EventSeverity::Info,
                category: "content_analysis_scte35_cue".into(),
                message: format!(
                    "SCTE-35 cue observed ({} since flow start)",
                    cue_count
                ),
                details: Some(serde_json::json!({
                    "cue_count": cue_count,
                    "last_command": self.scte35.last_command(),
                    "last_pts": self.scte35.last_pts(),
                })),
                flow_id: Some(flow_id.into()),
                input_id: None,
                output_id: None,
            });
            self.last_scte35_cue_count = cue_count;
        }

        let _ = now_since_start; // reserved for future decay calculations
    }

    fn snapshot(&self) -> ContentAnalysisLiteStats {
        ContentAnalysisLiteStats {
            gop: self.gop.snapshot(self.video_pid),
            signalling: self.signalling.snapshot(),
            timecode: self.timecode.snapshot(),
            captions: self.captions.snapshot(),
            scte35: self.scte35.snapshot(),
            mdi: self.mdi.snapshot(),
            analyser_drops: 0, // merged in at the accumulator layer
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Strip an RTP header if the packet carries one, matching the
/// `RtpPacket.is_raw_ts` flag. Returns the enclosed bytes — which for
/// PCM / ANC / ST 2110 payloads will not be MPEG-TS. The TS walker
/// downstream is a no-op on non-TS data (no sync byte at offset 0).
fn strip_rtp_header(packet: &RtpPacket) -> &[u8] {
    let data = packet.data.as_ref();
    if packet.is_raw_ts {
        return data;
    }
    if data.len() < RTP_HEADER_MIN_SIZE {
        return &[];
    }
    // RTP header: 12 bytes + 4 bytes * CSRC count + optional extension.
    let csrc_count = (data[0] & 0x0F) as usize;
    let ext_flag = (data[0] & 0x10) != 0;
    let mut offset = RTP_HEADER_MIN_SIZE + 4 * csrc_count;
    if ext_flag && offset + 4 <= data.len() {
        let ext_len_words = ((data[offset + 2] as usize) << 8) | data[offset + 3] as usize;
        offset += 4 + 4 * ext_len_words;
    }
    if offset > data.len() {
        return &[];
    }
    &data[offset..]
}

/// Recognise the MPEG-TS stream_type values we classify as video.
/// Non-exhaustive; the list mirrors the mapping
/// `crate::stats::collector::PerEsAccumulator::kind_for_stream_type`.
fn is_video_stream_type(st: u8) -> bool {
    matches!(
        st,
        0x01 // MPEG-1 video
        | 0x02 // MPEG-2 video
        | 0x10 // MPEG-4 visual
        | 0x1B // H.264/AVC
        | 0x20 // MVC subset
        | 0x24 // H.265/HEVC
        | 0x27 // HEVC aux
        | 0x42 // CAVS
        | 0xD1 // Dirac
        | 0xEA // VC-1
    )
}

// Sub-module snapshot impls live next to each tracker. This file only
// orchestrates task lifecycle + dispatch.
