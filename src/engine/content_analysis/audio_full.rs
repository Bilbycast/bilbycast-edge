// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Audio Full content-analysis tier.
//!
//! Subscribes to the flow's broadcast channel and decodes every audio PID
//! announced in the PMT. For each decoded PCM block we feed the
//! [`ebur128`] crate (pure Rust) and maintain per-PID loudness, true
//! peak, silence, hard-mute, and clipping state. The R128 pipeline
//! matches EBU R128 / ITU-R BS.1770 with the standard `M | S | I | LRA
//! | TRUE_PEAK` modes.
//!
//! **Codec coverage**: AAC-LC / HE-AAC (ADTS in MPEG-TS, `stream_type =
//! 0x0F`) decodes to PCM inline via the existing
//! [`crate::engine::audio_decode::AacDecoder`] (Fraunhofer FDK-AAC).
//! MP2 / AC-3 / E-AC-3 PIDs are detected and tracked but R128 decode
//! requires the libavcodec bridge that also powers the output-side
//! audio encoder; that decode path is intentionally kept out of the
//! content-analysis hot loop. Those PIDs publish `r128: null` +
//! `codec_decoded: false` with a structured reason so the manager UI
//! can render the pending state distinct from "analyser off".
//!
//! Non-blocking rules: drop-on-lag, per-PID PES buffers pre-sized, R128
//! state re-used across the life of the PID.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ebur128::{EbuR128, Mode as R128Mode};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::audio_decode::{sample_rate_from_index, AacDecoder};
use crate::manager::events::{Event, EventSender, EventSeverity};
use crate::stats::collector::ContentAnalysisAccumulator;

use crate::engine::packet::RtpPacket;
use crate::engine::ts_parse::{
    ts_adaptation_field_control, ts_has_payload, ts_pid, ts_pusi, RTP_HEADER_MIN_SIZE, TS_PACKET_SIZE,
    TS_SYNC_BYTE,
};

const PAT_PID: u16 = 0x0000;
const PUBLISH_INTERVAL: Duration = Duration::from_millis(500);
/// PCM-level silence threshold in dBFS. `-60 dBFS` is the widely-used
/// broadcast silence floor.
const SILENCE_DBFS_THRESHOLD: f32 = -60.0;
const SILENCE_DEBOUNCE: Duration = Duration::from_secs(2);
/// Samples whose magnitude meets or exceeds this are counted as clipping.
/// 0.9975 maps to the standard -0.02 dBFS clipping convention used by
/// broadcast R128 meters.
const CLIP_MAGNITUDE: f32 = 0.9975;
/// Consecutive all-zero samples that count as hard-mute. At 48 kHz this is
/// ≈ 41.7 ms — short enough to catch a real hard-mute, long enough to
/// ignore a single bad packet or silence-padding transient.
const HARD_MUTE_RUN: u64 = 2_000;
const EVENT_RATELIMIT: Duration = Duration::from_secs(30);

/// Selects how the Audio Full analyser parses incoming broadcast packets.
#[derive(Debug, Clone, Copy)]
pub enum AudioFullMode {
    /// Broadcast channel carries MPEG-TS — walk PMT, reassemble PES, decode
    /// AAC ADTS frames, feed PCM to R128. The full pipeline.
    Ts,
    /// Broadcast channel carries linear PCM big-endian, interleaved samples
    /// per RTP packet (ST 2110-30 PM/AM profiles, generic RtpAudio). No
    /// decoder needed — depacketize directly into f32 and feed R128.
    Pcm {
        codec: &'static str,   // "pcm_l16" or "pcm_l24"
        sample_rate: u32,
        channels: u8,
        bytes_per_sample: u8,  // 2 or 3
    },
    /// ST 2110-31 raw AES3 subframes (32 bits / 4 bytes per subframe). The
    /// 24-bit audio sample lives in bits 27..4 of each subframe; the rest
    /// is preamble + V/U/C/P bits we strip before feeding R128.
    Aes3 {
        sample_rate: u32,
        channels: u8,
    },
}

pub fn spawn_content_analysis_audio_full(
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<ContentAnalysisAccumulator>,
    event_sender: EventSender,
    flow_id: String,
    mode: AudioFullMode,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(audio_full_loop(rx, stats, event_sender, flow_id, mode, cancel))
}

