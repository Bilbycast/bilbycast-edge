// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared resolution logic for building a `video_codec::VideoEncoderConfig`
//! from the edge-side [`VideoEncodeConfig`], plus the shared
//! [`ScaledVideoEncoder`] pipeline that every decode→encode call site uses.
//!
//! Every call site that opens a [`video_engine::VideoEncoder`] — RTMP,
//! WebRTC, TS video replacer, CMAF, ST 2110-20/-23 — goes through
//! [`build_encoder_config`]. Centralising the mapping keeps the five
//! backend wiring paths in lock-step as new knobs (chroma, bit depth,
//! rate control, colour metadata, …) are added to the config schema.
//!
//! [`ScaledVideoEncoder`] wraps a lazily-opened [`video_engine::VideoEncoder`]
//! together with an optional [`video_engine::VideoScaler`] so every call
//! site gets resolution scaling for free when the operator asks for a
//! `video_encode.width` / `.height` that differs from the source. Without
//! it, the encoder is opened at the requested size but fed source-resolution
//! planes — which libavcodec crops to the top-left quadrant rather than
//! scaling. (See `docs/transcoding.md`.)

use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::models::VideoEncodeConfig;
use video_codec::{
    VideoChroma, VideoEncoderCodec, VideoEncoderConfig, VideoPreset, VideoProfile,
    VideoRateControl,
};

/// Lock-free cell that publishes the backend a [`ScaledVideoEncoder`]
/// actually opened with after lazy-open. `0` = unset (encoder not yet
/// opened); `1..=10` map to the ten [`VideoEncoderCodec`] variants.
/// Snapshot path maps the value back to the operator-facing label so
/// the manager-UI badge tracks the resolved backend after Auto-chain
/// demotion (e.g., NVENC → x264 fallback).
#[derive(Default, Debug)]
pub struct ResolvedBackendCell(AtomicU8);

impl ResolvedBackendCell {
    pub fn store(&self, codec: VideoEncoderCodec) {
        self.0.store(codec_to_u8(codec), Ordering::Relaxed);
    }

    /// Returns `Some(label)` once the encoder has lazy-opened, where
    /// `label` is the same family-collapsed tag the call sites compute
    /// at output start (`"x264"`, `"x265"`, `"nvenc"`, `"qsv"`,
    /// `"vaapi"`). `None` means the encoder hasn't opened yet, so the
    /// caller should keep using the requested-codec label.
    pub fn label(&self) -> Option<&'static str> {
        u8_to_codec(self.0.load(Ordering::Relaxed)).map(backend_label)
    }
}

fn codec_to_u8(c: VideoEncoderCodec) -> u8 {
    match c {
        VideoEncoderCodec::X264 => 1,
        VideoEncoderCodec::X265 => 2,
        VideoEncoderCodec::H264Nvenc => 3,
        VideoEncoderCodec::HevcNvenc => 4,
        VideoEncoderCodec::H264Qsv => 5,
        VideoEncoderCodec::HevcQsv => 6,
        VideoEncoderCodec::H264Vaapi => 7,
        VideoEncoderCodec::HevcVaapi => 8,
        VideoEncoderCodec::H264Rkmpp => 9,
        VideoEncoderCodec::HevcRkmpp => 10,
    }
}

fn u8_to_codec(v: u8) -> Option<VideoEncoderCodec> {
    match v {
        1 => Some(VideoEncoderCodec::X264),
        2 => Some(VideoEncoderCodec::X265),
        3 => Some(VideoEncoderCodec::H264Nvenc),
        4 => Some(VideoEncoderCodec::HevcNvenc),
        5 => Some(VideoEncoderCodec::H264Qsv),
        6 => Some(VideoEncoderCodec::HevcQsv),
        7 => Some(VideoEncoderCodec::H264Vaapi),
        8 => Some(VideoEncoderCodec::HevcVaapi),
        9 => Some(VideoEncoderCodec::H264Rkmpp),
        10 => Some(VideoEncoderCodec::HevcRkmpp),
        _ => None,
    }
}

fn backend_label(c: VideoEncoderCodec) -> &'static str {
    match c {
        VideoEncoderCodec::X264 => "x264",
        VideoEncoderCodec::X265 => "x265",
        VideoEncoderCodec::H264Nvenc | VideoEncoderCodec::HevcNvenc => "nvenc",
        VideoEncoderCodec::H264Qsv | VideoEncoderCodec::HevcQsv => "qsv",
        VideoEncoderCodec::H264Vaapi | VideoEncoderCodec::HevcVaapi => "vaapi",
        VideoEncoderCodec::H264Rkmpp | VideoEncoderCodec::HevcRkmpp => "rkmpp",
    }
}

/// libx264 / libx265 tune vocabulary.
const SW_TUNES: &[&str] = &[
    "zerolatency",
    "film",
    "animation",
    "grain",
    "stillimage",
    "fastdecode",
    "psnr",
    "ssim",
];
/// NVENC tune vocabulary. Disjoint from [`SW_TUNES`].
const NVENC_TUNES: &[&str] = &["hq", "ll", "ull", "lossless"];

/// Default `tune` when the operator did not set one.
///
/// `zerolatency` is an **x264/x265-only** tune. NVENC exposes a `tune` option
/// too, but its vocabulary is `hq` / `ll` / `ull` / `lossless`; handing it
/// `zerolatency` makes `avcodec_open2` fail with `EINVAL (-22)`. Defaulting it
/// unconditionally therefore broke *every* NVENC user who did not explicitly
/// set `tune: ""`.
///
/// Empty means "don't pass `tune` to the encoder at all" (see
/// `video_engine::VideoEncoder::open`), which is the right default for the
/// hardware backends: they are already low-latency by construction.
fn default_tune_for(backend: VideoEncoderCodec) -> String {
    match backend {
        VideoEncoderCodec::X264 | VideoEncoderCodec::X265 => "zerolatency".to_string(),
        _ => String::new(),
    }
}

