// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::watch;

use super::models::*;
use super::throughput::ThroughputEstimator;

/// Per-output atomic counters for a single output leg.
///
/// Tracks `packets_sent`, `bytes_sent`, `packets_dropped`, and
/// `fec_packets_sent` as lock-free [`AtomicU64`] values. The hot
/// data-plane path increments these with `Relaxed` ordering; the
/// stats snapshot path reads them to produce an [`OutputStats`].
pub struct OutputStatsAccumulator {
    pub output_id: String,
    pub output_name: String,
    pub output_type: String,
    pub packets_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub fec_packets_sent: AtomicU64,
    throughput: Mutex<ThroughputEstimator>,
    /// Cached SRT stats for primary leg, updated by the SRT output polling task
    /// via a lock-free watch channel. Read via `borrow()`, write via `send()`.
    pub srt_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Cached SRT stats for redundancy leg, updated by the SRT output polling task
    /// via a lock-free watch channel.
    pub srt_leg2_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Optional handle to a per-output PCM transcoder's stats counters.
    /// Set once at output startup by `run_st2110_audio_output` (and any other
    /// output that runs a TranscodeStage). Reading is a single atomic load.
    transcode_stats: OnceLock<Arc<crate::engine::audio_transcode::TranscodeStats>>,
    /// Optional handle to the per-output AAC decode counters plus the
    /// descriptors the UI needs to label the stage. Set once at output
    /// startup by outputs that build an `engine::audio_decode::AacDecoder`.
    audio_decode_stats: OnceLock<AudioDecodeStatsHandle>,
    /// Optional handle to the per-output audio encode counters plus the
    /// resolved target codec descriptors. Set once at output startup by
    /// outputs that spawn an `engine::audio_encode::AudioEncoder`.
    audio_encode_stats: OnceLock<AudioEncodeStatsHandle>,
}

/// Registered handle to a per-output decode stage's counters plus the
/// steady-state descriptors the snapshot path needs to build a
/// [`crate::stats::models::DecodeStatsSnapshot`].
pub struct AudioDecodeStatsHandle {
    pub stats: Arc<crate::engine::audio_decode::DecodeStats>,
    pub input_codec: String,
    pub output_sample_rate_hz: u32,
    pub output_channels: u8,
}

/// Registered handle to a per-output encode stage's counters plus the
/// resolved target codec / format descriptors.
pub struct AudioEncodeStatsHandle {
    pub stats: Arc<crate::engine::audio_encode::EncodeStats>,
    pub output_codec: String,
    pub target_sample_rate_hz: u32,
    pub target_channels: u8,
    pub target_bitrate_kbps: u32,
}

impl OutputStatsAccumulator {
    /// Create a new accumulator with all counters initialised to zero.
    pub fn new(output_id: String, output_name: String, output_type: String) -> Self {
        Self {
            output_id,
            output_name,
            output_type,
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            packets_dropped: AtomicU64::new(0),
            fec_packets_sent: AtomicU64::new(0),
            throughput: Mutex::new(ThroughputEstimator::new()),
            srt_stats_cache: Arc::new(watch::channel(None).0),
            srt_leg2_stats_cache: Arc::new(watch::channel(None).0),
            transcode_stats: OnceLock::new(),
            audio_decode_stats: OnceLock::new(),
            audio_encode_stats: OnceLock::new(),
        }
    }

    /// Register the per-output transcoder stats handle. Called once at
    /// output startup. Subsequent calls are no-ops (first wins).
    pub fn set_transcode_stats(
        &self,
        stats: Arc<crate::engine::audio_transcode::TranscodeStats>,
    ) {
        let _ = self.transcode_stats.set(stats);
    }

    /// Register the per-output audio decode stats handle. Called once at
    /// output startup by outputs that instantiate an AAC decoder. Subsequent
    /// calls are no-ops (first wins).
    pub fn set_decode_stats(
        &self,
        stats: Arc<crate::engine::audio_decode::DecodeStats>,
        input_codec: impl Into<String>,
        output_sample_rate_hz: u32,
        output_channels: u8,
    ) {
        let _ = self.audio_decode_stats.set(AudioDecodeStatsHandle {
            stats,
            input_codec: input_codec.into(),
            output_sample_rate_hz,
            output_channels,
        });
    }

    /// Register the per-output audio encode stats handle. Called once at
    /// output startup by outputs that spawn an [`crate::engine::audio_encode::AudioEncoder`].
    /// Subsequent calls are no-ops (first wins).
    pub fn set_encode_stats(
        &self,
        stats: Arc<crate::engine::audio_encode::EncodeStats>,
        output_codec: impl Into<String>,
        target_sample_rate_hz: u32,
        target_channels: u8,
        target_bitrate_kbps: u32,
    ) {
        let _ = self.audio_encode_stats.set(AudioEncodeStatsHandle {
            stats,
            output_codec: output_codec.into(),
            target_sample_rate_hz,
            target_channels,
            target_bitrate_kbps,
        });
    }

