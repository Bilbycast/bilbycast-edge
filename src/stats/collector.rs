// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, Ordering};
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
    /// Optional handle to the per-output `TsAudioReplacer` source-PID
    /// counters. Set once at output startup by outputs that build a
    /// `TsAudioReplacer`. Surfaces `source_audio_pid` /
    /// `source_audio_stream_type` on `AudioEncodeStatsSnapshot` so the
    /// manager UI can show "(from PID 0x0101)" on the audio-transcode
    /// badge.
    audio_replacer_stats: OnceLock<Arc<crate::engine::ts_audio_replace::TsAudioReplacerStats>>,
    /// Optional handle to the per-output video encode counters plus the
    /// resolved target codec / geometry descriptors. Set once at output
    /// startup by outputs that build an
    /// `engine::ts_video_replace::TsVideoReplacer`.
    video_encode_stats: OnceLock<VideoEncodeStatsHandle>,
    /// Optional handle to the per-output video decode counters plus the
    /// resolved source codec / geometry descriptors. Set once at output
    /// startup by outputs that drive a `video_engine::VideoDecoder` —
    /// either inside `engine::ts_video_replace::TsVideoReplacer` (true
    /// transcode, paired with [`Self::video_encode_stats`]) or as the
    /// terminal decoder in `engine::output_display` (decode-only).
    video_decode_stats: OnceLock<VideoDecodeStatsHandle>,
    /// Optional handle to the per-output local-display task's counters +
    /// chosen-mode descriptors. Set once at output startup by
    /// `engine::output_display`. Read by the snapshot path to populate
    /// [`crate::stats::models::DisplayStats`].
    display_stats: OnceLock<DisplayStatsHandle>,
    /// Optional handle to the per-output SDI playout worker's counters. Set
    /// once at output startup by `engine::output_sdi`. Read by the snapshot
    /// path to populate [`crate::stats::models::SdiOutputStats`].
    sdi_playout_stats: OnceLock<Arc<SdiPlayoutStats>>,
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

    // ── Per-output egress A/V mux-interleave metric ─────────────────
    // Fed inline by the output's send task for every 188-byte TS packet.
    // Self-bootstrapping: tracks PAT/PMT inline to discover video/audio
    // PIDs, then measures V−A PTS mux offset. Same reservoir pattern as
    // pcr_trust but signed ms values. NOT lip-sync — see stats::av_skew.
    av_interleave: crate::stats::av_interleave::AvInterleaveSampler,

    // ── Per-output edge-added A/V skew (output-local transcode) ─────
    // Set once by transcode_chain::build_for_output when this output
    // carries its own audio/video re-encode stages. Absent on plain
    // passthrough outputs (flow-level av_skew covers those).
    av_skew: std::sync::OnceLock<std::sync::Arc<crate::stats::av_skew::AvSkewReporter>>,

    // ── Wire pacing: tier + late-drop counter ────────────────────────
    // The active release-path tier for this output. Set once at output
    // startup by `engine::wire_emit::spawn_wire_emitter`. One of:
    //   "so_txtime" — Linux SO_TXTIME, kernel-paced
    //   "clock_nanosleep_fifo" — userspace SCHED_FIFO + clock_nanosleep
    //   "clock_nanosleep" — userspace SCHED_OTHER (no rt grant)
    //   "unpaced" — no pacing (probe-fail at high rates)
    pub wire_pacing_tier: OnceLock<String>,
    /// The egress pacing mode this output actually runs, resolved at
    /// spawn time. Explicit operator values report the bare mode
    /// ("forward" / "pcr" / "servo"); an unset (auto) field reports
    /// "auto (pcr)" / "auto (forward)" so an operator diagnosing a
    /// post-upgrade latency change can see what auto resolved to —
    /// the resolution depends on whether the flow had a bonded input
    /// at spawn. Only set on UDP/RTP-family outputs (the knob exists
    /// only there).
    pub egress_pacing_effective: OnceLock<String>,
    /// EOVERFLOW count from the SO_TXTIME error queue: kernel rejected
    /// the datagram because its target tx time landed in the past.
    /// Incremented from the wire thread on each errqueue drain. This is a
    /// SO_TXTIME-path signal and stays 0 on the default `clock_nanosleep`
    /// release path.
    pub wire_pacing_late: AtomicU64,
    /// Datagrams dropped by a **partial `sendmmsg` short-write** (kernel
    /// accepted fewer messages than submitted, e.g. transient ENOBUFS / socket
    /// buffer pressure on the batch send path). Distinct from `wire_pacing_late`
    /// (SO_TXTIME EOVERFLOW) so a socket-backpressure event on the default path
    /// is not misread as a PCR/pacing-lateness alarm. Distinct from
    /// `egress_shed` (residence-cap latency shed) and `packets_dropped`
    /// (broadcast-channel lag).
    pub wire_short_write: AtomicU64,
    /// CPU index the wire-emit thread is pinned to via
    /// `pthread_setaffinity_np` (operator-configured via
    /// `BILBYCAST_WIRE_EMIT_CPUS`). `-1` means not pinned (the kernel
    /// scheduler may move it across cores). Surfaced on
    /// `OutputStatsSnapshot` so the manager UI can show one row per
    /// pacing thread with its core assignment.
    pub wire_pacing_pinned_cpu: AtomicI32,
    /// Egress de-jitter: count of datagrams **shed** by the residence cap
    /// (compressed / `WirePacingClass::Lossless` outputs only). When the
    /// internal wire queue's oldest datagram has waited longer than the
    /// de-jitter residence cap (default 250 ms — a source-rate-vs-wallclock
    /// mismatch or burst backing the queue up), the emitter drops the stale
    /// backlog and re-anchors, bounding end-to-end latency by construction.
    /// Distinct from `packets_dropped` (broadcast-channel lag) and
    /// `wire_pacing_late` (SO_TXTIME kernel EOVERFLOW). The companion
    /// residence value is already surfaced via the `latency.{avg,max}_us`
    /// metric (`record_latency` = `now − recv_time`). See
    /// `engine::wire_emit::DejitterConfig` + `docs/egress-dejitter-design.md`.
    pub egress_shed: AtomicU64,
    /// Handle to the wire-emit thread's live queue-depth gauge (shared
    /// `Arc<AtomicUsize>` with the `WireTxHandle`/`WireTxReceiver`).
    /// Registered once at output startup by `wire_emit::spawn_wire_emitter`.
    /// Read (single atomic load) on snapshot to surface `wire_emit_depth`.
    wire_emit_depth: OnceLock<Arc<std::sync::atomic::AtomicUsize>>,

    // ── Display-output liveness ──────────────────────────────────────
    // Prev-sample tracker over the cumulative `frames_displayed` counter
    // (the display analogue of the byte-delta bitrate estimator that
    // decays network outputs to `idle`). Sampled only from `snapshot()`
    // — never on the render path. Without it a panel frozen mid-run
    // (e.g. an `UpdateFlowAssembly` hot-swap onto a dead slot, which
    // does NOT restart outputs) would report `active` forever off the
    // lifetime counter.
    display_frames_fresh: crate::stats::throughput::CounterFreshness,
    // Same prev-sample tracker over the audio decode stage's cumulative
    // `output_blocks` counter. Catches AUDIO-ONLY death on a display
    // output (video keeps rendering, ALSA goes silent — e.g. the source
    // drops its audio PID, or the demux stops producing audio PES):
    // `audio_underruns` only counts inside a write attempt, so a starved
    // audio task that never reaches `writei` registers nothing at all.
    display_audio_fresh: crate::stats::throughput::CounterFreshness,
}

/// A display output whose `frames_displayed` counter has not advanced for
/// this long reads as `idle` instead of `active` in [`derive_output_state`].
const DISPLAY_FRAMES_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// Registered handle to a per-output decode stage's counters plus the
/// steady-state descriptors the snapshot path needs to build a
/// [`crate::stats::models::DecodeStatsSnapshot`]. `input_codec` is wrapped
/// in `Mutex` so display outputs can update the label as the source codec
/// changes mid-flow (an input switch can flip AAC → MP2 → AC-3) without
/// re-registering the handle.
pub struct AudioDecodeStatsHandle {
    pub stats: Arc<crate::engine::audio_decode::DecodeStats>,
    pub input_codec: std::sync::Mutex<String>,
    pub output_sample_rate_hz: std::sync::atomic::AtomicU32,
    pub output_channels: std::sync::atomic::AtomicU8,
}

impl AudioDecodeStatsHandle {
    pub fn new(
        stats: Arc<crate::engine::audio_decode::DecodeStats>,
        input_codec: impl Into<String>,
        output_sample_rate_hz: u32,
        output_channels: u8,
    ) -> Self {
        Self {
            stats,
            input_codec: std::sync::Mutex::new(input_codec.into()),
            output_sample_rate_hz: std::sync::atomic::AtomicU32::new(output_sample_rate_hz),
            output_channels: std::sync::atomic::AtomicU8::new(output_channels),
        }
    }

    pub fn set_input_codec(&self, codec: impl Into<String>) {
        if let Ok(mut g) = self.input_codec.lock() {
            *g = codec.into();
        }
    }

    pub fn set_output_shape(&self, sample_rate_hz: u32, channels: u8) {
        use std::sync::atomic::Ordering;
        self.output_sample_rate_hz.store(sample_rate_hz, Ordering::Relaxed);
        self.output_channels.store(channels, Ordering::Relaxed);
    }