/// Drop a `tune` the resolved backend cannot accept, rather than letting
/// `avcodec_open2` fail with an opaque `EINVAL (-22)`.
///
/// Config validation is permissive over the union of the vocabularies because
/// `h264_auto` / `hevc_auto` resolve their backend per-host at flow start: a
/// tune legal for the x264 one host picks is illegal for the NVENC another host
/// picks, and neither is knowable at config load. This is the point where the
/// backend *is* known, so it is the only place the check can be correct.
///
/// QSV and VAAPI expose no `tune` option at all — libavcodec ignores unknown
/// dictionary entries, so passing one is harmless, but dropping it keeps the
/// encoder's option dictionary honest.
fn sanitise_tune(backend: VideoEncoderCodec, tune: String) -> String {
    if tune.is_empty() {
        return tune;
    }
    let acceptable: &[&str] = match backend {
        VideoEncoderCodec::X264 | VideoEncoderCodec::X265 => SW_TUNES,
        VideoEncoderCodec::H264Nvenc | VideoEncoderCodec::HevcNvenc => NVENC_TUNES,
        // QSV / VAAPI: no `tune` option exists.
        _ => &[],
    };
    if acceptable.contains(&tune.as_str()) {
        return tune;
    }
    let accepts = if acceptable.is_empty() {
        "no tune option".to_string()
    } else {
        acceptable.join(", ")
    };
    tracing::warn!(
        error_code = "encoder_tune_not_supported",
        "video_encode.tune '{tune}' is not supported by the {} backend ({accepts}); \
         ignoring it — the encoder would otherwise fail to open with EINVAL",
        backend_label(backend),
    );
    String::new()
}

/// Map a preset the resolved backend cannot accept onto its nearest
/// equivalent, rather than letting `avcodec_open2` fail with `EINVAL (-22)`.
///
/// `tune`'s sibling, with one difference: an unsupported tune is safely
/// *dropped* (the encoder picks its default), but a preset carries the
/// operator's speed/quality intent, so it is *mapped* — an `ultrafast` ask on
/// NVENC becomes `fast`, not silence.
///
/// Vocabularies, per the vendored FFmpeg n7.1.3:
/// * libx264 / libx265 accept the full nine-name ladder.
/// * NVENC's named presets are `slow` / `medium` / `fast` (plus `p1`–`p7` and
///   legacy names the edge never emits). `ultrafast` et al. ⇒ EINVAL at open.
/// * QSV accepts `veryfast` … `veryslow` — everything except `ultrafast` /
///   `superfast`.
/// * VAAPI exposes no `preset` option; libavcodec ignores the unknown dict
///   entry, so anything is safe there.
///
/// Lives post-resolution for the same reason as [`sanitise_tune`]: with
/// `h264_auto` / `hevc_auto` the backend — and hence the legal vocabulary —
/// is only known here.
fn sanitise_preset(backend: VideoEncoderCodec, preset: VideoPreset) -> VideoPreset {
    use VideoPreset::*;
    let mapped = match backend {
        VideoEncoderCodec::H264Nvenc | VideoEncoderCodec::HevcNvenc => match preset {
            Ultrafast | Superfast | Veryfast | Faster => Fast,
            Slower | Veryslow => Slow,
            ok => return ok,
        },
        VideoEncoderCodec::H264Qsv | VideoEncoderCodec::HevcQsv => match preset {
            Ultrafast | Superfast => Veryfast,
            ok => return ok,
        },
        // x264 / x265 accept the full ladder; VAAPI ignores the option.
        _ => return preset,
    };
    tracing::warn!(
        error_code = "encoder_preset_not_supported",
        "video_encode.preset '{}' is not supported by the {} backend; \
         using '{}' instead — the encoder would otherwise fail to open with EINVAL",
        preset.as_str(),
        backend_label(backend),
        mapped.as_str(),
    );
    mapped
}

/// Build a [`VideoEncoderConfig`] from the edge-side [`VideoEncodeConfig`],
/// runtime-derived source dimensions, and the backend selected by the
/// caller. `global_header` depends on the container — RTMP needs
/// out-of-band SPS/PPS (`true`), WebRTC and MPEG-TS emit SPS/PPS in-band
/// (`false`).
///
/// Callers are expected to have already validated the config via
/// [`crate::config::validation::validate_video_encode`], so unknown
/// strings silently fall through to encoder defaults here rather than
/// returning `Err`.
pub fn build_encoder_config(
    cfg: &VideoEncodeConfig,
    backend: VideoEncoderCodec,
    src_w: u32,
    src_h: u32,
    src_fps_num: u32,
    src_fps_den: u32,
    global_header: bool,
) -> VideoEncoderConfig {
    let width = cfg.width.unwrap_or(src_w);
    let height = cfg.height.unwrap_or(src_h);
    let fps_num = cfg.fps_num.unwrap_or(src_fps_num.max(1));
    let fps_den = cfg.fps_den.unwrap_or(src_fps_den.max(1));
    let bitrate_kbps = cfg.bitrate_kbps.unwrap_or(8_000);
    let gop_size = cfg
        .gop_size
        .unwrap_or_else(|| 2 * (fps_num / fps_den.max(1)).max(1));

    VideoEncoderConfig {
        codec: backend,
        width,
        height,
        fps_num,
        fps_den,
        bitrate_kbps,
        max_bitrate_kbps: cfg.max_bitrate_kbps.unwrap_or(0),
        gop_size,
        preset: sanitise_preset(backend, resolve_preset(cfg.preset.as_deref())),
        profile: resolve_profile(cfg.profile.as_deref()),
        chroma: resolve_chroma(cfg.chroma.as_deref()),
        bit_depth: cfg.bit_depth.unwrap_or(8),
        rate_control: resolve_rate_control(cfg.rate_control.as_deref()),
        crf: cfg.crf.unwrap_or(23),
        max_b_frames: cfg.bframes.unwrap_or(0),
        refs: cfg.refs.unwrap_or(0),
        // Default 0/0 = pts is a frame counter in 1/fps ticks (the transcode
        // outputs' contract). 90 kHz ingest paths override via
        // `ScaledVideoEncoder::set_pts_90k` at lazy-open.
        time_base_num: 0,
        time_base_den: 0,
        tune: sanitise_tune(
            backend,
            cfg.tune.clone().unwrap_or_else(|| default_tune_for(backend)),
        ),
        level: cfg.level.clone().unwrap_or_default(),
        color_primaries: cfg.color_primaries.clone().unwrap_or_default(),
        color_transfer: cfg.color_transfer.clone().unwrap_or_default(),
        color_matrix: cfg.color_matrix.clone().unwrap_or_default(),
        color_range: cfg.color_range.clone().unwrap_or_default(),
        global_header,
        // Synchronous one-frame-in/one-frame-out by default — live
        // transcode outputs keep their one-frame latency. Throughput-
        // critical ingest call sites (ST 2110-20/-23) override this via
        // `ScaledVideoEncoder::set_async_depth` at lazy-open.
        async_depth: 0,
    }
}

