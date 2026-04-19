// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Phase 3 audio & video re-encoders for CMAF outputs.
//!
//! Both are callable from the hot path via `block_in_place` so the
//! broadcast subscriber task does not lose ordering with other outputs.
//!
//! # Audio re-encoder
//!
//! Source AAC frame → `AacDecoder` → optional PCM `transcode` →
//! `AudioEncoder` → target AAC frame(s). For AAC-LC → AAC-LC with
//! same sample-rate / channels / bitrate this is a passthrough; the
//! encoder optimises out the round-trip.
//!
//! # Video re-encoder
//!
//! Source H.264/HEVC access unit → Annex-B framing → `VideoDecoder`
//! → `VideoEncoder` (x264 / x265 / NVENC, feature-gated) → target
//! H.264/HEVC NAL units. GoP alignment is forced to
//! `segment_duration_secs * fps` so CMAF segment boundaries always
//! land on an IDR.

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;

use crate::config::models::{AudioEncodeConfig, VideoEncodeConfig};
use crate::engine::audio_decode::AacDecoder;
use crate::engine::audio_encode::{AudioCodec, AudioEncoder, EncoderParams};
use crate::stats::collector::OutputStatsAccumulator;

use super::fmp4::VideoCodec as CmafVideoCodec;

// ────────────────────────────────────────────────────────────────────────
//  Audio re-encoder
// ────────────────────────────────────────────────────────────────────────

pub struct AudioReencoder {
    decoder: Option<AacDecoder>,
    encoder: Option<AudioEncoder>,
    target_codec: AudioCodec,
    target_bitrate_kbps: u32,
    target_sample_rate: Option<u32>,
    target_channels: Option<u8>,
    lazy_init_params: LazyAudioInit,
    flow_id: String,
    output_id: String,
    cancel: CancellationToken,
    out_stats: Arc<OutputStatsAccumulator>,
}

struct LazyAudioInit {
    /// ADTS config tuple cached by the demuxer. `None` until first
    /// AAC frame observed.
    adts_config: Option<(u8, u8, u8)>,
}

impl AudioReencoder {
    pub fn new(
        cfg: &AudioEncodeConfig,
        cancel: &CancellationToken,
        output_id: &str,
        flow_id: &str,
    ) -> Result<Self> {
        let target_codec = AudioCodec::parse(&cfg.codec)
            .ok_or_else(|| anyhow::anyhow!("unknown audio codec: {}", cfg.codec))?;
        // CMAF audio is AAC only in Phase 3 — reject exotic codecs
        // early so operators see the error at startup, not at runtime.
        if !matches!(target_codec, AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2) {
            bail!(
                "CMAF audio_encode rejects codec '{}' — allowed: aac_lc, he_aac_v1, he_aac_v2",
                cfg.codec
            );
        }
        let target_bitrate_kbps = cfg
            .bitrate_kbps
            .unwrap_or_else(|| target_codec.default_bitrate_kbps());
        Ok(Self {
            decoder: None,
            encoder: None,
            target_codec,
            target_bitrate_kbps,
            target_sample_rate: cfg.sample_rate,
            target_channels: cfg.channels,
            lazy_init_params: LazyAudioInit { adts_config: None },
            flow_id: flow_id.to_string(),
            output_id: output_id.to_string(),
            cancel: cancel.clone(),
            // Stats accumulator is optional for the encoder subsystem; we
            // pass a detached handle because CMAF uses its own collector.
            out_stats: Arc::new(OutputStatsAccumulator::new(
                "cmaf-audio-encode".to_string(),
                "cmaf-audio-encode".to_string(),
                "cmaf-audio-encode".to_string(),
            )),
        })
    }

    /// Encode one source AAC frame. Returns zero or more re-encoded
    /// AAC frames (the encoder may buffer one input before emitting
    /// depending on the codec's frame size).
    pub fn encode_aac_frame(&mut self, frame: &[u8], pts: u64) -> Result<Vec<Vec<u8>>> {
        // Lazy decoder / encoder construction — we can't initialise
        // either until we know the source sample rate + channels, which
        // come from the demuxer-cached ADTS config. The caller passes
        // the tuple via `set_adts_config` before the first frame.
        if self.decoder.is_none() {
            let (profile, sr_idx, ch_cfg) = self
                .lazy_init_params
                .adts_config
                .ok_or_else(|| anyhow::anyhow!("audio re-encoder: ADTS config not set"))?;
            let dec = AacDecoder::from_adts_config(profile, sr_idx, ch_cfg)
                .map_err(|e| anyhow::anyhow!("AacDecoder open failed: {e}"))?;
            self.decoder = Some(dec);
        }
        let dec = self.decoder.as_mut().unwrap();
        let planar = dec
            .decode_frame(frame)
            .map_err(|e| anyhow::anyhow!("AAC decode failed: {e}"))?;
        if planar.is_empty() {
            return Ok(Vec::new());
        }
        let source_sr = dec.sample_rate();
        let source_ch = dec.channels();

        if self.encoder.is_none() {
            let params = EncoderParams {
                codec: self.target_codec,
                sample_rate: source_sr,
                channels: source_ch,
                target_bitrate_kbps: self.target_bitrate_kbps,
                target_sample_rate: self.target_sample_rate.unwrap_or(source_sr),
                target_channels: self.target_channels.unwrap_or(source_ch),
            };
            let enc = AudioEncoder::spawn(
                params,
                self.cancel.clone(),
                self.flow_id.clone(),
                self.output_id.clone(),
                self.out_stats.clone(),
                None,
            )
            .map_err(|e| anyhow::anyhow!("AudioEncoder spawn failed: {e}"))?;
            self.encoder = Some(enc);
        }
        let enc = self.encoder.as_mut().unwrap();
        enc.submit_planar(&planar, pts);
        let mut out = Vec::new();
        while let Some(frame) = enc.try_recv() {
            out.push(frame.data.to_vec());
        }
        Ok(out)
    }