    fn snapshot_input_codec(&self) -> String {
        self.input_codec
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
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

/// Registered handle to a per-input/output video decode stage's counters
/// plus the source codec + decoded geometry descriptors. The atomic
/// counters live in [`crate::engine::video_decode_stats::VideoDecodeStats`];
/// the descriptors are wrapped in `Mutex<...>` so the registering task can
/// update them when the source codec / resolution / frame rate changes
/// mid-flow (e.g. an input switch on a Display output).
pub struct VideoDecodeStatsHandle {
    pub stats: Arc<crate::engine::video_decode_stats::VideoDecodeStats>,
    pub input_codec: std::sync::Mutex<String>,
    pub output_width: std::sync::atomic::AtomicU32,
    pub output_height: std::sync::atomic::AtomicU32,
    /// Frame rate stored as IEEE-754 bits via [`AtomicU32`] — same trick
    /// used by `engine::ts_video_replace::VideoEncodeStats` for atomic
    /// reads of a float.
    pub output_fps_bits: std::sync::atomic::AtomicU32,
}

impl VideoDecodeStatsHandle {
    pub fn new(stats: Arc<crate::engine::video_decode_stats::VideoDecodeStats>) -> Self {
        use std::sync::atomic::AtomicU32;
        Self {
            stats,
            input_codec: std::sync::Mutex::new(String::new()),
            output_width: AtomicU32::new(0),
            output_height: AtomicU32::new(0),
            output_fps_bits: AtomicU32::new(0),
        }
    }

    pub fn set_input_codec(&self, codec: impl Into<String>) {
        if let Ok(mut g) = self.input_codec.lock() {
            *g = codec.into();
        }
    }

    pub fn set_geometry(&self, width: u32, height: u32, fps: f32) {
        use std::sync::atomic::Ordering;
        self.output_width.store(width, Ordering::Relaxed);
        self.output_height.store(height, Ordering::Relaxed);
        self.output_fps_bits.store(fps.to_bits(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> crate::stats::models::VideoDecodeStatsSnapshot {
        use std::sync::atomic::Ordering;
        let input_codec = self
            .input_codec
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        crate::stats::models::VideoDecodeStatsSnapshot {
            input_frames: self.stats.input_frames.load(Ordering::Relaxed),
            output_frames: self.stats.output_frames.load(Ordering::Relaxed),
            decode_errors: self.stats.decode_errors.load(Ordering::Relaxed),
            input_codec,
            output_width: self.output_width.load(Ordering::Relaxed),
            output_height: self.output_height.load(Ordering::Relaxed),
            output_fps: f32::from_bits(self.output_fps_bits.load(Ordering::Relaxed)),
        }
    }
}

/// Discriminant table for the lock-free `video_codec_label` /
/// `audio_codec_label` atomics on [`DisplayStatsCounters`]. Stored as
/// `u8` so the demux + display tasks (separate threads) can update the
/// label without any locking, and the snapshot path can map it back to
/// the `&'static str` the manager expects without allocation.
#[allow(dead_code)]
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisplayCodecLabel {
    Unknown = 0,
    /// Audio-only sentinel: output has no `audio_device`, so audio is
    /// permanently silent by configuration (not a missing-codec bug).
    None = 1,
    H264 = 2,
    Hevc = 3,
    Mpeg2Video = 4,
    Aac = 5,
    Mp2 = 6,
    Ac3 = 7,
    Eac3 = 8,
    Opus = 9,
    /// Dolby AC-4. Surfaces on display outputs whose source is AC-4
    /// (ATSC 3.0 / DVB AC-4) so the manager UI labels the codec
    /// correctly. AC-4 has no open-source decoder; the display path
    /// renders video and silences audio (see
    /// [`crate::engine::ts_demux::SYNTHETIC_STREAM_TYPE_AC4`]).
    Ac4 = 10,
}

#[allow(dead_code)]
impl DisplayCodecLabel {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::None,
            2 => Self::H264,
            3 => Self::Hevc,
            4 => Self::Mpeg2Video,
            5 => Self::Aac,
            6 => Self::Mp2,
            7 => Self::Ac3,
            8 => Self::Eac3,
            9 => Self::Opus,
            10 => Self::Ac4,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::None => "none",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Mpeg2Video => "mpeg2",
            Self::Aac => "aac",
            Self::Mp2 => "mp2",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
            Self::Opus => "opus",
            Self::Ac4 => "ac4",
        }
    }
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
    /// Live panel mode, updated on every auto-match / modeset. The
    /// snapshot path prefers these (when nonzero) over the
    /// `DisplayStatsHandle` strings — the handle is a set-once
    /// `OnceLock`, so its mode fields freeze at the FIRST registration
    /// and silently misreport every later mode change (a 4K→1080p
    /// auto-match kept showing "3840x2160" for the rest of the
    /// session, 2026-06-11).
    pub panel_width: AtomicU64,
    pub panel_height: AtomicU64,
    pub panel_refresh_hz: AtomicU64,
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

    // ── HW-decode lifecycle (Section 1 of the display playback fix) ──
    /// Cumulative count of `VideoDecoder::send_packet_with_pts` calls
    /// that returned `Err`. Bumped from `feed_video_decoder` for every
    /// rejected access unit. Stays low on a healthy stream — sustained
    /// growth signals the decoder is unhappy with the bitstream or the
    /// HW backend can't actually decode (e.g. QSV opens but every send
    /// returns EINVAL on a brand-new iGPU with stale driver). Drives
    /// the runtime HW→CPU demotion in `force_cpu_fallback`.
    pub send_packet_errors: AtomicU64,
    /// Number of times this display run flipped the active decoder from
    /// a HW backend to CPU mid-flight (open-time fallback in
    /// `open_video_decoder_with_retry`, runtime fallback after
    /// `consecutive_send_errors` cap, or watchdog-triggered fallback in
    /// `display_loop`). Single per-flow open + immediate switch counts
    /// once. Manager UI flags `>0` as "HW degraded to CPU on this output".
    pub decoder_demotions: AtomicU64,
    /// Decoded frames pulled out of the active decoder since it was
    /// last opened. Reset to 0 by `force_cpu_fallback`. Watchdog reads
    /// this — `0` after `first_send_after_open` + 2.5 s while still on
    /// a HW backend means the decoder accepted packets but never
    /// produced a picture, so we demote.
    pub frames_received_since_open: AtomicU64,

    // ── Reolink + S4 4K diagnostics (Section 5) ──
    /// Times the demux loop detected a > 1 s PTS step and flushed the
    /// decoder. Bumped from the `pts_jump` arms in `demux_decode_loop`.
    /// Steady non-zero growth on Reolink (RTSP camera over SRT-FEC) is
    /// the canonical signal that FEC repair is producing out-of-order
    /// PTS that trip our flush heuristic — the targeted fix is a
    /// hysteresis on the jump detection.
    pub pts_jumps_observed: AtomicU64,
    /// Video access units the demux loop dropped because a decoder
    /// flush (startup, operator switch, broadcast Lagged, pts_jump) was
    /// pending and the AU was not a keyframe. A freshly-flushed decoder
    /// has no reference frames, so pre-IDR slices are guaranteed
    /// `send_packet` rejections — skipping them removes the
    /// AVERROR_INVALIDDATA warn-spam at every mid-GOP join AND the
    /// false HW→CPU demotion a long pre-IDR error run could trip.
    /// Steady growth bounded by one GOP per switch is healthy; sustained
    /// steady-state growth means the flush triggers are firing
    /// spuriously (audit `pts_jumps_observed` / `subscriber_lag_events`).
    pub aus_skipped_awaiting_keyframe: AtomicU64,
    /// Decoded frames whose `frame.pts()` was `None` (decoder didn't
    /// emit a display-order PTS), so the display loop fell back to the
    /// most recent input PTS. Bumped from `drain_video_frames`. Should
    /// stay near zero on B-frame-free sources; growth + degraded
    /// picture points at the B-frame display-PTS commit interacting
    /// badly with the source.
    pub frame_pts_fallbacks: AtomicU64,
    /// Decoded frames dropped because the decoder produced a pixel
    /// format the display path can't blit. Bumped per dropped frame in
    /// `drain_video_frames`. The 1-shot warning event still fires once
    /// (re-armed every 60 s by the periodic gate) but this counter
    /// gives the manager UI a per-second signal.
    pub frames_dropped_unsupported_pixfmt: AtomicU64,
    /// Times the broadcast subscriber returned `Lagged(n)` — bumped
    /// once per Lagged event, not per dropped packet. Sustained growth
    /// indicates the demux+decode child can't keep up with the input
    /// rate (heavy decode, slow scaler, etc.) and the broadcast bus's
    /// 2048-deep ring is overflowing.
    pub subscriber_lag_events: AtomicU64,

    /// Frames the demux+scale child dropped because the bounded
    /// `vtx` mpsc (display-feeder channel, currently 16 slots) was
    /// full when `try_send` ran. **This is the dominant signal that
    /// per-frame blit/present is too slow** — the demux side decoded
    /// the frame fine, the display side just didn't drain it in time.
    /// Distinguishes "decode is slow" (`subscriber_lag_events`) from
    /// "blit is slow" (this counter) from "frame arrived too late to
    /// show" (`frames_dropped_late`).
    pub frames_dropped_mpsc_full: AtomicU64,

    /// Decoded frames the display loop dropped because they were
    /// produced by the demux/decode child *before* the most recent
    /// switch / Lagged / pts_jump (the demux side bumps a shared
    /// `frame_gen` atomic in `flush_decoders_for_switch`; the display
    /// loop reads it on every recv). Distinct from
    /// `frames_dropped_mpsc_full` (queue-back-pressure → drop AT decode
    /// time) and `frames_dropped_late` (drift past audio clock → drop
    /// AT present time). Sustained growth around an operator-driven
    /// switch is healthy — that's the queued pre-switch frames being
    /// shed in microseconds instead of bleeding the previous stream's
    /// last second of decoded video onto the panel; sustained growth
    /// in steady state would indicate spurious flush triggers (audit
    /// the Lagged / pts_jump signals first).
    pub frames_dropped_stale_gen: AtomicU64,

    /// Audio blocks the demux+decode child dropped because the bounded
    /// `atx` mpsc (audio-task channel, `MPSC_AUDIO_DEPTH` slots) was
    /// full when `try_send` ran. Audio counterpart to
    /// `frames_dropped_mpsc_full`. Each silent drop is a hole in the
    /// audio timeline (~21 ms for AAC at 48 kHz, ~24 ms for MP2,
    /// ~32 ms for AC-3) that the `AudioClock` would otherwise advance
    /// over via the per-frame catch-up logic — but the catch-up only
    /// fires past `PTS_GAP_CATCHUP_MS` (50 ms by default), so single-
    /// frame drops accumulate as a slow positive `av_sync_offset_ms`
    /// (video drifting "ahead of" audio because audio_pts lags real
    /// time without ever crossing the catch-up threshold). Surfaced
    /// as a discrete signal so operators can distinguish "decode is
    /// fast and ALSA can't drain" from "audio decode is silently
    /// failing on some PIDs".
    pub audio_dropped_mpsc_full: AtomicU64,

    /// `true` when `KmsDisplay::enable_bars_overlay` succeeded at
    /// display-task startup. Without it the per-frame bars + header
    /// rasterise can never reach the panel: the bars + header live on
    /// a dedicated KMS overlay plane composed at vblank, and a host
    /// whose driver lacks a suitable plane (no atomic, no Overlay /
    /// Cursor plane on the CRTC, ARGB8888 unsupported, etc.) silently
    /// falls through with no overlay. Surfaced so an operator
    /// debugging "bars / overlay don't show up" can tell apart
    /// "rasterise produced nothing" from "compositor never saw
    /// anything to compose".
    pub bars_overlay_enabled: AtomicBool,

    /// Counts every `audio_meter::MeterPublisher::publish` — each one
    /// hands a fresh `Arc<MeterSnapshot>` to the display loop. Reads
    /// at zero mean the meter task is alive but never decoding any
    /// audio (broadcast Lagged, codec-decoder open failure, source
    /// stream type misclassified) — sustained zero with audio playing
    /// through ALSA fingers the meter as the broken side.
    pub meter_publishes: AtomicU64,


    /// Largest single `blit_and_present` duration since startup (µs).
    /// Includes libswscale colour-convert, optional HDR LUT, the
    /// audio-bars overlay, and the `kms.present()` vblank wait. On a
    /// 60 Hz panel a one-vblank flip is ~16 700 µs, so values north of
    /// ~33 000 µs mean we've missed a vblank slot at least once and
    /// are deferring the next iteration's pacing.
    pub blit_us_max: AtomicU64,
    /// Sum of `blit_and_present` durations since startup (µs). Read
    /// against `blit_count` for an average; the manager UI doesn't
    /// reset these mid-run.
    pub blit_us_total: AtomicU64,
    /// Count of `blit_and_present` invocations whose timing fed
    /// `blit_us_total`.
    pub blit_count: AtomicU64,

    /// Largest single decode-AU duration since startup, in µs. Wraps
    /// `send_packet_with_pts` + `drain_video_frames` (the synchronous
    /// libavcodec call plus the loop that pulls every reorder-buffer
    /// frame the AU made available, plus the YUV plane copies that
    /// move data out of the decoder's lifetime). Sustained values
    /// above the source frame period (40 ms at 25 fps) are the
    /// signature of "decode is slower than real-time on this content"
    /// — the failure mode that produces motion-heavy stutter even
    /// when blit and queue depth are healthy.
    pub decode_us_max: AtomicU64,
    /// Sum of decode-AU durations since startup (µs).
    pub decode_us_total: AtomicU64,
    /// Count of decode-AU invocations whose timing fed
    /// `decode_us_total`.
    pub decode_count: AtomicU64,

    /// Latest video codec the demuxer surfaced on this output, encoded
    /// as a `DisplayCodecLabel` discriminant. Updated on every received
    /// video frame so a mid-stream input switch (H.264 → HEVC) refreshes
    /// the manager-visible label without re-creating the stats handle.
    /// `0` (Unknown) until the first frame lands.
    pub video_codec_label: AtomicU8,
    /// Latest audio codec the demuxer surfaced on this output (or
    /// `None` when the output has no audio device configured). Same
    /// shape as `video_codec_label`.
    pub audio_codec_label: AtomicU8,
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
    pub fn set_video_codec_label(&self, label: DisplayCodecLabel) {
        self.video_codec_label.store(label as u8, Ordering::Relaxed);
    }
    pub fn set_audio_codec_label(&self, label: DisplayCodecLabel) {
        self.audio_codec_label.store(label as u8, Ordering::Relaxed);
    }
    pub fn load_video_codec_label(&self) -> DisplayCodecLabel {
        DisplayCodecLabel::from_u8(self.video_codec_label.load(Ordering::Relaxed))
    }
    pub fn load_audio_codec_label(&self) -> DisplayCodecLabel {
        DisplayCodecLabel::from_u8(self.audio_codec_label.load(Ordering::Relaxed))
    }
}

/// Registered handle to a per-output local-display task's counters plus
/// the steady-state descriptors (chosen mode) the snapshot path needs
/// to populate [`crate::stats::models::DisplayStats`]. Set once at
/// output startup. Codec labels live on `counters` so they can be
/// updated at runtime without re-creating the handle.
pub struct DisplayStatsHandle {
    pub counters: Arc<DisplayStatsCounters>,
    pub current_resolution: String,
    pub current_refresh_hz: u32,
    pub pixel_format: String,
    pub decoder_kind: String,
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
    /// Configured video-encode target (operator intent, taken from the
    /// `video_encode` block at startup). Surfaced in the egress summary as a
    /// fallback for the codec / resolution / bitrate fields when the live
    /// `VideoEncodeStatsSnapshot` hasn't reported yet — or never reports
    /// because the encoder failed to open (e.g. NVENC on a host with no
    /// NVIDIA driver). The companion `Critical` `Video encoder failed`
    /// event explains the runtime failure mode in that case.
    pub video_target_codec: Option<String>,
    pub video_target_width: Option<u32>,
    pub video_target_height: Option<u32>,
    pub video_target_fps: Option<f32>,
    pub video_target_bitrate_kbps: Option<u32>,
    pub video_target_encoder_backend: Option<String>,
    /// Configured audio-encode target (operator intent, taken from the
    /// `audio_encode` block at startup). Same fallback semantics as the
    /// video target above.
    pub audio_target_codec: Option<String>,
    pub audio_target_sample_rate_hz: Option<u32>,
    pub audio_target_channels: Option<u8>,
    pub audio_target_bitrate_kbps: Option<u32>,
}

impl EgressMediaSummaryStatic {
    /// Populate the `video_target_*` fields from the operator's
    /// `video_encode` config block. Run at output startup so the egress
    /// summary always reflects the configured target — even if the encoder
    /// fails to open at runtime (e.g. NVENC on a host without the NVIDIA
    /// driver), the operator still sees what the output is *meant to do*.
    pub fn with_video_encode_target(mut self, enc: &crate::config::models::VideoEncodeConfig) -> Self {
        let target_codec = match enc.codec.as_str() {
            "x264" | "h264_nvenc" | "h264_qsv" | "h264_vaapi" => "h264",
            "x265" | "hevc_nvenc" | "hevc_qsv" | "hevc_vaapi" => "hevc",
            other => other,
        };
        let backend = match enc.codec.as_str() {
            "x264" | "x265" => enc.codec.clone(),
            "h264_nvenc" | "hevc_nvenc" => "nvenc".to_string(),
            "h264_qsv" | "hevc_qsv" => "qsv".to_string(),
            "h264_vaapi" | "hevc_vaapi" => "vaapi".to_string(),
            other => other.to_string(),
        };
        self.video_target_codec = Some(target_codec.to_string());
        self.video_target_width = enc.width;
        self.video_target_height = enc.height;
        self.video_target_fps = match (enc.fps_num, enc.fps_den) {
            (Some(n), Some(d)) if d > 0 => Some(n as f32 / d as f32),
            _ => None,
        };
        self.video_target_bitrate_kbps = enc.bitrate_kbps;
        self.video_target_encoder_backend = Some(backend);
        self
    }

    /// Populate the `audio_target_*` fields from the operator's
    /// `audio_encode` config block. Same intent-vs-runtime contract as the
    /// video helper above.
    pub fn with_audio_encode_target(mut self, enc: &crate::config::models::AudioEncodeConfig) -> Self {
        self.audio_target_codec = Some(enc.codec.clone());
        self.audio_target_sample_rate_hz = enc.sample_rate;
        self.audio_target_channels = enc.channels;
        self.audio_target_bitrate_kbps = enc.bitrate_kbps;
        self
    }
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
            audio_replacer_stats: OnceLock::new(),
            video_encode_stats: OnceLock::new(),
            video_decode_stats: OnceLock::new(),
            display_stats: OnceLock::new(),
            sdi_playout_stats: OnceLock::new(),
            egress_static: OnceLock::new(),
            latency_min_us: AtomicU64::new(u64::MAX),
            latency_max_us: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            pcr_trust: crate::stats::pcr_trust::PcrTrustSampler::new(),
            av_interleave: crate::stats::av_interleave::AvInterleaveSampler::new(),
            av_skew: std::sync::OnceLock::new(),
            wire_pacing_tier: OnceLock::new(),
            egress_pacing_effective: OnceLock::new(),
            wire_pacing_late: AtomicU64::new(0),
            wire_short_write: AtomicU64::new(0),
            wire_pacing_pinned_cpu: AtomicI32::new(-1),
            egress_shed: AtomicU64::new(0),
            wire_emit_depth: OnceLock::new(),
            display_frames_fresh: crate::stats::throughput::CounterFreshness::new(),
            display_audio_fresh: crate::stats::throughput::CounterFreshness::new(),
        }
    }

    /// Register the active wire-pacing tier for this output. Called
    /// once at output startup by `engine::wire_emit`. Subsequent calls
    /// are no-ops (first wins).
    pub fn set_wire_pacing_tier(&self, tier: &'static str) {
        let _ = self.wire_pacing_tier.set(tier.to_string());
    }

    /// Register the resolved egress pacing mode for this output.
    /// Called once at output spawn by `engine::flow::start_output`
    /// (UDP/RTP-family only). Subsequent calls are no-ops (first wins).
    pub fn set_egress_pacing_effective(&self, label: String) {
        let _ = self.egress_pacing_effective.set(label);
    }

    /// Register the wire-emit queue-depth gauge so the snapshot path can
    /// surface live egress buffer occupancy (`wire_emit_depth`). Called
    /// once at output startup by `engine::wire_emit::spawn_wire_emitter`.
    pub fn set_wire_emit_depth_handle(&self, depth: Arc<std::sync::atomic::AtomicUsize>) {
        let _ = self.wire_emit_depth.set(depth);
    }

    /// Record the CPU index the wire-emit thread was pinned to (or
    /// `None` if the operator did not configure pinning, or pinning
    /// failed). Called once at thread spawn by `engine::wire_emit`.
    pub fn set_wire_pacing_pinned_cpu(&self, cpu: Option<u32>) {
        // i32::MAX is unreachable as a real CPU index; use -1 sentinel
        // for "not pinned" so snapshot consumers can distinguish from
        // a real pinning to CPU 0.
        let v = match cpu {
            Some(c) => c as i32,
            None => -1,
        };
        self.wire_pacing_pinned_cpu.store(v, Ordering::Relaxed);
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

    /// Feed one 188-byte TS packet at egress for A/V mux-interleave
    /// measurement. The sampler internally tracks PAT/PMT to discover
    /// video/audio PIDs, then measures V−A PTS mux offset. Call once per
    /// TS packet in the output's send loop.
    #[inline]
    pub fn observe_av_interleave_packet(&self, pkt: &[u8]) {
        self.av_interleave.observe_packet(pkt);
    }

    /// Borrow the A/V interleave sampler for flow-level rollup / reset.
    pub fn av_interleave_sampler(&self) -> &crate::stats::av_interleave::AvInterleaveSampler {
        &self.av_interleave
    }

    /// Register the output-local edge-added A/V skew reporter (set by
    /// `transcode_chain::build_for_output` when this output re-encodes
    /// audio and/or video). First call wins.
    pub fn set_av_skew_reporter(&self, r: std::sync::Arc<crate::stats::av_skew::AvSkewReporter>) {
        let _ = self.av_skew.set(r);
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
    ) -> &AudioDecodeStatsHandle {
        let handle = self.audio_decode_stats.get_or_init(|| {
            AudioDecodeStatsHandle::new(
                stats,
                input_codec.into(),
                output_sample_rate_hz,
                output_channels,
            )
        });
        // If the handle was already registered (e.g. a display output
        // re-registering on a codec switch), keep the live counters but
        // refresh the descriptors so the manager UI tracks reality.
        handle.set_output_shape(output_sample_rate_hz, output_channels);
        handle
    }

    /// Borrow the registered audio-decode handle, if any. Used by
    /// long-lived tasks (Display output) that need to update the source
    /// codec / output PCM shape whenever the upstream changes mid-flow.
    pub fn audio_decode_stats_handle(&self) -> Option<&AudioDecodeStatsHandle> {
        self.audio_decode_stats.get()
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

    /// Register the per-output `TsAudioReplacer` source-PID stats
    /// handle. Surfaces `source_audio_pid` + `source_audio_stream_type`
    /// on the audio_encode snapshot so the manager UI can show
    /// "(from PID 0x0101)" on the audio-transcode badge.
    pub fn set_audio_replacer_stats(
        &self,
        stats: Arc<crate::engine::ts_audio_replace::TsAudioReplacerStats>,
    ) {
        let _ = self.audio_replacer_stats.set(stats);
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

    /// Register the per-output video decode stats handle. Called once at
    /// output startup by outputs that drive a `video_engine::VideoDecoder` —
    /// either `engine::ts_video_replace::TsVideoReplacer` (true transcode)
    /// or `engine::output_display` (decode-only). Subsequent calls return
    /// the existing handle so the caller can keep updating the source-codec
    /// label and geometry on input switches without re-registering.
    pub fn set_video_decode_stats(
        &self,
        stats: Arc<crate::engine::video_decode_stats::VideoDecodeStats>,
        input_codec: impl Into<String>,
        output_width: u32,
        output_height: u32,
        output_fps: f32,
    ) -> &VideoDecodeStatsHandle {
        let handle = self
            .video_decode_stats
            .get_or_init(|| VideoDecodeStatsHandle::new(stats));
        handle.set_input_codec(input_codec);
        handle.set_geometry(output_width, output_height, output_fps);
        handle
    }

    /// Borrow the registered video decode handle, if any. Used by long-lived
    /// tasks (Display output) that need to update the source-codec / geometry
    /// fields whenever the upstream stream changes mid-flow.
    #[cfg(all(feature = "display", target_os = "linux"))]
    pub fn video_decode_stats_handle(&self) -> Option<&VideoDecodeStatsHandle> {
        self.video_decode_stats.get()
    }

    /// Register the per-output local-display counters + mode descriptors.
    /// Called once at startup by `engine::output_display::run` after
    /// modeset succeeds. Subsequent calls are no-ops (first wins).
    /// Codec labels are read live off `counters` (see
    /// `DisplayStatsCounters::set_{video,audio}_codec_label`) — they are
    /// not snapshotted into the handle.
    #[allow(dead_code)]
    pub fn set_display_stats(
        &self,
        counters: Arc<DisplayStatsCounters>,
        current_resolution: impl Into<String>,
        current_refresh_hz: u32,
        pixel_format: impl Into<String>,
        decoder_kind: impl Into<String>,
    ) {
        // The handle itself is set-once (the counters Arc + static
        // descriptors), but the panel mode changes at runtime
        // (auto-match modesets) — mirror it onto the counters atomics
        // so the snapshot path reports the LIVE mode, not the mode at
        // first registration.
        let resolution = current_resolution.into();
        if let Some((w, h)) = resolution
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u64>().ok()?, h.parse::<u64>().ok()?)))
        {
            counters.panel_width.store(w, Ordering::Relaxed);
            counters.panel_height.store(h, Ordering::Relaxed);
        }
        counters
            .panel_refresh_hz
            .store(current_refresh_hz as u64, Ordering::Relaxed);
        let _ = self.display_stats.set(DisplayStatsHandle {
            counters,
            current_resolution: resolution,
            current_refresh_hz,
            pixel_format: pixel_format.into(),
            decoder_kind: decoder_kind.into(),
        });
    }

    /// Borrow the registered display-stats handle (used by the snapshot
    /// path to populate `OutputStats.display_stats`).
    #[allow(dead_code)]
    pub fn display_stats_handle(&self) -> Option<&DisplayStatsHandle> {
        self.display_stats.get()
    }

    /// Register the SDI playout worker's lock-free counters. Set once at output
    /// startup; the snapshot path reads them into `OutputStats.sdi_stats`.
    #[allow(dead_code)]
    pub fn set_sdi_playout_stats(&self, stats: Arc<SdiPlayoutStats>) {
        let _ = self.sdi_playout_stats.set(stats);
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
                input_codec: h.snapshot_input_codec(),
                output_sample_rate_hz: h.output_sample_rate_hz.load(Ordering::Relaxed),
                output_channels: h.output_channels.load(Ordering::Relaxed),
            }
        });
        let audio_encode_stats = self.audio_encode_stats.get().map(|h| {
            // Source-PID telemetry comes from the TsAudioReplacer handle
            // when present. Subprocess-AudioEncoder outputs leave these
            // as 0 (no replacer in their data path); the snapshot's
            // serde-skip_serializing_if_zero handlers hide them in JSON
            // so the manager UI just sees `source_pid: undefined`.
            let (src_pid, src_stream_type) = self
                .audio_replacer_stats
                .get()
                .map(|s| (s.source_pid.load(Ordering::Relaxed), s.source_stream_type.load(Ordering::Relaxed)))
                .unwrap_or((0, 0));
            crate::stats::models::EncodeStatsSnapshot {
                pcm_frames_submitted: h.stats.pcm_frames_submitted.load(Ordering::Relaxed),
                pcm_frames_dropped: h.stats.pcm_frames_dropped.load(Ordering::Relaxed),
                encoded_frames_out: h.stats.encoded_frames_out.load(Ordering::Relaxed),
                supervisor_restarts: h.stats.supervisor_restarts.load(Ordering::Relaxed),
                output_codec: h.output_codec.clone(),
                target_sample_rate_hz: h.target_sample_rate_hz,
                target_channels: h.target_channels,
                target_bitrate_kbps: h.target_bitrate_kbps,
                source_pid: src_pid,
                source_stream_type: src_stream_type,
            }
        });
        let video_decode_stats = self.video_decode_stats.get().map(|h| h.snapshot());
        let video_encode_stats = self.video_encode_stats.get().map(|h| {
            // Prefer the actually-opened backend (after Auto-chain
            // demote) over the requested-codec label captured at
            // output start. `None` until the encoder lazy-opens.
            let encoder_backend = h
                .stats
                .resolved_backend
                .label()
                .map(str::to_owned)
                .unwrap_or_else(|| h.encoder_backend.clone());
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
                encoder_backend,
                last_latency_us: h.stats.last_latency_us.load(Ordering::Relaxed),
                supervisor_restarts: h.stats.supervisor_restarts.load(Ordering::Relaxed),
                source_pid: h.stats.source_pid.load(Ordering::Relaxed),
                source_stream_type: h.stats.source_stream_type.load(Ordering::Relaxed),
            }
        });
        // Audio-only stall on a display output: the decode stage delivered
        // blocks at some point, then the counter froze for the stall
        // window. The `> 0` guard keeps a legitimately video-only source
        // from reading as stalled (it never had audio to lose).
        let display_audio_blocks = audio_decode_stats
            .as_ref()
            .map(|a| a.output_blocks)
            .unwrap_or(0);
        let display_audio_stalled = display_audio_blocks > 0
            && !self
                .display_audio_fresh
                .advanced_within(display_audio_blocks, DISPLAY_FRAMES_STALL_WINDOW);
        let display_stats = self.display_stats.get().map(|h| crate::stats::models::DisplayStats {
            frames_displayed: h.counters.frames_displayed.load(Ordering::Relaxed),
            frames_dropped_late: h.counters.frames_dropped_late.load(Ordering::Relaxed),
            frames_repeated: h.counters.frames_repeated.load(Ordering::Relaxed),
            audio_underruns: h.counters.audio_underruns.load(Ordering::Relaxed),
            audio_stalled: display_audio_stalled,
            av_sync_offset_ms: h.counters.load_av_offset_ms(),
            current_resolution: {
                let w = h.counters.panel_width.load(Ordering::Relaxed);
                let ph = h.counters.panel_height.load(Ordering::Relaxed);
                if w > 0 && ph > 0 {
                    format!("{w}x{ph}")
                } else {
                    h.current_resolution.clone()
                }
            },
            current_refresh_hz: {
                let hz = h.counters.panel_refresh_hz.load(Ordering::Relaxed);
                if hz > 0 { hz as u32 } else { h.current_refresh_hz }
            },
            pixel_format: h.pixel_format.clone(),
            decoder_kind: h.decoder_kind.clone(),
            video_codec: h.counters.load_video_codec_label().as_str().to_string(),
            audio_codec: h.counters.load_audio_codec_label().as_str().to_string(),
            send_packet_errors: h.counters.send_packet_errors.load(Ordering::Relaxed),
            decoder_demotions: h.counters.decoder_demotions.load(Ordering::Relaxed),
            frames_received_since_open: h
                .counters
                .frames_received_since_open
                .load(Ordering::Relaxed),
            pts_jumps_observed: h.counters.pts_jumps_observed.load(Ordering::Relaxed),
            aus_skipped_awaiting_keyframe: h
                .counters
                .aus_skipped_awaiting_keyframe
                .load(Ordering::Relaxed),
            frame_pts_fallbacks: h.counters.frame_pts_fallbacks.load(Ordering::Relaxed),
            frames_dropped_unsupported_pixfmt: h
                .counters
                .frames_dropped_unsupported_pixfmt
                .load(Ordering::Relaxed),
            subscriber_lag_events: h.counters.subscriber_lag_events.load(Ordering::Relaxed),
            frames_dropped_mpsc_full: h
                .counters
                .frames_dropped_mpsc_full
                .load(Ordering::Relaxed),
            frames_dropped_stale_gen: h
                .counters
                .frames_dropped_stale_gen
                .load(Ordering::Relaxed),
            audio_dropped_mpsc_full: h
                .counters
                .audio_dropped_mpsc_full
                .load(Ordering::Relaxed),
            bars_overlay_enabled: h
                .counters
                .bars_overlay_enabled
                .load(Ordering::Relaxed),
            meter_publishes: h
                .counters
                .meter_publishes
                .load(Ordering::Relaxed),
            blit_us_max: h.counters.blit_us_max.load(Ordering::Relaxed),
            blit_us_avg: {
                let total = h.counters.blit_us_total.load(Ordering::Relaxed);
                let count = h.counters.blit_count.load(Ordering::Relaxed);
                if count == 0 { 0 } else { total / count }
            },
            decode_us_max: h.counters.decode_us_max.load(Ordering::Relaxed),
            decode_us_avg: {
                let total = h.counters.decode_us_total.load(Ordering::Relaxed);
                let count = h.counters.decode_count.load(Ordering::Relaxed);
                if count == 0 { 0 } else { total / count }
            },
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
        let display_frames_displayed =
            display_stats.as_ref().map(|d| d.frames_displayed).unwrap_or(0);
        // Frames flipped recently — the display "bitrate". The `> 0` guard
        // keeps the tracker's startup window (last_advance = construction
        // time) from reading a never-displayed output as fresh.
        let display_frames_advancing = display_frames_displayed > 0
            && self
                .display_frames_fresh
                .advanced_within(display_frames_displayed, DISPLAY_FRAMES_STALL_WINDOW);

        OutputStats {
            output_id: self.output_id.clone(),
            output_name: self.output_name.clone(),
            output_type: self.output_type.clone(),
            state: derive_output_state(
                bitrate_bps,
                packets_sent,
                packets_dropped,
                display_frames_displayed,
                display_frames_advancing,
            ),
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
            video_decode_stats,
            latency,
            // Filled in by `FlowStatsAccumulator::snapshot()` so it can merge
            // the flow's input MediaAnalysis with this output's per-stage
            // stats. Leaving as None here keeps the per-output snapshot
            // self-contained.
            egress_summary: None,
            pcr_trust: self.pcr_trust.snapshot(),
            av_interleave: self.av_interleave.snapshot(),
            av_skew: self.av_skew.get().map(|r| r.snapshot()),
            display_stats,
            sdi_stats: self.sdi_playout_stats.get().map(|h| h.snapshot()),
            wire_pacing_tier: self.wire_pacing_tier.get().cloned(),
            egress_pacing_effective: self.egress_pacing_effective.get().cloned(),
            wire_pacing_late: self.wire_pacing_late.load(Ordering::Relaxed),
            wire_short_write: self.wire_short_write.load(Ordering::Relaxed),
            wire_pacing_pinned_cpu: {
                let v = self.wire_pacing_pinned_cpu.load(Ordering::Relaxed);
                if v < 0 { None } else { Some(v as u32) }
            },
            egress_shed: self.egress_shed.load(Ordering::Relaxed),
            wire_emit_depth: self
                .wire_emit_depth
                .get()
                .map(|d| d.load(Ordering::Relaxed) as u64),
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
    /// Sliding window of (pcr_27mhz, wall_us_since_anchor) samples for the
    /// regression-based PCR accuracy check. The first sample's wall time is
    /// the anchor (stored as the first element with wall_us_since_anchor=0).
    /// Capped at PCR_HISTORY_LEN entries; oldest sample is dropped on insert.
    pub history: std::collections::VecDeque<(u64, u64)>,
    /// Anchor wall-time for the regression window. All wall_us values in
    /// `history` are deltas relative to this anchor (avoids u64 overflow on
    /// long-running streams when squaring wall_us during regression).
    pub history_anchor: Instant,
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
    /// A/V elementary stream PIDs discovered from PMT, mapped to their
    /// last-seen time. Used for PID error detection (P1): ES PIDs that
    /// stop appearing within `PAT_PMT_TIMEOUT`. Data-only stream types
    /// (DSM-CC carousels, private sections) are excluded — they live in
    /// `pmt_referenced_data_pids` so they don't fire false-positive
    /// pid_errors / pts_errors but still count as PMT-referenced for
    /// the unreferenced-PID check.
    pub es_pids: HashMap<u16, Option<Instant>>,
    /// Data-only PIDs referenced by a PMT (DSM-CC carousels, private
    /// sections — see `engine::tr101290::is_data_only_stream_type`).
    /// Tracked separately from `es_pids` so they don't fire false-positive
    /// `pid_error` / `pts_error` alarms designed for A/V QoS, but still
    /// excused from the unreferenced-PID check (they ARE in the PMT;
    /// TR 101 290 just doesn't apply A/V timeout semantics to them).
    pub pmt_referenced_data_pids: std::collections::HashSet<u16>,
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
    /// Latch of PMT PIDs that have already been counted as timed-out so the
    /// 500 ms sweep doesn't re-fire one error per PID per tick forever
    /// (legacy bug — `pmt_errors` accumulated ~2 errors/sec/orphan).
    /// Cleared when the PID is seen on the wire again or removed from PAT.
    pub pmt_errored: std::collections::HashSet<u16>,
    /// Same latch but for ES PIDs. See `pmt_errored`.
    pub es_errored: std::collections::HashSet<u16>,
    /// Per-ES-PID continuous-media flag from the PMT (stream_type +
    /// descriptor classification — 0x06 counts only when descriptor-tagged
    /// as DVB audio). Only continuous PIDs feed the persistent
    /// `missing_continuous_pids` gauge: sparse essences (teletext,
    /// subtitles, SCTE-35) latch `es_errored` for the one-shot counter but
    /// must never hold flow health in Error. Pruned in lockstep with
    /// `es_pids`.
    pub es_pid_continuous: HashMap<u16, bool>,
    /// Latch for `pcr_repetition_errors` — same one-fire-per-stall semantic.
    pub pcr_repetition_errored: std::collections::HashSet<u16>,
    /// Latch for `pts_errors` — same one-fire-per-stall semantic.
    pub pts_errored: std::collections::HashSet<u16>,
    /// First time the PAT was observed. Used as the anchor for the
    /// startup-grace window: PMT/ES `None`-state entries don't count as
    /// errors until at least PAT_PMT_TIMEOUT × 2 has elapsed since the
    /// PAT first arrived, so a freshly-discovered PMT entry isn't tagged
    /// just because it hasn't shown up yet.
    pub first_pat_time: Option<Instant>,
}

impl Default for Tr101290State {
    fn default() -> Self {
        Self {
            cc_tracker: HashMap::new(),
            last_pat_time: None,
            pat_seen: false,
            pmt_pids: HashMap::new(),
            es_pids: HashMap::new(),
            pmt_referenced_data_pids: std::collections::HashSet::new(),
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
            pmt_errored: std::collections::HashSet::new(),
            es_errored: std::collections::HashSet::new(),
            pcr_repetition_errored: std::collections::HashSet::new(),
            pts_errored: std::collections::HashSet::new(),
            first_pat_time: None,
            es_pid_continuous: HashMap::new(),
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

    // ── Gauges (current state, not counters) ──
    /// Number of PMT-referenced CONTINUOUS-media PIDs (video/audio,
    /// descriptor-classified for 0x06) currently latched missing in
    /// `es_errored`. Unlike the windowed `pid_errors` counter — which
    /// fires once per stall and lets `priority1_ok` recover within one
    /// snapshot — this gauge holds while the PID stays absent, so a flow
    /// whose advertised video/audio never arrives (PSI-only assembled
    /// output, dead passthrough PID) reads Error persistently instead of
    /// flickering for ~1 s. Sparse essences are excluded by construction.
    pub missing_continuous_pids: AtomicU64,

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
            missing_continuous_pids: AtomicU64::new(0),
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
        // `missing_continuous_pids` is a GAUGE, not a windowed counter: it
        // holds priority1_ok false for as long as an advertised video/audio
        // PID stays absent (TR 101 290 P1 PID_error is a state, not an
        // edge). Without it, the one-shot latched `pid_errors` lets P1
        // recover within one snapshot while the PID is still missing.
        let missing_continuous = self.missing_continuous_pids.load(Ordering::Relaxed);
        let priority1_ok = in_sync
            && w_cc == 0
            && w_pat == 0
            && w_pmt == 0
            && w_pid == 0
            && missing_continuous == 0;
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
            missing_continuous_pids: missing_continuous,
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

    pub fn reset_counters(&self) {
        self.ts_packets_analyzed.store(0, Ordering::Relaxed);
        self.pat_count.store(0, Ordering::Relaxed);
        self.pmt_count.store(0, Ordering::Relaxed);
        self.sync_loss_count.store(0, Ordering::Relaxed);
        self.sync_byte_errors.store(0, Ordering::Relaxed);
        self.cc_errors.store(0, Ordering::Relaxed);
        self.pat_errors.store(0, Ordering::Relaxed);
        self.pmt_errors.store(0, Ordering::Relaxed);
        self.pid_errors.store(0, Ordering::Relaxed);
        self.tei_errors.store(0, Ordering::Relaxed);
        self.crc_errors.store(0, Ordering::Relaxed);
        self.pcr_discontinuity_errors.store(0, Ordering::Relaxed);
        self.pcr_accuracy_errors.store(0, Ordering::Relaxed);
        self.pts_errors.store(0, Ordering::Relaxed);
        self.cat_errors.store(0, Ordering::Relaxed);
        self.pcr_repetition_errors.store(0, Ordering::Relaxed);
        self.nit_errors.store(0, Ordering::Relaxed);
        self.si_repetition_errors.store(0, Ordering::Relaxed);
        self.unreferenced_pid_errors.store(0, Ordering::Relaxed);
        self.sdt_errors.store(0, Ordering::Relaxed);
        self.eit_errors.store(0, Ordering::Relaxed);
        self.rst_errors.store(0, Ordering::Relaxed);
        self.tdt_errors.store(0, Ordering::Relaxed);
        self.window_cc_errors.store(0, Ordering::Relaxed);
        self.window_pat_errors.store(0, Ordering::Relaxed);
        self.window_pmt_errors.store(0, Ordering::Relaxed);
        self.window_pid_errors.store(0, Ordering::Relaxed);
        self.window_tei_errors.store(0, Ordering::Relaxed);
        self.window_crc_errors.store(0, Ordering::Relaxed);
        self.window_pcr_discontinuity_errors.store(0, Ordering::Relaxed);
        self.window_pcr_accuracy_errors.store(0, Ordering::Relaxed);
        self.window_pts_errors.store(0, Ordering::Relaxed);
        self.window_cat_errors.store(0, Ordering::Relaxed);
        self.window_pcr_repetition_errors.store(0, Ordering::Relaxed);
        self.window_nit_errors.store(0, Ordering::Relaxed);
        self.window_si_repetition_errors.store(0, Ordering::Relaxed);
        self.window_unreferenced_pid_errors.store(0, Ordering::Relaxed);
        self.window_sdt_errors.store(0, Ordering::Relaxed);
        self.window_eit_errors.store(0, Ordering::Relaxed);
        self.window_rst_errors.store(0, Ordering::Relaxed);
        self.window_tdt_errors.store(0, Ordering::Relaxed);
        *self.state.lock().unwrap() = Tr101290State::default();
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
    /// Cached LATM `StreamMuxConfig` for AAC-LATM PIDs (`stream_type 0x11`).
    /// Latched from the first PES with `useSameStreamMux=0` and reused on
    /// subsequent frames where `useSameStreamMux=1` (the dominant case in
    /// real broadcasts — full config is only re-emitted on multiplexer
    /// reconfiguration). Without this, the detector would keep returning
    /// "detecting…" because real LATM streams almost never carry the full
    /// config in every frame.
    pub latm_cached: Option<LatmCachedConfig>,
}

/// Cached LATM `StreamMuxConfig` payload — only the fields the dashboard
/// surfaces are kept (sample_rate / channels / profile_name).
#[derive(Clone, Debug)]
pub struct LatmCachedConfig {
    pub sample_rate: u32,
    pub channels: u8,
    pub profile_name: String,
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
    /// Most recent capture-failure reason. Replaced (not accumulated) on
    /// every error so the snapshot always reflects the *latest* failure
    /// mode. Cleared on the next successful capture so a transient failure
    /// doesn't linger across recovery. Surfaced on `ThumbnailStats` so
    /// operators can tell "no buffered frames" from "decoder rejected the
    /// AU" without enabling debug logging.
    last_error: Mutex<Option<String>>,
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
    /// Live-configurable capture cadence (seconds). `0` == use the generator's
    /// spawn-time default; `1..=60` == an operator override applied without a
    /// respawn. The thumbnail loop re-reads this each iteration and rebuilds
    /// its interval when it changes (see `engine::thumbnail::thumbnail_loop`),
    /// so changing the cadence on a running flow takes effect immediately
    /// rather than at the next flow restart.
    configured_interval_secs: AtomicU32,
}

impl ThumbnailAccumulator {
    pub fn new() -> Self {
        Self {
            latest_jpeg: Mutex::new(None),
            generation: AtomicU64::new(0),
            configured_interval_secs: AtomicU32::new(0),
            total_captured: AtomicU64::new(0),
            capture_errors: AtomicU64::new(0),
            last_error: Mutex::new(None),
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

    fn encode_interval(secs: Option<u32>) -> u32 {
        secs.map(|s| s.clamp(1, 60)).unwrap_or(0)
    }

    /// Seed the live cadence to the generator's spawn-time value **without**
    /// waking the loop. Called once at loop start so the first re-read matches.
    pub fn init_configured_interval(&self, secs: Option<u32>) {
        self.configured_interval_secs
            .store(Self::encode_interval(secs), Ordering::Relaxed);
    }

    /// Change the live capture cadence and wake the generator so it applies
    /// immediately rather than on the next (possibly slow) tick. `None` resets
    /// to the spawn-time default. Safe to call from any task.
    pub fn set_configured_interval(&self, secs: Option<u32>) {
        self.configured_interval_secs
            .store(Self::encode_interval(secs), Ordering::Relaxed);
        self.refresh_trigger.notify_one();
    }

    /// The current live cadence — `None` means the spawn-time default.
    pub fn configured_interval(&self) -> Option<u32> {
        match self.configured_interval_secs.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Store a newly captured thumbnail.
    pub fn store(&self, jpeg_data: bytes::Bytes) {
        *self.latest_jpeg.lock().unwrap() = Some((jpeg_data, Instant::now()));
        // A successful capture clears any lingering error reason so the
        // snapshot doesn't show "no video frames buffered" forever after
        // a transient warm-up gap.
        *self.last_error.lock().unwrap() = None;
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.total_captured.fetch_add(1, Ordering::Relaxed);
        if let Some(n) = &self.update_notify {
            n.notify_one();
        }
    }

    /// Record a capture error and replace the last-error reason.
    pub fn record_error(&self, reason: impl Into<String>) {
        self.capture_errors.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(reason.into());
    }

    /// Check whether the current JPEG hash matches the previous one and
    /// update the freeze counter accordingly. Returns `true` when the
    /// frame has been identical for `threshold` consecutive captures. The
    /// caller derives `threshold` from the configured capture cadence (see
    /// `engine::thumbnail::resolve_thumbnail_timing`) so "frozen" fires at a
    /// consistent ~30 s wall-clock window regardless of interval.
    pub fn check_freeze(&self, jpeg_hash: u64, threshold: u64) -> bool {
        let mut prev = self.prev_jpeg_hash.lock().unwrap();
        if *prev == Some(jpeg_hash) {
            let count = self.freeze_count.fetch_add(1, Ordering::Relaxed) + 1;
            count >= threshold
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
        let last_error = self.last_error.lock().unwrap().clone();
        ThumbnailStats {
            enabled: true,
            total_captured: self.total_captured.load(Ordering::Relaxed),
            capture_errors: self.capture_errors.load(Ordering::Relaxed),
            has_thumbnail,
            alarm,
            last_error,
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
///
/// Registered via
/// [`FlowStatsAccumulator::register_input_counters_with_psi_catalog`].
pub struct PerInputCounters {
    pub input_type: String,
    pub bytes: AtomicU64,
    pub packets: AtomicU64,
    pub throughput: ThroughputEstimator,
    /// Consecutive failed connect attempts on the input's caller-mode
    /// connection (SRT caller today). `Arc` so the input task's retry
    /// observer can hold the counter directly; written from the retry
    /// loop (once per back-off tick), read by the 1 Hz snapshot — never
    /// on the packet path. At or above
    /// [`crate::srt::connection::CONNECT_FAILED_THRESHOLD`] the input's
    /// reported state becomes `"connect_failed"`; reset to 0 on a
    /// successful connect.
    pub connect_failures: Arc<AtomicU32>,
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
        Self::with_psi_catalog(
            input_type,
            meta,
            Arc::new(crate::engine::ts_psi_catalog::PsiCatalogStore::new()),
        )
    }

    /// Build a [`PerInputCounters`] backed by an externally-supplied
    /// catalogue store. Used by `FlowRuntime::start` so the per-input
    /// PSI catalogue lives on the node-wide [`crate::engine::manager::FlowManager`]
    /// publisher entry — and is therefore reachable by sibling flows'
    /// Essence-slot resolvers via
    /// [`crate::engine::manager::FlowManager::psi_catalog_for_input`]
    /// (PES Switch Phase 2.2b). The owning flow's PSI observer task
    /// still writes to the same `Arc`; both the snapshot path and any
    /// foreign resolver read identical contents.
    pub fn with_psi_catalog(
        input_type: String,
        meta: InputConfigMeta,
        psi_catalog: Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>,
    ) -> Self {
        Self {
            input_type,
            bytes: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            throughput: ThroughputEstimator::new(),
            connect_failures: Arc::new(AtomicU32::new(0)),
            meta,
            psi_catalog,
            psi_catalog_cache: std::sync::Mutex::new((0, None)),
        }
    }

    /// Whether this input has crossed the consecutive-connect-failure
    /// threshold — the snapshot path maps `true` to state
    /// `"connect_failed"`.
    pub fn connect_failed(&self) -> bool {
        self.connect_failures.load(Ordering::Relaxed)
            >= crate::srt::connection::CONNECT_FAILED_THRESHOLD
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
    /// Operator-initiated reset. Analyser tasks poll this at the top of
    /// each sample interval; when true, they reinitialise internal state
    /// and flip it back to false.
    pub reset_requested: AtomicBool,
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
            reset_requested: AtomicBool::new(false),
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

    pub fn reset_counters(&self) {
        self.lite_drops.store(0, Ordering::Relaxed);
        self.audio_full_drops.store(0, Ordering::Relaxed);
        self.video_full_drops.store(0, Ordering::Relaxed);
        *self.lite.lock().unwrap() = None;
        *self.audio_full.lock().unwrap() = None;
        *self.video_full.lock().unwrap() = None;
        self.reset_requested.store(true, Ordering::Relaxed);
    }
}

impl Default for ContentAnalysisAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

// ── Per-flow Accumulator ───────────────────────────────────────────────────

/// Lock-free SDI capture telemetry for one input, written by the capture
/// thread and read by the snapshot path.
///
/// Deliberately holds no DeckLink types, so it compiles without the
/// `sdi-decklink` feature and the collector needs no `cfg` gates.
#[derive(Debug, Default)]
pub struct SdiCaptureStats {
    /// Card is locked to an input signal (`!bmdFrameHasNoInputSource`).
    /// Starts `false` — an input that has never received a frame is not
    /// locked, and reporting `true` before the first frame would be a lie.
    pub signal_present: AtomicBool,
    /// Cumulative signal-loss transitions.
    pub signal_losses: AtomicU64,
    /// Cumulative frames dropped in the capture shim (consumer fell behind).
    pub frames_dropped: AtomicU64,
    /// Capture sessions opened (raster change / device re-open increments).
    pub sessions: AtomicU64,
}

impl SdiCaptureStats {
    /// Snapshot into the serialisable form the manager sees.
    pub fn snapshot(&self) -> SdiInputStats {
        SdiInputStats {
            signal_present: self.signal_present.load(Ordering::Relaxed),
            signal_losses: self.signal_losses.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
        }
    }
}

/// Lock-free SDI playout telemetry for one output, written by the playout
/// worker and read by the snapshot path. Like [`SdiCaptureStats`], it holds no
/// DeckLink types so the collector needs no `cfg` gate.
#[derive(Debug, Default)]
pub struct SdiPlayoutStats {
    /// Video frames successfully scheduled onto the card.
    pub frames_sent: AtomicU64,
    /// Frames the card displayed late (soft — presented behind their slot).
    pub frames_late: AtomicU64,
    /// Frames dropped — never presented (hard loss).
    pub frames_dropped: AtomicU64,
}

impl SdiPlayoutStats {
    /// Snapshot into the serialisable form the manager sees.
    pub fn snapshot(&self) -> crate::stats::models::SdiOutputStats {
        crate::stats::models::SdiOutputStats {
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            frames_late: self.frames_late.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
        }
    }
}

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
    /// Cumulative source-side timing-field discontinuities observed
    /// in the ingress stream — file-loop boundaries on upstream
    /// senders (e.g. ffmpeg `-stream_loop -1 -c copy`), encoder
    /// restarts, decoder reseats. Aggregated across PCR + PTS + DTS
    /// jumps > 500 ms; individual per-code events still ride the
    /// events feed (`source_pcr_discontinuity` /
    /// `source_pts_discontinuity` / `source_dts_discontinuity`) for
    /// the operator. The counter is the running total so the manager
    /// UI's flow card can show "12 source discontinuities since
    /// start" alongside the recent-events panel.
    pub source_discontinuities: AtomicU64,
    /// Latest snapshot of the buffered SMPTE 2022-7 merger, when the
    /// input runs in industry-standard buffered mode. The redundant
    /// RTP listener publishes to this from the data-path task at the
    /// natural drain cadence; the snapshot path just clones it.
    pub buffered_hitless_snapshot:
        std::sync::RwLock<Option<crate::stats::models::BufferedHitlessSnapshot>>,
    /// Ingress de-jitter telemetry, keyed by `input_id` (one entry per
    /// de-jittered raw UDP / RTP input). The snapshot surfaces the **active**
    /// input's `shed` + `depth` on its `InputStats` via an `active_key`
    /// lookup — same per-input model as `input_video_decode_stats` et al.
    /// Keeping it per-input (not a single flow-shared counter) avoids
    /// conflating sheds and mis-reporting the buffer depth across multiple
    /// de-jittered inputs in one flow. Registered by
    /// [`crate::engine::ingress_dejitter::start`].
    pub ingress_dejitter_stats:
        DashMap<String, Arc<crate::engine::ingress_dejitter::IngressDejitterStats>>,
    /// SDI (DeckLink) capture telemetry, keyed by `input_id`. Same per-input
    /// model as `ingress_dejitter_stats`. Registered by
    /// `crate::engine::sdi_io::spawn_sdi_input`. Not feature-gated: the handle
    /// is plain atomics with no DeckLink dependency, so builds without
    /// `sdi-decklink` simply leave the map empty.
    pub sdi_capture_stats: DashMap<String, Arc<SdiCaptureStats>>,
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
    /// `register_input_counters_with_psi_catalog` and incremented by the
    /// shared per-input forwarder — so every input's liveness (bitrate,
    /// packets_received) is tracked regardless of whether the input is
    /// currently switched active.
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
    /// Per-input-id edge-added A/V skew reporters (`stats::av_skew`).
    /// Each input's PTS-touching stages (rewriter trim, audio/video
    /// transcode) publish their (output − source) PTS deltas here; the
    /// snapshot reads the entry keyed by `active_input_id`.
    av_skew_reporters: DashMap<String, Arc<crate::stats::av_skew::AvSkewReporter>>,
    /// Per-input-id map of AAC decode stage handles + descriptors. Wrapped
    /// in `Arc` so the owning ingress replacer can keep a clone and refresh
    /// the source-codec label whenever the PMT learns a new stream_type
    /// (input switch, MPTS program change) without going through the
    /// flow accumulator.
    input_audio_decode_stats: DashMap<String, Arc<AudioDecodeStatsHandle>>,
    /// Per-input-id map of audio encode stage handles + target codec descriptors.
    input_audio_encode_stats: DashMap<String, Arc<AudioEncodeStatsHandle>>,
    /// Per-input-id map of video encode stage handles + target codec / geometry
    /// descriptors. Populated by ST 2110-20/-23 inputs or by Group A inputs
    /// that configured a `video_encode` block (via the `InputTranscoder`).
    input_video_encode_stats: DashMap<String, VideoEncodeStatsHandle>,
    /// Per-input-id map of video decode stage handles. Populated by Group A
    /// inputs whose `InputTranscoder` composer decodes the source video
    /// before re-encoding it on the ingest leg.
    input_video_decode_stats: DashMap<String, Arc<VideoDecodeStatsHandle>>,
    /// Per-input-id map of static ingress-summary descriptors. The dynamic
    /// codec/format fields are merged in at `FlowStatsAccumulator::snapshot()`.
    ingress_static: DashMap<String, EgressMediaSummaryStatic>,
    /// PID-bus Phase 8: per-elementary-stream counters keyed by
    /// `(input_id, source_pid)`. Populated incrementally by the
    /// per-ES analyzer task (see `engine::ts_es_analysis`) that
    /// subscribes to each `NodeEsBus` channel. Surfaced via
    /// `FlowStats.per_es` on assembled flows; absent on passthrough
    /// flows (which have no `NodeEsBus` to read from).
    pub per_es_stats: DashMap<(String, u16), Arc<PerEsAccumulator>>,
    /// Snapshot of the running assembler's `(input_id, source_pid) → out_pid`
    /// routing. Updated by `FlowRuntime::replace_assembly` after every
    /// runtime swap. Used by `snapshot()` to annotate `PerEsStats.out_pid`
    /// so operators can pivot their trust signals off egress PID. Empty
    /// on passthrough flows.
    pub pid_routing: std::sync::RwLock<std::collections::HashMap<(String, u16), u16>>,
    /// Per-flow master-clock telemetry. Updated by `FlowRuntime::start`
    /// at flow bring-up and on every lipsync trim change. Read by the
    /// snapshot path into `FlowStats.master_clock`. Behind a `RwLock` so
    /// the (rare) write doesn't add a hot-path atomic.
    pub master_clock_state:
        std::sync::RwLock<Option<crate::stats::models::MasterClockStats>>,
    /// Per-slot assembly liveness, published at 1 Hz by the running TS
    /// assembler (`engine::ts_assembler`). Stays `None` on passthrough
    /// flows so `FlowStats.assembly_health` is absent for them. `Arc`'d
    /// because the assembler runs on its own dedicated thread.
    pub assembly_health: Arc<crate::engine::ts_assembler::AssemblyHealthCell>,
}

/// Per-elementary-stream accumulator. One instance per `(input_id, source_pid)`
/// channel on the flow's `NodeEsBus`. Atomic counters — updated inline by
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
            source_discontinuities: AtomicU64::new(0),
            buffered_hitless_snapshot: std::sync::RwLock::new(None),
            ingress_dejitter_stats: DashMap::new(),
            sdi_capture_stats: DashMap::new(),
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
            av_skew_reporters: DashMap::new(),
            input_transcode_stats: DashMap::new(),
            input_audio_decode_stats: DashMap::new(),
            input_audio_encode_stats: DashMap::new(),
            input_video_encode_stats: DashMap::new(),
            input_video_decode_stats: DashMap::new(),
            ingress_static: DashMap::new(),
            per_es_stats: DashMap::new(),
            pid_routing: std::sync::RwLock::new(std::collections::HashMap::new()),
            master_clock_state: std::sync::RwLock::new(None),
            assembly_health: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Replace the master-clock telemetry. Called by `FlowRuntime` at
    /// start-up, on every lipsync trim change, and at every 1 Hz stats
    /// tick by the master itself (so PLL convergence is visible in the
    /// manager UI without a re-bring-up).
    pub fn set_master_clock_telemetry(
        &self,
        telemetry: crate::engine::master_clock::MasterClockTelemetry,
    ) {
        let lipsync = 0i64; // overwritten by handle in FlowRuntime
        let stats = crate::stats::models::MasterClockStats {
            kind: telemetry.kind,
            locked: telemetry.locked,
            rate_offset_ppm: telemetry.rate_offset_ppm,
            jitter_us: telemetry.jitter_us,
            lipsync_offset_90k: lipsync,
            configured_kind: telemetry.configured_kind,
            fallback_active: telemetry.fallback_active,
            fallback_reason: telemetry.fallback_reason,
            active_input_id: telemetry.active_input_id,
        };
        if let Ok(mut g) = self.master_clock_state.write() {
            *g = Some(stats);
        }
    }

    /// Replace just the lipsync trim component of the master-clock
    /// telemetry. Called by the WS dispatcher when an operator nudges
    /// the trim knob.
    #[allow(dead_code)]
    pub fn set_master_clock_lipsync(&self, lipsync_offset_90k: i64) {
        if let Ok(mut g) = self.master_clock_state.write() {
            if let Some(s) = g.as_mut() {
                s.lipsync_offset_90k = lipsync_offset_90k;
            }
        }
    }

    pub fn reset_counters(&self, scope: &str) {
        match scope {
            "tr101290" => {
                if let Some(tr) = self.tr101290.get() {
                    tr.reset_counters();
                }
            }
            "content_analysis" => {
                if let Some(ca) = self.content_analysis.get() {
                    ca.reset_counters();
                }
            }
            _ => {
                if let Some(tr) = self.tr101290.get() {
                    tr.reset_counters();
                }
                if let Some(ca) = self.content_analysis.get() {
                    ca.reset_counters();
                }
            }
        }
    }

    /// Snapshot accessor used by the snapshot path.
    pub fn master_clock_snapshot(&self) -> Option<crate::stats::models::MasterClockStats> {
        self.master_clock_state.read().ok().and_then(|g| g.clone())
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

    /// Register an input-side audio decode stats handle. Returns the
    /// `Arc<AudioDecodeStatsHandle>` so the registering ingress replacer
    /// can refresh `input_codec` / output PCM shape whenever the PMT
    /// re-learns the source codec (input switch, MPTS program change).
    pub fn set_input_decode_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::audio_decode::DecodeStats>,
        input_codec: impl Into<String>,
        output_sample_rate_hz: u32,
        output_channels: u8,
    ) -> Arc<AudioDecodeStatsHandle> {
        let handle = Arc::new(AudioDecodeStatsHandle::new(
            stats,
            input_codec,
            output_sample_rate_hz,
            output_channels,
        ));
        self.input_audio_decode_stats
            .insert(input_id.to_string(), handle.clone());
        handle
    }

    /// Register an input-side audio encode stats handle. Returns the
    /// `Arc<AudioEncodeStatsHandle>` for symmetry with the decode setter
    /// (callers ignore the return today; the handle is fully described at
    /// registration time).
    pub fn set_input_encode_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::audio_encode::EncodeStats>,
        output_codec: impl Into<String>,
        target_sample_rate_hz: u32,
        target_channels: u8,
        target_bitrate_kbps: u32,
    ) -> Arc<AudioEncodeStatsHandle> {
        let handle = Arc::new(AudioEncodeStatsHandle {
            stats,
            output_codec: output_codec.into(),
            target_sample_rate_hz,
            target_channels,
            target_bitrate_kbps,
        });
        self.input_audio_encode_stats
            .insert(input_id.to_string(), handle.clone());
        handle
    }

    /// Register an input-side video decode stats handle. Called at flow start
    /// by Group A inputs whose `InputTranscoder` composer decodes the source
    /// before re-encoding it. The handle is shared so the registering task
    /// can keep updating `input_codec` / `output_*` whenever the upstream
    /// changes mid-flow.
    #[allow(dead_code)]
    pub fn set_input_video_decode_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::video_decode_stats::VideoDecodeStats>,
        input_codec: impl Into<String>,
        output_width: u32,
        output_height: u32,
        output_fps: f32,
    ) -> Arc<VideoDecodeStatsHandle> {
        let handle = Arc::new(VideoDecodeStatsHandle::new(stats));
        handle.set_input_codec(input_codec);
        handle.set_geometry(output_width, output_height, output_fps);
        self.input_video_decode_stats
            .insert(input_id.to_string(), handle.clone());
        handle
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
        let changed = self
            .active_input_id
            .read()
            .map(|g| *g != id)
            .unwrap_or(true);
        if let Ok(mut guard) = self.active_input_id.write() {
            *guard = id.to_string();
        }
        if changed {
            // Input switch: the old input's interleave geometry and skew
            // history are meaningless for the new input — reset so the
            // dashboard never averages across the switch (the old
            // lifetime average converging post-switch read as "drift",
            // 2026-06-06).
            for out_entry in self.output_stats.iter() {
                out_entry.value().av_interleave_sampler().reset();
            }
            if let Some(r) = self.av_skew_reporters.get(id) {
                r.value().reset_worst();
            }
        }
    }

    /// Snapshot the ACTIVE input's edge-added A/V skew (for the
    /// per-flow quality watcher — avoids building a full FlowStats).
    pub fn active_av_skew_snapshot(&self) -> Option<crate::stats::models::AvSkewStats> {
        let active = self
            .active_input_id
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        self.av_skew_reporters
            .get(&active)
            .map(|r| r.value().snapshot())
    }

    /// Worst per-output A/V interleave windowed p95 across all outputs
    /// (for the per-flow quality watcher).
    pub fn worst_av_interleave_window_p95_ms(&self) -> Option<i64> {
        let mut worst: Option<i64> = None;
        for out_entry in self.output_stats.iter() {
            if let Some(snap) = out_entry.value().av_interleave_sampler().snapshot() {
                worst = Some(worst.map_or(snap.window_p95_abs_ms, |w: i64| {
                    w.max(snap.window_p95_abs_ms)
                }));
            }
        }
        worst
    }

    /// Drop an input's edge-added A/V skew reporter. Called by
    /// `FlowRuntime::remove_input` — without this, re-adding the same
    /// input id with its transcode stage removed resurrects the old
    /// incarnation's frozen `mode="measured"` skew forever (adversarial
    /// review 2026-06-06).
    pub fn remove_av_skew_reporter(&self, input_id: &str) {
        self.av_skew_reporters.remove(input_id);
    }

    /// Get-or-create the edge-added A/V skew reporter for an input.
    /// Called by each input task at startup; the same Arc is handed to
    /// every PTS-touching stage on that input's chain.
    pub fn av_skew_reporter_for_input(
        &self,
        input_id: &str,
    ) -> Arc<crate::stats::av_skew::AvSkewReporter> {
        self.av_skew_reporters
            .entry(input_id.to_string())
            .or_insert_with(|| Arc::new(crate::stats::av_skew::AvSkewReporter::new()))
            .clone()
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

    /// Register a per-input ingress de-jitter telemetry handle (keyed by
    /// `input_id`) so the snapshot can surface the active input's shed +
    /// buffer depth. Called once per de-jittered input at startup by
    /// [`crate::engine::ingress_dejitter::start`]. Idempotent on re-register
    /// (e.g. hot re-add) — the latest handle wins.
    pub fn set_ingress_dejitter_stats(
        &self,
        input_id: &str,
        stats: Arc<crate::engine::ingress_dejitter::IngressDejitterStats>,
    ) {
        self.ingress_dejitter_stats.insert(input_id.to_string(), stats);
    }

    /// Register a per-input SDI capture telemetry handle (keyed by `input_id`)
    /// so the snapshot can surface the active input's signal lock, shim frame
    /// drops, and session count. Called once per SDI input at startup by
    /// `crate::engine::sdi_io::spawn_sdi_input`. Idempotent on re-register
    /// (e.g. hot re-add) — the latest handle wins.
    ///
    /// The only caller lives behind both `sdi-decklink` and `media-codecs`;
    /// the field and snapshot stay unconditional so the collector needs no
    /// `cfg` gates.
    #[cfg_attr(
        not(all(feature = "sdi-decklink", feature = "media-codecs")),
        allow(dead_code)
    )]
    pub fn set_sdi_capture_stats(&self, input_id: &str, stats: Arc<SdiCaptureStats>) {
        self.sdi_capture_stats.insert(input_id.to_string(), stats);
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
    /// Register per-input liveness counters for a flow. The PSI
    /// catalogue store is supplied by the caller — the owning flow
    /// passes the same `Arc` it just got from
    /// [`crate::engine::manager::FlowManager::register_input_publisher`]
    /// so the catalogue is reachable cross-flow for foreign Essence
    /// resolvers (PES Switch Phase 2.2b).
    pub fn register_input_counters_with_psi_catalog(
        &self,
        input_id: &str,
        input_type: &str,
        meta: InputConfigMeta,
        psi_catalog: Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>,
    ) -> Arc<PerInputCounters> {
        let c = Arc::new(PerInputCounters::with_psi_catalog(
            input_type.to_string(),
            meta,
            psi_catalog,
        ));
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

        // Flow-level "input" stats are the SUM across every running input
        // (active + passive) by design — the manager UI surfaces this on
        // the flow card as "total bandwidth across all configured inputs"
        // so an operator can see passive inputs are warm and ready to
        // take over. Per-input rates are still available via
        // `inputs_live[]` for the per-input drill-down view.
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
                out.video_decode_stats.as_ref(),
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
        let assembly_health_snap = self.assembly_health.read().ok().and_then(|g| g.clone());
        let (health, health_reasons) = derive_flow_health(
            input_bitrate,
            packets_lost,
            &tr101290_snap,
            bw_exceeded,
            bw_blocked,
            self.bandwidth_limit_mbps.get().copied(),
            assembly_health_snap.as_ref(),
        );

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
                // Resolve the per-input thumbnail accumulator once and read
                // both the alarm (legacy field, preserved for older managers)
                // and the full snapshot (new diagnostic field) from the same
                // entry — avoids two DashMap lookups per input.
                let (thumbnail_alarm, thumbnail) = self
                    .per_input_thumbnails
                    .get(&input_id)
                    .map(|t| (t.current_alarm(), Some(t.snapshot())))
                    .unwrap_or((None, None));
                // Per-input, not just the active one: an SDI input reports a
                // pulled cable only through the card, and a passive leg with
                // no signal is exactly what the operator needs to see before
                // cutting to it. Absent entry = not an SDI input, which stays
                // `None` — never `false`.
                let signal_present = self
                    .sdi_capture_stats
                    .get(&input_id)
                    .map(|h| h.signal_present.load(Ordering::Relaxed));
                PerInputLive {
                    input_id,
                    input_type: c.input_type.clone(),
                    state: derive_input_state(bitrate, packets, c.connect_failed()),
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
                    signal_present,
                    psi_catalog,
                    psi_catalog_tick: if psi_tick == 0 { None } else { Some(psi_tick) },
                    thumbnail_alarm,
                    thumbnail,
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

        // Flow-level A/V mux-interleave rollup. Pick the output with the
        // worst |p95| — gives operators "the most problematic output".
        let av_interleave_flow = {
            let mut worst: Option<crate::stats::models::AvInterleaveStats> = None;
            for out_entry in self.output_stats.iter() {
                if let Some(snap) = out_entry.value().av_interleave_sampler().snapshot() {
                    let dominated = worst
                        .as_ref()
                        .is_none_or(|w| snap.p95_abs_ms.abs() > w.p95_abs_ms.abs());
                    if dominated {
                        worst = Some(snap);
                    }
                }
            }
            worst
        };

        // Flow-level edge-added A/V skew: the ACTIVE input's reporter
        // (each input owns its own PTS-stage chain; passive inputs keep
        // processing and must not leak into the live number).
        let av_skew = {
            let active = self
                .active_input_id
                .read()
                .map(|g| g.clone())
                .unwrap_or_default();
            self.av_skew_reporters
                .get(&active)
                .map(|r| r.value().snapshot())
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
                            input_codec: h.snapshot_input_codec(),
                            output_sample_rate_hz: h.output_sample_rate_hz.load(Ordering::Relaxed),
                            output_channels: h.output_channels.load(Ordering::Relaxed),
                        }
                    });
                let in_audio_encode =
                    self.input_audio_encode_stats.get(active_key).map(|h| {
                        // Input-side source-PID telemetry isn't wired yet
                        // (no per-input audio replacer stats handle on
                        // FlowStatsAccumulator). Surface zeros so the
                        // snapshot serialises consistently with the
                        // output-side variant — manager UI sees no badge.
                        crate::stats::models::EncodeStatsSnapshot {
                            pcm_frames_submitted: h.stats.pcm_frames_submitted.load(Ordering::Relaxed),
                            pcm_frames_dropped: h.stats.pcm_frames_dropped.load(Ordering::Relaxed),
                            encoded_frames_out: h.stats.encoded_frames_out.load(Ordering::Relaxed),
                            supervisor_restarts: h.stats.supervisor_restarts.load(Ordering::Relaxed),
                            output_codec: h.output_codec.clone(),
                            target_sample_rate_hz: h.target_sample_rate_hz,
                            target_channels: h.target_channels,
                            target_bitrate_kbps: h.target_bitrate_kbps,
                            source_pid: 0,
                            source_stream_type: 0,
                        }
                    });
                let in_video_decode = self
                    .input_video_decode_stats
                    .get(active_key)
                    .map(|h| h.value().snapshot());
                let in_video_encode =
                    self.input_video_encode_stats.get(active_key).map(|h| {
                        // Prefer the actually-opened backend (after
                        // Auto-chain demote) over the requested-codec
                        // label. `None` until the encoder lazy-opens.
                        let encoder_backend = h
                            .stats
                            .resolved_backend
                            .label()
                            .map(str::to_owned)
                            .unwrap_or_else(|| h.encoder_backend.clone());
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
                            encoder_backend,
                            last_latency_us: h.stats.last_latency_us.load(Ordering::Relaxed),
                            supervisor_restarts: h.stats.supervisor_restarts.load(Ordering::Relaxed),
                            source_pid: h.stats.source_pid.load(Ordering::Relaxed),
                            source_stream_type: h.stats.source_stream_type.load(Ordering::Relaxed),
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
                    in_video_decode.as_ref(),
                );

                let total_input_packets = self.input_packets.load(Ordering::Relaxed);
                // Flow-level state mirrors the ACTIVE input's connect-failure
                // latch. NB: `input_bitrate` is the sum across every running
                // input, so sibling traffic can still mask a dead active input
                // here — `inputs_live[]` is the per-input honest surface.
                let active_connect_failed = self
                    .per_input_counters
                    .get(active_key)
                    .map(|c| c.connect_failed())
                    .unwrap_or(false);
                InputStats {
                    input_type,
                    state: derive_input_state(input_bitrate, total_input_packets, active_connect_failed),
                    mode: meta.and_then(|m| m.mode.clone()),
                    local_addr: meta.and_then(|m| m.local_addr.clone()),
                    remote_addr: meta.and_then(|m| m.remote_addr.clone()),
                    listen_addr: meta.and_then(|m| m.listen_addr.clone()),
                    bind_addr: meta.and_then(|m| m.bind_addr.clone()),
                    rtsp_url: meta.and_then(|m| m.rtsp_url.clone()),
                    whep_url: meta.and_then(|m| m.whep_url.clone()),
                    packets_received: total_input_packets,
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
                    source_discontinuities: self
                        .source_discontinuities
                        .load(Ordering::Relaxed),
                    buffered_hitless: self
                        .buffered_hitless_snapshot
                        .read()
                        .ok()
                        .and_then(|g| g.clone()),
                    ingress_dejitter_shed: self
                        .ingress_dejitter_stats
                        .get(active_key)
                        .map(|h| h.shed.load(Ordering::Relaxed))
                        .unwrap_or(0),
                    ingress_buffer_depth: self
                        .ingress_dejitter_stats
                        .get(active_key)
                        .map(|h| h.depth.load(Ordering::Relaxed) as u64),
                    sdi_stats: self.sdi_capture_stats.get(active_key).map(|h| h.snapshot()),
                    transcode_stats: in_transcode,
                    audio_decode_stats: in_audio_decode,
                    audio_encode_stats: in_audio_encode,
                    video_encode_stats: in_video_encode,
                    video_decode_stats: in_video_decode,
                    ingress_summary,
                }
            },
            outputs,
            uptime_secs: self.started_at.elapsed().as_secs(),
            tr101290: tr101290_snap,
            health,
            health_reasons,
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
            av_interleave_flow,
            av_skew,
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
            master_clock: self.master_clock_snapshot(),
            assembly_health: self.assembly_health.read().ok().and_then(|g| g.clone()),
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
    /// Per-path bitrate estimator, sampled once per snapshot (1 Hz)
    /// off the path's directional byte counter. The bonding crate
    /// declares `PathStats::throughput_bps` but never writes it, so
    /// the edge derives the rate here — same delta/elapsed×8 model the
    /// per-input/output bitrate uses.
    pub bitrate_est: Arc<crate::stats::throughput::ThroughputEstimator>,
    /// Per-path FEC-repair bitrate estimator — sampled off
    /// `PathStats::fec_bytes_sent` the same 1 Hz delta/elapsed×8 way as
    /// `bitrate_est`. Surfaces the proactive redundancy the media counter
    /// deliberately omits. Sender side only.
    pub fec_bitrate_est: Arc<crate::stats::throughput::ThroughputEstimator>,
    /// Per-path true-wire bitrate estimator — sampled off
    /// `PathStats::wire_bytes_sent` (media + retx + dup + FEC + AEAD
    /// envelope). The honest gross wire load of the leg. Sender side only.
    pub wire_bitrate_est: Arc<crate::stats::throughput::ThroughputEstimator>,
    /// True when this leg uses gateway-mode path selection (the edge
    /// programmed a policy route). Reported as `binding = "gateway"`;
    /// otherwise the resolved NIC-pin mechanism from `PathStats` is
    /// reported.
    pub gateway_mode: bool,
    /// Shared handle to the adaptive scheduler's discovered capacity for
    /// this leg (bits/sec). `None` for the non-adaptive policies, which
    /// don't model per-leg capacity.
    pub capacity_est: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Kernel netdev this leg egresses on (interface-mode UDP legs). Surfaced
    /// as `BondPathLegStats.interface` so the manager UI can join a leg to its
    /// cellular radio state. `None` for gateway-mode / QUIC / RIST / receiver.
    pub interface: Option<String>,
    /// Relay tunnel id (UUID) for a `transport == "relay"` leg. Surfaced as
    /// `BondPathLegStats.tunnel_id` so the manager can join this leg to the
    /// relay `udp_session` forwarding it. `None` for direct legs.
    pub tunnel_id: Option<String>,
}

impl BondPathStatsHandle {
    /// Build a path handle, allocating its bitrate estimator. The
    /// estimator lives behind an `Arc` so the handle stays `Clone` and
    /// every snapshot of the same path shares one rate-sampling window.
    pub fn new(
        id: u8,
        name: String,
        transport: String,
        stats: Arc<bonding_protocol::stats::PathStats>,
    ) -> Self {
        Self {
            id,
            name,
            transport,
            stats,
            bitrate_est: Arc::new(crate::stats::throughput::ThroughputEstimator::new()),
            fec_bitrate_est: Arc::new(crate::stats::throughput::ThroughputEstimator::new()),
            wire_bitrate_est: Arc::new(crate::stats::throughput::ThroughputEstimator::new()),
            gateway_mode: false,
            capacity_est: None,
            interface: None,
            tunnel_id: None,
        }
    }

    /// Mark this leg as gateway-mode (builder style).
    pub fn with_gateway_mode(mut self, gateway: bool) -> Self {
        self.gateway_mode = gateway;
        self
    }

    /// Attach the kernel netdev this leg egresses on (builder style).
    pub fn with_interface(mut self, interface: Option<String>) -> Self {
        self.interface = interface;
        self
    }

    /// Attach the relay tunnel id for a relay leg (builder style).
    pub fn with_tunnel_id(mut self, tunnel_id: Option<String>) -> Self {
        self.tunnel_id = tunnel_id;
        self
    }

    /// Attach the adaptive scheduler's per-leg capacity handle (builder).
    pub fn with_capacity_est(
        mut self,
        cap: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) -> Self {
        self.capacity_est = cap;
        self
    }
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
    /// Sender side: payloads over the per-datagram MTU budget, counted
    /// by the bonded output's send loop (the bond layer neither drops
    /// nor fragments them). Stays 0 on the receiver side.
    pub oversize_payloads: Arc<std::sync::atomic::AtomicU64>,
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
            // Derive the per-path rate off the byte counter for this
            // bond's active direction. Cached for 1 s inside the
            // estimator, so multiple consumers (WS + Prometheus + API)
            // within a tick reuse one computed value.
            let throughput_bps = match h.role {
                BondStatsRole::Sender => p.bitrate_est.sample(ps.bytes_sent),
                BondStatsRole::Receiver => p.bitrate_est.sample(ps.bytes_received),
            };
            // FEC repair + true-wire rates are sender-side concepts (FEC
            // is emitted by the sender; the wire counter is the send
            // direction). On the receiver side they stay 0 — the UI shows
            // these columns sender-side only.
            let (fec_throughput_bps, wire_throughput_bps) = match h.role {
                BondStatsRole::Sender => (
                    p.fec_bitrate_est.sample(ps.fec_bytes_sent),
                    p.wire_bitrate_est.sample(ps.wire_bytes_sent),
                ),
                BondStatsRole::Receiver => (0, 0),
            };
            BondPathLegStats {
                id: p.id,
                name: p.name.clone(),
                transport: p.transport.clone(),
                state: if ps.dead { "dead" } else { "alive" }.to_string(),
                rtt_ms: ps.rtt_ms(),
                jitter_us: ps.jitter_us,
                relative_owd_us: ps.relative_owd_us,
                loss_fraction: ps.loss_fraction(),
                throughput_bps,
                fec_throughput_bps,
                wire_throughput_bps,
                // Receiver-fed delivered rate (sender side): the capacity
                // controller's ground truth, now written by the bonding
                // crate from the v2 keepalive byte feedback.
                delivered_bps: ps.throughput_bps,
                // Adaptive scheduler's discovered capacity for this leg
                // (0 for non-adaptive policies / the receiver side).
                capacity_bps: p
                    .capacity_est
                    .as_ref()
                    .map(|a| a.load(Ordering::Relaxed))
                    .unwrap_or(0),
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
                rebuilds: ps.rebuilds,
                fec_recovered: ps.fec_recovered,
                // Gateway-mode legs report "gateway"; interface-mode
                // legs report the kernel primitive that actually bound
                // them (so_bindtodevice vs the unprivileged hint).
                binding: if p.gateway_mode {
                    "gateway".to_string()
                } else {
                    ps.pin_mechanism_label().to_string()
                },
                // Interface-mode legs carry the egress netdev so the UI can
                // join the leg to its cellular radio state; gateway-mode legs
                // leave it `None` (the "gateway" binding label tells that story).
                interface: if p.gateway_mode {
                    None
                } else {
                    p.interface.clone()
                },
                // Relay legs carry their tunnel id so the manager can join the
                // leg to the relay session forwarding it. `None` for direct legs.
                tunnel_id: p.tunnel_id.clone(),
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

    // Aggregate bond bandwidth = sum of per-path realized throughput in
    // the active direction. This is gross bytes on the wire across all
    // legs, not the unique/delivered payload rate: on the sender a
    // duplicated `Critical` IDR counts on each path it rides; on the
    // receiver a Broadcast-group bond counts the same packet on every
    // leg before reassembly dedup. Goodput is visible separately via the
    // aggregate `packets_delivered` / `duplicates_received` counters.
    let throughput_bps: u64 = paths.iter().map(|p| p.throughput_bps).sum();
    // Bond-wide redundancy + true-wire rollups: sum the per-leg rates so
    // the aggregate decomposes the same way each leg does (media vs FEC
    // vs total wire). Sender side only (per-leg values are 0 on receive).
    let fec_throughput_bps: u64 = paths.iter().map(|p| p.fec_throughput_bps).sum();
    let wire_throughput_bps: u64 = paths.iter().map(|p| p.wire_throughput_bps).sum();

    BondLegStats {
        state,
        flow_id: h.flow_id,
        role,
        scheduler: h.scheduler.clone(),
        throughput_bps,
        fec_throughput_bps,
        wire_throughput_bps,
        // Discovered aggregate usable capacity (sum of alive legs' estimates) —
        // the operator-facing "available bonded bitrate" for provisioning a
        // fixed external encoder. Sender side only (0 on the receiver).
        aggregate_capacity_bps: snap.aggregate_capacity_bps,
        // The receiver hold servo owns `current_hold_ms` (written on
        // init + every retarget); senders never write it, so report it
        // receiver-side only rather than a misleading 0.
        hold_ms: match h.role {
            BondStatsRole::Receiver => Some(snap.current_hold_ms),
            BondStatsRole::Sender => None,
        },
        session_resets: snap.session_resets,
        oversize_payloads: h.oversize_payloads.load(Ordering::Relaxed),
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
        late_stale_drops: snap.late_stale_drops,
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
#[allow(clippy::too_many_arguments)]
fn build_pipeline_summary(
    stat_desc: Option<&EgressMediaSummaryStatic>,
    program_number: Option<u16>,
    media_analysis: Option<&crate::stats::models::MediaAnalysisStats>,
    transcode: Option<&crate::stats::models::TranscodeStatsSnapshot>,
    audio_decode: Option<&crate::stats::models::DecodeStatsSnapshot>,
    audio_encode: Option<&crate::stats::models::EncodeStatsSnapshot>,
    video_encode: Option<&crate::stats::models::VideoEncodeStatsSnapshot>,
    video_decode: Option<&crate::stats::models::VideoDecodeStatsSnapshot>,
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
    //
    // Tags are intent-driven, not stats-driven: as soon as the output is
    // configured for `audio_encode` / `video_encode`, the badge surfaces
    // even before the first encode-stats snapshot lands (and stays surfaced
    // if the encoder fails to spawn — e.g. NVENC on a host without the
    // NVIDIA driver). This keeps the manager's pipeline indicator
    // symmetric across output types — passthrough-configured outputs
    // (UDP/RTP/SRT/RIST without an encode block) show "Passthrough";
    // transcode-configured outputs show "Audio/Video Transcode" the moment
    // the config arrives.
    if program_number.is_some() {
        summary.pipeline.push("program_filter".to_string());
    }
    if audio_decode.is_some() {
        summary.pipeline.push("audio_decode".to_string());
    }
    if transcode.is_some() {
        summary.pipeline.push("audio_transcode_pcm".to_string());
    }
    if audio_encode.is_some() || (!stat_desc.audio_passthrough && !stat_desc.audio_only) {
        summary.pipeline.push("audio_encode".to_string());
    }
    if video_decode.is_some() {
        summary.pipeline.push("video_decode".to_string());
    }
    if video_encode.is_some() || (!stat_desc.video_passthrough && !stat_desc.audio_only) {
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
    //
    // Three sources of truth, in priority order:
    //   1. Live `VideoEncodeStatsSnapshot` — populated once the encoder
    //      registers its handle (codec, backend, dimensions filled from the
    //      static config; bitrate/fps may inherit from source until the
    //      encoder reports its first frame).
    //   2. `EgressMediaSummaryStatic` target fields — operator intent from
    //      the `video_encode` block. Used when (1) hasn't reported yet or
    //      the encoder failed to open at all (e.g. NVENC on a non-NVIDIA
    //      host). This guarantees the operator always sees what the output
    //      is *configured to do*, not just what it's currently doing.
    //   3. `MediaAnalysis.video_streams.first()` — used for passthrough
    //      outputs (and as a resolution / fps fallback when the target
    //      target spec doesn't pin a specific output dimension).
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
        } else if stat_desc.video_target_codec.is_some() {
            summary.video_codec = stat_desc.video_target_codec.clone();
            summary.video_resolution = match (stat_desc.video_target_width, stat_desc.video_target_height) {
                (Some(w), Some(h)) if w > 0 && h > 0 => Some(format!("{}x{}", w, h)),
                _ => src_video.and_then(|v| v.resolution.clone()),
            };
            summary.video_fps = stat_desc
                .video_target_fps
                .or_else(|| src_video.and_then(|v| v.frame_rate.map(|f| f as f32)));
            summary.video_bitrate_kbps = stat_desc.video_target_bitrate_kbps;
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
    } else if stat_desc.audio_target_codec.is_some() {
        summary.audio_codec = stat_desc.audio_target_codec.clone();
        summary.audio_sample_rate_hz = stat_desc
            .audio_target_sample_rate_hz
            .or_else(|| src_audio.and_then(|a| a.sample_rate_hz));
        summary.audio_channels = stat_desc
            .audio_target_channels
            .or_else(|| src_audio.and_then(|a| a.channels));
        summary.audio_bitrate_kbps = stat_desc.audio_target_bitrate_kbps;
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
///
/// `connect_failed` is the per-input consecutive-connect-failure latch
/// ([`PerInputCounters::connect_failed`]) — it outranks `idle`/`waiting`
/// (the connection is provably down, not merely quiet) but never
/// `receiving` (live bytes mean the latch is about to reset).
fn derive_input_state(bitrate_bps: u64, packets_received: u64, connect_failed: bool) -> String {
    if bitrate_bps > 0 {
        "receiving"
    } else if connect_failed {
        "connect_failed"
    } else if packets_received > 0 {
        "idle"
    } else {
        "waiting"
    }
    .to_string()
}

/// Derive output connection state from counters.
/// Called during the 1/sec snapshot — zero hot-path impact.
///
/// Display outputs never increment `packets_sent` / `bytes_sent` (they
/// render locally rather than send over a network), so their two signals
/// mirror the network pair: `display_frames_advancing` (frames flipped
/// within [`DISPLAY_FRAMES_STALL_WINDOW`] — the bitrate analogue) drives
/// `active`, and the cumulative `display_frames_displayed` (the
/// `packets_sent` analogue) decays a panel frozen mid-run to `idle`
/// instead of latching `active` forever off the lifetime counter. Pass
/// `(0, false)` for non-display outputs — their derivation is unchanged.
fn derive_output_state(
    bitrate_bps: u64,
    packets_sent: u64,
    packets_dropped: u64,
    display_frames_displayed: u64,
    display_frames_advancing: bool,
) -> String {
    if bitrate_bps > 0 || display_frames_advancing {
        "active"
    } else if packets_sent > 0 || display_frames_displayed > 0 {
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
/// Derive a flow's overall [`FlowHealth`] **and** the structured list of
/// reasons that explain it. Each triggered condition becomes one
/// [`HealthReason`] carrying its own severity; the returned badge is the
/// maximum severity across them (`Healthy` when none fire), so the badge and
/// its explanation can never disagree.
///
/// This is a behaviour-preserving rework of the previous priority-ordered
/// early-return chain: every condition the old code checked still maps to a
/// reason of the same severity, and because the badge is the max, the result
/// is identical in every realistic case. The only divergence is two
/// physically-contradictory "no data flowing" combinations (zero bitrate
/// *and* a Priority-1 or all-slots-stalled fault, which can't co-occur — a
/// dead feed has no packets to throw CC/PMT errors): there the badge now
/// resolves to `Critical` instead of `Error`, i.e. strictly more severe and
/// fail-safe, never masking.
fn derive_flow_health(
    bitrate_bps: u64,
    packets_lost: u64,
    tr101290: &Option<Tr101290Stats>,
    bandwidth_exceeded: bool,
    bandwidth_blocked: bool,
    bandwidth_limit_mbps: Option<f64>,
    assembly_health: Option<&crate::stats::models::AssemblyHealth>,
) -> (FlowHealth, Vec<crate::stats::models::HealthReason>) {
    use crate::stats::models::HealthReason;
    let mut reasons: Vec<HealthReason> = Vec::new();

    // ── TR-101290 Priority 1 — the most severe transport faults ──
    if let Some(tr) = tr101290 {
        if tr.sync_loss_count > 0 {
            reasons.push(HealthReason {
                code: "tr101290_sync_loss".to_string(),
                severity: FlowHealth::Critical,
                detail: "TS sync loss — input signal lost or unparseable (TR-101290 Priority 1)"
                    .to_string(),
            });
        } else if !tr.priority1_ok {
            // Name the implicated P1 category from the WINDOWED counters
            // (plus the missing-PID gauge) — these are exactly the terms
            // `priority1_ok` gates on, so the detail describes what is
            // failing *now*. The cumulative lifetime totals would instead
            // list every category that ever fired (e.g. a long-resolved
            // start-up CC glitch surfacing during an unrelated PID outage).
            let mut parts: Vec<&str> = Vec::new();
            if tr.window_pat_errors > 0 {
                parts.push("PAT timeout");
            }
            if tr.window_pmt_errors > 0 {
                parts.push("PMT timeout");
            }
            if tr.window_pid_errors > 0 || tr.missing_continuous_pids > 0 {
                parts.push("missing ES PID");
            }
            if tr.window_cc_errors > 0 {
                parts.push("continuity-counter errors");
            }
            let detail = if parts.is_empty() {
                "Transport errors (TR-101290 Priority 1)".to_string()
            } else {
                format!("{} (TR-101290 Priority 1)", parts.join(", "))
            };
            reasons.push(HealthReason {
                code: "tr101290_p1".to_string(),
                severity: FlowHealth::Error,
                detail,
            });
        }
    }

    // ── No data flowing — distinct from a bandwidth block (handled below) ──
    if bitrate_bps == 0 && !bandwidth_blocked {
        reasons.push(HealthReason {
            code: "no_data".to_string(),
            severity: FlowHealth::Critical,
            detail: "No data flowing — input bitrate is zero".to_string(),
        });
    }

    // ── Bandwidth-limit enforcement: a hard block dominates a soft exceed ──
    if bandwidth_blocked {
        reasons.push(HealthReason {
            code: "bandwidth_blocked".to_string(),
            severity: FlowHealth::Error,
            detail: "Flow blocked — packets dropped by bandwidth-limit enforcement".to_string(),
        });
    } else if bandwidth_exceeded {
        let detail = match bandwidth_limit_mbps {
            Some(limit) => format!(
                "Ingest {:.1} Mbps exceeds the {:.0} Mbps limit",
                bitrate_bps as f64 / 1_000_000.0,
                limit
            ),
            None => "Ingest bitrate exceeds the configured bandwidth limit".to_string(),
        };
        reasons.push(HealthReason {
            code: "bandwidth_exceeded".to_string(),
            severity: FlowHealth::Warning,
            detail,
        });
    }

    // ── Packet loss: high loss is significant, any loss is a warning ──
    if packets_lost > 100 {
        reasons.push(HealthReason {
            code: "packet_loss_high".to_string(),
            severity: FlowHealth::Error,
            detail: format!("High packet loss — {} packets lost", packets_lost),
        });
    } else if packets_lost > 0 {
        reasons.push(HealthReason {
            code: "packet_loss".to_string(),
            severity: FlowHealth::Warning,
            detail: format!("Packet loss — {} packets lost", packets_lost),
        });
    }

    // ── Assembly slot liveness (assembled / PID-bus flows only) ──
    //
    // The flow-level input bitrate is the SUM over every referenced input,
    // so live-but-unwired inputs keep it > 0 while the assembly's actual
    // sources are dead — the bitrate term can't see that. Judge the plan's
    // own slot liveness instead: every slot stalled = no media leaving the
    // assembler (Error); a partial stall degrades (Warning). Stall
    // detection is stream-type-aware, so sparse essences never trip this.
    if let Some(ah) = assembly_health {
        if ah.total_slots > 0 && ah.stalled_slot_count >= ah.total_slots {
            reasons.push(HealthReason {
                code: "assembly_all_stalled".to_string(),
                severity: FlowHealth::Error,
                detail: format!(
                    "All {} assembly slot(s) stalled — no media leaving the assembler",
                    ah.total_slots
                ),
            });
        } else if ah.stalled_slot_count > 0 {
            let slots: Vec<String> = ah
                .stalled_slots
                .iter()
                .take(4)
                .map(|s| format!("program {} (PID 0x{:04X})", s.program_number, s.out_pid))
                .collect();
            let detail = if slots.is_empty() {
                format!(
                    "{} of {} assembly slot(s) stalled",
                    ah.stalled_slot_count, ah.total_slots
                )
            } else {
                format!(
                    "{} of {} assembly slot(s) stalled: {}",
                    ah.stalled_slot_count,
                    ah.total_slots,
                    slots.join(", ")
                )
            };
            reasons.push(HealthReason {
                code: "assembly_slot_stalled".to_string(),
                severity: FlowHealth::Warning,
                detail,
            });
        }
    }

    // ── TR-101290 Priority 2 — PCR/PTS accuracy, CRC, TEI ──
    if let Some(tr) = tr101290 {
        if !tr.priority2_ok {
            // Windowed counters again — match the `priority2_ok` gate so the
            // named categories reflect the current window, not lifetime totals.
            let mut parts: Vec<&str> = Vec::new();
            if tr.window_pcr_accuracy_errors > 0 {
                parts.push("PCR accuracy");
            }
            if tr.window_pcr_discontinuity_errors > 0 {
                parts.push("PCR discontinuity");
            }
            if tr.window_pts_errors > 0 {
                parts.push("PTS accuracy");
            }
            if tr.window_crc_errors > 0 {
                parts.push("CRC");
            }
            if tr.window_tei_errors > 0 {
                parts.push("transport-error-indicator");
            }
            let detail = if parts.is_empty() {
                "PCR / PTS accuracy errors (TR-101290 Priority 2)".to_string()
            } else {
                format!("{} errors (TR-101290 Priority 2)", parts.join(", "))
            };
            reasons.push(HealthReason {
                code: "tr101290_p2".to_string(),
                severity: FlowHealth::Warning,
                detail,
            });
        }
    }

    // Most-severe-first so the UI leads with the dominant cause; the badge
    // is the worst contributing severity (Healthy when there are none).
    reasons.sort_by(|a, b| b.severity.cmp(&a.severity));
    let health = reasons
        .iter()
        .map(|r| r.severity.clone())
        .max()
        .unwrap_or(FlowHealth::Healthy);
    (health, reasons)
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

#[cfg(test)]
mod input_state_tests {
    use super::*;

    fn ah(total: u32, stalled: u32) -> crate::stats::models::AssemblyHealth {
        crate::stats::models::AssemblyHealth {
            total_slots: total,
            stalled_slot_count: stalled,
            stalled_slots: Vec::new(),
        }
    }

    #[test]
    fn flow_health_all_slots_stalled_is_error_despite_live_input_bytes() {
        // The incident shape: 5 live-but-unwired inputs keep flow input
        // bitrate > 0 while the assembly's only sources are dead.
        let (h, reasons) = derive_flow_health(10_000_000, 0, &None, false, false, None, Some(&ah(2, 2)));
        assert_eq!(h, FlowHealth::Error);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].code, "assembly_all_stalled");
        assert_eq!(reasons[0].severity, FlowHealth::Error);
    }

    #[test]
    fn flow_health_partial_stall_is_warning() {
        let (h, reasons) = derive_flow_health(10_000_000, 0, &None, false, false, None, Some(&ah(3, 1)));
        assert_eq!(h, FlowHealth::Warning);
        assert_eq!(reasons[0].code, "assembly_slot_stalled");
    }

    #[test]
    fn flow_health_assembly_clean_is_healthy() {
        let (h, reasons) = derive_flow_health(10_000_000, 0, &None, false, false, None, Some(&ah(2, 0)));
        assert_eq!(h, FlowHealth::Healthy);
        assert!(reasons.is_empty());
    }

    #[test]
    fn flow_health_partial_stall_does_not_mask_no_data_critical() {
        // bitrate 0 must stay Critical even with a partial assembly stall.
        let (h, _) = derive_flow_health(0, 0, &None, false, false, None, Some(&ah(3, 1)));
        assert_eq!(h, FlowHealth::Critical);
    }

    #[test]
    fn flow_health_passthrough_unchanged_without_assembly() {
        assert_eq!(
            derive_flow_health(10_000_000, 0, &None, false, false, None, None).0,
            FlowHealth::Healthy
        );
        assert_eq!(
            derive_flow_health(0, 0, &None, false, false, None, None).0,
            FlowHealth::Critical
        );
    }

    #[test]
    fn flow_health_badge_is_max_severity_with_concurrent_reasons() {
        // Bandwidth exceeded (Warning) + high packet loss (Error) ride
        // together: the badge is the worst, but both reasons are surfaced,
        // most-severe first.
        let (h, reasons) =
            derive_flow_health(24_100_000, 250, &None, true, false, Some(20.0), None);
        assert_eq!(h, FlowHealth::Error);
        assert_eq!(reasons.len(), 2);
        assert_eq!(reasons[0].severity, FlowHealth::Error);
        assert_eq!(reasons[0].code, "packet_loss_high");
        assert_eq!(reasons[1].code, "bandwidth_exceeded");
        // The live numbers ride along in the detail string.
        assert!(reasons[1].detail.contains("24.1 Mbps"));
        assert!(reasons[1].detail.contains("20 Mbps"));
    }

    #[test]
    fn flow_health_p1_reason_names_live_cause_not_lifetime_totals() {
        use crate::stats::models::Tr101290Stats;
        // Start-up CC glitch left cc_errors = 5 forever, but this window is
        // clean; the flow is P1-red solely because an advertised PID is
        // currently missing (a sustained gauge). The reason must name the
        // live cause, not the long-resolved CC errors — the windowed
        // counters are exactly what priority1_ok gates on.
        let tr = Some(Tr101290Stats {
            priority1_ok: false,
            priority2_ok: true,
            cc_errors: 5,        // cumulative lifetime — must NOT be named
            window_cc_errors: 0, // clean this window
            missing_continuous_pids: 1,
            ..Default::default()
        });
        let (h, reasons) = derive_flow_health(10_000_000, 0, &tr, false, false, None, None);
        assert_eq!(h, FlowHealth::Error);
        let p1 = reasons
            .iter()
            .find(|r| r.code == "tr101290_p1")
            .expect("a P1 reason");
        assert!(p1.detail.contains("missing ES PID"), "got: {}", p1.detail);
        assert!(
            !p1.detail.contains("continuity"),
            "named a long-resolved lifetime CC error: {}",
            p1.detail
        );
    }

    #[test]
    fn flow_health_blocked_suppresses_no_data_and_exceeded() {
        // A hard bandwidth block is the single dominant reason even when
        // bitrate has been throttled to zero and the exceed flag is set.
        let (h, reasons) = derive_flow_health(0, 0, &None, true, true, Some(20.0), None);
        assert_eq!(h, FlowHealth::Error);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].code, "bandwidth_blocked");
    }

    #[test]
    fn derive_input_state_precedence() {
        // Live bytes always win — even with the failure latch still set
        // (the retry loop resets it on the connect that produced them).
        assert_eq!(derive_input_state(1_000, 10, true), "receiving");
        assert_eq!(derive_input_state(1_000, 10, false), "receiving");
        // No traffic + threshold crossed → connect_failed, regardless of
        // whether an earlier session ever produced packets.
        assert_eq!(derive_input_state(0, 0, true), "connect_failed");
        assert_eq!(derive_input_state(0, 10, true), "connect_failed");
        // Pre-existing semantics unchanged below the threshold.
        assert_eq!(derive_input_state(0, 10, false), "idle");
        assert_eq!(derive_input_state(0, 0, false), "waiting");
    }

    #[test]
    fn derive_output_state_wire_outputs_unchanged() {
        // Network outputs pass (0, false) for the display pair — the
        // pre-existing derivation must be bit-identical.
        assert_eq!(derive_output_state(1_000, 10, 0, 0, false), "active");
        assert_eq!(derive_output_state(0, 10, 0, 0, false), "idle");
        assert_eq!(derive_output_state(0, 0, 5, 0, false), "dropping");
        assert_eq!(derive_output_state(0, 0, 0, 0, false), "waiting");
    }

    #[test]
    fn derive_output_state_display_freezes_to_idle() {
        // Frames still advancing → active, exactly as before.
        assert_eq!(derive_output_state(0, 0, 0, 100, true), "active");
        // Frozen mid-run (lifetime counter > 0, no advance within the
        // stall window) → idle, NOT the latched-active the cumulative
        // counter used to produce.
        assert_eq!(derive_output_state(0, 0, 0, 100, false), "idle");
        // Never displayed a frame → waiting (registration alone isn't
        // activity).
        assert_eq!(derive_output_state(0, 0, 0, 0, false), "waiting");
    }
}

#[cfg(test)]
mod bond_throughput_tests {
    use super::*;
    use bonding_protocol::stats::{BondConnStats, PathStats};
    use std::sync::atomic::Ordering;

    /// `bond_handle_to_leg_stats` must derive a per-path rate from the
    /// byte counter in the bond's active direction, sum it into the
    /// aggregate, and ignore the opposite-direction counter. (The
    /// bonding crate declares `throughput_bps` but never writes it, so
    /// this rate has to come from the edge.)
    #[test]
    fn per_path_rate_is_derived_summed_and_direction_correct() {
        let p0 = PathStats::new();
        let p1 = PathStats::new();
        // Sender direction reads bytes_sent. The large bytes_received
        // value is a decoy a sender bond must NOT count.
        p0.bytes_sent.fetch_add(250_000, Ordering::Relaxed); // ~2 Mbps / s
        p0.bytes_received.fetch_add(50_000_000, Ordering::Relaxed); // decoy
        p1.bytes_sent.fetch_add(125_000, Ordering::Relaxed); // ~1 Mbps / s

        let handle = BondStatsHandle {
            flow_id: 7,
            role: BondStatsRole::Sender,
            scheduler: "weighted_rtt".to_string(),
            conn_stats: Arc::new(BondConnStats::default()),
            paths: vec![
                BondPathStatsHandle::new(0, "5G".to_string(), "udp".to_string(), p0.clone()),
                BondPathStatsHandle::new(1, "starlink".to_string(), "udp".to_string(), p1.clone()),
            ],
            oversize_payloads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        // The estimator caches for 1 s, so sample once, wait past the
        // window, then sample again to get a real rate.
        let _ = bond_handle_to_leg_stats(&handle);
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let legs = bond_handle_to_leg_stats(&handle);

        assert_eq!(legs.paths.len(), 2);
        assert!(legs.paths[0].throughput_bps > 0, "p0 rate should be > 0");
        assert!(legs.paths[1].throughput_bps > 0, "p1 rate should be > 0");
        // Direction: rate tracks the 250 kB of bytes_sent, never the
        // 50 MB bytes_received decoy (which would be ~400 Mbps).
        assert!(
            legs.paths[0].throughput_bps < 20_000_000,
            "sender rate must track bytes_sent, not bytes_received (got {})",
            legs.paths[0].throughput_bps
        );
        // p0 sent 2× p1, so it must carry the higher rate.
        assert!(legs.paths[0].throughput_bps > legs.paths[1].throughput_bps);
        // Aggregate is exactly the sum of the per-path rates.
        assert_eq!(
            legs.throughput_bps,
            legs.paths[0].throughput_bps + legs.paths[1].throughput_bps
        );
    }

    /// A receiver bond reads the bytes_received counter instead.
    #[test]
    fn receiver_direction_reads_bytes_received() {
        let p0 = PathStats::new();
        p0.bytes_received.fetch_add(250_000, Ordering::Relaxed);
        p0.bytes_sent.fetch_add(50_000_000, Ordering::Relaxed); // decoy

        let handle = BondStatsHandle {
            flow_id: 9,
            role: BondStatsRole::Receiver,
            scheduler: String::new(),
            conn_stats: Arc::new(BondConnStats::default()),
            paths: vec![BondPathStatsHandle::new(
                0,
                "wired".to_string(),
                "udp".to_string(),
                p0.clone(),
            )],
            oversize_payloads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let _ = bond_handle_to_leg_stats(&handle);
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let legs = bond_handle_to_leg_stats(&handle);

        assert!(legs.paths[0].throughput_bps > 0);
        assert!(
            legs.paths[0].throughput_bps < 20_000_000,
            "receiver rate must track bytes_received, not bytes_sent (got {})",
            legs.paths[0].throughput_bps
        );
        assert_eq!(legs.throughput_bps, legs.paths[0].throughput_bps);
    }

    /// New wire fields: a receiver bond reports the hold servo's
    /// `current_hold_ms` as `hold_ms` plus `session_resets`; a sender
    /// reports its MTU-oversize counter and per-path `rebuilds`, and
    /// omits `hold_ms` entirely (the servo never writes it on the
    /// sender side).
    #[test]
    fn hold_resets_rebuilds_and_oversize_fields_serialize() {
        let conn = Arc::new(BondConnStats::default());
        conn.current_hold_ms.store(840, Ordering::Relaxed);
        conn.session_resets.store(2, Ordering::Relaxed);
        let p0 = PathStats::new();
        p0.rebuilds.fetch_add(3, Ordering::Relaxed);

        let mut handle = BondStatsHandle {
            flow_id: 11,
            role: BondStatsRole::Receiver,
            scheduler: String::new(),
            conn_stats: conn,
            paths: vec![BondPathStatsHandle::new(
                0,
                "lte-0".to_string(),
                "udp".to_string(),
                p0,
            )],
            oversize_payloads: Arc::new(std::sync::atomic::AtomicU64::new(5)),
        };

        let legs = bond_handle_to_leg_stats(&handle);
        assert_eq!(legs.hold_ms, Some(840));
        assert_eq!(legs.session_resets, 2);
        assert_eq!(legs.oversize_payloads, 5);
        assert_eq!(legs.paths[0].rebuilds, 3);
        let json = serde_json::to_value(&legs).unwrap();
        assert_eq!(json["hold_ms"], 840);
        assert_eq!(json["session_resets"], 2);
        assert_eq!(json["oversize_payloads"], 5);
        assert_eq!(json["paths"][0]["rebuilds"], 3);

        // Sender role: hold_ms must be absent on the wire, not 0.
        handle.role = BondStatsRole::Sender;
        let legs = bond_handle_to_leg_stats(&handle);
        assert_eq!(legs.hold_ms, None);
        let json = serde_json::to_value(&legs).unwrap();
        assert!(json.get("hold_ms").is_none());
    }
}