pub fn resolve_preset(s: Option<&str>) -> VideoPreset {
    match s.unwrap_or("medium") {
        "ultrafast" => VideoPreset::Ultrafast,
        "superfast" => VideoPreset::Superfast,
        "veryfast" => VideoPreset::Veryfast,
        "faster" => VideoPreset::Faster,
        "fast" => VideoPreset::Fast,
        "medium" => VideoPreset::Medium,
        "slow" => VideoPreset::Slow,
        "slower" => VideoPreset::Slower,
        "veryslow" => VideoPreset::Veryslow,
        _ => VideoPreset::Medium,
    }
}

pub fn resolve_profile(s: Option<&str>) -> VideoProfile {
    match s.unwrap_or("") {
        "baseline" => VideoProfile::Baseline,
        "main" => VideoProfile::Main,
        "high" => VideoProfile::High,
        "high10" => VideoProfile::High10,
        "high422" => VideoProfile::High422,
        "high444" => VideoProfile::High444,
        "main10" => VideoProfile::Main10,
        "main422-10" => VideoProfile::Main422_10,
        "main422-10-intra" => VideoProfile::Main422_10Intra,
        _ => VideoProfile::Auto,
    }
}

pub fn resolve_chroma(s: Option<&str>) -> VideoChroma {
    match s.unwrap_or("yuv420p") {
        "yuv422p" => VideoChroma::Yuv422,
        "yuv444p" => VideoChroma::Yuv444,
        _ => VideoChroma::Yuv420,
    }
}

pub fn resolve_rate_control(s: Option<&str>) -> VideoRateControl {
    // Broadcast contribution defaults to CBR — VBR complicates wire pacing
    // and downstream mux ingest. Operators who want VBR opt in explicitly.
    match s.unwrap_or("cbr") {
        "vbr" => VideoRateControl::Vbr,
        "crf" => VideoRateControl::Crf,
        "abr" => VideoRateControl::Abr,
        _ => VideoRateControl::Cbr,
    }
}

/// Pick the [`video_codec::ScalerDstFormat`] that matches the encoder's
/// configured chroma + bit depth, so the scaler's output is feedable
/// directly into `VideoEncoder::encode_frame` without an extra repack.
///
/// Returns `None` for target combinations that `VideoScaler` does not
/// expose today (4:4:4). Callers should fall back to "no scale, use
/// source resolution" when that happens — cropping is still wrong, but
/// any behaviour change would be gated on extending
/// `bilbycast-ffmpeg-video-rs` first.
#[cfg(feature = "media-codecs")]
pub fn select_scaler_dst_format(
    chroma: VideoChroma,
    bit_depth: u8,
) -> Option<video_codec::ScalerDstFormat> {
    use video_codec::ScalerDstFormat;
    match (chroma, bit_depth) {
        // Limited-range `Yuv420p8`, not full-range `Yuvj420p`. This function
        // feeds a video encoder opened as `AV_PIX_FMT_YUV420P`; targeting a
        // `J` format makes libswscale range-expand the samples, which the
        // stream then signals as limited range. `Yuvj420p` remains correct for
        // the MJPEG / thumbnail path, which selects it directly.
        (VideoChroma::Yuv420, 8) => Some(ScalerDstFormat::Yuv420p8),
        (VideoChroma::Yuv420, 10) => Some(ScalerDstFormat::Yuv420p10le),
        (VideoChroma::Yuv422, 8) => Some(ScalerDstFormat::Yuv422p8),
        (VideoChroma::Yuv422, 10) => Some(ScalerDstFormat::Yuv422p10le),
        _ => None,
    }
}


/// Lazily-opened encoder + optional scaler, shared across every call
/// site that decodes a frame and re-encodes it.
///
/// The first call to [`ScaledVideoEncoder::encode`] inspects the decoded
/// frame's `width` / `height` / `pixel_format`, compares against the
/// operator's requested `video_encode.width` / `.height`, and:
///
/// - Opens the encoder at the resolved target resolution (requested, or
///   source if unset).
/// - Opens a [`video_engine::VideoScaler`] between decoder and encoder
///   iff the source and target dimensions differ AND the target
///   chroma/bit-depth combination is supported by the scaler.
/// - Caches the source dimensions; a later frame whose dimensions change
///   (rare but legal — mid-stream resolution change) triggers a scaler
///   rebuild.
///
/// ST 2110 ingest reaches the encoder through a different shape (raw
/// RFC 4175 planes, no upstream decoder), so it uses
/// [`ScaledVideoEncoder::encode_raw_planes`] instead.
#[cfg(feature = "media-codecs")]
pub struct ScaledVideoEncoder {
    encode_cfg: VideoEncodeConfig,
    /// Backend chain: try in order on `avcodec_open2` failure. Single
    /// element for explicit codecs (`x264`, `h264_qsv`, …); the full
    /// Auto priority list filtered through host capabilities for
    /// `*_auto`. See `engine::hardware_probe::resolve_video_encoder_chain`.
    /// Fall-through covers the case where the matrix says a backend is
    /// available (probe at startup succeeded) but a later runtime open
    /// fails — typical of HW backends that ran out of sessions or were
    /// shimmed by a userspace driver update mid-run.
    backend_chain: Vec<VideoEncoderCodec>,
    fps_num: u32,
    fps_den: u32,
    global_header: bool,

