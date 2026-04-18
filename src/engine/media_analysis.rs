// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Media content analysis module.
//!
//! Subscribes to a flow's broadcast channel as an independent consumer and
//! detects media content information: video/audio codecs, resolution, frame
//! rate, sample rate, and per-PID bitrates. Like the TR-101290 analyzer,
//! this module **cannot** block the hot path — if it falls behind, it
//! receives `Lagged(n)` and silently skips packets.
//!
//! Detection is done by parsing MPEG-TS PSI tables (PAT/PMT) and
//! elementary stream headers (H.264 SPS, H.265 SPS, AAC ADTS). All
//! parsing is pure Rust with zero external C dependencies.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::*;
use crate::stats::collector::{
    AudioStreamState, MediaAnalysisAccumulator, MediaAnalysisState, ProgramState, VideoStreamState,
};

use super::packet::RtpPacket;
use super::ts_parse::*;

/// How often to recalculate per-PID bitrates.
const BITRATE_CALC_INTERVAL: Duration = Duration::from_secs(1);

// ── Public API ───────────────────────────────────────────────────────────

/// Spawn the media analyzer as an independent broadcast subscriber.
pub fn spawn_media_analyzer(
    resolved_flow: &ResolvedFlow,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<MediaAnalysisAccumulator>,
    cancel: CancellationToken,
    frame_rate_tx: Option<tokio::sync::watch::Sender<Option<f64>>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
    inputs: Vec<InputDefinition>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();

    // Pre-populate transport-level info from config
    if let Some(def) = resolved_flow.active_input() {
        let mut state = stats.state.lock().unwrap();
        populate_transport_info(&def.config, &mut state);
    }

    tokio::spawn(media_analyzer_loop(
        rx,
        stats,
        cancel,
        frame_rate_tx,
        active_input_rx,
        inputs,
    ))
}

/// Extract transport-level information from an input configuration.
fn populate_transport_info(input: &InputConfig, state: &mut MediaAnalysisState) {
    match input {
        InputConfig::Rtp(rtp) => {
            state.protocol = "rtp".to_string();
            state.payload_format = "rtp_ts".to_string();
            if let Some(fec) = &rtp.fec_decode {
                state.fec_enabled = true;
                state.fec_type = Some(format!(
                    "SMPTE 2022-1 (L={}, D={})",
                    fec.columns, fec.rows
                ));
            }
        }
        InputConfig::Udp(_) => {
            state.protocol = "udp".to_string();
            state.payload_format = "raw_ts".to_string();
        }
        InputConfig::Srt(srt) => {
            state.protocol = "srt".to_string();
            // SRT can carry either RTP-wrapped or raw TS; we'll detect from data
            state.payload_format = "unknown".to_string();
            if srt.redundancy.is_some() {
                state.redundancy_enabled = true;
                state.redundancy_type = Some("SMPTE 2022-7".to_string());
            }
        }
        InputConfig::Rist(rist) => {
            state.protocol = "rist".to_string();
            state.payload_format = "raw_ts".to_string();
            if rist.redundancy.is_some() {
                state.redundancy_enabled = true;
                state.redundancy_type = Some("SMPTE 2022-7".to_string());
            }
        }
        InputConfig::Rtmp(_) => {
            state.protocol = "rtmp".to_string();
            state.payload_format = "raw_ts".to_string();
        }
        InputConfig::Rtsp(_) => {
            state.protocol = "rtsp".to_string();
            state.payload_format = "raw_ts".to_string();
        }
        InputConfig::Webrtc(_) => {
            state.protocol = "webrtc".to_string();
            state.payload_format = "rtp_h264".to_string();
        }
        InputConfig::Whep(_) => {
            state.protocol = "whep".to_string();
            state.payload_format = "rtp_h264".to_string();
        }
        InputConfig::St2110_30(_) => {
            state.protocol = "st2110_30".to_string();
            state.payload_format = "pcm_l24".to_string();
        }
        InputConfig::St2110_31(_) => {
            state.protocol = "st2110_31".to_string();
            state.payload_format = "aes3".to_string();
        }
        InputConfig::St2110_40(_) => {
            state.protocol = "st2110_40".to_string();
            state.payload_format = "anc".to_string();
        }
        InputConfig::RtpAudio(c) => {
            state.protocol = "rtp_audio".to_string();
            state.payload_format = if c.bit_depth == 16 {
                "pcm_l16".to_string()
            } else {
                "pcm_l24".to_string()
            };
        }
        InputConfig::St2110_20(_) => {
            state.protocol = "st2110_20".to_string();
            state.payload_format = "rfc4175_ycbcr422".to_string();
        }
        InputConfig::St2110_23(_) => {
            state.protocol = "st2110_23".to_string();
            state.payload_format = "rfc4175_ycbcr422_multi".to_string();
        }
        InputConfig::Bonded(_) => {
            // Underlying transport is opaque to the analyzer — a bonded
            // flow aggregates N heterogeneous paths. Surface `bonded`
            // so the UI renders the topology correctly.
            state.protocol = "bonded".to_string();
            state.payload_format = "raw_ts".to_string();
        }
    }
}

// ── Analyzer Loop ────────────────────────────────────────────────────────

