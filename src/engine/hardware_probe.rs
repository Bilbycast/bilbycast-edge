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
    /// VAAPI (Linux): AMD Mesa radeonsi or Intel iHD via libva. Both
    /// encoder and decoder share the same field shape — older
    /// managers ignore the unknown field, newer managers light up the
    /// VAAPI rows on the Resources card.
    #[serde(default)]
    pub h264_vaapi: bool,
    #[serde(default)]
    pub hevc_vaapi: bool,
    /// RKMPP (Rockchip Media Process Platform) on ARM Rockchip SoCs
    /// (RK3568 / RK3588). Encoder side only — `h264_rkmpp` / `hevc_rkmpp`
    /// are 8-bit 4:2:0. Older managers ignore the unknown field; newer
    /// managers light up the RKMPP row on the Resources card.
    #[serde(default)]
    pub h264_rkmpp: bool,
    #[serde(default)]
    pub hevc_rkmpp: bool,
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
            || self.h264_vaapi
            || self.hevc_vaapi
            || self.h264_rkmpp
            || self.hevc_rkmpp
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
    /// VAAPI encoder session ceiling — Mesa radeonsi (AMD) typically
    /// caps low (one or two concurrent encode sessions per VCN
    /// engine), Intel iHD scales higher.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vaapi_max_sessions: Option<u32>,
    /// Concurrent 4K-grade NVENC encode sessions (probed at
    /// 3840×2160). Capacity at 4K is materially smaller than at 1080p
    /// on consumer GPUs (VRAM, engine throughput). `None` when the 4K
    /// tier was disabled via `BILBYCAST_PROBE_4K=0` or when the family
    /// is absent / failed to open at 4K.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvenc_max_sessions_4k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qsv_max_sessions_4k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amf_max_sessions_4k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vaapi_max_sessions_4k: Option<u32>,
    /// RKMPP (Rockchip VPU) encoder session ceiling. RK3568 is 1080p60-max
    /// (no 4K encode → `rkmpp_max_sessions_4k` stays `None`); RK3588 encodes
    /// up to 8K30 and is throughput-bound, so the probed count is an estimate
    /// of concurrent 1080p / 4K sessions the single VEPU sustains. Older
    /// managers omit these fields (serde skips `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rkmpp_max_sessions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rkmpp_max_sessions_4k: Option<u32>,
}

impl HwSessionLimits {
    /// `true` when no per-family limit was probed. Lets the manager UI
    /// degrade to "in use: N" without a `/M` denominator and saves a
    /// `Some(_)` round-trip on hosts where probing was disabled.
    pub fn is_empty(&self) -> bool {
        self.nvenc_max_sessions.is_none()
            && self.qsv_max_sessions.is_none()
            && self.amf_max_sessions.is_none()
            && self.vaapi_max_sessions.is_none()
            && self.nvenc_max_sessions_4k.is_none()
            && self.qsv_max_sessions_4k.is_none()
            && self.amf_max_sessions_4k.is_none()
            && self.vaapi_max_sessions_4k.is_none()
            && self.rkmpp_max_sessions.is_none()
            && self.rkmpp_max_sessions_4k.is_none()
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
    /// VAAPI decoder session ceiling. Same probe shape as the
    /// encoder twin. Note: AMD's previous "no FFmpeg HW decoder name"
    /// gap is filled by VAAPI on Linux — `h264_vaapi` /
    /// `hevc_vaapi` are real decoder names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vaapi_max_sessions: Option<u32>,
    /// Concurrent 4K-grade decode sessions (probed at 3840×2160). HW
    /// decoders pre-allocate session resources at `avcodec_open2`
    /// time when given a coded-dimensions hint, so the 4K count
    /// reflects realistic concurrent workload rather than the raw
    /// open-context count. `None` when disabled or the family failed
    /// to open at 4K.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvdec_max_sessions_4k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qsv_max_sessions_4k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vaapi_max_sessions_4k: Option<u32>,
}

impl HwDecoderSessionLimits {
    pub fn is_empty(&self) -> bool {
        self.nvdec_max_sessions.is_none()
            && self.qsv_max_sessions.is_none()
            && self.vaapi_max_sessions.is_none()
            && self.nvdec_max_sessions_4k.is_none()
            && self.qsv_max_sessions_4k.is_none()
            && self.vaapi_max_sessions_4k.is_none()
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

    // VAAPI per-(codec, chroma, bit-depth) cells. H.264 VAAPI is
    // 4:2:0 8-bit only on every implementation, so only the HEVC
    // VAAPI rows have non-baseline cells. Driver support varies:
    //   • Intel iHD (Tiger Lake / 11th gen+): HEVC 4:2:2 8/10-bit.
    //   • AMD VCN (radeonsi): generally 4:2:0 only — broadcast 4:2:2
    //     contribution on AMD typically falls back to libx265.
    /// HEVC 4:2:0 10-bit (P010LE) — Intel iHD on Skylake+, AMD VCN
    /// on RDNA2+. The most-common 10-bit broadcast cell.
    #[serde(default)] pub hevc_vaapi_yuv420_10bit: bool,
    /// HEVC 4:2:2 8-bit (NV16) — Intel iHD on Tiger Lake+ HEVC. The
    /// SDI baseband contribution standard. AMD VCN typically rejects.
    #[serde(default)] pub hevc_vaapi_yuv422_8bit: bool,
    /// HEVC 4:2:2 10-bit (P210LE) — Intel iHD on Tiger Lake+ HEVC.
    /// The 10-bit SDI broadcast contribution standard (used in both
    /// SDR and HDR workflows). AMD VCN typically rejects.
    #[serde(default)] pub hevc_vaapi_yuv422_10bit: bool,
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
            || self.libx265_yuv422_10bit
            || self.hevc_vaapi_yuv420_10bit
            || self.hevc_vaapi_yuv422_8bit
            || self.hevc_vaapi_yuv422_10bit)
    }
}

/// VAAPI capability tagged by direction. Newer wire shape that
/// disambiguates the four VAAPI booleans operators commonly want to
/// distinguish — Mesa radeonsi (AMD) on older RDNA cards has decode
/// but not all HEVC encode profiles; Intel iHD on Skylake+ generally
/// has both. The legacy `HwCodecCapability::{h264,hevc}_vaapi` fields
/// stay populated to mirror these values for older managers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VaapiCapability {
    #[serde(default)]
    pub h264_encode: bool,
    #[serde(default)]
    pub h264_decode: bool,
    #[serde(default)]
    pub hevc_encode: bool,
    #[serde(default)]
    pub hevc_decode: bool,
}

impl VaapiCapability {
    /// `true` when no field is set — the manager UI hides the VAAPI
    /// row entirely on hosts where libva isn't installed or the user
    /// doesn't have render-node access.
    pub fn is_empty(&self) -> bool {
        !(self.h264_encode || self.h264_decode || self.hevc_encode || self.hevc_decode)
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
    /// VAAPI capability split by direction. New managers prefer this
    /// flat-named view; older managers fall back to the legacy
    /// `hw_encoders.{h264,hevc}_vaapi` / `hw_decoders.{h264,hevc}_vaapi`
    /// pair, which is mirrored to the same values.
    #[serde(default, skip_serializing_if = "VaapiCapability::is_empty")]
    pub vaapi: VaapiCapability,
    pub cpu: CpuInfo,
    pub sw_capacity: SwCapacityEstimate,
    /// `true` when the kernel accepts `setsockopt(SO_TXTIME)` on a UDP
    /// socket. Linux ≥ 4.19 typically yes; older kernels and non-Linux
    /// hosts no. Drives the `wire_pacing_txtime` capability advertised
    /// to the manager. See `engine::wire_emit_txtime`. Note: a `true`
    /// here does NOT imply ETF qdisc is installed — that's an
    /// operator-side concern surfaced via the runbook.
    #[serde(default)]
    pub wire_pacing_txtime: bool,
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
    // VAAPI direction-tagged view — assembled from the encoder + decoder
    // probe results so older managers (reading the legacy
    // `hw_encoders.h264_vaapi` field) and newer managers (reading
    // `vaapi.h264_encode`) report the same answer.
    let vaapi = VaapiCapability {
        h264_encode: hw_encoders.h264_vaapi,
        h264_decode: hw_decoders.h264_vaapi,
        hevc_encode: hw_encoders.hevc_vaapi,
        hevc_decode: hw_decoders.hevc_vaapi,
    };
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
    let wire_pacing_txtime = crate::engine::wire_emit_txtime::probe();
    if wire_pacing_txtime {
        tracing::info!(
            "SO_TXTIME accepted by kernel; wire_pacing_txtime capability advertised. \
             ETF qdisc must still be installed by the operator on the egress NIC \
             for actual kernel pacing — see packaging/setup-etf-qdisc.sh"
        );
    }
    StaticCapabilities {
        hw_encoders,
        hw_decoders,
        hw_encoder_session_limits,
        hw_decoder_session_limits,
        hw_encoder_chroma,
        vaapi,
        cpu,
        sw_capacity,
        wire_pacing_txtime,
    }
}

/// Probe each available encoder for the (chroma × bit-depth) matrix.
/// Skipped per-codec when the baseline 4:2:0 8-bit probe didn't even
/// return true — there's no point asking "can it do 4:2:2?" if it
/// can't open at all.
#[cfg(feature = "media-codecs")]
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
    // VAAPI per-cell probes via `VideoEncoder::open()` — the generic
    // `probe_chroma` path doesn't set up the AVHWDeviceContext +
    // hw_frames_ctx that VAAPI requires. Only HEVC has non-baseline
    // cells (h264_vaapi is 4:2:0 8-bit only on every implementation).
    if hw.hevc_vaapi {
        out.hevc_vaapi_yuv420_10bit =
            probe_vaapi_chroma("hevc_vaapi", video_engine::ProbeChroma::Yuv420_10bit);
        out.hevc_vaapi_yuv422_8bit =
            probe_vaapi_chroma("hevc_vaapi", video_engine::ProbeChroma::Yuv422_8bit);
        out.hevc_vaapi_yuv422_10bit =
            probe_vaapi_chroma("hevc_vaapi", video_engine::ProbeChroma::Yuv422_10bit);
    }

    out
}

#[cfg(not(feature = "media-codecs"))]
fn probe_encoder_chroma_capability(_: &HwCodecCapability) -> HwEncoderChromaCapability {
    HwEncoderChromaCapability::default()
}

/// Single-shot chroma/bit-depth probe with consistent logging.
#[cfg(feature = "media-codecs")]
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

