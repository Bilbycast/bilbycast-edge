// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::watch;

use super::models::*;
use super::throughput::ThroughputEstimator;

// Note: `Mutex` is still imported for `Tr101290State`, `MediaAnalysisState`,
// and `ThumbnailAccumulator` (non-hot-path, 1/sec snapshot reads).
// `ThroughputEstimator` is now fully lock-free (atomic-based).

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
    throughput: ThroughputEstimator,
    /// Cached SRT stats for primary leg, updated by the SRT output polling task
    /// via a lock-free watch channel. Read via `borrow()`, write via `send()`.
    pub srt_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Cached SRT stats for redundancy leg, updated by the SRT output polling task
    /// via a lock-free watch channel.
    pub srt_leg2_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Cached native-libsrt bonding stats (aggregate + per-member) for an
    /// SRT output whose config has a `bonding` block. Lock-free watch
    /// channel, populated by `spawn_srt_group_stats_poller`.
    pub srt_bonding_stats_cache: Arc<watch::Sender<Option<crate::stats::models::SrtBondingStats>>>,
    /// Shared RIST connection-level counters for the primary output leg.
    /// Stored as an `Arc` handle because the `RistSocket` sender task owns
    /// and mutates the counters directly — snapshotting is a cheap atomic
    /// load.
    rist_stats_handle: OnceLock<Arc<rist_transport::RistConnStats>>,
    /// Shared RIST counters for the SMPTE 2022-7 leg 2 output (when redundancy
    /// is configured).
    rist_leg2_stats_handle: OnceLock<Arc<rist_transport::RistConnStats>>,
    /// Shared bond stats handle — aggregate `BondConnStats` plus
    /// per-path `PathStats`. Populated by
    /// `engine::output_bonded::spawn_bonded_output` after the
    /// `BondSocket` is built.
    bond_stats_handle: OnceLock<BondStatsHandle>,
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
    /// Optional handle to the per-output video encode counters plus the
    /// resolved target codec / geometry descriptors. Set once at output
    /// startup by outputs that build an
    /// `engine::ts_video_replace::TsVideoReplacer`.
    video_encode_stats: OnceLock<VideoEncodeStatsHandle>,
    /// Optional handle to the per-output local-display task's counters +
    /// chosen-mode descriptors. Set once at output startup by
    /// `engine::output_display`. Read by the snapshot path to populate
    /// [`crate::stats::models::DisplayStats`].
    display_stats: OnceLock<DisplayStatsHandle>,
    /// Optional snapshot-time descriptors used to build
    /// [`crate::stats::models::EgressMediaSummary`]. Set once at output
    /// startup with everything that doesn't change at runtime — the dynamic
    /// passthrough fields are merged in at `FlowStatsAccumulator::snapshot()`.
    egress_static: OnceLock<EgressMediaSummaryStatic>,

    // ── End-to-end latency tracking ──────────────────────────────────
    // Windowed min/avg/max, reset on each snapshot (1s).
    latency_min_us: AtomicU64,
    latency_max_us: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_count: AtomicU64,

    // ── PID-bus Phase 8: per-output PCR accuracy trust metric ────────
    // Fed inline by the output's send task whenever it forwards a
    // PCR-bearing TS packet. Cheap — one Mutex + VecDeque push per PCR
    // (~25/sec typical). Lifetime-cumulative percentiles across a
    // rotating reservoir (last 4096 samples).
    pcr_trust: crate::stats::pcr_trust::PcrTrustSampler,
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

/// Registered handle to a per-output video encode stage's counters plus the
/// resolved target codec / geometry descriptors. The atomic counters live in
/// `crate::engine::ts_video_replace::VideoEncodeStats`; everything else is
/// fixed at output startup.
pub struct VideoEncodeStatsHandle {
    pub stats: Arc<crate::engine::ts_video_replace::VideoEncodeStats>,
    pub input_codec: String,
    pub output_codec: String,
    pub output_width: u32,
    pub output_height: u32,
    pub output_fps: f32,
    pub output_bitrate_kbps: u32,
    pub encoder_backend: String,
}

/// Lock-free counters that back [`crate::stats::models::DisplayStats`].
/// Owned by `engine::output_display`; the accumulator stores an `Arc`
/// to it so the snapshot path can sample without locking.
///
/// The constructors are only called under
/// `cfg(all(feature = "display", target_os = "linux"))`; allow dead
/// code so a feature-off `cargo build` on macOS stays clean.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct DisplayStatsCounters {
    pub frames_displayed: AtomicU64,
    pub frames_dropped_late: AtomicU64,
    pub frames_repeated: AtomicU64,
    pub audio_underruns: AtomicU64,
    /// `i32` stored in an `AtomicI32`-shaped `AtomicU64` cast — the
    /// snapshot path round-trips it via `to_be_bytes` to preserve the
    /// sign without needing AtomicI32 (which `Arc<...>` can't always
    /// give us across stable Rust versions cleanly). The display task
    /// writes via `store_av_offset_ms`; readers use `load_av_offset_ms`.
    pub av_offset_ms_packed: AtomicU64,
}

#[allow(dead_code)]
impl DisplayStatsCounters {
    pub fn store_av_offset_ms(&self, ms: i32) {
        self.av_offset_ms_packed
            .store(ms as i64 as u64, Ordering::Relaxed);
    }
    pub fn load_av_offset_ms(&self) -> i32 {
        self.av_offset_ms_packed.load(Ordering::Relaxed) as i64 as i32
    }
}

/// Registered handle to a per-output local-display task's counters plus
/// the steady-state descriptors (chosen mode, codec families) the
/// snapshot path needs to populate
/// [`crate::stats::models::DisplayStats`]. Set once at output startup.
pub struct DisplayStatsHandle {
    pub counters: Arc<DisplayStatsCounters>,
    pub current_resolution: String,
    pub current_refresh_hz: u32,
    pub pixel_format: String,
    pub decoder_kind: String,
    pub video_codec: String,
    pub audio_codec: String,
}