    /// Take a point-in-time snapshot of all atomic counters and return an
    /// [`OutputStats`] value suitable for JSON serialisation.
    pub fn snapshot(&self) -> OutputStats {
        let bytes = self.bytes_sent.load(Ordering::Relaxed);
        let bitrate_bps = self.throughput.lock().unwrap().sample(bytes);
        let transcode_stats = self.transcode_stats.get().map(|t| {
            crate::stats::models::TranscodeStatsSnapshot {
                input_packets: t.input_packets.load(Ordering::Relaxed),
                output_packets: t.output_packets.load(Ordering::Relaxed),
                dropped: t.dropped.load(Ordering::Relaxed),
                format_resets: t.format_resets.load(Ordering::Relaxed),
                last_latency_us: t.last_latency_us.load(Ordering::Relaxed),
            }
        });
        let audio_decode_stats = self.audio_decode_stats.get().map(|h| {
            crate::stats::models::DecodeStatsSnapshot {
                input_frames: h.stats.input_frames.load(Ordering::Relaxed),
                output_blocks: h.stats.output_blocks.load(Ordering::Relaxed),
                decode_errors: h.stats.decode_errors.load(Ordering::Relaxed),
                dropped_uninit: h.stats.dropped_uninit.load(Ordering::Relaxed),
                input_codec: h.input_codec.clone(),
                output_sample_rate_hz: h.output_sample_rate_hz,
                output_channels: h.output_channels,
            }
        });
        let audio_encode_stats = self.audio_encode_stats.get().map(|h| {
            crate::stats::models::EncodeStatsSnapshot {
                pcm_frames_submitted: h.stats.pcm_frames_submitted.load(Ordering::Relaxed),
                pcm_frames_dropped: h.stats.pcm_frames_dropped.load(Ordering::Relaxed),
                encoded_frames_out: h.stats.encoded_frames_out.load(Ordering::Relaxed),
                supervisor_restarts: h.stats.supervisor_restarts.load(Ordering::Relaxed),
                output_codec: h.output_codec.clone(),
                target_sample_rate_hz: h.target_sample_rate_hz,
                target_channels: h.target_channels,
                target_bitrate_kbps: h.target_bitrate_kbps,
            }
        });
        OutputStats {
            output_id: self.output_id.clone(),
            output_name: self.output_name.clone(),
            output_type: self.output_type.clone(),
            state: "active".to_string(),
            mode: None,
            remote_addr: None,
            dest_addr: None,
            dest_url: None,
            ingest_url: None,
            whip_url: None,
            local_addr: None,
            program_number: None,
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            bytes_sent: bytes,
            bitrate_bps,
            packets_dropped: self.packets_dropped.load(Ordering::Relaxed),
            fec_packets_sent: self.fec_packets_sent.load(Ordering::Relaxed),
            srt_stats: self.srt_stats_cache.borrow().clone(),
            srt_leg2_stats: self.srt_leg2_stats_cache.borrow().clone(),
            transcode_stats,
            audio_decode_stats,
            audio_encode_stats,
        }
    }
}

// ── TR-101290 Accumulator ──────────────────────────────────────────────────

/// Internal per-PID PCR tracking state.
pub struct PcrState {
    /// Last PCR value in 27 MHz ticks.
    pub last_pcr_value: u64,
    /// Wall-clock time when the last PCR was received.
    pub last_pcr_wall_time: Instant,
}

/// Internal mutable state for stateful TR-101290 checks.
///
/// Only accessed by the single analyzer task (writes) and the 1/sec
/// snapshot path (brief read lock). Contention is negligible.
pub struct Tr101290State {
    /// Per-PID last continuity counter value.
    pub cc_tracker: HashMap<u16, u8>,
    /// Last time PID 0x0000 (PAT) was seen.
    pub last_pat_time: Option<Instant>,
    /// Whether we have ever received a PAT (to avoid false alarms on startup).
    pub pat_seen: bool,
    /// PMT PIDs discovered from PAT, mapped to their last-seen time.
    pub pmt_pids: HashMap<u16, Option<Instant>>,
    /// Elementary stream PIDs discovered from PMT, mapped to their last-seen time.
    /// Used for PID error detection (P1): ES PIDs that stop appearing.
    pub es_pids: HashMap<u16, Option<Instant>>,
    /// Per-PID PCR tracking for discontinuity and accuracy checks.
    pub pcr_tracker: HashMap<u16, PcrState>,
    /// Whether the stream is currently in sync.
    pub in_sync: bool,
    /// Consecutive TS packets with correct sync byte.
    pub sync_consecutive_good: u32,
    /// Consecutive TS packets with incorrect sync byte.
    pub sync_consecutive_bad: u32,
    // ── IAT / PDV tracking (RP 2129 U2, M2, M3) ──
    /// Last RTP packet receive time in microseconds.
    pub last_recv_time_us: Option<u64>,
    /// Last RTP timestamp for jitter calculation.
    pub last_rtp_timestamp: Option<u32>,
    /// RFC 3550 jitter estimator (in timestamp units, scaled to microseconds).
    pub jitter_us: f64,
    /// IAT running stats for the current window.
    pub iat_min_us: f64,
    pub iat_max_us: f64,
    pub iat_sum_us: f64,
    pub iat_count: u64,
    // ── VSF TR-07 detection ──
    /// Whether JPEG XS (stream type 0x61) has been detected in any PMT.
    pub jpeg_xs_detected: bool,
    /// PID of the JPEG XS elementary stream, if detected.
    pub jpeg_xs_pid: Option<u16>,
}

impl Default for Tr101290State {
    fn default() -> Self {
        Self {
            cc_tracker: HashMap::new(),
            last_pat_time: None,
            pat_seen: false,
            pmt_pids: HashMap::new(),
            es_pids: HashMap::new(),
            pcr_tracker: HashMap::new(),
            in_sync: true,
            sync_consecutive_good: 0,
            sync_consecutive_bad: 0,
            last_recv_time_us: None,
            last_rtp_timestamp: None,
            jitter_us: 0.0,
            iat_min_us: f64::MAX,
            iat_max_us: 0.0,
            iat_sum_us: 0.0,
            iat_count: 0,
            jpeg_xs_detected: false,
            jpeg_xs_pid: None,
        }
    }
}

