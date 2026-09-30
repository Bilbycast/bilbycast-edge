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

use crate::config::models::{VideoEncodeConfig, VideoScan};
use video_codec::{
    VideoChroma, VideoEncoderCodec, VideoEncoderConfig, VideoFieldOrder, VideoPreset,
    VideoProfile, VideoRateControl,
};

/// Lock-free cell that publishes the backend a [`ScaledVideoEncoder`]
/// actually opened with after lazy-open, and the scan it codes. Backend:
/// `0` = unset (encoder not yet opened); `1..=10` map to the ten
/// [`VideoEncoderCodec`] variants. Snapshot path maps the value back to
/// the operator-facing label so the manager-UI badge tracks the resolved
/// backend after Auto-chain demotion (e.g., NVENC → x264 fallback). Scan:
/// `0` = unset, then progressive / top field first / bottom field first,
/// rewritten on every (re)open and again when a field-coded encoder
/// follows the source to the other field order without one —
/// `video_encode_stats.coded_scan`.
#[derive(Default, Debug)]
pub struct ResolvedBackendCell {
    backend: AtomicU8,
    scan: AtomicU8,
}

impl ResolvedBackendCell {
    /// Record an open: the backend and the field order it codes (`None` =
    /// progressive).
    pub fn store(&self, codec: VideoEncoderCodec, field_order: Option<VideoFieldOrder>) {
        self.backend.store(codec_to_u8(codec), Ordering::Relaxed);
        self.scan.store(scan_to_u8(field_order), Ordering::Relaxed);
    }

    /// Record that a field-coded encoder now stamps `order` on the frames
    /// it codes: a TFF ↔ BFF source switch it followed without a reopen
    /// ([`ScaledVideoEncoder`]'s per-frame field-order follow). The
    /// backend is unchanged, so only the scan byte is rewritten.
    pub fn store_field_order(&self, order: VideoFieldOrder) {
        self.scan.store(scan_to_u8(Some(order)), Ordering::Relaxed);
    }

    /// Returns `Some(label)` once the encoder has lazy-opened, where
    /// `label` is the same family-collapsed tag the call sites compute
    /// at output start (`"x264"`, `"x265"`, `"nvenc"`, `"qsv"`,
    /// `"vaapi"`). `None` means the encoder hasn't opened yet, so the
    /// caller should keep using the requested-codec label.
    pub fn label(&self) -> Option<&'static str> {
        u8_to_codec(self.backend.load(Ordering::Relaxed)).map(backend_label)
    }

    /// The scan the open encoder codes — `"progressive"`,
    /// `"interlaced_tff"` or `"interlaced_bff"` — or `None` before the
    /// first open. What `video_encode.scan` actually resolved to (`auto`
    /// and a refused `interlaced` both land here as what was coded).
    pub fn coded_scan(&self) -> Option<&'static str> {
        match self.scan.load(Ordering::Relaxed) {
            1 => Some("progressive"),
            2 => Some("interlaced_tff"),
            3 => Some("interlaced_bff"),
            _ => None,
        }
    }
}