async fn audio_full_loop(
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<ContentAnalysisAccumulator>,
    events: EventSender,
    flow_id: String,
    mode: AudioFullMode,
    cancel: CancellationToken,
) {
    tracing::info!(flow_id = %flow_id, mode = ?mode, "content-analysis Audio Full analyser started");

    let mut state = AudioFullState::new(mode);
    let mut publish_interval = tokio::time::interval(PUBLISH_INTERVAL);
    publish_interval.tick().await;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(flow_id = %flow_id, "content-analysis Audio Full stopping (cancelled)");
                break;
            }

            _ = publish_interval.tick() => {
                state.tick(&flow_id, &events);
                let snap = state.snapshot();
                *stats.audio_full.lock().unwrap() = Some(snap);
            }

            result = rx.recv() => {
                match result {
                    Ok(packet) => state.process_packet(&packet),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.audio_full_drops.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            flow_id = %flow_id,
                            "content-analysis Audio Full: broadcast closed"
                        );
                        break;
                    }
                }
            }
        }
    }
}

// ── Per-PID state ──────────────────────────────────────────────────────────

struct AudioPidState {
    pid: u16,
    stream_type: u8,
    codec: &'static str,
    packets: u64,
    bytes: u64,
    window_start: Instant,
    window_bytes: u64,
    last_bitrate_bps: f64,

    /// AAC decoder, lazily constructed once we've seen the first ADTS
    /// frame. `None` for non-AAC codecs (MP2 / AC-3 / E-AC-3) where the
    /// decode path is not wired in today's build.
    aac_decoder: Option<AacDecoder>,
    /// PES reassembly buffer keyed by PUSI. Sized for 1 s of 320 kbps —
    /// comfortably larger than the largest realistic audio PES.
    pes_buf: Vec<u8>,
    capturing_pes: bool,

    /// EBU R128 state, initialised once the decoder tells us sample
    /// rate + channels. Feed PCM frames into `add_frames_f32_planar`
    /// (via intermediate copy — R128 wants interleaved).
    r128: Option<EbuR128>,
    r128_sample_rate: u32,
    r128_channels: u32,

    /// Rolling per-second clip counter.
    clip_run_sec_start: Instant,
    clip_samples_this_second: u64,
    clip_rate_pps: f64,

    /// Hard-mute detection: track consecutive all-zero samples across
    /// channels. Resets on any non-zero.
    zero_sample_run: u64,
    hard_mute_active: bool,

    /// True-peak in linear scale (max |sample| observed this publish window).
    true_peak_linear: f32,
    /// Last-published true-peak in dBTP.
    last_true_peak_dbtp: Option<f32>,

    /// Silence state + event debounce.
    silent_since: Option<Instant>,
    likely_silent: bool,
    last_silence_event_at: Option<Instant>,

    /// Published R128 values (momentary / short-term / integrated / LRA).
    last_lufs_m: Option<f64>,
    last_lufs_s: Option<f64>,
    last_lufs_i: Option<f64>,
    last_lra: Option<f64>,

    /// Decode-status reason string for codecs we can't currently decode.
    decode_note: Option<&'static str>,
}

impl AudioPidState {
    fn new(pid: u16, stream_type: u8) -> Self {
        Self {
            pid,
            stream_type,
            codec: codec_name(stream_type),
            packets: 0,
            bytes: 0,
            window_start: Instant::now(),
            window_bytes: 0,
            last_bitrate_bps: 0.0,
            aac_decoder: None,
            pes_buf: Vec::with_capacity(16_384),
            capturing_pes: false,
            r128: None,
            r128_sample_rate: 0,
            r128_channels: 0,
            clip_run_sec_start: Instant::now(),
            clip_samples_this_second: 0,
            clip_rate_pps: 0.0,
            zero_sample_run: 0,
            hard_mute_active: false,
            true_peak_linear: 0.0,
            last_true_peak_dbtp: None,
            silent_since: None,
            likely_silent: false,
            last_silence_event_at: None,
            last_lufs_m: None,
            last_lufs_s: None,
            last_lufs_i: None,
            last_lra: None,
            decode_note: match stream_type {
                0x03 => Some("mpeg-1 audio decode deferred (libavcodec wire-up)"),
                0x04 => Some("mpeg-2 audio decode deferred (libavcodec wire-up)"),
                0x81 | 0x80 | 0xC1 => Some("ac-3 decode deferred (libavcodec wire-up)"),
                0x87 | 0xC2 => Some("e-ac-3 decode deferred (libavcodec wire-up)"),
                _ => None,
            },
        }
    }

