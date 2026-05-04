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
//! - **Runtime probe** — calls FFmpeg's `avcodec_open2` against a minimal
//!   320×240@25 fps YUV420P context for every common HW backend (NVENC,
//!   QSV, VideoToolbox, AMF — H.264 + HEVC each, encoders + matched
//!   decoders). A successful open + immediate close means the host has
//!   the driver, GPU, and permissions to actually run the codec; a
//!   failure means it doesn't. Replaces the old presence-only check
//!   (`avcodec_find_*_by_name` non-NULL) which over-reported availability
//!   on hosts that had the codec compiled in but no GPU. Per-backend
//!   nuance: NVENC retries once on `EAGAIN` (transient session-busy
//!   distinct from "no driver"); QSV emits a warn-level diagnostic on
//!   `EACCES` so operators see the `/dev/dri/renderD128` permissions
//!   path explicitly.
//! - **CPU class** — brand string + physical / logical core count from
//!   `sysinfo` (already a dep), AVX class from `std::is_x86_feature_detected!`.
//! - **SW capacity** — deterministic heuristic. `physical_cores / 2 × avx_mult`
//!   approximates 720p30 x264 broadcast streams. ±50 % accuracy. Numbers
//!   are a planning unit, not a benchmark — operators read live CPU% for
//!   ground truth.
//!
//! ## What this is not
//!
//! No first-start benchmark, no per-resolution lookup table. The runtime
//! probe is one-shot at startup and cached for process lifetime — it
//! doesn't re-probe across the day. Operators who hot-plug a GPU or
//! reload a driver mid-process must restart the edge for the new
//! capability set to land on the manager.

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

/// Per-family concurrent encoder-session capacity. One number per
/// hardware family — H.264 and HEVC share the same engine on every
/// supported backend, so `nvenc_max_sessions` covers both
/// `h264_nvenc` and `hevc_nvenc` (and likewise for QSV / AMF).
/// VideoToolbox isn't here because macOS manages sessions
/// system-wide, not per-process — there's no useful "max" to probe.
///
/// `None` means "not probed" — either the family wasn't compiled in, the
/// per-family probe failed at session 0 (so capacity is 0 — but we omit
/// the field instead of writing 0 to keep the wire shape distinguishable
/// from "limit is zero"), or the operator disabled session-count probing
/// via `BILBYCAST_PROBE_SESSION_LIMITS=0`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HwSessionLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvenc_max_sessions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qsv_max_sessions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amf_max_sessions: Option<u32>,
}

impl HwSessionLimits {
    /// `true` when no per-family limit was probed. Lets the manager UI
    /// degrade to "in use: N" without a `/M` denominator and saves a
    /// `Some(_)` round-trip on hosts where probing was disabled.
    pub fn is_empty(&self) -> bool {
        self.nvenc_max_sessions.is_none()
            && self.qsv_max_sessions.is_none()
            && self.amf_max_sessions.is_none()
    }
}

/// Per-family concurrent decoder-session capacity. NVDEC (`h264_cuvid`
/// / `hevc_cuvid`) and QSV decode share the same engine across H.264 +
/// HEVC. AMD has no first-party FFmpeg HW decoder name today (decode
/// goes through VAAPI on Linux, D3D11VA on Windows), so AMF is omitted
/// here for symmetry with [`HwCodecCapability::h264_amf`] / `hevc_amf`
/// which we hardcode to `false` on the decoder side.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HwDecoderSessionLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvdec_max_sessions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qsv_max_sessions: Option<u32>,
}

impl HwDecoderSessionLimits {
    pub fn is_empty(&self) -> bool {
        self.nvdec_max_sessions.is_none() && self.qsv_max_sessions.is_none()
    }
}

/// Per-codec runtime support for the (chroma × bit-depth) matrix the
/// edge can transcode under. The `Yuv420_8bit` baseline is already
/// covered by the boolean in [`HwCodecCapability`] — this struct adds
/// the three additional combinations broadcast workflows commonly
/// need: 4:2:2 8-bit (SDI baseband contribution), 4:2:0 10-bit (HDR
/// HEVC Main10), and 4:2:2 10-bit (10-bit SDI broadcast).
///
/// `false` here means the runtime probe attempted `avcodec_open2` for
/// that combo and got `EINVAL` / similar — so the host's iGPU / GPU
/// genuinely can't do that combination, even though the codec itself
/// is compiled in. NVENC variants always report `false` for any
/// 4:2:2 entry (NVENC has no 4:2:2 path on any GPU generation);
/// h264_qsv always reports `false` for 10-bit (H.264 has no Main10
/// profile on QSV / NVENC / AMF).
///
/// Software encoders (libx264 / libx265) report `true` when the
/// matching profile is compiled into the vendored FFmpeg
/// (High10 / High422 / Main422 — all enabled in the bilbycast build).
/// VideoToolbox is system-managed on macOS — we don't probe its chroma
/// matrix and the manager UI assumes the operator has researched
/// support for their Apple Silicon / Intel Mac generation.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HwEncoderChromaCapability {
    /// 4:2:2 8-bit support per HW codec. NVENC entries always false.
    #[serde(default)] pub h264_qsv_yuv422_8bit: bool,
    #[serde(default)] pub hevc_qsv_yuv422_8bit: bool,
    #[serde(default)] pub h264_amf_yuv422_8bit: bool,
    #[serde(default)] pub hevc_amf_yuv422_8bit: bool,

    /// 4:2:0 10-bit support per HW codec. h264_* entries always false
    /// (H.264 Main10 isn't real on any HW backend).
    #[serde(default)] pub hevc_qsv_yuv420_10bit: bool,
    #[serde(default)] pub hevc_nvenc_yuv420_10bit: bool,
    #[serde(default)] pub hevc_amf_yuv420_10bit: bool,

    /// 4:2:2 10-bit support per HW codec. NVENC + AMF entries always
    /// false (no 4:2:2 path).
    #[serde(default)] pub hevc_qsv_yuv422_10bit: bool,

    /// 4:2:2 8-bit + 10-bit support for software encoders. Always true
    /// on the bilbycast vendored build (High422 / Main422 enabled),
    /// but probed at startup so a custom build with the profile
    /// disabled reports honestly.
    #[serde(default)] pub libx264_yuv422_8bit: bool,
    #[serde(default)] pub libx265_yuv422_8bit: bool,
    #[serde(default)] pub libx264_yuv420_10bit: bool,
    #[serde(default)] pub libx265_yuv420_10bit: bool,
    #[serde(default)] pub libx264_yuv422_10bit: bool,
    #[serde(default)] pub libx265_yuv422_10bit: bool,
}