/// The scan byte of a [`ResolvedBackendCell`]: `None` (progressive) = 1,
/// top field first = 2, bottom field first = 3 — read back by
/// [`ResolvedBackendCell::coded_scan`].
fn scan_to_u8(field_order: Option<VideoFieldOrder>) -> u8 {
    match field_order {
        None => 1,
        Some(VideoFieldOrder::Tff) => 2,
        Some(VideoFieldOrder::Bff) => 3,
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

/// Raise `video_encode_interlace_unavailable` for a re-encoding RTMP or
/// CMAF output (`kind`), from the notice its pipeline left at open
/// ([`ScaledVideoEncoder::take_interlace_notice`]) — once per encoder open.
/// Same Warning the TS replacer raises on its own scope: output-scoped,
/// category `video_encode`, details `{error_code, reason,
/// source_stream_type}`. These paths see a codec rather than a PMT, so
/// `source_stream_type` is that codec's standard stream_type (H.264
/// `0x1B`, HEVC `0x24`, MPEG-2 `0x02`).
pub fn emit_output_interlace_unavailable(
    events: &crate::manager::events::EventSender,
    kind: &str,
    output_id: &str,
    why: &str,
    source: video_codec::VideoCodec,
) {
    let source_stream_type: u8 = match source {
        video_codec::VideoCodec::H264 => 0x1B,
        video_codec::VideoCodec::Hevc => 0x24,
        video_codec::VideoCodec::Mpeg2 => 0x02,
    };
    events.emit_output_with_details(
        crate::manager::events::EventSeverity::Warning,
        crate::manager::events::category::VIDEO_ENCODE,
        format!(
            "{kind} output '{output_id}': video_encode.scan=interlaced cannot be honoured — \
             {why}; encoding progressive"
        ),
        output_id,
        serde_json::json!({
            "error_code": "video_encode_interlace_unavailable",
            "reason": why,
            "source_stream_type": source_stream_type,
        }),
    );
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

/// The GOP (frames) an encoder opened at `fps_num / fps_den` gets when
/// `video_encode.gop_size` is unset: two seconds of pictures, **rounded** to
/// the nearest frame. It was `2 * floor(fps)`, which truncates a fractional
/// rate — 29.97 fps got 58 frames (1.935 s) and 59.94 got 118, where 60 and
/// 120 are two seconds. Every transcoding output shares it (the CMAF
/// re-encode sizes its own to tile the segment, `cmaf_default_gop`).
pub fn default_gop_frames(fps_num: u32, fps_den: u32) -> u32 {
    let den = u64::from(fps_den.max(1));
    ((2 * u64::from(fps_num) + den / 2) / den).clamp(1, u64::from(u32::MAX)) as u32
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
    let gop_size = cfg.gop_size.unwrap_or_else(|| default_gop_frames(fps_num, fps_den));

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
        // Progressive, square-pixel signalling unless a caller sets them.
        field_order: None,
        sample_aspect_ratio: None,
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

// ───────────────────── Frame-rate measurement ─────────────────────

/// Standard frame rates a measured cadence snaps to, as `(num, den)`.
const STANDARD_FRAME_RATES: &[(u32, u32)] = &[
    (24_000, 1001),
    (24, 1),
    (25, 1),
    (30_000, 1001),
    (30, 1),
    (48, 1),
    (50, 1),
    (60_000, 1001),
    (60, 1),
    (100, 1),
    (120_000, 1001),
    (120, 1),
    (25, 2),
    (15, 1),
    (12, 1),
    (10, 1),
];

/// Relative distance within which a measured rate is taken to be a
/// [`STANDARD_FRAME_RATES`] entry (0.1 %) — widened by what the timestamps
/// can resolve (see [`FrameCadence`]).
const CADENCE_SNAP_TOLERANCE: f64 = 0.001;
/// The widest snap tolerance the meter answers with: past it (5 %) the
/// stamps are too noisy yet to say.
const CADENCE_SNAP_TOLERANCE_MAX: f64 = 0.05;
/// Relative spread within which two standard rates are one family — a rate
/// and its 1001 neighbour (0.1 % apart). 24 and 25, 48 and 50 are 4 % apart:
/// two families.
const CADENCE_FAMILY: f64 = 0.002;
/// How far two deltas — the fast path's, or a periodic cadence's — may
/// differ and still agree: 0.1 %, at least 2 ticks (90 kHz rounding).
const CADENCE_FAST_AGREEMENT: f64 = 0.001;
/// Standard errors of the least-squares slope the snap tolerance spans on
/// uneven stamps.
const CADENCE_JITTER_SIGMAS: f64 = 4.5;
/// Deltas jittered stamps need before the meter answers: a dozen at ±8 ms
/// swing the slope by up to 4 % — 25 fps read as 24.
const CADENCE_JITTER_MIN_DELTAS: usize = 16;
/// Deltas the cadence path averages over together: whole cycles of every
/// periodic cadence — 3:2 (period 2), 2:3:3:2 (4), and the 33 / 33 / 34 ms
/// pattern millisecond timestamps give 30 and 60 fps (3).
const CADENCE_WINDOW_UNIT: usize = 12;
/// Deltas the meter keeps (the newest win).
const CADENCE_WINDOW: usize = 32;
/// Most consecutive deltas the fast path needs to agree on: four one-frame
/// deltas.
const CADENCE_FAST_DELTAS: usize = 4;
/// Frames the fast path's agreeing deltas must cover between them — four
/// one-frame deltas, or as few as two spans of a source that stamps only
/// every Nth picture (each already a mean over its N frames).
const CADENCE_FAST_FRAMES: u32 = 4;
/// Stamped spans of a sparse source an unpinned encoder waits for past its
/// first stamp before opening at the fallback: the fast path's two, and one
/// for the stamp it joined partway through.
const SPARSE_LOCK_SPANS: u32 = 3;
/// The most frames past a sparse source's first stamp that wait may run
/// (see [`FrameCadence::lock_wait_frames`]): enough for a PTS every 700 ms
/// — MPEG-TS's limit — at 60 fps, joined anywhere; a source that stamps
/// once and never again waits this long, not for ever.
const SPARSE_LOCK_EXTRA_MAX: u32 = 2 * RATE_LOCK_FRAME_CAP;
/// Deltas the cadence path needs before it answers.
const CADENCE_MIN_DELTAS: usize = 12;
/// A delta more than this many times the median of those measured is a
/// splice — a media-player loop, a PTS jump the source made — not frames,
/// and is left out; two in a row are a new cadence, measured afresh. Taken
/// as frames, 770_H's 18.8-frame loop step read as 19 and a 50 fps source
/// measured 50.083 (see [`FrameCadence::observe`]).
const CADENCE_GAP_FACTOR: f64 = 4.0;
/// 33-bit PTS space.
const PTS_MASK_33B: u64 = (1u64 << 33) - 1;

/// Frame-rate meter over the presentation timestamps of **decoded
/// frames** — the pictures an encoder is actually handed, one call per
/// frame.
///
/// The TS video replacer used to take its encoder rate from the first
/// PES DTS delta. That is the rate of *coded pictures*, which is the frame
/// rate only when every picture is a frame: a PAFF H.264 or an MPEG-2
/// field-picture source carries one field per PES, 1800 ticks apart at
/// 25 Hz, while the decoder weaves each pair into one frame and the
/// encoder is called 25 times a second. Locked at 50/1, libx264 then
/// signalled 50 fps in the VUI, budgeted CBR for 50 frames a second (half
/// the configured bitrate) and ran a 4 s default GOP.
///
/// Estimator, over the per-frame deltas between consecutive stamped
/// frames (masked to 33 bits, the span divided by the frames it covers —
/// see below; a step back or a delta past 90 000 ticks is a discontinuity
/// and is skipped; a stamp jitter put within a millisecond of the last is a
/// frame whose time joins the next span):
///
/// - **fast path** — the newest deltas covering [`CADENCE_FAST_FRAMES`]
///   frames (four one-frame deltas; two spans of a source stamping only
///   every Nth picture) agree within `max(2 ticks, 0.1 %)`, and — unless
///   they are millisecond steps — so does every delta measured so far (one
///   over a dropped frame as the frames it covers): their mean. Four
///   deltas of jittered stamps agree now and then, and their mean is off by
///   up to half the jitter.
/// - **cadence path** — otherwise, with at least [`CADENCE_MIN_DELTAS`]
///   deltas. The median of the sums of every 4 consecutive deltas, over 4,
///   says roughly how long a frame is, and so how many frames each delta
///   covers (a dropped frame's delta, two — when it stands clear of the
///   other deltas' spread). A steady or periodic cadence (every delta, or
///   every 2nd / 3rd / 4th, agreeing: 3:2 or 2:3:3:2 pulldown, even
///   millisecond steps) is measured over windows of 12 deltas (24 once
///   there are that many) — whole cycles, so soft-telecined film measures
///   exactly 24000/1001 — as the median window, to 0.1 % (a millisecond's
///   rounding over the span for millisecond steps). Uneven millisecond
///   steps and jittered stamps take the least-squares slope of the stamps
///   over the frames, which averages the rounding or the jitter out, to
///   [`CADENCE_JITTER_SIGMAS`] standard errors of that slope (from the
///   scatter about it, or the per-frame deltas', whichever says more).
///   Jittered stamps wait for [`CADENCE_JITTER_MIN_DELTAS`] deltas, answer
///   only with a standard rate, and not while the tolerance is past
///   [`CADENCE_SNAP_TOLERANCE_MAX`]. The median of 4-delta sums on its own
///   read the 33 / 33 / 34 ms steps of an RTMP publish at 30 fps as 2992.5
///   ticks: an encoder opened at 90000/2993 (30.07 fps).
///
/// The duration then snaps to the **nearest** [`STANDARD_FRAME_RATES`]
/// entry within that tolerance — or, while two rate families (24 and 25,
/// 48 and 50; a rate and its 1001 neighbour are one) both lie within it,
/// the meter cannot say yet. Otherwise it is reported as
/// `90000 / round(duration)`, reduced — a genuinely non-standard rate
/// measured from clean stamps is never forced onto a standard one. The
/// encoder locks at the meter's **first** answer, so an answer must be
/// right when it comes. The meter used to answer jittered stamps from the
/// first dozen deltas (or four that happened to agree within 0.5 %), whose
/// estimate the jitter moves by up to 4 %, snapped within half the
/// per-frame spread over the span: over 2000 starts, 25 fps at ±8 ms locked
/// 24/1 in 7 % and a non-standard rate in 9 %, 30 fps a non-standard rate
/// in 11 %, 60 fps in 81 %, 50 fps 48/1 in 22 %, and even ±0.5 ms stamps a
/// non-standard rate in 7 % (30 fps) to 16 % (25 fps). Now all of those
/// lock their own family (one start in 2000 at 50 fps ±8 ms — jitter of
/// 40 % of a frame — took 48/1), at frame 17 (21 at 50 / 60 fps ±8 ms)
/// instead of 13. At the lock, a dozen millisecond stamps
/// cannot tell 30000/1001 from 30/1 (they drift a millisecond apart per
/// second): such a source may lock at the other one, 0.1 % off.
///
/// Only a decoder-carried timestamp is evidence: a frame without one (and
/// a negative one) is never stepped by a guess, which would only confirm
/// the guess — but it is **counted**. MPEG-TS needs a PTS only every
/// 700 ms, and some encoders stamp only every Nth picture or only the I
/// pictures, so the span between two stamped frames covers every frame
/// decoded across it: one delta of `span / frames`. Taken as one frame, a
/// source stamping every 12th picture at 29.97 fps measured 36 036 ticks —
/// an encoder opened at 2500/1001 fps, CBR budgeting 12x the bitrate per
/// frame, a 4-frame GOP. How long an encoder waits for such a source is
/// [`Self::lock_wait_frames`].
#[derive(Debug, Clone, Default)]
pub struct FrameCadence {
    last_pts: Option<u64>,
    /// Frames observed without a timestamp since `last_pts`.
    frames_since_pts: u32,
    /// Per-frame deltas (90 kHz ticks), fractional where a span over
    /// several frames does not divide evenly.
    deltas: std::collections::VecDeque<f64>,
    /// The frames each of `deltas` covers.
    delta_frames: std::collections::VecDeque<u32>,
    /// Frames observed since the reset, and how many had been when the
    /// first one with a timestamp came.
    observed: u32,
    first_stamp_at: Option<u32>,
    /// The most frames one measured delta has covered.
    widest_span_frames: u32,
    /// Consecutive deltas left out as a splice ([`CADENCE_GAP_FACTOR`]).
    gaps: u32,
}

impl FrameCadence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything — a new source.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feed one decoded frame's PTS (90 kHz, display order), exactly as
    /// the decoder returned it. `None` (no timestamp) measures nothing but
    /// counts a frame towards the next stamped one's span.
    pub fn observe(&mut self, pts: Option<i64>) {
        self.observed = self.observed.saturating_add(1);
        let Some(p) = pts.filter(|p| *p >= 0) else {
            if self.last_pts.is_some() {
                self.frames_since_pts = self.frames_since_pts.saturating_add(1);
            }
            return;
        };
        self.first_stamp_at.get_or_insert(self.observed);
        let p = p as u64 & PTS_MASK_33B;
        if let Some(last) = self.last_pts {
            let span = p.wrapping_sub(last) & PTS_MASK_33B;
            // A span past half the PTS space is a step back.
            let frames = self.frames_since_pts.saturating_add(1);
            let delta = span as f64 / f64::from(frames);
            if span > 0 && span < 1 << 32 && delta < 90.0 {
                // Jitter put this stamp within a millisecond of the last:
                // the frame joins the next span, as one without a stamp
                // does. Dropped with its stamp as a discontinuity, it took a
                // frame out of the count and its time with it, and the next
                // span read a frame long (60 fps at ±8 ms locked 56.5).
                self.frames_since_pts = frames;
                return;
            }
            if span < 1 << 32 && (90.0..=90_000.0).contains(&delta) {
                // A step many frames long is a splice (a media-player loop,
                // whose step the audio's whole frames set, not the video's),
                // not cadence: left out, the frames either side of it measure
                // the rate. Two in a row are the cadence itself changing (a
                // playlist item at another rate): measured afresh.
                if self.deltas.len() >= CADENCE_FAST_DELTAS
                    && delta > CADENCE_GAP_FACTOR * self.median_delta()
                {
                    self.gaps += 1;
                    if self.gaps < 2 {
                        self.last_pts = Some(p);
                        self.frames_since_pts = 0;
                        return;
                    }
                    self.deltas.clear();
                    self.delta_frames.clear();
                }
                self.gaps = 0;
                if self.deltas.len() == CADENCE_WINDOW {
                    self.deltas.pop_front();
                    self.delta_frames.pop_front();
                }
                self.deltas.push_back(delta);
                self.delta_frames.push_back(frames);
                self.widest_span_frames = self.widest_span_frames.max(frames);
            }
        }
        self.last_pts = Some(p);
        self.frames_since_pts = 0;
    }

    /// The median of the per-frame deltas measured so far.
    fn median_delta(&self) -> f64 {
        let mut d: Vec<f64> = self.deltas.iter().copied().collect();
        d.sort_unstable_by(f64::total_cmp);
        d[d.len() / 2]
    }

    /// Decoded frames (counted from the reset, as [`Self::observe`] saw
    /// them) an unpinned encoder waits for a rate before opening at a
    /// fallback: [`RATE_LOCK_FRAME_CAP`] — or, when the source stamps only
    /// every Nth picture, long enough past its first stamp for
    /// [`SPARSE_LOCK_SPANS`] such spans (the widest measured, or the one in
    /// progress), at most [`SPARSE_LOCK_EXTRA_MAX`] past it. Each span is
    /// one delta, and a flat 60 frames held only four spans of 14 pictures:
    /// a source stamping its I pictures every 15 or more (a 25 fps source
    /// with a 1 s GOP, a 50 fps one stamping every 25th picture) opened at
    /// 30/1 before its rate could be measured.
    pub fn lock_wait_frames(&self) -> u32 {
        let Some(first) = self.first_stamp_at else {
            return RATE_LOCK_FRAME_CAP;
        };
        let span = self.widest_span_frames.max(self.frames_since_pts.saturating_add(1));
        let sparse = first.saturating_add((SPARSE_LOCK_SPANS * span).min(SPARSE_LOCK_EXTRA_MAX));
        sparse.max(RATE_LOCK_FRAME_CAP)
    }

    /// Deltas measured so far (at most [`CADENCE_WINDOW`]).
    #[cfg(test)]
    pub fn deltas_seen(&self) -> usize {
        self.deltas.len()
    }

    /// Mean frame duration in 90 kHz ticks, or `None` until the meter can
    /// say.
    #[cfg(test)]
    pub fn frame_duration_90k(&self) -> Option<f64> {
        self.measure().map(|m| m.duration)
    }

    /// The measured rate as `(num, den)`, snapped to a standard rate —
    /// or `None` until the meter can say. It cannot while two rate
    /// families (24 / 25, 48 / 50: a rate and its 1001 neighbour are one
    /// family) both lie within what the stamps resolve, nor — for jittered
    /// stamps — while no standard rate does.
    pub fn rate(&self) -> Option<(u32, u32)> {
        let m = self.measure()?;
        let fps = 90_000.0 / m.duration;
        let mut within = STANDARD_FRAME_RATES
            .iter()
            .map(|&(n, d)| n as f64 / d as f64)
            .filter(|std| ((fps - std) / std).abs() <= m.tolerance);
        if let Some(first) = within.next()
            && within.any(|other| ((other - first) / first).abs() > CADENCE_FAMILY)
        {
            return None;
        }
        match nearest_standard_rate(m.duration, m.tolerance) {
            Some(r) => Some(r),
            None if m.standard_only => None,
            None => Some(snap_frame_duration(m.duration, m.tolerance)),
        }
    }

    /// See [`FrameCadence`].
    fn measure(&self) -> Option<Measured> {
        let n = self.deltas.len();
        // The newest deltas covering CADENCE_FAST_FRAMES frames: four
        // one-frame deltas, two spans of a sparse source.
        let mut covered = 0;
        let fast = self
            .delta_frames
            .iter()
            .rev()
            .take(CADENCE_FAST_DELTAS)
            .position(|f| {
                covered += f;
                covered >= CADENCE_FAST_FRAMES
            })
            .map(|i| (i + 1).max(2));
        if let Some(k) = fast.filter(|k| *k <= n) {
            let last: Vec<f64> = self.deltas.iter().skip(n - k).copied().collect();
            let lo = last.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = last.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let span: f64 = last.iter().sum();
            let mean = span / k as f64;
            let agree = (mean * CADENCE_FAST_AGREEMENT).max(2.0);
            if hi - lo <= agree {
                if !millisecond_stamps(&last) {
                    // Every delta so far agrees too (one over a dropped
                    // frame as the frames it covers): four deltas of
                    // jittered stamps agree now and then, and their mean
                    // is off by up to half the jitter.
                    let steady = self.deltas.iter().all(|x| {
                        let frames = (x / mean).round().max(1.0);
                        (x / frames - mean).abs() <= agree
                    });
                    if steady {
                        return Some(Measured::exact(mean, CADENCE_SNAP_TOLERANCE));
                    }
                } else {
                    // Four agreeing millisecond steps can sit half a
                    // millisecond off the rate (42 ms for 41.7): taken only
                    // when that still names a standard rate, else left to
                    // the cadence path.
                    let tolerance = (90.0 / span).max(CADENCE_SNAP_TOLERANCE);
                    if nearest_standard_rate(mean, tolerance).is_some() {
                        return Some(Measured::exact(mean, tolerance));
                    }
                }
            }
        }
        if n < CADENCE_MIN_DELTAS {
            return None;
        }
        let d: Vec<f64> = self.deltas.iter().copied().collect();
        let mut sums: Vec<f64> = d.windows(4).map(|w| w.iter().sum()).collect();
        sums.sort_unstable_by(f64::total_cmp);
        let coarse = sums[sums.len() / 2] / 4.0;
        // Frames each delta covers: a dropped frame's delta covers two —
        // when it stands clear of the jitter. Past 1.5 frames and further
        // from a frame than twice the widest spread of the one-frame deltas;
        // otherwise a jittered delta (±8 ms at 50 fps swings one to 1.6
        // frames) was read as a dropped frame and moved the slope 4 %.
        let spread = d
            .iter()
            .filter(|x| **x < 1.5 * coarse)
            .map(|x| (x - coarse).abs())
            .fold(0.0f64, f64::max);
        let frames: Vec<f64> = d
            .iter()
            .map(|&x| if x >= 1.5 * coarse && x - coarse > 2.0 * spread { (x / coarse).round() } else { 1.0 })
            .collect();
        let per_frame: Vec<f64> = d.iter().zip(&frames).map(|(x, f)| x / f).collect();
        let lo = per_frame.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = per_frame.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let quantised = millisecond_stamps(&d);
        // A steady or periodic cadence (3:2, 2:3:3:2 pulldown; even
        // millisecond steps): windows of whole cycles measure it exactly.
        let periodic = (1..=4).any(|p| {
            per_frame
                .iter()
                .zip(per_frame.iter().skip(p))
                .all(|(a, b)| (a - b).abs() <= (a * CADENCE_FAST_AGREEMENT).max(2.0))
        });
        if (quantised && hi <= lo) || (!quantised && periodic) {
            let w = n / CADENCE_WINDOW_UNIT * CADENCE_WINDOW_UNIT;
            let mut windows: Vec<(f64, f64)> = (0..=n - w)
                .map(|i| {
                    let span: f64 = d[i..i + w].iter().sum();
                    (span / frames[i..i + w].iter().sum::<f64>(), span)
                })
                .collect();
            windows.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
            let (duration, span) = windows[windows.len() / 2];
            let tolerance = if quantised { 90.0 / span } else { 0.0 };
            return Some(Measured::exact(duration, tolerance.max(CADENCE_SNAP_TOLERANCE)));
        }
        // Uneven millisecond steps (their rounding) or jittered stamps: the
        // least-squares slope of the stamps over the frames they are, which
        // averages both out. Jittered stamps wait for more deltas: a dozen
        // swing the slope by up to 4 % at ±8 ms.
        let jittered = !quantised || hi - lo > 90.0;
        if jittered && n < CADENCE_JITTER_MIN_DELTAS {
            return None;
        }
        let mut at = (0.0f64, 0.0f64);
        let points: Vec<(f64, f64)> = std::iter::once(at)
            .chain(d.iter().zip(&frames).map(|(x, f)| {
                at = (at.0 + f, at.1 + x);
                at
            }))
            .collect();
        let m = points.len() as f64;
        let (kx, ty) = points.iter().fold((0.0, 0.0), |(a, b), (k, t)| (a + k, b + t));
        let (kx, ty) = (kx / m, ty / m);
        let (num, sxx) = points
            .iter()
            .fold((0.0, 0.0), |(a, b), (k, t)| (a + (k - kx) * (t - ty), b + (k - kx) * (k - kx)));
        let duration = num / sxx;
        // The slope's standard error, from the stamps' scatter about it or
        // the per-frame deltas' (each the difference of two stamps' jitter),
        // whichever says more: a dozen residuals can look quiet by chance.
        let residual = points
            .iter()
            .map(|(k, t)| (t - ty - duration * (k - kx)).powi(2))
            .sum::<f64>()
            / (m - 2.0);
        let mean_frame = per_frame.iter().sum::<f64>() / per_frame.len() as f64;
        let frame_var = per_frame.iter().map(|x| (x - mean_frame).powi(2)).sum::<f64>()
            / per_frame.len() as f64
            / 2.0;
        let stderr = (residual.max(frame_var) / sxx).sqrt();
        let mut tolerance = (CADENCE_JITTER_SIGMAS * stderr / duration).max(CADENCE_SNAP_TOLERANCE);
        if quantised {
            tolerance = tolerance.max(90.0 / at.1);
        }
        if tolerance > CADENCE_SNAP_TOLERANCE_MAX {
            return None;
        }
        Some(Measured { duration, tolerance, standard_only: jittered })
    }
}

/// What [`FrameCadence::measure`] found: a mean frame duration (90 kHz),
/// the relative tolerance the stamps resolve it to, and whether only a
/// standard rate may be read from it (jittered stamps: a non-standard rate
/// within their noise is more likely a standard one measured badly).
struct Measured {
    duration: f64,
    tolerance: f64,
    standard_only: bool,
}

impl Measured {
    fn exact(duration: f64, tolerance: f64) -> Self {
        Self { duration, tolerance, standard_only: false }
    }
}

/// Whether every delta is a whole number of milliseconds — the stamps of an
/// RTMP publish (FLV carries milliseconds) or anything muxed from one.
fn millisecond_stamps(deltas: &[f64]) -> bool {
    deltas.iter().all(|d| d.fract() == 0.0 && (*d as u64).is_multiple_of(90))
}

/// The [`STANDARD_FRAME_RATES`] entry nearest `90000 / duration_90k` within
/// `tolerance` (relative).
fn nearest_standard_rate(duration_90k: f64, tolerance: f64) -> Option<(u32, u32)> {
    let fps = 90_000.0 / duration_90k;
    STANDARD_FRAME_RATES
        .iter()
        .map(|&(n, d)| {
            let std = n as f64 / d as f64;
            (((fps - std) / std).abs(), (n, d))
        })
        .filter(|(e, _)| *e <= tolerance)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, r)| r)
}

/// `90000 / duration_90k` as a frame rate: the nearest
/// [`STANDARD_FRAME_RATES`] entry within [`CADENCE_SNAP_TOLERANCE`], else
/// `90000 / round(duration)` reduced.
pub fn rate_from_frame_duration(duration_90k: f64) -> (u32, u32) {
    snap_frame_duration(duration_90k, CADENCE_SNAP_TOLERANCE)
}

/// [`rate_from_frame_duration`] within `tolerance`.
fn snap_frame_duration(duration_90k: f64, tolerance: f64) -> (u32, u32) {
    if let Some(r) = nearest_standard_rate(duration_90k, tolerance) {
        return r;
    }
    let den = (duration_90k.round() as u64).max(1);
    let g = gcd(90_000, den);
    ((90_000 / g) as u32, (den / g) as u32)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

/// Decoded frames an unpinned encoder waits for [`FrameCadence`] to measure
/// the source's rate before it opens at the fallback ([`RATE_LOCK_FALLBACK`])
/// — a source whose frames carry no usable PTS still gets an encoder.
pub const RATE_LOCK_FRAME_CAP: u32 = 60;

/// The rate an unpinned encoder opens at when the cadence cannot be
/// measured.
pub const RATE_LOCK_FALLBACK: (u32, u32) = (30, 1);

/// The encoder rate of a decode→encode output whose operator pinned no
/// `video_encode.fps_num` / `fps_den` — RTMP, WebRTC and CMAF — measured
/// from the decoded frames' own PTS as the TS video replacer measures it
/// (whose lock, `ts_video_replace::Inner::try_lock_rate`, also knows the PES
/// DTS step it falls back on).
///
/// These outputs used to open their encoder at a flat 30/1 whatever the
/// source ran at: a 25 fps source's SPS VUI said 30 fps, CBR budgeted
/// 25/30 of the configured bitrate, and the default GOP ran 20 % longer
/// than asked. The encoder's time base is fixed at open, so the frames
/// decoded before the rate is known are dropped — about four at start-up,
/// which is what the TS path drops too.
#[derive(Debug)]
pub struct EncoderRateLock {
    cadence: FrameCadence,
    locked: bool,
    unlocked_frames: u32,
}

/// What to do with a decoded frame, from [`EncoderRateLock::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateStep {
    /// The rate is not known yet: drop the frame — the encoder cannot open
    /// without one.
    Wait,
    /// The rate is known as of this frame: open the encoder at
    /// `num / den`, then encode the frame. `measured` is false for the
    /// fallback.
    Lock { num: u32, den: u32, measured: bool },
    /// Encode the frame.
    Encode,
}

impl EncoderRateLock {
    /// `pinned`: the operator set the rate, so there is nothing to measure
    /// and every frame is encoded.
    pub fn new(pinned: bool) -> Self {
        Self { cadence: FrameCadence::new(), locked: pinned, unlocked_frames: 0 }
    }

    /// Feed one decoded frame's PTS (90 kHz, display order, as the decoder
    /// returned it — the caller must hand the decoder each access unit's
    /// PTS).
    pub fn observe(&mut self, frame_pts: Option<i64>) -> RateStep {
        if self.locked {
            return RateStep::Encode;
        }
        self.cadence.observe(frame_pts);
        self.unlocked_frames += 1;
        let (num, den, measured) = match self.cadence.rate() {
            Some((n, d)) => (n, d, true),
            None if self.unlocked_frames >= self.cadence.lock_wait_frames() => {
                (RATE_LOCK_FALLBACK.0, RATE_LOCK_FALLBACK.1, false)
            }
            None => return RateStep::Wait,
        };
        self.locked = true;
        RateStep::Lock { num, den, measured }
    }

    /// [`Self::observe`] for a frame about to go to `pipeline`: sets the
    /// rate the encoder opens at when it locks, and says whether to encode
    /// the frame.
    pub fn admit(&mut self, frame_pts: Option<i64>, pipeline: &mut ScaledVideoEncoder) -> bool {
        match self.observe(frame_pts) {
            RateStep::Wait => false,
            RateStep::Encode => true,
            RateStep::Lock { num, den, measured } => {
                let applied = pipeline.set_fps_if_unopened(num, den);
                if measured {
                    tracing::info!(
                        "{}: source frame rate {num}/{den} ({:.3} fps) from the decoded-frame \
                         cadence — encoder {}",
                        pipeline.log_tag,
                        num as f64 / den as f64,
                        if applied { "opens at it" } else { "already open" },
                    );
                } else {
                    tracing::warn!(
                        "{}: source frame rate not measurable from {} decoded frames (no \
                         usable frame PTS) — opening the encoder at {num}/{den}",
                        pipeline.log_tag,
                        self.unlocked_frames,
                    );
                }
                true
            }
        }
    }
}

/// Which picture an encoded frame codes: the source PTS of every frame handed
/// to an encoder, keyed by the frame counter it was stamped with (the
/// encoder's 1 / fps pts, which it echoes on its output), until the encoder
/// hands the frame back. A decoder returns pictures in display order, as
/// many or as few per access unit as it has, so the access unit being fed
/// when a frame comes out is not the picture it codes.
///
/// An encoder with B-frames hands frames back in decode order (counters 0,
/// 3, 1, 2, …): each is found by its counter, and an entry
/// [`ENCODED_PTS_REORDER_WINDOW`] frames behind the counter coming back is
/// one the encoder dropped. Popping everything up to the echoed counter
/// took 1 and 2 as dropped when 3 came back, and every B-frame lost its
/// PTS.
#[derive(Debug, Default)]
pub struct EncodedPtsMap {
    in_flight: std::collections::VecDeque<(i64, Option<u64>)>,
}

/// Frames an encoder may hand a picture back behind the newest one (its
/// B-frame reordering; `video_encode.bframes` is at most 16).
pub const ENCODED_PTS_REORDER_WINDOW: i64 = 32;

impl EncodedPtsMap {
    /// A frame stamped `counter` goes to the encoder; `pts` is its decoded
    /// picture's source PTS (90 kHz), if it has one.
    pub fn push(&mut self, counter: i64, pts: Option<u64>) {
        self.in_flight.push_back((counter, pts));
    }

    /// The frame pushed last never reached the encoder (its encode failed).
    pub fn cancel_last(&mut self) {
        self.in_flight.pop_back();
    }

    /// The source PTS of the frame the encoder handed back stamped
    /// `counter`. Entries more than [`ENCODED_PTS_REORDER_WINDOW`] behind it
    /// were frames the encoder dropped.
    pub fn take(&mut self, counter: i64) -> Option<u64> {
        while self
            .in_flight
            .front()
            .is_some_and(|(c, _)| *c < counter - ENCODED_PTS_REORDER_WINDOW)
        {
            self.in_flight.pop_front();
        }
        let i = self.in_flight.iter().position(|(c, _)| *c == counter)?;
        self.in_flight.remove(i).and_then(|(_, pts)| pts)
    }
}

/// The source PTS each decoded picture goes out on: the decoder's, else —
/// the picture of an access unit its PES carried no PTS for (MPEG-TS needs
/// one only every 700 ms; some encoders stamp only their I pictures) — the
/// last picture's that had one plus a frame for each picture since. The
/// frame is measured from decoder PTS only, as the TS video replacer learns
/// the interval it steps by (`ts_video_replace::Inner::admit_decoded`): the
/// last span between two pictures that carried one, over the pictures it
/// covers, when that is a frame at 10–200 fps. Until a span is known it is
/// one frame at the encoder's rate — which is a pin, the 30 fps fallback,
/// or an earlier input's rate (an input switch cannot reopen the encoder),
/// so it can be far off the source: stepped by it for good, a 50 fps source
/// stamping every 25th picture, on an encoder at 30/1, stamped each GOP's
/// last picture 27 000 ticks past the next real PTS, and the timeline
/// stepped back once a GOP (zero-duration CMAF samples, RTP timestamps
/// going backwards). A derived PTS never teaches the interval. Fed every
/// decoded picture in display order, the ones the rate lock drops too, so
/// the count is right. `None` before any picture carried a PTS.
#[derive(Debug, Default)]
pub struct FramePtsStamper {
    last: Option<u64>,
    since: u64,
    /// The last span between two stamped pictures and the pictures it
    /// covers, when that is a frame at 10–200 fps.
    span: Option<(u64, u64)>,
}

impl FramePtsStamper {
    /// Stamp one decoded picture: `frame_pts` as the decoder returned it,
    /// `fallback_interval_90k` one frame at the encoder's rate — used only
    /// until a span between two real PTS has been measured.
    pub fn stamp(&mut self, frame_pts: Option<i64>, fallback_interval_90k: u64) -> Option<u64> {
        match frame_pts.filter(|p| *p >= 0) {
            Some(p) => {
                let p = p as u64 & PTS_MASK_33B;
                if let Some(last) = self.last {
                    let span = p.wrapping_sub(last) & PTS_MASK_33B;
                    let pictures = self.since + 1;
                    if (450..=9_000).contains(&(span / pictures)) {
                        self.span = Some((span, pictures));
                    }
                }
                self.last = Some(p);
                self.since = 0;
                Some(p)
            }
            None => {
                self.since += 1;
                let advance = match self.span {
                    Some((span, pictures)) => span * self.since / pictures,
                    None => fallback_interval_90k * self.since,
                };
                self.last.map(|l| l.wrapping_add(advance) & PTS_MASK_33B)
            }
        }
    }
}

/// One frame at `(num, den)` fps, in 90 kHz ticks.
pub fn frame_interval_90k((num, den): (u32, u32)) -> u64 {
    (90_000 * u64::from(den.max(1)) / u64::from(num.max(1))).max(1)
}

// ───────────────────── Lazy H.264 decoder open ─────────────────────

/// Access units a lazy H.264 decoder open passes over waiting for one that
/// carries an SPS, before it opens on whatever arrives (~6-12 s; broadcast
/// repeats the SPS every GOP).
pub const SPS_OPEN_WAIT_AUS: u32 = 300;

/// Whether an Annex B access unit carries an H.264 SPS NAL unit.
pub fn carries_h264_sps(au: &[u8]) -> bool {
    video_engine::annexb_nal_units(au).any(|n| n.first().is_some_and(|b| b & 0x1F == 7))
}

/// Holds a lazy H.264 decoder open back until an access unit that carries
/// an SPS arrives, bounded by [`SPS_OPEN_WAIT_AUS`].
///
/// A decoder opened with `ReorderSeed::FromAccessUnit` takes its reorder
/// depth from the AU it opens on: 0 when that AU's SPS declares one
/// (libavcodec then applies the declared depth — 0 for the IPPP streams
/// x264 `zerolatency` and most contribution encoders produce), 1 otherwise.
/// libavcodec only ever raises the depth, so a decoder opened on an AU
/// without the SPS — any join mid-GOP — holds one frame (40 ms at 25 fps)
/// for good on a source that declares none, where waiting for the SPS
/// costs nothing: no picture decodes before one anyway. The TS video
/// replacer has always waited; the RTMP, WebRTC, CMAF, ST 2110-20 / -23,
/// MXL and mosaic-tile decoders wait through this. Other codecs pass at
/// once.
#[derive(Debug, Clone, Default)]
pub struct SpsOpenGate {
    passed_over: u32,
}

impl SpsOpenGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a decoder for `codec` may open on `au` (Annex B) now: any
    /// codec but H.264, an AU carrying an SPS, or any AU once
    /// [`SPS_OPEN_WAIT_AUS`] have been passed over. The count restarts on
    /// the AU it admits, so a later re-open waits afresh.
    pub fn admits(&mut self, codec: video_codec::VideoCodec, au: &[u8]) -> bool {
        if codec != video_codec::VideoCodec::H264
            || self.passed_over >= SPS_OPEN_WAIT_AUS
            || carries_h264_sps(au)
        {
            self.passed_over = 0;
            return true;
        }
        self.passed_over += 1;
        false
    }

    /// AUs passed over in the current wait.
    #[cfg(test)]
    pub fn passed_over(&self) -> u32 {
        self.passed_over
    }
}