    fn observe_ts(&mut self, pusi: bool, ts_payload: &[u8]) {
        self.packets += 1;
        self.bytes += ts_payload.len() as u64;
        self.window_bytes += ts_payload.len() as u64;
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.last_bitrate_bps =
                (self.window_bytes as f64 * 8.0) / elapsed.as_secs_f64();
            self.window_start = Instant::now();
            self.window_bytes = 0;
        }

        // Only AAC streams feed the PES buffer today — MP2 / AC-3 PES
        // bytes are counted toward bitrate but not buffered (we don't
        // decode them), saving ~40 KB of per-PID heap.
        if self.stream_type != 0x0F && self.stream_type != 0x11 {
            return;
        }

        if pusi {
            // Skip PES header before appending.
            let start = pes_payload_offset(ts_payload);
            self.pes_buf.clear();
            self.capturing_pes = true;
            if start < ts_payload.len() {
                self.pes_buf.extend_from_slice(&ts_payload[start..]);
            }
        } else if self.capturing_pes {
            self.pes_buf.extend_from_slice(ts_payload);
        }

        // Extract every complete ADTS frame currently buffered and decode it.
        self.drain_adts_frames();
    }

    fn drain_adts_frames(&mut self) {
        let mut consume = 0;
        loop {
            let buf = &self.pes_buf[consume..];
            if buf.len() < 7 {
                break;
            }
            // ADTS sync: 12 bits of 1s
            if !(buf[0] == 0xFF && (buf[1] & 0xF0) == 0xF0) {
                // Lost sync — walk forward one byte at a time looking
                // for the next sync word.
                consume += 1;
                continue;
            }
            let frame_len = (((buf[3] as usize) & 0x03) << 11)
                | ((buf[4] as usize) << 3)
                | ((buf[5] as usize) >> 5);
            if frame_len < 7 || frame_len > buf.len() {
                break;
            }
            let has_crc = (buf[1] & 0x01) == 0;
            let header_len = if has_crc { 9 } else { 7 };
            if frame_len <= header_len {
                consume += frame_len;
                continue;
            }
            // Extract config bits
            let profile = (buf[2] >> 6) & 0x03;
            let sr_index = (buf[2] >> 2) & 0x0F;
            let channel_config = ((buf[2] & 0x01) << 2) | ((buf[3] >> 6) & 0x03);

            // Initialise decoder lazily once we have a valid config.
            if self.aac_decoder.is_none() {
                if let Some(sr) = sample_rate_from_index(sr_index) {
                    if channel_config >= 1
                        && channel_config <= 7
                        && let Ok(dec) =
                            AacDecoder::from_adts_config(profile, sr_index, channel_config)
                    {
                        self.aac_decoder = Some(dec);
                        self.r128_sample_rate = sr;
                        self.r128_channels = channel_config.min(7) as u32;
                        if let Ok(r128) = EbuR128::new(
                            self.r128_channels,
                            self.r128_sample_rate,
                            R128Mode::I
                                | R128Mode::M
                                | R128Mode::S
                                | R128Mode::LRA
                                | R128Mode::TRUE_PEAK,
                        ) {
                            self.r128 = Some(r128);
                        }
                    }
                }
            }
            let frame = &buf[header_len..frame_len];
            if let Some(ref mut dec) = self.aac_decoder {
                match dec.decode_frame(frame) {
                    Ok(planar) => {
                        self.feed_pcm(&planar);
                    }
                    Err(_e) => {
                        // decode error — ignore, next frame
                    }
                }
            }
            consume += frame_len;
        }
        if consume > 0 {
            self.pes_buf.drain(..consume);
        }
    }

    fn feed_pcm(&mut self, planar: &[Vec<f32>]) {
        if planar.is_empty() {
            return;
        }
        let channels = planar.len();
        let frames = planar[0].len();
        // Track mute / clip / true-peak on interleaved samples without
        // actually re-interleaving — walk by frame index.
        for i in 0..frames {
            let mut all_zero = true;
            for ch in 0..channels {
                let s = planar[ch].get(i).copied().unwrap_or(0.0);
                let mag = s.abs();
                if mag > self.true_peak_linear {
                    self.true_peak_linear = mag;
                }
                if mag >= CLIP_MAGNITUDE {
                    self.clip_samples_this_second += 1;
                }
                if s != 0.0 {
                    all_zero = false;
                }
            }
            if all_zero {
                self.zero_sample_run += 1;
            } else {
                self.zero_sample_run = 0;
            }
        }
        if self.zero_sample_run >= HARD_MUTE_RUN {
            self.hard_mute_active = true;
        } else {
            self.hard_mute_active = false;
        }

        // Feed R128 — it wants interleaved f32. Build a scratch buffer
        // sized to this PCM block only (no long-lived allocation).
        if let Some(ref mut r128) = self.r128 {
            let mut interleaved = Vec::with_capacity(frames * channels);
            for i in 0..frames {
                for ch in 0..channels {
                    interleaved.push(planar[ch].get(i).copied().unwrap_or(0.0));
                }
            }
            let _ = r128.add_frames_f32(&interleaved);
        }

        // Update clip rolling rate every second.
        if self.clip_run_sec_start.elapsed() >= Duration::from_secs(1) {
            self.clip_rate_pps = self.clip_samples_this_second as f64;
            self.clip_samples_this_second = 0;
            self.clip_run_sec_start = Instant::now();
        }
    }

    fn tick(&mut self, flow_id: &str, events: &EventSender) {
        // Query R128 if ready.
        if let Some(ref r128) = self.r128 {
            self.last_lufs_m = r128.loudness_momentary().ok();
            self.last_lufs_s = r128.loudness_shortterm().ok();
            self.last_lufs_i = r128.loudness_global().ok();
            self.last_lra = r128.loudness_range().ok();
        }
        if self.true_peak_linear > 0.0 {
            self.last_true_peak_dbtp = Some(20.0 * self.true_peak_linear.log10());
        }
        // Reset true-peak window after snapshotting so it reflects recent
        // peak, not historical.
        self.true_peak_linear = 0.0;

        // Silence detection — prefer PCM-level (momentary dBFS); fall back
        // to bitrate collapse if we have no decoder.
        let now = Instant::now();
        let pcm_loudness = self.last_lufs_m;
        let is_silent_now = if let Some(lufs) = pcm_loudness {
            // R128 `M` is LUFS (≈ dBFS minus a few dB for the K-weighting
            // bump). -70 LUFS means genuine silence. Be permissive so a
            // quiet dialog scene doesn't alarm.
            lufs.is_finite() && lufs as f32 <= SILENCE_DBFS_THRESHOLD
        } else {
            self.last_bitrate_bps < 1_000.0
        };
        if is_silent_now {
            if self.silent_since.is_none() {
                self.silent_since = Some(now);
            }
            if let Some(since) = self.silent_since {
                if now.duration_since(since) >= SILENCE_DEBOUNCE && !self.likely_silent {
                    self.likely_silent = true;
                    let fire = self
                        .last_silence_event_at
                        .map_or(true, |t| t.elapsed() >= EVENT_RATELIMIT);
                    if fire {
                        events.send(Event {
                            severity: EventSeverity::Warning,
                            category: "content_analysis_audio_silent".into(),
                            message: format!(
                                "Audio PID 0x{:04X} ({}): silent for {:.1}s (loudness {:?})",
                                self.pid,
                                self.codec,
                                SILENCE_DEBOUNCE.as_secs_f32(),
                                pcm_loudness,
                            ),
                            details: Some(serde_json::json!({
                                "error_code": "content_analysis_audio_silent",
                                "pid": self.pid,
                                "codec": self.codec,
                                "lufs_m": pcm_loudness,
                                "bitrate_bps": self.last_bitrate_bps,
                            })),
                            flow_id: Some(flow_id.into()),
                            input_id: None,
                            output_id: None,
                        });
                        self.last_silence_event_at = Some(now);
                    }
                }
            }
        } else {
            self.silent_since = None;
            self.likely_silent = false;
        }
    }
}