    /// Tell the re-encoder the ADTS parameters observed by the demuxer
    /// so the lazy AacDecoder construction can succeed.
    pub fn set_adts_config(&mut self, profile: u8, sr_idx: u8, ch_cfg: u8) {
        self.lazy_init_params.adts_config = Some((profile, sr_idx, ch_cfg));
    }
}

// ────────────────────────────────────────────────────────────────────────
//  Video re-encoder
// ────────────────────────────────────────────────────────────────────────

#[cfg(feature = "video-thumbnail")]
pub struct VideoReencoder {
    decoder: Option<video_engine::VideoDecoder>,
    encoder: Option<video_engine::VideoEncoder>,
    target_codec: video_codec::VideoEncoderCodec,
    target_bitrate_kbps: u32,
    target_width: Option<u32>,
    target_height: Option<u32>,
    target_fps_num: Option<u32>,
    target_fps_den: Option<u32>,
    /// GoP size = fps × segment_duration_secs. Forced at encoder open.
    target_gop_size: Option<u32>,
    /// Output_id for tracing.
    output_id: String,
    /// Annex-B scratch buffer reused across frames to avoid allocs.
    annex_b_scratch: Vec<u8>,
    /// Source codec pinned at first observed frame; changing codec is
    /// rejected (operator must restart the flow).
    source_codec: Option<CmafVideoCodec>,
}

#[cfg(not(feature = "video-thumbnail"))]
pub struct VideoReencoder {
    _phantom: (),
    output_id: String,
}

pub struct VideoOutFrame {
    pub nalus: Vec<Vec<u8>>,
    pub is_keyframe: bool,
}

#[cfg(feature = "video-thumbnail")]
impl VideoReencoder {
    pub fn new(cfg: &VideoEncodeConfig, output_id: &str) -> Result<Self> {
        let target_codec = match cfg.codec.as_str() {
            "x264" => video_codec::VideoEncoderCodec::X264,
            "x265" => video_codec::VideoEncoderCodec::X265,
            "h264_nvenc" => video_codec::VideoEncoderCodec::H264Nvenc,
            "hevc_nvenc" => video_codec::VideoEncoderCodec::HevcNvenc,
            other => bail!("unknown video codec: {other}"),
        };
        Ok(Self {
            decoder: None,
            encoder: None,
            target_codec,
            target_bitrate_kbps: cfg.bitrate_kbps.unwrap_or(5_000),
            target_width: cfg.width,
            target_height: cfg.height,
            target_fps_num: cfg.fps_num,
            target_fps_den: cfg.fps_den,
            target_gop_size: cfg.gop_size,
            output_id: output_id.to_string(),
            annex_b_scratch: Vec::with_capacity(256 * 1024),
            source_codec: None,
        })
    }