/// Access units a [`LazyDecoder`] whose open failed passes over before it
/// tries again (1-2 s of video), so a decoder that cannot open is not
/// re-attempted — and logged — on every access unit.
pub const DECODER_REOPEN_BACKOFF_AUS: u32 = 50;

/// A lazily opened decoder and the codec it decodes, for a decode worker fed
/// access units whose codec can change under it (a PMT change, a sniffed
/// codec override, an input switch).
///
/// The codec counts as open only once its open has **succeeded**. The MXL
/// and ST 2110-20 / -23 egress workers used to record the codec before
/// opening: a failed first open left no decoder while the codec said one
/// was open, so the next access unit skipped the open and panicked on
/// `decoder.as_mut().unwrap()`; a failed open on a codec change left the
/// previous codec's decoder in place to be fed the new codec's access
/// units. Here a codec change drops the old decoder before the new one is
/// tried, a failed open leaves none, and the next attempt waits
/// [`DECODER_REOPEN_BACKOFF_AUS`] access units and then the [`SpsOpenGate`]
/// as usual.
pub struct LazyDecoder<D> {
    codec: Option<video_codec::VideoCodec>,
    decoder: Option<D>,
    gate: SpsOpenGate,
    backoff: u32,
}

/// What [`LazyDecoder::decoder_for`] has for an access unit.
pub enum DecoderFor<'a, D, E> {
    /// The decoder open for the access unit's codec; `fresh` on the access
    /// unit that opened it (the caller resets its per-decoder state).
    Ready { decoder: &'a mut D, fresh: bool },
    /// No decoder for this access unit: an H.264 open is waiting for an SPS,
    /// or a failed open is backing off.
    Waiting,
    /// The open this access unit triggered failed; no decoder is open.
    Failed(E),
}

impl<D> Default for LazyDecoder<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D> LazyDecoder<D> {
    pub fn new() -> Self {
        Self { codec: None, decoder: None, gate: SpsOpenGate::new(), backoff: 0 }
    }