struct AudioFullState {
    mode: AudioFullMode,
    pmt_pids: HashMap<u16, u16>,
    audio_pids: HashMap<u16, AudioPidState>,
    /// PCM / AES3 mode produces a single synthetic "PID" entry — we
    /// stamp it with PID 0 so the manager UI gets a stable key. The
    /// state is created on first packet so input config gets to define
    /// rate / channels.
    pcm_state: Option<PcmAudioState>,
}

impl AudioFullState {
    fn new(mode: AudioFullMode) -> Self {
        Self {
            mode,
            pmt_pids: HashMap::new(),
            audio_pids: HashMap::new(),
            pcm_state: None,
        }
    }

    fn process_packet(&mut self, packet: &RtpPacket) {
        match self.mode {
            AudioFullMode::Ts => self.process_ts_payload(packet),
            AudioFullMode::Pcm { .. } | AudioFullMode::Aes3 { .. } => {
                self.process_pcm_payload(packet)
            }
        }
    }

    fn process_ts_payload(&mut self, packet: &RtpPacket) {
        let payload = strip_rtp_header(packet);
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= payload.len() {
            let pkt = &payload[offset..offset + TS_PACKET_SIZE];
            if pkt[0] == TS_SYNC_BYTE {
                self.process_ts_packet(pkt);
            }
            offset += TS_PACKET_SIZE;
        }
    }

