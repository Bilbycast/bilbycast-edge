// Copyright (c) 2026 Reza Rahimi / Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Shared resolution logic for building a `video_codec::VideoEncoderConfig`
//! from the edge-side [`VideoEncodeConfig`].
//!
//! Every call site that opens a [`video_engine::VideoEncoder`] — RTMP,
//! WebRTC, TS video replacer, ST 2110-20/-23 — goes through
//! [`build_encoder_config`]. Centralising the mapping keeps the four
//! backend wiring paths in lock-step as new knobs (chroma, bit depth,
//! rate control, colour metadata, …) are added to the config schema.

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
