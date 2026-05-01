// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hardware encoder / decoder probe + CPU class + software capacity estimate.
//!
//! Runs once at startup, cached for process lifetime. Powers the
//! `resource_budget` block on `HealthPayload` so the manager UI can show a
//! per-node "this box has NVENC h264 + ~4× 720p30 x264 streams" budget.
//!
//! ## What this is
//!
//! - **Presence probe** — calls FFmpeg's `avcodec_find_encoder_by_name` /
//!   `..._decoder_by_name` for every common HW backend (NVENC, QSV,
//!   VideoToolbox, AMF). Non-NULL pointer means the codec is compiled in;
//!   it does NOT prove a session opens (driver / hardware still resolved
//!   at `avcodec_open2` time). Treat as "advertised, not guaranteed."
//! - **CPU class** — brand string + physical / logical core count from
//!   `sysinfo` (already a dep), AVX class from `std::is_x86_feature_detected!`.
//! - **SW capacity** — deterministic heuristic. `physical_cores / 2 × avx_mult`
//!   approximates 720p30 x264 broadcast streams. ±50 % accuracy. Numbers
//!   are a planning unit, not a benchmark — operators read live CPU% for
//!   ground truth.
//!
//! ## What this is not
//!
//! No try-open verification, no first-start benchmark, no per-resolution
//! lookup table. All of those are deferred until they're needed.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering};

/// Per-codec presence map for one set of hardware backends (encoders or
/// decoders). All fields default to `false`; the probe sets each one
/// independently, so a host with `h264_nvenc` available but
/// `hevc_nvenc` missing is reported faithfully.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HwCodecCapability {
    pub h264_nvenc: bool,
    pub hevc_nvenc: bool,
    pub h264_qsv: bool,
    pub hevc_qsv: bool,
    pub h264_videotoolbox: bool,
    pub hevc_videotoolbox: bool,
    pub h264_amf: bool,
    pub hevc_amf: bool,
}

impl HwCodecCapability {
    /// Returns `true` if any field is `true`. Drives the manager UI's
    /// "any HW encoder" badge.
    pub fn any(&self) -> bool {
        self.h264_nvenc
            || self.hevc_nvenc
            || self.h264_qsv
            || self.hevc_qsv
            || self.h264_videotoolbox
            || self.hevc_videotoolbox
            || self.h264_amf
            || self.hevc_amf
    }
}

/// AVX feature class on x86. On non-x86 (aarch64, etc.) `Other` is used as
/// a placeholder — modern ARM with NEON is roughly Avx2-comparable for
/// x264, so the SW capacity heuristic treats it as the Avx2 multiplier.
#[allow(dead_code)] // x86 variants unused on aarch64 builds; needed for wire shape + cross-target builds
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AvxClass {
    Avx512,
    Avx2,
    Sse42,
    None,
    Other,
}

/// Static CPU information gathered once at startup.
#[derive(Debug, Clone, Serialize)]
pub struct CpuInfo {
    pub brand: String,
    pub physical_cores: u32,
    pub logical_cores: u32,
    pub avx_class: AvxClass,
}

/// Software encode capacity estimate. **Rough** — anchored to broadcast
/// crf28 720p30 H.264 / HEVC; accuracy ±50 %.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SwCapacityEstimate {
    pub x264_720p30_streams: u32,
    pub x265_720p30_streams: u32,
    pub aac_encode_streams: u32,
}

/// Static (probed-once) hardware + CPU capabilities for this edge.
#[derive(Debug, Clone, Serialize)]
pub struct StaticCapabilities {
    pub hw_encoders: HwCodecCapability,
    pub hw_decoders: HwCodecCapability,
    pub cpu: CpuInfo,
    pub sw_capacity: SwCapacityEstimate,
}

/// Live (periodically polled) NVIDIA NVENC / NVDEC utilization. Reports
/// the first NVIDIA device only — multi-GPU edges report the device-0
/// view, which is the typical broadcast deployment shape. Populated when
/// NVML init succeeds on Linux / Windows; otherwise stays at defaults
/// with `available = false` and the manager surfaces no live block.
pub struct LiveUtilizationState {
    /// Set to `true` after a successful first NVML poll.
    pub available: AtomicBool,
    /// NVENC engine utilization, 0–100. Most-recent NVML sample.
    pub nvenc_encoder_percent: AtomicU8,
    /// NVDEC engine utilization, 0–100.
    pub nvdec_decoder_percent: AtomicU8,
    /// Active NVENC session count (best effort — NVML's
    /// `encoder_sessions` enumeration may be unsupported on some
    /// driver versions; falls back to 0).
    pub nvenc_session_count: AtomicU32,
    /// Unix timestamp (seconds) of the most recent successful poll.
    /// `0` until the first poll lands.
    pub last_poll_unix: AtomicI64,
}