    fn process_pcm_payload(&mut self, packet: &RtpPacket) {
        let payload = strip_rtp_header(packet);
        if payload.is_empty() {
            return;
        }
        // Lazy-init the PCM state on first packet.
        if self.pcm_state.is_none() {
            self.pcm_state = Some(PcmAudioState::new(self.mode));
        }
        let Some(ref mut p) = self.pcm_state else { return };
        p.observe_rtp_payload(payload, self.mode);
    }

    fn process_ts_packet(&mut self, pkt: &[u8]) {
        let pid = ts_pid(pkt);
        let pusi = ts_pusi(pkt);
        let adaptation = ts_adaptation_field_control(pkt);
        if !ts_has_payload(pkt) {
            return;
        }
        let payload_offset = if adaptation & 0x02 != 0 {
            5 + pkt[4] as usize
        } else {
            4
        };
        if payload_offset >= TS_PACKET_SIZE {
            return;
        }
        let payload = &pkt[payload_offset..];
        if pid == PAT_PID {
            if pusi {
                self.parse_pat(payload);
            }
            return;
        }
        if self.pmt_pids.values().any(|&p| p == pid) {
            if pusi {
                self.parse_pmt(payload);
            }
            return;
        }
        if let Some(audio_state) = self.audio_pids.get_mut(&pid) {
            audio_state.observe_ts(pusi, payload);
        }
    }

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
        if 3 + section_length > section.len() || section_length < 9 {
            return;
        }
        let body_end = 3 + section_length - 4;
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

    fn parse_pmt(&mut self, payload: &[u8]) {
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
        if 3 + section_length > section.len() || section_length < 13 {
            return;
        }
        let program_info_length =
            (((section[10] as usize) & 0x0F) << 8) | section[11] as usize;
        let es_start = 12 + program_info_length;
        let body_end = 3 + section_length - 4;
        if es_start >= body_end {
            return;
        }

        let mut discovered: HashMap<u16, u8> = HashMap::new();
        let mut i = es_start;
        while i + 5 <= body_end {
            let stream_type = section[i];
            let es_pid = (((section[i + 1] as u16) & 0x1F) << 8) | section[i + 2] as u16;
            let es_info_length =
                (((section[i + 3] as usize) & 0x0F) << 8) | section[i + 4] as usize;
            if is_audio_stream_type(stream_type) {
                discovered.insert(es_pid, stream_type);
            }
            i += 5 + es_info_length;
        }
        for (pid, stype) in discovered.iter() {
            self.audio_pids
                .entry(*pid)
                .or_insert_with(|| AudioPidState::new(*pid, *stype));
        }
        self.audio_pids.retain(|pid, _| discovered.contains_key(pid));
    }

    fn tick(&mut self, flow_id: &str, events: &EventSender) {
        for audio in self.audio_pids.values_mut() {
            audio.tick(flow_id, events);
        }
        if let Some(ref mut p) = self.pcm_state {
            p.tick(flow_id, events);
        }
    }