    encoder: Option<video_engine::VideoEncoder>,
    scaler: Option<video_engine::VideoScaler>,
    // Cached to spot mid-stream resolution / pixel-format changes.
    src_w: u32,
    src_h: u32,
    src_pix_fmt: i32,
    // Resolved output resolution, once the encoder is open.
    dst_w: u32,
    dst_h: u32,
    // Label used in warnings; injected by the caller so logs are readable.
    log_tag: String,
    // Optional sink that the encoder writes to on lazy-open success so
    // downstream stats can surface the actually-opened backend after
    // an Auto-chain demote. `None` means no caller cares.
    resolved_backend_sink: Option<std::sync::Arc<ResolvedBackendCell>>,
    // HW-encoder pipeline depth applied at lazy-open (0 = synchronous,
    // the default). Honoured by the QSV backends only today; see
    // `VideoEncoderConfig::async_depth`. Set by throughput-critical
    // ingest call sites (ST 2110-20/-23) where the source is a paced
    // raster and a per-frame submit-then-sync round trip caps the
    // encoder below wire rate.
    async_depth: u32,
    /// When set, the encoder is opened with a 1/90000 pts timebase: the pts
    /// passed to `encode` / `encode_raw_planes` are 90 kHz ticks (MPEG-TS
    /// ingest paths — SDI, ST 2110-20/-23), not a frame counter. Getting this
    /// wrong is not cosmetic: libx264's VBV rate control reads 90 kHz ticks
    /// against a 1/fps timebase as "frames minutes apart" and **segfaults**.
    pts_90k: bool,
}

#[cfg(feature = "media-codecs")]
impl ScaledVideoEncoder {
    /// Create a new pipeline pinned to a single backend. No Auto
    /// fall-through. Callers that already invoked the resolver and
    /// only want the resolved single backend (e.g. operator picked
    /// `x264` explicitly) keep using this. For Auto resolution that
    /// should fall through to the next candidate on
    /// `avcodec_open2` failure, use [`Self::with_backend_chain`].
    pub fn new(
        encode_cfg: VideoEncodeConfig,
        backend: VideoEncoderCodec,
        fps_num: u32,
        fps_den: u32,
        global_header: bool,
        log_tag: impl Into<String>,
    ) -> Self {
        Self::with_backend_chain(
            encode_cfg,
            vec![backend],
            fps_num,
            fps_den,
            global_header,
            log_tag,
        )
    }

    /// Create a pipeline that tries each backend in `backend_chain`
    /// until one's `VideoEncoder::open` succeeds. Empty input is
    /// rejected at lazy-open time with a clear error.
    pub fn with_backend_chain(
        encode_cfg: VideoEncodeConfig,
        backend_chain: Vec<VideoEncoderCodec>,
        fps_num: u32,
        fps_den: u32,
        global_header: bool,
        log_tag: impl Into<String>,
    ) -> Self {
        Self {
            encode_cfg,
            backend_chain,
            fps_num,
            fps_den,
            global_header,
            encoder: None,
            scaler: None,
            src_w: 0,
            src_h: 0,
            src_pix_fmt: 0,
            dst_w: 0,
            dst_h: 0,
            log_tag: log_tag.into(),
            resolved_backend_sink: None,
            async_depth: 0,
            pts_90k: false,
        }
    }

    /// Request pipelined HW submission (`depth` frames in flight) at
    /// lazy-open. Honoured by the QSV backends only today; the other
    /// backends ignore the value. Must be called before the first
    /// encode — once the encoder has lazy-opened the depth is locked in
    /// (libavcodec has no mid-stream pipeline reconfigure).
    pub fn set_async_depth(&mut self, depth: u32) {
        self.async_depth = depth;
    }

    /// Declare that this pipeline's pts values are 90 kHz ticks rather than a
    /// frame counter. Must be called before the first `encode*` (lazy-open
    /// reads it). See the `pts_90k` field for why this is load-bearing.
    pub fn set_pts_90k(&mut self) {
        self.pts_90k = true;
    }

    /// Plumb a [`ResolvedBackendCell`] that the encoder writes to on
    /// successful lazy-open. Call sites that surface a backend label
    /// in stats use this so the manager-UI badge tracks the actually-
    /// opened backend rather than the requested one (Auto-chain
    /// demotion would otherwise show a stale label).
    pub fn set_resolved_backend_sink(&mut self, sink: std::sync::Arc<ResolvedBackendCell>) {
        self.resolved_backend_sink = Some(sink);
    }

    pub fn is_open(&self) -> bool {
        self.encoder.is_some()
    }

    /// Update the fps the encoder will be opened with. Only effective
    /// before the encoder has lazy-opened — once the first frame has
    /// been encoded, libavcodec's time-base is locked in and changing
    /// it would invalidate downstream decoders. Callers use this to
    /// substitute a measured source fps for the placeholder fps the
    /// pipeline was constructed with.
    pub fn set_fps_if_unopened(&mut self, fps_num: u32, fps_den: u32) -> bool {
        if self.encoder.is_some() || fps_num == 0 || fps_den == 0 {
            return false;
        }
        self.fps_num = fps_num;
        self.fps_den = fps_den;
        true
    }

    /// Resolved output dimensions. Zero until [`Self::encode`] has been
    /// called at least once.
    pub fn dst_dimensions(&self) -> (u32, u32) {
        (self.dst_w, self.dst_h)
    }