impl Default for LiveUtilizationState {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveUtilizationState {
    pub fn new() -> Self {
        Self {
            available: AtomicBool::new(false),
            nvenc_encoder_percent: AtomicU8::new(0),
            nvdec_decoder_percent: AtomicU8::new(0),
            nvenc_session_count: AtomicU32::new(0),
            last_poll_unix: AtomicI64::new(0),
        }
    }

    /// Snapshot the live state for the health payload. Returns `None`
    /// when NVML isn't initialised — the manager UI hides the live
    /// block entirely on `None`.
    pub fn snapshot(&self) -> Option<LiveUtilizationSnapshot> {
        if !self.available.load(Ordering::Relaxed) {
            return None;
        }
        Some(LiveUtilizationSnapshot {
            nvenc_encoder_percent: self.nvenc_encoder_percent.load(Ordering::Relaxed),
            nvdec_decoder_percent: self.nvdec_decoder_percent.load(Ordering::Relaxed),
            nvenc_session_count: self.nvenc_session_count.load(Ordering::Relaxed),
            last_poll_unix: self.last_poll_unix.load(Ordering::Relaxed),
        })
    }
}

/// Wire shape for the live block. Matches the manager's
/// `LiveUtilization` struct (`bilbycast-manager/crates/manager-core/src/models/ws_protocol.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct LiveUtilizationSnapshot {
    pub nvenc_encoder_percent: u8,
    pub nvdec_decoder_percent: u8,
    pub nvenc_session_count: u32,
    pub last_poll_unix: i64,
}

// ── Probe entry points ──────────────────────────────────────────────

/// Probe every HW encoder / decoder name once and gather CPU info. Cheap
/// — call at startup, store the result.
pub fn probe_static_capabilities() -> StaticCapabilities {
    let cpu = probe_cpu_info();
    let sw_capacity = estimate_sw_capacity(&cpu);
    StaticCapabilities {
        hw_encoders: probe_hw_encoders(),
        hw_decoders: probe_hw_decoders(),
        cpu,
        sw_capacity,
    }
}

fn probe_hw_encoders() -> HwCodecCapability {
    HwCodecCapability {
        h264_nvenc: probe_encoder_named("h264_nvenc"),
        hevc_nvenc: probe_encoder_named("hevc_nvenc"),
        h264_qsv: probe_encoder_named("h264_qsv"),
        hevc_qsv: probe_encoder_named("hevc_qsv"),
        h264_videotoolbox: probe_encoder_named("h264_videotoolbox"),
        hevc_videotoolbox: probe_encoder_named("hevc_videotoolbox"),
        h264_amf: probe_encoder_named("h264_amf"),
        hevc_amf: probe_encoder_named("hevc_amf"),
    }
}

fn probe_hw_decoders() -> HwCodecCapability {
    HwCodecCapability {
        h264_nvenc: probe_decoder_named("h264_cuvid"),
        hevc_nvenc: probe_decoder_named("hevc_cuvid"),
        h264_qsv: probe_decoder_named("h264_qsv"),
        hevc_qsv: probe_decoder_named("hevc_qsv"),
        h264_videotoolbox: probe_decoder_named("h264_videotoolbox"),
        hevc_videotoolbox: probe_decoder_named("hevc_videotoolbox"),
        // AMD has no first-party FFmpeg HW decoder name today; AMF is
        // encode-only at the FFmpeg layer (decode goes through VAAPI on
        // Linux, D3D11VA on Windows). Keep the slot for symmetry.
        h264_amf: false,
        hevc_amf: false,
    }
}