    fn snapshot(&self) -> serde_json::Value {
        let mut pids: Vec<serde_json::Value> = self
            .audio_pids
            .values()
            .map(|p| {
                let decoded = p.aac_decoder.is_some();
                let lufs_i = p.last_lufs_i.filter(|v| v.is_finite());
                let lufs_m = p.last_lufs_m.filter(|v| v.is_finite());
                let lufs_s = p.last_lufs_s.filter(|v| v.is_finite());
                let lra = p.last_lra.filter(|v| v.is_finite());
                serde_json::json!({
                    "pid": p.pid,
                    "stream_type": p.stream_type,
                    "codec": p.codec,
                    "packets": p.packets,
                    "bytes": p.bytes,
                    "bitrate_bps": p.last_bitrate_bps.round() as u64,
                    "codec_decoded": decoded,
                    "decode_note": p.decode_note,
                    "sample_rate": if decoded { Some(p.r128_sample_rate) } else { None },
                    "channels": if decoded { Some(p.r128_channels) } else { None },
                    "likely_silent": p.likely_silent,
                    "mute": p.hard_mute_active,
                    "clip_rate_pps": p.clip_rate_pps,
                    "true_peak_dbtp": p.last_true_peak_dbtp,
                    "r128": {
                        "m_lufs": lufs_m,
                        "s_lufs": lufs_s,
                        "i_lufs": lufs_i,
                        "lra": lra,
                    },
                })
            })
            .collect();

        if let Some(ref p) = self.pcm_state {
            pids.push(p.snapshot_json());
        }

        let tier_mode = match self.mode {
            AudioFullMode::Ts => "ts",
            AudioFullMode::Pcm { .. } => "pcm",
            AudioFullMode::Aes3 { .. } => "aes3",
        };
        serde_json::json!({
            "tier": "audio_full",
            "version": 3,
            "ingress": tier_mode,
            "audio_pids": pids,
        })
    }
}

// ── PCM / AES3 audio path ──────────────────────────────────────────────────

/// Audio Full state for ST 2110-30 / -31 / RtpAudio inputs. The flow has
/// no PMT and a single audio essence, so this collapses to one PID-less
/// state object. We stamp `pid: 0` in the wire snapshot for UI key
/// stability.
struct PcmAudioState {
    codec: &'static str,
    sample_rate: u32,
    channels: u8,
    packets: u64,
    bytes: u64,
    window_start: Instant,
    window_bytes: u64,
    last_bitrate_bps: f64,

    r128: Option<EbuR128>,
    clip_run_sec_start: Instant,
    clip_samples_this_second: u64,
    clip_rate_pps: f64,
    zero_sample_run: u64,
    hard_mute_active: bool,
    true_peak_linear: f32,
    last_true_peak_dbtp: Option<f32>,

    silent_since: Option<Instant>,
    likely_silent: bool,
    last_silence_event_at: Option<Instant>,

    last_lufs_m: Option<f64>,
    last_lufs_s: Option<f64>,
    last_lufs_i: Option<f64>,
    last_lra: Option<f64>,

    /// Reusable interleaved scratch buffer to avoid per-packet allocations.
    interleaved_scratch: Vec<f32>,
}

impl PcmAudioState {
    fn new(mode: AudioFullMode) -> Self {
        let (codec, sample_rate, channels) = match mode {
            AudioFullMode::Pcm { codec, sample_rate, channels, .. } => {
                (codec, sample_rate, channels)
            }
            AudioFullMode::Aes3 { sample_rate, channels } => {
                ("aes3_l24", sample_rate, channels)
            }
            AudioFullMode::Ts => ("ts", 0, 0), // unreachable in PCM path
        };
        let r128 = if sample_rate > 0 && channels > 0 {
            EbuR128::new(
                channels as u32,
                sample_rate,
                R128Mode::I | R128Mode::M | R128Mode::S | R128Mode::LRA | R128Mode::TRUE_PEAK,
            )
            .ok()
        } else {
            None
        };
        Self {
            codec,
            sample_rate,
            channels,
            packets: 0,
            bytes: 0,
            window_start: Instant::now(),
            window_bytes: 0,
            last_bitrate_bps: 0.0,
            r128,
            clip_run_sec_start: Instant::now(),
            clip_samples_this_second: 0,
            clip_rate_pps: 0.0,
            zero_sample_run: 0,
            hard_mute_active: false,
            true_peak_linear: 0.0,
            last_true_peak_dbtp: None,
            silent_since: None,
            likely_silent: false,
            last_silence_event_at: None,
            last_lufs_m: None,
            last_lufs_s: None,
            last_lufs_i: None,
            last_lra: None,
            interleaved_scratch: Vec::with_capacity(8192),
        }
    }

    fn observe_rtp_payload(&mut self, payload: &[u8], mode: AudioFullMode) {
        self.packets += 1;
        self.bytes += payload.len() as u64;
        self.window_bytes += payload.len() as u64;
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.last_bitrate_bps =
                (self.window_bytes as f64 * 8.0) / elapsed.as_secs_f64();
            self.window_start = Instant::now();
            self.window_bytes = 0;
        }

        let channels = self.channels as usize;
        if channels == 0 {
            return;
        }

