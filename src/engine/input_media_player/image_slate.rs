// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Still-image slate encoder for the file-backed media-player input.
//!
//! Decodes a JPEG / PNG / WebP / BMP / GIF (first-frame) once, builds
//! YUV420P planes via BT.709 RGB→YCbCr conversion, then feeds those
//! planes to the in-process H.264 encoder at a low frame rate so the
//! receiver's decoder timestamp clock keeps advancing without burning
//! CPU. Optionally pairs the video with silent stereo AAC so downstream
//! audio paths see a continuous PCM/AAC carrier rather than glitching
//! whenever the live audio drops out.
//!
//! Used as the broadcast "slate" pattern — operators upload a "we'll
//! be right back" PNG and assign it as the Hitless backup leg of an
//! Assembled flow. When the live primary stalls, the assembler cuts
//! over to this image's TS within 200 ms.

use std::path::Path;

use anyhow::{Result, anyhow};
use bytes::BytesMut;
use tokio::time::{Duration, Instant, MissedTickBehavior, interval_at};

use super::{BUNDLE_SIZE, PlayerSession, emit_bundle};
use crate::config::models::VideoEncodeConfig;
use crate::engine::rtmp::ts_mux::TsMuxer;
use crate::engine::video_encode_util::ScaledVideoEncoder;

const STREAM_TYPE_AAC: u8 = 0x0F;
const STREAM_TYPE_H264: u8 = 0x1B;
const AV_PIX_FMT_YUV420P: i32 = 0;

pub async fn play_image_file(
    path: &Path,
    fps: u8,
    bitrate_kbps: u32,
    audio_silence: bool,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    let path_owned = path.to_path_buf();
    let decoded: DecodedImage = tokio::task::spawn_blocking(move || decode_to_yuv420p(&path_owned))
        .await
        .map_err(|e| anyhow!("image decode join failed: {e}"))??;

    encode_loop(decoded, fps, bitrate_kbps, audio_silence, session).await
}

struct DecodedImage {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    y_stride: usize,
    uv_stride: usize,
}

fn decode_to_yuv420p(path: &Path) -> Result<DecodedImage> {
    let img = image::open(path).map_err(|e| anyhow!("decode {}: {e}", path.display()))?;
    // Force even dimensions — both H.264 chroma subsampling and YUV420P
    // require width/height divisible by 2.
    let mut rgb = img.to_rgb8();
    if rgb.width() % 2 != 0 || rgb.height() % 2 != 0 {
        let new_w = rgb.width() & !1;
        let new_h = rgb.height() & !1;
        rgb = image::imageops::resize(
            &rgb,
            new_w.max(2),
            new_h.max(2),
            image::imageops::FilterType::Lanczos3,
        );
    }
    let w = rgb.width() as usize;
    let h = rgb.height() as usize;
    let cw = w / 2;
    let ch = h / 2;

    let mut y_plane = vec![0u8; w * h];
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];

    // BT.709 limited range RGB → YCbCr (the 8-bit broadcast default).
    for row in 0..h {
        for col in 0..w {
            let p = rgb.get_pixel(col as u32, row as u32);
            let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
            let y_lin = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let y_tv = (16.0 + (219.0 / 255.0) * y_lin).round().clamp(16.0, 235.0);
            y_plane[row * w + col] = y_tv as u8;
        }
    }
    // Chroma — average each 2×2 luma block.
    for row in 0..ch {
        for col in 0..cw {
            let mut r_acc = 0f32;
            let mut g_acc = 0f32;
            let mut b_acc = 0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = rgb.get_pixel((col * 2 + dx) as u32, (row * 2 + dy) as u32);
                    r_acc += p[0] as f32;
                    g_acc += p[1] as f32;
                    b_acc += p[2] as f32;
                }
            }
            let r = r_acc / 4.0;
            let g = g_acc / 4.0;
            let b = b_acc / 4.0;
            let y_lin = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let cb = ((b - y_lin) / 1.8556) * (224.0 / 255.0) + 128.0;
            let cr = ((r - y_lin) / 1.5748) * (224.0 / 255.0) + 128.0;
            u_plane[row * cw + col] = cb.round().clamp(16.0, 240.0) as u8;
            v_plane[row * cw + col] = cr.round().clamp(16.0, 240.0) as u8;
        }
    }

    Ok(DecodedImage {
        width: w as u32,
        height: h as u32,
        y: y_plane,
        u: u_plane,
        v: v_plane,
        y_stride: w,
        uv_stride: cw,
    })
}