    /// Out-of-band codec config (SPS/PPS for H.264, VPS/SPS/PPS for HEVC)
    /// once the encoder is open. Only populated when the encoder was
    /// opened with `global_header = true` (i.e. RTMP FLV sequence header,
    /// CMAF `avcC` / `hvcC`).
    pub fn extradata(&self) -> Option<Vec<u8>> {
        self.encoder
            .as_ref()
            .and_then(|e| e.extradata().map(|slice| slice.to_vec()))
    }

    /// Force the encoder to mark the next frame as an IDR. No-op until
    /// the encoder has been opened by the first [`Self::encode`] call.
    pub fn force_next_keyframe(&mut self) {
        if let Some(e) = self.encoder.as_mut() {
            e.force_next_keyframe();
        }
    }

    /// Drain any buffered frames from the encoder at end-of-stream.
    /// No-op if the encoder was never opened.
    pub fn flush(&mut self) -> Result<Vec<video_codec::EncodedVideoFrame>, String> {
        match self.encoder.as_mut() {
            Some(e) => e.flush().map_err(|err| format!("encoder flush failed: {err}")),
            None => Ok(Vec::new()),
        }
    }

    /// Encode one decoded frame. Lazy-opens the encoder (and, when
    /// needed, the scaler) on the first call.
    ///
    /// HW-decoded source frames (`AV_PIX_FMT_VAAPI` produced by
    /// `DecoderBackend::Vaapi`) are downloaded to system memory via
    /// `download_to_sysmem` before the format check — the resulting
    /// frame is `NV12` (8-bit 4:2:0) or `P010LE` (10-bit 4:2:0) which
    /// the scaler converts to the encoder's planar input layout
    /// (`YUVJ420P` / `YUV420P10LE` / `YUV422P` / `YUV422P10LE`). NVDEC
    /// and QSV decoders already auto-download to sysmem in
    /// `DecodedFrame` so they reach this method as `NV12` / `P010LE`
    /// directly and just need the libswscale conversion.
    ///
    /// Returns the list of encoded frames libavcodec emitted for this
    /// input — often zero during the encoder's warm-up, otherwise one
    /// frame (may be more if B-frames are enabled, but MVP forces
    /// `max_b_frames = 0`).
    pub fn encode(
        &mut self,
        decoded: &video_engine::DecodedFrame,
        pts: Option<i64>,
    ) -> Result<Vec<video_codec::EncodedVideoFrame>, String> {
        // VAAPI HW frames live on the GPU — `yuv_planes()` returns
        // `None` and any planar accessor reads garbage. Download to a
        // sysmem `NV12` / `P010LE` frame so the scaler / SW-encoder
        // path can read pixels. NVDEC / QSV already produce sysmem
        // frames; non-VAAPI inputs hit this branch as a no-op.
        let downloaded;
        let frame_ref = if decoded.is_vaapi() {
            downloaded = decoded
                .download_to_sysmem()
                .map_err(|e| format!("VAAPI hwframe download failed: {e:?}"))?;
            &downloaded
        } else {
            decoded
        };

        let src_w = frame_ref.width();
        let src_h = frame_ref.height();
        let src_pix_fmt = frame_ref.pixel_format();

        if self.encoder.is_none() {
            self.lazy_open(src_w, src_h, src_pix_fmt)?;
        } else if src_w != self.src_w
            || src_h != self.src_h
            || src_pix_fmt != self.src_pix_fmt
        {
            // Mid-stream resolution / pixel-format change — rebuild the
            // scaler (but keep the encoder; changing the encoded-output
            // resolution mid-stream would invalidate downstream decoders
            // / DASH manifests, so we keep the target dims as-is).
            tracing::info!(
                "{}: source resolution/format changed {}x{}({}) -> {}x{}({}); rebuilding scaler",
                self.log_tag, self.src_w, self.src_h, self.src_pix_fmt,
                src_w, src_h, src_pix_fmt,
            );
            self.src_w = src_w;
            self.src_h = src_h;
            self.src_pix_fmt = src_pix_fmt;
            self.scaler = self.try_build_scaler(src_w, src_h, src_pix_fmt);
        }

        let enc = self.encoder.as_mut().unwrap();

        if let Some(scaler) = self.scaler.as_ref() {
            let scaled = scaler
                .scale(frame_ref)
                .map_err(|e| format!("scaler failed: {e}"))?;
            let (y, y_s) = scaled
                .plane(0)
                .ok_or_else(|| "scaled frame missing Y plane".to_string())?;
            let (u, u_s) = scaled
                .plane(1)
                .ok_or_else(|| "scaled frame missing U plane".to_string())?;
            let (v, v_s) = scaled
                .plane(2)
                .ok_or_else(|| "scaled frame missing V plane".to_string())?;
            enc.encode_frame(y, y_s, u, u_s, v, v_s, pts)
                .map_err(|e| format!("encoder encode_frame failed: {e}"))
        } else {
            let (y, y_s, u, u_s, v, v_s) = frame_ref
                .yuv_planes()
                .ok_or_else(|| "decoded frame has no planar YUV".to_string())?;
            enc.encode_frame(y, y_s, u, u_s, v, v_s, pts)
                .map_err(|e| format!("encoder encode_frame failed: {e}"))
        }
    }