        // Decode RTP payload bytes → interleaved f32 samples.
        self.interleaved_scratch.clear();
        match mode {
            AudioFullMode::Pcm { bytes_per_sample, .. } => {
                let bps = bytes_per_sample as usize;
                let frame_size = bps * channels;
                if frame_size == 0 || payload.len() < frame_size {
                    return;
                }
                let frames = payload.len() / frame_size;
                self.interleaved_scratch.reserve(frames * channels);
                for f in 0..frames {
                    for ch in 0..channels {
                        let off = f * frame_size + ch * bps;
                        let s = match bps {
                            2 => i16_be(&payload[off..off + 2]) as f32 / 32768.0,
                            3 => i24_be(&payload[off..off + 3]) as f32 / 8_388_608.0,
                            _ => 0.0,
                        };
                        self.interleaved_scratch.push(s);
                    }
                }
            }
            AudioFullMode::Aes3 { .. } => {
                // 32-bit AES3 subframes, big-endian. 24-bit audio sample
                // sits in bits 27..4 of each 4-byte subframe.
                let frame_size = 4 * channels;
                if frame_size == 0 || payload.len() < frame_size {
                    return;
                }
                let frames = payload.len() / frame_size;
                self.interleaved_scratch.reserve(frames * channels);
                for f in 0..frames {
                    for ch in 0..channels {
                        let off = f * frame_size + ch * 4;
                        let sample_24 = aes3_extract_sample(&payload[off..off + 4]);
                        self.interleaved_scratch.push(sample_24 as f32 / 8_388_608.0);
                    }
                }
            }
            AudioFullMode::Ts => return,
        }

        // Per-sample mute / clip / true-peak tracking.
        let frames = self.interleaved_scratch.len() / channels;
        for i in 0..frames {
            let mut all_zero = true;
            for ch in 0..channels {
                let s = self.interleaved_scratch[i * channels + ch];
                let mag = s.abs();
                if mag > self.true_peak_linear {
                    self.true_peak_linear = mag;
                }
                if mag >= CLIP_MAGNITUDE {
                    self.clip_samples_this_second += 1;
                }
                if s != 0.0 {
                    all_zero = false;
                }
            }
            if all_zero {
                self.zero_sample_run += 1;
            } else {
                self.zero_sample_run = 0;
            }
        }
        self.hard_mute_active = self.zero_sample_run >= HARD_MUTE_RUN;

        // Feed R128 with the interleaved buffer (ebur128 wants interleaved).
        if let Some(ref mut r128) = self.r128 {
            let _ = r128.add_frames_f32(&self.interleaved_scratch);
        }

        if self.clip_run_sec_start.elapsed() >= Duration::from_secs(1) {
            self.clip_rate_pps = self.clip_samples_this_second as f64;
            self.clip_samples_this_second = 0;
            self.clip_run_sec_start = Instant::now();
        }
    }

    fn tick(&mut self, flow_id: &str, events: &EventSender) {
        if let Some(ref r128) = self.r128 {
            self.last_lufs_m = r128.loudness_momentary().ok();
            self.last_lufs_s = r128.loudness_shortterm().ok();
            self.last_lufs_i = r128.loudness_global().ok();
            self.last_lra = r128.loudness_range().ok();
        }
        if self.true_peak_linear > 0.0 {
            self.last_true_peak_dbtp = Some(20.0 * self.true_peak_linear.log10());
        }
        self.true_peak_linear = 0.0;

        let now = Instant::now();
        let pcm_loudness = self.last_lufs_m;
        let is_silent_now = pcm_loudness
            .map(|lufs| lufs.is_finite() && lufs as f32 <= SILENCE_DBFS_THRESHOLD)
            .unwrap_or(false);
        if is_silent_now {
            if self.silent_since.is_none() {
                self.silent_since = Some(now);
            }
            if let Some(since) = self.silent_since {
                if now.duration_since(since) >= SILENCE_DEBOUNCE && !self.likely_silent {
                    self.likely_silent = true;
                    let fire = self
                        .last_silence_event_at
                        .map_or(true, |t| t.elapsed() >= EVENT_RATELIMIT);
                    if fire {
                        events.send(Event {
                            severity: EventSeverity::Warning,
                            category: "content_analysis_audio_silent".into(),
                            message: format!(
                                "PCM audio silent for {:.1}s (loudness {:?})",
                                SILENCE_DEBOUNCE.as_secs_f32(),
                                pcm_loudness,
                            ),
                            details: Some(serde_json::json!({
                                "error_code": "content_analysis_audio_silent",
                                "codec": self.codec,
                                "lufs_m": pcm_loudness,
                            })),
                            flow_id: Some(flow_id.into()),
                            input_id: None,
                            output_id: None,
                        });
                        self.last_silence_event_at = Some(now);
                    }
                }
            }
        } else {
            self.silent_since = None;
            self.likely_silent = false;
        }
    }

    fn snapshot_json(&self) -> serde_json::Value {
        let lufs_i = self.last_lufs_i.filter(|v| v.is_finite());
        let lufs_m = self.last_lufs_m.filter(|v| v.is_finite());
        let lufs_s = self.last_lufs_s.filter(|v| v.is_finite());
        let lra = self.last_lra.filter(|v| v.is_finite());
        serde_json::json!({
            "pid": 0,
            "stream_type": 0,
            "codec": self.codec,
            "packets": self.packets,
            "bytes": self.bytes,
            "bitrate_bps": self.last_bitrate_bps.round() as u64,
            "codec_decoded": self.r128.is_some(),
            "decode_note": null,
            "sample_rate": self.sample_rate,
            "channels": self.channels,
            "likely_silent": self.likely_silent,
            "mute": self.hard_mute_active,
            "clip_rate_pps": self.clip_rate_pps,
            "true_peak_dbtp": self.last_true_peak_dbtp,
            "r128": {
                "m_lufs": lufs_m,
                "s_lufs": lufs_s,
                "i_lufs": lufs_i,
                "lra": lra,
            },
        })
    }
}