    /// The decoder for an access unit `au` (Annex B) of `codec`. When none
    /// is open for `codec`, any other codec's decoder is dropped and — once
    /// the back-off has run out and the SPS gate admits `au` — `open` is
    /// called: seed it from `au`.
    pub fn decoder_for<E>(
        &mut self,
        codec: video_codec::VideoCodec,
        au: &[u8],
        open: impl FnOnce() -> Result<D, E>,
    ) -> DecoderFor<'_, D, E> {
        let mut fresh = false;
        if self.codec != Some(codec) {
            self.codec = None;
            self.decoder = None;
            if self.backoff > 0 {
                self.backoff -= 1;
                return DecoderFor::Waiting;
            }
            if !self.gate.admits(codec, au) {
                return DecoderFor::Waiting;
            }
            match open() {
                Ok(d) => {
                    self.decoder = Some(d);
                    self.codec = Some(codec);
                    fresh = true;
                }
                Err(e) => {
                    self.backoff = DECODER_REOPEN_BACKOFF_AUS;
                    return DecoderFor::Failed(e);
                }
            }
        }
        match self.decoder.as_mut() {
            Some(decoder) => DecoderFor::Ready { decoder, fresh },
            None => DecoderFor::Waiting,
        }
    }

    /// Drop the decoder: the next access unit re-opens one (through the SPS
    /// gate).
    pub fn close(&mut self) {
        self.codec = None;
        self.decoder = None;
    }
}

// ───────────────────── Sample aspect ratio ─────────────────────

/// Largest term the H.264 / HEVC VUI can carry for `sar_width` /
/// `sar_height`.
const SAR_TERM_MAX: u64 = 65_535;

/// Consecutive frames a changed source sample aspect ratio must hold before
/// an open encoder follows it (see `ScaledVideoEncoder::follow_sar`).
#[cfg(feature = "media-codecs")]
const SAR_CHANGE_FRAMES: u32 = 3;

/// The sample aspect ratio to signal on an encode of a `src` picture into
/// `dst`, preserving the source's **display** aspect ratio.
///
/// Only a ratio the source actually signalled is carried: `None`
/// (unspecified) stays `None`, scaled or not — exactly what every encode
/// signalled before, so an unspecified source is unchanged. It is not taken
/// as square: a raw SD capture (SDI, ST 2110) signals nothing and is
/// anamorphic, so 720x576 scaled to 1920x1080 "as if square" signalled
/// 45:64 and displayed a 16:9 picture at 5:4. Unscaled, a signalled SAR is
/// the source's own; scaled, `out = src_sar × (src_w × dst_h) / (src_h ×
/// dst_w)`, reduced. A term that will not fit the 16-bit VUI fields is
/// approximated (best rational with both terms ≤ 65535), never dropped.
///
/// `src` is the geometry the source's SAR describes — the frame, which for
/// a single-field source is twice the height of each picture (see
/// [`sar_geometry`]).
///
/// 720x576 at 64:45 (16:9 anamorphic SD) unscaled stays 64:45 — it used to
/// leave SAR-less and display at 5:4 — and scaled to 1024x576 becomes 1:1;
/// 1920x1080 1:1 to 720x576 becomes 64:45.
pub fn output_sar(
    src_sar: Option<(u32, u32)>,
    (src_w, src_h): (u32, u32),
    (dst_w, dst_h): (u32, u32),
) -> Option<(u32, u32)> {
    let (sn, sd) = src_sar.filter(|(n, d)| *n > 0 && *d > 0)?;
    if (src_w, src_h) == (dst_w, dst_h) || src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Some(bounded_ratio(sn as u64, sd as u64));
    }
    let num = sn as u64 * src_w as u64 * dst_h as u64;
    let den = sd as u64 * src_h as u64 * dst_w as u64;
    Some(bounded_ratio(num, den))
}

/// The geometry a decoded picture's sample aspect ratio describes: the
/// picture itself, except a single field (an HEVC field_seq decode, one
/// 1920x540 field per picture) whose SAR is the frame's — a 1080i service
/// signals 1:1 on its 540-line fields. Read per field, a 1920x540 1:1
/// source displayed at 32:9; as the frame it is 16:9, so an unscaled
/// 540-line output signals 1:2 and one scaled to 1920x1080 signals 1:1.
#[cfg(feature = "media-codecs")]
pub fn sar_geometry((w, h): (u32, u32), scan: SourceScan) -> (u32, u32) {
    match scan {
        SourceScan::SingleField => (w, h.saturating_mul(2)),
        SourceScan::Progressive | SourceScan::Woven(_) => (w, h),
    }
}

/// `num / den` reduced, and if either term still exceeds
/// [`SAR_TERM_MAX`], the closest fraction whose terms both fit
/// (continued-fraction convergents / semiconvergents).
fn bounded_ratio(num: u64, den: u64) -> (u32, u32) {
    let g = gcd(num, den);
    let (num, den) = (num / g, den / g);
    if num <= SAR_TERM_MAX && den <= SAR_TERM_MAX {
        return (num as u32, den as u32);
    }
    // Best approximation with both terms bounded: walk the continued
    // fraction, keeping the last convergent inside the bound, then try the
    // best semiconvergent past it.
    let (mut p0, mut q0, mut p1, mut q1) = (0u64, 1u64, 1u64, 0u64);
    let (mut n, mut d) = (num, den);
    while d != 0 {
        let a = n / d;
        let (p2, q2) = (a * p1 + p0, a * q1 + q0);
        if p2 > SAR_TERM_MAX || q2 > SAR_TERM_MAX {
            let k_p = if p1 == 0 { u64::MAX } else { (SAR_TERM_MAX - p0) / p1 };
            let k_q = if q1 == 0 { u64::MAX } else { (SAR_TERM_MAX - q0) / q1 };
            let k = k_p.min(k_q);
            let (ps, qs) = (k * p1 + p0, k * q1 + q0);
            let target = num as f64 / den as f64;
            let err = |p: u64, q: u64| (p as f64 / q as f64 - target).abs();
            if qs > 0 && ps > 0 && err(ps, qs) < err(p1, q1) {
                return (ps as u32, qs as u32);
            }
            break;
        }
        (p0, q0, p1, q1) = (p1, q1, p2, q2);
        (n, d) = (d, n % d);
    }
    ((p1.max(1)) as u32, (q1.max(1)) as u32)
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
///
/// **Scan** (`video_encode.scan`, see [`VideoScan`]) is settled at the same
/// lazy-open, from the frame in hand: whether it is a woven interlaced
/// frame and in which field order, whether the output is scaled
/// vertically, and which backend in the chain opens with field coding
/// (see [`field_coding_plan`] / [`open_attempts`]). A field-coded encoder
/// follows the source's field order frame by frame, and scales each field
/// on its own and weaves them back — a woven frame is never scaled
/// vertically as one picture, which would blend its two fields.
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
    /// Source sample aspect ratio to use when a decoded frame carries
    /// none — the decoder context's, set by the call site. See
    /// [`Self::set_source_sar_fallback`].
    sar_fallback: Option<(u32, u32)>,
    /// Sample aspect ratio the open encoder signals, and a different one
    /// the source has held for fewer than [`SAR_CHANGE_FRAMES`] frames.
    /// See [`Self::follow_sar`].
    sar_signalled: Option<(u32, u32)>,
    sar_pending: Option<(Option<(u32, u32)>, u32)>,
    /// The backend fixed its ratio at open (or refused a change): the
    /// pipeline stops asking, having said so once.
    sar_fixed: bool,
    /// `scan: auto` may field-code on this pipeline (a TS output's
    /// re-encode). See [`Self::allow_auto_field_coding`].
    auto_field_coding: bool,
    /// The call site's decoder weaves the two fields of an interlaced
    /// picture into one frame (H.264, MPEG-2) rather than handing out one
    /// field per picture (HEVC field_seq). See [`Self::set_source_codec`].
    source_weaves_fields: bool,
    /// Field order the open encoder codes, `None` = progressive.
    field_order: Option<VideoFieldOrder>,
    /// The scaler (when there is one) works on single fields: each field
    /// is scaled on its own and the two are woven back into `weave_buf`.
    field_split: bool,
    weave_buf: [Vec<u8>; 3],
    /// Why an explicit `scan: interlaced` could not be honoured, for the
    /// call site to raise as an event. See [`Self::take_interlace_notice`].
    interlace_notice: Option<String>,
}

/// What the frame an encoder opens on says about its fields.
#[cfg(feature = "media-codecs")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceScan {
    Progressive,
    /// Both fields woven into one frame (an interlaced H.264 / MPEG-2
    /// decode), temporally first field as given.
    Woven(VideoFieldOrder),
    /// One field per picture (an HEVC field_seq decode): half-height
    /// pictures at the field rate.
    SingleField,
}

/// The field order to open the encoder with, whether that was asked for
/// explicitly (`scan: interlaced`, which falls through the whole chain for
/// a backend that can before giving up) — and, when an explicit request
/// cannot be met at all, why.
#[cfg(feature = "media-codecs")]
pub fn field_coding_plan(
    scan: VideoScan,
    source: SourceScan,
    auto_allowed: bool,
    vertical_scaling: bool,
) -> (Option<VideoFieldOrder>, bool, Option<&'static str>) {
    match scan {
        VideoScan::Progressive => (None, false, None),
        VideoScan::Auto => match source {
            SourceScan::Woven(order) if auto_allowed && !vertical_scaling => {
                (Some(order), false, None)
            }
            _ => (None, false, None),
        },
        VideoScan::Interlaced => match source {
            SourceScan::Woven(order) => (Some(order), true, None),
            // Progressive content carried as interlaced: both fields from
            // one instant, top first.
            SourceScan::Progressive => (Some(VideoFieldOrder::Tff), true, None),
            SourceScan::SingleField => (
                None,
                false,
                Some(
                    "the source's decoder hands out one field per picture (HEVC field_seq), \
                     and weaving fields back into frames is not supported",
                ),
            ),
        },
    }
}

/// Every `(backend, field order)` open to try, in order. Progressive: the
/// chain as given. `auto` wanting fields: each backend that can code fields
/// is tried interlaced and then, if the host refuses (h264_qsv on a GPU
/// without field encode), progressive — `auto` follows the backend the
/// resolver lands on and never demotes to another backend to get fields.
/// `interlaced`: every backend that can code fields, in chain order, then
/// the whole chain progressive as the last resort.
#[cfg(feature = "media-codecs")]
pub fn open_attempts(
    chain: &[VideoEncoderCodec],
    field_order: Option<VideoFieldOrder>,
    explicit: bool,
) -> Vec<(VideoEncoderCodec, Option<VideoFieldOrder>)> {
    let Some(order) = field_order else {
        return chain.iter().map(|&c| (c, None)).collect();
    };
    let mut out = Vec::new();
    if explicit {
        out.extend(chain.iter().filter(|c| c.supports_field_coding()).map(|&c| (c, Some(order))));
        out.extend(chain.iter().map(|&c| (c, None)));
    } else {
        for &c in chain {
            if c.supports_field_coding() {
                out.push((c, Some(order)));
            }
            out.push((c, None));
        }
    }
    out
}