    /// Encode one raw planar YUV frame that did not come from a
    /// [`video_engine::VideoDecoder`] (e.g. RFC 4175 depacketised ST 2110
    /// frames). `src_pix_fmt` must be the FFmpeg `AVPixelFormat` value
    /// matching the supplied planes (YUV422P for 4:2:2 8-bit,
    /// YUV422P10LE for 4:2:2 10-bit, YUV420P for 4:2:0 8-bit, …).
    ///
    /// Same lazy-open + optional-scaler semantics as [`Self::encode`];
    /// when scaling is not needed the planes are forwarded verbatim.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_raw_planes(
        &mut self,
        src_w: u32,
        src_h: u32,
        src_pix_fmt: i32,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        u_stride: usize,
        v: &[u8],
        v_stride: usize,
        pts: Option<i64>,
    ) -> Result<Vec<video_codec::EncodedVideoFrame>, String> {
        if self.encoder.is_none() {
            self.lazy_open(src_w, src_h, src_pix_fmt)?;
        } else if src_w != self.src_w
            || src_h != self.src_h
            || src_pix_fmt != self.src_pix_fmt
        {
            tracing::info!(
                "{}: source resolution/format changed {}x{}({}) -> {}x{}({}); rebuilding scaler",
                self.log_tag, self.src_w, self.src_h, self.src_pix_fmt,
                src_w, src_h, src_pix_fmt,
            );
            self.src_w = src_w;
            self.src_h = src_h;
            self.src_pix_fmt = src_pix_fmt;
            self.scaler = self.try_build_scaler(src_w, src_h, src_pix_fmt);
        }

        let enc = self.encoder.as_mut().unwrap();

        if let Some(scaler) = self.scaler.as_ref() {
            let scaled = scaler
                .scale_raw_planes(
                    src_w, src_h, src_pix_fmt,
                    y, y_stride, u, u_stride, v, v_stride,
                )
                .map_err(|e| format!("scaler failed: {e}"))?;
            let (y2, y2_s) = scaled
                .plane(0)
                .ok_or_else(|| "scaled frame missing Y plane".to_string())?;
            let (u2, u2_s) = scaled
                .plane(1)
                .ok_or_else(|| "scaled frame missing U plane".to_string())?;
            let (v2, v2_s) = scaled
                .plane(2)
                .ok_or_else(|| "scaled frame missing V plane".to_string())?;
            enc.encode_frame(y2, y2_s, u2, u2_s, v2, v2_s, pts)
                .map_err(|e| format!("encoder encode_frame failed: {e}"))
        } else {
            enc.encode_frame(y, y_stride, u, u_stride, v, v_stride, pts)
                .map_err(|e| format!("encoder encode_frame failed: {e}"))
        }
    }

    fn lazy_open(&mut self, src_w: u32, src_h: u32, src_pix_fmt: i32) -> Result<(), String> {
        if self.backend_chain.is_empty() {
            return Err(
                "encoder open failed: backend chain is empty (no candidates passed by the resolver)"
                    .into(),
            );
        }

        let mut last_err = String::new();
        let total = self.backend_chain.len();
        for (idx, &candidate) in self.backend_chain.iter().enumerate() {
            let mut enc_cfg = build_encoder_config(
                &self.encode_cfg,
                candidate,
                src_w,
                src_h,
                self.fps_num,
                self.fps_den,
                self.global_header,
            );
            enc_cfg.async_depth = self.async_depth;
            if self.pts_90k {
                enc_cfg.time_base_num = 1;
                enc_cfg.time_base_den = 90_000;
            }
            let dst_w = enc_cfg.width;
            let dst_h = enc_cfg.height;
            match video_engine::VideoEncoder::open(&enc_cfg) {
                Ok(encoder) => {
                    if idx > 0 {
                        // We fell through at least one backend in the
                        // Auto chain. Surface the demote loudly so the
                        // operator can see in the field that QSV /
                        // NVENC went sideways and we landed on the
                        // fallback — matches the `display_atomic_unavailable`
                        // pattern on the display output.
                        tracing::warn!(
                            "{}: video_encode resolver demoted to {} after {} failed open(s); reason: {}",
                            self.log_tag,
                            candidate.ffmpeg_name(),
                            idx,
                            last_err,
                        );
                    } else {
                        tracing::debug!(
                            "{}: video_encode opened with {}",
                            self.log_tag,
                            candidate.ffmpeg_name(),
                        );
                    }
                    self.encoder = Some(encoder);
                    if let Some(sink) = &self.resolved_backend_sink {
                        sink.store(candidate);
                    }
                    self.src_w = src_w;
                    self.src_h = src_h;
                    self.src_pix_fmt = src_pix_fmt;
                    self.dst_w = dst_w;
                    self.dst_h = dst_h;
                    self.scaler = self.try_build_scaler(src_w, src_h, src_pix_fmt);
                    return Ok(());
                }
                Err(e) => {
                    last_err = format!("{} open failed: {e}", candidate.ffmpeg_name());
                    if idx + 1 < total {
                        // More candidates to try — log at info so the
                        // demote chain is visible without flooding warn
                        // when the next one succeeds.
                        tracing::info!(
                            "{}: {}; trying next backend in chain",
                            self.log_tag,
                            last_err,
                        );
                    }
                }
            }
        }

        Err(format!(
            "encoder open failed: every backend in the resolver chain refused open ({} candidate(s)). Last: {}",
            total, last_err,
        ))
    }

    fn try_build_scaler(
        &self,
        src_w: u32,
        src_h: u32,
        src_pix_fmt: i32,
    ) -> Option<video_engine::VideoScaler> {
        // Three reasons to build a scaler:
        //   1. dimensions differ (operator asked for resize), or
        //   2. source pixel format is not a planar YUV layout the SW encoder
        //      feed path can drain via `yuv_planes()` — e.g. `NV12` / `P010LE`
        //      / `NV16` / `P210LE` from a HW decoder, after the VAAPI sysmem
        //      download in `ScaledVideoEncoder::encode`, or
        //   3. the source is planar YUV but a *different* planar layout to the
        //      one the encoder was opened with.
        //
        // (3) is the subtle one. `is_planar_yuv_av_pix_fmt` is equally true for
        // 4:2:0, 4:2:2 and 4:4:4 — they all carry three planes. But 4:2:2 has
        // twice the chroma rows of 4:2:0, so handing its planes to a 4:2:0
        // encoder makes the encoder read chroma from the wrong lines: luma and
        // geometry come out perfect while colour is ghosted and smeared.
        // Testing only "both planar" therefore silently corrupts every
        // same-resolution 4:2:2 → 4:2:0 transcode — which is the default
        // (`chroma: yuv420p`) for ST 2110-20 and SDI ingest of a 4:2:2 source.
        //
        // Skip conversion only when the source's plane geometry is exactly what
        // the encoder expects.
        let chroma = resolve_chroma(self.encode_cfg.chroma.as_deref());
        let bit_depth = self.encode_cfg.bit_depth.unwrap_or(8);

        let dims_match = src_w == self.dst_w && src_h == self.dst_h;
        let layout_matches = video_engine::planar_yuv_layout(src_pix_fmt)
            .is_some_and(|src_layout| src_layout == (chroma, bit_depth));
        if dims_match && layout_matches {
            return None;
        }
        let Some(dst_fmt) = select_scaler_dst_format(chroma, bit_depth) else {
            tracing::warn!(
                "{}: video_encode target {:?} {}-bit is not supported by VideoScaler; \
                 encoder will crop instead of scaling (source {}x{} -> requested {}x{})",
                self.log_tag, chroma, bit_depth, src_w, src_h, self.dst_w, self.dst_h,
            );
            return None;
        };
        match video_engine::VideoScaler::new_with_dst_format(
            src_w, src_h, src_pix_fmt, self.dst_w, self.dst_h, dst_fmt,
        ) {
            Ok(s) => {
                tracing::info!(
                    "{}: scaling {}x{}(pix_fmt={}) -> {}x{} ({:?})",
                    self.log_tag, src_w, src_h, src_pix_fmt, self.dst_w, self.dst_h, dst_fmt,
                );
                Some(s)
            }
            Err(e) => {
                tracing::warn!(
                    "{}: failed to build VideoScaler for {}x{} -> {}x{}: {e}; \
                     encoder will crop instead of scaling",
                    self.log_tag, src_w, src_h, self.dst_w, self.dst_h,
                );
                None
            }
        }
    }
}