/// Lock-free TR-101290 statistics accumulator.
///
/// Atomic counters are incremented by the analyzer task with `Relaxed`
/// ordering. The `state` mutex holds per-PID tracking data and is only
/// contended between the analyzer task and the rare 1/sec snapshot.
pub struct Tr101290Accumulator {
    // ── Cumulative counters (monotonically increasing, lifetime of the flow) ──
    // Informational
    pub ts_packets_analyzed: AtomicU64,
    pub pat_count: AtomicU64,
    pub pmt_count: AtomicU64,
    // Priority 1
    pub sync_loss_count: AtomicU64,
    pub sync_byte_errors: AtomicU64,
    pub cc_errors: AtomicU64,
    pub pat_errors: AtomicU64,
    pub pmt_errors: AtomicU64,
    pub pid_errors: AtomicU64,
    // Priority 2
    pub tei_errors: AtomicU64,
    pub crc_errors: AtomicU64,
    pub pcr_discontinuity_errors: AtomicU64,
    pub pcr_accuracy_errors: AtomicU64,

    // ── Windowed counters (reset each snapshot, "errors since last report") ──
    pub window_cc_errors: AtomicU64,
    pub window_pat_errors: AtomicU64,
    pub window_pmt_errors: AtomicU64,
    pub window_pid_errors: AtomicU64,
    pub window_tei_errors: AtomicU64,
    pub window_crc_errors: AtomicU64,
    pub window_pcr_discontinuity_errors: AtomicU64,
    pub window_pcr_accuracy_errors: AtomicU64,

    // Internal state
    pub state: Mutex<Tr101290State>,
}

impl Tr101290Accumulator {
    pub fn new() -> Self {
        Self {
            ts_packets_analyzed: AtomicU64::new(0),
            pat_count: AtomicU64::new(0),
            pmt_count: AtomicU64::new(0),
            sync_loss_count: AtomicU64::new(0),
            sync_byte_errors: AtomicU64::new(0),
            cc_errors: AtomicU64::new(0),
            pat_errors: AtomicU64::new(0),
            pmt_errors: AtomicU64::new(0),
            pid_errors: AtomicU64::new(0),
            tei_errors: AtomicU64::new(0),
            crc_errors: AtomicU64::new(0),
            pcr_discontinuity_errors: AtomicU64::new(0),
            pcr_accuracy_errors: AtomicU64::new(0),
            window_cc_errors: AtomicU64::new(0),
            window_pat_errors: AtomicU64::new(0),
            window_pmt_errors: AtomicU64::new(0),
            window_pid_errors: AtomicU64::new(0),
            window_tei_errors: AtomicU64::new(0),
            window_crc_errors: AtomicU64::new(0),
            window_pcr_discontinuity_errors: AtomicU64::new(0),
            window_pcr_accuracy_errors: AtomicU64::new(0),
            state: Mutex::new(Tr101290State::default()),
        }
    }

    /// Take a point-in-time snapshot of all TR-101290 counters.
    ///
    /// Cumulative counters are always-increasing totals. Windowed counters are
    /// atomically swapped to zero on each snapshot, providing "errors since last
    /// report" for operational dashboards. `priority1_ok` / `priority2_ok` are
    /// derived from the **windowed** counters so they reflect current stream
    /// health, not historical errors.
    pub fn snapshot(&self) -> Tr101290Stats {
        // Cumulative totals
        let sync_loss = self.sync_loss_count.load(Ordering::Relaxed);
        let sync_byte = self.sync_byte_errors.load(Ordering::Relaxed);
        let cc = self.cc_errors.load(Ordering::Relaxed);
        let pat = self.pat_errors.load(Ordering::Relaxed);
        let pmt = self.pmt_errors.load(Ordering::Relaxed);
        let pid = self.pid_errors.load(Ordering::Relaxed);
        let tei = self.tei_errors.load(Ordering::Relaxed);
        let crc = self.crc_errors.load(Ordering::Relaxed);
        let pcr_disc = self.pcr_discontinuity_errors.load(Ordering::Relaxed);
        let pcr_acc = self.pcr_accuracy_errors.load(Ordering::Relaxed);

        // Windowed counters — swap to zero atomically
        let w_cc = self.window_cc_errors.swap(0, Ordering::Relaxed);
        let w_pat = self.window_pat_errors.swap(0, Ordering::Relaxed);
        let w_pmt = self.window_pmt_errors.swap(0, Ordering::Relaxed);
        let w_pid = self.window_pid_errors.swap(0, Ordering::Relaxed);
        let w_tei = self.window_tei_errors.swap(0, Ordering::Relaxed);
        let w_crc = self.window_crc_errors.swap(0, Ordering::Relaxed);
        let w_pcr_disc = self.window_pcr_discontinuity_errors.swap(0, Ordering::Relaxed);
        let w_pcr_acc = self.window_pcr_accuracy_errors.swap(0, Ordering::Relaxed);

        // Priority flags based on windowed counters (current health, not historical)
        let in_sync = { self.state.lock().unwrap().in_sync };
        let priority1_ok = in_sync && w_cc == 0 && w_pat == 0 && w_pmt == 0 && w_pid == 0;
        let priority2_ok = w_tei == 0 && w_crc == 0 && w_pcr_disc == 0 && w_pcr_acc == 0;

        // Read TR-07 state from the state mutex
        let (jpeg_xs_detected, jpeg_xs_pid) = {
            let state = self.state.lock().unwrap();
            (state.jpeg_xs_detected, state.jpeg_xs_pid)
        };

        Tr101290Stats {
            ts_packets_analyzed: self.ts_packets_analyzed.load(Ordering::Relaxed),
            pat_count: self.pat_count.load(Ordering::Relaxed),
            pmt_count: self.pmt_count.load(Ordering::Relaxed),
            sync_loss_count: sync_loss,
            sync_byte_errors: sync_byte,
            cc_errors: cc,
            pat_errors: pat,
            pmt_errors: pmt,
            pid_errors: pid,
            tei_errors: tei,
            crc_errors: crc,
            pcr_discontinuity_errors: pcr_disc,
            pcr_accuracy_errors: pcr_acc,
            window_cc_errors: w_cc,
            window_pat_errors: w_pat,
            window_pmt_errors: w_pmt,
            window_pid_errors: w_pid,
            window_tei_errors: w_tei,
            window_crc_errors: w_crc,
            window_pcr_discontinuity_errors: w_pcr_disc,
            window_pcr_accuracy_errors: w_pcr_acc,
            priority1_ok,
            priority2_ok,
            tr07_compliant: jpeg_xs_detected,
            jpeg_xs_pid,
        }
    }
}