/// A decoded (sysmem) frame's planes as `(bytes, stride)`: three for a
/// planar layout, two (luma, interleaved chroma) for NV12 / NV16 / P010 /
/// P210.
#[cfg(feature = "media-codecs")]
fn frame_planes(f: &video_engine::DecodedFrame) -> Option<Vec<(&[u8], usize)>> {
    if let Some((y, ys, u, us, v, vs)) = f.yuv_planes() {
        return Some(vec![(y, ys), (u, us), (v, vs)]);
    }
    if let Some((y, ys, uv, uvs)) = f.nv12_planes().or_else(|| f.nv16_planes()) {
        return Some(vec![(y, ys), (uv, uvs)]);
    }
    if let Some((y, ys, uv, uvs, _)) = f.p01x_planes().or_else(|| f.p21x_planes()) {
        return Some(vec![(y, ys), (uv, uvs)]);
    }
    None
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
            sar_fallback: None,
            sar_signalled: None,
            sar_pending: None,
            sar_fixed: false,
            auto_field_coding: false,
            source_weaves_fields: false,
            field_order: None,
            field_split: false,
            weave_buf: [Vec::new(), Vec::new(), Vec::new()],
            interlace_notice: None,
        }
    }

    /// Let `video_encode.scan: auto` field-code an interlaced source on
    /// this pipeline. A TS output's re-encode calls it — its audience is
    /// broadcast receivers, which display interlace natively; RTMP, WebRTC,
    /// CMAF (mostly progressive displays) and the TS ingress transcoder
    /// (whose output feeds browser-facing passthrough outputs too) leave
    /// `auto` progressive. Only read at lazy-open.
    pub fn allow_auto_field_coding(&mut self) {
        self.auto_field_coding = true;
    }

    /// The codec the caller decodes. An interlaced-flagged frame is a woven
    /// frame (both fields) from an H.264 or MPEG-2 decoder, but a single
    /// field from an HEVC field_seq decoder, which must not be field-coded
    /// as if it were a frame. Until this is called no frame counts as
    /// woven. Only read at lazy-open.
    pub fn set_source_codec(&mut self, codec: video_codec::VideoCodec) {
        self.source_weaves_fields = codec != video_codec::VideoCodec::Hevc;
    }

    /// The field order the open encoder codes (`None` = progressive, or
    /// not open yet). Test-only (the encoder tests need libx264), like
    /// [`Self::is_pts_90k`].
    #[cfg(all(test, feature = "video-encoder-x264"))]
    pub fn field_order(&self) -> Option<VideoFieldOrder> {
        self.field_order
    }

    /// Why an explicit `scan: interlaced` fell back to progressive, once —
    /// for the call site to raise `video_encode_interlace_unavailable`.
    pub fn take_interlace_notice(&mut self) -> Option<String> {
        self.interlace_notice.take()
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

    /// Whether this pipeline has declared 90 kHz PTS. Exposed so call sites
    /// that owe the declaration can assert it in a unit test — a missing
    /// `set_pts_90k()` does not fail loudly (it silently mis-scales rate
    /// control), so it needs a regression guard rather than runtime detection.
    ///
    /// Test-only: nothing in the shipping binary reads it, and leaving it
    /// unconditional trips the crate's dead-code warning.
    #[cfg(test)]
    pub fn is_pts_90k(&self) -> bool {
        self.pts_90k
    }

    /// The sample aspect ratio the decoder parsed from the bitstream
    /// (`VideoDecoder::sample_aspect_ratio`), used for a frame that carries
    /// none. Call it per frame (or at least whenever the decoder changes):
    /// it is read for every frame encoded.
    pub fn set_source_sar_fallback(&mut self, sar: Option<(u32, u32)>) {
        self.sar_fallback = sar;
    }

    /// Plumb a [`ResolvedBackendCell`] that the encoder writes to on
    /// successful lazy-open. Call sites that surface a backend label
    /// in stats use this so the manager-UI badge tracks the actually-
    /// opened backend rather than the requested one (Auto-chain
    /// demotion would otherwise show a stale label), and so
    /// `video_encode_stats.coded_scan` reports the scan it codes (the one
    /// it opened with, its field order then following the source).
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

    /// The rate the encoder opens (or opened) at, `(num, den)`.
    pub fn fps(&self) -> (u32, u32) {
        (self.fps_num, self.fps_den)
    }

    /// Set the GOP the encoder opens with when the operator set none — for a
    /// caller whose default depends on the rate it only learns at the lock
    /// (CMAF: one segment's worth). A no-op once the encoder is open or when
    /// `video_encode.gop_size` is set.
    pub fn set_default_gop_if_unopened(&mut self, gop: u32) -> bool {
        if self.encoder.is_some() || self.encode_cfg.gop_size.is_some() {
            return false;
        }
        self.encode_cfg.gop_size = Some(gop);
        true
    }

    /// The GOP the encoder opens (or opened) with, when one is set; `None`
    /// is `build_encoder_config`'s default of two seconds' worth.
    pub fn gop_size(&self) -> Option<u32> {
        self.encode_cfg.gop_size
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

        let source_scan = if !frame_ref.is_interlaced() {
            SourceScan::Progressive
        } else if self.source_weaves_fields {
            SourceScan::Woven(VideoFieldOrder::from_top_field_first(frame_ref.top_field_first()))
        } else {
            SourceScan::SingleField
        };
        let src_sar = frame_ref.sample_aspect_ratio().or(self.sar_fallback);
        if self.encoder.is_none() {
            self.lazy_open(src_w, src_h, src_pix_fmt, src_sar, source_scan)?;
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
        // Follow the source's sample aspect ratio: an in-band aspect
        // change (an SD service's 16:9 programme and 4:3 insert) or an
        // input switch to a source of another shape or size.
        let want = output_sar(
            src_sar,
            sar_geometry((src_w, src_h), source_scan),
            (self.dst_w, self.dst_h),
        );
        self.follow_sar(want);

        let enc = self.encoder.as_mut().unwrap();

        // Follow the source's field order frame by frame (a switch from a
        // TFF feed to a BFF one); a progressive-flagged frame keeps the
        // last order. Progressive vs interlaced is fixed at open.
        if let (Some(current), SourceScan::Woven(order)) = (self.field_order, source_scan)
            && current != order
        {
            enc.set_frame_field_order(order)
                .map_err(|e| format!("encoder field order change failed: {e}"))?;
            self.field_order = Some(order);
            // The stream is coded the other way from here on:
            // `video_encode_stats.coded_scan` says so.
            if let Some(sink) = &self.resolved_backend_sink {
                sink.store_field_order(order);
            }
        }

        if self.field_split
            && let Some(scaler) = self.scaler.as_ref()
        {
            // Scale each field on its own and weave them back: scaling the
            // woven frame would filter across both fields.
            let planes =
                frame_planes(frame_ref).ok_or_else(|| "decoded frame has no planes".to_string())?;
            let chroma = resolve_chroma(self.encode_cfg.chroma.as_deref());
            let bps = if self.encode_cfg.bit_depth.unwrap_or(8) > 8 { 2 } else { 1 };
            let (dw, dh) = (self.dst_w as usize, self.dst_h as usize);
            let (cw, ch) = match chroma {
                VideoChroma::Yuv420 => (dw / 2, dh / 2),
                VideoChroma::Yuv422 => (dw / 2, dh),
                VideoChroma::Yuv444 => (dw, dh),
            };
            let geom = [(dw * bps, dh), (cw * bps, ch), (cw * bps, ch)];
            for (buf, (row, rows)) in self.weave_buf.iter_mut().zip(geom) {
                buf.resize(row * rows, 0);
            }
            for field in 0..2 {
                let view = |i: usize| {
                    let (p, stride) = planes[i.min(planes.len() - 1)];
                    (&p[(field * stride).min(p.len())..], stride * 2)
                };
                let ((y, ys), (u, us), (v, vs)) = (view(0), view(1), view(2));
                let scaled = scaler
                    .scale_raw_planes(src_w, src_h / 2, src_pix_fmt, y, ys, u, us, v, vs)
                    .map_err(|e| format!("field scaler failed: {e}"))?;
                for (i, (row, rows)) in geom.into_iter().enumerate() {
                    let (sp, ss) = scaled
                        .plane(i)
                        .ok_or_else(|| format!("scaled field missing plane {i}"))?;
                    let dst = &mut self.weave_buf[i];
                    for r in 0..rows / 2 {
                        let d = (2 * r + field) * row;
                        dst[d..d + row].copy_from_slice(&sp[r * ss..r * ss + row]);
                    }
                }
            }
            let [y, u, v] = &self.weave_buf;
            return enc
                .encode_frame(y, geom[0].0, u, geom[1].0, v, geom[2].0, pts)
                .map_err(|e| format!("encoder encode_frame failed: {e}"));
        }

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
            // Raw planes carry no sample aspect ratio (an SD capture is
            // anamorphic, not square: none is signalled, as before) and no
            // field order (progressive to `scan: auto`).
            self.lazy_open(src_w, src_h, src_pix_fmt, None, SourceScan::Progressive)?;
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

    fn lazy_open(
        &mut self,
        src_w: u32,
        src_h: u32,
        src_pix_fmt: i32,
        src_sar: Option<(u32, u32)>,
        source_scan: SourceScan,
    ) -> Result<(), String> {
        if self.backend_chain.is_empty() {
            return Err(
                "encoder open failed: backend chain is empty (no candidates passed by the resolver)"
                    .into(),
            );
        }

        let scan = self.encode_cfg.scan.unwrap_or_default();
        let vertical_scaling = self.encode_cfg.height.is_some_and(|h| h != src_h);
        let (want, explicit, refusal) =
            field_coding_plan(scan, source_scan, self.auto_field_coding, vertical_scaling);
        if let Some(why) = refusal {
            self.note_interlace_unavailable(why.to_string());
        }
        let attempts = open_attempts(&self.backend_chain, want, explicit);

        let mut last_err = String::new();
        let mut field_refusal = String::new();
        let total = attempts.len();
        for (idx, &(candidate, field_order)) in attempts.iter().enumerate() {
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
            // Keep the source's display shape: its own SAR unscaled, the
            // DAR-preserving one scaled. Nothing set this before, so an
            // anamorphic source (720x576 16:9 = 64:45) left SAR-less and
            // displayed squeezed.
            enc_cfg.sample_aspect_ratio = output_sar(
                src_sar,
                sar_geometry((src_w, src_h), source_scan),
                (enc_cfg.width, enc_cfg.height),
            );
            enc_cfg.field_order = field_order;
            if self.pts_90k {
                enc_cfg.time_base_num = 1;
                enc_cfg.time_base_den = 90_000;
            }
            let dst_w = enc_cfg.width;
            let dst_h = enc_cfg.height;
            let scan_label = match field_order {
                Some(VideoFieldOrder::Tff) => "interlaced (fields, top first)",
                Some(VideoFieldOrder::Bff) => "interlaced (fields, bottom first)",
                None => "progressive",
            };
            match video_engine::VideoEncoder::open(&enc_cfg) {
                Ok(encoder) => {
                    let fell_back_for_fields = idx > 0
                        && attempts[idx - 1].0 == candidate
                        && attempts[idx - 1].1.is_some()
                        && field_order.is_none();
                    if fell_back_for_fields && !explicit {
                        // `auto`: the backend the resolver landed on cannot
                        // code fields on this host — progressive, on it.
                        tracing::info!(
                            "{}: {} cannot code interlaced on this host ({field_refusal}); \
                             encoding progressive",
                            self.log_tag,
                            candidate.ffmpeg_name(),
                        );
                    } else if idx > 0 {
                        // We fell through at least one attempt in the
                        // chain. Surface the demote loudly so the
                        // operator can see in the field that QSV /
                        // NVENC went sideways and we landed on the
                        // fallback — matches the `display_atomic_unavailable`
                        // pattern on the display output.
                        tracing::warn!(
                            "{}: video_encode resolver demoted to {} ({scan_label}) after {} failed open(s); reason: {}",
                            self.log_tag,
                            candidate.ffmpeg_name(),
                            idx,
                            last_err,
                        );
                    }
                    if explicit && field_order.is_none() {
                        self.note_interlace_unavailable(format!(
                            "no backend in the chain could open for field coding on this host \
                             (last refusal: {field_refusal})"
                        ));
                    }
                    tracing::info!(
                        "{}: video_encode opened with {}, {scan_label}, {}x{} at {}/{}{}",
                        self.log_tag,
                        candidate.ffmpeg_name(),
                        dst_w,
                        dst_h,
                        self.fps_num,
                        self.fps_den,
                        match enc_cfg.sample_aspect_ratio {
                            Some((n, d)) => format!(", SAR {n}:{d}"),
                            None => String::new(),
                        },
                    );
                    self.sar_signalled = encoder.sample_aspect_ratio();
                    self.sar_pending = None;
                    self.encoder = Some(encoder);
                    if let Some(sink) = &self.resolved_backend_sink {
                        sink.store(candidate, field_order);
                    }
                    self.field_order = field_order;
                    // Only a woven source has two fields to keep apart; a
                    // progressive picture coded as fields scales whole.
                    self.field_split =
                        field_order.is_some() && matches!(source_scan, SourceScan::Woven(_));
                    self.src_w = src_w;
                    self.src_h = src_h;
                    self.src_pix_fmt = src_pix_fmt;
                    self.dst_w = dst_w;
                    self.dst_h = dst_h;
                    self.scaler = self.try_build_scaler(src_w, src_h, src_pix_fmt);
                    return Ok(());
                }
                Err(e) => {
                    last_err = format!("{} ({scan_label}) open failed: {e}", candidate.ffmpeg_name());
                    if field_order.is_some() {
                        field_refusal = last_err.clone();
                    }
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
            "encoder open failed: every backend in the resolver chain refused open ({} attempt(s)). Last: {}",
            total, last_err,
        ))
    }

    /// Re-signal the sample aspect ratio when the source's changes: the
    /// ratio fixed at open is wrong for everything after an in-band aspect
    /// change (an SD service switching between a 16:9 programme at 64:45
    /// and a 4:3 insert at 16:15) or an input switch to a source of another
    /// shape. A change is followed once the source has held it for
    /// [`SAR_CHANGE_FRAMES`] frames, so a stray frame does not cost an IDR.
    ///
    /// libx264 follows (`VideoEncoder::set_sample_aspect_ratio`, which
    /// forces an IDR so the new SPS goes out at once); every other backend
    /// fixes the ratio at open, which is logged once and kept. A pipeline
    /// with out-of-band headers (`global_header`: the RTMP sequence header)
    /// keeps its open-time ratio too — the receiver has the SPS already.
    /// libx264 cannot withdraw a ratio, so a source that stops signalling
    /// one after one that did is signalled 1:1, which a receiver reads the
    /// same way as unspecified.
    fn follow_sar(&mut self, want: Option<(u32, u32)>) {
        if self.global_header || self.sar_fixed {
            return;
        }
        let want = want.or(self.sar_signalled.map(|_| (1, 1)));
        if want == self.sar_signalled {
            self.sar_pending = None;
            return;
        }
        let held = match self.sar_pending {
            Some((pending, n)) if pending == want => n + 1,
            _ => 1,
        };
        if held < SAR_CHANGE_FRAMES {
            self.sar_pending = Some((want, held));
            return;
        }
        self.sar_pending = None;
        let Some(enc) = self.encoder.as_mut() else {
            return;
        };
        let show = |sar: Option<(u32, u32)>| match sar {
            Some((n, d)) => format!("{n}:{d}"),
            None => "unspecified".to_string(),
        };
        match enc.set_sample_aspect_ratio(want) {
            Ok(true) => {
                tracing::info!(
                    "{}: source sample aspect ratio changed — signalling {} (was {}) from an IDR",
                    self.log_tag,
                    show(want),
                    show(self.sar_signalled),
                );
                self.sar_signalled = enc.sample_aspect_ratio();
            }
            Ok(false) => {
                tracing::warn!(
                    "{}: source sample aspect ratio changed to {}, but {} fixes it at open — \
                     the output keeps signalling {} until it restarts",
                    self.log_tag,
                    show(want),
                    enc.codec().ffmpeg_name(),
                    show(self.sar_signalled),
                );
                self.sar_fixed = true;
            }
            Err(e) => {
                tracing::warn!(
                    "{}: sample aspect ratio {} refused ({e}); keeping {}",
                    self.log_tag,
                    show(want),
                    show(self.sar_signalled),
                );
                self.sar_fixed = true;
            }
        }
    }

    /// Record (and log) that an explicit `scan: interlaced` is coding
    /// progressive; the call site raises it as an event.
    fn note_interlace_unavailable(&mut self, why: String) {
        tracing::warn!(
            error_code = "video_encode_interlace_unavailable",
            "{}: video_encode.scan=interlaced cannot be honoured — {why}; encoding progressive",
            self.log_tag,
        );
        self.interlace_notice = Some(why);
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
        // A field-coded woven source is scaled one field at a time.
        let (src_h, dst_h) = if self.field_split {
            (src_h / 2, self.dst_h / 2)
        } else {
            (src_h, self.dst_h)
        };
        let Some(dst_fmt) = select_scaler_dst_format(chroma, bit_depth) else {
            tracing::warn!(
                "{}: video_encode target {:?} {}-bit is not supported by VideoScaler; \
                 encoder will crop instead of scaling (source {}x{} -> requested {}x{})",
                self.log_tag, chroma, bit_depth, src_w, src_h, self.dst_w, dst_h,
            );
            return None;
        };
        match video_engine::VideoScaler::new_with_dst_format(
            src_w, src_h, src_pix_fmt, self.dst_w, dst_h, dst_fmt,
        ) {
            Ok(s) => {
                tracing::info!(
                    "{}: scaling {}x{}(pix_fmt={}) -> {}x{} ({:?}){}",
                    self.log_tag, src_w, src_h, src_pix_fmt, self.dst_w, dst_h, dst_fmt,
                    if self.field_split { " per field" } else { "" },
                );
                Some(s)
            }
            Err(e) => {
                tracing::warn!(
                    "{}: failed to build VideoScaler for {}x{} -> {}x{}: {e}; \
                     encoder will crop instead of scaling",
                    self.log_tag, src_w, src_h, self.dst_w, dst_h,
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

#[cfg(test)]
mod cadence_tests {
    use super::{rate_from_frame_duration, FrameCadence};

    fn meter(steps: impl IntoIterator<Item = u64>, start: u64) -> FrameCadence {
        let mut m = FrameCadence::new();
        let mut pts = start;
        m.observe(Some(pts as i64));
        for s in steps {
            pts = (pts + s) & ((1u64 << 33) - 1);
            m.observe(Some(pts as i64));
        }
        m
    }

    /// The PAFF case: one PES per field (DTS step 1800) but the decoder
    /// hands out one woven frame per field pair, 3600 ticks apart. The meter
    /// sees only the frames, so it says 25, not 50.
    #[test]
    fn woven_field_pairs_measure_the_frame_rate() {
        assert_eq!(meter([3_600; 4], 900_000).rate(), Some((25, 1)));
        // Four deltas are the minimum: three say nothing yet.
        assert_eq!(meter([3_600; 3], 900_000).rate(), None);
    }

    #[test]
    fn standard_rates_snap() {
        assert_eq!(meter([3_003; 6], 0).rate(), Some((30_000, 1001)));
        assert_eq!(meter([1_800; 6], 0).rate(), Some((50, 1)), "HEVC field pictures");
        assert_eq!(meter([1_501, 1_502, 1_501, 1_502, 1_501], 0).rate(), Some((60_000, 1001)));
        assert_eq!(meter([3_753, 3_754, 3_754, 3_753], 0).rate(), Some((24_000, 1001)));
        assert_eq!(meter([7_200; 5], 0).rate(), Some((25, 2)));
    }

    /// Soft-telecined film: decoded frames alternate a 3-field and a
    /// 2-field display duration. The first delta alone would say 19.98 or
    /// 29.97 (the old DTS lock's failure); the 4-delta windows say 23.976.
    #[test]
    fn pulldown_cadences_measure_the_film_rate() {
        let three_two = [3_003u64, 4_505].iter().copied().cycle().take(13);
        let m = meter(three_two, 1_000);
        assert_eq!(m.rate(), Some((24_000, 1001)));
        let two_three_three_two = [3_003u64, 4_505, 4_505, 3_003].iter().copied().cycle().take(13);
        assert_eq!(meter(two_three_three_two, 1_000).rate(), Some((24_000, 1001)));
        // Not enough of a pulldown cadence to call it yet.
        let short = [3_003u64, 4_505].iter().copied().cycle().take(11);
        assert_eq!(meter(short, 1_000).rate(), None);
    }

    /// One dropped frame (a 7200 step) inside a 25 fps run moves only the
    /// windows that contain it.
    #[test]
    fn a_single_gap_does_not_move_the_rate() {
        let mut steps = vec![3_600u64; 6];
        steps.push(7_200);
        steps.extend([3_600u64; 2]);
        // The gap is inside the last four deltas: the fast path declines,
        // the cadence path does not have 12 deltas yet.
        assert_eq!(meter(steps.clone(), 0).rate(), None);
        // Twelve deltas with the gap still among the last four: the
        // cadence path's median ignores the two windows that hold it.
        let mut steps = vec![3_600u64; 10];
        steps.extend([7_200, 3_600]);
        let m = meter(steps.clone(), 0);
        assert_eq!(m.frame_duration_90k(), Some(3_600.0));
        assert_eq!(m.rate(), Some((25, 1)));
        // ...and once past it, four agreeing deltas answer at once.
        steps.extend([3_600u64; 3]);
        assert_eq!(meter(steps, 0).rate(), Some((25, 1)));
    }

    /// A splice many frames long — a media-player loop, whose step the
    /// audio's whole frames set, not the video's — is no cadence: 770_H
    /// program 4030 at 50 fps stepped 33 840 ticks (18.8 frames) across its
    /// loop, the cadence path read it as 19 frames, and the meter said
    /// 50.083 fps: a `video_encode_fps_mismatch` warning on every output at
    /// the first loop. The step is left out; the frames either side of it
    /// measure the rate, as before it.
    #[test]
    fn a_splice_many_frames_long_is_not_cadence() {
        for gap in [33_840u64, 32_400, 7_700, 180_000] {
            let mut m = meter([1_800u64; 30], 0);
            let mut pts = 30 * 1_800 + gap;
            m.observe(Some(pts as i64));
            for k in 0..40 {
                assert_eq!(m.rate(), Some((50, 1)), "gap {gap}, frame {k} after it");
                pts += 1_800;
                m.observe(Some(pts as i64));
            }
        }
        // A frame or two dropped still counts as the frames it covers.
        let mut steps = vec![1_800u64; 20];
        steps.push(5_400);
        steps.extend([1_800u64; 3]);
        assert_eq!(meter(steps, 0).rate(), Some((50, 1)));
        // Steps that stay long are the cadence changing: a 10 fps slate
        // after 50 fps video measures 10 fps.
        let mut steps = vec![1_800u64; 30];
        steps.extend([9_000u64; 6]);
        assert_eq!(meter(steps, 0).rate(), Some((10, 1)));
    }

    /// A source that stamps a PTS on every 12th picture only (29.97 fps):
    /// the span between two stamped frames covers the twelve frames decoded
    /// across it — 3003 ticks a frame, not one 36 036-tick frame (which
    /// measured 2500/1001 fps).
    #[test]
    fn a_span_over_unstamped_frames_is_divided_by_its_frames() {
        let sparse = |every: u64, step: u64, frames: u64| {
            let mut m = FrameCadence::new();
            for n in 0..frames {
                m.observe((n % every == 0).then_some((900_000 + n * step) as i64));
            }
            m
        };
        let m = sparse(12, 3_003, 49);
        assert_eq!(m.deltas_seen(), 4);
        assert_eq!(m.rate(), Some((30_000, 1001)));
        // Every other picture at 59.94 (1501.5 a frame) and at 25 fps.
        assert_eq!(sparse(2, 1_501, 11).rate(), Some((60_000, 1001)));
        assert_eq!(sparse(2, 3_600, 11).rate(), Some((25, 1)));
        // PTS on the I pictures of a 2 s GOP only: 180 000 ticks a span,
        // past a one-frame delta's ceiling, 3600 a frame.
        assert_eq!(sparse(50, 3_600, 201).rate(), Some((25, 1)));
        // Unstamped frames before the first stamped one measure nothing.
        let mut m = FrameCadence::new();
        for _ in 0..5 {
            m.observe(None);
        }
        m.observe(Some(0));
        m.observe(Some(3_600));
        assert_eq!(m.frame_duration_90k(), None);
        assert_eq!(m.deltas_seen(), 1);
        for p in [7_200, 10_800, 14_400] {
            m.observe(Some(p));
        }
        assert_eq!(m.frame_duration_90k(), Some(3_600.0));
    }

    /// A source stamping only every Nth picture — its I pictures — locks
    /// its own rate through the encoder's lock, measured, not the 30/1
    /// fallback, at any GOP up to MPEG-TS's 700 ms between PTS (and a 2 s
    /// one): two stamped spans that agree (each already a mean over its N
    /// frames) are enough, and the lock waits past the first stamp for
    /// them. It needed four spans within 60 frames, so a stamp every 15 or
    /// more pictures fell back to 30/1: VUI 30 fps, a CBR budget 25/30 or
    /// 50/30 off, a 60-frame CMAF GOP.
    #[test]
    fn a_source_stamping_every_nth_picture_locks_its_rate() {
        use super::{EncoderRateLock, RateStep};
        let lock = |(num, den): (u32, u32), every: u64, join: u64| {
            let step = 90_000.0 * den as f64 / num as f64;
            let mut l = EncoderRateLock::new(false);
            for n in 0..400u64 {
                let k = n + join;
                let pts = k.is_multiple_of(every).then(|| (900_000.0 + k as f64 * step).round() as i64);
                if let RateStep::Lock { num, den, measured } = l.observe(pts) {
                    return (num, den, measured, n + 1);
                }
            }
            panic!("never locked");
        };
        for (rate, every, join) in [
            ((25, 1), 15, 0),
            ((25, 1), 25, 0),
            ((25, 1), 25, 9),
            ((30, 1), 30, 0),
            ((50, 1), 25, 3),
            ((60, 1), 42, 41),
            ((60_000, 1001), 42, 1),
            ((30_000, 1001), 12, 0),
            ((25, 1), 50, 0),
        ] {
            let (num, den, measured, at) = lock(rate, every, join);
            assert!(measured, "{rate:?} every {every}: the fallback at frame {at}");
            assert_eq!((num, den), rate, "{rate:?} every {every}");
        }
        // Dense stamps and none at all keep the 60-frame wait.
        let mut m = FrameCadence::new();
        assert_eq!(m.lock_wait_frames(), 60);
        m.observe(Some(0));
        m.observe(Some(3_600));
        assert_eq!(m.lock_wait_frames(), 60);
        // A source that stamps once and never again waits a bounded while.
        let mut m = FrameCadence::new();
        m.observe(Some(0));
        for _ in 0..500 {
            m.observe(None);
        }
        assert_eq!(m.lock_wait_frames(), 1 + 120);
    }

    #[test]
    fn a_wrap_across_2_pow_33_is_a_forward_step() {
        let top = (1u64 << 33) - 5_000;
        let m = meter([3_600; 5], top);
        assert_eq!(m.deltas_seen(), 5);
        assert_eq!(m.rate(), Some((25, 1)));
    }

    /// Constant, zero, backward and missing timestamps are not evidence.
    #[test]
    fn no_rate_without_real_advancing_timestamps() {
        assert_eq!(meter([0; 20], 90_000).rate(), None, "constant PTS");
        let mut m = FrameCadence::new();
        for _ in 0..60 {
            m.observe(None);
            m.observe(Some(-1));
        }
        assert_eq!(m.rate(), None, "NOPTS frames");
        assert_eq!(m.deltas_seen(), 0);
        // A step back or one over a second is a discontinuity, skipped.
        let mut m = meter([3_600; 3], 900_000);
        m.observe(Some(10));
        m.observe(Some(10 + 200_000));
        assert_eq!(m.deltas_seen(), 3);
        assert_eq!(m.rate(), None);
        m.reset();
        assert_eq!(m.deltas_seen(), 0);
    }

    /// Millisecond timestamps (an RTMP publish: `round(k * 1000 / fps)` ms,
    /// or truncated) lock a standard rate at every phase: 30 fps's 33 / 33 /
    /// 34 ms steps measured 2992.5 ticks, an encoder opened at 90000/2993
    /// (30.07 fps); 60 fps opened at 22500/377, 24 fps at 24000/1001. On the
    /// first dozen or so stamps a rate and its 1001 neighbour (0.1 % apart,
    /// a millisecond a second) cannot be told apart and either may lock; a
    /// full window tells them apart.
    #[test]
    fn millisecond_timestamps_lock_a_standard_rate() {
        let stamped = |num: u32, den: u32, deltas: u64, phase: f64, floor: bool| {
            let mut m = FrameCadence::new();
            for k in 0..=deltas {
                let ms = (k as f64 + phase) * 1_000.0 * den as f64 / num as f64;
                let ms = if floor { ms.floor() } else { ms.round() };
                m.observe(Some(ms as i64 * 90));
            }
            m.rate()
        };
        let rates: [((u32, u32), Option<(u32, u32)>); 8] = [
            ((30, 1), Some((30_000, 1001))),
            ((60, 1), Some((60_000, 1001))),
            ((24, 1), Some((24_000, 1001))),
            ((30_000, 1001), Some((30, 1))),
            ((60_000, 1001), Some((60, 1))),
            ((24_000, 1001), Some((24, 1))),
            ((25, 1), None),
            ((50, 1), None),
        ];
        for floor in [false, true] {
            for phase in (0..20).map(|i| i as f64 * 0.05) {
                for (rate, neighbour) in rates {
                    for deltas in [12, 16] {
                        let r = stamped(rate.0, rate.1, deltas, phase, floor).unwrap();
                        assert!(r == rate || Some(r) == neighbour, "{rate:?} x{deltas} +{phase}: {r:?}");
                    }
                    assert_eq!(stamped(rate.0, rate.1, 32, phase, floor), Some(rate), "x32 +{phase}");
                }
            }
        }
    }

    /// Capture timestamps with jitter (a browser's WHIP publish, ±8 ms or
    /// ±2 ms around 25, 30 or 60 fps): the encoder's rate — the lock's
    /// **first** answer, which the encoder opens at for good — is the
    /// source's rate or its 1001 neighbour, never a neighbouring family nor
    /// a rate like 28.9 fps, and it comes within the 60-frame wait. The
    /// noise-widened snap used to answer at the first dozen deltas, whose
    /// endpoint-to-endpoint estimate the jitter moves by up to 4 %, and
    /// snapped within that: 30 fps ±8 ms locked 90000/3113, 90000/3089 or
    /// 6000/193 in 3 runs of 20, 25 fps locked 24/1 (and 24 locked 25/1)
    /// in about 4 %, and 60 fps mostly locked a non-standard rate.
    #[test]
    fn jittered_timestamps_lock_their_own_rate_family() {
        use super::{EncoderRateLock, RateStep};
        let mut seed = 12_345u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f64 / (1u32 << 24) as f64
        };
        for (rate, neighbour, jitter_ms) in [
            ((25, 1), (25, 1), 8.0),
            ((24, 1), (24_000, 1001), 8.0),
            ((30, 1), (30_000, 1001), 8.0),
            ((60, 1), (60_000, 1001), 8.0),
            ((50, 1), (50, 1), 8.0),
            ((30, 1), (30_000, 1001), 2.0),
            ((25, 1), (25, 1), 2.0),
            ((30, 1), (30_000, 1001), 0.5),
            ((60_000, 1001), (60, 1), 8.0),
        ] {
            let step = 90_000.0 * rate.1 as f64 / rate.0 as f64;
            let j = jitter_ms * 90.0;
            for run in 0..400 {
                let mut l = EncoderRateLock::new(false);
                let mut got = None;
                for k in 0..200u64 {
                    let pts = 900_000.0 + k as f64 * step + (next() * 2.0 - 1.0) * j;
                    if let RateStep::Lock { num, den, measured } = l.observe(Some(pts as i64)) {
                        got = Some(((num, den), measured, k + 1));
                        break;
                    }
                }
                let (r, measured, at) = got.unwrap();
                assert!(measured, "{rate:?} ±{jitter_ms} ms run {run}: fallback at {at}");
                assert!(r == rate || r == neighbour, "{rate:?} ±{jitter_ms} ms run {run}: locked {r:?} at {at}");
            }
        }
    }

    /// A stamp that jitter put within a millisecond of the one before (40
    /// ticks, at 60 fps) is a frame, not a discontinuity: it joins the next
    /// span, which covers two frames, and the cadence stays exact. Dropped
    /// with its stamp, it took a frame out of the count and the next span
    /// read as one frame of 2960 ticks.
    #[test]
    fn a_stamp_within_a_millisecond_of_the_last_is_a_frame() {
        let mut m = FrameCadence::new();
        for k in 0..40u64 {
            let pts = if k == 20 { 19 * 1_500 + 40 } else { k * 1_500 };
            m.observe(Some(900_000 + pts as i64));
        }
        assert_eq!(m.frame_duration_90k(), Some(1_500.0));
        assert_eq!(m.rate(), Some((60, 1)));
    }

    /// A rate no standard is within 0.1 % of is reported as measured.
    #[test]
    fn a_non_standard_rate_is_not_forced() {
        assert_eq!(rate_from_frame_duration(7_000.0), (90, 7));
        assert_eq!(meter([6_000; 4], 0).rate(), Some((15, 1)));
        assert_eq!(meter([5_000; 4], 0).rate(), Some((18, 1)));
        // 0.2 % off 25 fps: not snapped.
        assert_eq!(rate_from_frame_duration(3_608.0), (11_250, 451));
    }
}

#[cfg(test)]
mod frame_pts_stamper_tests {
    use super::FramePtsStamper;

    /// A picture without a PTS is stamped from the last one that had one,
    /// a frame per picture since; nothing before the first real PTS.
    #[test]
    fn unstamped_pictures_follow_the_last_stamped_one() {
        let mut s = FramePtsStamper::default();
        assert_eq!(s.stamp(None, 3_600), None);
        assert_eq!(s.stamp(Some(900_000), 3_600), Some(900_000));
        assert_eq!(s.stamp(None, 3_600), Some(903_600));
        assert_eq!(s.stamp(Some(-1), 3_600), Some(907_200), "a negative PTS is none");
        assert_eq!(s.stamp(Some(950_000), 3_600), Some(950_000));
        assert_eq!(s.stamp(None, 1_800), Some(951_800));
        // Across the 33-bit wrap.
        let top = (1i64 << 33) - 1_800;
        assert_eq!(s.stamp(Some(top), 3_600), Some(top as u64));
        assert_eq!(s.stamp(None, 3_600), Some(1_800));
    }

    /// A 50 fps source stamping every 25th picture, on an encoder whose
    /// rate is 30/1 (the fallback, a pin, an earlier input's): once a span
    /// between two real PTS is known, the pictures between step by the
    /// source's frame — 1800 — and the next real PTS lands one frame past
    /// the last derived one. Stepped by the encoder's 3000 they ran 27 000
    /// ticks past it: the timeline stepped back once a GOP.
    #[test]
    fn unstamped_pictures_step_by_the_measured_frame_not_the_encoders() {
        let mut s = FramePtsStamper::default();
        let mut out = Vec::new();
        for n in 0..101u64 {
            let pts = (n % 25 == 0).then_some((900_000 + n * 1_800) as i64);
            out.push(s.stamp(pts, 3_000).unwrap());
        }
        // The first span is stepped by the encoder's rate: nothing measured
        // yet. Every picture after the second real PTS is a frame apart.
        for (n, w) in out.windows(2).enumerate().skip(25) {
            assert_eq!(w[1] - w[0], 1_800, "picture {}", n + 1);
        }
        // An uneven span (59.94: 1501.5 a frame) is spread exactly.
        let mut s = FramePtsStamper::default();
        s.stamp(Some(0), 3_600);
        s.stamp(None, 3_600);
        assert_eq!(s.stamp(Some(3_003), 3_600), Some(3_003));
        assert_eq!(s.stamp(None, 3_600), Some(3_003 + 1_501));
        assert_eq!(s.stamp(None, 3_600), Some(3_003 + 3_003));
        // A span that is no frame (a jump) teaches nothing.
        let mut s = FramePtsStamper::default();
        s.stamp(Some(0), 3_600);
        s.stamp(Some(900_000), 3_600);
        assert_eq!(s.stamp(None, 3_600), Some(903_600));
    }
}

#[cfg(test)]
mod encoded_pts_map_tests {
    use super::EncodedPtsMap;

    /// An encoder with B-frames hands frames back in decode order: each
    /// still finds its own picture's PTS. A frame the encoder dropped is
    /// forgotten once the counters have moved well past it.
    #[test]
    fn frames_handed_back_in_decode_order_keep_their_pts() {
        let mut m = EncodedPtsMap::default();
        for c in 0..9i64 {
            m.push(c, Some(1_000 + c as u64 * 3_600));
        }
        for c in [0i64, 3, 1, 2, 6, 4, 5] {
            assert_eq!(m.take(c), Some(1_000 + c as u64 * 3_600), "counter {c}");
        }
        // 7 never came back; 8 did, and 7 is still held (it may yet)...
        assert_eq!(m.take(8), Some(1_000 + 8 * 3_600));
        assert_eq!(m.in_flight.len(), 1);
        // ...until the encoder is a reorder window past it.
        m.push(60, None);
        assert_eq!(m.take(60), None);
        assert!(m.in_flight.is_empty());
    }
}

#[cfg(test)]
mod sps_gate_tests {
    use super::{SpsOpenGate, SPS_OPEN_WAIT_AUS};
    use video_codec::VideoCodec;

    /// AUD, SPS, PPS, IDR slice.
    const WITH_SPS: &[u8] = &[
        0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 0x42, 0, 0x1E, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 1,
        0x65, 0x88,
    ];
    /// AUD, non-IDR slice.
    const P_PICTURE: &[u8] = &[0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x41, 0x9A];

    #[test]
    fn an_h264_open_waits_for_an_sps_but_not_for_ever() {
        let mut g = SpsOpenGate::new();
        assert!(!g.admits(VideoCodec::H264, P_PICTURE));
        assert!(!g.admits(VideoCodec::H264, P_PICTURE));
        assert_eq!(g.passed_over(), 2);
        assert!(g.admits(VideoCodec::H264, WITH_SPS));
        assert_eq!(g.passed_over(), 0, "a later re-open waits afresh");
        for _ in 0..SPS_OPEN_WAIT_AUS {
            assert!(!g.admits(VideoCodec::H264, P_PICTURE));
        }
        assert!(g.admits(VideoCodec::H264, P_PICTURE), "bounded");
    }

    #[test]
    fn other_codecs_open_at_once() {
        let mut g = SpsOpenGate::new();
        assert!(g.admits(VideoCodec::Hevc, P_PICTURE));
        assert!(g.admits(VideoCodec::Mpeg2, &[0, 0, 1, 0xB3]));
        assert_eq!(g.passed_over(), 0);
    }
}

#[cfg(test)]
mod lazy_decoder_tests {
    use super::{DecoderFor, LazyDecoder, DECODER_REOPEN_BACKOFF_AUS};
    use video_codec::VideoCodec;

    /// A "decoder" that remembers the codec it was opened for.
    #[derive(Debug, PartialEq)]
    struct Fake(VideoCodec);

    /// One access unit through `lazy`; `open_ok` decides what an open does.
    /// Returns the codec of the decoder handed back (with its `fresh` flag),
    /// `Err(true)` for a failed open, `Err(false)` for waiting, and counts
    /// the opens attempted.
    fn feed(
        lazy: &mut LazyDecoder<Fake>,
        codec: VideoCodec,
        open_ok: bool,
        opens: &mut u32,
    ) -> Result<(VideoCodec, bool), bool> {
        let result = lazy.decoder_for(codec, &[0, 0, 1, 0x40], || {
            *opens += 1;
            if open_ok { Ok(Fake(codec)) } else { Err("no decoder") }
        });
        match result {
            DecoderFor::Ready { decoder, fresh } => Ok((decoder.0, fresh)),
            DecoderFor::Failed(_) => Err(true),
            DecoderFor::Waiting => Err(false),
        }
    }

    /// The MXL / ST 2110 egress panic: the first open fails and the next
    /// access unit found the codec recorded as open with no decoder behind
    /// it. Here it waits out the back-off, retries, and opens.
    #[test]
    fn a_failed_open_leaves_no_decoder_and_is_retried_after_the_back_off() {
        let mut lazy = LazyDecoder::new();
        let mut opens = 0;
        assert_eq!(feed(&mut lazy, VideoCodec::Hevc, false, &mut opens), Err(true));
        for _ in 0..DECODER_REOPEN_BACKOFF_AUS {
            assert_eq!(
                feed(&mut lazy, VideoCodec::Hevc, true, &mut opens),
                Err(false),
                "no decoder while the failed open backs off",
            );
        }
        assert_eq!(opens, 1, "not retried on every access unit");
        assert_eq!(feed(&mut lazy, VideoCodec::Hevc, true, &mut opens), Ok((VideoCodec::Hevc, true)));
        assert_eq!(feed(&mut lazy, VideoCodec::Hevc, true, &mut opens), Ok((VideoCodec::Hevc, false)));
        assert_eq!(opens, 2);
    }

    /// A failed open on a codec change must not leave the previous codec's
    /// decoder to be fed the new codec's access units.
    #[test]
    fn a_failed_open_on_a_codec_change_drops_the_old_decoder() {
        let mut lazy = LazyDecoder::new();
        let mut opens = 0;
        assert_eq!(feed(&mut lazy, VideoCodec::Hevc, true, &mut opens), Ok((VideoCodec::Hevc, true)));
        assert_eq!(feed(&mut lazy, VideoCodec::Mpeg2, false, &mut opens), Err(true));
        assert_eq!(feed(&mut lazy, VideoCodec::Mpeg2, true, &mut opens), Err(false));
    }

    /// An H.264 open still waits for an access unit that carries the SPS;
    /// `close` forces a fresh open through the gate.
    #[test]
    fn an_h264_open_waits_for_the_sps_and_close_reopens() {
        const WITH_SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0, 0x1E, 0, 0, 0, 1, 0x65, 0x88];
        const P_PICTURE: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A];
        let mut lazy: LazyDecoder<Fake> = LazyDecoder::new();
        let mut opens = 0;
        let mut open = |au: &[u8], lazy: &mut LazyDecoder<Fake>| {
            match lazy.decoder_for(VideoCodec::H264, au, || {
                opens += 1;
                Ok::<_, ()>(Fake(VideoCodec::H264))
            }) {
                DecoderFor::Ready { fresh, .. } => Some(fresh),
                _ => None,
            }
        };
        assert_eq!(open(P_PICTURE, &mut lazy), None);
        assert_eq!(open(WITH_SPS, &mut lazy), Some(true));
        assert_eq!(open(P_PICTURE, &mut lazy), Some(false));
        lazy.close();
        assert_eq!(open(P_PICTURE, &mut lazy), None, "a re-open waits for the SPS again");
        assert_eq!(open(WITH_SPS, &mut lazy), Some(true));
        assert_eq!(opens, 2);
    }
}

#[cfg(test)]
mod default_gop_tests {
    use super::default_gop_frames;

    /// Two seconds of pictures, rounded: `2 * floor(fps)` gave 58 frames at
    /// 29.97 fps (1.935 s) and 118 at 59.94.
    #[test]
    fn the_default_gop_is_two_seconds_rounded_to_a_frame() {
        assert_eq!(default_gop_frames(30_000, 1001), 60);
        assert_eq!(default_gop_frames(60_000, 1001), 120);
        assert_eq!(default_gop_frames(24_000, 1001), 48);
        assert_eq!(default_gop_frames(25, 1), 50);
        assert_eq!(default_gop_frames(30, 1), 60);
        assert_eq!(default_gop_frames(50, 1), 100);
        assert_eq!(default_gop_frames(1, 1), 2);
        assert_eq!(default_gop_frames(0, 0), 1, "a zero rate still opens a GOP of one");
    }
}

#[cfg(test)]
mod sar_tests {
    use super::{bounded_ratio, output_sar};

    #[test]
    fn unscaled_keeps_the_source_sar_and_unspecified_stays_unspecified() {
        assert_eq!(output_sar(Some((64, 45)), (720, 576), (720, 576)), Some((64, 45)));
        assert_eq!(output_sar(Some((10, 11)), (704, 480), (704, 480)), Some((10, 11)));
        assert_eq!(output_sar(Some((128, 90)), (720, 576), (720, 576)), Some((64, 45)));
        assert_eq!(output_sar(None, (1920, 1080), (1920, 1080)), None);
        assert_eq!(output_sar(Some((0, 1)), (1920, 1080), (1920, 1080)), None);
    }

    #[test]
    fn scaling_preserves_the_display_aspect_ratio() {
        // 16:9 anamorphic SD to square-pixel 16:9.
        assert_eq!(output_sar(Some((64, 45)), (720, 576), (1024, 576)), Some((1, 1)));
        // Square HD to 16:9 anamorphic SD.
        assert_eq!(output_sar(Some((1, 1)), (1920, 1080), (720, 576)), Some((64, 45)));
        // Square stays square.
        assert_eq!(output_sar(Some((1, 1)), (1920, 1080), (1280, 720)), Some((1, 1)));
        // VH1: 528x480 at 40:33 (4:3) widened to 720x480 is 8:9, still 4:3.
        assert_eq!(output_sar(Some((40, 33)), (528, 480), (720, 480)), Some((8, 9)));
    }

    /// An unspecified source is not taken as square: a raw SD capture (SDI,
    /// ST 2110) signals nothing and is anamorphic, and 720x576 upconverted
    /// to 1920x1080 "as if square" signalled 45:64 — a 16:9 picture shown
    /// at 5:4. It stays unspecified, scaled or not, as every encode did.
    #[test]
    fn an_unspecified_source_stays_unspecified_when_scaled() {
        assert_eq!(output_sar(None, (720, 576), (1920, 1080)), None);
        assert_eq!(output_sar(None, (1920, 1080), (720, 576)), None);
        assert_eq!(output_sar(None, (1920, 1080), (1280, 720)), None);
    }

    /// An HEVC field_seq source's SAR describes the frame its fields make:
    /// 1920x540 fields at 1:1 are a 16:9 picture. Read per field they were
    /// 32:9 — and scaled to 1920x1080, "keeping" that shape signalled 2:1.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn a_single_field_source_keeps_its_frame_shape() {
        use super::{sar_geometry, SourceScan};
        let field = sar_geometry((1920, 540), SourceScan::SingleField);
        assert_eq!(field, (1920, 1080));
        assert_eq!(output_sar(Some((1, 1)), field, (1920, 1080)), Some((1, 1)));
        assert_eq!(output_sar(Some((1, 1)), field, (1920, 540)), Some((1, 2)));
        // A woven or progressive picture is its own geometry.
        let woven = sar_geometry((1920, 1080), SourceScan::Woven(video_codec::VideoFieldOrder::Tff));
        assert_eq!(woven, (1920, 1080));
        assert_eq!(sar_geometry((720, 576), SourceScan::Progressive), (720, 576));
    }

    #[test]
    fn a_ratio_past_16_bits_is_approximated_not_dropped() {
        assert_eq!(bounded_ratio(128, 90), (64, 45));
        let (n, d) = bounded_ratio(100_003, 99_991);
        assert!(n <= 65_535 && d <= 65_535, "{n}:{d}");
        let err = (n as f64 / d as f64 - 100_003.0 / 99_991.0).abs();
        assert!(err < 1e-8, "{n}:{d} off by {err}");
        assert_eq!(bounded_ratio(10_000_000, 1), (65_535, 1));
    }
}

#[cfg(all(test, feature = "media-codecs"))]
mod scan_tests {
    use super::{field_coding_plan, open_attempts, ResolvedBackendCell, SourceScan};
    use crate::config::models::VideoScan;
    use video_codec::VideoEncoderCodec::*;
    use video_codec::VideoFieldOrder::{Bff, Tff};

    /// RTMP / CMAF raise `video_encode_interlace_unavailable` with the TS
    /// replacer's shape: output-scoped, category `video_encode`, the
    /// source codec's stream_type.
    #[test]
    fn the_rtmp_and_cmaf_interlace_warning_has_the_ts_shape() {
        let (tx, mut rx) = crate::manager::events::event_channel();
        super::emit_output_interlace_unavailable(
            &tx,
            "RTMP",
            "rtmp-1",
            "no backend could",
            video_codec::VideoCodec::Mpeg2,
        );
        let ev = rx.try_recv().expect("event");
        assert_eq!(ev.output_id.as_deref(), Some("rtmp-1"));
        assert!(ev.flow_id.is_none() && ev.input_id.is_none());
        assert_eq!(ev.severity, crate::manager::events::EventSeverity::Warning);
        assert_eq!(ev.category, crate::manager::events::category::VIDEO_ENCODE);
        assert!(ev.message.starts_with("RTMP output 'rtmp-1': "), "{}", ev.message);
        let d = ev.details.unwrap();
        assert_eq!(d["error_code"], "video_encode_interlace_unavailable");
        assert_eq!(d["reason"], "no backend could");
        assert_eq!(d["source_stream_type"], 0x02);
    }

    /// `coded_scan` is unset until an open, then names the scan of the
    /// latest (re)open — a reopen at another scan overwrites it — or the
    /// field order a field-coded encoder followed to without a reopen,
    /// which leaves the backend alone.
    #[test]
    fn the_resolved_backend_cell_carries_the_coded_scan() {
        let cell = ResolvedBackendCell::default();
        assert_eq!((cell.label(), cell.coded_scan()), (None, None));
        cell.store(H264Nvenc, Some(Bff));
        assert_eq!((cell.label(), cell.coded_scan()), (Some("nvenc"), Some("interlaced_bff")));
        cell.store_field_order(Tff);
        assert_eq!((cell.label(), cell.coded_scan()), (Some("nvenc"), Some("interlaced_tff")));
        cell.store(X264, Some(Tff));
        assert_eq!((cell.label(), cell.coded_scan()), (Some("x264"), Some("interlaced_tff")));
        cell.store_field_order(Bff);
        assert_eq!((cell.label(), cell.coded_scan()), (Some("x264"), Some("interlaced_bff")));
        cell.store(X264, None);
        assert_eq!(cell.coded_scan(), Some("progressive"));
    }

    #[test]
    fn auto_field_codes_only_a_woven_source_on_a_ts_path_unscaled() {
        let woven = SourceScan::Woven(Bff);
        assert_eq!(field_coding_plan(VideoScan::Auto, woven, true, false), (Some(Bff), false, None));
        // Scaled vertically, not a TS path, or not woven: progressive.
        assert_eq!(field_coding_plan(VideoScan::Auto, woven, true, true), (None, false, None));
        assert_eq!(field_coding_plan(VideoScan::Auto, woven, false, false), (None, false, None));
        for src in [SourceScan::Progressive, SourceScan::SingleField] {
            assert_eq!(field_coding_plan(VideoScan::Auto, src, true, false), (None, false, None));
        }
        assert_eq!(
            field_coding_plan(VideoScan::Progressive, woven, true, false),
            (None, false, None)
        );
    }

    #[test]
    fn interlaced_field_codes_whatever_it_can() {
        let woven = SourceScan::Woven(Tff);
        // Scaling does not stop it (the fields are scaled apart), nor a
        // non-TS path.
        assert_eq!(field_coding_plan(VideoScan::Interlaced, woven, false, true), (Some(Tff), true, None));
        assert_eq!(
            field_coding_plan(VideoScan::Interlaced, SourceScan::Progressive, false, false),
            (Some(Tff), true, None)
        );
        // A single field per picture cannot be coded as a frame of two.
        let (order, explicit, why) =
            field_coding_plan(VideoScan::Interlaced, SourceScan::SingleField, true, false);
        assert_eq!((order, explicit), (None, false));
        assert!(why.unwrap().contains("field_seq"));
    }

    #[test]
    fn attempts_follow_the_resolved_backend_for_auto_and_the_whole_chain_for_interlaced() {
        let chain = [H264Qsv, H264Vaapi, X264];
        assert_eq!(
            open_attempts(&chain, None, false),
            vec![(H264Qsv, None), (H264Vaapi, None), (X264, None)]
        );
        // auto: QSV interlaced, then QSV progressive before anything else —
        // never a demotion to libx264 just to get fields.
        assert_eq!(
            open_attempts(&chain, Some(Tff), false),
            vec![
                (H264Qsv, Some(Tff)),
                (H264Qsv, None),
                (H264Vaapi, None),
                (X264, Some(Tff)),
                (X264, None)
            ]
        );
        // interlaced: every backend that can, then progressive as a last
        // resort.
        assert_eq!(
            open_attempts(&chain, Some(Bff), true),
            vec![
                (H264Qsv, Some(Bff)),
                (X264, Some(Bff)),
                (H264Qsv, None),
                (H264Vaapi, None),
                (X264, None)
            ]
        );
        // HEVC cannot code fields at all.
        assert_eq!(open_attempts(&[X265], Some(Tff), false), vec![(X265, None)]);
    }
}

#[cfg(all(test, feature = "video-encoder-x264"))]
mod scan_x264_tests {
    use super::ScaledVideoEncoder;
    use crate::config::models::{VideoEncodeConfig, VideoScan};
    use video_codec::{VideoCodec, VideoEncoderCodec, VideoEncoderConfig, VideoFieldOrder};

    /// Decoded frames of a small MBAFF (top field first) stream.
    fn with_interlaced_frames(f: impl FnMut(&video_engine::DecodedFrame)) {
        with_interlaced_frames_in(VideoFieldOrder::Tff, f);
    }

    /// Decoded frames of a small MBAFF stream coded in `order`.
    fn with_interlaced_frames_in(
        order: VideoFieldOrder,
        mut f: impl FnMut(&video_engine::DecodedFrame),
    ) {
        let (w, h) = (320usize, 240usize);
        let mut enc = video_engine::VideoEncoder::open(&VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: w as u32,
            height: h as u32,
            fps_num: 25,
            fps_den: 1,
            global_header: false,
            field_order: Some(order),
            ..VideoEncoderConfig::default()
        })
        .unwrap();
        let mut dec = video_engine::VideoDecoder::open(VideoCodec::H264).unwrap();
        let y = vec![100u8; w * h];
        let c = vec![128u8; w / 2 * h / 2];
        for i in 0..4 {
            for ef in enc.encode_frame(&y, w, &c, w / 2, &c, w / 2, Some(i)).unwrap() {
                dec.send_packet(&ef.data).unwrap();
                while let Ok(fr) = dec.receive_frame() {
                    f(&fr);
                }
            }
        }
    }

    fn pipeline(scan: VideoScan) -> ScaledVideoEncoder {
        let cfg: VideoEncodeConfig =
            serde_json::from_value(serde_json::json!({ "codec": "x264" })).unwrap();
        let cfg = VideoEncodeConfig { scan: Some(scan), ..cfg };
        ScaledVideoEncoder::new(cfg, VideoEncoderCodec::X264, 25, 1, false, "test")
    }

    /// An HEVC field_seq decoder hands out one field per picture, flagged
    /// interlaced: coding it as a woven frame would be wrong, so an
    /// explicit `interlaced` codes progressive and leaves the reason for
    /// the call site's Warning — once.
    #[test]
    fn interlaced_on_single_field_pictures_codes_progressive_and_says_why() {
        let mut p = pipeline(VideoScan::Interlaced);
        p.set_source_codec(VideoCodec::Hevc);
        with_interlaced_frames(|f| {
            assert!(f.is_interlaced());
            p.encode(f, Some(0)).unwrap();
        });
        assert!(p.is_open());
        assert_eq!(p.field_order(), None);
        assert!(p.take_interlace_notice().unwrap().contains("field_seq"));
        assert_eq!(p.take_interlace_notice(), None);
    }

    /// An SDI 625-line capture (720x576 raw planes, which carry no SAR)
    /// upconverted to 1920x1080 signals no SAR — not the 45:64 a source
    /// taken as square would get, which shows a 16:9 picture at 5:4.
    #[test]
    fn raw_sd_planes_scaled_to_hd_signal_no_sar() {
        let cfg: VideoEncodeConfig = serde_json::from_value(serde_json::json!({
            "codec": "x264", "width": 1920, "height": 1080
        }))
        .unwrap();
        let mut p = ScaledVideoEncoder::new(cfg, VideoEncoderCodec::X264, 25, 1, false, "test");
        let (w, h) = (720usize, 576usize);
        let fmt = video_engine::av_pix_fmt_for_yuv(video_codec::VideoChroma::Yuv420, 8).unwrap();
        let (y, c) = (vec![100u8; w * h], vec![128u8; w / 2 * h / 2]);
        p.encode_raw_planes(720, 576, fmt, &y, w, &c, w / 2, &c, w / 2, Some(0)).unwrap();
        assert_eq!(p.dst_dimensions(), (1920, 1080));
        assert_eq!(p.encoder.as_ref().unwrap().sample_aspect_ratio(), None);
    }

    /// The same frames from an H.264 decoder are woven: field-coded.
    #[test]
    fn woven_frames_are_field_coded_when_asked() {
        let mut p = pipeline(VideoScan::Interlaced);
        p.set_source_codec(VideoCodec::H264);
        with_interlaced_frames(|f| {
            p.encode(f, Some(0)).unwrap();
        });
        assert_eq!(p.field_order(), Some(VideoFieldOrder::Tff));
        assert_eq!(p.take_interlace_notice(), None);
        // `auto` off a TS path (RTMP / WebRTC / CMAF) stays progressive —
        // and the stats cell says what was coded, not what was asked.
        let mut p = pipeline(VideoScan::Auto);
        let cell = std::sync::Arc::new(super::ResolvedBackendCell::default());
        p.set_resolved_backend_sink(cell.clone());
        assert_eq!(cell.coded_scan(), None, "nothing coded before the open");
        p.set_source_codec(VideoCodec::H264);
        with_interlaced_frames(|f| {
            p.encode(f, Some(0)).unwrap();
        });
        assert_eq!(p.field_order(), None);
        assert_eq!(cell.coded_scan(), Some("progressive"));
        assert_eq!(cell.label(), Some("x264"));
        let mut p = pipeline(VideoScan::Auto);
        p.set_source_codec(VideoCodec::H264);
        p.allow_auto_field_coding();
        with_interlaced_frames(|f| {
            p.encode(f, Some(0)).unwrap();
        });
        assert_eq!(p.field_order(), Some(VideoFieldOrder::Tff));
    }

    /// `video_encode_stats.coded_scan` reports a field-coded open with its
    /// field order.
    #[test]
    fn the_stats_cell_reports_a_field_coded_open() {
        let mut p = pipeline(VideoScan::Interlaced);
        let cell = std::sync::Arc::new(super::ResolvedBackendCell::default());
        p.set_resolved_backend_sink(cell.clone());
        p.set_source_codec(VideoCodec::H264);
        with_interlaced_frames(|f| {
            p.encode(f, Some(0)).unwrap();
        });
        assert_eq!(cell.coded_scan(), Some("interlaced_tff"));
    }

    /// An input switch from a TFF source to a BFF one keeps the encoder
    /// open and follows the field order frame by frame — and
    /// `video_encode_stats.coded_scan` follows with it, instead of
    /// reporting the order the encoder opened with.
    #[test]
    fn the_stats_cell_follows_a_tff_to_bff_switch_without_a_reopen() {
        let mut p = pipeline(VideoScan::Interlaced);
        let cell = std::sync::Arc::new(super::ResolvedBackendCell::default());
        p.set_resolved_backend_sink(cell.clone());
        p.set_source_codec(VideoCodec::H264);
        with_interlaced_frames_in(VideoFieldOrder::Tff, |f| {
            assert!(f.top_field_first());
            p.encode(f, Some(0)).unwrap();
        });
        assert_eq!(p.field_order(), Some(VideoFieldOrder::Tff));
        assert_eq!((cell.label(), cell.coded_scan()), (Some("x264"), Some("interlaced_tff")));
        // A sentinel backend: a reopen would rewrite it to x264, the
        // field-order follow leaves it alone.
        cell.store(VideoEncoderCodec::H264Nvenc, Some(VideoFieldOrder::Tff));
        with_interlaced_frames_in(VideoFieldOrder::Bff, |f| {
            assert!(f.is_interlaced() && !f.top_field_first());
            p.encode(f, Some(0)).unwrap();
        });
        assert!(p.is_open());
        assert_eq!(p.field_order(), Some(VideoFieldOrder::Bff));
        assert_eq!((cell.label(), cell.coded_scan()), (Some("nvenc"), Some("interlaced_bff")));
    }
}