#[inline]
fn i16_be(b: &[u8]) -> i16 {
    ((b[0] as i16) << 8) | (b[1] as i16 & 0xFF)
}

#[inline]
fn i24_be(b: &[u8]) -> i32 {
    let raw = ((b[0] as i32) << 16) | ((b[1] as i32) << 8) | (b[2] as i32);
    // Sign-extend 24 → 32 bits.
    if raw & 0x0080_0000 != 0 {
        raw | !0x00FF_FFFFi32
    } else {
        raw
    }
}

/// Extract the 24-bit signed audio sample from one 32-bit AES3 subframe in
/// network byte order. Layout (per IEC 60958-3 + SMPTE ST 2110-31, viewed
/// big-endian):
///   byte 0 = audio[27..24] in low nibble + V/U/C/P bits in high nibble
///   byte 1 = audio[23..16]
///   byte 2 = audio[15..8]
///   byte 3 = audio[7..0]  (high nibble is preamble)
#[inline]
fn aes3_extract_sample(b: &[u8]) -> i32 {
    let raw = (((b[0] & 0x0F) as i32) << 20)
        | ((b[1] as i32) << 12)
        | ((b[2] as i32) << 4)
        | (((b[3] >> 4) & 0x0F) as i32);
    if raw & 0x0080_0000 != 0 {
        raw | !0x00FF_FFFFi32
    } else {
        raw
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn strip_rtp_header(packet: &RtpPacket) -> &[u8] {
    let data = packet.data.as_ref();
    if packet.is_raw_ts {
        return data;
    }
    if data.len() < RTP_HEADER_MIN_SIZE {
        return &[];
    }
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

fn pes_payload_offset(payload: &[u8]) -> usize {
    if payload.len() < 9 {
        return 0;
    }
    if payload[0] != 0 || payload[1] != 0 || payload[2] != 1 {
        return 0;
    }
    let stream_id = payload[3];
    if !(0xC0..=0xEF).contains(&stream_id) {
        return 0;
    }
    let hdr_len = payload[8] as usize;
    let es_start = 9 + hdr_len;
    if es_start > payload.len() {
        return payload.len();
    }
    es_start
}

fn is_audio_stream_type(st: u8) -> bool {
    matches!(
        st,
        0x03 | 0x04 | 0x0F | 0x11 | 0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85 | 0x87 | 0x88 | 0x8A | 0xC1 | 0xC2
    )
}

fn codec_name(stream_type: u8) -> &'static str {
    match stream_type {
        0x03 => "mp1_audio",
        0x04 => "mp2_audio",
        0x0F => "aac_adts",
        0x11 => "aac_latm",
        0x80 | 0x81 => "ac3",
        0x82 => "dts",
        0x83 => "lpcm",
        0x87 | 0xC2 => "eac3",
        _ => "unknown",
    }
}