/// VAAPI-aware chroma/bit-depth probe. Routes through
/// `probe_open_vaapi_encoder_chroma` which exercises the full
/// hwdevice + hw_frames_ctx setup — `probe_chroma` (which uses the
/// generic encoder probe) returns false for every VAAPI cell because
/// VAAPI encoders require an `AVHWDeviceContext` set on the codec
/// context before `avcodec_open2`.
#[cfg(feature = "media-codecs")]
fn probe_vaapi_chroma(name: &str, chroma: video_engine::ProbeChroma) -> bool {
    match video_engine::probe_open_vaapi_encoder_chroma(name, chroma) {
        Ok(()) => {
            tracing::debug!("vaapi chroma probe ok: {name} {}", chroma.label());
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            tracing::trace!(
                "vaapi chroma probe skipped: {name} {} (not supported by encoder)",
                chroma.label()
            );
            false
        }
        Err(e) => {
            tracing::debug!("vaapi chroma probe failed: {name} {}: {e}", chroma.label());
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

/// Honour `BILBYCAST_PROBE_4K=0` (or `false`) to skip the additional
/// 4K-tier session-capacity probe. Default on. Operators on
/// 1080p-only deployments who want a faster startup opt out — the
/// existing 1080p probe still runs, only the second-tier 4K pass is
/// skipped. The global `BILBYCAST_PROBE_SESSION_LIMITS=0` switch
/// disables both tiers.
fn probe_4k_enabled() -> bool {
    match std::env::var("BILBYCAST_PROBE_4K") {
        Ok(v) => {
            let lower = v.trim().to_ascii_lowercase();
            !matches!(lower.as_str(), "0" | "false" | "no" | "off")
        }
        Err(_) => true,
    }
}

/// Upper bound on the 1080p session count probe. Bumped past the
/// previous `8` so pro-class GPUs with materially higher capacity
/// (server-class NVENC, Intel Arc QSV, large iGPU pools) report
/// honest numbers instead of a flat `≥ 8`. The probe still runs at
/// `avcodec_open2` time only — startup latency scales linearly with
/// the cap, but each open is cheap (~tens of ms even at 1080p) so
/// 32 keeps total well under a second per family on real hardware.
const SESSION_PROBE_UPPER_BOUND: u32 = 32;

/// Upper bound on the 4K-tier session count probe. Held tighter than
/// the 1080p ceiling because (a) 4K capacity is naturally lower on
/// every GPU, (b) holding 32 × 4K NV12 surface contexts open at once
/// would consume ~1.5 GiB of VRAM on the probe path alone, and (c)
/// startup time scales linearly with the cap.
const FOUR_K_UPPER_BOUND: u32 = 8;

/// Probe per-family encoder session capacity for every family with at
/// least one codec confirmed available in [`probe_hw_encoders`]. Two
/// passes: a 1080p baseline (the dominant HD-tier broadcast workload
/// class — mapped onto the existing `*_max_sessions` wire fields) and
/// an optional 4K tier (mapped onto the new `*_max_sessions_4k`
/// fields, gated by `BILBYCAST_PROBE_4K`). Probes one codec per
/// family (the H.264 variant — H.264 + HEVC share the engine on
/// every supported backend) so we don't double the startup cost.
/// Returns `None` for families where no codec was available at
/// runtime, or where the 1080p baseline returned 0 (no point probing
/// 4K when the engine can't open at HD).
#[cfg(feature = "media-codecs")]
fn probe_encoder_session_limits(hw: &HwCodecCapability) -> HwSessionLimits {
    let mut out = HwSessionLimits::default();
    let probe_4k = probe_4k_enabled();
    if hw.h264_nvenc || hw.hevc_nvenc {
        let probe_codec = if hw.h264_nvenc { "h264_nvenc" } else { "hevc_nvenc" };
        let n_1080p = video_engine::count_max_encoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P,
            video_engine::PROBE_HEIGHT_1080P,
        );
        if n_1080p > 0 {
            tracing::info!("nvenc encoder 1080p session capacity probed: {n_1080p}");
            out.nvenc_max_sessions = Some(n_1080p);
            if probe_4k {
                let n_4k = video_engine::count_max_encoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K,
                    video_engine::PROBE_HEIGHT_4K,
                );
                if n_4k > 0 {
                    tracing::info!("nvenc encoder 4K session capacity probed: {n_4k}");
                    out.nvenc_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    if hw.h264_qsv || hw.hevc_qsv {
        let probe_codec = if hw.h264_qsv { "h264_qsv" } else { "hevc_qsv" };
        let n_1080p = video_engine::count_max_encoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P,
            video_engine::PROBE_HEIGHT_1080P,
        );
        if n_1080p > 0 {
            tracing::info!("qsv encoder 1080p session capacity probed: {n_1080p}");
            out.qsv_max_sessions = Some(n_1080p);
            if probe_4k {
                let n_4k = video_engine::count_max_encoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K,
                    video_engine::PROBE_HEIGHT_4K,
                );
                if n_4k > 0 {
                    tracing::info!("qsv encoder 4K session capacity probed: {n_4k}");
                    out.qsv_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    if hw.h264_amf || hw.hevc_amf {
        let probe_codec = if hw.h264_amf { "h264_amf" } else { "hevc_amf" };
        let n_1080p = video_engine::count_max_encoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P,
            video_engine::PROBE_HEIGHT_1080P,
        );
        if n_1080p > 0 {
            tracing::info!("amf encoder 1080p session capacity probed: {n_1080p}");
            out.amf_max_sessions = Some(n_1080p);
            if probe_4k {
                let n_4k = video_engine::count_max_encoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K,
                    video_engine::PROBE_HEIGHT_4K,
                );
                if n_4k > 0 {
                    tracing::info!("amf encoder 4K session capacity probed: {n_4k}");
                    out.amf_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    if hw.h264_vaapi || hw.hevc_vaapi {
        let probe_codec = if hw.h264_vaapi { "h264_vaapi" } else { "hevc_vaapi" };
        // VAAPI session-count probe needs the full `VideoEncoder::open()`
        // path (hwdevice + frames-context setup), not the generic
        // `count_max_encoder_sessions` which uses `try_open_encoder_context`
        // and would return 0 every time on VAAPI.
        let n_1080p = video_engine::count_max_vaapi_encoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P as u32,
            video_engine::PROBE_HEIGHT_1080P as u32,
        );
        if n_1080p > 0 {
            tracing::info!("vaapi encoder 1080p session capacity probed: {n_1080p}");
            out.vaapi_max_sessions = Some(n_1080p);
            if probe_4k {
                let n_4k = video_engine::count_max_vaapi_encoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K as u32,
                    video_engine::PROBE_HEIGHT_4K as u32,
                );
                if n_4k > 0 {
                    tracing::info!("vaapi encoder 4K session capacity probed: {n_4k}");
                    out.vaapi_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    if hw.h264_rkmpp || hw.hevc_rkmpp {
        // RKMPP uses the generic session counter (sysmem NV12, no hwdevice)
        // — same path as NVENC/QSV/AMF, unlike VAAPI. The single Rockchip
        // VEPU is throughput-bound, so this measures how many concurrent
        // sessions it sustains at the probe resolution.
        let probe_codec = if hw.h264_rkmpp { "h264_rkmpp" } else { "hevc_rkmpp" };
        let n_1080p = video_engine::count_max_encoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P,
            video_engine::PROBE_HEIGHT_1080P,
        );
        if n_1080p > 0 {
            tracing::info!("rkmpp encoder 1080p session capacity probed: {n_1080p}");
            out.rkmpp_max_sessions = Some(n_1080p);
            if probe_4k {
                // RK3568 has no 4K encode path at all — this probe returns
                // 0 there so `rkmpp_max_sessions_4k` stays `None`. RK3588
                // (4K/8K encode) returns a real count.
                let n_4k = video_engine::count_max_encoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K,
                    video_engine::PROBE_HEIGHT_4K,
                );
                if n_4k > 0 {
                    tracing::info!("rkmpp encoder 4K session capacity probed: {n_4k}");
                    out.rkmpp_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    out
}

#[cfg(not(feature = "media-codecs"))]
fn probe_encoder_session_limits(_: &HwCodecCapability) -> HwSessionLimits {
    HwSessionLimits::default()
}

/// Decoder twin of [`probe_encoder_session_limits`]. Probes NVDEC
/// (`h264_cuvid`) and QSV decode (`h264_qsv`) and VAAPI decode — AMF
/// has no first-party FFmpeg HW decoder so it's omitted. Same 1080p +
/// 4K tier shape as the encoder twin; HW decoders that pre-allocate
/// session resources at `avcodec_open2` time when given a coded-
/// dimensions hint reflect the realistic workload class.
#[cfg(feature = "media-codecs")]
fn probe_decoder_session_limits(hw: &HwCodecCapability) -> HwDecoderSessionLimits {
    let mut out = HwDecoderSessionLimits::default();
    let probe_4k = probe_4k_enabled();
    if hw.h264_nvenc || hw.hevc_nvenc {
        // hw_decoders.h264_nvenc actually corresponds to h264_cuvid by
        // wire convention — the field stays `h264_nvenc` for symmetry
        // with the encoder shape, but the underlying probe used the
        // cuvid name.
        let probe_codec = if hw.h264_nvenc { "h264_cuvid" } else { "hevc_cuvid" };
        let n_1080p = video_engine::count_max_decoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P,
            video_engine::PROBE_HEIGHT_1080P,
        );
        if n_1080p > 0 {
            tracing::info!("nvdec decoder 1080p session capacity probed: {n_1080p}");
            out.nvdec_max_sessions = Some(n_1080p);
            if probe_4k {
                let n_4k = video_engine::count_max_decoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K,
                    video_engine::PROBE_HEIGHT_4K,
                );
                if n_4k > 0 {
                    tracing::info!("nvdec decoder 4K session capacity probed: {n_4k}");
                    out.nvdec_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    if hw.h264_qsv || hw.hevc_qsv {
        let probe_codec = if hw.h264_qsv { "h264_qsv" } else { "hevc_qsv" };
        let n_1080p = video_engine::count_max_decoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P,
            video_engine::PROBE_HEIGHT_1080P,
        );
        if n_1080p > 0 {
            tracing::info!("qsv decoder 1080p session capacity probed: {n_1080p}");
            out.qsv_max_sessions = Some(n_1080p);
            if probe_4k {
                let n_4k = video_engine::count_max_decoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K,
                    video_engine::PROBE_HEIGHT_4K,
                );
                if n_4k > 0 {
                    tracing::info!("qsv decoder 4K session capacity probed: {n_4k}");
                    out.qsv_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    if hw.h264_vaapi || hw.hevc_vaapi {
        let probe_codec = if hw.h264_vaapi { "h264_vaapi" } else { "hevc_vaapi" };
        let n_1080p = video_engine::count_max_decoder_sessions(
            probe_codec,
            SESSION_PROBE_UPPER_BOUND,
            video_engine::PROBE_WIDTH_1080P,
            video_engine::PROBE_HEIGHT_1080P,
        );
        if n_1080p > 0 {
            tracing::info!("vaapi decoder 1080p session capacity probed: {n_1080p}");
            out.vaapi_max_sessions = Some(n_1080p);
            if probe_4k {
                let n_4k = video_engine::count_max_decoder_sessions(
                    probe_codec,
                    FOUR_K_UPPER_BOUND,
                    video_engine::PROBE_WIDTH_4K,
                    video_engine::PROBE_HEIGHT_4K,
                );
                if n_4k > 0 {
                    tracing::info!("vaapi decoder 4K session capacity probed: {n_4k}");
                    out.vaapi_max_sessions_4k = Some(n_4k);
                }
            }
        }
    }
    out
}

#[cfg(not(feature = "media-codecs"))]
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
        // VAAPI encoder probe routes through `VideoEncoder::open()` so
        // it exercises the full hwdevice + frames-context setup —
        // `probe_open_encoder` (which reuses the generic
        // `try_open_encoder_context`) doesn't, and would always fail
        // because VAAPI requires `hw_device_ctx` set on the codec
        // before `avcodec_open2`.
        h264_vaapi: probe_runtime_vaapi_encoder("h264_vaapi"),
        hevc_vaapi: probe_runtime_vaapi_encoder("hevc_vaapi"),
        // RKMPP (Rockchip VPU) uses the generic no-hwdevice probe path:
        // the encoder accepts a plain sysmem NV12 frame and auto-creates
        // its own MPP device internally, so `probe_runtime_encoder`'s
        // avcodec_open2 round-trip suffices (unlike VAAPI). Returns false
        // on non-Rockchip / non-rkmpp builds (encoder not compiled in).
        h264_rkmpp: probe_rkmpp_encoder("h264_rkmpp"),
        hevc_rkmpp: probe_rkmpp_encoder("hevc_rkmpp"),
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
        // AMD has no first-party FFmpeg HW decoder name *outside* VAAPI;
        // AMF is encode-only at the FFmpeg layer (decode rides VAAPI on
        // Linux, D3D11VA on Windows). The VAAPI fields below cover AMD
        // decode now.
        h264_amf: false,
        hevc_amf: false,
        // VAAPI decode — VAAPI is implemented in FFmpeg as a HWACCEL
        // attached to the standard `h264` / `hevc` SW decoders, NOT a
        // separately-named decoder entry. So `avcodec_find_decoder_by_name(
        // "h264_vaapi")` returns NULL even on a working VAAPI host.
        // The right probe is "can we open an `AV_HWDEVICE_TYPE_VAAPI`
        // hwdevice on the default render node?" — when that succeeds,
        // every codec with a `--enable-hwaccel=*_vaapi` flag in the
        // build (h264 / hevc / mpeg2 today) can use the zero-copy
        // path. Cache the result so we don't open + tear down the
        // VAAPI device twice in the probe.
        h264_vaapi: probe_runtime_vaapi_device(),
        hevc_vaapi: probe_runtime_vaapi_device(),
        // RKMPP decode is not wired into a codec-selection path in this
        // pass (the Rockchip VPU has capable h264/hevc/av1 decoders, but
        // neither the transcode input-decode nor the display output selects
        // rkmpp decode yet). Stubbed `false` like AMF on the decoder side.
        h264_rkmpp: false,
        hevc_rkmpp: false,
    }
}

/// Probe whether a VAAPI hwdevice can be opened against the default
/// render node. `true` iff `av_hwdevice_ctx_create(VAAPI, ...)`
/// succeeds — confirms libva + the kernel driver (i915 / amdgpu /
/// nouveau) are wired up and the running user has permission on
/// `/dev/dri/renderD128`. Cheap (~10 ms) and idempotent; called once
/// per startup-probe pass.
#[cfg(feature = "media-codecs")]
fn probe_runtime_vaapi_device() -> bool {
    match video_engine::VaapiDevice::open(None) {
        Ok(_) => {
            tracing::debug!("vaapi hwdevice probe ok");
            true
        }
        Err(e) => {
            tracing::debug!("vaapi hwdevice probe failed: {e}");
            false
        }
    }
}

#[cfg(not(feature = "media-codecs"))]
fn probe_runtime_vaapi_device() -> bool {
    false
}

/// VAAPI encoder probe via the full `VideoEncoder::open()` path.
/// Confirms hwdevice + frames-context setup + `avcodec_open2` with the
/// VAAPI codec all succeed for the requested codec at 320×240 8-bit
/// 4:2:0 — meaningful answer for "can this host encode H.264 / HEVC
/// via VAAPI right now?". Build prerequisite: `video-encoder-vaapi`
/// feature on the `video-engine` crate.
#[cfg(feature = "media-codecs")]
fn probe_runtime_vaapi_encoder(name: &str) -> bool {
    match video_engine::probe_open_vaapi_encoder(name) {
        Ok(()) => {
            tracing::debug!("vaapi encoder probe ok: {name}");
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            tracing::trace!("vaapi encoder not compiled in: {name}");
            false
        }
        Err(e) => {
            tracing::debug!("vaapi encoder probe failed: {name}: {e}");
            false
        }
    }
}

#[cfg(not(feature = "media-codecs"))]
fn probe_runtime_vaapi_encoder(_: &str) -> bool {
    false
}

/// Generic runtime encoder probe — try `avcodec_open2`, classify the
/// result. Used directly for AMF (no special retry / diagnostic
/// behaviour); NVENC and QSV wrap this with backend-specific
/// pre/post-conditions.
#[cfg(feature = "media-codecs")]
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

#[cfg(not(feature = "media-codecs"))]
fn probe_runtime_encoder(_: &str) -> bool {
    // No FFmpeg binding compiled in — every probe answers "no".
    false
}

/// Generic runtime decoder probe.
#[cfg(feature = "media-codecs")]
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

#[cfg(not(feature = "media-codecs"))]
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
#[cfg(feature = "media-codecs")]
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

#[cfg(not(feature = "media-codecs"))]
fn probe_nvenc_encoder(_: &str) -> bool {
    false
}

/// QSV encoder probe with `EACCES` diagnostic. QSV opens
/// `/dev/dri/renderD128` (or `card0`) — when the running user lacks
/// access (typical for systemd services that aren't in the `video` and
/// `render` groups), the open fails with `EACCES`. That's a fixable
/// permissions issue, distinct from "no Intel iGPU" — surface it loudly
/// so operators don't waste time chasing GPU driver problems.
#[cfg(feature = "media-codecs")]
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

#[cfg(not(feature = "media-codecs"))]
fn probe_qsv_encoder(_: &str) -> bool {
    false
}

/// RKMPP (Rockchip MPP) encoder probe with a `/dev/mpp_service`
/// diagnostic. Uses the generic `probe_open_encoder` avcodec_open2
/// round-trip — the rkmpp encoder accepts a plain sysmem NV12 frame and
/// auto-creates its own MPP device internally, so no VAAPI-style hwdevice
/// pre-setup is needed. On non-Rockchip / non-rkmpp builds the encoder
/// isn't compiled into FFmpeg (`NotCompiled` → false). When the MPP kernel
/// node is present but the open fails, the running user usually lacks
/// access to `/dev/mpp_service` (add them to the `video` group) or
/// librockchip_mpp is older than 1.3.8 — surface that loudly, mirroring
/// the QSV `EACCES` diagnostic.
#[cfg(feature = "media-codecs")]
fn probe_rkmpp_encoder(name: &str) -> bool {
    match video_engine::probe_open_encoder(name) {
        Ok(()) => {
            tracing::debug!("rkmpp probe ok: {name}");
            true
        }
        Err(video_engine::ProbeError::NotCompiled) => {
            tracing::trace!("rkmpp not compiled in: {name}");
            false
        }
        Err(video_engine::ProbeError::PermissionDenied) => {
            tracing::warn!(
                "rkmpp probe denied for {name}: EACCES on the Rockchip MPP device. \
                Add the running user to the 'video' group (or grant access to \
                /dev/mpp_service) and restart."
            );
            false
        }
        Err(e) => {
            if std::path::Path::new("/dev/mpp_service").exists() {
                tracing::warn!(
                    "rkmpp probe failed for {name}: {e}. The MPP kernel node \
                    /dev/mpp_service is present — check that librockchip_mpp is \
                    >= 1.3.8 and the VPU driver is loaded."
                );
            } else {
                tracing::debug!("rkmpp probe failed: {name}: {e}");
            }
            false
        }
    }
}

#[cfg(not(feature = "media-codecs"))]
fn probe_rkmpp_encoder(_: &str) -> bool {
    false
}

/// VideoToolbox encoder probe. macOS-only: the framework is
/// system-provided, so a non-NULL FFmpeg registry entry is sufficient
/// proof — a runtime open could grab and release a CoreMedia session
/// that competing apps are using, for no extra information. On non-Mac
/// hosts the codec isn't in the build at all and the probe returns
/// false via the registry pre-filter.
#[cfg(all(feature = "media-codecs", target_os = "macos"))]
fn probe_videotoolbox_encoder(name: &str) -> bool {
    let present = video_engine::is_encoder_available(name);
    if present {
        tracing::debug!("videotoolbox encoder available: {name}");
    }
    present
}

#[cfg(all(feature = "media-codecs", target_os = "macos"))]
fn probe_videotoolbox_decoder(name: &str) -> bool {
    let present = video_engine::is_decoder_available(name);
    if present {
        tracing::debug!("videotoolbox decoder available: {name}");
    }
    present
}

#[cfg(not(all(feature = "media-codecs", target_os = "macos")))]
fn probe_videotoolbox_encoder(_: &str) -> bool {
    false
}

#[cfg(not(all(feature = "media-codecs", target_os = "macos")))]
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
/// | Flow shape                                          | Base units    |
/// |---|---|
/// | Passthrough flow (base)                             | 1             |
/// | Each video_encode output (HW, 1080p30 4:2:0 8-bit)  | 100 × scale   |
/// | Each video_encode output (SW, 1080p30 4:2:0 8-bit)  | 500 × scale   |
/// | Each audio_encode output                            | 5             |
/// | content_analysis = lite                             | 2             |
/// | content_analysis = audio_full                       | 20            |
/// | content_analysis = video_full                       | 50            |
/// | recording (replay) enabled                          | 5             |
/// | thumbnail (per generator @ 5 s default)             | 3             |
///
/// Thumbnail cost scales with cadence (work ∝ 1/interval) and with the
/// number of concurrent generators a flow runs — one flow-level preview
/// plus one per input. `derive_cost_plan` folds both into the precomputed
/// [`FlowCostPlan::thumbnail_units`]; see [`thumbnail_units_per_decoder`].
///
/// `scale` for video_encode = (width × height × fps) / (1920 × 1080 × 30)
///   × 1.5 if 10-bit
///   × 1.33 if 4:2:2 chroma  (× 2.0 if 4:4:4)
///
/// `derive_cost_plan` precomputes the scaled units per output (since
/// the resolution / fps / chroma data lives on the per-output config,
/// not on the aggregated plan), so [`FlowCostPlan::hw_video_encode_units`]
/// already carries the pixel-rate-aware value. This function just adds
/// it onto the base.
///
/// All flows pay the base passthrough cost (1) regardless of shape.
/// Transcoding / analysis / recording add on top.
/// Thumbnail cost-model anchors. Mirror `engine::thumbnail`'s 5 s default,
/// kept local so the cost model doesn't depend on that module's privates.
const THUMBNAIL_DEFAULT_INTERVAL_SECS: u32 = 5;
/// Units for one thumbnail generator at the 5 s default cadence. A generator
/// decodes a frame + downscales to 320×180 + JPEG-encodes + computes
/// luminance every interval — a real decode, so it sits above
/// `content_analysis_lite` (2, compressed-domain only) and well below
/// `content_analysis_video_full` (50, a 1 Hz decode plus pixel metrics).
const THUMBNAIL_UNITS_AT_DEFAULT: u32 = 3;

/// Cost units for ONE thumbnail generator at the given capture cadence.
/// Decode work is roughly constant per tick and ticks run at `1/interval`,
/// so cost scales linearly with frequency: `round(3 × 5 / interval)`, floored
/// at 1. `None` = the 5 s default (3 units). The interval is clamped to the
/// validated `1..=60` range. Multiply by the generator count (flow-level +
/// per-input) for the flow total — see `derive_cost_plan`.
pub fn thumbnail_units_per_decoder(interval_secs: Option<u32>) -> u32 {
    let secs = interval_secs
        .map(|s| s.clamp(1, 60))
        .unwrap_or(THUMBNAIL_DEFAULT_INTERVAL_SECS);
    let numer = THUMBNAIL_UNITS_AT_DEFAULT * THUMBNAIL_DEFAULT_INTERVAL_SECS;
    // Integer round-half-up, then floor at 1.
    ((numer + secs / 2) / secs).max(1)
}

pub fn compute_flow_cost_units(plan: &FlowCostPlan) -> u32 {
    let mut units: u32 = 1; // base
    units = units.saturating_add(plan.hw_video_encode_units);
    units = units.saturating_add(plan.sw_video_encode_units);
    units = units.saturating_add(
        5u32.saturating_mul(
            plan.audio_encode_outputs.saturating_add(plan.audio_encode_inputs),
        ),
    );
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
    // Thumbnail preview generation — precomputed in `derive_cost_plan`
    // (number of concurrent generators × per-generator cadence cost).
    units = units.saturating_add(plan.thumbnail_units);
    units
}

/// Lightweight description of a flow's cost-driving shape. Decoupled
/// from `FlowConfig` so tests can drive the cost model directly and so
/// the same shape can be derived from either an existing flow's runtime
/// config or a candidate flow in the manager preflight.
///
/// **Units, not counts**: `hw_video_encode_units` and `sw_video_encode_units`
/// are pre-weighted unit totals (resolution / fps / bit-depth / chroma all
/// folded in by `derive_cost_plan`'s [`weighted_video_encode_units`]) so a
/// 4K60 HEVC 4:2:2 10-bit output rolls up at ~10× the cost of a 720p30 H.264
/// 4:2:0 8-bit output, mirroring the actual GPU / CPU pressure delta.
/// Per-family session counts (`nvenc_sessions` etc.) stay as plain counts
/// because the manager uses them against the probed session limits, where
/// each session is one slot regardless of resolution.
#[derive(Debug, Clone, Default)]
pub struct FlowCostPlan {
    /// HW video encode units — pre-weighted, 100 = 1080p30 4:2:0 8-bit
    /// baseline. See `weighted_video_encode_units` for the formula.
    pub hw_video_encode_units: u32,
    /// SW (libx264 / libx265) video encode units — pre-weighted, 500 =
    /// 1080p30 4:2:0 8-bit baseline.
    pub sw_video_encode_units: u32,
    pub audio_encode_outputs: u32,
    /// Audio-encode count from the input side. Mirrors `audio_encode_outputs`
    /// for transcoders that live on inputs (e.g. an SRT/RTP/RTMP input with
    /// `audio_encode` set, or an ST 2110-30/-31 input encoding to AAC into
    /// the PID bus). Folded into the audio cost-unit total alongside the
    /// output side — see `compute_flow_cost_units`.
    pub audio_encode_inputs: u32,
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
    /// Precomputed thumbnail-preview cost: number of concurrent generators
    /// (flow-level + one per input) × per-generator cadence cost. Zero when
    /// the flow has thumbnails disabled or no inputs. See
    /// `thumbnail_units_per_decoder` and `derive_cost_plan`.
    pub thumbnail_units: u32,
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
    /// VAAPI encoder sessions on this flow (libva on Linux — AMD or
    /// Intel). Tracked alongside the other families so the manager
    /// can render `vaapi_in_use / vaapi_max` chips.
    pub vaapi_sessions: u32,
    /// 4K-tier subset of `*_sessions`. A flow output whose
    /// `video_encode.{width,height}` resolves to ≥ 3840×2160 is
    /// counted in **both** `*_sessions` (the total per family) and
    /// `*_sessions_4k`. The manager UI treats each 4K session as
    /// `floor(max_1080p / max_4k)` worth of 1080p capacity so the
    /// 1080p denominator shrinks dynamically. Sessions without an
    /// explicit resolution default to the 1080p tier.
    pub nvenc_sessions_4k: u32,
    pub qsv_sessions_4k: u32,
    pub amf_sessions_4k: u32,
    pub vaapi_sessions_4k: u32,
    /// RKMPP (Rockchip VPU) encoder sessions on this flow. RK3568 caps at
    /// 1080p (4K sessions always 0); RK3588 supports 4K, counted in both
    /// `rkmpp_sessions` and `rkmpp_sessions_4k`.
    pub rkmpp_sessions: u32,
    pub rkmpp_sessions_4k: u32,
    /// Per-family decoder session counts charged by HW-decoded
    /// `display` outputs on this flow. NVDEC and QSV-decode are
    /// tracked separately even though QSV-decode shares an iGPU with
    /// QSV-encode — operators want to see decoder pressure
    /// independently from encoder pressure when planning capacity.
    pub nvdec_sessions: u32,
    pub qsv_decode_sessions: u32,
    /// VAAPI decoder sessions charged by HW-decoded `display` outputs
    /// resolved to VAAPI (Linux; AMD or Intel via libva).
    pub vaapi_decode_sessions: u32,
    /// 4K-tier subset of decoder sessions. Display outputs don't
    /// carry an explicit source resolution at plan time, so today
    /// every HW-decoded display contributes to the 1080p tier and
    /// these fields stay `0`. Reserved for future input-side HW
    /// decode (RTSP/RTMP/SRT) where the negotiated resolution is
    /// known up front.
    pub nvdec_sessions_4k: u32,
    pub qsv_decode_sessions_4k: u32,
    pub vaapi_decode_sessions_4k: u32,
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
    /// VAAPI encoder sessions in use across every flow (Linux; AMD
    /// Mesa radeonsi or Intel iHD via libva).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub vaapi_in_use: u32,
    /// 4K-tier subset of `*_in_use`. A 4K-resolution session is
    /// counted in **both** `*_in_use` (the unconditional total) and
    /// `*_in_use_4k`. Manager UI subtracts `*_in_use_4k *
    /// floor(max_1080p / max_4k)` from the 1080p denominator so the
    /// HD ceiling shrinks as 4K work runs. Older edges omit these
    /// fields and serde defaults them to `0`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub nvenc_in_use_4k: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub qsv_in_use_4k: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub amf_in_use_4k: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub vaapi_in_use_4k: u32,
    /// RKMPP (Rockchip VPU) encoder sessions in use across every flow.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub rkmpp_in_use: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub rkmpp_in_use_4k: u32,
    /// Active hardware-decoder sessions. Charged today only by
    /// HW-decoded `display` outputs (`hw_decode` resolved to NVDEC /
    /// QSV / VAAPI). When future input-side HW decode lands the same
    /// counters absorb that contribution.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub nvdec_in_use: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub qsv_decode_in_use: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub vaapi_decode_in_use: u32,
    /// 4K-tier subset of decoder usage. Reserved for future
    /// input-side HW decode that knows the source resolution at plan
    /// time. Display outputs don't carry source resolution today, so
    /// these stay `0` until that wiring lands.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub nvdec_in_use_4k: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub qsv_decode_in_use_4k: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub vaapi_decode_in_use_4k: u32,
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
    Vaapi,
    /// Rockchip MPP (RK3568 / RK3588 VPU). H.264 + HEVC share the single
    /// on-chip VEPU engine, so one family covers both.
    Rkmpp,
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
        } else if c.contains("vaapi") {
            Some(HwEncoderFamily::Vaapi)
        } else if c.contains("rkmpp") {
            Some(HwEncoderFamily::Rkmpp)
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
    Vaapi,
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
            HwDecodePreference::Vaapi => Some(HwDecoderFamily::Vaapi),
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

/// Compute pixel-rate-aware cost units for one video_encode output.
///
/// Anchored to a 1080p30 SDR 4:2:0 8-bit baseline. Scales linearly with
/// pixels-per-second, then applies a flat 1.5× for 10-bit and a chroma
/// multiplier (1.33× for 4:2:2, 2.0× for 4:4:4) to capture the extra
/// bandwidth and arithmetic the encoder pipeline does at higher
/// chroma resolutions. Bounded below at the per-output cost weight so a
/// pathological (or just-unset-yet) resolution doesn't underprice the
/// flow at flow-start time.
///
/// `is_hw` selects the 100-unit (HW) vs 500-unit (SW) baseline. The
/// `width` / `height` / `fps_num` / `fps_den` / `bit_depth` / `chroma`
/// arguments come straight from the operator's `VideoEncodeConfig`;
/// pass `None` to defaults that match the runtime fall-throughs (1080p,
/// 30 fps, 8-bit, 4:2:0).
pub fn weighted_video_encode_units(
    is_hw: bool,
    width: Option<u32>,
    height: Option<u32>,
    fps_num: Option<u32>,
    fps_den: Option<u32>,
    bit_depth: Option<u8>,
    chroma: Option<&str>,
) -> u32 {
    let base = if is_hw { 100u32 } else { 500u32 };
    let baseline_pps = 1920.0_f32 * 1080.0 * 30.0;
    let w = width.unwrap_or(1920) as f32;
    let h = height.unwrap_or(1080) as f32;
    let fps = match (fps_num, fps_den) {
        (Some(n), Some(d)) if d > 0 => (n as f32) / (d as f32),
        _ => 30.0,
    }
    .max(1.0);
    let mut factor = (w * h * fps) / baseline_pps;
    if bit_depth.unwrap_or(8) == 10 {
        factor *= 1.5;
    }
    match chroma.unwrap_or("yuv420p") {
        "yuv422p" => factor *= 1.33,
        "yuv444p" => factor *= 2.0,
        _ => {}
    }
    let scaled = (base as f32 * factor).round();
    // Floor at the baseline so a 240p test flow doesn't accidentally
    // count as zero units; ceiling at 100 000 so a misconfigured 16K120
    // doesn't overflow the running total.
    scaled.clamp(base as f32, 100_000.0) as u32
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
/// (which only exists when `media-codecs` is on) so cost-plan code
/// can call into the resolver from non-display builds too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedDisplayDecoder {
    Cpu,
    Nvdec,
    Qsv,
    Vaapi,
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
            ResolvedDisplayDecoder::Vaapi => Some(HwDecoderFamily::Vaapi),
        }
    }

    /// Translate to the video-engine backend handle. Only callable
    /// when `media-codecs` is on (display path always has it).
    #[cfg(feature = "media-codecs")]
    pub fn as_backend(&self) -> video_engine::DecoderBackend {
        match self {
            ResolvedDisplayDecoder::Cpu => video_engine::DecoderBackend::Cpu,
            ResolvedDisplayDecoder::Nvdec => video_engine::DecoderBackend::Nvdec,
            ResolvedDisplayDecoder::Qsv => video_engine::DecoderBackend::Qsv,
            ResolvedDisplayDecoder::Vaapi => video_engine::DecoderBackend::Vaapi,
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

    let nvdec_compiled = cfg!(feature = "video-decoder-nvdec");
    let qsv_compiled = cfg!(feature = "video-decoder-qsv");
    let vaapi_compiled = cfg!(feature = "video-decoder-vaapi");
    let nvdec_ok = nvdec_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_nvenc || c.hw_decoders.hevc_nvenc);
    let qsv_ok = qsv_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_qsv || c.hw_decoders.hevc_qsv);
    let vaapi_ok = vaapi_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_vaapi || c.hw_decoders.hevc_vaapi);

    match pref {
        HwDecodePreference::Cpu => Ok(ResolvedDisplayDecoder::Cpu),
        HwDecodePreference::Auto => {
            // Auto priority: VAAPI ≻ NVDEC ≻ QSV ≻ CPU. With
            // `drmModeAtomicCommit` driving the local-display
            // page-flip (see `display::kms::KmsDisplay::present_prime`),
            // cross-modifier flips between tiled NV12 / P010 surfaces
            // and the linear XRGB8888 dumb buffer happen at vblank with
            // no full-plane reconfigure — microseconds per flip rather
            // than the 10–30 ms `drmModeSetCrtc` budget that pinned
            // 1080p sources at ~10 fps on a 60 Hz panel. That removes
            // the only reason we previously demoted VAAPI behind NVDEC
            // and QSV. Zero-copy is now strictly cheaper than the
            // NVDEC/QSV decode-then-blit pipelines on every panel rate
            // we care about: no DMA-BUF download, no libswscale colour
            // convert, no dumb-buffer write. NVDEC and QSV remain Auto
            // fallbacks for hosts where atomic-commit is rejected (the
            // edge logs `display_atomic_unavailable` once and the
            // present path stays on legacy `set_crtc`); CPU is the
            // last-resort fallback.
            if vaapi_ok {
                Ok(ResolvedDisplayDecoder::Vaapi)
            } else if nvdec_ok {
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
        HwDecodePreference::Vaapi => {
            if !vaapi_compiled {
                Err(DecoderResolutionError::FeatureDisabled)
            } else if caps.is_none() {
                Err(DecoderResolutionError::ProbeUnavailable)
            } else if !vaapi_ok {
                Err(DecoderResolutionError::DriverMissing)
            } else {
                Ok(ResolvedDisplayDecoder::Vaapi)
            }
        }
    }
}

#[cfg(any(test, all(feature = "display", target_os = "linux")))]
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

/// Resolve an operator's `video_encode.hw_decode` preference (on the
/// transcode input decoder side) into a concrete backend. Mirrors
/// [`resolve_display_decoder`] but feature-gates on the
/// `video-decoder-*` Cargo features instead of the `display-*`
/// composites — operators who want HW transcode decode but no local
/// display output build with just `video-decoder-nvdec` /
/// `video-decoder-qsv` / `video-decoder-vaapi`.
///
/// Auto picks the same VAAPI ≻ NVDEC ≻ QSV ≻ CPU priority as the
/// display path (DMA-BUF / zero-copy advantages don't apply here, but
/// VAAPI's broader broadcast format coverage on Intel iHD does).
pub fn resolve_transcode_decoder(
    pref: &crate::config::models::HwDecodePreference,
    caps: Option<&StaticCapabilities>,
) -> Result<ResolvedDisplayDecoder, DecoderResolutionError> {
    use crate::config::models::HwDecodePreference;

    let nvdec_compiled = cfg!(feature = "video-decoder-nvdec");
    let qsv_compiled = cfg!(feature = "video-decoder-qsv");
    let vaapi_compiled = cfg!(feature = "video-decoder-vaapi");
    let nvdec_ok = nvdec_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_nvenc || c.hw_decoders.hevc_nvenc);
    let qsv_ok = qsv_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_qsv || c.hw_decoders.hevc_qsv);
    let vaapi_ok = vaapi_compiled
        && caps.is_some_and(|c| c.hw_decoders.h264_vaapi || c.hw_decoders.hevc_vaapi);

    match pref {
        HwDecodePreference::Cpu => Ok(ResolvedDisplayDecoder::Cpu),
        HwDecodePreference::Auto => {
            if vaapi_ok {
                Ok(ResolvedDisplayDecoder::Vaapi)
            } else if nvdec_ok {
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
        HwDecodePreference::Vaapi => {
            if !vaapi_compiled {
                Err(DecoderResolutionError::FeatureDisabled)
            } else if caps.is_none() {
                Err(DecoderResolutionError::ProbeUnavailable)
            } else if !vaapi_ok {
                Err(DecoderResolutionError::DriverMissing)
            } else {
                Ok(ResolvedDisplayDecoder::Vaapi)
            }
        }
    }
}

// ── Video-encoder resolution (Auto + explicit) ─────────────────────
//
// Only meaningful when `media-codecs` is on (no video encoder exists
// without it). Wrapping the whole resolver in the feature gate keeps the
// `video_codec::VideoChroma` reference inside its compile-time scope.

/// Output codec family produced on the wire. The `Auto` codec strings
/// (`h264_auto` / `hevc_auto`) carry only the family; the resolver
/// picks the concrete backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderFamily {
    H264,
    Hevc,
}

/// Resolved encoder backend choice. Symmetric to [`ResolvedDisplayDecoder`]
/// but on the encode side. Drops back into a `video_codec::VideoEncoderCodec`
/// at call time when the `media-codecs` feature is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedVideoEncoder {
    X264,
    X265,
    H264Nvenc,
    HevcNvenc,
    H264Qsv,
    HevcQsv,
    H264Vaapi,
    HevcVaapi,
    /// Rockchip MPP H.264 hardware encoder (ARM RK3568 / RK3588).
    H264Rkmpp,
    /// Rockchip MPP HEVC hardware encoder (ARM RK3568 / RK3588).
    HevcRkmpp,
}

impl ResolvedVideoEncoder {
    /// FFmpeg / config-string identifier — matches `parse_codec` /
    /// `resolve_backend` keys in `ts_video_replace.rs` and
    /// `output_rtmp.rs`.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            ResolvedVideoEncoder::X264 => "x264",
            ResolvedVideoEncoder::X265 => "x265",
            ResolvedVideoEncoder::H264Nvenc => "h264_nvenc",
            ResolvedVideoEncoder::HevcNvenc => "hevc_nvenc",
            ResolvedVideoEncoder::H264Qsv => "h264_qsv",
            ResolvedVideoEncoder::HevcQsv => "hevc_qsv",
            ResolvedVideoEncoder::H264Vaapi => "h264_vaapi",
            ResolvedVideoEncoder::HevcVaapi => "hevc_vaapi",
            ResolvedVideoEncoder::H264Rkmpp => "h264_rkmpp",
            ResolvedVideoEncoder::HevcRkmpp => "hevc_rkmpp",
        }
    }

    /// Output codec family — the wire-level codec, ignoring backend.
    pub fn family(self) -> EncoderFamily {
        match self {
            ResolvedVideoEncoder::X264
            | ResolvedVideoEncoder::H264Nvenc
            | ResolvedVideoEncoder::H264Qsv
            | ResolvedVideoEncoder::H264Vaapi
            | ResolvedVideoEncoder::H264Rkmpp => EncoderFamily::H264,
            ResolvedVideoEncoder::X265
            | ResolvedVideoEncoder::HevcNvenc
            | ResolvedVideoEncoder::HevcQsv
            | ResolvedVideoEncoder::HevcVaapi
            | ResolvedVideoEncoder::HevcRkmpp => EncoderFamily::Hevc,
        }
    }

    /// HW family this encoder charges against, or `None` for libx264 /
    /// libx265 (CPU). Mirrors [`HwEncoderFamily::classify`] but typed.
    pub fn hw_family(self) -> Option<HwEncoderFamily> {
        match self {
            ResolvedVideoEncoder::X264 | ResolvedVideoEncoder::X265 => None,
            ResolvedVideoEncoder::H264Nvenc | ResolvedVideoEncoder::HevcNvenc => {
                Some(HwEncoderFamily::Nvenc)
            }
            ResolvedVideoEncoder::H264Qsv | ResolvedVideoEncoder::HevcQsv => {
                Some(HwEncoderFamily::Qsv)
            }
            ResolvedVideoEncoder::H264Vaapi | ResolvedVideoEncoder::HevcVaapi => {
                Some(HwEncoderFamily::Vaapi)
            }
            ResolvedVideoEncoder::H264Rkmpp | ResolvedVideoEncoder::HevcRkmpp => {
                Some(HwEncoderFamily::Rkmpp)
            }
        }
    }

    /// `true` for libx264 / libx265. Used by the cost model to charge
    /// the SW per-output rate (500) instead of the HW rate (100).
    pub fn is_software(self) -> bool {
        matches!(
            self,
            ResolvedVideoEncoder::X264 | ResolvedVideoEncoder::X265
        )
    }

    /// Translate to the `video_codec::VideoEncoderCodec` handle used by
    /// `VideoEncoder::open`. Only callable on a build with the
    /// `media-codecs` feature (every encoder spawn path is gated on
    /// it anyway).
    #[cfg(feature = "media-codecs")]
    pub fn as_video_encoder_codec(self) -> video_codec::VideoEncoderCodec {
        use video_codec::VideoEncoderCodec;
        match self {
            ResolvedVideoEncoder::X264 => VideoEncoderCodec::X264,
            ResolvedVideoEncoder::X265 => VideoEncoderCodec::X265,
            ResolvedVideoEncoder::H264Nvenc => VideoEncoderCodec::H264Nvenc,
            ResolvedVideoEncoder::HevcNvenc => VideoEncoderCodec::HevcNvenc,
            ResolvedVideoEncoder::H264Qsv => VideoEncoderCodec::H264Qsv,
            ResolvedVideoEncoder::HevcQsv => VideoEncoderCodec::HevcQsv,
            ResolvedVideoEncoder::H264Vaapi => VideoEncoderCodec::H264Vaapi,
            ResolvedVideoEncoder::HevcVaapi => VideoEncoderCodec::HevcVaapi,
            ResolvedVideoEncoder::H264Rkmpp => VideoEncoderCodec::H264Rkmpp,
            ResolvedVideoEncoder::HevcRkmpp => VideoEncoderCodec::HevcRkmpp,
        }
    }
}

/// Convenience wrapper that pulls the static capabilities snapshot and
/// runs [`resolve_video_encoder`] against it. Each output's encoder spawn
/// path uses this so they all see the same Auto resolution behaviour.
#[cfg(feature = "media-codecs")]
pub fn resolve_for_video_encode_config(
    cfg: &crate::config::models::VideoEncodeConfig,
) -> Result<ResolvedVideoEncoder, EncoderResolutionError> {
    let caps = static_capabilities();
    resolve_video_encoder(
        &cfg.codec,
        cfg.chroma.as_deref(),
        cfg.bit_depth,
        caps.as_deref(),
    )
}

/// Resolve a `video_encode` config into the **full Auto fall-through
/// chain** of encoder backends to try in order on `avcodec_open2`
/// failure.
///
/// * Explicit codec strings (`x264`, `h264_qsv`, …) → single-element
///   `Vec` (the operator picked a specific backend and a fall-through
///   would silently mis-behave the way they asked us not to).
/// * Auto codec strings (`h264_auto` / `hevc_auto` / bare `auto`) →
///   the priority chain filtered down to backends the host can do per
///   the runtime probe + the chroma/bit-depth matrix. The first entry
///   matches `resolve_for_video_encode_config(cfg)`.
///
/// Why a chain rather than a single resolved backend: on Intel iGPU
/// hosts the matrix correctly says QSV is available (the startup probe
/// opens it cleanly), but a flow opened later — when the GPU is busy /
/// out of MFX sessions / shimmed by a userspace driver update — can
/// still fail at `avcodec_open2`. Today that hard-fails the flow even
/// when VAAPI / libx264 would have worked. Returning the chain lets
/// `ScaledVideoEncoder::lazy_open` retry the next backend instead of
/// dropping the operator off a cliff. See codec-matrix bug C3.
#[cfg(feature = "media-codecs")]
pub fn resolve_video_encoder_chain(
    codec_string: &str,
    chroma: Option<&str>,
    bit_depth: Option<u8>,
    caps: Option<&StaticCapabilities>,
) -> Result<Vec<ResolvedVideoEncoder>, EncoderResolutionError> {
    let chroma_typed = parse_chroma_str(chroma);
    let bd = bit_depth.unwrap_or(8);
    let chroma_label = chroma.unwrap_or("yuv420p").to_string();

    if let Some(family) = parse_auto_family(codec_string) {
        let caps = caps.ok_or(EncoderResolutionError::ProbeUnavailable)?;
        let chain: Vec<ResolvedVideoEncoder> = auto_priority_chain(family, chroma_typed, bd)
            .iter()
            .copied()
            .filter(|&candidate| host_supports_encoder(candidate, chroma_typed, bd, caps))
            .collect();
        if chain.is_empty() {
            return Err(EncoderResolutionError::AutoNoBackend {
                family,
                chroma: chroma_label,
                bit_depth: bd,
            });
        }
        return Ok(chain);
    }

    // Explicit backend — single-element chain. Re-uses the existing
    // single-backend resolver for the feature/driver/chroma checks so
    // the error semantics stay identical.
    let resolved = resolve_video_encoder(codec_string, chroma, bit_depth, caps)?;
    Ok(vec![resolved])
}

/// Edge-side companion to [`resolve_for_video_encode_config`] returning
/// the full Auto fall-through chain. See
/// [`resolve_video_encoder_chain`] for semantics.
#[cfg(feature = "media-codecs")]
pub fn resolve_chain_for_video_encode_config(
    cfg: &crate::config::models::VideoEncodeConfig,
) -> Result<Vec<ResolvedVideoEncoder>, EncoderResolutionError> {
    let caps = static_capabilities();
    resolve_video_encoder_chain(
        &cfg.codec,
        cfg.chroma.as_deref(),
        cfg.bit_depth,
        caps.as_deref(),
    )
}

/// Reason `resolve_video_encoder` couldn't satisfy the request.
/// Surfaced on `video_encoder_unavailable` events with structured
/// `details` so the manager UI can render an actionable message
/// instead of just "encode failed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderResolutionError {
    /// Codec string didn't match any known backend or `*_auto`.
    UnknownCodec(String),
    /// Forced backend isn't compiled into this build.
    FeatureDisabled { backend: &'static str },
    /// Build has the feature but the runtime probe failed (no driver,
    /// no hardware, or `/dev/dri/renderD128` permissions).
    DriverMissing { backend: &'static str },
    /// Explicit backend was selected but the host's runtime probe
    /// reports it can't do the requested chroma + bit-depth combo
    /// (e.g. `h264_nvenc` + 4:2:2 — NVENC has no 4:2:2 path).
    ChromaUnsupported {
        backend: &'static str,
        chroma: String,
        bit_depth: u8,
    },
    /// Auto resolution: nothing on the host satisfies the request.
    AutoNoBackend {
        family: EncoderFamily,
        chroma: String,
        bit_depth: u8,
    },
    /// `static_capabilities()` returned `None` — probe didn't run.
    /// Treated as a startup-ordering bug; should not happen in
    /// production.
    ProbeUnavailable,
}

impl EncoderResolutionError {
    /// Short tag for the `details.error_code` companion field. The
    /// outer event `error_code` stays `video_encoder_unavailable`.
    pub fn as_reason(&self) -> &'static str {
        match self {
            EncoderResolutionError::UnknownCodec(_) => "unknown_codec",
            EncoderResolutionError::FeatureDisabled { .. } => "feature_disabled",
            EncoderResolutionError::DriverMissing { .. } => "driver_missing",
            EncoderResolutionError::ChromaUnsupported { .. } => "chroma_unsupported",
            EncoderResolutionError::AutoNoBackend { .. } => "auto_no_backend",
            EncoderResolutionError::ProbeUnavailable => "probe_unavailable",
        }
    }

    /// Human-readable rendering for log lines + event messages. Includes
    /// remediation hints where applicable so the operator doesn't have to
    /// remember which codec rejects which chroma.
    pub fn message(&self) -> String {
        match self {
            EncoderResolutionError::UnknownCodec(s) => {
                format!("unknown video encoder '{s}' (expected one of: x264, x265, h264_auto, hevc_auto, h264_nvenc, hevc_nvenc, h264_qsv, hevc_qsv, h264_vaapi, hevc_vaapi)")
            }
            EncoderResolutionError::FeatureDisabled { backend } => format!(
                "video encoder '{backend}' is not compiled into this build — rebuild with the matching `video-encoder-*` feature"
            ),
            EncoderResolutionError::DriverMissing { backend } => format!(
                "video encoder '{backend}' is compiled in but the host has no driver / hardware for it (or render-node permissions are missing)"
            ),
            EncoderResolutionError::ChromaUnsupported { backend, chroma, bit_depth } => format!(
                "video encoder '{backend}' cannot encode {chroma} {bit_depth}-bit on this host (NVENC + QSV have no 4:2:2 path; AMD VAAPI typically rejects 4:2:2 — pick libx265 or hevc_vaapi on Intel iHD instead, or use h264_auto / hevc_auto to let the resolver choose)"
            ),
            EncoderResolutionError::AutoNoBackend { family, chroma, bit_depth } => {
                let family = match family {
                    EncoderFamily::H264 => "H.264",
                    EncoderFamily::Hevc => "HEVC",
                };
                format!(
                    "no {family} encoder on this host can produce {chroma} {bit_depth}-bit (rebuild with `video-encoder-x264` / `video-encoder-x265` for the broadcast 4:2:2 fallback)"
                )
            }
            EncoderResolutionError::ProbeUnavailable => {
                "hardware probe snapshot is not yet available — encoder selection cannot proceed".into()
            }
        }
    }
}

/// `chroma`-string parser used by both [`resolve_video_encoder`] and the
/// chroma matrix lookup. Returns `Yuv420` for `None` and unrecognised
/// values, matching the existing `VideoEncodeConfig` default.
#[cfg(feature = "media-codecs")]
fn parse_chroma_str(s: Option<&str>) -> video_codec::VideoChroma {
    use video_codec::VideoChroma;
    match s {
        Some("yuv422p") => VideoChroma::Yuv422,
        Some("yuv444p") => VideoChroma::Yuv444,
        _ => VideoChroma::Yuv420,
    }
}

/// `*_auto` codec-string parser. Returns the requested output family for
/// `h264_auto` / `hevc_auto`, `None` for explicit backend strings.
fn parse_auto_family(codec: &str) -> Option<EncoderFamily> {
    match codec {
        "h264_auto" | "auto_h264" => Some(EncoderFamily::H264),
        "hevc_auto" | "auto_hevc" | "h265_auto" | "auto_h265" => Some(EncoderFamily::Hevc),
        // Bare "auto" defaults to H.264 (more universally consumable).
        // Explicit family selection is preferred and emitted by the
        // manager UI; the bare form keeps short-form ad-hoc CLI configs
        // working without surprising operators.
        "auto" => Some(EncoderFamily::H264),
        _ => None,
    }
}

/// `true` when the host can actually do the requested combination,
/// considering compile-time features AND runtime probe results AND
/// the per-(codec, chroma, bit-depth) chroma matrix.
#[cfg(feature = "media-codecs")]
fn host_supports_encoder(
    encoder: ResolvedVideoEncoder,
    chroma: video_codec::VideoChroma,
    bit_depth: u8,
    caps: &StaticCapabilities,
) -> bool {
    use video_codec::VideoChroma;
    match encoder {
        ResolvedVideoEncoder::X264 => {
            if !cfg!(feature = "video-encoder-x264") {
                return false;
            }
            // libx264 covers 4:2:0/4:2:2/4:4:4 × 8/10 in the vendored
            // bilbycast build (High10 / High422 / High444 profiles all
            // enabled); chroma matrix probes confirm at startup. 4:4:4
            // baseline is always true when libx264 is in.
            match (chroma, bit_depth) {
                (VideoChroma::Yuv420, 8) => true,
                (VideoChroma::Yuv422, 8) => caps.hw_encoder_chroma.libx264_yuv422_8bit,
                (VideoChroma::Yuv420, 10) => caps.hw_encoder_chroma.libx264_yuv420_10bit,
                (VideoChroma::Yuv422, 10) => caps.hw_encoder_chroma.libx264_yuv422_10bit,
                (VideoChroma::Yuv444, _) => true,
                _ => false,
            }
        }
        ResolvedVideoEncoder::X265 => {
            if !cfg!(feature = "video-encoder-x265") {
                return false;
            }
            match (chroma, bit_depth) {
                (VideoChroma::Yuv420, 8) => true,
                (VideoChroma::Yuv422, 8) => caps.hw_encoder_chroma.libx265_yuv422_8bit,
                (VideoChroma::Yuv420, 10) => caps.hw_encoder_chroma.libx265_yuv420_10bit,
                (VideoChroma::Yuv422, 10) => caps.hw_encoder_chroma.libx265_yuv422_10bit,
                (VideoChroma::Yuv444, _) => true,
                _ => false,
            }
        }
        ResolvedVideoEncoder::H264Nvenc => {
            if !cfg!(feature = "video-encoder-nvenc") {
                return false;
            }
            // H.264 NVENC is 4:2:0 8-bit only (no 4:2:2, no Main10).
            caps.hw_encoders.h264_nvenc
                && chroma == VideoChroma::Yuv420
                && bit_depth == 8
        }
        ResolvedVideoEncoder::HevcNvenc => {
            if !cfg!(feature = "video-encoder-nvenc") {
                return false;
            }
            if !caps.hw_encoders.hevc_nvenc {
                return false;
            }
            match (chroma, bit_depth) {
                (VideoChroma::Yuv420, 8) => true,
                (VideoChroma::Yuv420, 10) => caps.hw_encoder_chroma.hevc_nvenc_yuv420_10bit,
                _ => false,
            }
        }
        ResolvedVideoEncoder::H264Qsv => {
            if !cfg!(feature = "video-encoder-qsv") {
                return false;
            }
            caps.hw_encoders.h264_qsv
                && chroma == VideoChroma::Yuv420
                && bit_depth == 8
        }
        ResolvedVideoEncoder::HevcQsv => {
            if !cfg!(feature = "video-encoder-qsv") {
                return false;
            }
            if !caps.hw_encoders.hevc_qsv {
                return false;
            }
            match (chroma, bit_depth) {
                (VideoChroma::Yuv420, 8) => true,
                (VideoChroma::Yuv420, 10) => caps.hw_encoder_chroma.hevc_qsv_yuv420_10bit,
                (VideoChroma::Yuv422, 8) => caps.hw_encoder_chroma.hevc_qsv_yuv422_8bit,
                (VideoChroma::Yuv422, 10) => caps.hw_encoder_chroma.hevc_qsv_yuv422_10bit,
                _ => false,
            }
        }
        ResolvedVideoEncoder::H264Vaapi => {
            if !cfg!(feature = "video-encoder-vaapi") {
                return false;
            }
            // h264_vaapi is 4:2:0 8-bit only on every implementation
            // (no Main10 / High422 in any libva driver).
            caps.hw_encoders.h264_vaapi
                && chroma == VideoChroma::Yuv420
                && bit_depth == 8
        }
        ResolvedVideoEncoder::HevcVaapi => {
            if !cfg!(feature = "video-encoder-vaapi") {
                return false;
            }
            if !caps.hw_encoders.hevc_vaapi {
                return false;
            }
            match (chroma, bit_depth) {
                (VideoChroma::Yuv420, 8) => true,
                (VideoChroma::Yuv420, 10) => caps.hw_encoder_chroma.hevc_vaapi_yuv420_10bit,
                (VideoChroma::Yuv422, 8) => caps.hw_encoder_chroma.hevc_vaapi_yuv422_8bit,
                (VideoChroma::Yuv422, 10) => caps.hw_encoder_chroma.hevc_vaapi_yuv422_10bit,
                _ => false, // 4:4:4 (NV24 packer) deferred
            }
        }
        ResolvedVideoEncoder::H264Rkmpp => {
            if !cfg!(feature = "video-encoder-rkmpp") {
                return false;
            }
            // Rockchip VEPU is 8-bit 4:2:0 only (no 4:2:2, no 4:4:4, no
            // 10-bit) — this is what keeps a 10-bit / 4:2:2 request from
            // ever resolving to RKMPP on the auto chain.
            caps.hw_encoders.h264_rkmpp
                && chroma == VideoChroma::Yuv420
                && bit_depth == 8
        }
        ResolvedVideoEncoder::HevcRkmpp => {
            if !cfg!(feature = "video-encoder-rkmpp") {
                return false;
            }
            // hevc_rkmpp is 8-bit 4:2:0 only as well — the RK3588 VEPU has
            // no 10-bit encode path (10-bit is decode-only).
            caps.hw_encoders.hevc_rkmpp
                && chroma == VideoChroma::Yuv420
                && bit_depth == 8
        }
    }
}

/// Auto-resolution priority chain per `(family, chroma, bit_depth)`.
/// Heads of each list are the cheapest hosts that handle the request;
/// tails are the safe SW fallback.
#[cfg(feature = "media-codecs")]
fn auto_priority_chain(
    family: EncoderFamily,
    chroma: video_codec::VideoChroma,
    bit_depth: u8,
) -> &'static [ResolvedVideoEncoder] {
    use video_codec::VideoChroma;
    use ResolvedVideoEncoder::*;
    match (family, chroma, bit_depth) {
        // ── H.264 ──
        // 4:2:0 8-bit (the dominant distribution path): NVENC ≻ QSV ≻
        // VAAPI ≻ libx264. NVENC has the lowest CPU cost; QSV is a
        // close second on Intel iGPU; VAAPI is the AMD/iHD path; x264
        // is the fallback that always works.
        (EncoderFamily::H264, VideoChroma::Yuv420, 8) => {
            // RKMPP leads on aarch64 Rockchip boards (it's the only HW
            // encoder there); on every other host `host_supports_encoder`
            // filters it out (feature off or probe false) and the resolver
            // falls straight through to NVENC/QSV/VAAPI/x264 as before.
            &[H264Rkmpp, H264Nvenc, H264Qsv, H264Vaapi, X264]
        }
        // H.264 + anything else → libx264 (no HW H.264 encoder
        // supports 4:2:2, 10-bit, or 4:4:4).
        (EncoderFamily::H264, _, _) => &[X264],

        // ── HEVC ──
        // 4:2:0 8-bit: NVENC ≻ QSV ≻ VAAPI ≻ libx265. Same shape as
        // H.264 baseline distribution.
        (EncoderFamily::Hevc, VideoChroma::Yuv420, 8) => {
            // RKMPP leads on Rockchip; filtered out elsewhere (see the
            // H.264 4:2:0/8 arm above).
            &[HevcRkmpp, HevcNvenc, HevcQsv, HevcVaapi, X265]
        }
        // 4:2:0 10-bit: NVENC ≻ VAAPI ≻ QSV ≻ libx265. NVENC Main10
        // is mature on Pascal+; VAAPI Main10 works on Intel iHD
        // Skylake+ and AMD VCN RDNA2+; QSV Main10 covers Kaby Lake+.
        // QSV demoted to last HW because broadcast 10-bit deployments
        // tend to be on AMD or NVIDIA in our user base.
        (EncoderFamily::Hevc, VideoChroma::Yuv420, 10) => {
            &[HevcNvenc, HevcVaapi, HevcQsv, X265]
        }
        // 4:2:2 (8 + 10-bit): VAAPI on Intel iHD ≻ libx265. NVENC and
        // QSV are silently skipped because they reject 4:2:2 at
        // `avcodec_open2` — putting them in the chain would just
        // waste the operator's flow-startup time.
        (EncoderFamily::Hevc, VideoChroma::Yuv422, _) => {
            &[HevcVaapi, X265]
        }
        // HEVC + 4:4:4: libx265 only (HW backends reject NV24 today).
        (EncoderFamily::Hevc, VideoChroma::Yuv444, _) => &[X265],

        // Catch-all: libx264 always works for H.264, libx265 for HEVC.
        // (The exhaustive matches above mean this arm is technically
        // unreachable, but match-coverage keeps Clippy happy.)
        #[allow(unreachable_patterns)]
        (EncoderFamily::H264, _, _) => &[X264],
        #[allow(unreachable_patterns)]
        (EncoderFamily::Hevc, _, _) => &[X265],
    }
}

/// Resolve a `video_encode.codec` string into a concrete encoder backend,
/// considering the host's probed capabilities and the requested chroma +
/// bit-depth.
///
/// Three classes of input:
/// * `"h264_auto"` / `"hevc_auto"` (and `"auto"` as a shorthand) — Auto
///   resolution. Walks the per-(family, chroma, bit-depth) priority
///   chain and returns the first backend the host can actually do.
/// * Explicit backend names (`"x264"`, `"h264_nvenc"`, …) — Validated
///   end-to-end. Returns `FeatureDisabled` / `DriverMissing` /
///   `ChromaUnsupported` so the manager UI can render an actionable
///   error before the flow even starts.
/// * Anything else — `UnknownCodec`.
///
/// Pure function over `(codec, chroma, bit_depth, capabilities)`. The
/// flow start path uses the resolved `ffmpeg_name()` to drive the existing
/// `parse_codec` / `resolve_backend` helpers in `ts_video_replace.rs` /
/// `output_rtmp.rs`.
#[cfg(feature = "media-codecs")]
pub fn resolve_video_encoder(
    codec_string: &str,
    chroma: Option<&str>,
    bit_depth: Option<u8>,
    caps: Option<&StaticCapabilities>,
) -> Result<ResolvedVideoEncoder, EncoderResolutionError> {
    let chroma_typed = parse_chroma_str(chroma);
    let bd = bit_depth.unwrap_or(8);
    let chroma_label = chroma.unwrap_or("yuv420p").to_string();

    // Auto family path
    if let Some(family) = parse_auto_family(codec_string) {
        let caps = caps.ok_or(EncoderResolutionError::ProbeUnavailable)?;
        for &candidate in auto_priority_chain(family, chroma_typed, bd) {
            if host_supports_encoder(candidate, chroma_typed, bd, caps) {
                return Ok(candidate);
            }
        }
        return Err(EncoderResolutionError::AutoNoBackend {
            family,
            chroma: chroma_label,
            bit_depth: bd,
        });
    }

    // Explicit backend
    let resolved = match codec_string {
        "x264" => ResolvedVideoEncoder::X264,
        "x265" => ResolvedVideoEncoder::X265,
        "h264_nvenc" => ResolvedVideoEncoder::H264Nvenc,
        "hevc_nvenc" => ResolvedVideoEncoder::HevcNvenc,
        "h264_qsv" => ResolvedVideoEncoder::H264Qsv,
        "hevc_qsv" => ResolvedVideoEncoder::HevcQsv,
        "h264_vaapi" => ResolvedVideoEncoder::H264Vaapi,
        "hevc_vaapi" => ResolvedVideoEncoder::HevcVaapi,
        "h264_rkmpp" => ResolvedVideoEncoder::H264Rkmpp,
        "hevc_rkmpp" => ResolvedVideoEncoder::HevcRkmpp,
        other => return Err(EncoderResolutionError::UnknownCodec(other.to_string())),
    };

    // Compile-time feature gate
    let feature_ok = match resolved {
        ResolvedVideoEncoder::X264 => cfg!(feature = "video-encoder-x264"),
        ResolvedVideoEncoder::X265 => cfg!(feature = "video-encoder-x265"),
        ResolvedVideoEncoder::H264Nvenc | ResolvedVideoEncoder::HevcNvenc => {
            cfg!(feature = "video-encoder-nvenc")
        }
        ResolvedVideoEncoder::H264Qsv | ResolvedVideoEncoder::HevcQsv => {
            cfg!(feature = "video-encoder-qsv")
        }
        ResolvedVideoEncoder::H264Vaapi | ResolvedVideoEncoder::HevcVaapi => {
            cfg!(feature = "video-encoder-vaapi")
        }
        ResolvedVideoEncoder::H264Rkmpp | ResolvedVideoEncoder::HevcRkmpp => {
            cfg!(feature = "video-encoder-rkmpp")
        }
    };
    if !feature_ok {
        return Err(EncoderResolutionError::FeatureDisabled {
            backend: resolved.ffmpeg_name(),
        });
    }

    // Runtime baseline check (does the host actually have the driver / GPU?)
    let caps = caps.ok_or(EncoderResolutionError::ProbeUnavailable)?;
    let baseline_available = match resolved {
        ResolvedVideoEncoder::X264 | ResolvedVideoEncoder::X265 => true, // SW always
        ResolvedVideoEncoder::H264Nvenc => caps.hw_encoders.h264_nvenc,
        ResolvedVideoEncoder::HevcNvenc => caps.hw_encoders.hevc_nvenc,
        ResolvedVideoEncoder::H264Qsv => caps.hw_encoders.h264_qsv,
        ResolvedVideoEncoder::HevcQsv => caps.hw_encoders.hevc_qsv,
        ResolvedVideoEncoder::H264Vaapi => caps.hw_encoders.h264_vaapi,
        ResolvedVideoEncoder::HevcVaapi => caps.hw_encoders.hevc_vaapi,
        ResolvedVideoEncoder::H264Rkmpp => caps.hw_encoders.h264_rkmpp,
        ResolvedVideoEncoder::HevcRkmpp => caps.hw_encoders.hevc_rkmpp,
    };
    if !baseline_available {
        return Err(EncoderResolutionError::DriverMissing {
            backend: resolved.ffmpeg_name(),
        });
    }

    // Chroma + bit-depth matrix check
    if !host_supports_encoder(resolved, chroma_typed, bd, caps) {
        return Err(EncoderResolutionError::ChromaUnsupported {
            backend: resolved.ffmpeg_name(),
            chroma: chroma_label,
            bit_depth: bd,
        });
    }

    Ok(resolved)
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
        // enable `video-decoder-nvdec` or `video-decoder-qsv` by
        // default — the feature gates short-circuit before we even
        // look at caps.
        if !cfg!(feature = "video-decoder-nvdec") {
            assert_eq!(
                resolve_display_decoder(&HwDecodePreference::Nvdec, None),
                Err(DecoderResolutionError::FeatureDisabled),
            );
        }
        if !cfg!(feature = "video-decoder-qsv") {
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
        // Pre-weighted: 1080p30 4:2:0 8-bit SW = 500 units (the
        // baseline for the SW per-output anchor).
        let plan = FlowCostPlan {
            sw_video_encode_units: 500,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&plan), 1 + 500);
    }

    #[test]
    fn cost_hw_cheaper_than_sw() {
        let hw = FlowCostPlan {
            hw_video_encode_units: 100,
            ..Default::default()
        };
        let sw = FlowCostPlan {
            sw_video_encode_units: 500,
            ..Default::default()
        };
        assert!(compute_flow_cost_units(&hw) < compute_flow_cost_units(&sw));
    }

    #[test]
    fn weighted_units_scale_with_resolution_and_fps() {
        // 1080p30 4:2:0 8-bit baseline for SW.
        let baseline = weighted_video_encode_units(false, None, None, None, None, None, None);
        assert_eq!(baseline, 500);
        // 4K30 4:2:0 8-bit ≈ 4× baseline (3840 × 2160 = 4× pixels of 1920×1080).
        let four_k = weighted_video_encode_units(
            false, Some(3840), Some(2160), Some(30), Some(1), None, None,
        );
        assert_eq!(four_k, 2000);
        // 1080p30 4:2:0 10-bit ≈ 1.5× baseline.
        let ten_bit = weighted_video_encode_units(
            false, Some(1920), Some(1080), Some(30), Some(1), Some(10), Some("yuv420p"),
        );
        assert_eq!(ten_bit, 750);
        // 1080p30 4:2:2 8-bit ≈ 1.33× baseline.
        let four_two_two = weighted_video_encode_units(
            false, Some(1920), Some(1080), Some(30), Some(1), Some(8), Some("yuv422p"),
        );
        assert_eq!(four_two_two, 665);
        // 4K60 HEVC 4:2:2 10-bit on libx265 — broadcast contribution
        // worst case. Scale = 8 (4× pixels × 2× fps) × 1.5 (10-bit) ×
        // 1.33 (4:2:2) = 15.96, so ~7980 units. The flow's cost goes
        // from "fits in a 4-core box" (1800 units) to "needs an 8+
        // core box" (7400 units on 32-core), which is the right
        // signal — matches what operators eyeball as a "this can't run
        // on the smallest edge" config.
        let four_k_60_422_10 = weighted_video_encode_units(
            false, Some(3840), Some(2160), Some(60), Some(1), Some(10), Some("yuv422p"),
        );
        assert!(four_k_60_422_10 > 7000 && four_k_60_422_10 < 9000);
        // HW NVENC 1080p30 4:2:0 8-bit baseline = 100.
        let hw_baseline =
            weighted_video_encode_units(true, None, None, None, None, None, None);
        assert_eq!(hw_baseline, 100);
        // Floor: 240p test pattern shouldn't drop below the per-output
        // baseline (a 100-unit HW output).
        let tiny = weighted_video_encode_units(
            true, Some(320), Some(240), Some(15), Some(1), None, None,
        );
        assert_eq!(tiny, 100);
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
    fn cost_thumbnail_per_decoder_scales_with_cadence() {
        // round(3 × 5 / interval), floored at 1. None = the 5 s default.
        assert_eq!(thumbnail_units_per_decoder(None), 3);
        assert_eq!(thumbnail_units_per_decoder(Some(5)), 3);
        assert_eq!(thumbnail_units_per_decoder(Some(1)), 15);
        assert_eq!(thumbnail_units_per_decoder(Some(2)), 8);
        assert_eq!(thumbnail_units_per_decoder(Some(10)), 2);
        assert_eq!(thumbnail_units_per_decoder(Some(30)), 1);
        // Out-of-range values clamp rather than divide-by-zero / overflow.
        assert_eq!(thumbnail_units_per_decoder(Some(0)), 15); // clamps to 1 s
        assert_eq!(thumbnail_units_per_decoder(Some(1000)), 1); // clamps to 60 s
    }

    #[test]
    fn cost_thumbnail_units_add_to_total() {
        // A precomputed thumbnail cost (e.g. 2 generators × 3 units @ 5 s)
        // lands on the flow total on top of the base passthrough cost.
        let plan = FlowCostPlan {
            thumbnail_units: 6,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&plan), 1 + 6);
    }

    #[test]
    fn cost_combo_realistic() {
        // Realistic transcode flow: SW H.264 + AAC encode + lite analysis + recording.
        let plan = FlowCostPlan {
            sw_video_encode_units: 500,
            audio_encode_outputs: 1,
            content_analysis_lite: true,
            recording_enabled: true,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&plan), 1 + 500 + 5 + 2 + 5);
    }

    #[test]
    fn cost_audio_encode_inputs_folds_with_outputs() {
        // Audio encoders on inputs charge the same per-encoder unit cost
        // as audio encoders on outputs — both fold into the same pool.
        let inputs_only = FlowCostPlan {
            audio_encode_inputs: 2,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&inputs_only), 1 + 10);

        let mixed = FlowCostPlan {
            audio_encode_inputs: 1,
            audio_encode_outputs: 3,
            ..Default::default()
        };
        assert_eq!(compute_flow_cost_units(&mixed), 1 + 20);
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
        assert_eq!(
            HwEncoderFamily::classify("h264_vaapi"),
            Some(HwEncoderFamily::Vaapi)
        );
        assert_eq!(
            HwEncoderFamily::classify("HEVC_VAAPI"),
            Some(HwEncoderFamily::Vaapi)
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

    // ── Video-encoder resolution tests ─────────────────────────────

    #[cfg(feature = "media-codecs")]
    fn make_caps(hw_encoders: HwCodecCapability) -> StaticCapabilities {
        StaticCapabilities {
            hw_encoders,
            hw_decoders: HwCodecCapability::default(),
            hw_encoder_session_limits: HwSessionLimits::default(),
            hw_decoder_session_limits: HwDecoderSessionLimits::default(),
            hw_encoder_chroma: HwEncoderChromaCapability {
                // Vendored bilbycast FFmpeg has the High10 / High422 /
                // Main10 / Main422 profiles compiled in, so the SW
                // encoders advertise the broadcast 4:2:2 / 10-bit
                // matrix as supported.
                libx264_yuv422_8bit: true,
                libx264_yuv420_10bit: true,
                libx264_yuv422_10bit: true,
                libx265_yuv422_8bit: true,
                libx265_yuv420_10bit: true,
                libx265_yuv422_10bit: true,
                ..Default::default()
            },
            vaapi: VaapiCapability::default(),
            cpu: CpuInfo {
                brand: "Test CPU".into(),
                physical_cores: 4,
                logical_cores: 8,
                avx_class: AvxClass::Avx2,
            },
            sw_capacity: SwCapacityEstimate::default(),
            wire_pacing_txtime: false,
        }
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn auto_h264_4_2_0_8bit_picks_nvenc_when_available() {
        // NVIDIA host + h264_auto + 4:2:0 8-bit → h264_nvenc (top of chain).
        let caps = make_caps(HwCodecCapability {
            h264_nvenc: true,
            hevc_nvenc: true,
            ..Default::default()
        });
        let resolved =
            resolve_video_encoder("h264_auto", Some("yuv420p"), Some(8), Some(&caps));
        if cfg!(feature = "video-encoder-nvenc") {
            assert_eq!(resolved.unwrap(), ResolvedVideoEncoder::H264Nvenc);
        } else {
            // No NVENC compiled — Auto falls through to whichever SW
            // encoder is in the build, or returns AutoNoBackend on a
            // build with neither x264 nor x265 (rare).
            match resolved {
                Ok(ResolvedVideoEncoder::X264) => {}
                Err(EncoderResolutionError::AutoNoBackend { .. }) => {}
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn auto_hevc_4_2_2_10bit_skips_nvenc_qsv_picks_vaapi_or_x265() {
        // Intel iHD host + hevc_auto + 4:2:2 10-bit → hevc_vaapi (the
        // only HW backend that does 4:2:2). NVENC + QSV stay in the
        // chain but `host_supports_encoder` returns false for them.
        let caps = make_caps(HwCodecCapability {
            h264_nvenc: true, // present but irrelevant for 4:2:2
            hevc_nvenc: true,
            h264_qsv: true,
            hevc_qsv: true,
            h264_vaapi: true,
            hevc_vaapi: true,
            ..Default::default()
        });
        // Mark VAAPI 4:2:2 10-bit as supported (Intel iHD Tiger Lake+).
        let mut caps = caps;
        caps.hw_encoder_chroma.hevc_vaapi_yuv422_10bit = true;
        let resolved =
            resolve_video_encoder("hevc_auto", Some("yuv422p"), Some(10), Some(&caps));
        if cfg!(feature = "video-encoder-vaapi") {
            assert_eq!(resolved.unwrap(), ResolvedVideoEncoder::HevcVaapi);
        } else if cfg!(feature = "video-encoder-x265") {
            assert_eq!(resolved.unwrap(), ResolvedVideoEncoder::X265);
        }
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn auto_falls_through_to_software_when_no_hw() {
        // Software-only host. Auto should land on libx264 / libx265.
        let caps = make_caps(HwCodecCapability::default());
        let resolved =
            resolve_video_encoder("h264_auto", Some("yuv420p"), Some(8), Some(&caps));
        match resolved {
            Ok(ResolvedVideoEncoder::X264) => {
                assert!(cfg!(feature = "video-encoder-x264"));
            }
            Err(EncoderResolutionError::AutoNoBackend { .. }) => {
                assert!(!cfg!(feature = "video-encoder-x264"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn explicit_h264_nvenc_with_4_2_2_returns_chroma_unsupported() {
        let caps = make_caps(HwCodecCapability {
            h264_nvenc: true,
            ..Default::default()
        });
        if !cfg!(feature = "video-encoder-nvenc") {
            return; // build doesn't have NVENC — handled by FeatureDisabled path elsewhere
        }
        let result =
            resolve_video_encoder("h264_nvenc", Some("yuv422p"), Some(8), Some(&caps));
        match result {
            Err(EncoderResolutionError::ChromaUnsupported {
                backend, chroma, bit_depth,
            }) => {
                assert_eq!(backend, "h264_nvenc");
                assert_eq!(chroma, "yuv422p");
                assert_eq!(bit_depth, 8);
            }
            other => panic!("expected ChromaUnsupported, got {other:?}"),
        }
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn explicit_unknown_codec_returns_unknown_codec_error() {
        let result =
            resolve_video_encoder("totally_made_up", Some("yuv420p"), Some(8), None);
        match result {
            Err(EncoderResolutionError::UnknownCodec(s)) => {
                assert_eq!(s, "totally_made_up");
            }
            other => panic!("expected UnknownCodec, got {other:?}"),
        }
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn auto_without_caps_returns_probe_unavailable() {
        let result = resolve_video_encoder("h264_auto", None, None, None);
        assert_eq!(result, Err(EncoderResolutionError::ProbeUnavailable));
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn resolved_video_encoder_metadata() {
        assert_eq!(ResolvedVideoEncoder::X264.ffmpeg_name(), "x264");
        assert_eq!(ResolvedVideoEncoder::H264Nvenc.ffmpeg_name(), "h264_nvenc");
        assert_eq!(ResolvedVideoEncoder::HevcVaapi.ffmpeg_name(), "hevc_vaapi");
        assert_eq!(ResolvedVideoEncoder::X264.family(), EncoderFamily::H264);
        assert_eq!(ResolvedVideoEncoder::HevcQsv.family(), EncoderFamily::Hevc);
        assert!(ResolvedVideoEncoder::X264.is_software());
        assert!(!ResolvedVideoEncoder::H264Nvenc.is_software());
        assert_eq!(
            ResolvedVideoEncoder::H264Nvenc.hw_family(),
            Some(HwEncoderFamily::Nvenc)
        );
        assert_eq!(ResolvedVideoEncoder::X264.hw_family(), None);
    }

    #[cfg(feature = "media-codecs")]
    #[test]
    fn encoder_resolution_error_reasons_are_stable() {
        assert_eq!(
            EncoderResolutionError::UnknownCodec("x".into()).as_reason(),
            "unknown_codec"
        );
        assert_eq!(
            EncoderResolutionError::FeatureDisabled { backend: "h264_nvenc" }
                .as_reason(),
            "feature_disabled"
        );
        assert_eq!(
            EncoderResolutionError::DriverMissing { backend: "h264_nvenc" }
                .as_reason(),
            "driver_missing"
        );
        assert_eq!(
            EncoderResolutionError::ChromaUnsupported {
                backend: "h264_nvenc",
                chroma: "yuv422p".into(),
                bit_depth: 8,
            }
            .as_reason(),
            "chroma_unsupported"
        );
        assert_eq!(
            EncoderResolutionError::AutoNoBackend {
                family: EncoderFamily::H264,
                chroma: "yuv422p".into(),
                bit_depth: 10,
            }
            .as_reason(),
            "auto_no_backend"
        );
        assert_eq!(
            EncoderResolutionError::ProbeUnavailable.as_reason(),
            "probe_unavailable"
        );
    }
}