/// Static (set-once-at-output-startup) descriptors used to build the egress
/// media summary in the per-flow snapshot path. Anything that depends on live
/// stats (encode bitrate, decode codec, etc.) is layered in by the snapshot
/// path itself, so this struct only carries values that don't change while
/// the output is running.
#[derive(Debug, Clone, Default)]
pub struct EgressMediaSummaryStatic {
    /// `"ts"`, `"rtp"`, `"audio_302m"`, `"st2110-30"`, `"st2110-31"`,
    /// `"st2110-40"`, `"flv"`, `"hls"`, `"webrtc"`.
    pub transport_mode: Option<String>,
    /// `true` when the output forwards the input video unchanged (no
    /// `video_encode` block, no decode-and-remux).
    pub video_passthrough: bool,
    /// `true` when the output forwards the input audio unchanged (no
    /// `audio_encode`, no `audio_decode`, no PCM `transcode`).
    pub audio_passthrough: bool,
    /// `true` when this output produces no video essence (audio-only outputs).
    pub audio_only: bool,
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
            throughput: ThroughputEstimator::new(),
            srt_stats_cache: Arc::new(watch::channel(None).0),
            srt_leg2_stats_cache: Arc::new(watch::channel(None).0),
            srt_bonding_stats_cache: Arc::new(watch::channel(None).0),
            rist_stats_handle: OnceLock::new(),
            rist_leg2_stats_handle: OnceLock::new(),
            bond_stats_handle: OnceLock::new(),
            transcode_stats: OnceLock::new(),
            audio_decode_stats: OnceLock::new(),
            audio_encode_stats: OnceLock::new(),
            video_encode_stats: OnceLock::new(),
            display_stats: OnceLock::new(),
            egress_static: OnceLock::new(),
            latency_min_us: AtomicU64::new(u64::MAX),
            latency_max_us: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            pcr_trust: crate::stats::pcr_trust::PcrTrustSampler::new(),
        }
    }

    /// Record one PCR observation at egress. Called inline by the output's
    /// send task for every PCR-bearing TS packet it forwards. `pcr_27mhz`
    /// is reconstructed from the adaptation field; `now_us` is the
    /// monotonic wall clock at forward time.
    ///
    /// PID-bus Phase 8: this is the sampling point for the per-output
    /// PCR accuracy trust metric. See `stats::pcr_trust` for the
    /// reservoir design.
    #[inline]
    pub fn record_pcr_egress(&self, pcr_27mhz: u64, now_us: u64) {
        self.pcr_trust.record(pcr_27mhz, now_us);
    }

    /// Borrow the PCR trust sampler — used by the flow-level snapshot
    /// path to compute `FlowStats.pcr_trust_flow` as a roll-up across
    /// all outputs.
    pub fn pcr_trust_sampler(&self) -> &crate::stats::pcr_trust::PcrTrustSampler {
        &self.pcr_trust
    }

    /// Register the per-output transcoder stats handle. Called once at
    /// output startup. Subsequent calls are no-ops (first wins).
    pub fn set_transcode_stats(
        &self,
        stats: Arc<crate::engine::audio_transcode::TranscodeStats>,
    ) {
        let _ = self.transcode_stats.set(stats);
    }

    /// Register the primary RIST socket's shared stats handle. Called once
    /// after the `RistSocket` is built for this output. Subsequent calls are
    /// no-ops so a later `set_rist_leg2_stats` never overwrites leg 1.
    pub fn set_rist_stats(&self, stats: Arc<rist_transport::RistConnStats>) {
        let _ = self.rist_stats_handle.set(stats);
    }

    /// Register the SMPTE 2022-7 second-leg RIST socket's stats handle.
    pub fn set_rist_leg2_stats(&self, stats: Arc<rist_transport::RistConnStats>) {
        let _ = self.rist_leg2_stats_handle.set(stats);
    }

    /// Register the bond stats handle for a bonded output. Called
    /// once after the `BondSocket::sender` is built. Subsequent
    /// calls are no-ops (first wins).
    pub fn set_bond_stats(&self, handle: BondStatsHandle) {
        let _ = self.bond_stats_handle.set(handle);
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

    /// Register the per-output video encode stats handle. Called once at
    /// output startup by outputs that build a
    /// [`crate::engine::ts_video_replace::TsVideoReplacer`]. Subsequent calls
    /// are no-ops (first wins).
    #[allow(clippy::too_many_arguments)]
    pub fn set_video_encode_stats(
        &self,
        stats: Arc<crate::engine::ts_video_replace::VideoEncodeStats>,
        input_codec: impl Into<String>,
        output_codec: impl Into<String>,
        output_width: u32,
        output_height: u32,
        output_fps: f32,
        output_bitrate_kbps: u32,
        encoder_backend: impl Into<String>,
    ) {
        let _ = self.video_encode_stats.set(VideoEncodeStatsHandle {
            stats,
            input_codec: input_codec.into(),
            output_codec: output_codec.into(),
            output_width,
            output_height,
            output_fps,
            output_bitrate_kbps,
            encoder_backend: encoder_backend.into(),
        });
    }

    /// Register the per-output local-display counters + mode descriptors.
    /// Called once at startup by `engine::output_display::run` after
    /// modeset succeeds. Subsequent calls are no-ops (first wins).
    #[allow(clippy::too_many_arguments, dead_code)]
    pub fn set_display_stats(
        &self,
        counters: Arc<DisplayStatsCounters>,
        current_resolution: impl Into<String>,
        current_refresh_hz: u32,
        pixel_format: impl Into<String>,
        decoder_kind: impl Into<String>,
        video_codec: impl Into<String>,
        audio_codec: impl Into<String>,
    ) {
        let _ = self.display_stats.set(DisplayStatsHandle {
            counters,
            current_resolution: current_resolution.into(),
            current_refresh_hz,
            pixel_format: pixel_format.into(),
            decoder_kind: decoder_kind.into(),
            video_codec: video_codec.into(),
            audio_codec: audio_codec.into(),
        });
    }

    /// Borrow the registered display-stats handle (used by the snapshot
    /// path to populate `OutputStats.display_stats`).
    #[allow(dead_code)]
    pub fn display_stats_handle(&self) -> Option<&DisplayStatsHandle> {
        self.display_stats.get()
    }

    /// Register the static portion of this output's egress media summary.
    /// Called once at output startup with values that don't change at
    /// runtime; the dynamic codec/format fields are merged in at snapshot
    /// time by [`FlowStatsAccumulator::snapshot`].
    pub fn set_egress_static(&self, descriptor: EgressMediaSummaryStatic) {
        let _ = self.egress_static.set(descriptor);
    }

    /// Borrow the static egress descriptor (used by the per-flow snapshot
    /// path to build [`crate::stats::models::EgressMediaSummary`]).
    pub fn egress_static(&self) -> Option<&EgressMediaSummaryStatic> {
        self.egress_static.get()
    }


    /// Record an end-to-end latency sample. Called on the hot path after
    /// each successful output send. `recv_time_us` is the monotonic receive
    /// time stamped on the packet at the flow's input.
    #[inline]
    pub fn record_latency(&self, recv_time_us: u64) {
        let now = crate::util::time::now_us();
        let latency = now.saturating_sub(recv_time_us);

        // Update min (CAS loop — converges fast, few updates after first packets)
        let mut cur = self.latency_min_us.load(Ordering::Relaxed);
        while latency < cur {
            match self.latency_min_us.compare_exchange_weak(
                cur,
                latency,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }

        // Update max (CAS loop)
        let mut cur = self.latency_max_us.load(Ordering::Relaxed);
        while latency > cur {
            match self.latency_max_us.compare_exchange_weak(
                cur,
                latency,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }

        self.latency_sum_us.fetch_add(latency, Ordering::Relaxed);
        self.latency_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a point-in-time snapshot of all atomic counters and return an
    /// [`OutputStats`] value suitable for JSON serialisation.
    pub fn snapshot(&self) -> OutputStats {
        let bytes = self.bytes_sent.load(Ordering::Relaxed);
        let bitrate_bps = self.throughput.sample(bytes);
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
        let video_encode_stats = self.video_encode_stats.get().map(|h| {
            crate::stats::models::VideoEncodeStatsSnapshot {
                input_frames: h.stats.input_frames.load(Ordering::Relaxed),
                output_frames: h.stats.output_frames.load(Ordering::Relaxed),
                dropped_frames: h.stats.dropped_frames.load(Ordering::Relaxed),
                input_codec: h.input_codec.clone(),
                output_codec: h.output_codec.clone(),
                output_width: h.output_width,
                output_height: h.output_height,
                output_fps: h.output_fps,
                output_bitrate_kbps: h.output_bitrate_kbps,
                encoder_backend: h.encoder_backend.clone(),
                last_latency_us: h.stats.last_latency_us.load(Ordering::Relaxed),
                supervisor_restarts: h.stats.supervisor_restarts.load(Ordering::Relaxed),
            }
        });
        let display_stats = self.display_stats.get().map(|h| crate::stats::models::DisplayStats {
            frames_displayed: h.counters.frames_displayed.load(Ordering::Relaxed),
            frames_dropped_late: h.counters.frames_dropped_late.load(Ordering::Relaxed),
            frames_repeated: h.counters.frames_repeated.load(Ordering::Relaxed),
            audio_underruns: h.counters.audio_underruns.load(Ordering::Relaxed),
            av_sync_offset_ms: h.counters.load_av_offset_ms(),
            current_resolution: h.current_resolution.clone(),
            current_refresh_hz: h.current_refresh_hz,
            pixel_format: h.pixel_format.clone(),
            decoder_kind: h.decoder_kind.clone(),
            video_codec: h.video_codec.clone(),
            audio_codec: h.audio_codec.clone(),
        });

        // Swap latency window and compute min/avg/max.
        let lat_count = self.latency_count.swap(0, Ordering::Relaxed);
        let latency = if lat_count > 0 {
            let lat_min = self.latency_min_us.swap(u64::MAX, Ordering::Relaxed);
            let lat_max = self.latency_max_us.swap(0, Ordering::Relaxed);
            let lat_sum = self.latency_sum_us.swap(0, Ordering::Relaxed);
            Some(OutputLatencyStats {
                min_us: lat_min,
                avg_us: lat_sum / lat_count,
                max_us: lat_max,
                latency_frames: None, // injected by FlowStatsAccumulator::snapshot()
            })
        } else {
            // Reset min even if no samples (keeps it primed for the next window).
            self.latency_min_us.store(u64::MAX, Ordering::Relaxed);
            None
        };

        let packets_sent = self.packets_sent.load(Ordering::Relaxed);
        let packets_dropped = self.packets_dropped.load(Ordering::Relaxed);

        OutputStats {
            output_id: self.output_id.clone(),
            output_name: self.output_name.clone(),
            output_type: self.output_type.clone(),
            state: derive_output_state(bitrate_bps, packets_sent, packets_dropped),
            mode: None,
            remote_addr: None,
            dest_addr: None,
            dest_url: None,
            ingest_url: None,
            whip_url: None,
            local_addr: None,
            program_number: None,
            packets_sent,
            bytes_sent: bytes,
            bitrate_bps,
            packets_dropped,
            fec_packets_sent: self.fec_packets_sent.load(Ordering::Relaxed),
            srt_stats: self.srt_stats_cache.borrow().clone(),
            srt_leg2_stats: self.srt_leg2_stats_cache.borrow().clone(),
            srt_bonding_stats: self.srt_bonding_stats_cache.borrow().clone(),
            rist_stats: self
                .rist_stats_handle
                .get()
                .map(|h| rist_conn_to_leg_stats(h.as_ref(), packets_sent > 0)),
            rist_leg2_stats: self
                .rist_leg2_stats_handle
                .get()
                .map(|h| rist_conn_to_leg_stats(h.as_ref(), packets_sent > 0)),
            bond_stats: self
                .bond_stats_handle
                .get()
                .map(bond_handle_to_leg_stats),
            transcode_stats,
            audio_decode_stats,
            audio_encode_stats,
            video_encode_stats,
            latency,
            // Filled in by `FlowStatsAccumulator::snapshot()` so it can merge
            // the flow's input MediaAnalysis with this output's per-stage
            // stats. Leaving as None here keeps the per-output snapshot
            // self-contained.
            egress_summary: None,
            pcr_trust: self.pcr_trust.snapshot(),
            display_stats,
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
    // ── TR 101 290 P2-extended / P3 SI-table tracking ──
    // The "ever observed" trick mirrors `pat_seen` so a contribution feed
    // missing optional SI tables doesn't paint the dashboard amber. Once
    // the table is seen, repetition timeouts start counting.
    pub cat_seen: bool,
    pub last_cat_time: Option<Instant>,
    pub sdt_seen: bool,
    pub last_sdt_time: Option<Instant>,
    pub nit_seen: bool,
    pub last_nit_time: Option<Instant>,
    pub eit_seen: bool,
    pub last_eit_time: Option<Instant>,
    pub tdt_seen: bool,
    pub last_tdt_time: Option<Instant>,
    pub rst_seen: bool,
    pub last_rst_time: Option<Instant>,
    /// PIDs that were seen on the wire but do not appear in any PMT, the
    /// PAT, the NIT slot, or the reserved 0x1FFF null. Population is
    /// lazy — once a PID is observed and not classified, it lands here.
    pub unreferenced_pids: HashMap<u16, ()>,
    /// Last PTS observed per PES PID and the wallclock at observation.
    /// Used for `pts_error` (no PTS within 700 ms when video/audio carry
    /// PES with a PTS field).
    pub pts_tracker: HashMap<u16, (u64, Instant)>,
    /// Last PCR observation wallclock per PCR-bearing PID. Splitting
    /// repetition (no PCR within 100 ms) from discontinuity (PCR jump
    /// > 100 ms or backwards) — both used to live in
    /// `pcr_discontinuity_errors`. Repetition reads via this map; the
    /// existing `pcr_tracker` keeps the value-vs-wall comparison.
    pub pcr_repetition_tracker: HashMap<u16, Instant>,
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
            cat_seen: false,
            last_cat_time: None,
            sdt_seen: false,
            last_sdt_time: None,
            nit_seen: false,
            last_nit_time: None,
            eit_seen: false,
            last_eit_time: None,
            tdt_seen: false,
            last_tdt_time: None,
            rst_seen: false,
            last_rst_time: None,
            unreferenced_pids: HashMap::new(),
            pts_tracker: HashMap::new(),
            pcr_repetition_tracker: HashMap::new(),
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
    // Priority 2 extended (TR 101 290 §5.2 — PTS / CAT / PCR repetition split)
    pub pts_errors: AtomicU64,
    pub cat_errors: AtomicU64,
    pub pcr_repetition_errors: AtomicU64,
    // Priority 3 (TR 101 290 §5.3 — application-specific)
    pub nit_errors: AtomicU64,
    pub si_repetition_errors: AtomicU64,
    pub unreferenced_pid_errors: AtomicU64,
    pub sdt_errors: AtomicU64,
    pub eit_errors: AtomicU64,
    pub rst_errors: AtomicU64,
    pub tdt_errors: AtomicU64,

    // ── Windowed counters (reset each snapshot, "errors since last report") ──
    pub window_cc_errors: AtomicU64,
    pub window_pat_errors: AtomicU64,
    pub window_pmt_errors: AtomicU64,
    pub window_pid_errors: AtomicU64,
    pub window_tei_errors: AtomicU64,
    pub window_crc_errors: AtomicU64,
    pub window_pcr_discontinuity_errors: AtomicU64,
    pub window_pcr_accuracy_errors: AtomicU64,
    pub window_pts_errors: AtomicU64,
    pub window_cat_errors: AtomicU64,
    pub window_pcr_repetition_errors: AtomicU64,
    pub window_nit_errors: AtomicU64,
    pub window_si_repetition_errors: AtomicU64,
    pub window_unreferenced_pid_errors: AtomicU64,
    pub window_sdt_errors: AtomicU64,
    pub window_eit_errors: AtomicU64,
    pub window_rst_errors: AtomicU64,
    pub window_tdt_errors: AtomicU64,

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
            pts_errors: AtomicU64::new(0),
            cat_errors: AtomicU64::new(0),
            pcr_repetition_errors: AtomicU64::new(0),
            nit_errors: AtomicU64::new(0),
            si_repetition_errors: AtomicU64::new(0),
            unreferenced_pid_errors: AtomicU64::new(0),
            sdt_errors: AtomicU64::new(0),
            eit_errors: AtomicU64::new(0),
            rst_errors: AtomicU64::new(0),
            tdt_errors: AtomicU64::new(0),
            window_cc_errors: AtomicU64::new(0),
            window_pat_errors: AtomicU64::new(0),
            window_pmt_errors: AtomicU64::new(0),
            window_pid_errors: AtomicU64::new(0),
            window_tei_errors: AtomicU64::new(0),
            window_crc_errors: AtomicU64::new(0),
            window_pcr_discontinuity_errors: AtomicU64::new(0),
            window_pcr_accuracy_errors: AtomicU64::new(0),
            window_pts_errors: AtomicU64::new(0),
            window_cat_errors: AtomicU64::new(0),
            window_pcr_repetition_errors: AtomicU64::new(0),
            window_nit_errors: AtomicU64::new(0),
            window_si_repetition_errors: AtomicU64::new(0),
            window_unreferenced_pid_errors: AtomicU64::new(0),
            window_sdt_errors: AtomicU64::new(0),
            window_eit_errors: AtomicU64::new(0),
            window_rst_errors: AtomicU64::new(0),
            window_tdt_errors: AtomicU64::new(0),
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

        // P2-extended + P3 cumulative
        let pts = self.pts_errors.load(Ordering::Relaxed);
        let cat = self.cat_errors.load(Ordering::Relaxed);
        let pcr_rep = self.pcr_repetition_errors.load(Ordering::Relaxed);
        let nit = self.nit_errors.load(Ordering::Relaxed);
        let si_rep = self.si_repetition_errors.load(Ordering::Relaxed);
        let unref_pid = self.unreferenced_pid_errors.load(Ordering::Relaxed);
        let sdt = self.sdt_errors.load(Ordering::Relaxed);
        let eit = self.eit_errors.load(Ordering::Relaxed);
        let rst = self.rst_errors.load(Ordering::Relaxed);
        let tdt = self.tdt_errors.load(Ordering::Relaxed);

        // Windowed counters — swap to zero atomically
        let w_cc = self.window_cc_errors.swap(0, Ordering::Relaxed);
        let w_pat = self.window_pat_errors.swap(0, Ordering::Relaxed);
        let w_pmt = self.window_pmt_errors.swap(0, Ordering::Relaxed);
        let w_pid = self.window_pid_errors.swap(0, Ordering::Relaxed);
        let w_tei = self.window_tei_errors.swap(0, Ordering::Relaxed);
        let w_crc = self.window_crc_errors.swap(0, Ordering::Relaxed);
        let w_pcr_disc = self.window_pcr_discontinuity_errors.swap(0, Ordering::Relaxed);
        let w_pcr_acc = self.window_pcr_accuracy_errors.swap(0, Ordering::Relaxed);
        let w_pts = self.window_pts_errors.swap(0, Ordering::Relaxed);
        let w_cat = self.window_cat_errors.swap(0, Ordering::Relaxed);
        let w_pcr_rep = self.window_pcr_repetition_errors.swap(0, Ordering::Relaxed);
        let w_nit = self.window_nit_errors.swap(0, Ordering::Relaxed);
        let w_si_rep = self.window_si_repetition_errors.swap(0, Ordering::Relaxed);
        let w_unref = self.window_unreferenced_pid_errors.swap(0, Ordering::Relaxed);
        let w_sdt = self.window_sdt_errors.swap(0, Ordering::Relaxed);
        let w_eit = self.window_eit_errors.swap(0, Ordering::Relaxed);
        let w_rst = self.window_rst_errors.swap(0, Ordering::Relaxed);
        let w_tdt = self.window_tdt_errors.swap(0, Ordering::Relaxed);

        // Priority flags based on windowed counters (current health, not historical)
        let in_sync = { self.state.lock().unwrap().in_sync };
        let priority1_ok = in_sync && w_cc == 0 && w_pat == 0 && w_pmt == 0 && w_pid == 0;
        let priority2_ok = w_tei == 0
            && w_crc == 0
            && w_pcr_disc == 0
            && w_pcr_acc == 0
            && w_pts == 0
            && w_cat == 0
            && w_pcr_rep == 0;
        let priority3_ok = Some(
            w_nit == 0
                && w_si_rep == 0
                && w_unref == 0
                && w_sdt == 0
                && w_eit == 0
                && w_rst == 0
                && w_tdt == 0,
        );

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
            priority3_ok,
            pts_errors: pts,
            cat_errors: cat,
            pcr_repetition_errors: pcr_rep,
            nit_errors: nit,
            si_repetition_errors: si_rep,
            unreferenced_pid_errors: unref_pid,
            sdt_errors: sdt,
            eit_errors: eit,
            rst_errors: rst,
            tdt_errors: tdt,
            window_pts_errors: w_pts,
            window_cat_errors: w_cat,
            window_pcr_repetition_errors: w_pcr_rep,
            window_nit_errors: w_nit,
            window_si_repetition_errors: w_si_rep,
            window_unreferenced_pid_errors: w_unref,
            window_sdt_errors: w_sdt,
            window_eit_errors: w_eit,
            window_rst_errors: w_rst,
            window_tdt_errors: w_tdt,
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
    // PTS-based frame rate detection fallback (when VUI timing is absent)
    pub last_pts: Option<u64>,
    pub pts_frame_count: u32,
    pub pts_interval_sum: u64,
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

    /// Reset all accumulated state. Called when the flow's active input
    /// switches so the analyzer starts fresh for the new input.
    pub fn reset_state(&self) {
        let mut state = self.state.lock().unwrap();
        state.programs.clear();
        state.pid_bytes.clear();
        state.pid_bitrates.clear();
        state.total_bitrate_bps = 0;
        state.last_bitrate_calc = Instant::now();
        state.protocol = String::new();
        state.payload_format = String::new();
        state.fec_enabled = false;
        state.fec_type = None;
        state.redundancy_enabled = false;
        state.redundancy_type = None;
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
/// "frozen" alarm. At 5 s per capture this corresponds to ~30 s — tuned
/// for fast operator feedback on flow changes.
const FREEZE_THRESHOLD: u64 = 6;

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
    /// External trigger for out-of-cycle captures. The per-input thumbnail
    /// loop awaits this in addition to its 5s interval so events like "this
    /// input just became active via a switch" can force an immediate
    /// capture rather than waiting up to a full interval. The loop enforces
    /// its own cooldown so floods are harmless.
    pub refresh_trigger: Arc<tokio::sync::Notify>,
    /// Shared notifier fired from `store()`. Cloned from the collector-wide
    /// `thumbnail_update_notify` at construction time so the manager WS
    /// client wakes and forwards the new JPEG immediately — without this
    /// hook the WS send loop polls and can sit up to 5s on a stale frame
    /// after an input switch or signal-state change.
    update_notify: Option<Arc<tokio::sync::Notify>>,
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
            refresh_trigger: Arc::new(tokio::sync::Notify::new()),
            update_notify: None,
        }
    }

    /// Same as `new()` but wires a collector-wide update notifier so every
    /// `store()` wakes the manager WS client for an out-of-cycle push.
    pub fn new_with_update_notify(notify: Arc<tokio::sync::Notify>) -> Self {
        let mut acc = Self::new();
        acc.update_notify = Some(notify);
        acc
    }

    /// Ask the thumbnail loop to capture a fresh frame at its next opportunity,
    /// out-of-cycle from the 5s interval. Safe to call from any task; the loop
    /// enforces a short cooldown so repeated calls do not overwhelm the
    /// decoder.
    pub fn request_refresh(&self) {
        self.refresh_trigger.notify_one();
    }

    /// Store a newly captured thumbnail.
    pub fn store(&self, jpeg_data: bytes::Bytes) {
        *self.latest_jpeg.lock().unwrap() = Some((jpeg_data, Instant::now()));
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.total_captured.fetch_add(1, Ordering::Relaxed);
        if let Some(n) = &self.update_notify {
            n.notify_one();
        }
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

    /// Reset the freeze-detection state (cleared alarm + zero counter +
    /// forgotten previous hash). Called when the capture path can no longer
    /// observe the stream — e.g. the input went idle and the TS ring is
    /// empty, or the decoder errored out. Without this the alarm remains
    /// "sticky frozen" indefinitely after a capture gap, and the next
    /// successful capture would immediately re-flag frozen if it happened
    /// to hash to the last stored value.
    pub fn reset_freeze(&self) {
        *self.alarm.lock().unwrap() = None;
        *self.prev_jpeg_hash.lock().unwrap() = None;
        self.freeze_count.store(0, Ordering::Relaxed);
    }

    /// Read the current thumbnail alarm state (cloned `Option<String>`).
    pub fn current_alarm(&self) -> Option<String> {
        self.alarm.lock().unwrap().clone()
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

/// Per-input atomic counters for a single input leg within a flow.
///
/// Each input owns its own byte/packet counters plus a private
/// [`ThroughputEstimator`], so a multi-input flow can report whether each
/// configured source is currently receiving a feed independently of which
/// one is switched live. Incremented from the shared per-input forwarder
/// in `engine::flow::spawn_input_forwarder` on every packet that arrives
/// on the input's dedicated broadcast channel — active or passive.
pub struct PerInputCounters {
    pub input_type: String,
    pub bytes: AtomicU64,
    pub packets: AtomicU64,
    pub throughput: ThroughputEstimator,
    /// Per-input topology/address metadata, captured at registration so
    /// the snapshot path can fill `PerInputLive.{mode, local_addr, ...}`
    /// for every configured input — not only the single active one.
    /// Immutable for the life of the input; a config-driven input edit
    /// rebuilds the flow and re-registers counters.
    pub meta: InputConfigMeta,
    /// Lightweight PAT/PMT catalogue for this input, maintained by
    /// [`crate::engine::ts_psi_catalog`]. `None` on non-TS inputs
    /// (RTMP / WebRTC / RTP-ES / ST 2110-30/-40) where there is no
    /// PSI to parse; the observer is not spawned for those.
    pub psi_catalog: Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>,
    /// Deep-clone cache for the catalogue, keyed by the store's update
    /// tick. `(0, None)` until the observer publishes its first PAT+PMT.
    /// The snapshot path refreshes this only when `PsiCatalogStore::tick`
    /// advances — steady-state PSI is stable, so the expensive deep clone
    /// happens once per PAT/PMT update instead of once per snapshot.
    psi_catalog_cache: std::sync::Mutex<(u64, Option<crate::engine::ts_psi_catalog::PsiCatalog>)>,
}

impl PerInputCounters {
    pub fn new(input_type: String, meta: InputConfigMeta) -> Self {
        Self {
            input_type,
            bytes: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            throughput: ThroughputEstimator::new(),
            meta,
            psi_catalog: Arc::new(crate::engine::ts_psi_catalog::PsiCatalogStore::new()),
            psi_catalog_cache: std::sync::Mutex::new((0, None)),
        }
    }

    /// Snapshot the catalogue with tick-based drop-equal caching. Returns
    /// `(catalog_clone, tick)` where the clone is reused from the cache
    /// whenever the store's tick hasn't advanced since the last snapshot.
    /// `tick == 0` means no PSI has ever been observed.
    pub fn psi_catalog_snapshot(
        &self,
    ) -> (
        Option<crate::engine::ts_psi_catalog::PsiCatalog>,
        u64,
    ) {
        let tick = self.psi_catalog.tick();
        if tick == 0 {
            return (None, 0);
        }
        let mut cache = match self.psi_catalog_cache.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if cache.0 != tick {
            let fresh = self.psi_catalog.load().map(|arc| (*arc).clone());
            *cache = (tick, fresh);
        }
        (cache.1.clone(), tick)
    }
}

// ── Content Analysis Accumulator ───────────────────────────────────────────

/// Per-flow accumulator for the in-depth content-analysis subsystem.
///
/// Each tier (Lite / Audio Full / Video Full) is updated by a single task
/// that subscribes to the flow's broadcast channel as an independent
/// consumer. The task owns all running state; only the wire-shaped
/// snapshot crosses the lock boundary, so the snapshot path is a cheap
/// `Mutex.lock().clone()` once per second.
///
/// **Hot path is never touched.** Tier tasks are subscribers — they cannot
/// add jitter or block the producer. If a task lags behind, it receives
/// `RecvError::Lagged(n)` and bumps the matching `*_drops` counter.
pub struct ContentAnalysisAccumulator {
    /// Lifetime broadcast-lag drops for the Lite analyser.
    pub lite_drops: AtomicU64,
    /// Lifetime broadcast-lag drops for the Audio Full analyser
    /// (Phase 2 — kept here so the wire shape stays stable).
    #[allow(dead_code)]
    pub audio_full_drops: AtomicU64,
    /// Lifetime broadcast-lag drops for the Video Full analyser
    /// (Phase 3 — kept here so the wire shape stays stable).
    #[allow(dead_code)]
    pub video_full_drops: AtomicU64,
    /// Most recent Lite-tier wire snapshot. The analyser task replaces
    /// this on every sample interval (~250 ms). `None` until the first
    /// sample interval completes, so the manager UI can render an
    /// "analysing…" state distinguishable from "tier off".
    pub lite: Mutex<Option<crate::stats::models::ContentAnalysisLiteStats>>,
    /// Phase 2 placeholder.
    pub audio_full: Mutex<Option<serde_json::Value>>,
    /// Phase 3 placeholder.
    pub video_full: Mutex<Option<serde_json::Value>>,
}

impl ContentAnalysisAccumulator {
    pub fn new() -> Self {
        Self {
            lite_drops: AtomicU64::new(0),
            audio_full_drops: AtomicU64::new(0),
            video_full_drops: AtomicU64::new(0),
            lite: Mutex::new(None),
            audio_full: Mutex::new(None),
            video_full: Mutex::new(None),
        }
    }

    /// Build a snapshot for [`crate::stats::models::FlowStats::content_analysis`].
    ///
    /// Returns `None` only when no tier has produced any data yet AND no
    /// drops have been recorded — otherwise the accumulator's mere presence
    /// means at least one tier is enabled, and we want the UI to render
    /// the "analysing…" state.
    pub fn snapshot(&self) -> crate::stats::models::ContentAnalysisStats {
        let mut lite = self.lite.lock().unwrap().clone();
        if let Some(ref mut l) = lite {
            l.analyser_drops = self.lite_drops.load(Ordering::Relaxed);
        }
        crate::stats::models::ContentAnalysisStats {
            lite,
            audio_full: self.audio_full.lock().unwrap().clone(),
            video_full: self.video_full.lock().unwrap().clone(),
        }
    }

    /// Replace the Lite-tier snapshot. Called from the lite analyser task
    /// at the end of each sample interval. The analyser owns the running
    /// state and rebuilds the wire-shaped struct to publish here.
    pub fn publish_lite(
        &self,
        snap: crate::stats::models::ContentAnalysisLiteStats,
    ) {
        *self.lite.lock().unwrap() = Some(snap);
    }
}

impl Default for ContentAnalysisAccumulator {
    fn default() -> Self {
        Self::new()
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
    /// Active input's transport type (e.g. `"srt"`, `"rtp"`). Stored behind
    /// an `RwLock` so it can be rewritten when the active input switches —
    /// the snapshot path reads it into `InputStats.input_type`.
    pub input_type: std::sync::RwLock<String>,
    pub started_at: Instant,
    // Input counters
    pub input_packets: AtomicU64,
    pub input_bytes: AtomicU64,
    pub input_loss: AtomicU64,
    pub input_filtered: AtomicU64,
    pub fec_recovered: AtomicU64,
    pub redundancy_switches: AtomicU64,
    /// Latest snapshot of the buffered SMPTE 2022-7 merger, when the
    /// input runs in industry-standard buffered mode. The redundant
    /// RTP listener publishes to this from the data-path task at the
    /// natural drain cadence; the snapshot path just clones it.
    pub buffered_hitless_snapshot:
        std::sync::RwLock<Option<crate::stats::models::BufferedHitlessSnapshot>>,
    // Per-output stats
    pub output_stats: DashMap<String, Arc<OutputStatsAccumulator>>,
    pub input_throughput: ThroughputEstimator,
    /// TR-101290 analyzer stats, set once when the flow starts.
    pub tr101290: OnceLock<Arc<Tr101290Accumulator>>,
    /// Media analysis stats, set once when the flow starts (if enabled).
    pub media_analysis: OnceLock<Arc<MediaAnalysisAccumulator>>,
    /// Thumbnail generation stats, set once when the flow starts (if enabled and ffmpeg available).
    pub thumbnail: OnceLock<Arc<ThumbnailAccumulator>>,
    /// In-depth content-analysis (Lite / Audio Full / Video Full) stats,
    /// set once when the flow starts if any tier is enabled.
    pub content_analysis: OnceLock<Arc<ContentAnalysisAccumulator>>,
    /// Replay-server recording stats handle, set once when the flow
    /// starts and the `replay` feature is compiled in. The
    /// FlowRuntime calls `set_recording_stats` after it spawns the
    /// writer; the snapshot path reads atomic counters off it without
    /// taking a lock. Behind a feature flag so non-replay builds pay
    /// no struct overhead.
    #[cfg(feature = "replay")]
    pub recording_stats: OnceLock<Arc<crate::replay::writer::RecordingStats>>,
    /// Recording id (mirrors [`crate::replay::writer::RecordingHandle::recording_id`])
    /// — exposed on the snapshot so the manager UI can render the
    /// recording id alongside the live counters without a separate
    /// round-trip.
    #[cfg(feature = "replay")]
    pub recording_id: OnceLock<String>,
    /// Per-input thumbnail accumulators, keyed by input ID. Each input in
    /// a multi-input flow gets its own thumbnail generator subscribing to
    /// the input's dedicated broadcast channel (not the flow's main channel).
    pub per_input_thumbnails: DashMap<String, Arc<ThumbnailAccumulator>>,
    /// Per-input byte/packet/throughput counters. One entry per input in the
    /// flow, keyed by input ID. Populated at flow start by
    /// `register_input_counters` and incremented by the shared per-input
    /// forwarder — so every input's liveness (bitrate, packets_received) is
    /// tracked regardless of whether the input is currently switched active.
    /// The snapshot path reads this into `FlowStats.inputs_live` so the
    /// manager UI can render a NO SIGNAL / feed-present state per input.
    pub per_input_counters: DashMap<String, Arc<PerInputCounters>>,
    /// Input config metadata for the currently active input (topology /
    /// header display). Rewritten by `update_active_input_meta` on every
    /// `FlowRuntime::switch_active_input` call so the snapshot reflects the
    /// live input's `mode` / address fields, not the input that happened to
    /// be active at flow start.
    pub input_config_meta: std::sync::RwLock<Option<InputConfigMeta>>,
    /// Per-output config metadata for topology display (set once per output).
    pub output_config_meta: DashMap<String, OutputConfigMeta>,
    /// Cached SRT stats for primary input leg, updated by the SRT input polling task
    /// via a lock-free watch channel.
    pub input_srt_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Cached SRT stats for redundancy input leg, updated by the SRT input polling task
    /// via a lock-free watch channel.
    pub input_srt_leg2_stats_cache: Arc<watch::Sender<Option<SrtLegStats>>>,
    /// Cached native-libsrt bonding stats (aggregate + per-member) for an
    /// SRT input whose config has a `bonding` block. Lock-free watch
    /// channel, populated by `spawn_srt_group_stats_poller`.
    pub input_srt_bonding_stats_cache:
        Arc<watch::Sender<Option<crate::stats::models::SrtBondingStats>>>,
    /// Shared bond stats handle for bonded inputs — the only input
    /// type that aggregates N paths at the transport layer.
    /// Populated by `engine::input_bonded::spawn_bonded_input`.
    input_bond_stats_handle: OnceLock<BondStatsHandle>,
    /// Shared RIST connection-level counters for the primary input leg.
    input_rist_stats_handle: OnceLock<Arc<rist_transport::RistConnStats>>,
    /// Shared RIST counters for the SMPTE 2022-7 second input leg.
    input_rist_leg2_stats_handle: OnceLock<Arc<rist_transport::RistConnStats>>,
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
    /// ID of the currently active input for this flow, if any. Updated by
    /// `FlowRuntime` on startup and every `switch_active_input` call. The
    /// snapshot path reads this into `FlowStats.active_input_id` so the
    /// manager UI can show which input source is currently live. Empty
    /// string = no active input (flow idle).
    pub active_input_id: std::sync::RwLock<String>,
    /// Per-input-id map of PCM transcoder stats handles (channel shuffle /
    /// sample-rate conversion on the ingest leg). At snapshot time the
    /// collector reads the entry keyed by `active_input_id` so passive inputs'
    /// handles don't leak into the reported ingress pipeline after a switch.
    input_transcode_stats:
        DashMap<String, Arc<crate::engine::audio_transcode::TranscodeStats>>,
    /// Per-input-id map of AAC decode stage handles + descriptors.
    input_audio_decode_stats: DashMap<String, AudioDecodeStatsHandle>,
    /// Per-input-id map of audio encode stage handles + target codec descriptors.
    input_audio_encode_stats: DashMap<String, AudioEncodeStatsHandle>,
    /// Per-input-id map of video encode stage handles + target codec / geometry
    /// descriptors. Populated by ST 2110-20/-23 inputs or by Group A inputs
    /// that configured a `video_encode` block (via the `InputTranscoder`).
    input_video_encode_stats: DashMap<String, VideoEncodeStatsHandle>,
    /// Per-input-id map of static ingress-summary descriptors. The dynamic
    /// codec/format fields are merged in at `FlowStatsAccumulator::snapshot()`.
    ingress_static: DashMap<String, EgressMediaSummaryStatic>,
    /// PID-bus Phase 8: per-elementary-stream counters keyed by
    /// `(input_id, source_pid)`. Populated incrementally by the
    /// per-ES analyzer task (see `engine::ts_es_analysis`) that
    /// subscribes to each `FlowEsBus` channel. Surfaced via
    /// `FlowStats.per_es` on assembled flows; absent on passthrough
    /// flows (which have no `FlowEsBus` to read from).
    pub per_es_stats: DashMap<(String, u16), Arc<PerEsAccumulator>>,
    /// Snapshot of the running assembler's `(input_id, source_pid) → out_pid`
    /// routing. Updated by `FlowRuntime::replace_assembly` after every
    /// runtime swap. Used by `snapshot()` to annotate `PerEsStats.out_pid`
    /// so operators can pivot their trust signals off egress PID. Empty
    /// on passthrough flows.
    pub pid_routing: std::sync::RwLock<std::collections::HashMap<(String, u16), u16>>,
}

/// Per-elementary-stream accumulator. One instance per `(input_id, source_pid)`
/// channel on the flow's `FlowEsBus`. Atomic counters — updated inline by
/// the per-ES analyzer subscriber, read at 1 Hz by the snapshot path.
pub struct PerEsAccumulator {
    pub input_id: String,
    pub source_pid: u16,
    /// Most recently observed PMT `stream_type`. `AtomicU8`-equivalent via
    /// the same `AtomicU64` we use for every other counter. Written each
    /// time an ES packet arrives (the bus stamps stream_type per packet).
    pub stream_type: AtomicU64,
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub cc_errors: AtomicU64,
    pub pcr_discontinuity_errors: AtomicU64,
    /// Last continuity-counter value (0..=15), packed in the low nibble.
    /// Mutated by a single owner (the per-ES analyzer task) via Relaxed.
    /// 0x100 sentinel = "no CC observed yet".
    pub last_cc: AtomicU64,
    /// Last observed PCR in 27 MHz ticks; `u64::MAX` sentinel = none yet.
    pub last_pcr_27mhz: AtomicU64,
    pub throughput: ThroughputEstimator,
}

impl PerEsAccumulator {
    pub fn new(input_id: String, source_pid: u16) -> Self {
        Self {
            input_id,
            source_pid,
            stream_type: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            cc_errors: AtomicU64::new(0),
            pcr_discontinuity_errors: AtomicU64::new(0),
            last_cc: AtomicU64::new(0x100),
            last_pcr_27mhz: AtomicU64::new(u64::MAX),
            throughput: ThroughputEstimator::new(),
        }
    }

    /// Derive a human-readable essence kind from the observed `stream_type`.
    /// Mirrors the mapping `ts_psi_catalog` uses, kept here to avoid a
    /// cross-module borrow in the snapshot path.
    pub fn kind_for_stream_type(st: u8) -> &'static str {
        match st {
            0x01 | 0x02 | 0x10 | 0x1B | 0x20 | 0x24 | 0x27 | 0x42 | 0xD1 | 0xEA => "video",
            0x03 | 0x04 | 0x0F | 0x11 | 0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85 | 0x86
            | 0x87 | 0x88 | 0x89 | 0x8A | 0x8B | 0x8C | 0x8D | 0x8E | 0x8F | 0xA1 | 0xC1
            | 0xC2 => "audio",
            0x06 => "subtitle",
            0x05 | 0x0B | 0x0C | 0x0D | 0x15 | 0x16 => "data",
            _ => "",
        }
    }
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
            input_type: std::sync::RwLock::new(input_type),
            started_at: Instant::now(),
            input_packets: AtomicU64::new(0),
            input_bytes: AtomicU64::new(0),
            input_loss: AtomicU64::new(0),
            input_filtered: AtomicU64::new(0),
            fec_recovered: AtomicU64::new(0),
            redundancy_switches: AtomicU64::new(0),
            buffered_hitless_snapshot: std::sync::RwLock::new(None),
            output_stats: DashMap::new(),
            input_throughput: ThroughputEstimator::new(),
            tr101290: OnceLock::new(),
            media_analysis: OnceLock::new(),
            thumbnail: OnceLock::new(),
            content_analysis: OnceLock::new(),
            #[cfg(feature = "replay")]
            recording_stats: OnceLock::new(),
            #[cfg(feature = "replay")]
            recording_id: OnceLock::new(),
            per_input_thumbnails: DashMap::new(),
            per_input_counters: DashMap::new(),
            input_config_meta: std::sync::RwLock::new(None),
            output_config_meta: DashMap::new(),
            input_srt_stats_cache: Arc::new(watch::channel(None).0),
            input_srt_leg2_stats_cache: Arc::new(watch::channel(None).0),
            input_srt_bonding_stats_cache: Arc::new(watch::channel(None).0),
            input_bond_stats_handle: OnceLock::new(),
            input_rist_stats_handle: OnceLock::new(),
            input_rist_leg2_stats_handle: OnceLock::new(),
            bandwidth_exceeded: AtomicBool::new(false),
            bandwidth_blocked: AtomicBool::new(false),
            bandwidth_limit_mbps: OnceLock::new(),
            ptp_state: OnceLock::new(),
            red_blue_stats: OnceLock::new(),
            active_input_id: std::sync::RwLock::new(String::new()),
            input_transcode_stats: DashMap::new(),
            input_audio_decode_stats: DashMap::new(),
            input_audio_encode_stats: DashMap::new(),
            input_video_encode_stats: DashMap::new(),
            ingress_static: DashMap::new(),
            per_es_stats: DashMap::new(),
            pid_routing: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Update the `(input_id, source_pid) → out_pid` routing snapshot. Called
    /// by `FlowRuntime` after every assembler plan swap so the per-ES
    /// snapshot path can annotate each entry with its egress PID.
    ///
    /// The map is replaced wholesale — entries that were in the old plan
    /// but not the new one no longer carry an `out_pid` in the wire shape,
    /// matching the reality that the assembler no longer forwards them.
    pub fn set_pid_routing(&self, routing: std::collections::HashMap<(String, u16), u16>) {
        if let Ok(mut w) = self.pid_routing.write() {
            *w = routing;
        }
    }

    /// Resolve (or create) a per-ES accumulator for a given
    /// `(input_id, source_pid)`. Called by the per-ES analyzer as new
    /// bus channels appear — subsequent calls return the existing entry.
    pub fn per_es_acc(&self, input_id: &str, source_pid: u16) -> Arc<PerEsAccumulator> {
        let key = (input_id.to_string(), source_pid);
        if let Some(existing) = self.per_es_stats.get(&key) {
            return existing.value().clone();
        }
        let new_acc = Arc::new(PerEsAccumulator::new(input_id.to_string(), source_pid));
        self.per_es_stats
            .entry(key)
            .or_insert_with(|| new_acc.clone())
            .value()
            .clone()
    }

    /// Register an input-side PCM transcoder's stats handle for a specific
    /// input. Called at flow start by each input that instantiates a transcode
    /// stage. Keyed by `input_id` so a multi-input flow can track each input's
    /// pipeline independently and report only the active input's stats.
    #[allow(dead_code)]
    pub fn set_input_transcode_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::audio_transcode::TranscodeStats>,
    ) {
        self.input_transcode_stats.insert(input_id.to_string(), stats);
    }

    /// Register an input-side audio decode stats handle.
    #[allow(dead_code)]
    pub fn set_input_decode_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::audio_decode::DecodeStats>,
        input_codec: impl Into<String>,
        output_sample_rate_hz: u32,
        output_channels: u8,
    ) {
        self.input_audio_decode_stats.insert(
            input_id.to_string(),
            AudioDecodeStatsHandle {
                stats,
                input_codec: input_codec.into(),
                output_sample_rate_hz,
                output_channels,
            },
        );
    }

    /// Register an input-side audio encode stats handle.
    #[allow(dead_code)]
    pub fn set_input_encode_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::audio_encode::EncodeStats>,
        output_codec: impl Into<String>,
        target_sample_rate_hz: u32,
        target_channels: u8,
        target_bitrate_kbps: u32,
    ) {
        self.input_audio_encode_stats.insert(
            input_id.to_string(),
            AudioEncodeStatsHandle {
                stats,
                output_codec: output_codec.into(),
                target_sample_rate_hz,
                target_channels,
                target_bitrate_kbps,
            },
        );
    }

    /// Register an input-side video encode stats handle for a specific input.
    /// Called at flow start by ST 2110-20/-23 inputs or by Group A inputs that
    /// run a `TsVideoReplacer` on the ingest leg.
    #[allow(clippy::too_many_arguments)]
    pub fn set_input_video_encode_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::ts_video_replace::VideoEncodeStats>,
        input_codec: impl Into<String>,
        output_codec: impl Into<String>,
        output_width: u32,
        output_height: u32,
        output_fps: f32,
        output_bitrate_kbps: u32,
        encoder_backend: impl Into<String>,
    ) {
        self.input_video_encode_stats.insert(
            input_id.to_string(),
            VideoEncodeStatsHandle {
                stats,
                input_codec: input_codec.into(),
                output_codec: output_codec.into(),
                output_width,
                output_height,
                output_fps,
                output_bitrate_kbps,
                encoder_backend: encoder_backend.into(),
            },
        );
    }

    /// Register the static portion of an input's ingress media summary.
    /// Called at flow start, once per input, with values derived from that
    /// input's config. Snapshot reads only the active input's entry.
    pub fn set_ingress_static(
        &self,
        input_id: &str,
        descriptor: EgressMediaSummaryStatic,
    ) {
        self.ingress_static
            .insert(input_id.to_string(), descriptor);
    }

    /// Set or clear the currently active input ID. Called by `FlowRuntime`
    /// during startup and on every input switch. The empty string means
    /// "no active input" (the flow is idle).
    pub fn set_active_input_id(&self, id: &str) {
        if let Ok(mut guard) = self.active_input_id.write() {
            *guard = id.to_string();
        }
    }

    /// Register the primary RIST input's shared stats handle. Called once
    /// when the `RistSocket::receiver` is built for this flow.
    pub fn set_input_rist_stats(&self, stats: Arc<rist_transport::RistConnStats>) {
        let _ = self.input_rist_stats_handle.set(stats);
    }

    /// Register the SMPTE 2022-7 second-leg RIST input's stats handle.
    pub fn set_input_rist_leg2_stats(&self, stats: Arc<rist_transport::RistConnStats>) {
        let _ = self.input_rist_leg2_stats_handle.set(stats);
    }

    /// Register the bond stats handle for a bonded input. Called
    /// once after `BondSocket::receiver` is built.
    pub fn set_input_bond_stats(&self, handle: BondStatsHandle) {
        let _ = self.input_bond_stats_handle.set(handle);
    }

    /// Replace the header fields (`input_type` + `InputConfigMeta`) that the
    /// snapshot path reports to the manager. Called once at flow start and
    /// again on every `switch_active_input` so the UI sees the *live* input's
    /// transport / mode / address rather than the one that happened to be
    /// active when the flow was first registered.
    pub fn update_active_input_meta(&self, input_type: &str, meta: InputConfigMeta) {
        if let Ok(mut g) = self.input_type.write() {
            *g = input_type.to_string();
        }
        if let Ok(mut g) = self.input_config_meta.write() {
            *g = Some(meta);
        }
    }

    /// Register per-input counters for a specific input and return a shared
    /// reference. Called once per input at flow start so the per-input
    /// forwarder can increment bytes/packets for every packet flowing on that
    /// input's dedicated broadcast channel, regardless of active/passive.
    pub fn register_input_counters(
        &self,
        input_id: &str,
        input_type: &str,
        meta: InputConfigMeta,
    ) -> Arc<PerInputCounters> {
        let c = Arc::new(PerInputCounters::new(input_type.to_string(), meta));
        self.per_input_counters.insert(input_id.to_string(), c.clone());
        c
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
        let mut outputs: Vec<OutputStats> = self
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
        let input_bitrate = self.input_throughput.sample(input_bytes);

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

        // Inject frame-based latency into outputs when video frame rate is known.
        if let Some(ref ma) = media_analysis {
            let frame_rate = ma.video_streams.first().and_then(|v| v.frame_rate);
            if let Some(fps) = frame_rate {
                if fps > 0.0 {
                    let us_per_frame = 1_000_000.0 / fps;
                    for out in &mut outputs {
                        if let Some(ref mut lat) = out.latency {
                            lat.latency_frames = Some(lat.avg_us as f64 / us_per_frame);
                        }
                    }
                }
            }
        }

        // Build per-output EgressMediaSummary. Combines the static descriptors
        // each output registered at startup with its live encode/decode/transcode
        // stats and the cached input MediaAnalysis. Zero new CPU on the data
        // plane — every field is read from values already computed elsewhere.
        for out in &mut outputs {
            let acc = match self.output_stats.get(&out.output_id) {
                Some(a) => a.value().clone(),
                None => continue,
            };
            out.egress_summary = build_pipeline_summary(
                acc.egress_static(),
                out.program_number,
                media_analysis.as_ref(),
                out.transcode_stats.as_ref(),
                out.audio_decode_stats.as_ref(),
                out.audio_encode_stats.as_ref(),
                out.video_encode_stats.as_ref(),
            );
        }

        // Snapshot RIST input stats (primary + 2022-7 leg 2 when present).
        // The RIST receiver task owns `packets_lost` / `packets_recovered`
        // inside the `RistConnStats` Arc — surface those here and use them
        // to drive the generic `input.packets_lost` field for RIST flows
        // (RIST has its own gap/loss accounting, separate from `input_loss`
        // which is wired by RTP/SRT inputs).
        let rist_input_primary_snapshot = self
            .input_rist_stats_handle
            .get()
            .map(|h| h.snapshot());
        let rist_input_leg2_snapshot = self
            .input_rist_leg2_stats_handle
            .get()
            .map(|h| h.snapshot());
        let rist_input_has_packets = rist_input_primary_snapshot
            .as_ref()
            .map(|s| s.packets_received > 0)
            .unwrap_or(false);
        let rist_input_stats = rist_input_primary_snapshot
            .as_ref()
            .map(|s| rist_snapshot_to_leg_stats(s, RistStatsRole::Receiver, rist_input_has_packets));
        let rist_input_leg2_stats = rist_input_leg2_snapshot
            .as_ref()
            .map(|s| rist_snapshot_to_leg_stats(s, RistStatsRole::Receiver, s.packets_received > 0));

        // Derive flow health (RP 2129 M6)
        let rist_loss = rist_input_primary_snapshot
            .as_ref()
            .map(|s| s.packets_lost)
            .unwrap_or(0)
            + rist_input_leg2_snapshot
                .as_ref()
                .map(|s| s.packets_lost)
                .unwrap_or(0);
        let packets_lost = self.input_loss.load(Ordering::Relaxed).max(rist_loss);
        let bw_exceeded = self.bandwidth_exceeded.load(Ordering::Relaxed);
        let bw_blocked = self.bandwidth_blocked.load(Ordering::Relaxed);
        let health = derive_flow_health(input_bitrate, packets_lost, &tr101290_snap, bw_exceeded, bw_blocked);

        let active_input_id = self
            .active_input_id
            .read()
            .ok()
            .and_then(|g| if g.is_empty() { None } else { Some(g.clone()) });

        // Per-input liveness snapshot. One entry per registered input — lets
        // the manager UI surface NO SIGNAL / feed-present independently of the
        // currently switched input. Left as `None` when a flow has no inputs
        // registered so the JSON shape stays stable for old builds.
        let inputs_live: Vec<PerInputLive> = self
            .per_input_counters
            .iter()
            .map(|entry| {
                let input_id = entry.key().clone();
                let c = entry.value();
                let bytes = c.bytes.load(Ordering::Relaxed);
                let packets = c.packets.load(Ordering::Relaxed);
                let bitrate = c.throughput.sample(bytes);
                let (psi_catalog, psi_tick) = c.psi_catalog_snapshot();
                PerInputLive {
                    input_id,
                    input_type: c.input_type.clone(),
                    state: derive_input_state(bitrate, packets),
                    packets_received: packets,
                    bytes_received: bytes,
                    bitrate_bps: bitrate,
                    mode: c.meta.mode.clone(),
                    local_addr: c.meta.local_addr.clone(),
                    remote_addr: c.meta.remote_addr.clone(),
                    listen_addr: c.meta.listen_addr.clone(),
                    bind_addr: c.meta.bind_addr.clone(),
                    rtsp_url: c.meta.rtsp_url.clone(),
                    whep_url: c.meta.whep_url.clone(),
                    psi_catalog,
                    psi_catalog_tick: if psi_tick == 0 { None } else { Some(psi_tick) },
                }
            })
            .collect();
        let inputs_live = if inputs_live.is_empty() { None } else { Some(inputs_live) };

        // Per-ES snapshot. Read the routing map once so every entry gets a
        // consistent `out_pid`.
        let routing = self.pid_routing.read().ok().map(|g| g.clone()).unwrap_or_default();
        let per_es_vec: Vec<crate::stats::models::PerEsStats> = self
            .per_es_stats
            .iter()
            .map(|entry| {
                let acc = entry.value();
                let bytes = acc.bytes.load(Ordering::Relaxed);
                let bitrate = acc.throughput.sample(bytes);
                let st = acc.stream_type.load(Ordering::Relaxed) as u8;
                let key = (acc.input_id.clone(), acc.source_pid);
                crate::stats::models::PerEsStats {
                    input_id: acc.input_id.clone(),
                    source_pid: acc.source_pid,
                    out_pid: routing.get(&key).copied(),
                    stream_type: st,
                    kind: PerEsAccumulator::kind_for_stream_type(st).to_string(),
                    packets: acc.packets.load(Ordering::Relaxed),
                    bytes,
                    bitrate_bps: bitrate,
                    cc_errors: acc.cc_errors.load(Ordering::Relaxed),
                    pcr_discontinuity_errors: acc
                        .pcr_discontinuity_errors
                        .load(Ordering::Relaxed),
                }
            })
            .collect();
        let per_es = if per_es_vec.is_empty() { None } else { Some(per_es_vec) };

        // Flow-level PCR trust rollup. Aggregate every output's sampler
        // snapshot into one. We report the max p50/p95/p99/max across
        // outputs rather than a re-percentile of the union — simpler to
        // reason about ("no output is worse than this") and computable
        // from already-computed per-output snapshots.
        let pcr_trust_flow = {
            let mut acc: Option<crate::stats::models::PcrTrustStats> = None;
            for out_entry in self.output_stats.iter() {
                if let Some(snap) = out_entry.value().pcr_trust_sampler().snapshot() {
                    let agg = acc.get_or_insert_with(Default::default);
                    agg.samples = agg.samples.max(snap.samples);
                    agg.cumulative_samples = agg.cumulative_samples + snap.cumulative_samples;
                    agg.avg_us = agg.avg_us.max(snap.avg_us);
                    agg.p50_us = agg.p50_us.max(snap.p50_us);
                    agg.p95_us = agg.p95_us.max(snap.p95_us);
                    agg.p99_us = agg.p99_us.max(snap.p99_us);
                    agg.max_us = agg.max_us.max(snap.max_us);
                    agg.window_samples = agg.window_samples.max(snap.window_samples);
                    agg.window_p95_us = agg.window_p95_us.max(snap.window_p95_us);
                }
            }
            acc
        };

        FlowStats {
            flow_id: self.flow_id.clone(),
            flow_name: self.flow_name.clone(),
            state: FlowState::Running,
            active_input_id: active_input_id.clone(),
            input: {
                let input_type = self
                    .input_type
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let meta_guard = self.input_config_meta.read().ok();
                let meta = meta_guard.as_ref().and_then(|g| g.as_ref());

                // Ingress stats/summary are keyed by input_id so a multi-input
                // flow reports only the currently-active input's pipeline.
                // Empty key = no active input → all lookups return None, so the
                // UI shows no transcode/encode badges at all (same as idle).
                let active_key = active_input_id.as_deref().unwrap_or("");
                let in_transcode =
                    self.input_transcode_stats.get(active_key).map(|t| {
                        crate::stats::models::TranscodeStatsSnapshot {
                            input_packets: t.input_packets.load(Ordering::Relaxed),
                            output_packets: t.output_packets.load(Ordering::Relaxed),
                            dropped: t.dropped.load(Ordering::Relaxed),
                            format_resets: t.format_resets.load(Ordering::Relaxed),
                            last_latency_us: t.last_latency_us.load(Ordering::Relaxed),
                        }
                    });
                let in_audio_decode =
                    self.input_audio_decode_stats.get(active_key).map(|h| {
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
                let in_audio_encode =
                    self.input_audio_encode_stats.get(active_key).map(|h| {
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
                let in_video_encode =
                    self.input_video_encode_stats.get(active_key).map(|h| {
                        crate::stats::models::VideoEncodeStatsSnapshot {
                            input_frames: h.stats.input_frames.load(Ordering::Relaxed),
                            output_frames: h.stats.output_frames.load(Ordering::Relaxed),
                            dropped_frames: h.stats.dropped_frames.load(Ordering::Relaxed),
                            input_codec: h.input_codec.clone(),
                            output_codec: h.output_codec.clone(),
                            output_width: h.output_width,
                            output_height: h.output_height,
                            output_fps: h.output_fps,
                            output_bitrate_kbps: h.output_bitrate_kbps,
                            encoder_backend: h.encoder_backend.clone(),
                            last_latency_us: h.stats.last_latency_us.load(Ordering::Relaxed),
                            supervisor_restarts: h.stats.supervisor_restarts.load(Ordering::Relaxed),
                        }
                    });
                let ingress_static_snap = self
                    .ingress_static
                    .get(active_key)
                    .map(|e| e.value().clone());
                let ingress_summary = build_pipeline_summary(
                    ingress_static_snap.as_ref(),
                    None, // ingress does not apply output-side MPTS program filtering
                    media_analysis.as_ref(),
                    in_transcode.as_ref(),
                    in_audio_decode.as_ref(),
                    in_audio_encode.as_ref(),
                    in_video_encode.as_ref(),
                );

                InputStats {
                    input_type,
                    state: derive_input_state(input_bitrate, self.input_packets.load(Ordering::Relaxed)),
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
                    srt_bonding_stats: self.input_srt_bonding_stats_cache.borrow().clone(),
                    rist_stats: rist_input_stats,
                    rist_leg2_stats: rist_input_leg2_stats,
                    bond_stats: self
                        .input_bond_stats_handle
                        .get()
                        .map(bond_handle_to_leg_stats),
                    redundancy_switches: self.redundancy_switches.load(Ordering::Relaxed),
                    buffered_hitless: self
                        .buffered_hitless_snapshot
                        .read()
                        .ok()
                        .and_then(|g| g.clone()),
                    transcode_stats: in_transcode,
                    audio_decode_stats: in_audio_decode,
                    audio_encode_stats: in_audio_encode,
                    video_encode_stats: in_video_encode,
                    ingress_summary,
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
            inputs_live,
            per_es,
            pcr_trust_flow,
            content_analysis: self.content_analysis.get().map(|acc| acc.snapshot()),
            #[cfg(feature = "replay")]
            recording: self.recording_stats.get().map(|s| {
                use std::sync::atomic::Ordering;
                crate::stats::models::RecordingSnapshot {
                    armed: s.armed.load(Ordering::Relaxed),
                    recording_id: self.recording_id.get().cloned(),
                    current_pts_90khz: s.current_pts_90khz.load(Ordering::Relaxed),
                    segments_written: s.segments_written.load(Ordering::Relaxed),
                    bytes_written: s.bytes_written.load(Ordering::Relaxed),
                    segments_pruned: s.segments_pruned.load(Ordering::Relaxed),
                    packets_dropped: s.packets_dropped.load(Ordering::Relaxed),
                    index_entries: s.index_entries.load(Ordering::Relaxed),
                    last_write_unix_ms: s.last_write_unix_ms.load(Ordering::Relaxed),
                    mode: Some(
                        crate::replay::writer::mode_to_wire_str(
                            s.mode.load(Ordering::Relaxed),
                        )
                        .to_string(),
                    ),
                }
            }),
            #[cfg(not(feature = "replay"))]
            recording: None,
        }
    }
}

/// Convert a live `engine::st2110::ptp::PtpState` into the wire-shaped
/// `PtpStateStats` carried by `FlowStats`. Pure mapping — no I/O, no locks.
/// Socket role for RIST stats conversion. Mirrors `rist_transport::RistRole`
/// but stays local so the `RistLegStats` serde type never leaks a crate
/// boundary.
#[derive(Clone, Copy)]
pub enum RistStatsRole {
    Sender,
    Receiver,
}

/// Convert a RIST connection-level snapshot into the wire-shaped
/// [`RistLegStats`] the stats API surfaces. Infers a human-readable state
/// string from whether packets have flowed in the current reporting window.
pub fn rist_snapshot_to_leg_stats(
    snap: &rist_transport::RistConnStatsSnapshot,
    role: RistStatsRole,
    active: bool,
) -> RistLegStats {
    let (role_name, state) = match role {
        RistStatsRole::Sender => (
            "sender".to_string(),
            if active { "sending" } else { "idle" }.to_string(),
        ),
        RistStatsRole::Receiver => (
            "receiver".to_string(),
            if snap.packets_received > 0 {
                "receiving"
            } else {
                "idle"
            }
            .to_string(),
        ),
    };
    RistLegStats {
        state,
        role: role_name,
        rtt_ms: snap.rtt_ms(),
        jitter_us: snap.jitter_us,
        packets_sent: snap.packets_sent,
        bytes_sent: snap.bytes_sent,
        pkt_retransmit_total: snap.packets_retransmitted,
        nack_received_total: snap.nacks_received,
        packets_received: snap.packets_received,
        bytes_received: snap.bytes_received,
        packets_lost: snap.packets_lost,
        packets_recovered: snap.packets_recovered,
        nack_sent_total: snap.nacks_sent,
        duplicates: snap.duplicates,
        reorder_drops: snap.reorder_drops,
        retransmits_received: snap.retransmits_received,
    }
}

/// Convenience for the output-stats path: always snapshots as a sender.
fn rist_conn_to_leg_stats(stats: &rist_transport::RistConnStats, active: bool) -> RistLegStats {
    rist_snapshot_to_leg_stats(&stats.snapshot(), RistStatsRole::Sender, active)
}

// ── Bonding stats ──────────────────────────────────────────────────────────

/// Per-path stats handle registered on an input or output
/// accumulator alongside the aggregate [`BondStatsHandle`]. Carries
/// the operator-facing name + transport label so the snapshot
/// output is self-describing without the edge needing to re-consult
/// the flow config.
#[derive(Clone, Debug)]
pub struct BondPathStatsHandle {
    pub id: u8,
    pub name: String,
    pub transport: String,
    pub stats: Arc<bonding_protocol::stats::PathStats>,
}

/// Aggregate + per-path bond stats handle. One per bonded input or
/// output accumulator.
#[derive(Clone, Debug)]
pub struct BondStatsHandle {
    pub flow_id: u32,
    pub role: BondStatsRole,
    pub scheduler: String,
    pub conn_stats: Arc<bonding_protocol::stats::BondConnStats>,
    pub paths: Vec<BondPathStatsHandle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BondStatsRole {
    Sender,
    Receiver,
}

/// Convert a live [`BondStatsHandle`] into the wire-shaped
/// [`BondLegStats`] carried by `InputStats` / `OutputStats`.
pub fn bond_handle_to_leg_stats(h: &BondStatsHandle) -> BondLegStats {
    let snap = h.conn_stats.snapshot();
    let role = match h.role {
        BondStatsRole::Sender => "sender".to_string(),
        BondStatsRole::Receiver => "receiver".to_string(),
    };
    let paths: Vec<BondPathLegStats> = h
        .paths
        .iter()
        .map(|p| {
            let ps = p.stats.snapshot();
            BondPathLegStats {
                id: p.id,
                name: p.name.clone(),
                transport: p.transport.clone(),
                state: if ps.dead { "dead" } else { "alive" }.to_string(),
                rtt_ms: ps.rtt_ms(),
                jitter_us: ps.jitter_us,
                loss_fraction: ps.loss_fraction(),
                throughput_bps: ps.throughput_bps,
                queue_depth: ps.queue_depth,
                packets_sent: ps.packets_sent,
                bytes_sent: ps.bytes_sent,
                packets_received: ps.packets_received,
                bytes_received: ps.bytes_received,
                nacks_sent: ps.nacks_sent,
                nacks_received: ps.nacks_received,
                retransmits_sent: ps.retransmits_sent,
                retransmits_received: ps.retransmits_received,
                keepalives_sent: ps.keepalives_sent,
                keepalives_received: ps.keepalives_received,
            }
        })
        .collect();

    // Aggregate state: "up" if any path alive and any traffic
    // observed; "degraded" if one or more paths are dead; "idle" if
    // everything's zero.
    let any_dead = paths.iter().any(|p| p.state == "dead");
    let any_activity = match h.role {
        BondStatsRole::Sender => snap.packets_sent > 0,
        BondStatsRole::Receiver => snap.packets_received > 0,
    };
    let state = if any_dead {
        "degraded".to_string()
    } else if any_activity {
        "up".to_string()
    } else {
        "idle".to_string()
    };

    BondLegStats {
        state,
        flow_id: h.flow_id,
        role,
        scheduler: h.scheduler.clone(),
        packets_sent: snap.packets_sent,
        bytes_sent: snap.bytes_sent,
        packets_retransmitted: snap.packets_retransmitted,
        packets_duplicated: snap.packets_duplicated,
        packets_dropped_no_path: snap.packets_dropped_no_path,
        packets_received: snap.packets_received,
        bytes_received: snap.bytes_received,
        packets_delivered: snap.packets_delivered,
        gaps_recovered: snap.gaps_recovered,
        gaps_lost: snap.gaps_lost,
        duplicates_received: snap.duplicates_received,
        reassembly_overflow: snap.reassembly_overflow,
        paths,
    }
}

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

/// Build a pipeline (ingress or egress) media summary at snapshot time.
///
/// Combines the static descriptors registered at start-up with live
/// encode/decode/transcode stats and the cached input `MediaAnalysisStats`.
/// Returns `None` when no static descriptor was registered — callers then
/// degrade to their previous behaviour.
fn build_pipeline_summary(
    stat_desc: Option<&EgressMediaSummaryStatic>,
    program_number: Option<u16>,
    media_analysis: Option<&crate::stats::models::MediaAnalysisStats>,
    transcode: Option<&crate::stats::models::TranscodeStatsSnapshot>,
    audio_decode: Option<&crate::stats::models::DecodeStatsSnapshot>,
    audio_encode: Option<&crate::stats::models::EncodeStatsSnapshot>,
    video_encode: Option<&crate::stats::models::VideoEncodeStatsSnapshot>,
) -> Option<crate::stats::models::EgressMediaSummary> {
    let stat_desc = stat_desc?;

    let mut summary = crate::stats::models::EgressMediaSummary {
        transport_mode: stat_desc.transport_mode.clone(),
        program_number,
        ..Default::default()
    };

    // Pick the source video / audio descriptor from the input MediaAnalysis.
    // If a `program_number` is set, prefer that program's streams; otherwise
    // fall back to the flat union (which is the lowest-program-number default).
    let (src_video, src_audio) = if let Some(ma) = media_analysis {
        let prog = program_number.and_then(|pn| ma.programs.iter().find(|p| p.program_number == pn));
        let v = prog
            .and_then(|p| p.video_streams.first())
            .or_else(|| ma.video_streams.first());
        let a = prog
            .and_then(|p| p.audio_streams.first())
            .or_else(|| ma.audio_streams.first());
        (v, a)
    } else {
        (None, None)
    };

    // ── Pipeline tags (ordered) ───────────────────────────────────────────
    if program_number.is_some() {
        summary.pipeline.push("program_filter".to_string());
    }
    if audio_decode.is_some() {
        summary.pipeline.push("audio_decode".to_string());
    }
    if transcode.is_some() {
        summary.pipeline.push("audio_transcode_pcm".to_string());
    }
    if audio_encode.is_some() {
        summary.pipeline.push("audio_encode".to_string());
    }
    if video_encode.is_some() {
        summary.pipeline.push("video_encode".to_string());
    }
    if stat_desc
        .transport_mode
        .as_deref()
        .map(|t| t == "audio_302m")
        .unwrap_or(false)
    {
        summary.pipeline.push("audio_302m".to_string());
    }
    if summary.pipeline.is_empty() && (stat_desc.audio_passthrough || stat_desc.video_passthrough) {
        summary.pipeline.push("passthrough".to_string());
    }

    // ── Video fields ──────────────────────────────────────────────────────
    if !stat_desc.audio_only {
        if let Some(ve) = video_encode {
            summary.video_codec = Some(ve.output_codec.clone());
            if ve.output_width > 0 && ve.output_height > 0 {
                summary.video_resolution =
                    Some(format!("{}x{}", ve.output_width, ve.output_height));
            } else if let Some(v) = src_video {
                summary.video_resolution = v.resolution.clone();
            }
            if ve.output_fps > 0.0 {
                summary.video_fps = Some(ve.output_fps);
            } else if let Some(v) = src_video {
                summary.video_fps = v.frame_rate.map(|f| f as f32);
            }
            if ve.output_bitrate_kbps > 0 {
                summary.video_bitrate_kbps = Some(ve.output_bitrate_kbps);
            }
        } else if stat_desc.video_passthrough {
            if let Some(v) = src_video {
                summary.video_codec = Some(v.codec.clone());
                summary.video_resolution = v.resolution.clone();
                summary.video_fps = v.frame_rate.map(|f| f as f32);
                if v.bitrate_bps > 0 {
                    summary.video_bitrate_kbps = Some((v.bitrate_bps / 1000) as u32);
                }
            }
        }
    }

    // ── Audio fields ──────────────────────────────────────────────────────
    if let Some(ae) = audio_encode {
        summary.audio_codec = Some(ae.output_codec.clone());
        if ae.target_sample_rate_hz > 0 {
            summary.audio_sample_rate_hz = Some(ae.target_sample_rate_hz);
        }
        if ae.target_channels > 0 {
            summary.audio_channels = Some(ae.target_channels);
        }
        if ae.target_bitrate_kbps > 0 {
            summary.audio_bitrate_kbps = Some(ae.target_bitrate_kbps);
        }
    } else if let Some(ts) = transcode {
        // PCM transcode (no codec change) — describe the PCM output we know
        // about. The transcode block doesn't carry SR/channels, so fall back
        // to whatever the audio_decode handle reports.
        let _ = ts;
        if let Some(ad) = audio_decode {
            summary.audio_codec = Some("pcm".to_string());
            summary.audio_sample_rate_hz = Some(ad.output_sample_rate_hz);
            summary.audio_channels = Some(ad.output_channels);
        }
    } else if let Some(ad) = audio_decode {
        // Decode-only (no re-encode) — output is PCM at the decoded format.
        summary.audio_codec = Some("pcm".to_string());
        summary.audio_sample_rate_hz = Some(ad.output_sample_rate_hz);
        summary.audio_channels = Some(ad.output_channels);
    } else if stat_desc.audio_passthrough {
        if let Some(a) = src_audio {
            summary.audio_codec = Some(a.codec.clone());
            summary.audio_sample_rate_hz = a.sample_rate_hz;
            summary.audio_channels = a.channels;
            if a.bitrate_bps > 0 {
                summary.audio_bitrate_kbps = Some((a.bitrate_bps / 1000) as u32);
            }
        }
    }

    Some(summary)
}

/// Derive input connection state from counters.
/// Called during the 1/sec snapshot — zero hot-path impact.
fn derive_input_state(bitrate_bps: u64, packets_received: u64) -> String {
    if bitrate_bps > 0 {
        "receiving"
    } else if packets_received > 0 {
        "idle"
    } else {
        "waiting"
    }
    .to_string()
}

/// Derive output connection state from counters.
/// Called during the 1/sec snapshot — zero hot-path impact.
fn derive_output_state(bitrate_bps: u64, packets_sent: u64, packets_dropped: u64) -> String {
    if bitrate_bps > 0 {
        "active"
    } else if packets_sent > 0 {
        "idle"
    } else if packets_dropped > 0 {
        "dropping"
    } else {
        "waiting"
    }
    .to_string()
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
    /// Shared notifier fired whenever any `ThumbnailAccumulator::store()`
    /// writes a fresh JPEG (flow-level or per-input). The manager WS client
    /// awaits this so thumbnails forward within milliseconds of capture
    /// instead of waiting for a 5s poll tick. Safe to notify from any task;
    /// permits latch even if no consumer is waiting.
    pub thumbnail_update_notify: Arc<tokio::sync::Notify>,
}

impl StatsCollector {
    /// Create an empty stats collector.
    pub fn new() -> Self {
        Self {
            flow_stats: DashMap::new(),
            thumbnail_update_notify: Arc::new(tokio::sync::Notify::new()),
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