// ── Media Analysis Accumulator ────────────────────────────────────────────

/// Internal state for media content analysis.
pub struct MediaAnalysisState {
    // Transport (set once from config)
    pub protocol: String,
    pub payload_format: String,
    pub fec_enabled: bool,
    pub fec_type: Option<String>,
    pub redundancy_enabled: bool,
    pub redundancy_type: Option<String>,
    // Parsed from stream — one entry per MPEG-TS program (PMT) found in the PAT.
    pub programs: Vec<ProgramState>,
    // Per-PID byte counters for bitrate estimation
    pub pid_bytes: HashMap<u16, u64>,
    pub last_bitrate_calc: Instant,
    pub pid_bitrates: HashMap<u16, u64>,
    pub total_bitrate_bps: u64,
}

/// Internal state for one MPEG-TS program (one PMT).
pub struct ProgramState {
    pub program_number: u16,
    pub pmt_pid: u16,
    pub last_pmt_version: Option<u8>,
    pub video_streams: Vec<VideoStreamState>,
    pub audio_streams: Vec<AudioStreamState>,
}

/// Internal state for a detected video stream.
pub struct VideoStreamState {
    pub pid: u16,
    pub codec: String,
    pub stream_type: u8,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub frame_rate: Option<f64>,
    pub profile: Option<String>,
    pub level: Option<String>,
    pub sps_detected: bool,
}

/// Internal state for a detected audio stream.
pub struct AudioStreamState {
    pub pid: u16,
    pub codec: String,
    pub stream_type: u8,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub language: Option<String>,
    pub header_detected: bool,
}

/// Media analysis accumulator. The single analyzer task writes to `state`;
/// the 1/sec snapshot path reads it briefly.
pub struct MediaAnalysisAccumulator {
    pub state: Mutex<MediaAnalysisState>,
}

impl MediaAnalysisAccumulator {
    pub fn new(
        protocol: String,
        payload_format: String,
        fec_enabled: bool,
        fec_type: Option<String>,
        redundancy_enabled: bool,
        redundancy_type: Option<String>,
    ) -> Self {
        Self {
            state: Mutex::new(MediaAnalysisState {
                protocol,
                payload_format,
                fec_enabled,
                fec_type,
                redundancy_enabled,
                redundancy_type,
                programs: Vec::new(),
                pid_bytes: HashMap::new(),
                last_bitrate_calc: Instant::now(),
                pid_bitrates: HashMap::new(),
                total_bitrate_bps: 0,
            }),
        }
    }