// NOTE: the former `is_planar_yuv_av_pix_fmt` shim lived here. `try_build_scaler`
// was its only caller, and "is this *some* planar YUV?" is precisely the question
// that let a 4:2:2 source reach a 4:2:0 encoder unconverted. It is replaced by
// `video_engine::planar_yuv_layout`, which reports the actual plane geometry so
// the source can be compared against the encoder's.

#[cfg(all(test, feature = "media-codecs"))]
mod scaler_selection_tests {
    use super::select_scaler_dst_format;
    use video_codec::{ScalerDstFormat, VideoChroma};

    /// The encoder is opened as `AV_PIX_FMT_YUV420P` (limited range). Targeting
    /// the full-range `YUVJ420P` makes libswscale range-expand the samples,
    /// which the stream then signals as limited — a levels shift on every
    /// scaled 8-bit 4:2:0 encode. The MJPEG/thumbnail path picks `Yuvj420p`
    /// directly and is unaffected.
    #[test]
    fn eight_bit_420_encoder_feed_is_limited_range() {
        assert_eq!(
            select_scaler_dst_format(VideoChroma::Yuv420, 8),
            Some(ScalerDstFormat::Yuv420p8),
            "encoder feed must not target a full-range J format",
        );
    }

    #[test]
    fn other_targets_unchanged() {
        assert_eq!(
            select_scaler_dst_format(VideoChroma::Yuv420, 10),
            Some(ScalerDstFormat::Yuv420p10le),
        );
        assert_eq!(
            select_scaler_dst_format(VideoChroma::Yuv422, 8),
            Some(ScalerDstFormat::Yuv422p8),
        );
        assert_eq!(
            select_scaler_dst_format(VideoChroma::Yuv422, 10),
            Some(ScalerDstFormat::Yuv422p10le),
        );
        // 4:4:4 has no scaler destination today.
        assert_eq!(select_scaler_dst_format(VideoChroma::Yuv444, 8), None);
    }

    /// The predicate `try_build_scaler` now uses. A scaler must be built
    /// whenever the source's plane geometry differs from the encoder's, even at
    /// identical dimensions — the case the old "both planar" test missed.
    fn conversion_needed(src_pix_fmt: i32, chroma: VideoChroma, bit_depth: u8) -> bool {
        !video_engine::planar_yuv_layout(src_pix_fmt)
            .is_some_and(|src| src == (chroma, bit_depth))
    }

    #[test]
    fn same_resolution_422_source_into_420_encoder_needs_conversion() {
        let src_422 = video_engine::av_pix_fmt_for_yuv(VideoChroma::Yuv422, 8).unwrap();
        assert!(
            conversion_needed(src_422, VideoChroma::Yuv420, 8),
            "4:2:2 planes must never reach a 4:2:0 encoder unconverted \
             (perfect luma, ghosted chroma)",
        );
    }

    #[test]
    fn matching_layout_still_skips_the_scaler() {
        let src_420 = video_engine::av_pix_fmt_for_yuv(VideoChroma::Yuv420, 8).unwrap();
        assert!(
            !conversion_needed(src_420, VideoChroma::Yuv420, 8),
            "identical layout must stay zero-copy — no pointless swscale pass",
        );
        let src_422 = video_engine::av_pix_fmt_for_yuv(VideoChroma::Yuv422, 8).unwrap();
        assert!(!conversion_needed(src_422, VideoChroma::Yuv422, 8));
    }

    #[test]
    fn bit_depth_mismatch_also_needs_conversion() {
        let src_8 = video_engine::av_pix_fmt_for_yuv(VideoChroma::Yuv420, 8).unwrap();
        assert!(conversion_needed(src_8, VideoChroma::Yuv420, 10));
        let src_10 = video_engine::av_pix_fmt_for_yuv(VideoChroma::Yuv420, 10).unwrap();
        assert!(conversion_needed(src_10, VideoChroma::Yuv420, 8));
    }
}

#[cfg(test)]
mod tune_tests {
    use super::{default_tune_for, sanitise_tune};
    use video_codec::VideoEncoderCodec::{self, *};