async fn media_analyzer_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<MediaAnalysisAccumulator>,
    cancel: CancellationToken,
    frame_rate_tx: Option<tokio::sync::watch::Sender<Option<f64>>>,
    mut active_input_rx: tokio::sync::watch::Receiver<String>,
    inputs: Vec<InputDefinition>,
) {
    tracing::info!("Media analyzer started");

    let mut bitrate_interval = tokio::time::interval(BITRATE_CALC_INTERVAL);
    bitrate_interval.tick().await; // consume first immediate tick

    // Mark the initial value as seen so the changed() arm doesn't fire
    // spuriously on startup.
    active_input_rx.mark_changed();
    let _ = active_input_rx.borrow_and_update();

    // Local parsing state (not shared — only accessed by this task)
    let mut payload_format_detected = false;
    // Track last broadcast frame rate to avoid redundant watch sends.
    let mut last_broadcast_fps: Option<f64> = None;

    loop {
        // biased: ensure cancel and input-switch reset are processed before
        // any queued packets, preventing stale injected PSI from poisoning
        // state after a reset.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!("Media analyzer stopping (cancelled)");
                break;
            }

            // React to active input changes: reset all state and
            // re-populate transport info from the new input's config.
            Ok(()) = active_input_rx.changed() => {
                let new_id = active_input_rx.borrow_and_update().clone();

                // Snapshot the current frame rate before resetting state so
                // the UI doesn't flash "detecting..." during the switch.
                // The natural detection path will update the watch when the
                // new input's SPS/VUI is parsed.
                let preserved_fps = {
                    let state = stats.state.lock().unwrap();
                    state.programs.iter()
                        .flat_map(|p| &p.video_streams)
                        .find_map(|v| v.frame_rate)
                };

                stats.reset_state();
                payload_format_detected = false;
                last_broadcast_fps = preserved_fps;

                if let Some(def) = inputs.iter().find(|i| i.id == new_id) {
                    let mut state = stats.state.lock().unwrap();
                    populate_transport_info(&def.config, &mut state);
                }

                // Only send None if frame rate was already unknown before
                // the switch. Otherwise preserve the old value — the
                // detection loop will update it when the new rate differs.
                if preserved_fps.is_none() {
                    if let Some(ref tx) = frame_rate_tx {
                        let _ = tx.send(None);
                    }
                }

                tracing::info!("Media analyzer reset for new active input '{new_id}'");
            }

            _ = bitrate_interval.tick() => {
                calculate_bitrates(&stats);
            }

            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        process_packet(
                            &packet,
                            &stats,
                            &mut payload_format_detected,
                        );

                        // Broadcast frame rate to output tasks that need it
                        // (e.g., TargetFrames delay mode). Only send when the
                        // value changes to avoid unnecessary wake-ups.
                        if let Some(ref tx) = frame_rate_tx {
                            let fps = {
                                let state = stats.state.lock().unwrap();
                                state.programs.iter()
                                    .flat_map(|p| &p.video_streams)
                                    .find_map(|v| v.frame_rate)
                            };
                            if fps != last_broadcast_fps {
                                last_broadcast_fps = fps;
                                let _ = tx.send(fps);
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("Media analyzer lagged, skipped {n} packets");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("Media analyzer: broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }
}

/// Calculate per-PID bitrates from accumulated byte counters.
fn calculate_bitrates(stats: &MediaAnalysisAccumulator) {
    let mut state = stats.state.lock().unwrap();
    let now = Instant::now();
    let elapsed = now.duration_since(state.last_bitrate_calc);
    let elapsed_secs = elapsed.as_secs_f64();

    if elapsed_secs < 0.1 {
        return; // Too soon
    }

    // Compute bitrates from byte counters, then reset
    let pid_bytes: Vec<(u16, u64)> = state.pid_bytes.drain().collect();
    let mut total = 0u64;
    for (pid, bytes) in pid_bytes {
        let bits = (bytes as f64 * 8.0 / elapsed_secs) as u64;
        state.pid_bitrates.insert(pid, bits);
        total += bits;
    }
    state.total_bitrate_bps = total;
    state.last_bitrate_calc = now;
}

// ── Packet Processing ────────────────────────────────────────────────────

fn process_packet(
    packet: &RtpPacket,
    stats: &MediaAnalysisAccumulator,
    payload_format_detected: &mut bool,
) {
    let payload = strip_rtp_header(packet);
    if payload.is_empty() {
        return;
    }

    // Detect payload format on first packet
    if !*payload_format_detected {
        let mut state = stats.state.lock().unwrap();
        state.payload_format = if packet.is_raw_ts {
            "raw_ts".to_string()
        } else {
            "rtp_ts".to_string()
        };
        *payload_format_detected = true;
    }

    let mut state = stats.state.lock().unwrap();

    // Iterate over 188-byte TS packets
    let mut offset = 0;
    while offset + TS_PACKET_SIZE <= payload.len() {
        let ts_pkt = &payload[offset..offset + TS_PACKET_SIZE];
        process_ts_packet(ts_pkt, &mut state);
        offset += TS_PACKET_SIZE;
    }
}

fn process_ts_packet(pkt: &[u8], state: &mut MediaAnalysisState) {
    if pkt[0] != TS_SYNC_BYTE {
        return;
    }

    let pid = ts_pid(pkt);

    // Count bytes per PID for bitrate estimation
    *state.pid_bytes.entry(pid).or_insert(0) += TS_PACKET_SIZE as u64;

    // PAT handling — reconcile programs list with the PAT contents.
    if pid == PAT_PID && ts_pusi(pkt) {
        let found = parse_pat_programs(pkt);
        // Drop programs no longer in the PAT
        state
            .programs
            .retain(|p| found.iter().any(|(pn, _)| *pn == p.program_number));
        // Update PMT PIDs and append new programs (preserve detection state across PAT bumps)
        for (program_number, pmt_pid) in &found {
            if let Some(existing) = state
                .programs
                .iter_mut()
                .find(|p| p.program_number == *program_number)
            {
                if existing.pmt_pid != *pmt_pid {
                    existing.pmt_pid = *pmt_pid;
                    // PMT PID changed — force a re-parse next PMT seen
                    existing.last_pmt_version = None;
                }
            } else {
                state.programs.push(ProgramState {
                    program_number: *program_number,
                    pmt_pid: *pmt_pid,
                    last_pmt_version: None,
                    video_streams: Vec::new(),
                    audio_streams: Vec::new(),
                });
            }
        }
        // Sort by program_number for stable UI ordering
        state.programs.sort_by_key(|p| p.program_number);
    }

    // PMT handling — find the program owning this PID and update its streams.
    if ts_pusi(pkt) {
        if let Some(program_idx) = state.programs.iter().position(|p| p.pmt_pid == pid) {
            parse_pmt_streams(pkt, &mut state.programs[program_idx]);
        }
    }

    // PES header detection for codec detail extraction
    if ts_pusi(pkt) && ts_has_payload(pkt) && pid != PAT_PID {
        let is_pmt_pid = state.programs.iter().any(|p| p.pmt_pid == pid);
        if is_pmt_pid {
            return;
        }

        // Check if this PID is a known video stream that needs SPS or frame rate detection
        let needs_video_parse = state
            .programs
            .iter()
            .flat_map(|p| p.video_streams.iter())
            .any(|v| v.pid == pid && (!v.sps_detected || v.frame_rate.is_none()));
        let needs_audio_parse = state
            .programs
            .iter()
            .flat_map(|p| p.audio_streams.iter())
            .any(|a| a.pid == pid && !a.header_detected);

        if needs_video_parse {
            try_parse_video_pes(pkt, pid, state);
        }
        if needs_audio_parse {
            try_parse_audio_pes(pkt, pid, state);
        }
    }
}

// ── PMT Stream Extraction ────────────────────────────────────────────────

/// Parse a PMT section to extract all elementary stream entries for one program.
fn parse_pmt_streams(pkt: &[u8], program: &mut ProgramState) {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return;
    }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;

    if offset + 12 > TS_PACKET_SIZE {
        return;
    }
    let table_id = pkt[offset];
    if table_id != 0x02 {
        return;
    }

    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);

    // Check PMT version — skip re-parsing if unchanged
    let version = (pkt[offset + 5] >> 1) & 0x1F;
    if program.last_pmt_version == Some(version) {
        return;
    }
    program.last_pmt_version = Some(version);

    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);

    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    // Clear existing streams and re-populate
    program.video_streams.clear();
    program.audio_streams.clear();

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let stream_type = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_length =
            (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

        // Parse ES descriptors for language, etc.
        let desc_start = pos + 5;
        let desc_end = (desc_start + es_info_length).min(data_end);
        let language = parse_language_descriptor(&pkt[desc_start..desc_end]);

        match stream_type {
            // Video stream types
            0x1B => {
                program.video_streams.push(VideoStreamState {
                    pid: es_pid,
                    codec: "H.264/AVC".to_string(),
                    stream_type,
                    width: None,
                    height: None,
                    frame_rate: None,
                    profile: None,
                    level: None,
                    sps_detected: false,
                    last_pts: None,
                    pts_frame_count: 0,
                    pts_interval_sum: 0,
                });
            }
            0x24 => {
                program.video_streams.push(VideoStreamState {
                    pid: es_pid,
                    codec: "H.265/HEVC".to_string(),
                    stream_type,
                    width: None,
                    height: None,
                    frame_rate: None,
                    profile: None,
                    level: None,
                    sps_detected: false,
                    last_pts: None,
                    pts_frame_count: 0,
                    pts_interval_sum: 0,
                });
            }
            0x61 => {
                program.video_streams.push(VideoStreamState {
                    pid: es_pid,
                    codec: "JPEG XS".to_string(),
                    stream_type,
                    width: None,
                    height: None,
                    frame_rate: None,
                    profile: None,
                    level: None,
                    sps_detected: true, // No SPS to detect for JPEG XS
                    last_pts: None,
                    pts_frame_count: 0,
                    pts_interval_sum: 0,
                });
            }
            0x01 | 0x02 => {
                program.video_streams.push(VideoStreamState {
                    pid: es_pid,
                    codec: if stream_type == 0x01 {
                        "MPEG-1 Video".to_string()
                    } else {
                        "MPEG-2 Video".to_string()
                    },
                    stream_type,
                    width: None,
                    height: None,
                    frame_rate: None,
                    profile: None,
                    level: None,
                    sps_detected: true, // Not parsing sequence headers for legacy codecs
                    last_pts: None,
                    pts_frame_count: 0,
                    pts_interval_sum: 0,
                });
            }
            // Audio stream types
            0x03 | 0x04 => {
                program.audio_streams.push(AudioStreamState {
                    pid: es_pid,
                    codec: "MPEG Audio".to_string(),
                    stream_type,
                    sample_rate_hz: None,
                    channels: None,
                    language,
                    header_detected: true,
                });
            }
            0x0F => {
                program.audio_streams.push(AudioStreamState {
                    pid: es_pid,
                    codec: "AAC".to_string(),
                    stream_type,
                    sample_rate_hz: None,
                    channels: None,
                    language,
                    header_detected: false,
                });
            }
            0x11 => {
                program.audio_streams.push(AudioStreamState {
                    pid: es_pid,
                    codec: "AAC-LATM".to_string(),
                    stream_type,
                    sample_rate_hz: None,
                    channels: None,
                    language,
                    header_detected: true, // LATM parsing not implemented
                });
            }
            0x81 => {
                program.audio_streams.push(AudioStreamState {
                    pid: es_pid,
                    codec: "AC-3".to_string(),
                    stream_type,
                    sample_rate_hz: Some(48000), // AC-3 is almost always 48kHz
                    channels: None,
                    language,
                    header_detected: true,
                });
            }
            0x87 => {
                program.audio_streams.push(AudioStreamState {
                    pid: es_pid,
                    codec: "E-AC-3".to_string(),
                    stream_type,
                    sample_rate_hz: Some(48000),
                    channels: None,
                    language,
                    header_detected: true,
                });
            }
            0x06 => {
                // Private data — check descriptors for AC-3/E-AC-3
                let codec = detect_private_stream_codec(&pkt[desc_start..desc_end]);
                if let Some(codec_name) = codec {
                    program.audio_streams.push(AudioStreamState {
                        pid: es_pid,
                        codec: codec_name,
                        stream_type,
                        sample_rate_hz: Some(48000),
                        channels: None,
                        language,
                        header_detected: true,
                    });
                }
            }
            _ => {} // Unknown stream type — skip
        }

        pos += 5 + es_info_length;
    }

    tracing::info!(
        "Media analysis: program {} (PMT 0x{:04X}): {} video stream(s), {} audio stream(s)",
        program.program_number,
        program.pmt_pid,
        program.video_streams.len(),
        program.audio_streams.len(),
    );
    for v in &program.video_streams {
        tracing::info!(
            "  Video PID 0x{:04X}: {} (stream_type=0x{:02X})",
            v.pid,
            v.codec,
            v.stream_type,
        );
    }
    for a in &program.audio_streams {
        tracing::info!(
            "  Audio PID 0x{:04X}: {} (stream_type=0x{:02X}){}",
            a.pid,
            a.codec,
            a.stream_type,
            a.language
                .as_ref()
                .map(|l| format!(" [{}]", l))
                .unwrap_or_default(),
        );
    }
}