    /// Take a point-in-time snapshot for JSON serialisation.
    pub fn snapshot(&self) -> MediaAnalysisStats {
        let state = self.state.lock().unwrap();
        MediaAnalysisStats {
            protocol: state.protocol.clone(),
            payload_format: state.payload_format.clone(),
            fec: if state.fec_enabled {
                // Parse fec_type string for L/D params
                Some(FecInfo {
                    standard: "SMPTE 2022-1".to_string(),
                    columns: 0, // filled from config string
                    rows: 0,
                })
            } else {
                None
            }.or_else(|| {
                // Try to parse from fec_type
                state.fec_type.as_ref().map(|ft| {
                    // Format: "SMPTE 2022-1 (L=5, D=5)"
                    let mut cols = 0u8;
                    let mut rows = 0u8;
                    if let Some(l_start) = ft.find("L=") {
                        if let Some(end) = ft[l_start + 2..].find(|c: char| !c.is_ascii_digit()) {
                            cols = ft[l_start + 2..l_start + 2 + end].parse().unwrap_or(0);
                        }
                    }
                    if let Some(d_start) = ft.find("D=") {
                        if let Some(end) = ft[d_start + 2..].find(|c: char| !c.is_ascii_digit()) {
                            rows = ft[d_start + 2..d_start + 2 + end].parse().unwrap_or(0);
                        }
                    }
                    FecInfo { standard: "SMPTE 2022-1".to_string(), columns: cols, rows }
                })
            }),
            redundancy: if state.redundancy_enabled {
                Some(RedundancyInfo {
                    standard: state.redundancy_type.clone().unwrap_or_else(|| "SMPTE 2022-7".to_string()),
                })
            } else {
                None
            },
            program_count: state.programs.len() as u16,
            programs: state.programs.iter().map(|p| {
                let video_streams: Vec<VideoStreamInfo> = p.video_streams.iter().map(|v| {
                    let bitrate = state.pid_bitrates.get(&v.pid).copied().unwrap_or(0);
                    VideoStreamInfo {
                        pid: v.pid,
                        codec: v.codec.clone(),
                        stream_type: v.stream_type,
                        resolution: match (v.width, v.height) {
                            (Some(w), Some(h)) => Some(format!("{}x{}", w, h)),
                            _ => None,
                        },
                        frame_rate: v.frame_rate,
                        profile: v.profile.clone(),
                        level: v.level.clone(),
                        bitrate_bps: bitrate,
                    }
                }).collect();
                let audio_streams: Vec<AudioStreamInfo> = p.audio_streams.iter().map(|a| {
                    let bitrate = state.pid_bitrates.get(&a.pid).copied().unwrap_or(0);
                    AudioStreamInfo {
                        pid: a.pid,
                        codec: a.codec.clone(),
                        stream_type: a.stream_type,
                        sample_rate_hz: a.sample_rate_hz,
                        channels: a.channels,
                        language: a.language.clone(),
                        bitrate_bps: bitrate,
                    }
                }).collect();
                let total_bitrate_bps: u64 = video_streams.iter().map(|v| v.bitrate_bps).sum::<u64>()
                    + audio_streams.iter().map(|a| a.bitrate_bps).sum::<u64>();
                ProgramInfo {
                    program_number: p.program_number,
                    pmt_pid: p.pmt_pid,
                    video_streams,
                    audio_streams,
                    total_bitrate_bps,
                }
            }).collect(),
            // Backward-compat flat union of all programs' streams.
            video_streams: state.programs.iter().flat_map(|p| p.video_streams.iter()).map(|v| {
                let bitrate = state.pid_bitrates.get(&v.pid).copied().unwrap_or(0);
                VideoStreamInfo {
                    pid: v.pid,
                    codec: v.codec.clone(),
                    stream_type: v.stream_type,
                    resolution: match (v.width, v.height) {
                        (Some(w), Some(h)) => Some(format!("{}x{}", w, h)),
                        _ => None,
                    },
                    frame_rate: v.frame_rate,
                    profile: v.profile.clone(),
                    level: v.level.clone(),
                    bitrate_bps: bitrate,
                }
            }).collect(),
            audio_streams: state.programs.iter().flat_map(|p| p.audio_streams.iter()).map(|a| {
                let bitrate = state.pid_bitrates.get(&a.pid).copied().unwrap_or(0);
                AudioStreamInfo {
                    pid: a.pid,
                    codec: a.codec.clone(),
                    stream_type: a.stream_type,
                    sample_rate_hz: a.sample_rate_hz,
                    channels: a.channels,
                    language: a.language.clone(),
                    bitrate_bps: bitrate,
                }
            }).collect(),
            total_bitrate_bps: state.total_bitrate_bps,
        }
    }
}

// ── Thumbnail Accumulator ─────────────────────────────────────────────────

/// Number of consecutive identical thumbnail captures before raising a
/// "frozen" alarm. At 10 s per capture this corresponds to ~30 s.
const FREEZE_THRESHOLD: u64 = 3;

/// Thumbnail generation accumulator. The thumbnail task writes the latest
/// JPEG bytes; the 1/sec snapshot path reads the counters.
pub struct ThumbnailAccumulator {
    /// Latest captured JPEG thumbnail data and capture timestamp.
    pub latest_jpeg: Mutex<Option<(bytes::Bytes, Instant)>>,
    /// Monotonically increasing generation counter. Incremented each time a
    /// new thumbnail is captured, so consumers can detect changes.
    pub generation: AtomicU64,
    /// Total thumbnails successfully captured.
    pub total_captured: AtomicU64,
    /// Total capture errors (ffmpeg failures, timeouts).
    pub capture_errors: AtomicU64,
    /// Hash of the previously captured JPEG for freeze-frame comparison.
    prev_jpeg_hash: Mutex<Option<u64>>,
    /// How many consecutive captures produced an identical JPEG hash.
    freeze_count: AtomicU64,
    /// Current thumbnail alarm: `"black"`, `"frozen"`, or `None`.
    alarm: Mutex<Option<String>>,
}

impl ThumbnailAccumulator {
    pub fn new() -> Self {
        Self {
            latest_jpeg: Mutex::new(None),
            generation: AtomicU64::new(0),
            total_captured: AtomicU64::new(0),
            capture_errors: AtomicU64::new(0),
            prev_jpeg_hash: Mutex::new(None),
            freeze_count: AtomicU64::new(0),
            alarm: Mutex::new(None),
        }
    }