    const ALL_BACKENDS: &[VideoEncoderCodec] = &[
        X264, X265, H264Nvenc, HevcNvenc, H264Qsv, HevcQsv, H264Vaapi, HevcVaapi,
    ];

    /// `zerolatency` is x264-only. Defaulting it for every backend made
    /// `avcodec_open2` return EINVAL(-22) for every NVENC user who did not
    /// explicitly set `tune: ""` — i.e. all of them.
    #[test]
    fn zerolatency_defaults_only_for_software_backends() {
        assert_eq!(default_tune_for(X264), "zerolatency");
        assert_eq!(default_tune_for(X265), "zerolatency");
        for &b in &[H264Nvenc, HevcNvenc, H264Qsv, HevcQsv, H264Vaapi, HevcVaapi] {
            assert_eq!(
                default_tune_for(b),
                "",
                "{b:?} must not default to an x264 tune",
            );
        }
    }

    /// The invariant that actually protects the encoder: whatever the operator
    /// (or the `*_auto` resolver) produces, the tune handed to `avcodec_open2`
    /// is one that backend accepts — or empty, meaning "not passed at all".
    #[test]
    fn no_backend_ever_receives_a_tune_it_rejects() {
        let every_tune = [
            "zerolatency",
            "film",
            "animation",
            "grain",
            "stillimage",
            "fastdecode",
            "psnr",
            "ssim",
            "hq",
            "ll",
            "ull",
            "lossless",
            "",
        ];
        for &backend in ALL_BACKENDS {
            let acceptable: &[&str] = match backend {
                X264 | X265 => super::SW_TUNES,
                H264Nvenc | HevcNvenc => super::NVENC_TUNES,
                _ => &[],
            };
            for tune in every_tune {
                let out = sanitise_tune(backend, tune.to_string());
                assert!(
                    out.is_empty() || acceptable.contains(&out.as_str()),
                    "{backend:?} would receive unsupported tune {out:?}",
                );
            }
        }
    }

    #[test]
    fn valid_tunes_survive_sanitisation() {
        assert_eq!(sanitise_tune(X264, "zerolatency".into()), "zerolatency");
        assert_eq!(sanitise_tune(X265, "grain".into()), "grain");
        assert_eq!(sanitise_tune(H264Nvenc, "ll".into()), "ll");
        assert_eq!(sanitise_tune(HevcNvenc, "hq".into()), "hq");
    }

    #[test]
    fn cross_family_tunes_are_dropped_not_passed_through() {
        // The reported bug, exactly.
        assert_eq!(sanitise_tune(H264Nvenc, "zerolatency".into()), "");
        // And its mirror image.
        assert_eq!(sanitise_tune(X264, "ll".into()), "");
        // QSV / VAAPI have no tune option at all.
        assert_eq!(sanitise_tune(H264Qsv, "zerolatency".into()), "");
        assert_eq!(sanitise_tune(HevcVaapi, "hq".into()), "");
    }

    /// The SW and NVENC vocabularies must stay disjoint, otherwise
    /// `sanitise_tune`'s per-family dispatch would silently accept a tune for
    /// the wrong backend.
    #[test]
    fn tune_vocabularies_are_disjoint() {
        for t in super::NVENC_TUNES {
            assert!(!super::SW_TUNES.contains(t), "{t} appears in both tables");
        }
    }
}

#[cfg(test)]
mod preset_tests {
    use super::sanitise_preset;
    use video_codec::VideoEncoderCodec::*;
    use video_codec::VideoPreset::{self, *};

    const ALL_PRESETS: &[VideoPreset] = &[
        Ultrafast, Superfast, Veryfast, Faster, Fast, Medium, Slow, Slower, Veryslow,
    ];

    /// The invariant: whatever the operator (or the `*_auto` resolver)
    /// produces, the preset handed to `avcodec_open2` is one that backend
    /// accepts. NVENC's named presets are slow/medium/fast; QSV rejects
    /// ultrafast/superfast. Handing either an x264-only name is EINVAL.
    #[test]
    fn no_backend_ever_receives_a_preset_it_rejects() {
        for &p in ALL_PRESETS {
            for &b in &[H264Nvenc, HevcNvenc] {
                assert!(
                    matches!(sanitise_preset(b, p), Fast | Medium | Slow),
                    "{b:?} would receive unsupported preset {:?}",
                    sanitise_preset(b, p),
                );
            }
            for &b in &[H264Qsv, HevcQsv] {
                assert!(
                    !matches!(sanitise_preset(b, p), Ultrafast | Superfast),
                    "{b:?} would receive unsupported preset {:?}",
                    sanitise_preset(b, p),
                );
            }
        }
    }

    /// Mapping preserves the operator's speed/quality intent rather than
    /// silently resetting to a default: fast asks stay fast, slow stay slow.
    #[test]
    fn mapping_preserves_speed_intent() {
        // The reported bug, exactly: ultrafast on NVENC.
        assert_eq!(sanitise_preset(H264Nvenc, Ultrafast), Fast);
        assert_eq!(sanitise_preset(HevcNvenc, Veryslow), Slow);
        assert_eq!(sanitise_preset(H264Qsv, Ultrafast), Veryfast);
    }

    /// Presets the backend accepts pass through untouched — including on
    /// x264/x265 (full ladder) and VAAPI (no preset option; harmless).
    #[test]
    fn supported_presets_pass_through() {
        for &p in ALL_PRESETS {
            assert_eq!(sanitise_preset(X264, p), p);
            assert_eq!(sanitise_preset(X265, p), p);
            assert_eq!(sanitise_preset(H264Vaapi, p), p);
            assert_eq!(sanitise_preset(HevcVaapi, p), p);
        }
        for &p in &[Fast, Medium, Slow] {
            assert_eq!(sanitise_preset(H264Nvenc, p), p);
        }
        for &p in &[Veryfast, Faster, Fast, Medium, Slow, Slower, Veryslow] {
            assert_eq!(sanitise_preset(H264Qsv, p), p);
        }
    }
}
