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

use crate::config::models::VideoEncodeConfig;
use video_codec::{
    VideoChroma, VideoEncoderCodec, VideoEncoderConfig, VideoPreset, VideoProfile,
    VideoRateControl,
};

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
        preset: resolve_preset(cfg.preset.as_deref()),
        profile: resolve_profile(cfg.profile.as_deref()),
        chroma: resolve_chroma(cfg.chroma.as_deref()),
        bit_depth: cfg.bit_depth.unwrap_or(8),
        rate_control: resolve_rate_control(cfg.rate_control.as_deref()),
        crf: cfg.crf.unwrap_or(23),
        max_b_frames: cfg.bframes.unwrap_or(0),
        refs: cfg.refs.unwrap_or(0),
        tune: cfg.tune.clone().unwrap_or_else(|| "zerolatency".to_string()),
        level: cfg.level.clone().unwrap_or_default(),
        color_primaries: cfg.color_primaries.clone().unwrap_or_default(),
        color_transfer: cfg.color_transfer.clone().unwrap_or_default(),
        color_matrix: cfg.color_matrix.clone().unwrap_or_default(),
        color_range: cfg.color_range.clone().unwrap_or_default(),
        global_header,
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
    match s.unwrap_or("vbr") {
        "cbr" => VideoRateControl::Cbr,
        "crf" => VideoRateControl::Crf,
        "abr" => VideoRateControl::Abr,
        _ => VideoRateControl::Vbr,
    }
}

/// Pick the [`video_codec::ScalerDstFormat`] that matches the encoder's
/// configured chroma + bit depth, so the scaler's output is feedable
/// directly into `VideoEncoder::encode_frame` without an extra repack.
///
/// Returns `None` for target combinations that `VideoScaler` does not
/// expose today (4:2:0 10-bit, 4:4:4). Callers should fall back to
/// "no scale, use source resolution" when that happens — cropping is
/// still wrong, but any behaviour change would be gated on extending
/// `bilbycast-ffmpeg-video-rs` first.
#[cfg(feature = "video-thumbnail")]
pub fn select_scaler_dst_format(
    chroma: VideoChroma,
    bit_depth: u8,
) -> Option<video_codec::ScalerDstFormat> {
    use video_codec::ScalerDstFormat;
    match (chroma, bit_depth) {
        (VideoChroma::Yuv420, 8) => Some(ScalerDstFormat::Yuvj420p),
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
#[cfg(feature = "video-thumbnail")]
pub struct ScaledVideoEncoder {
    encode_cfg: VideoEncodeConfig,
    backend: VideoEncoderCodec,
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
}

#[cfg(feature = "video-thumbnail")]
impl ScaledVideoEncoder {
    pub fn new(
        encode_cfg: VideoEncodeConfig,
        backend: VideoEncoderCodec,
        fps_num: u32,
        fps_den: u32,
        global_header: bool,
        log_tag: impl Into<String>,
    ) -> Self {
        Self {
            encode_cfg,
            backend,
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
        }
    }

    pub fn is_open(&self) -> bool {
        self.encoder.is_some()
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
    /// Returns the list of encoded frames libavcodec emitted for this
    /// input — often zero during the encoder's warm-up, otherwise one
    /// frame (may be more if B-frames are enabled, but MVP forces
    /// `max_b_frames = 0`).
    pub fn encode(
        &mut self,
        decoded: &video_engine::DecodedFrame,
        pts: Option<i64>,
    ) -> Result<Vec<video_codec::EncodedVideoFrame>, String> {
        let src_w = decoded.width();
        let src_h = decoded.height();
        let src_pix_fmt = decoded.pixel_format();

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
                .scale(decoded)
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
            let (y, y_s, u, u_s, v, v_s) = decoded
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
        let enc_cfg = build_encoder_config(
            &self.encode_cfg,
            self.backend,
            src_w,
            src_h,
            self.fps_num,
            self.fps_den,
            self.global_header,
        );
        let dst_w = enc_cfg.width;
        let dst_h = enc_cfg.height;
        let encoder = video_engine::VideoEncoder::open(&enc_cfg)
            .map_err(|e| format!("encoder open failed: {e}"))?;
        self.encoder = Some(encoder);
        self.src_w = src_w;
        self.src_h = src_h;
        self.src_pix_fmt = src_pix_fmt;
        self.dst_w = dst_w;
        self.dst_h = dst_h;
        self.scaler = self.try_build_scaler(src_w, src_h, src_pix_fmt);
        Ok(())
    }

    fn try_build_scaler(
        &self,
        src_w: u32,
        src_h: u32,
        src_pix_fmt: i32,
    ) -> Option<video_engine::VideoScaler> {
        if src_w == self.dst_w && src_h == self.dst_h {
            return None;
        }
        let chroma = resolve_chroma(self.encode_cfg.chroma.as_deref());
        let bit_depth = self.encode_cfg.bit_depth.unwrap_or(8);
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
                    "{}: scaling {}x{} -> {}x{} ({:?})",
                    self.log_tag, src_w, src_h, self.dst_w, self.dst_h, dst_fmt,
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