async fn encode_loop(
    img: DecodedImage,
    fps: u8,
    bitrate_kbps: u32,
    audio_silence: bool,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    let backend = select_video_backend()
        .ok_or_else(|| anyhow!(
            "media-player image: no video encoder backend compiled in (enable one of video-encoder-x264 / -x265 / -nvenc)"
        ))?;
    let video_cfg = VideoEncodeConfig {
        codec: backend_codec_string(backend).to_string(),
        width: Some(img.width),
        height: Some(img.height),
        fps_num: Some(fps as u32),
        fps_den: Some(1),
        bitrate_kbps: Some(bitrate_kbps),
        gop_size: Some(fps as u32), // 1 IDR / second so receivers join fast
        preset: Some("ultrafast".to_string()),
        profile: None,
        chroma: None,
        bit_depth: None,
        rate_control: None,
        crf: None,
        max_bitrate_kbps: None,
        bframes: Some(0),
        refs: None,
        level: None,
        tune: Some("stillimage".to_string()),
        color_primaries: None,
        color_transfer: None,
        color_matrix: None,
        color_range: None,
    };

    let mut encoder = ScaledVideoEncoder::new(
        video_cfg,
        backend,
        fps as u32,
        1,
        false,
        format!("media-player-image-{}x{}", img.width, img.height),
    );

    let mut audio_state = if audio_silence {
        Some(build_silence_encoder()?)
    } else {
        None
    };

    let mut ts_mux = TsMuxer::new();
    ts_mux.set_has_video(true);
    ts_mux.set_video_stream_type(STREAM_TYPE_H264);
    if audio_state.is_some() {
        ts_mux.set_has_audio(true);
        ts_mux.set_audio_stream(STREAM_TYPE_AAC, None);
    } else {
        ts_mux.set_has_audio(false);
    }

    let frame_duration = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
    let start = Instant::now();
    let mut ticker = interval_at(start, frame_duration);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut bundle = BytesMut::with_capacity(BUNDLE_SIZE);
    let mut frame_idx: u64 = 0;
    let mut out_rtp_ts: u32 = 0;

    loop {
        tokio::select! {
            _ = session.cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let pts_90khz = frame_idx.saturating_mul(90_000) / fps.max(1) as u64;
        out_rtp_ts = pts_90khz as u32;

        let encoded = tokio::task::block_in_place(|| {
            encoder.encode_raw_planes(
                img.width,
                img.height,
                AV_PIX_FMT_YUV420P,
                &img.y,
                img.y_stride,
                &img.u,
                img.uv_stride,
                &img.v,
                img.uv_stride,
                Some(pts_90khz as i64),
            )
        })
        .map_err(|e| anyhow!("image slate encode: {e}"))?;

        for ef in encoded {
            for ts_pkt in ts_mux.mux_video(&ef.data, ef.pts as u64, ef.pts as u64, ef.keyframe) {
                bundle.extend_from_slice(&ts_pkt);
                if bundle.len() >= BUNDLE_SIZE {
                    emit_bundle(&mut bundle, session, out_rtp_ts);
                }
            }
        }

        if let Some(ref mut s) = audio_state {
            for (adts, apts) in s.tick(fps as usize)? {
                for ts_pkt in ts_mux.mux_audio_pre_adts(&adts, apts) {
                    bundle.extend_from_slice(&ts_pkt);
                    if bundle.len() >= BUNDLE_SIZE {
                        emit_bundle(&mut bundle, session, out_rtp_ts);
                    }
                }
            }
        }

        frame_idx = frame_idx.saturating_add(1);
    }

    if !bundle.is_empty() {
        emit_bundle(&mut bundle, session, out_rtp_ts);
    }
    Ok(())
}

#[allow(unused_imports, unreachable_code)]
fn select_video_backend() -> Option<video_codec::VideoEncoderCodec> {
    use video_codec::VideoEncoderCodec;
    #[cfg(feature = "video-encoder-x264")]
    {
        return Some(VideoEncoderCodec::X264);
    }
    #[cfg(all(feature = "video-encoder-x265", not(feature = "video-encoder-x264")))]
    {
        return Some(VideoEncoderCodec::X265);
    }
    #[cfg(all(
        feature = "video-encoder-nvenc",
        not(feature = "video-encoder-x264"),
        not(feature = "video-encoder-x265")
    ))]
    {
        return Some(VideoEncoderCodec::H264Nvenc);
    }
    #[cfg(all(
        feature = "video-encoder-qsv",
        not(feature = "video-encoder-x264"),
        not(feature = "video-encoder-x265"),
        not(feature = "video-encoder-nvenc")
    ))]
    {
        return Some(VideoEncoderCodec::H264Qsv);
    }
    None
}