#[cfg(feature = "video-thumbnail")]
fn probe_encoder_named(name: &str) -> bool {
    video_engine::is_encoder_available(name)
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_encoder_named(_: &str) -> bool {
    // No FFmpeg binding compiled in — every probe answers "no".
    false
}

#[cfg(feature = "video-thumbnail")]
fn probe_decoder_named(name: &str) -> bool {
    video_engine::is_decoder_available(name)
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_decoder_named(_: &str) -> bool {
    false
}

fn probe_cpu_info() -> CpuInfo {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    let cpus = sys.cpus();
    let brand = cpus
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let logical_cores = cpus.len() as u32;
    let physical_cores = sysinfo::System::physical_core_count()
        .map(|c| c as u32)
        .unwrap_or((logical_cores / 2).max(1));
    CpuInfo {
        brand,
        physical_cores,
        logical_cores,
        avx_class: detect_avx_class(),
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn detect_avx_class() -> AvxClass {
    if std::is_x86_feature_detected!("avx512f") {
        AvxClass::Avx512
    } else if std::is_x86_feature_detected!("avx2") {
        AvxClass::Avx2
    } else if std::is_x86_feature_detected!("sse4.2") {
        AvxClass::Sse42
    } else {
        AvxClass::None
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn detect_avx_class() -> AvxClass {
    // Modern aarch64 with NEON / SVE is roughly Avx2-comparable for
    // x264 but doesn't carry the AVX flag. Report `Other` so callers
    // can label it accurately; the SW heuristic treats it the same as
    // Avx2 (multiplier 1.0).
    AvxClass::Other
}

/// Static heuristic mapping CPU class → 720p30 H.264 (x264) broadcast
/// streams. **Rough.** Anchored on:
///
/// - 1 stream per 2 physical cores baseline (x264 medium preset, crf28).
/// - AVX-512 → ×1.3 (heavy SIMD wins for x264 quantize / SAD loops).
/// - AVX2    → ×1.0 (the assumed baseline).
/// - SSE4.2  → ×0.6 (older Westmere / first-gen i-series).
/// - None    → ×0.4 (truly ancient — clamped, not zero, so old hosts
///   still report a planning estimate).
/// - Other   → ×1.0 (aarch64 NEON, treated like AVX2).
///
/// HEVC (x265) is roughly 2× the CPU of H.264, so `x265_streams = x264 / 2`.
/// AAC encode is essentially unbounded for typical workloads — report a
/// large round number per physical core so the UI can label "audio
/// encode capacity" without implying a tight ceiling.
fn estimate_sw_capacity(cpu: &CpuInfo) -> SwCapacityEstimate {
    let base = (cpu.physical_cores as f32) / 2.0;
    let mult: f32 = match cpu.avx_class {
        AvxClass::Avx512 => 1.3,
        AvxClass::Avx2 => 1.0,
        AvxClass::Sse42 => 0.6,
        AvxClass::None => 0.4,
        AvxClass::Other => 1.0,
    };
    let x264 = (base * mult).floor().max(0.0) as u32;
    let x265 = x264 / 2;
    SwCapacityEstimate {
        x264_720p30_streams: x264,
        x265_720p30_streams: x265,
        aac_encode_streams: 200u32.saturating_mul(cpu.physical_cores.max(1)),
    }
}

// ── NVML live polling ───────────────────────────────────────────────

/// NVML context held for the lifetime of the process. `Some(_)` on
/// hosts where `Nvml::init()` succeeds (NVIDIA driver present + at
/// least one supported GPU); `None` everywhere else, including
/// non-NVIDIA hosts and builds without the `hardware-monitor-nvml`
/// feature.
pub struct NvmlPoller {
    #[cfg(feature = "hardware-monitor-nvml")]
    nvml: nvml_wrapper::Nvml,
}

impl NvmlPoller {
    /// Try to initialise NVML. Returns `None` (silently — debug log
    /// only) when the feature is off, the dynamic library isn't
    /// installed, or there's no NVIDIA GPU. Callers should treat
    /// `None` as the steady state on any non-NVIDIA host.
    #[cfg(feature = "hardware-monitor-nvml")]
    pub fn try_init() -> Option<Self> {
        match nvml_wrapper::Nvml::init() {
            Ok(nvml) => {
                tracing::info!("NVML initialised — live NVENC / NVDEC utilization sampling enabled");
                Some(Self { nvml })
            }
            Err(e) => {
                tracing::debug!("NVML init failed (no NVIDIA driver / GPU?): {e}");
                None
            }
        }
    }

    #[cfg(not(feature = "hardware-monitor-nvml"))]
    pub fn try_init() -> Option<Self> {
        None
    }

    /// Sample the first NVIDIA device and store the result in
    /// `state`. Failures (device gone, NVML transient error) flip
    /// `state.available` to `false` so the manager UI hides the live
    /// block until the next successful poll.
    #[cfg(feature = "hardware-monitor-nvml")]
    pub fn poll(&self, state: &LiveUtilizationState) {
        let now = chrono::Utc::now().timestamp();
        let device = match self.nvml.device_by_index(0) {
            Ok(d) => d,
            Err(_) => {
                state.available.store(false, Ordering::Relaxed);
                return;
            }
        };

        if let Ok(util) = device.encoder_utilization() {
            state
                .nvenc_encoder_percent
                .store(util.utilization.min(100) as u8, Ordering::Relaxed);
        }
        if let Ok(util) = device.decoder_utilization() {
            state
                .nvdec_decoder_percent
                .store(util.utilization.min(100) as u8, Ordering::Relaxed);
        }
        // `running_processes` is widely supported; the dedicated
        // `encoder_sessions` enumeration isn't available on every
        // driver version, so we approximate.
        if let Ok(procs) = device.running_compute_processes() {
            state
                .nvenc_session_count
                .store(procs.len() as u32, Ordering::Relaxed);
        }
        state.last_poll_unix.store(now, Ordering::Relaxed);
        state.available.store(true, Ordering::Relaxed);
    }

    #[cfg(not(feature = "hardware-monitor-nvml"))]
    pub fn poll(&self, _state: &LiveUtilizationState) {
        // Feature off — nothing to poll.
    }
}

// ── Per-flow cost-unit attribution ──────────────────────────────────

/// Compute the units a flow consumes against the node's resource
/// budget. Values are deterministic (no measurement) and anchored to
/// the cost notes documented in the project root `CLAUDE.md`.
///
/// | Flow shape                              | Units |
/// |---|---|
/// | Passthrough flow (base)                 | 1     |
/// | Each video_encode output (HW)           | 100   |
/// | Each video_encode output (SW)           | 500   |
/// | Each audio_encode output                | 5     |
/// | content_analysis = lite                 | 2     |
/// | content_analysis = audio_full           | 20    |
/// | content_analysis = video_full           | 50    |
/// | recording (replay) enabled              | 5     |
///
/// All flows pay the base passthrough cost (1) regardless of shape.
/// Transcoding / analysis / recording add on top.
pub fn compute_flow_cost_units(plan: &FlowCostPlan) -> u32 {
    let mut units: u32 = 1; // base
    units = units.saturating_add(100u32.saturating_mul(plan.hw_video_encode_outputs));
    units = units.saturating_add(500u32.saturating_mul(plan.sw_video_encode_outputs));
    units = units.saturating_add(5u32.saturating_mul(plan.audio_encode_outputs));
    // Local-display outputs run a SW video decode + ALSA write per
    // active flow. The weight roughly mirrors a SW video encode at
    // 1080p30 (250) — we charge 275 units (250 video + 5 audio +
    // 20 KMS render) so a 4K60 display output approaches the cost of
    // a 4K60 transcode. Operators on hosts with `display-vaapi` /
    // `display-nvdec` will see this drop to 100 in v2.
    units = units.saturating_add(275u32.saturating_mul(plan.display_outputs));
    if plan.content_analysis_lite {
        units = units.saturating_add(2);
    }
    if plan.content_analysis_audio_full {
        units = units.saturating_add(20);
    }
    if plan.content_analysis_video_full {
        units = units.saturating_add(50);
    }
    if plan.recording_enabled {
        units = units.saturating_add(5);
    }
    units
}

/// Lightweight description of a flow's cost-driving shape. Decoupled
/// from `FlowConfig` so tests can drive the cost model directly and so
/// the same shape can be derived from either an existing flow's runtime
/// config or a candidate flow in the manager preflight.
#[derive(Debug, Clone, Default)]
pub struct FlowCostPlan {
    pub hw_video_encode_outputs: u32,
    pub sw_video_encode_outputs: u32,
    pub audio_encode_outputs: u32,
    /// Number of `display` outputs on the flow. Linux-only; on
    /// non-Linux / non-feature builds this stays 0 because the
    /// schema-only Display variant is rejected at `start_output`.
    pub display_outputs: u32,
    pub content_analysis_lite: bool,
    pub content_analysis_audio_full: bool,
    pub content_analysis_video_full: bool,
    pub recording_enabled: bool,
}

/// Total budget capacity in units. Conservative, machine-independent:
/// `1000 + 200 × physical_cores`. A 4-core box gets 1800 units; a
/// 32-core EPYC gets 7400. Values intentionally imply a soft ceiling —
/// the manager UI surfaces percentage utilisation, not a hard cap.
pub fn compute_units_total(cpu: &CpuInfo) -> u32 {
    1000u32.saturating_add(200u32.saturating_mul(cpu.physical_cores.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_passthrough_is_one() {
        let plan = FlowCostPlan::default();
        assert_eq!(compute_flow_cost_units(&plan), 1);
    }

    #[test]
    fn cost_sw_video_encode_dominates() {
        let plan = FlowCostPlan {
            sw_video_encode_outputs: 1,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&plan), 1 + 500);
    }

    #[test]
    fn cost_hw_cheaper_than_sw() {
        let hw = FlowCostPlan {
            hw_video_encode_outputs: 1,
            ..Default::default()
        };
        let sw = FlowCostPlan {
            sw_video_encode_outputs: 1,
            ..Default::default()
        };
        assert!(compute_flow_cost_units(&hw) < compute_flow_cost_units(&sw));
    }

    #[test]
    fn cost_full_analysis_video_only() {
        let plan = FlowCostPlan {
            content_analysis_video_full: true,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&plan), 1 + 50);
    }

    #[test]
    fn cost_recording_adds_five() {
        let plan = FlowCostPlan {
            recording_enabled: true,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&plan), 1 + 5);
    }

    #[test]
    fn cost_combo_realistic() {
        // Realistic transcode flow: SW H.264 + AAC encode + lite analysis + recording.
        let plan = FlowCostPlan {
            sw_video_encode_outputs: 1,
            audio_encode_outputs: 1,
            content_analysis_lite: true,
            recording_enabled: true,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&plan), 1 + 500 + 5 + 2 + 5);
    }

    #[test]
    fn units_total_scales_with_cores() {
        let four = CpuInfo {
            brand: "test".into(),
            physical_cores: 4,
            logical_cores: 8,
            avx_class: AvxClass::Avx2,
        };
        let thirty_two = CpuInfo {
            brand: "test".into(),
            physical_cores: 32,
            logical_cores: 64,
            avx_class: AvxClass::Avx512,
        };
        assert_eq!(compute_units_total(&four), 1000 + 800);
        assert_eq!(compute_units_total(&thirty_two), 1000 + 6400);
    }

    #[test]
    fn sw_capacity_avx_classes_ordered() {
        let make = |cores, avx| CpuInfo {
            brand: "test".into(),
            physical_cores: cores,
            logical_cores: cores * 2,
            avx_class: avx,
        };
        let avx512 = estimate_sw_capacity(&make(8, AvxClass::Avx512)).x264_720p30_streams;
        let avx2 = estimate_sw_capacity(&make(8, AvxClass::Avx2)).x264_720p30_streams;
        let sse42 = estimate_sw_capacity(&make(8, AvxClass::Sse42)).x264_720p30_streams;
        let none = estimate_sw_capacity(&make(8, AvxClass::None)).x264_720p30_streams;
        assert!(avx512 >= avx2);
        assert!(avx2 >= sse42);
        assert!(sse42 >= none);
    }

    #[test]
    fn sw_capacity_x265_half_of_x264() {
        let cpu = CpuInfo {
            brand: "test".into(),
            physical_cores: 16,
            logical_cores: 32,
            avx_class: AvxClass::Avx2,
        };
        let cap = estimate_sw_capacity(&cpu);
        assert_eq!(cap.x265_720p30_streams, cap.x264_720p30_streams / 2);
    }

    #[test]
    fn hw_capability_any_detects_one_present() {
        let mut caps = HwCodecCapability::default();
        assert!(!caps.any());
        caps.h264_nvenc = true;
        assert!(caps.any());
    }

    #[test]
    fn live_state_starts_unavailable() {
        let state = LiveUtilizationState::new();
        assert!(state.snapshot().is_none());
    }

    #[test]
    fn live_state_snapshot_after_set() {
        let state = LiveUtilizationState::new();
        state.nvenc_encoder_percent.store(42, Ordering::Relaxed);
        state.available.store(true, Ordering::Relaxed);
        let snap = state.snapshot().expect("snapshot present after available");
        assert_eq!(snap.nvenc_encoder_percent, 42);
    }
}