    /// Store a newly captured thumbnail.
    pub fn store(&self, jpeg_data: bytes::Bytes) {
        *self.latest_jpeg.lock().unwrap() = Some((jpeg_data, Instant::now()));
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.total_captured.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a capture error.
    pub fn record_error(&self) {
        self.capture_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Check whether the current JPEG hash matches the previous one and
    /// update the freeze counter accordingly. Returns `true` when the
    /// frame has been identical for [`FREEZE_THRESHOLD`] consecutive
    /// captures.
    pub fn check_freeze(&self, jpeg_hash: u64) -> bool {
        let mut prev = self.prev_jpeg_hash.lock().unwrap();
        if *prev == Some(jpeg_hash) {
            let count = self.freeze_count.fetch_add(1, Ordering::Relaxed) + 1;
            count >= FREEZE_THRESHOLD
        } else {
            *prev = Some(jpeg_hash);
            self.freeze_count.store(1, Ordering::Relaxed);
            false
        }
    }

    /// Set or clear the current thumbnail alarm.
    pub fn set_alarm(&self, value: Option<String>) {
        *self.alarm.lock().unwrap() = value;
    }

    /// Take a point-in-time snapshot for JSON serialisation.
    pub fn snapshot(&self) -> ThumbnailStats {
        let has_thumbnail = self.latest_jpeg.lock().unwrap().is_some();
        let alarm = self.alarm.lock().unwrap().clone();
        ThumbnailStats {
            enabled: true,
            total_captured: self.total_captured.load(Ordering::Relaxed),
            capture_errors: self.capture_errors.load(Ordering::Relaxed),
            has_thumbnail,
            alarm,
        }
    }
}

// ── Per-flow Accumulator ───────────────────────────────────────────────────

/// Per-flow atomic counters for a single media flow (one input, N outputs).
///
/// Holds input-side counters (`input_packets`, `input_bytes`, `input_loss`,
/// `fec_recovered`, `redundancy_switches`) as lock-free [`AtomicU64`] values,
/// plus a [`DashMap`] of per-output [`OutputStatsAccumulator`] instances keyed
/// by output ID. This allows each output's hot path to update its own counters
/// independently without contention on a shared lock.
pub struct FlowStatsAccumulator {
    pub flow_id: String,
    pub flow_name: String,
    pub input_type: String,
    pub started_at: Instant,
    // Input counters
    pub input_packets: AtomicU64,
    pub input_bytes: AtomicU64,
    pub input_loss: AtomicU64,
    pub input_filtered: AtomicU64,
    pub fec_recovered: AtomicU64,
    pub redundancy_switches: AtomicU64,
    // Per-output stats
    pub output_stats: DashMap<String, Arc<OutputStatsAccumulator>>,
    pub input_throughput: Mutex<ThroughputEstimator>,
    /// TR-101290 analyzer stats, set once when the flow starts.
    pub tr101290: OnceLock<Arc<Tr101290Accumulator>>,
    /// Media analysis stats, set once when the flow starts (if enabled).
    pub media_analysis: OnceLock<Arc<MediaAnalysisAccumulator>>,
    /// Thumbnail generation stats, set once when the flow starts (if enabled and ffmpeg available).
    pub thumbnail: OnceLock<Arc<ThumbnailAccumulator>>,
    /// Input config metadata for topology display (set once at flow start).
    pub input_config_meta: OnceLock<InputConfigMeta>,
    /// Per-output config metadata for topology display (set once per output).
    pub output_config_meta: DashMap<String, OutputConfigMeta>,
    /// Cached SRT stats for primary input leg, updated by the SRT input polling task
    /// via a lock-free watch channel.
    pub input_srt_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Cached SRT stats for redundancy input leg, updated by the SRT input polling task
    /// via a lock-free watch channel.
    pub input_srt_leg2_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Set to `true` by the bandwidth monitor when the input bitrate exceeds the configured limit.
    pub bandwidth_exceeded: AtomicBool,
    /// Set to `true` by the bandwidth monitor to gate the flow (block action).
    /// Input tasks check this flag and drop packets while it is set.
    pub bandwidth_blocked: AtomicBool,
    /// Configured bandwidth limit in Mbps (set once at flow start, for dashboard display).
    pub bandwidth_limit_mbps: OnceLock<f64>,
    /// PTP state handle for ST 2110 flows whose `clock_domain` is set.
    /// Populated by the input spawn helpers in `engine/st2110_io.rs`. The
    /// snapshot path reads it via `PtpStateHandle::snapshot()` and converts
    /// the result into the wire-shaped `PtpStateStats`.
    pub ptp_state: OnceLock<crate::engine::st2110::ptp::PtpStateHandle>,
    /// Per-leg packet/byte counters for SMPTE 2022-7 dual-network inputs.
    /// Populated by the input spawn helpers after the Red/Blue UDP sockets
    /// have been bound. Absent for non-ST-2110 flows and for ST 2110 flows
    /// without a `redundancy` config (Red-only).
    pub red_blue_stats: OnceLock<Arc<crate::engine::st2110::redblue::RedBlueStats>>,
}

/// Lightweight input config metadata for topology display.
#[derive(Debug, Clone)]
pub struct InputConfigMeta {
    pub mode: Option<String>,
    pub local_addr: Option<String>,
    pub remote_addr: Option<String>,
    pub listen_addr: Option<String>,
    pub bind_addr: Option<String>,
    pub rtsp_url: Option<String>,
    pub whep_url: Option<String>,
}

/// Lightweight output config metadata for topology display.
#[derive(Debug, Clone)]
pub struct OutputConfigMeta {
    pub mode: Option<String>,
    pub remote_addr: Option<String>,
    pub dest_addr: Option<String>,
    pub dest_url: Option<String>,
    pub ingest_url: Option<String>,
    pub whip_url: Option<String>,
    pub local_addr: Option<String>,
    /// Configured MPTS `program_number` filter, mirrored into `OutputStats` for
    /// the manager status view.
    pub program_number: Option<u16>,
}

impl FlowStatsAccumulator {
    /// Create a new accumulator with all counters initialised to zero.
    ///
    /// Records `Instant::now()` as the flow start time for uptime calculation.
    pub fn new(flow_id: String, flow_name: String, input_type: String) -> Self {
        Self {
            flow_id,
            flow_name,
            input_type,
            started_at: Instant::now(),
            input_packets: AtomicU64::new(0),
            input_bytes: AtomicU64::new(0),
            input_loss: AtomicU64::new(0),
            input_filtered: AtomicU64::new(0),
            fec_recovered: AtomicU64::new(0),
            redundancy_switches: AtomicU64::new(0),
            output_stats: DashMap::new(),
            input_throughput: Mutex::new(ThroughputEstimator::new()),
            tr101290: OnceLock::new(),
            media_analysis: OnceLock::new(),
            thumbnail: OnceLock::new(),
            input_config_meta: OnceLock::new(),
            output_config_meta: DashMap::new(),
            input_srt_stats_cache: Arc::new(watch::channel(None).0),
            input_srt_leg2_stats_cache: Arc::new(watch::channel(None).0),
            bandwidth_exceeded: AtomicBool::new(false),
            bandwidth_blocked: AtomicBool::new(false),
            bandwidth_limit_mbps: OnceLock::new(),
            ptp_state: OnceLock::new(),
            red_blue_stats: OnceLock::new(),
        }
    }

    /// Register a new output for this flow and return a shared reference to its
    /// [`OutputStatsAccumulator`]. The accumulator is inserted into the internal
    /// `DashMap` keyed by `output_id`.
    pub fn register_output(&self, output_id: String, output_name: String, output_type: String) -> Arc<OutputStatsAccumulator> {
        let acc = Arc::new(OutputStatsAccumulator::new(output_id.clone(), output_name, output_type));
        self.output_stats.insert(output_id, acc.clone());
        acc
    }

    /// Remove an output's accumulator and config metadata from this flow.
    pub fn unregister_output(&self, output_id: &str) {
        self.output_stats.remove(output_id);
        self.output_config_meta.remove(output_id);
    }

    /// Take a point-in-time snapshot of all input counters and every registered
    /// output's counters, assembling them into a [`FlowStats`] value.
    pub fn snapshot(&self) -> FlowStats {
        let outputs: Vec<OutputStats> = self
            .output_stats
            .iter()
            .map(|entry| {
                let mut snap = entry.value().snapshot();
                // Inject config metadata for topology display
                if let Some(meta) = self.output_config_meta.get(entry.key()) {
                    snap.mode = meta.mode.clone();
                    snap.remote_addr = meta.remote_addr.clone();
                    snap.dest_addr = meta.dest_addr.clone();
                    snap.dest_url = meta.dest_url.clone();
                    snap.ingest_url = meta.ingest_url.clone();
                    snap.whip_url = meta.whip_url.clone();
                    snap.local_addr = meta.local_addr.clone();
                    snap.program_number = meta.program_number;
                }
                snap
            })
            .collect();

        let input_bytes = self.input_bytes.load(Ordering::Relaxed);
        let input_bitrate = self.input_throughput.lock().unwrap().sample(input_bytes);

        let tr101290_snap = self.tr101290.get().map(|acc| acc.snapshot());

        // Extract IAT/PDV from the TR-101290 analyzer state
        let (iat, pdv_jitter_us) = self.tr101290.get()
            .map(|acc| {
                let state = acc.state.lock().unwrap();
                let iat = if state.iat_count > 0 {
                    Some(IatStats {
                        min_us: if state.iat_min_us == f64::MAX { 0.0 } else { state.iat_min_us },
                        max_us: state.iat_max_us,
                        avg_us: state.iat_sum_us / state.iat_count as f64,
                    })
                } else {
                    None
                };
                let pdv = if state.jitter_us > 0.0 { Some(state.jitter_us) } else { None };
                (iat, pdv)
            })
            .unwrap_or((None, None));

        let media_analysis = self.media_analysis.get().map(|acc| acc.snapshot());
        let thumbnail = self.thumbnail.get().map(|acc| acc.snapshot());

        // Derive flow health (RP 2129 M6)
        let packets_lost = self.input_loss.load(Ordering::Relaxed);
        let bw_exceeded = self.bandwidth_exceeded.load(Ordering::Relaxed);
        let bw_blocked = self.bandwidth_blocked.load(Ordering::Relaxed);
        let health = derive_flow_health(input_bitrate, packets_lost, &tr101290_snap, bw_exceeded, bw_blocked);

        FlowStats {
            flow_id: self.flow_id.clone(),
            flow_name: self.flow_name.clone(),
            state: FlowState::Running,
            input: {
                let meta = self.input_config_meta.get();
                InputStats {
                    input_type: self.input_type.clone(),
                    state: "receiving".to_string(),
                    mode: meta.and_then(|m| m.mode.clone()),
                    local_addr: meta.and_then(|m| m.local_addr.clone()),
                    remote_addr: meta.and_then(|m| m.remote_addr.clone()),
                    listen_addr: meta.and_then(|m| m.listen_addr.clone()),
                    bind_addr: meta.and_then(|m| m.bind_addr.clone()),
                    rtsp_url: meta.and_then(|m| m.rtsp_url.clone()),
                    whep_url: meta.and_then(|m| m.whep_url.clone()),
                    packets_received: self.input_packets.load(Ordering::Relaxed),
                    bytes_received: input_bytes,
                    bitrate_bps: input_bitrate,
                    packets_lost,
                    packets_filtered: self.input_filtered.load(Ordering::Relaxed),
                    packets_recovered_fec: self.fec_recovered.load(Ordering::Relaxed),
                    srt_stats: self.input_srt_stats_cache.borrow().clone(),
                    srt_leg2_stats: self.input_srt_leg2_stats_cache.borrow().clone(),
                    redundancy_switches: self.redundancy_switches.load(Ordering::Relaxed),
                }
            },
            outputs,
            uptime_secs: self.started_at.elapsed().as_secs(),
            tr101290: tr101290_snap,
            health,
            iat,
            pdv_jitter_us,
            media_analysis,
            thumbnail,
            bandwidth_exceeded: bw_exceeded,
            bandwidth_blocked: bw_blocked,
            bandwidth_limit_mbps: self.bandwidth_limit_mbps.get().copied(),
            // ST 2110 / NMOS optional fields. Populated for ST 2110 flows
            // whose input spawn helpers have stored a PtpStateHandle and/or a
            // RedBlueStats Arc on this accumulator. Non-ST-2110 flows leave
            // these as `None` so the JSON shape stays unchanged.
            ptp_state: self.ptp_state.get().map(ptp_state_to_stats),
            network_legs: self
                .red_blue_stats
                .get()
                .map(|s| red_blue_to_stats(&s.snapshot())),
            essence_flows: None,
        }
    }
}

/// Convert a live `engine::st2110::ptp::PtpState` into the wire-shaped
/// `PtpStateStats` carried by `FlowStats`. Pure mapping — no I/O, no locks.
fn ptp_state_to_stats(handle: &crate::engine::st2110::ptp::PtpStateHandle) -> PtpStateStats {
    use crate::engine::st2110::ptp::PtpLockState;
    let s = handle.snapshot();
    let lock_state = match s.lock_state {
        PtpLockState::Locked => "locked",
        PtpLockState::Holdover => "holdover",
        PtpLockState::Acquiring => "acquiring",
        PtpLockState::Master => "master",
        PtpLockState::Unknown => "unknown",
        PtpLockState::Unavailable => "unavailable",
    }
    .to_string();
    PtpStateStats {
        lock_state,
        domain: Some(s.domain),
        grandmaster_id: s.grandmaster_id.map(|gm| gm.to_string()),
        offset_ns: s.offset_ns,
        mean_path_delay_ns: s.mean_path_delay_ns,
        steps_removed: s.steps_removed,
        last_update_ms: s.last_update_unix_ms.map(|v| v as u64),
    }
}

/// Convert a `RedBlueStatsSnapshot` into the wire-shaped `NetworkLegsStats`.
fn red_blue_to_stats(
    snap: &crate::engine::st2110::redblue::RedBlueStatsSnapshot,
) -> NetworkLegsStats {
    NetworkLegsStats {
        red: LegCounters {
            packets_received: snap.red.packets_received,
            bytes_received: snap.red.bytes_received,
            packets_forwarded: snap.red.packets_forwarded,
            packets_duplicate: snap.red.packets_duplicate,
        },
        blue: LegCounters {
            packets_received: snap.blue.packets_received,
            bytes_received: snap.blue.bytes_received,
            packets_forwarded: snap.blue.packets_forwarded,
            packets_duplicate: snap.blue.packets_duplicate,
        },
        leg_switches: snap.leg_switches,
    }
}

/// Derive flow health from available metrics (RP 2129 M6).
/// Called during the 1/sec snapshot — zero hot-path impact.
fn derive_flow_health(
    bitrate_bps: u64,
    packets_lost: u64,
    tr101290: &Option<Tr101290Stats>,
    bandwidth_exceeded: bool,
    bandwidth_blocked: bool,
) -> FlowHealth {
    if let Some(tr) = tr101290 {
        // Critical: sync loss or sustained errors
        if tr.sync_loss_count > 0 {
            return FlowHealth::Critical;
        }
        // Error: P1 errors (CC, PAT, PMT)
        if !tr.priority1_ok {
            return FlowHealth::Error;
        }
    }

    // Critical: no data flowing
    if bitrate_bps == 0 && !bandwidth_blocked {
        return FlowHealth::Critical;
    }

    // Error: flow is actively blocked by bandwidth enforcement
    if bandwidth_blocked {
        return FlowHealth::Error;
    }

    // Error: significant packet loss
    if packets_lost > 100 {
        return FlowHealth::Error;
    }

    // Warning: bandwidth exceeded (alarm mode)
    if bandwidth_exceeded {
        return FlowHealth::Warning;
    }

    // Warning: P2 errors or minor loss
    if let Some(tr) = tr101290 {
        if !tr.priority2_ok {
            return FlowHealth::Warning;
        }
    }
    if packets_lost > 0 {
        return FlowHealth::Warning;
    }

    FlowHealth::Healthy
}

/// Global statistics registry that holds all flow stats accumulators.
///
/// Backed by a [`DashMap<String, Arc<FlowStatsAccumulator>>`], keyed by
/// flow ID. Engine tasks register/unregister flows at start-up and
/// shutdown. The REST API reads snapshots via [`Self::all_snapshots`] or
/// [`Self::flow_snapshot`] without blocking the data plane.
pub struct StatsCollector {
    pub flow_stats: DashMap<String, Arc<FlowStatsAccumulator>>,
}

impl StatsCollector {
    /// Create an empty stats collector.
    pub fn new() -> Self {
        Self {
            flow_stats: DashMap::new(),
        }
    }

    /// Register a new flow and return a shared reference to its
    /// [`FlowStatsAccumulator`]. Inserts the accumulator into the global
    /// `DashMap` keyed by `flow_id`.
    pub fn register_flow(&self, flow_id: String, flow_name: String, input_type: String) -> Arc<FlowStatsAccumulator> {
        let acc = Arc::new(FlowStatsAccumulator::new(flow_id.clone(), flow_name, input_type));
        self.flow_stats.insert(flow_id, acc.clone());
        acc
    }

    /// Remove a flow's accumulator from the global registry.
    pub fn unregister_flow(&self, flow_id: &str) {
        self.flow_stats.remove(flow_id);
    }

    /// Snapshot every registered flow and return a `Vec` of [`FlowStats`].
    pub fn all_snapshots(&self) -> Vec<FlowStats> {
        self.flow_stats
            .iter()
            .map(|entry| entry.value().snapshot())
            .collect()
    }

    /// Snapshot a single flow by ID. Returns `None` if the flow is not registered.
    pub fn flow_snapshot(&self, flow_id: &str) -> Option<FlowStats> {
        self.flow_stats.get(flow_id).map(|entry| entry.snapshot())
    }
}