fn backend_codec_string(codec: video_codec::VideoEncoderCodec) -> &'static str {
    use video_codec::VideoEncoderCodec;
    match codec {
        VideoEncoderCodec::X264 => "x264",
        VideoEncoderCodec::X265 => "x265",
        VideoEncoderCodec::H264Nvenc => "h264_nvenc",
        VideoEncoderCodec::HevcNvenc => "hevc_nvenc",
        VideoEncoderCodec::H264Qsv => "h264_qsv",
        VideoEncoderCodec::HevcQsv => "hevc_qsv",
    }
}

struct SilenceEncoder {
    encoder: aac_audio::AacEncoder,
    sample_rate: usize,
    channels: usize,
    frame_size: usize,
    accumulator: Vec<Vec<f32>>,
    accumulated: usize,
    pts_90k: u64,
}

fn build_silence_encoder() -> Result<SilenceEncoder> {
    let sample_rate = 48_000usize;
    let channels = 2usize;
    let cfg = aac_codec::EncoderConfig {
        profile: aac_codec::AacProfile::AacLc,
        sample_rate: sample_rate as u32,
        channels: channels as u8,
        bitrate: 64_000,
        afterburner: true,
        sbr_signaling: aac_codec::SbrSignaling::default(),
        transport: aac_codec::TransportType::Adts,
    };
    let encoder = aac_audio::AacEncoder::open(&cfg).map_err(|e| anyhow!("fdk-aac open: {e}"))?;
    let frame_size = encoder.frame_size() as usize;
    Ok(SilenceEncoder {
        encoder,
        sample_rate,
        channels,
        frame_size,
        accumulator: vec![Vec::with_capacity(frame_size); channels],
        accumulated: 0,
        pts_90k: 0,
    })
}

impl SilenceEncoder {
    fn tick(&mut self, fps: usize) -> Result<Vec<(Vec<u8>, u64)>> {
        let samples_needed = self.sample_rate / fps.max(1);
        for ch in self.accumulator.iter_mut() {
            ch.extend(std::iter::repeat(0.0f32).take(samples_needed));
        }
        self.accumulated += samples_needed;

        let mut out = Vec::new();
        while self.accumulated >= self.frame_size {
            let mut frame_planar: Vec<Vec<f32>> = Vec::with_capacity(self.channels);
            for ch_buf in self.accumulator.iter_mut() {
                frame_planar.push(ch_buf.drain(..self.frame_size).collect());
            }
            self.accumulated -= self.frame_size;
            let encoded = self
                .encoder
                .encode_frame(&frame_planar)
                .map_err(|e| anyhow!("aac silence encode: {e}"))?;
            let pts = self.pts_90k;
            if self.sample_rate > 0 {
                self.pts_90k = self
                    .pts_90k
                    .saturating_add((encoded.num_samples as u64) * 90_000 / self.sample_rate as u64);
            }
            out.push((encoded.bytes, pts));
        }
        Ok(out)
    }
}