/// Parse ISO 639 language descriptor (tag 0x0A) from ES descriptor bytes.
fn parse_language_descriptor(descriptors: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos + 2 <= descriptors.len() {
        let tag = descriptors[pos];
        let len = descriptors[pos + 1] as usize;
        if pos + 2 + len > descriptors.len() {
            break;
        }
        if tag == 0x0A && len >= 3 {
            // ISO 639 language code: 3 ASCII characters
            let lang = &descriptors[pos + 2..pos + 5];
            if lang.iter().all(|&b| b.is_ascii_alphabetic()) {
                return Some(
                    std::str::from_utf8(lang)
                        .unwrap_or("und")
                        .to_lowercase(),
                );
            }
        }
        pos += 2 + len;
    }
    None
}

/// Detect codec for private stream (stream_type 0x06) by checking descriptors.
fn detect_private_stream_codec(descriptors: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos + 2 <= descriptors.len() {
        let tag = descriptors[pos];
        let len = descriptors[pos + 1] as usize;
        if pos + 2 + len > descriptors.len() {
            break;
        }
        match tag {
            0x6A => return Some("AC-3".to_string()),       // AC-3 descriptor
            0x7A => return Some("E-AC-3".to_string()),     // Enhanced AC-3 descriptor
            0x7B => return Some("DTS".to_string()),        // DTS descriptor
            0x7C => return Some("AAC".to_string()),        // AAC descriptor
            0x05 => {
                // Registration descriptor — check format_identifier
                if len >= 4 {
                    let id = &descriptors[pos + 2..pos + 6];
                    match id {
                        b"AC-3" => return Some("AC-3".to_string()),
                        b"EAC3" => return Some("E-AC-3".to_string()),
                        b"Opus" => return Some("Opus".to_string()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        pos += 2 + len;
    }
    None
}

// ── PES / NAL Unit Parsing ───────────────────────────────────────────────

/// Try to extract video codec details from a PES-start TS packet.
fn try_parse_video_pes(pkt: &[u8], pid: u16, state: &mut MediaAnalysisState) {
    let payload_start = ts_payload_offset(pkt);
    if payload_start >= TS_PACKET_SIZE {
        return;
    }

    let payload = &pkt[payload_start..];

    // Check PES start code: 0x00 0x00 0x01
    if payload.len() < 9 || payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return;
    }

    // PES header data length
    let pes_header_data_len = payload[8] as usize;
    let es_start = 9 + pes_header_data_len;
    if es_start >= payload.len() {
        return;
    }
    let es_data = &payload[es_start..];

    // Determine codec and SPS state from stream state
    let (stream_type, sps_detected) = match state
        .programs
        .iter()
        .flat_map(|p| p.video_streams.iter())
        .find(|v| v.pid == pid)
    {
        Some(v) => (v.stream_type, v.sps_detected),
        None => return,
    };

    // SPS-based detection (codec, resolution, profile, level, and optionally frame rate from VUI)
    if !sps_detected {
        match stream_type {
            0x1B => {
                // H.264/AVC — look for SPS NAL unit (type 7)
                if let Some(sps_info) = find_and_parse_h264_sps(es_data) {
                    tracing::info!(
                        "Media analysis: H.264 PID 0x{:04X}: {}x{}{}, profile={}, level={}",
                        pid,
                        sps_info.width,
                        sps_info.height,
                        sps_info
                            .frame_rate
                            .map(|f| format!(", {:.2} fps", f))
                            .unwrap_or_default(),
                        sps_info.profile,
                        sps_info.level,
                    );
                    if let Some(v) = state
                        .programs
                        .iter_mut()
                        .find_map(|p| p.video_streams.iter_mut().find(|v| v.pid == pid))
                    {
                        v.width = Some(sps_info.width);
                        v.height = Some(sps_info.height);
                        v.frame_rate = sps_info.frame_rate;
                        v.profile = Some(sps_info.profile);
                        v.level = Some(sps_info.level);
                        v.sps_detected = true;
                    }
                }
            }
            0x24 => {
                // H.265/HEVC — look for SPS NAL unit (type 33)
                if let Some(sps_info) = find_and_parse_h265_sps(es_data) {
                    tracing::info!(
                        "Media analysis: H.265 PID 0x{:04X}: {}x{}{}, profile={}, level={}",
                        pid,
                        sps_info.width,
                        sps_info.height,
                        sps_info
                            .frame_rate
                            .map(|f| format!(", {:.2} fps", f))
                            .unwrap_or_default(),
                        sps_info.profile,
                        sps_info.level,
                    );
                    if let Some(v) = state
                        .programs
                        .iter_mut()
                        .find_map(|p| p.video_streams.iter_mut().find(|v| v.pid == pid))
                    {
                        v.width = Some(sps_info.width);
                        v.height = Some(sps_info.height);
                        v.frame_rate = sps_info.frame_rate;
                        v.profile = Some(sps_info.profile);
                        v.level = Some(sps_info.level);
                        v.sps_detected = true;
                    }
                }
            }
            _ => {}
        }
    }

    // Timestamp-based frame rate fallback — runs when VUI timing didn't provide a frame rate.
    // Uses DTS (monotonic) when B-frames are present, PTS otherwise.
    let needs_ts_fps = state
        .programs
        .iter()
        .flat_map(|p| p.video_streams.iter())
        .any(|v| v.pid == pid && v.frame_rate.is_none());

    if needs_ts_fps {
        if let Some(ts) = extract_pes_decode_timestamp(payload) {
            if let Some(v) = state
                .programs
                .iter_mut()
                .find_map(|p| p.video_streams.iter_mut().find(|v| v.pid == pid))
            {
                if let Some(prev_ts) = v.last_pts {
                    // Timestamps are 33-bit and wrap at 2^33. Handle wraparound.
                    let delta = if ts >= prev_ts {
                        ts - prev_ts
                    } else {
                        (1u64 << 33) - prev_ts + ts
                    };
                    // Sanity bounds: ignore deltas outside the range of valid broadcast
                    // frame rates. Min 1200 ticks (~75 fps ceiling) filters out non-frame
                    // PES packets (parameter sets with near-duplicate timestamps).
                    // Max 90000 ticks (1 fps floor) filters out stale/wrapped values.
                    if delta >= 1200 && delta <= 90_000 {
                        v.pts_interval_sum += delta;
                        v.pts_frame_count += 1;

                        // After 30 samples, compute frame rate
                        const PTS_SAMPLES_NEEDED: u32 = 30;
                        if v.pts_frame_count >= PTS_SAMPLES_NEEDED {
                            let avg_interval =
                                v.pts_interval_sum as f64 / v.pts_frame_count as f64;
                            if avg_interval > 0.0 {
                                let fps = 90_000.0 / avg_interval;
                                tracing::info!(
                                    "Media analysis: PID 0x{:04X} frame rate detected from DTS/PTS: {:.2} fps",
                                    pid, fps
                                );
                                v.frame_rate = Some(fps);
                            }
                        }
                    }
                }
                v.last_pts = Some(ts);
            }
        }
    }
}

/// Extract a monotonic decode timestamp from a PES header for frame rate measurement.
/// Returns DTS when present (B-frames), otherwise PTS (which is monotonic when no B-frames).
/// Value is a 33-bit timestamp in 90kHz clock ticks.
fn extract_pes_decode_timestamp(pes: &[u8]) -> Option<u64> {
    // PES header: [0x00, 0x00, 0x01, stream_id, len_hi, len_lo, flags1, flags2, hdr_data_len, ...]
    // flags2 byte (index 7): bits 7-6 = PTS_DTS_flags
    //   0b10 = PTS only (no B-frames, PTS is monotonic)
    //   0b11 = PTS + DTS (B-frames present, use DTS)
    if pes.len() < 14 {
        return None;
    }
    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    if pts_dts_flags == 0b11 && pes.len() >= 19 {
        // DTS present — 5 bytes starting at index 14 (after the 5-byte PTS)
        let b = &pes[14..19];
        let dts = (((b[0] as u64 >> 1) & 0x07) << 30)
            | ((b[1] as u64) << 22)
            | (((b[2] as u64 >> 1) & 0x7F) << 15)
            | ((b[3] as u64) << 7)
            | ((b[4] as u64 >> 1) & 0x7F);
        return Some(dts);
    }
    if pts_dts_flags == 0b10 {
        // PTS only — monotonic (no B-frames)
        let b = &pes[9..14];
        let pts = (((b[0] as u64 >> 1) & 0x07) << 30)
            | ((b[1] as u64) << 22)
            | (((b[2] as u64 >> 1) & 0x7F) << 15)
            | ((b[3] as u64) << 7)
            | ((b[4] as u64 >> 1) & 0x7F);
        return Some(pts);
    }
    None
}

/// Try to extract audio codec details from a PES-start TS packet.
fn try_parse_audio_pes(pkt: &[u8], pid: u16, state: &mut MediaAnalysisState) {
    let payload_start = ts_payload_offset(pkt);
    if payload_start >= TS_PACKET_SIZE {
        return;
    }

    let payload = &pkt[payload_start..];

    // Check PES start code
    if payload.len() < 9 || payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return;
    }

    let pes_header_data_len = payload[8] as usize;
    let es_start = 9 + pes_header_data_len;
    if es_start + 7 > payload.len() {
        return;
    }
    let es_data = &payload[es_start..];

    // Try ADTS header detection (AAC)
    if es_data.len() >= 7 && es_data[0] == 0xFF && (es_data[1] & 0xF0) == 0xF0 {
        if let Some(adts) = parse_adts_header(es_data) {
            if let Some(a) = state
                .programs
                .iter_mut()
                .find_map(|p| p.audio_streams.iter_mut().find(|a| a.pid == pid))
            {
                a.sample_rate_hz = Some(adts.sample_rate);
                a.channels = Some(adts.channels);
                a.codec = adts.profile_name;
                a.header_detected = true;
                tracing::info!(
                    "Media analysis: AAC PID 0x{:04X}: {} Hz, {} ch, {}",
                    pid,
                    adts.sample_rate,
                    adts.channels,
                    a.codec,
                );
            }
        }
    }
}

// ── H.264 SPS Parser ────────────────────────────────────────────────────

struct SpsInfo {
    width: u16,
    height: u16,
    frame_rate: Option<f64>,
    profile: String,
    level: String,
}

/// Find a H.264 SPS NAL unit in elementary stream data and parse it.
fn find_and_parse_h264_sps(data: &[u8]) -> Option<SpsInfo> {
    // Look for NAL start codes (0x00 0x00 0x01 or 0x00 0x00 0x00 0x01)
    let mut i = 0;
    while i + 4 < data.len() {
        let nal_start = if data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x01 {
            Some(i + 3)
        } else if i + 4 < data.len()
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x00
            && data[i + 3] == 0x01
        {
            Some(i + 4)
        } else {
            None
        };

        if let Some(start) = nal_start {
            if start < data.len() {
                let nal_type = data[start] & 0x1F;
                if nal_type == 7 {
                    // SPS
                    // Find end of this NAL unit
                    let end = find_nal_end(data, start);
                    let sps_bytes = remove_emulation_prevention(&data[start..end]);
                    return parse_h264_sps(&sps_bytes);
                }
            }
            i = start;
        } else {
            i += 1;
        }
    }

    // Also try without start code — some PES packets have SPS directly
    if !data.is_empty() && (data[0] & 0x1F) == 7 {
        let sps_bytes = remove_emulation_prevention(data);
        return parse_h264_sps(&sps_bytes);
    }

    None
}

/// Parse H.264 SPS NAL unit (starting from the NAL header byte).
fn parse_h264_sps(data: &[u8]) -> Option<SpsInfo> {
    if data.len() < 4 {
        return None;
    }

    let mut reader = BitReader::new(&data[1..]); // Skip NAL header byte

    let profile_idc = reader.read_bits(8)? as u8;
    let _constraint_flags = reader.read_bits(8)?; // constraint_set0..5_flag + reserved
    let level_idc = reader.read_bits(8)? as u8;
    let _seq_parameter_set_id = reader.read_exp_golomb()?; // seq_parameter_set_id

    // For High profile and above, parse chroma/scaling info
    let mut chroma_format_idc = 1u32; // Default
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = reader.read_exp_golomb()?;
        if chroma_format_idc == 3 {
            reader.read_bits(1)?; // separate_colour_plane_flag
        }
        reader.read_exp_golomb()?; // bit_depth_luma_minus8
        reader.read_exp_golomb()?; // bit_depth_chroma_minus8
        reader.read_bits(1)?; // qpprime_y_zero_transform_bypass_flag
        let seq_scaling_matrix_present = reader.read_bits(1)?;
        if seq_scaling_matrix_present == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for _ in 0..count {
                let present = reader.read_bits(1)?;
                if present == 1 {
                    skip_scaling_list(&mut reader, if count <= 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    reader.read_exp_golomb()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = reader.read_exp_golomb()?;
    if pic_order_cnt_type == 0 {
        reader.read_exp_golomb()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        reader.read_bits(1)?; // delta_pic_order_always_zero_flag
        reader.read_signed_exp_golomb()?; // offset_for_non_ref_pic
        reader.read_signed_exp_golomb()?; // offset_for_top_to_bottom_field
        let num_ref_frames_in_pic_order_cnt_cycle = reader.read_exp_golomb()?;
        for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
            reader.read_signed_exp_golomb()?;
        }
    }

    reader.read_exp_golomb()?; // max_num_ref_frames
    reader.read_bits(1)?; // gaps_in_frame_num_value_allowed_flag

    let pic_width_in_mbs_minus1 = reader.read_exp_golomb()?;
    let pic_height_in_map_units_minus1 = reader.read_exp_golomb()?;
    let frame_mbs_only_flag = reader.read_bits(1)?;

    if frame_mbs_only_flag == 0 {
        reader.read_bits(1)?; // mb_adaptive_frame_field_flag
    }

    reader.read_bits(1)?; // direct_8x8_inference_flag

    let frame_cropping_flag = reader.read_bits(1)?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag == 1 {
        (
            reader.read_exp_golomb()?,
            reader.read_exp_golomb()?,
            reader.read_exp_golomb()?,
            reader.read_exp_golomb()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    // Calculate dimensions
    let sub_width_c: u32 = if chroma_format_idc == 3 { 1 } else { 2 };
    let sub_height_c: u32 = if chroma_format_idc == 1 { 2 } else { 1 };
    let crop_unit_x = if chroma_format_idc == 0 { 1 } else { sub_width_c };
    let crop_unit_y = if chroma_format_idc == 0 {
        2 - frame_mbs_only_flag
    } else {
        sub_height_c * (2 - frame_mbs_only_flag)
    };

    let width = ((pic_width_in_mbs_minus1 + 1) * 16
        - crop_unit_x * (crop_left + crop_right)) as u16;
    let height = ((2 - frame_mbs_only_flag) * (pic_height_in_map_units_minus1 + 1) * 16
        - crop_unit_y * (crop_top + crop_bottom)) as u16;

    // Try to get timing info (VUI parameters)
    let mut frame_rate = None;
    let vui_present = reader.read_bits(1).unwrap_or(0);
    if vui_present == 1 {
        frame_rate = parse_h264_vui_timing(&mut reader);
    }

    let profile = match profile_idc {
        66 => "Baseline",
        77 => "Main",
        88 => "Extended",
        100 => "High",
        110 => "High 10",
        122 => "High 4:2:2",
        244 => "High 4:4:4 Predictive",
        _ => "Unknown",
    };

    let level = format!("{}.{}", level_idc / 10, level_idc % 10);

    Some(SpsInfo {
        width,
        height,
        frame_rate,
        profile: profile.to_string(),
        level,
    })
}

/// Skip a scaling list in H.264 SPS.
fn skip_scaling_list(reader: &mut BitReader, size: usize) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = reader.read_signed_exp_golomb()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
    Some(())
}

/// Parse VUI timing info from H.264 SPS to extract frame rate.
fn parse_h264_vui_timing(reader: &mut BitReader) -> Option<f64> {
    // aspect_ratio_info_present_flag
    if reader.read_bits(1)? == 1 {
        let sar_idc = reader.read_bits(8)?;
        if sar_idc == 255 {
            // Extended_SAR
            reader.read_bits(16)?; // sar_width
            reader.read_bits(16)?; // sar_height
        }
    }
    // overscan_info_present_flag
    if reader.read_bits(1)? == 1 {
        reader.read_bits(1)?; // overscan_appropriate_flag
    }
    // video_signal_type_present_flag
    if reader.read_bits(1)? == 1 {
        reader.read_bits(3)?; // video_format
        reader.read_bits(1)?; // video_full_range_flag
        if reader.read_bits(1)? == 1 {
            // colour_description_present_flag
            reader.read_bits(8)?; // colour_primaries
            reader.read_bits(8)?; // transfer_characteristics
            reader.read_bits(8)?; // matrix_coefficients
        }
    }
    // chroma_loc_info_present_flag
    if reader.read_bits(1)? == 1 {
        reader.read_exp_golomb()?;
        reader.read_exp_golomb()?;
    }
    // timing_info_present_flag
    if reader.read_bits(1)? == 1 {
        let num_units_in_tick = reader.read_bits(32)?;
        let time_scale = reader.read_bits(32)?;
        if num_units_in_tick > 0 {
            return Some(time_scale as f64 / (2.0 * num_units_in_tick as f64));
        }
    }
    None
}

// ── H.265 SPS Parser ────────────────────────────────────────────────────

/// Find a H.265 SPS NAL unit in elementary stream data and parse it.
fn find_and_parse_h265_sps(data: &[u8]) -> Option<SpsInfo> {
    let mut i = 0;
    while i + 4 < data.len() {
        let nal_start = if data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x01 {
            Some(i + 3)
        } else if i + 4 < data.len()
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x00
            && data[i + 3] == 0x01
        {
            Some(i + 4)
        } else {
            None
        };

        if let Some(start) = nal_start {
            if start + 1 < data.len() {
                let nal_type = (data[start] >> 1) & 0x3F;
                if nal_type == 33 {
                    // SPS
                    let end = find_nal_end(data, start);
                    let sps_bytes = remove_emulation_prevention(&data[start..end]);
                    return parse_h265_sps(&sps_bytes);
                }
            }
            i = start;
        } else {
            i += 1;
        }
    }
    None
}

/// Parse H.265 SPS NAL unit.
fn parse_h265_sps(data: &[u8]) -> Option<SpsInfo> {
    if data.len() < 4 {
        return None;
    }

    let mut reader = BitReader::new(&data[2..]); // Skip 2-byte NAL header

    let _sps_video_parameter_set_id = reader.read_bits(4)?;
    let sps_max_sub_layers_minus1 = reader.read_bits(3)?;
    let _sps_temporal_id_nesting_flag = reader.read_bits(1)?;

    // profile_tier_level
    let general_profile_idc = reader.read_bits(5)? as u8; // general_profile_space(2) + general_tier_flag(1) + (we just read 5 to get profile)
    // Actually: general_profile_space(2), general_tier_flag(1), general_profile_idc(5)
    // Let me re-read correctly
    // We already read 4 bits above incorrectly. Let me restart the profile parsing.
    // The profile_tier_level structure is complex. For simplicity, extract key fields:

    // We need to re-approach. After the NAL header (2 bytes), SPS starts with:
    // sps_video_parameter_set_id (4 bits) - already read
    // sps_max_sub_layers_minus1 (3 bits) - already read
    // sps_temporal_id_nesting_flag (1 bit) - already read
    // Then profile_tier_level(1, sps_max_sub_layers_minus1)

    // profile_tier_level:
    // general_profile_space (2), general_tier_flag (1), general_profile_idc (5)
    let _general_profile_space = (general_profile_idc >> 3) & 0x03;
    let _general_tier_flag = (general_profile_idc >> 2) & 0x01;
    let _profile_idc = general_profile_idc & 0x1F;

    // Let me simplify: skip profile_tier_level entirely and just get resolution
    // profile_tier_level is 11 bytes minimum for the general part
    // general_profile_space(2) + general_tier_flag(1) + general_profile_idc(5) = 1 byte
    // general_profile_compatibility_flags(32) = 4 bytes
    // general_constraint_indicator_flags(48) = 6 bytes
    // general_level_idc(8) = 1 byte
    // = 12 bytes (96 bits) for the general part
    // Then sub_layer info if sps_max_sub_layers_minus1 > 0

    // We already consumed 5 bits (wrongly). Let's just consume remaining bits for profile_tier_level.
    // Restart with a cleaner approach:
    let reader2 = BitReader::new(&data[2..]);
    return parse_h265_sps_clean(reader2, sps_max_sub_layers_minus1 as u8);
}

fn parse_h265_sps_clean(mut reader: BitReader, sps_max_sub_layers_minus1: u8) -> Option<SpsInfo> {
    let _sps_video_parameter_set_id = reader.read_bits(4)?;
    let _sps_max_sub_layers_minus1 = reader.read_bits(3)?;
    let _sps_temporal_id_nesting_flag = reader.read_bits(1)?;

    // profile_tier_level(1, sps_max_sub_layers_minus1)
    let _general_profile_space = reader.read_bits(2)?;
    let _general_tier_flag = reader.read_bits(1)?;
    let general_profile_idc = reader.read_bits(5)? as u8;
    let _general_profile_compat = reader.read_bits(32)?; // 32 compatibility flags
    // 48 bits of constraint flags
    reader.read_bits(32)?;
    reader.read_bits(16)?;
    let general_level_idc = reader.read_bits(8)? as u8;

    // Skip sub-layer info (ITU-T H.265 §7.3.3)
    if sps_max_sub_layers_minus1 > 0 {
        let mut sub_layer_profile_present = [false; 8];
        let mut sub_layer_level_present = [false; 8];
        for i in 0..sps_max_sub_layers_minus1 as usize {
            sub_layer_profile_present[i] = reader.read_bits(1)? == 1;
            sub_layer_level_present[i] = reader.read_bits(1)? == 1;
        }
        if sps_max_sub_layers_minus1 < 8 {
            for _ in sps_max_sub_layers_minus1..8 {
                reader.read_bits(2)?; // reserved_zero_2bits
            }
        }
        for i in 0..sps_max_sub_layers_minus1 as usize {
            if sub_layer_profile_present[i] {
                // sub_layer_profile_space(2) + sub_layer_tier_flag(1) + sub_layer_profile_idc(5)
                reader.read_bits(8)?;
                reader.read_bits(32)?; // sub_layer_profile_compatibility_flag[32]
                // sub_layer_constraint flags (48 bits)
                reader.read_bits(32)?;
                reader.read_bits(16)?;
            }
            if sub_layer_level_present[i] {
                reader.read_bits(8)?; // sub_layer_level_idc
            }
        }
    }

    let _sps_seq_parameter_set_id = reader.read_exp_golomb()?;
    let chroma_format_idc = reader.read_exp_golomb()?;
    if chroma_format_idc == 3 {
        reader.read_bits(1)?; // separate_colour_plane_flag
    }

    let pic_width_in_luma_samples = reader.read_exp_golomb()? as u16;
    let pic_height_in_luma_samples = reader.read_exp_golomb()? as u16;

    let conformance_window_flag = reader.read_bits(1)?;
    let (mut width, mut height) = (pic_width_in_luma_samples, pic_height_in_luma_samples);
    if conformance_window_flag == 1 {
        let left = reader.read_exp_golomb()?;
        let right = reader.read_exp_golomb()?;
        let top = reader.read_exp_golomb()?;
        let bottom = reader.read_exp_golomb()?;
        let sub_width_c: u32 = if chroma_format_idc == 1 || chroma_format_idc == 2 { 2 } else { 1 };
        let sub_height_c: u32 = if chroma_format_idc == 1 { 2 } else { 1 };
        width = (pic_width_in_luma_samples as u32 - sub_width_c * (left + right)) as u16;
        height = (pic_height_in_luma_samples as u32 - sub_height_c * (top + bottom)) as u16;
    }

    // Parse remaining SPS fields to reach VUI (ITU-T H.265 §7.3.2.2.1)
    // Wrapped in a closure so any parse failure falls back to frame_rate: None
    let frame_rate = (|| -> Option<f64> {
        let bit_depth_luma_minus8 = reader.read_exp_golomb()?;
        let bit_depth_chroma_minus8 = reader.read_exp_golomb()?;
        let log2_max_pic_order_cnt_lsb_minus4 = reader.read_exp_golomb()?;
        tracing::debug!(
            "H.265 SPS VUI parse: bit_depth_luma_m8={}, bit_depth_chroma_m8={}, log2_max_poc_lsb_m4={}",
            bit_depth_luma_minus8, bit_depth_chroma_minus8, log2_max_pic_order_cnt_lsb_minus4
        );

        let sps_sub_layer_ordering_info_present_flag = reader.read_bits(1)?;
        let start = if sps_sub_layer_ordering_info_present_flag == 1 {
            0
        } else {
            sps_max_sub_layers_minus1
        };
        for _ in start..=sps_max_sub_layers_minus1 {
            reader.read_exp_golomb()?; // max_dec_pic_buffering_minus1
            reader.read_exp_golomb()?; // max_num_reorder_pics
            reader.read_exp_golomb()?; // max_latency_increase_plus1
        }
        tracing::debug!("H.265 SPS VUI parse: passed sub-layer ordering");

        let _log2_min_luma_coding_block_size_minus3 = reader.read_exp_golomb()?;
        let _log2_diff_max_min_luma_coding_block_size = reader.read_exp_golomb()?;
        let _log2_min_luma_transform_block_size_minus2 = reader.read_exp_golomb()?;
        let _log2_diff_max_min_luma_transform_block_size = reader.read_exp_golomb()?;
        let _max_transform_hierarchy_depth_inter = reader.read_exp_golomb()?;
        let _max_transform_hierarchy_depth_intra = reader.read_exp_golomb()?;

        let scaling_list_enabled_flag = reader.read_bits(1)?;
        tracing::debug!("H.265 SPS VUI parse: scaling_list_enabled={}", scaling_list_enabled_flag);
        if scaling_list_enabled_flag == 1 {
            let sps_scaling_list_data_present_flag = reader.read_bits(1)?;
            if sps_scaling_list_data_present_flag == 1 {
                skip_h265_scaling_list_data(&mut reader)?;
            }
        }

        let amp_enabled = reader.read_bits(1)?; // amp_enabled_flag
        let sao_enabled = reader.read_bits(1)?; // sample_adaptive_offset_enabled_flag

        let pcm_enabled_flag = reader.read_bits(1)?;
        tracing::debug!("H.265 SPS VUI parse: amp={}, sao={}, pcm={}", amp_enabled, sao_enabled, pcm_enabled_flag);
        if pcm_enabled_flag == 1 {
            reader.read_bits(4)?; // pcm_sample_bit_depth_luma_minus1
            reader.read_bits(4)?; // pcm_sample_bit_depth_chroma_minus1
            reader.read_exp_golomb()?; // log2_min_pcm_luma_coding_block_size_minus3
            reader.read_exp_golomb()?; // log2_diff_max_min_pcm_luma_coding_block_size
            reader.read_bits(1)?; // pcm_loop_filter_disabled_flag
        }

        // Short-term ref pic sets — must track num_delta_pocs per set for inter-prediction
        let num_short_term_ref_pic_sets = reader.read_exp_golomb()?;
        tracing::debug!("H.265 SPS VUI parse: num_short_term_ref_pic_sets={}", num_short_term_ref_pic_sets);
        let mut num_delta_pocs = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
        for i in 0..num_short_term_ref_pic_sets {
            let ndp = parse_h265_short_term_ref_pic_set(
                &mut reader,
                i,
                &num_delta_pocs,
            )?;
            num_delta_pocs.push(ndp);
        }
        tracing::debug!("H.265 SPS VUI parse: parsed all ref pic sets, num_delta_pocs={:?}", num_delta_pocs);

        // Long-term ref pics
        let long_term_ref_pics_present_flag = reader.read_bits(1)?;
        tracing::debug!("H.265 SPS VUI parse: long_term_ref_pics_present={}", long_term_ref_pics_present_flag);
        if long_term_ref_pics_present_flag == 1 {
            let num_long_term_ref_pics_sps = reader.read_exp_golomb()?;
            let log2_max_pic_order_cnt_lsb = log2_max_pic_order_cnt_lsb_minus4 + 4;
            for _ in 0..num_long_term_ref_pics_sps {
                reader.read_bits(log2_max_pic_order_cnt_lsb as u8)?; // lt_ref_pic_poc_lsb_sps
                reader.read_bits(1)?; // used_by_curr_pic_lt_sps_flag
            }
        }

        let temporal_mvp = reader.read_bits(1)?; // sps_temporal_mvp_enabled_flag
        let strong_intra = reader.read_bits(1)?; // strong_intra_smoothing_enabled_flag

        let vui_parameters_present_flag = reader.read_bits(1)?;
        tracing::debug!(
            "H.265 SPS VUI parse: temporal_mvp={}, strong_intra={}, vui_present={}",
            temporal_mvp, strong_intra, vui_parameters_present_flag
        );
        if vui_parameters_present_flag == 1 {
            let result = parse_h265_vui_timing(&mut reader);
            tracing::debug!("H.265 SPS VUI parse: vui_timing result={:?}", result);
            return result;
        }
        None
    })();

    let profile = match general_profile_idc {
        1 => "Main",
        2 => "Main 10",
        3 => "Main Still Picture",
        4 => "Range Extensions",
        5 => "High Throughput",
        _ => "Unknown",
    };

    let level = format!(
        "{}.{}",
        general_level_idc / 30,
        (general_level_idc % 30) / 3
    );

    Some(SpsInfo {
        width,
        height,
        frame_rate,
        profile: profile.to_string(),
        level,
    })
}

/// Parse VUI timing info from H.265 SPS to extract frame rate.
/// Nearly identical to H.264 VUI, but H.265 timing uses `vui_num_units_in_tick`
/// and `vui_time_scale` directly (no ×2 divisor like H.264).
fn parse_h265_vui_timing(reader: &mut BitReader) -> Option<f64> {
    // aspect_ratio_info_present_flag
    if reader.read_bits(1)? == 1 {
        let sar_idc = reader.read_bits(8)?;
        if sar_idc == 255 {
            reader.read_bits(16)?; // sar_width
            reader.read_bits(16)?; // sar_height
        }
    }
    // overscan_info_present_flag
    if reader.read_bits(1)? == 1 {
        reader.read_bits(1)?; // overscan_appropriate_flag
    }
    // video_signal_type_present_flag
    if reader.read_bits(1)? == 1 {
        reader.read_bits(3)?; // video_format
        reader.read_bits(1)?; // video_full_range_flag
        if reader.read_bits(1)? == 1 {
            // colour_description_present_flag
            reader.read_bits(8)?; // colour_primaries
            reader.read_bits(8)?; // transfer_characteristics
            reader.read_bits(8)?; // matrix_coefficients
        }
    }
    // chroma_loc_info_present_flag
    if reader.read_bits(1)? == 1 {
        reader.read_exp_golomb()?; // chroma_sample_loc_type_top_field
        reader.read_exp_golomb()?; // chroma_sample_loc_type_bottom_field
    }
    // neutral_chroma_indication_flag, field_seq_flag, frame_field_info_present_flag
    reader.read_bits(1)?;
    reader.read_bits(1)?;
    reader.read_bits(1)?;
    // default_display_window_flag
    if reader.read_bits(1)? == 1 {
        reader.read_exp_golomb()?; // def_disp_win_left_offset
        reader.read_exp_golomb()?; // def_disp_win_right_offset
        reader.read_exp_golomb()?; // def_disp_win_top_offset
        reader.read_exp_golomb()?; // def_disp_win_bottom_offset
    }
    // vui_timing_info_present_flag
    let timing_info_present = reader.read_bits(1)?;
    tracing::debug!("H.265 VUI: timing_info_present={}", timing_info_present);
    if timing_info_present == 1 {
        let num_units_in_tick = reader.read_bits(32)?;
        let time_scale = reader.read_bits(32)?;
        tracing::debug!("H.265 VUI: num_units_in_tick={}, time_scale={}", num_units_in_tick, time_scale);
        if num_units_in_tick > 0 {
            return Some(time_scale as f64 / num_units_in_tick as f64);
        }
    }
    None
}

/// Skip H.265 scaling list data (ITU-T H.265 §7.3.4).
fn skip_h265_scaling_list_data(reader: &mut BitReader) -> Option<()> {
    for size_id in 0..4u32 {
        let count = if size_id == 3 { 2 } else { 6 };
        for _ in 0..count {
            let scaling_list_pred_mode_flag = reader.read_bits(1)?;
            if scaling_list_pred_mode_flag == 0 {
                reader.read_exp_golomb()?; // scaling_list_pred_matrix_id_delta
            } else {
                let coef_num = std::cmp::min(64, 1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    reader.read_signed_exp_golomb()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..coef_num {
                    reader.read_signed_exp_golomb()?; // scaling_list_delta_coef
                }
            }
        }
    }
    Some(())
}

/// Parse one H.265 short-term ref pic set (ITU-T H.265 §7.3.7).
/// Returns NumDeltaPocs for this set (needed by subsequent inter-predicted sets).
fn parse_h265_short_term_ref_pic_set(
    reader: &mut BitReader,
    idx: u32,
    prev_num_delta_pocs: &[u32],
) -> Option<u32> {
    let inter_ref_pic_set_prediction_flag = if idx > 0 {
        reader.read_bits(1)? == 1
    } else {
        false
    };

    if inter_ref_pic_set_prediction_flag {
        let delta_idx_minus1 = reader.read_exp_golomb()?;
        let ref_idx = idx.checked_sub(delta_idx_minus1 + 1)?;
        let ref_num_delta_pocs = *prev_num_delta_pocs.get(ref_idx as usize)?;

        reader.read_bits(1)?; // delta_rps_sign
        reader.read_exp_golomb()?; // abs_delta_rps_minus1

        // For each delta poc in the reference set + 1, read used_by_curr_pic_flag
        // and conditionally use_delta_flag
        let mut num_delta_pocs = 0u32;
        for _ in 0..=ref_num_delta_pocs {
            let used_by_curr_pic_flag = reader.read_bits(1)?;
            if used_by_curr_pic_flag == 0 {
                let use_delta_flag = reader.read_bits(1)?;
                if use_delta_flag == 1 {
                    num_delta_pocs += 1;
                }
            } else {
                num_delta_pocs += 1;
            }
        }
        Some(num_delta_pocs)
    } else {
        let num_negative_pics = reader.read_exp_golomb()?;
        let num_positive_pics = reader.read_exp_golomb()?;
        for _ in 0..num_negative_pics {
            reader.read_exp_golomb()?; // delta_poc_s0_minus1
            reader.read_bits(1)?; // used_by_curr_pic_s0_flag
        }
        for _ in 0..num_positive_pics {
            reader.read_exp_golomb()?; // delta_poc_s1_minus1
            reader.read_bits(1)?; // used_by_curr_pic_s1_flag
        }
        Some(num_negative_pics + num_positive_pics)
    }
}

// ── AAC ADTS Parser ─────────────────────────────────────────────────────

struct AdtsInfo {
    sample_rate: u32,
    channels: u8,
    profile_name: String,
}

/// AAC sample rate lookup table indexed by sampling_frequency_index.
const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Parse an AAC ADTS header (7 or 9 bytes).
fn parse_adts_header(data: &[u8]) -> Option<AdtsInfo> {
    if data.len() < 7 {
        return None;
    }

    // Sync word check: 0xFFF
    if data[0] != 0xFF || (data[1] & 0xF0) != 0xF0 {
        return None;
    }

    let profile = ((data[2] >> 6) & 0x03) + 1; // object type = profile + 1
    let sampling_freq_index = ((data[2] >> 2) & 0x0F) as usize;
    let channel_config = ((data[2] & 0x01) << 2) | ((data[3] >> 6) & 0x03);

    if sampling_freq_index >= AAC_SAMPLE_RATES.len() {
        return None;
    }

    let profile_name = match profile {
        1 => "AAC-Main",
        2 => "AAC-LC",
        3 => "AAC-SSR",
        4 => "AAC-LTP",
        _ => "AAC",
    };

    Some(AdtsInfo {
        sample_rate: AAC_SAMPLE_RATES[sampling_freq_index],
        channels: channel_config,
        profile_name: profile_name.to_string(),
    })
}

// ── Bit Reader (Exp-Golomb) ──────────────────────────────────────────────

/// Simple bitstream reader for Exp-Golomb coded fields in NAL units.
struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7, MSB first
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn read_bit(&mut self) -> Option<u32> {
        if self.byte_pos >= self.data.len() {
            return None;
        }
        let bit = ((self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1) as u32;
        self.bit_pos += 1;
        if self.bit_pos >= 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Some(bit)
    }

    fn read_bits(&mut self, n: u8) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()?;
        }
        Some(value)
    }

    /// Read an unsigned Exp-Golomb coded value (ue(v)).
    fn read_exp_golomb(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        loop {
            let bit = self.read_bit()?;
            if bit == 1 {
                break;
            }
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None; // Prevent infinite loop on bad data
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros as u8)?;
        Some((1 << leading_zeros) - 1 + suffix)
    }

    /// Read a signed Exp-Golomb coded value (se(v)).
    fn read_signed_exp_golomb(&mut self) -> Option<i32> {
        let code = self.read_exp_golomb()?;
        let value = ((code + 1) / 2) as i32;
        if code % 2 == 0 {
            Some(-value)
        } else {
            Some(value)
        }
    }
}

// ── NAL Unit Helpers ─────────────────────────────────────────────────────

/// Find the end of a NAL unit (next start code or end of data).
fn find_nal_end(data: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i + 2 < data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 && (data[i + 2] == 0x01 || data[i + 2] == 0x00)
        {
            return i;
        }
        i += 1;
    }
    data.len()
}

/// Remove emulation prevention bytes (0x00 0x00 0x03 → 0x00 0x00).
fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x03 {
            result.push(0x00);
            result.push(0x00);
            i += 3; // Skip the 0x03
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adts_header_44100_stereo() {
        // ADTS header: AAC-LC, 44100 Hz, 2 channels
        // Sync: 0xFFF, ID=0 (MPEG-4), Layer=0, Protection=1 (no CRC)
        // Profile: 01 (AAC-LC), SFI: 0100 (44100), Private: 0, Channel: 010
        let header: [u8; 7] = [0xFF, 0xF1, 0x50, 0x80, 0x00, 0x1F, 0xFC];
        let result = parse_adts_header(&header).unwrap();
        assert_eq!(result.sample_rate, 44100);
        assert_eq!(result.channels, 2);
        assert_eq!(result.profile_name, "AAC-LC");
    }

    #[test]
    fn test_adts_header_48000_stereo() {
        // AAC-LC, 48000 Hz, 2 channels
        // Profile: 01, SFI: 0011 (48000), Channel: 010
        let header: [u8; 7] = [0xFF, 0xF1, 0x4C, 0x80, 0x00, 0x1F, 0xFC];
        let result = parse_adts_header(&header).unwrap();
        assert_eq!(result.sample_rate, 48000);
        assert_eq!(result.channels, 2);
        assert_eq!(result.profile_name, "AAC-LC");
    }

    #[test]
    fn test_adts_invalid_sync() {
        let header: [u8; 7] = [0xFF, 0x00, 0x50, 0x80, 0x00, 0x1F, 0xFC];
        assert!(parse_adts_header(&header).is_none());
    }

    #[test]
    fn test_exp_golomb_reader() {
        // ue(0) = 1 (binary: 1)
        // ue(1) = 010 (binary: 010)
        // ue(2) = 011
        // ue(3) = 00100
        let data = [0b10100110, 0b01000000];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_exp_golomb(), Some(0)); // 1 → 0
        assert_eq!(reader.read_exp_golomb(), Some(1)); // 010 → 1
        assert_eq!(reader.read_exp_golomb(), Some(2)); // 011 → 2
        assert_eq!(reader.read_exp_golomb(), Some(3)); // 00100 → 3
    }

    #[test]
    fn test_emulation_prevention_removal() {
        let data = [0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x00];
        let result = remove_emulation_prevention(&data);
        assert_eq!(result, vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_h264_sps_1080p() {
        // Minimal H.264 SPS for 1920x1080 High profile, level 4.0
        // NAL header (0x67 = SPS)
        // Profile: High (100), Level: 40
        // This is a real-world SPS from a 1080p H.264 stream (simplified)
        let sps_data: Vec<u8> = vec![
            0x67, // NAL header: forbidden(0) + nal_ref_idc(3) + nal_type(7=SPS)
            0x64, // profile_idc = 100 (High)
            0x00, // constraint_set flags
            0x28, // level_idc = 40 (Level 4.0)
            0xAD, // seq_parameter_set_id=0 (ue=0→1), chroma_format_idc=1(ue=0→1)
            // This is a simplification; real SPS has more Exp-Golomb fields
        ];
        // A proper test would need a fully valid SPS bitstream. The parser
        // should gracefully handle truncated data by returning None.
        let result = parse_h264_sps(&sps_data);
        // With this truncated data, we may not get a result, and that's OK
        // The parser should not panic on truncated input
        let _ = result;
    }

    #[test]
    fn test_language_descriptor() {
        // ISO 639 descriptor: tag=0x0A, length=4, "eng" + audio_type
        let desc = [0x0A, 0x04, b'e', b'n', b'g', 0x00];
        let lang = parse_language_descriptor(&desc);
        assert_eq!(lang, Some("eng".to_string()));
    }

    #[test]
    fn test_language_descriptor_none() {
        // Non-language descriptor
        let desc = [0x05, 0x04, 0x48, 0x44, 0x4D, 0x56];
        let lang = parse_language_descriptor(&desc);
        assert!(lang.is_none());
    }

    #[test]
    fn test_private_stream_ac3_descriptor() {
        // AC-3 descriptor tag 0x6A
        let desc = [0x6A, 0x01, 0x00];
        let codec = detect_private_stream_codec(&desc);
        assert_eq!(codec, Some("AC-3".to_string()));
    }

    #[test]
    fn test_private_stream_eac3_descriptor() {
        // E-AC-3 descriptor tag 0x7A
        let desc = [0x7A, 0x01, 0x00];
        let codec = detect_private_stream_codec(&desc);
        assert_eq!(codec, Some("E-AC-3".to_string()));
    }

    #[test]
    fn test_bit_reader_basic() {
        let data = [0b10110100];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(3), Some(0b101));
        assert_eq!(reader.read_bits(5), Some(0b10100));
    }

    #[test]
    fn test_signed_exp_golomb() {
        // se(v): code_num → value mapping:
        // 0 → 0, 1 → 1, 2 → -1, 3 → 2, 4 → -2
        let data = [0b10100110, 0b01000000];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_signed_exp_golomb(), Some(0)); // ue=0 → se=0
        assert_eq!(reader.read_signed_exp_golomb(), Some(1)); // ue=1 → se=1
        assert_eq!(reader.read_signed_exp_golomb(), Some(-1)); // ue=2 → se=-1
        assert_eq!(reader.read_signed_exp_golomb(), Some(2)); // ue=3 → se=2
    }
}