impl HwEncoderChromaCapability {
    /// `true` when no field reports a non-baseline chroma/bit-depth.
    /// Lets `build_resource_budget_payload` skip the wire field
    /// entirely on hosts where every probe returned false.
    pub fn is_empty(&self) -> bool {
        !(self.h264_qsv_yuv422_8bit
            || self.hevc_qsv_yuv422_8bit
            || self.h264_amf_yuv422_8bit
            || self.hevc_amf_yuv422_8bit
            || self.hevc_qsv_yuv420_10bit
            || self.hevc_nvenc_yuv420_10bit
            || self.hevc_amf_yuv420_10bit
            || self.hevc_qsv_yuv422_10bit
            || self.libx264_yuv422_8bit
            || self.libx265_yuv422_8bit
            || self.libx264_yuv420_10bit
            || self.libx265_yuv420_10bit
            || self.libx264_yuv422_10bit
            || self.libx265_yuv422_10bit)
    }
}

/// Static (probed-once) hardware + CPU capabilities for this edge.
#[derive(Debug, Clone, Serialize)]
pub struct StaticCapabilities {
    pub hw_encoders: HwCodecCapability,
    pub hw_decoders: HwCodecCapability,
    /// Per-family encoder session caps — `None` for unprobed families.
    /// Populated by [`probe_static_capabilities`] when at least one
    /// codec in the family probed ok and session-count probing is on.
    pub hw_encoder_session_limits: HwSessionLimits,
    /// Per-family decoder session caps — same semantics.
    pub hw_decoder_session_limits: HwDecoderSessionLimits,
    /// Per-codec chroma + bit-depth matrix for HW + SW encoders.
    /// Probed alongside the boolean availability check at startup;
    /// the manager UI can gate the chroma / bit-depth dropdowns in
    /// the flow modal off this matrix so an operator can't pick a
    /// combination the host's iGPU / GPU genuinely can't do.
    pub hw_encoder_chroma: HwEncoderChromaCapability,
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
///
/// Two phases:
/// 1. Per-codec runtime probe (`avcodec_open2`) — populates
///    `hw_encoders` / `hw_decoders` booleans.
/// 2. Per-family session-capacity probe — opens encoders / decoders in a
///    loop until one fails, capped at 8 (covers consumer + most pro
///    cards in <300 ms). Skipped per-family when no codec in that
///    family probed available. Disable globally with
///    `BILBYCAST_PROBE_SESSION_LIMITS=0` if startup latency matters.
pub fn probe_static_capabilities() -> StaticCapabilities {
    let cpu = probe_cpu_info();
    let sw_capacity = estimate_sw_capacity(&cpu);
    let hw_encoders = probe_hw_encoders();
    let hw_decoders = probe_hw_decoders();
    let hw_encoder_chroma = probe_encoder_chroma_capability(&hw_encoders);
    let session_probe_enabled = session_limit_probe_enabled();
    let hw_encoder_session_limits = if session_probe_enabled {
        probe_encoder_session_limits(&hw_encoders)
    } else {
        tracing::info!(
            "session-limit probe disabled via BILBYCAST_PROBE_SESSION_LIMITS=0; \
            manager will report HW sessions used without a max-sessions denominator"
        );
        HwSessionLimits::default()
    };
    let hw_decoder_session_limits = if session_probe_enabled {
        probe_decoder_session_limits(&hw_decoders)
    } else {
        HwDecoderSessionLimits::default()
    };
    StaticCapabilities {
        hw_encoders,
        hw_decoders,
        hw_encoder_session_limits,
        hw_decoder_session_limits,
        hw_encoder_chroma,
        cpu,
        sw_capacity,
    }
}

/// Probe each available encoder for the (chroma × bit-depth) matrix.
/// Skipped per-codec when the baseline 4:2:0 8-bit probe didn't even
/// return true — there's no point asking "can it do 4:2:2?" if it
/// can't open at all.
#[cfg(feature = "video-thumbnail")]
fn probe_encoder_chroma_capability(hw: &HwCodecCapability) -> HwEncoderChromaCapability {
    let mut out = HwEncoderChromaCapability::default();

    // Per-codec probes — gated on the codec being available at all.
    // Software encoders are also probed (libx264 / libx265) so a
    // custom build with the High422 / Main422 profile disabled
    // reports honestly. The vendored build has them on so the result
    // is normally true.
    out.libx264_yuv422_8bit  = probe_chroma("libx264",   video_engine::ProbeChroma::Yuv422_8bit);
    out.libx264_yuv420_10bit = probe_chroma("libx264",   video_engine::ProbeChroma::Yuv420_10bit);
    out.libx264_yuv422_10bit = probe_chroma("libx264",   video_engine::ProbeChroma::Yuv422_10bit);
    out.libx265_yuv422_8bit  = probe_chroma("libx265",   video_engine::ProbeChroma::Yuv422_8bit);
    out.libx265_yuv420_10bit = probe_chroma("libx265",   video_engine::ProbeChroma::Yuv420_10bit);
    out.libx265_yuv422_10bit = probe_chroma("libx265",   video_engine::ProbeChroma::Yuv422_10bit);

    if hw.h264_qsv {
        out.h264_qsv_yuv422_8bit  = probe_chroma("h264_qsv", video_engine::ProbeChroma::Yuv422_8bit);
    }
    if hw.hevc_qsv {
        out.hevc_qsv_yuv422_8bit  = probe_chroma("hevc_qsv", video_engine::ProbeChroma::Yuv422_8bit);
        out.hevc_qsv_yuv420_10bit = probe_chroma("hevc_qsv", video_engine::ProbeChroma::Yuv420_10bit);
        out.hevc_qsv_yuv422_10bit = probe_chroma("hevc_qsv", video_engine::ProbeChroma::Yuv422_10bit);
    }
    if hw.hevc_nvenc {
        out.hevc_nvenc_yuv420_10bit =
            probe_chroma("hevc_nvenc", video_engine::ProbeChroma::Yuv420_10bit);
    }
    if hw.h264_amf {
        out.h264_amf_yuv422_8bit  = probe_chroma("h264_amf", video_engine::ProbeChroma::Yuv422_8bit);
    }
    if hw.hevc_amf {
        out.hevc_amf_yuv422_8bit  = probe_chroma("hevc_amf", video_engine::ProbeChroma::Yuv422_8bit);
        out.hevc_amf_yuv420_10bit = probe_chroma("hevc_amf", video_engine::ProbeChroma::Yuv420_10bit);
    }

    out
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_encoder_chroma_capability(_: &HwCodecCapability) -> HwEncoderChromaCapability {
    HwEncoderChromaCapability::default()
}

/// Single-shot chroma/bit-depth probe with consistent logging.
#[cfg(feature = "video-thumbnail")]
fn probe_chroma(name: &str, chroma: video_engine::ProbeChroma) -> bool {
    match video_engine::probe_open_encoder_chroma(name, chroma) {
        Ok(()) => {
            tracing::debug!("chroma probe ok: {name} {}", chroma.label());
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            // For chroma probes, "not compiled" includes per-(codec,
            // chroma) combos we deliberately skip (e.g. NVENC + 4:2:2
            // returns NotCompiled because there's no path). Trace, not
            // warn — this is an expected outcome.
            tracing::trace!(
                "chroma probe skipped: {name} {} (not in pix_fmts matrix)",
                chroma.label()
            );
            false
        }
        Err(e) => {
            tracing::debug!("chroma probe failed: {name} {}: {e}", chroma.label());
            false
        }
    }
}

/// Honour `BILBYCAST_PROBE_SESSION_LIMITS=0` (or `false`) to skip the
/// loop-open session-capacity probe. Default on. Operators with strict
/// startup-latency budgets opt out.
fn session_limit_probe_enabled() -> bool {
    match std::env::var("BILBYCAST_PROBE_SESSION_LIMITS") {
        Ok(v) => {
            let lower = v.trim().to_ascii_lowercase();
            !matches!(lower.as_str(), "0" | "false" | "no" | "off")
        }
        Err(_) => true,
    }
}

/// Upper bound on session count probing. Cap chosen so probing never
/// takes more than ~300 ms even on slow open paths — enough to
/// distinguish 3-session consumer cards from "many" without
/// committing to an exact number on pro cards. Reported as the count
/// of successful opens before the first failure (so an 8-capable card
/// reports `8` and the UI labels it "≥ 8").
const SESSION_PROBE_UPPER_BOUND: u32 = 8;

/// Probe per-family encoder session capacity for every family with at
/// least one codec confirmed available in [`probe_hw_encoders`].
/// Probes one codec per family (the H.264 variant — H.264 + HEVC share
/// the engine on every supported backend) so we don't double the
/// startup cost. Returns `None` for families where no codec was
/// available at runtime.
#[cfg(feature = "video-thumbnail")]
fn probe_encoder_session_limits(hw: &HwCodecCapability) -> HwSessionLimits {
    let mut out = HwSessionLimits::default();
    if hw.h264_nvenc || hw.hevc_nvenc {
        let probe_codec = if hw.h264_nvenc { "h264_nvenc" } else { "hevc_nvenc" };
        let n = video_engine::count_max_encoder_sessions(probe_codec, SESSION_PROBE_UPPER_BOUND);
        if n > 0 {
            tracing::info!("nvenc encoder session capacity probed: {n}");
            out.nvenc_max_sessions = Some(n);
        }
    }
    if hw.h264_qsv || hw.hevc_qsv {
        let probe_codec = if hw.h264_qsv { "h264_qsv" } else { "hevc_qsv" };
        let n = video_engine::count_max_encoder_sessions(probe_codec, SESSION_PROBE_UPPER_BOUND);
        if n > 0 {
            tracing::info!("qsv encoder session capacity probed: {n}");
            out.qsv_max_sessions = Some(n);
        }
    }
    if hw.h264_amf || hw.hevc_amf {
        let probe_codec = if hw.h264_amf { "h264_amf" } else { "hevc_amf" };
        let n = video_engine::count_max_encoder_sessions(probe_codec, SESSION_PROBE_UPPER_BOUND);
        if n > 0 {
            tracing::info!("amf encoder session capacity probed: {n}");
            out.amf_max_sessions = Some(n);
        }
    }
    out
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_encoder_session_limits(_: &HwCodecCapability) -> HwSessionLimits {
    HwSessionLimits::default()
}

/// Decoder twin of [`probe_encoder_session_limits`]. Probes NVDEC
/// (`h264_cuvid`) and QSV decode (`h264_qsv`) — AMF has no first-party
/// FFmpeg HW decoder so it's omitted.
#[cfg(feature = "video-thumbnail")]
fn probe_decoder_session_limits(hw: &HwCodecCapability) -> HwDecoderSessionLimits {
    let mut out = HwDecoderSessionLimits::default();
    if hw.h264_nvenc || hw.hevc_nvenc {
        // hw_decoders.h264_nvenc actually corresponds to h264_cuvid by
        // wire convention — the field stays `h264_nvenc` for symmetry
        // with the encoder shape, but the underlying probe used the
        // cuvid name.
        let probe_codec = if hw.h264_nvenc { "h264_cuvid" } else { "hevc_cuvid" };
        let n = video_engine::count_max_decoder_sessions(probe_codec, SESSION_PROBE_UPPER_BOUND);
        if n > 0 {
            tracing::info!("nvdec decoder session capacity probed: {n}");
            out.nvdec_max_sessions = Some(n);
        }
    }
    if hw.h264_qsv || hw.hevc_qsv {
        let probe_codec = if hw.h264_qsv { "h264_qsv" } else { "hevc_qsv" };
        let n = video_engine::count_max_decoder_sessions(probe_codec, SESSION_PROBE_UPPER_BOUND);
        if n > 0 {
            tracing::info!("qsv decoder session capacity probed: {n}");
            out.qsv_max_sessions = Some(n);
        }
    }
    out
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_decoder_session_limits(_: &HwCodecCapability) -> HwDecoderSessionLimits {
    HwDecoderSessionLimits::default()
}

fn probe_hw_encoders() -> HwCodecCapability {
    HwCodecCapability {
        h264_nvenc: probe_nvenc_encoder("h264_nvenc"),
        hevc_nvenc: probe_nvenc_encoder("hevc_nvenc"),
        h264_qsv: probe_qsv_encoder("h264_qsv"),
        hevc_qsv: probe_qsv_encoder("hevc_qsv"),
        h264_videotoolbox: probe_videotoolbox_encoder("h264_videotoolbox"),
        hevc_videotoolbox: probe_videotoolbox_encoder("hevc_videotoolbox"),
        h264_amf: probe_runtime_encoder("h264_amf"),
        hevc_amf: probe_runtime_encoder("hevc_amf"),
    }
}

fn probe_hw_decoders() -> HwCodecCapability {
    HwCodecCapability {
        h264_nvenc: probe_runtime_decoder("h264_cuvid"),
        hevc_nvenc: probe_runtime_decoder("hevc_cuvid"),
        h264_qsv: probe_runtime_decoder("h264_qsv"),
        hevc_qsv: probe_runtime_decoder("hevc_qsv"),
        h264_videotoolbox: probe_videotoolbox_decoder("h264_videotoolbox"),
        hevc_videotoolbox: probe_videotoolbox_decoder("hevc_videotoolbox"),
        // AMD has no first-party FFmpeg HW decoder name today; AMF is
        // encode-only at the FFmpeg layer (decode goes through VAAPI on
        // Linux, D3D11VA on Windows). Keep the slot for symmetry.
        h264_amf: false,
        hevc_amf: false,
    }
}

/// Generic runtime encoder probe — try `avcodec_open2`, classify the
/// result. Used directly for AMF (no special retry / diagnostic
/// behaviour); NVENC and QSV wrap this with backend-specific
/// pre/post-conditions.
#[cfg(feature = "video-thumbnail")]
fn probe_runtime_encoder(name: &str) -> bool {
    match video_engine::probe_open_encoder(name) {
        Ok(()) => {
            tracing::debug!("hw encoder probe ok: {name}");
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            tracing::trace!("hw encoder not compiled in: {name}");
            false
        }
        Err(e) => {
            tracing::debug!("hw encoder probe failed: {name}: {e}");
            false
        }
    }
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_runtime_encoder(_: &str) -> bool {
    // No FFmpeg binding compiled in — every probe answers "no".
    false
}

/// Generic runtime decoder probe.
#[cfg(feature = "video-thumbnail")]
fn probe_runtime_decoder(name: &str) -> bool {
    match video_engine::probe_open_decoder(name) {
        Ok(()) => {
            tracing::debug!("hw decoder probe ok: {name}");
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            tracing::trace!("hw decoder not compiled in: {name}");
            false
        }
        Err(e) => {
            tracing::debug!("hw decoder probe failed: {name}: {e}");
            false
        }
    }
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_runtime_decoder(_: &str) -> bool {
    false
}

/// NVENC encoder probe with retry-once-on-EAGAIN. NVENC distinguishes
/// "no driver" (`ENOSYS`/`ENODEV`/`ENOENT`, returns in microseconds —
/// permanent) from "session slot busy" (`EAGAIN`, returns in ~50 ms —
/// transient, e.g. another process is using all 3 sessions on a
/// consumer card). Treating the latter as "unavailable" would lock out
/// the host every time we restart while another tool is briefly using
/// the GPU. So: on EAGAIN, sleep 100 ms and retry once.
#[cfg(feature = "video-thumbnail")]
fn probe_nvenc_encoder(name: &str) -> bool {
    match video_engine::probe_open_encoder(name) {
        Ok(()) => {
            tracing::debug!("nvenc probe ok: {name}");
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            tracing::trace!("nvenc not compiled in: {name}");
            false
        }
        Err(video_engine::ProbeError::Busy) => {
            tracing::debug!("nvenc probe busy on first attempt: {name}, retrying after 100 ms");
            std::thread::sleep(std::time::Duration::from_millis(100));
            match video_engine::probe_open_encoder(name) {
                Ok(()) => {
                    tracing::info!("nvenc probe ok on retry: {name}");
                    true
                }
                Err(e) => {
                    tracing::warn!("nvenc probe still failing after retry: {name}: {e}");
                    false
                }
            }
        }
        Err(e) => {
            tracing::debug!("nvenc probe failed: {name}: {e}");
            false
        }
    }
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_nvenc_encoder(_: &str) -> bool {
    false
}

/// QSV encoder probe with `EACCES` diagnostic. QSV opens
/// `/dev/dri/renderD128` (or `card0`) — when the running user lacks
/// access (typical for systemd services that aren't in the `video` and
/// `render` groups), the open fails with `EACCES`. That's a fixable
/// permissions issue, distinct from "no Intel iGPU" — surface it loudly
/// so operators don't waste time chasing GPU driver problems.
#[cfg(feature = "video-thumbnail")]
fn probe_qsv_encoder(name: &str) -> bool {
    match video_engine::probe_open_encoder(name) {
        Ok(()) => {
            tracing::debug!("qsv probe ok: {name}");
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            tracing::trace!("qsv not compiled in: {name}");
            false
        }
        Err(video_engine::ProbeError::PermissionDenied) => {
            tracing::warn!(
                "qsv probe denied for {name}: EACCES on Intel iGPU device. \
                Add the running user to the 'video' and 'render' groups \
                (or grant access to /dev/dri/renderD128) and restart."
            );
            false
        }
        Err(e) => {
            tracing::debug!("qsv probe failed: {name}: {e}");
            false
        }
    }
}

#[cfg(not(feature = "video-thumbnail"))]
fn probe_qsv_encoder(_: &str) -> bool {
    false
}

/// VideoToolbox encoder probe. macOS-only: the framework is
/// system-provided, so a non-NULL FFmpeg registry entry is sufficient
/// proof — a runtime open could grab and release a CoreMedia session
/// that competing apps are using, for no extra information. On non-Mac
/// hosts the codec isn't in the build at all and the probe returns
/// false via the registry pre-filter.
#[cfg(all(feature = "video-thumbnail", target_os = "macos"))]
fn probe_videotoolbox_encoder(name: &str) -> bool {
    let present = video_engine::is_encoder_available(name);
    if present {
        tracing::debug!("videotoolbox encoder available: {name}");
    }
    present
}

#[cfg(all(feature = "video-thumbnail", target_os = "macos"))]
fn probe_videotoolbox_decoder(name: &str) -> bool {
    let present = video_engine::is_decoder_available(name);
    if present {
        tracing::debug!("videotoolbox decoder available: {name}");
    }
    present
}

#[cfg(not(all(feature = "video-thumbnail", target_os = "macos")))]
fn probe_videotoolbox_encoder(_: &str) -> bool {
    false
}

#[cfg(not(all(feature = "video-thumbnail", target_os = "macos")))]
fn probe_videotoolbox_decoder(_: &str) -> bool {
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
    // Local-display outputs split into two cost tiers based on whether
    // the resolved decoder backend lands on CPU or HW. CPU-decoded
    // display outputs run a libavcodec SW decode + libswscale colour
    // convert + ALSA write per active flow — 275 units (mirrors a SW
    // video encode at 1080p30 plus the KMS render + audio path). HW-
    // decoded display outputs offload the decode to NVDEC / QSV, so
    // the host CPU only pays libswscale + the KMS blit + audio — 100
    // units, matching the v2 roadmap note in the architecture doc.
    units = units.saturating_add(275u32.saturating_mul(plan.display_outputs));
    units = units.saturating_add(100u32.saturating_mul(plan.display_hw_decoded_outputs));
    // Audio-bars overlay on a `display` output runs an independent
    // multi-PID audio decoder + per-frame BGRA rasterise. ~15 units per
    // enabled output (matches `audio_encode` at 5 plus a small bump for
    // the ~3 PIDs typical on a broadcast multi-language feed).
    units = units.saturating_add(15u32.saturating_mul(plan.display_audio_bars_outputs));
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
    /// Number of `display` outputs on the flow that decode video on
    /// the CPU (libavcodec). Counted separately from
    /// `display_hw_decoded_outputs` because the cost weight differs
    /// (CPU decode dominates a 4K display output's CPU budget).
    /// Linux-only; on non-Linux / non-feature builds this stays 0
    /// because the schema-only Display variant is rejected at
    /// `start_output`.
    pub display_outputs: u32,
    /// Number of `display` outputs whose resolved decoder backend is
    /// hardware (NVDEC / QSV) — the `hw_decode` preference resolved
    /// against the host's probed capabilities at flow-bring-up time.
    /// Each one charges roughly a third of the CPU-decoded display
    /// cost (libswscale colour-convert + KMS blit only) plus one
    /// session against the matching family limit.
    pub display_hw_decoded_outputs: u32,
    /// Number of `display` outputs that have `show_audio_bars: true`.
    /// Counts the extra audio-decoder pool the meter spawns, not the
    /// rasterisation cost (negligible).
    pub display_audio_bars_outputs: u32,
    pub content_analysis_lite: bool,
    pub content_analysis_audio_full: bool,
    pub content_analysis_video_full: bool,
    pub recording_enabled: bool,
    /// Per-family encoder session counts derived from the flow's
    /// output codecs. Sums across HW outputs by family — H.264 and
    /// HEVC share the engine on every supported backend, so a flow
    /// with one `h264_nvenc` output and one `hevc_nvenc` output
    /// contributes `nvenc_sessions = 2`. Used by
    /// `FlowManager::total_hw_sessions()` to compute the live
    /// `HwSessionUsage` block on every health tick.
    pub nvenc_sessions: u32,
    pub qsv_sessions: u32,
    pub videotoolbox_sessions: u32,
    pub amf_sessions: u32,
    /// Per-family decoder session counts charged by HW-decoded
    /// `display` outputs on this flow. NVDEC and QSV-decode are
    /// tracked separately even though QSV-decode shares an iGPU with
    /// QSV-encode — operators want to see decoder pressure
    /// independently from encoder pressure when planning capacity.
    pub nvdec_sessions: u32,
    pub qsv_decode_sessions: u32,
}

/// Per-family hardware encoder session counts in active use across
/// every flow on this edge. Health-payload sibling of
/// [`HwSessionLimits`] — manager UI renders `nvenc_in_use / nvenc_max`
/// chips when both are present, falling back to `nvenc_in_use` only
/// when the limit is `None` (probe disabled or no NVENC family).
#[derive(Debug, Clone, Default, Serialize)]
pub struct HwSessionUsage {
    pub nvenc_in_use: u32,
    pub qsv_in_use: u32,
    pub videotoolbox_in_use: u32,
    pub amf_in_use: u32,
    /// Active hardware-decoder sessions. Charged today only by
    /// HW-decoded `display` outputs (`hw_decode` resolved to NVDEC /
    /// QSV). When future input-side HW decode lands the same
    /// counters absorb that contribution.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub nvdec_in_use: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub qsv_decode_in_use: u32,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// HW encoder family — used both to classify codec names from config and
/// to attribute live session usage onto the corresponding limit slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwEncoderFamily {
    Nvenc,
    Qsv,
    VideoToolbox,
    Amf,
}

impl HwEncoderFamily {
    /// Map a video-encode codec string to a family. Unknown / SW codecs
    /// (libx264, libx265, plain `h264`) return `None`. Match is
    /// case-insensitive, matching the existing `is_hw_video_codec`
    /// classifier in `flow.rs`.
    pub fn classify(codec: &str) -> Option<Self> {
        let c = codec.to_lowercase();
        if c.contains("nvenc") {
            Some(HwEncoderFamily::Nvenc)
        } else if c.contains("qsv") {
            Some(HwEncoderFamily::Qsv)
        } else if c.contains("videotoolbox") {
            Some(HwEncoderFamily::VideoToolbox)
        } else if c.contains("amf") {
            Some(HwEncoderFamily::Amf)
        } else {
            None
        }
    }
}

/// HW decoder family — used to attribute display-output decoder session
/// usage onto the corresponding limit slot in
/// [`HwDecoderSessionLimits`]. VideoToolbox / AMF are deliberately
/// omitted: macOS manages decoder sessions system-wide so there's no
/// per-process limit to track, and AMD has no first-party FFmpeg HW
/// decoder name (decode rides VAAPI on Linux, D3D11VA on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwDecoderFamily {
    Nvdec,
    Qsv,
}

impl HwDecoderFamily {
    /// Map an operator's `hw_decode` preference to a family. Returns
    /// `None` for `Auto` / `Cpu` (neither charges a HW decoder
    /// session) — the auto-resolver downstream may still pick a HW
    /// family, but that decision is recorded separately on the
    /// resolved [`video_engine::DecoderBackend`].
    ///
    /// Public symmetric helper alongside [`HwEncoderFamily::classify`]
    /// — currently exercised by tests; runtime callers go through
    /// [`ResolvedDisplayDecoder::family`] which already keys off the
    /// resolution result. Kept on the API surface so future input-side
    /// HW decode (RTSP cameras, ST 2110 ingress) can charge sessions
    /// without re-deriving the mapping.
    #[allow(dead_code)]
    pub fn from_preference(
        pref: &crate::config::models::HwDecodePreference,
    ) -> Option<Self> {
        use crate::config::models::HwDecodePreference;
        match pref {
            HwDecodePreference::Auto | HwDecodePreference::Cpu => None,
            HwDecodePreference::Nvdec => Some(HwDecoderFamily::Nvdec),
            HwDecodePreference::Qsv => Some(HwDecoderFamily::Qsv),
        }
    }
}

/// Total budget capacity in units. Conservative, machine-independent:
/// `1000 + 200 × physical_cores`. A 4-core box gets 1800 units; a
/// 32-core EPYC gets 7400. Values intentionally imply a soft ceiling —
/// the manager UI surfaces percentage utilisation, not a hard cap.
pub fn compute_units_total(cpu: &CpuInfo) -> u32 {
    1000u32.saturating_add(200u32.saturating_mul(cpu.physical_cores.max(1)))
}

// ── Static caps singleton ──────────────────────────────────────────
//
// `probe_static_capabilities()` runs once at startup in `main.rs`. We
// also stash the result here so any module that needs to consult the
// host's HW decoder availability (e.g. `output_display::start_output`
// resolving an operator's `hw_decode` preference, or `derive_cost_plan`
// charging the right per-display cost) can read it without threading
// the snapshot through every function call.

static STATIC_CAPS: std::sync::OnceLock<std::sync::Arc<StaticCapabilities>> =
    std::sync::OnceLock::new();

/// Install the one-shot static-capabilities snapshot. Called from
/// `main.rs` after `probe_static_capabilities()` returns. Subsequent
/// calls are silently ignored — the probe runs once per process.
pub fn install_static_capabilities(caps: std::sync::Arc<StaticCapabilities>) {
    let _ = STATIC_CAPS.set(caps);
}

/// Read the installed snapshot. Returns `None` until
/// [`install_static_capabilities`] runs (early-startup paths and unit
/// tests that don't initialise the probe).
pub fn static_capabilities() -> Option<std::sync::Arc<StaticCapabilities>> {
    STATIC_CAPS.get().cloned()
}

// ── Display-output decoder resolution ──────────────────────────────

/// Reason the operator's `hw_decode` preference couldn't be honoured.
/// Surfaced on the `display_hw_decode_unavailable` event's `details`
/// block so the manager UI can render an actionable message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderResolutionError {
    /// Forced backend isn't compiled into this build.
    FeatureDisabled,
    /// Build has the feature but the runtime probe failed (no driver,
    /// no hardware, or `/dev/dri` permissions).
    DriverMissing,
    /// `static_capabilities()` returned `None` — probe didn't run.
    /// Treated like FeatureDisabled by the spawner.
    ProbeUnavailable,
}

/// Resolved family choice from `resolve_display_decoder` — the
/// HW-aware variant. Decoupled from `video_engine::DecoderBackend`
/// (which only exists when `video-thumbnail` is on) so cost-plan code
/// can call into the resolver from non-display builds too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedDisplayDecoder {
    Cpu,
    Nvdec,
    Qsv,
}

impl ResolvedDisplayDecoder {
    /// `true` when this resolution actually uses a hardware decoder
    /// (cost-plan path keys the per-output cost on this).
    pub fn is_hardware(&self) -> bool {
        !matches!(self, ResolvedDisplayDecoder::Cpu)
    }

    /// Map onto the family limit slot in
    /// [`HwDecoderSessionLimits`]. `None` for `Cpu`.
    pub fn family(&self) -> Option<HwDecoderFamily> {
        match self {
            ResolvedDisplayDecoder::Cpu => None,
            ResolvedDisplayDecoder::Nvdec => Some(HwDecoderFamily::Nvdec),
            ResolvedDisplayDecoder::Qsv => Some(HwDecoderFamily::Qsv),
        }
    }

    /// Translate to the video-engine backend handle. Only callable
    /// when `video-thumbnail` is on (display path always has it).
    #[cfg(feature = "video-thumbnail")]
    pub fn as_backend(&self) -> video_engine::DecoderBackend {
        match self {
            ResolvedDisplayDecoder::Cpu => video_engine::DecoderBackend::Cpu,
            ResolvedDisplayDecoder::Nvdec => video_engine::DecoderBackend::Nvdec,
            ResolvedDisplayDecoder::Qsv => video_engine::DecoderBackend::Qsv,
        }
    }
}

/// Resolve an operator's display-output decode preference into a
/// concrete backend. Auto picks the best HW family the host can do
/// (NVDEC ≻ QSV) and falls back to CPU. Forced choices error out when
/// the host can't satisfy them — the spawner emits
/// `display_hw_decode_unavailable` and refuses to start the output.
///
/// Pure function over `(pref, capabilities)` — call from cost-plan
/// derivation and from `start_output()` and they always agree.
pub fn resolve_display_decoder(
    pref: &crate::config::models::HwDecodePreference,
    caps: Option<&StaticCapabilities>,
) -> Result<ResolvedDisplayDecoder, DecoderResolutionError> {
    use crate::config::models::HwDecodePreference;

    let nvdec_compiled = cfg!(feature = "display-nvdec");
    let qsv_compiled = cfg!(feature = "display-qsv");
    let nvdec_ok = nvdec_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_nvenc || c.hw_decoders.hevc_nvenc);
    let qsv_ok = qsv_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_qsv || c.hw_decoders.hevc_qsv);

    match pref {
        HwDecodePreference::Cpu => Ok(ResolvedDisplayDecoder::Cpu),
        HwDecodePreference::Auto => {
            if nvdec_ok {
                Ok(ResolvedDisplayDecoder::Nvdec)
            } else if qsv_ok {
                Ok(ResolvedDisplayDecoder::Qsv)
            } else {
                Ok(ResolvedDisplayDecoder::Cpu)
            }
        }
        HwDecodePreference::Nvdec => {
            if !nvdec_compiled {
                Err(DecoderResolutionError::FeatureDisabled)
            } else if caps.is_none() {
                Err(DecoderResolutionError::ProbeUnavailable)
            } else if !nvdec_ok {
                Err(DecoderResolutionError::DriverMissing)
            } else {
                Ok(ResolvedDisplayDecoder::Nvdec)
            }
        }
        HwDecodePreference::Qsv => {
            if !qsv_compiled {
                Err(DecoderResolutionError::FeatureDisabled)
            } else if caps.is_none() {
                Err(DecoderResolutionError::ProbeUnavailable)
            } else if !qsv_ok {
                Err(DecoderResolutionError::DriverMissing)
            } else {
                Ok(ResolvedDisplayDecoder::Qsv)
            }
        }
    }
}

impl DecoderResolutionError {
    /// Short tag for the `details.error_code` companion field. The
    /// outer `error_code` stays `display_hw_decode_unavailable` so
    /// existing manager-side dispatch keeps working.
    pub fn as_reason(&self) -> &'static str {
        match self {
            DecoderResolutionError::FeatureDisabled => "feature_disabled",
            DecoderResolutionError::DriverMissing => "driver_missing",
            DecoderResolutionError::ProbeUnavailable => "probe_unavailable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_passthrough_is_one() {
        let plan = FlowCostPlan::default();
        assert_eq!(compute_flow_cost_units(&plan), 1);
    }

    /// HW-decoded display output charges 100 units; CPU-decoded
    /// display output charges 275. Two outputs of each kind gives a
    /// flow whose cost matches `1 + 275*2 + 100*2 = 751`. The drop
    /// from 275 → 100 is the v2-roadmap saving promised in
    /// `compute_flow_cost_units`.
    #[test]
    fn cost_hw_decoded_display_is_cheaper_than_cpu() {
        let cpu = FlowCostPlan {
            display_outputs: 1,
            ..Default::default()
        };
        let hw = FlowCostPlan {
            display_hw_decoded_outputs: 1,
            ..Default::default()
        };
        assert!(compute_flow_cost_units(&hw) < compute_flow_cost_units(&cpu));
        assert_eq!(compute_flow_cost_units(&cpu), 1 + 275);
        assert_eq!(compute_flow_cost_units(&hw), 1 + 100);
    }

    #[test]
    fn hw_decoder_family_from_preference_maps_correctly() {
        use crate::config::models::HwDecodePreference;
        assert_eq!(
            HwDecoderFamily::from_preference(&HwDecodePreference::Nvdec),
            Some(HwDecoderFamily::Nvdec),
        );
        assert_eq!(
            HwDecoderFamily::from_preference(&HwDecodePreference::Qsv),
            Some(HwDecoderFamily::Qsv),
        );
        assert_eq!(
            HwDecoderFamily::from_preference(&HwDecodePreference::Auto),
            None,
        );
        assert_eq!(
            HwDecoderFamily::from_preference(&HwDecodePreference::Cpu),
            None,
        );
    }

    /// Cpu always resolves to Cpu — no host capability lookup needed.
    /// Auto on a host with no probed HW falls through to Cpu (the
    /// safe default that lets every config round-trip on a
    /// software-only edge). Forced HW choices on a missing-feature /
    /// missing-driver host error out with the right reason tag —
    /// `start_output()` keys the `display_hw_decode_unavailable` event
    /// off this.
    #[test]
    fn resolve_display_decoder_handles_cpu_and_auto_without_caps() {
        use crate::config::models::HwDecodePreference;
        assert_eq!(
            resolve_display_decoder(&HwDecodePreference::Cpu, None),
            Ok(ResolvedDisplayDecoder::Cpu),
        );
        assert_eq!(
            resolve_display_decoder(&HwDecodePreference::Auto, None),
            Ok(ResolvedDisplayDecoder::Cpu),
        );
    }

    #[test]
    fn resolve_display_decoder_rejects_forced_when_feature_off() {
        use crate::config::models::HwDecodePreference;
        // The bilbycast-edge build under `cargo test` here does not
        // enable `display-nvdec` or `display-qsv` by default — the
        // feature gates short-circuit before we even look at caps.
        if !cfg!(feature = "display-nvdec") {
            assert_eq!(
                resolve_display_decoder(&HwDecodePreference::Nvdec, None),
                Err(DecoderResolutionError::FeatureDisabled),
            );
        }
        if !cfg!(feature = "display-qsv") {
            assert_eq!(
                resolve_display_decoder(&HwDecodePreference::Qsv, None),
                Err(DecoderResolutionError::FeatureDisabled),
            );
        }
    }

    #[test]
    fn resolution_error_tags_are_stable() {
        assert_eq!(
            DecoderResolutionError::FeatureDisabled.as_reason(),
            "feature_disabled"
        );
        assert_eq!(
            DecoderResolutionError::DriverMissing.as_reason(),
            "driver_missing"
        );
        assert_eq!(
            DecoderResolutionError::ProbeUnavailable.as_reason(),
            "probe_unavailable"
        );
    }

    #[test]
    fn resolved_display_decoder_is_hardware_classifier() {
        assert!(!ResolvedDisplayDecoder::Cpu.is_hardware());
        assert!(ResolvedDisplayDecoder::Nvdec.is_hardware());
        assert!(ResolvedDisplayDecoder::Qsv.is_hardware());
        assert_eq!(ResolvedDisplayDecoder::Cpu.family(), None);
        assert_eq!(
            ResolvedDisplayDecoder::Nvdec.family(),
            Some(HwDecoderFamily::Nvdec)
        );
        assert_eq!(
            ResolvedDisplayDecoder::Qsv.family(),
            Some(HwDecoderFamily::Qsv)
        );
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

    #[test]
    fn hw_encoder_family_classifies_codec_strings() {
        assert_eq!(
            HwEncoderFamily::classify("h264_nvenc"),
            Some(HwEncoderFamily::Nvenc)
        );
        assert_eq!(
            HwEncoderFamily::classify("hevc_nvenc"),
            Some(HwEncoderFamily::Nvenc)
        );
        assert_eq!(
            HwEncoderFamily::classify("h264_qsv"),
            Some(HwEncoderFamily::Qsv)
        );
        assert_eq!(
            HwEncoderFamily::classify("HEVC_QSV"),
            Some(HwEncoderFamily::Qsv)
        );
        assert_eq!(
            HwEncoderFamily::classify("h264_videotoolbox"),
            Some(HwEncoderFamily::VideoToolbox)
        );
        assert_eq!(
            HwEncoderFamily::classify("h264_amf"),
            Some(HwEncoderFamily::Amf)
        );
        assert_eq!(HwEncoderFamily::classify("libx264"), None);
        assert_eq!(HwEncoderFamily::classify("libx265"), None);
        assert_eq!(HwEncoderFamily::classify("h264"), None);
    }

    #[test]
    fn hw_session_limits_is_empty_when_no_family_probed() {
        let limits = HwSessionLimits::default();
        assert!(limits.is_empty());
    }

    #[test]
    fn hw_session_limits_not_empty_when_one_family_probed() {
        let limits = HwSessionLimits {
            nvenc_max_sessions: Some(3),
            ..Default::default()
        };
        assert!(!limits.is_empty());
    }

    #[test]
    fn hw_decoder_session_limits_is_empty_default() {
        let limits = HwDecoderSessionLimits::default();
        assert!(limits.is_empty());
    }
}