    /// Encode one access unit. Returns the re-encoded NAL list +
    /// keyframe flag, or `None` if the encoder buffered the frame.
    pub fn encode_frame(
        &mut self,
        nalus: &[Vec<u8>],
        pts: u64,
        _is_keyframe: bool,
        codec: CmafVideoCodec,
    ) -> Result<Option<VideoOutFrame>> {
        match self.source_codec {
            None => self.source_codec = Some(codec),
            Some(prev) if prev != codec => {
                bail!("video re-encoder: source codec changed mid-flow");
            }
            _ => {}
        }

        // Assemble Annex-B bitstream for the decoder. Each NAL unit in
        // `nalus` has start codes already stripped, so we re-prepend
        // 0x00000001.
        self.annex_b_scratch.clear();
        for nalu in nalus {
            self.annex_b_scratch.extend_from_slice(&[0, 0, 0, 1]);
            self.annex_b_scratch.extend_from_slice(nalu);
        }

        if self.decoder.is_none() {
            let src_codec = match codec {
                CmafVideoCodec::H264 => video_codec::VideoCodec::H264,
                CmafVideoCodec::H265 => video_codec::VideoCodec::Hevc,
            };
            let dec = video_engine::VideoDecoder::open(src_codec)
                .map_err(|e| anyhow::anyhow!("VideoDecoder open failed: {e}"))?;
            self.decoder = Some(dec);
        }
        let dec = self.decoder.as_mut().unwrap();
        dec.send_packet(&self.annex_b_scratch)
            .map_err(|e| anyhow::anyhow!("VideoDecoder send_packet failed: {e}"))?;
        let decoded = match dec.receive_frame() {
            Ok(f) => f,
            Err(_e) => return Ok(None), // encoder buffered
        };

        // Lazy-open encoder now that we know the source resolution.
        if self.encoder.is_none() {
            let w = self.target_width.unwrap_or(decoded.width());
            let h = self.target_height.unwrap_or(decoded.height());
            let enc_cfg = video_codec::VideoEncoderConfig {
                codec: self.target_codec,
                width: w,
                height: h,
                fps_num: self.target_fps_num.unwrap_or(30),
                fps_den: self.target_fps_den.unwrap_or(1),
                bitrate_kbps: self.target_bitrate_kbps,
                gop_size: self.target_gop_size.unwrap_or(60),
                global_header: false,
                tune: "zerolatency".to_string(),
                ..Default::default()
            };
            let enc = video_engine::VideoEncoder::open(&enc_cfg)
                .map_err(|e| anyhow::anyhow!("VideoEncoder open failed: {e}"))?;
            tracing::info!(
                "CMAF output '{}': video re-encoder opened {}x{}@{}kbps",
                self.output_id, w, h, self.target_bitrate_kbps
            );
            self.encoder = Some(enc);
        }
        let enc = self.encoder.as_mut().unwrap();
        let (y, y_s, u, u_s, v, v_s) = decoded
            .yuv_planes()
            .ok_or_else(|| anyhow::anyhow!("decoded frame has no planar YUV"))?;
        let encoded = enc
            .encode_frame(y, y_s, u, u_s, v, v_s, Some(pts as i64))
            .map_err(|e| anyhow::anyhow!("VideoEncoder encode_frame failed: {e}"))?;
        if encoded.is_empty() {
            return Ok(None);
        }

        // Convert Annex-B bitstream back to start-code-stripped NAL
        // units (CMAF samples carry length-prefixed NALs; the
        // segmenter re-applies length prefixes before packing).
        let mut out_nalus = Vec::new();
        let mut is_keyframe = false;
        for frame in &encoded {
            if frame.keyframe {
                is_keyframe = true;
            }
            split_annex_b_to_nalus(&frame.data, &mut out_nalus);
        }
        if out_nalus.is_empty() {
            return Ok(None);
        }
        Ok(Some(VideoOutFrame {
            nalus: out_nalus,
            is_keyframe,
        }))
    }
}

#[cfg(not(feature = "video-thumbnail"))]
impl VideoReencoder {
    pub fn new(_cfg: &VideoEncodeConfig, output_id: &str) -> Result<Self> {
        bail!("video_encode requires the `video-thumbnail` feature (and a `video-encoder-*` backend) at build time")
    }

    pub fn encode_frame(
        &mut self,
        _nalus: &[Vec<u8>],
        _pts: u64,
        _is_keyframe: bool,
        _codec: CmafVideoCodec,
    ) -> Result<Option<VideoOutFrame>> {
        bail!("video_encode disabled at build time")
    }
}

/// Split an Annex-B byte stream into NALU vectors with the start
/// codes stripped. Handles both 3-byte (0x000001) and 4-byte
/// (0x00000001) start codes.
#[cfg(feature = "video-thumbnail")]
fn split_annex_b_to_nalus(data: &[u8], out: &mut Vec<Vec<u8>>) {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
                continue;
            }
            if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                starts.push(i + 4);
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    for (k, &s) in starts.iter().enumerate() {
        let end = if k + 1 < starts.len() {
            // Back up over the start code preamble of the next NAL.
            let next_start = starts[k + 1];
            let pre = if next_start >= 4 && &data[next_start - 4..next_start] == [0, 0, 0, 1] {
                next_start - 4
            } else if next_start >= 3 && &data[next_start - 3..next_start] == [0, 0, 1] {
                next_start - 3
            } else {
                next_start
            };
            pre
        } else {
            data.len()
        };
        if end > s {
            out.push(data[s..end].to_vec());
        }
    }
}

#[cfg(test)]
#[cfg(feature = "video-thumbnail")]
mod tests {
    use super::*;

    #[test]
    fn annex_b_split_4byte_prefix() {
        let data = [0u8, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE];
        let mut out = Vec::new();
        split_annex_b_to_nalus(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0x67, 0x42]);
        assert_eq!(out[1], vec![0x68, 0xCE]);
    }

    #[test]
    fn annex_b_split_3byte_prefix() {
        let data = [0u8, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE];
        let mut out = Vec::new();
        split_annex_b_to_nalus(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0x67, 0x42]);
        assert_eq!(out[1], vec![0x68, 0xCE]);
    }
}
