// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Streaming MPEG-TS audio elementary-stream replacement.
//!
//! [`TsAudioReplacer`] consumes raw TS packets (188-byte aligned), passes
//! video / PAT / null / other elementary streams through unchanged, and
//! transparently rewrites the audio ES:
//!
//! 1. On the first PMT seen, the replacer discovers the audio PID and its
//!    source stream_type. It rewrites the PMT in-place with the target
//!    stream_type (and recomputes the section CRC).
//! 2. On each audio TS packet, bytes are buffered into a PES. When a new
//!    PES begins (PUSI), the previous PES is decoded (AAC-LC ADTS → PCM),
//!    fed to the target encoder, and the resulting encoded audio frames
//!    are re-packetized into new audio TS packets with the same audio PID.
//! 3. Raw audio TS packets are dropped from the output — they are replaced
//!    by the re-encoded equivalents.
//!
//! This is the streaming variant of the HLS segment-level remuxer in
//! `output_hls.rs`. State is kept across chunks (PES buffer, decoder,
//! encoder, PCM accumulator, PMT identity) so every call to [`process`] is
//! incremental. [`flush`] drains any trailing PES / encoder buffer on
//! shutdown.
//!
//! The replacer is fully synchronous — output tasks that want to use it
//! inside an async context should call it from `tokio::task::block_in_place`
//! or delegate to a dedicated worker thread. AAC / codec operations take
//! single-digit milliseconds per frame and must not run inline on a
//! single-threaded runtime.

use crate::config::models::AudioEncodeConfig;

use super::audio_encode::AudioCodec;
use super::audio_transcode::{PlanarAudioTranscoder, TranscodeJson};
use super::ts_parse::{
    mpeg2_crc32, parse_pat_programs, ts_has_adaptation, ts_has_payload, ts_payload_offset, ts_pid,
    ts_pusi, PAT_PID, TS_PACKET_SIZE, TS_SYNC_BYTE,
};

// ────────────────────────── Public surface ──────────────────────────

/// Errors raised when constructing a [`TsAudioReplacer`].
#[derive(Debug)]
#[allow(dead_code)]
pub enum TsAudioReplaceError {
    /// Codec name not recognised.
    UnknownCodec(String),
    /// Codec cannot be carried inside MPEG-TS (e.g. Opus has no standard
    /// TS mapping on the RTP/UDP/SRT outputs we target).
    UnsupportedCodec(String),
    /// Build compiled without the feature required for this codec.
    MissingFeature(&'static str),
}

impl std::fmt::Display for TsAudioReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCodec(c) => write!(f, "unknown codec '{c}'"),
            Self::UnsupportedCodec(c) => write!(f, "codec '{c}' is not supported in MPEG-TS"),
            Self::MissingFeature(feat) => {
                write!(f, "this build was compiled without the '{feat}' feature")
            }
        }
    }
}

impl std::error::Error for TsAudioReplaceError {}

/// MPEG-TS audio elementary-stream replacer.
///
/// Not `Sync` (the in-process codecs hold raw C state), but `Send` so the
/// whole instance can be moved to a blocking worker.
pub struct TsAudioReplacer {
    /// Target codec for the output audio ES.
    codec: AudioCodec,
    /// Resolved output bitrate in kbps.
    bitrate_kbps: u32,
    /// Optional override of the output sample rate. `None` = use source.
    sample_rate_override: Option<u32>,
    /// Optional override of the output channel count. `None` = use source.
    channels_override: Option<u8>,

    /// Discovered PMT PID (the PAT's first program's PMT). `None` before
    /// the PAT has been seen.
    pmt_pid: Option<u16>,
    /// Discovered audio PID. `None` before the PMT has been parsed.
    audio_pid: Option<u16>,
    /// Source audio stream_type (0x0F = AAC-ADTS, etc.). Used to decide
    /// which decoder to instantiate.
    source_stream_type: u8,
    /// Target audio stream_type written into the rewritten PMT.
    target_stream_type: u8,

    /// PES bytes accumulated for the current audio packet (since the
    /// previous PUSI). Flushed on the next PUSI.
    pes_buffer: Vec<u8>,
    /// Have we started collecting a PES yet? (false until the first PUSI)
    pes_started: bool,

    /// Continuity counter for the output audio PID. Increments per emitted
    /// audio TS packet.
    out_audio_cc: u8,

    /// Running output PTS in 90 kHz ticks. Anchored to the first decoded
    /// PES PTS; advanced by encoded frame sample counts.
    out_pts_90k: u64,
    /// True until the first decoded PES anchors `out_pts_90k`.
    out_pts_anchored: bool,

    /// Lazily constructed AAC-LC / ADTS decoder. Opened on the first PES
    /// flush once we know the source is AAC.
    #[cfg(feature = "fdk-aac")]
    aac_decoder: Option<aac_audio::AacDecoder>,

    /// Lazily constructed AAC encoder (for AAC-family targets). Opened on
    /// the first encode call, once we know the input sample-rate/channels
    /// after the first successful decode.
    #[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
    aac_encoder: Option<aac_audio::AacEncoder>,

    /// Lazily constructed libavcodec encoder for MP2 / AC-3 targets.
    #[cfg(feature = "video-thumbnail")]
    av_encoder: Option<video_engine::AudioEncoder>,

    /// Per-channel PCM accumulator (f32, planar). Grown by successful
    /// decodes, drained in `frame_size`-sized chunks into the encoder.
    accumulator: Vec<Vec<f32>>,

    /// Resolved output channel count. Fixed after the first decode.
    resolved_channels: u8,
    /// Resolved output sample rate. Fixed after the first decode.
    resolved_sample_rate: u32,
    /// True after codecs are initialised (decoder + encoder both open).
    codecs_ready: bool,

    /// Optional channel-shuffle / sample-rate transcode block. Applied in
    /// planar PCM form between the AAC decoder and the target encoder.
    /// `None` preserves the pre-transcode behaviour exactly.
    transcode_cfg: Option<TranscodeJson>,
    /// Lazily constructed planar transcoder. Opened on the first decoded
    /// frame, once the input rate + channel count are known.
    transcoder: Option<PlanarAudioTranscoder>,
}

impl TsAudioReplacer {
    /// Build a new replacer from an `audio_encode` block and an optional
    /// `transcode` block.
    ///
    /// This only parses and validates the codec — all heavy codec state is
    /// opened lazily when the first PES is flushed, so a replacer for a
    /// flow that never carries audio costs essentially nothing.
    ///
    /// When `transcode` is `Some`, the decoded PCM is run through a planar
    /// channel-shuffle / sample-rate stage before the target encoder. Unset
    /// fields inside the block fall back to the encoder's own overrides and
    /// then the source format, so an empty block is a no-op.
    pub fn new(
        cfg: &AudioEncodeConfig,
        transcode: Option<TranscodeJson>,
    ) -> Result<Self, TsAudioReplaceError> {
        let codec = AudioCodec::parse(&cfg.codec)
            .ok_or_else(|| TsAudioReplaceError::UnknownCodec(cfg.codec.clone()))?;

        // Opus has no standard MPEG-TS mapping on our targeted outputs.
        if matches!(codec, AudioCodec::Opus) {
            return Err(TsAudioReplaceError::UnsupportedCodec(cfg.codec.clone()));
        }

        let target_stream_type = match codec {
            AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => 0x0F,
            AudioCodec::Mp2 => 0x03,
            AudioCodec::Ac3 => 0x81,
            AudioCodec::Opus => unreachable!(),
        };

        let bitrate_kbps = cfg.bitrate_kbps.unwrap_or_else(|| codec.default_bitrate_kbps());

        Ok(Self {
            codec,
            bitrate_kbps,
            sample_rate_override: cfg.sample_rate,
            channels_override: cfg.channels,
            pmt_pid: None,
            audio_pid: None,
            source_stream_type: 0,
            target_stream_type,
            pes_buffer: Vec::with_capacity(16 * 1024),
            pes_started: false,
            out_audio_cc: 0,
            out_pts_90k: 0,
            out_pts_anchored: false,
            #[cfg(feature = "fdk-aac")]
            aac_decoder: None,
            #[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
            aac_encoder: None,
            #[cfg(feature = "video-thumbnail")]
            av_encoder: None,
            accumulator: Vec::new(),
            resolved_channels: 0,
            resolved_sample_rate: 0,
            codecs_ready: false,
            transcode_cfg: transcode,
            transcoder: None,
        })
    }

    /// Human-readable description of the active encoder target.
    pub fn target_description(&self) -> String {
        format!(
            "{} @ {} kbps",
            self.codec.as_str(),
            self.bitrate_kbps,
        )
    }

    /// Process one chunk of raw MPEG-TS bytes.
    ///
    /// `input_ts` must be 188-byte aligned (caller is responsible for TS
    /// sync recovery). Output TS bytes are appended to `output`. On bad
    /// input (not TS-aligned) the chunk is appended unchanged.
    pub fn process(&mut self, input_ts: &[u8], output: &mut Vec<u8>) {
        if input_ts.is_empty() {
            return;
        }

        // Bail out for non-aligned input: passthrough, do nothing clever.
        if input_ts.len() % TS_PACKET_SIZE != 0 {
            output.extend_from_slice(input_ts);
            return;
        }

        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= input_ts.len() {
            let pkt = &input_ts[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if pkt[0] != TS_SYNC_BYTE {
                // Lost alignment — emit as-is and move on.
                output.extend_from_slice(pkt);
                continue;
            }

            let pid = ts_pid(pkt);

            // Before we know the PMT PID, learn it from the PAT.
            if pid == PAT_PID && ts_pusi(pkt) && self.pmt_pid.is_none() {
                let mut programs = parse_pat_programs(pkt);
                if !programs.is_empty() {
                    programs.sort_by_key(|(num, _)| *num);
                    self.pmt_pid = Some(programs[0].1);
                }
            }

            // Once we know the PMT PID, parse it (and rewrite the
            // broadcast copy) on every PUSI.
            if let Some(pmt_pid) = self.pmt_pid {
                if pid == pmt_pid && ts_pusi(pkt) {
                    // Populate audio_pid + source_stream_type if still
                    // unknown.
                    if self.audio_pid.is_none() {
                        if let Some((apid, ast)) = parse_pmt_audio(pkt) {
                            self.audio_pid = Some(apid);
                            self.source_stream_type = ast;
                        }
                    }
                    // Rewrite the stream_type for the audio PID and
                    // recompute the section CRC. If no audio PID is
                    // known yet this is a no-op.
                    let mut rewritten = pkt.to_vec();
                    if let Some(apid) = self.audio_pid {
                        rewrite_pmt_audio_stream_type(&mut rewritten, apid, self.target_stream_type);
                    }
                    output.extend_from_slice(&rewritten);
                    continue;
                }
            }

            // Audio packets: route to the PES accumulator. Do NOT emit.
            if Some(pid) == self.audio_pid {
                self.feed_audio_packet(pkt, output);
                continue;
            }

            // Everything else: passthrough.
            output.extend_from_slice(pkt);
        }
    }

    /// Flush any buffered PES + encoder state. Call once on graceful
    /// shutdown. No-op if codecs were never initialised.
    #[allow(dead_code)]
    pub fn flush(&mut self, output: &mut Vec<u8>) {
        // Flush pending PES.
        if self.pes_started && !self.pes_buffer.is_empty() {
            let pes = std::mem::take(&mut self.pes_buffer);
            let _ = self.consume_pes(&pes, output);
            self.pes_buffer.clear();
            self.pes_started = false;
        }

        // Flush the encoder (last encoded frames live here).
        #[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
        {
            if let Some(ref mut enc) = self.aac_encoder {
                // fdk-aac encoder drains by calling encode_frame with empty
                // input — we approximate by skipping, since most broadcast
                // feeds don't need the trailing frames.
                let _ = enc;
            }
        }

        #[cfg(feature = "video-thumbnail")]
        {
            if let Some(ref mut enc) = self.av_encoder {
                if let Ok(frames) = enc.flush() {
                    let pid = match self.audio_pid {
                        Some(p) => p,
                        None => return,
                    };
                    for ef in frames {
                        let pes = build_audio_pes(&ef.data, self.out_pts_90k);
                        let pkts = packetize_ts(pid, &pes, &mut self.out_audio_cc);
                        for pkt in &pkts {
                            output.extend_from_slice(pkt);
                        }
                        if enc.sample_rate() > 0 {
                            self.out_pts_90k +=
                                (ef.num_samples as u64) * 90_000 / enc.sample_rate() as u64;
                        }
                    }
                }
            }
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────

    /// Route one audio TS packet into the PES accumulator, flushing the
    /// previous PES (if any) into `output` when a new PES begins.
    fn feed_audio_packet(&mut self, pkt: &[u8], output: &mut Vec<u8>) {
        if !ts_has_payload(pkt) {
            return;
        }
        let pusi = ts_pusi(pkt);
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            return;
        }
        let payload = &pkt[payload_start..];

        if pusi {
            // Flush previous PES, if any.
            if self.pes_started && !self.pes_buffer.is_empty() {
                let pes = std::mem::take(&mut self.pes_buffer);
                let _ = self.consume_pes(&pes, output);
                self.pes_buffer.clear();
            }
            self.pes_buffer.extend_from_slice(payload);
            self.pes_started = true;
        } else if self.pes_started {
            self.pes_buffer.extend_from_slice(payload);
        }
    }

    /// Decode one complete PES worth of audio, feed the PCM into the
    /// encoder, and emit as many encoded-audio TS packets as the encoder
    /// produced.
    fn consume_pes(&mut self, pes: &[u8], output: &mut Vec<u8>) -> Result<(), ()> {
        let (es_data, pts) = match extract_pes_audio(pes) {
            Some(x) => x,
            None => return Err(()),
        };

        if !self.out_pts_anchored {
            self.out_pts_90k = pts;
            self.out_pts_anchored = true;
        }

        // Decode. Currently the only supported input codec is AAC-LC ADTS
        // (source_stream_type 0x0F).
        #[cfg(feature = "fdk-aac")]
        {
            if self.source_stream_type != 0x0F {
                return Err(());
            }

            // ── Phase 1: decode all ADTS frames in this PES ──
            struct Decoded {
                planar: Vec<Vec<f32>>,
                sample_rate: u32,
                channels: u8,
            }
            let mut decoded_frames: Vec<Decoded> = Vec::new();
            {
                if self.aac_decoder.is_none() {
                    self.aac_decoder = Some(
                        aac_audio::AacDecoder::open_adts().map_err(|_| ())?,
                    );
                }
                let decoder = self.aac_decoder.as_mut().unwrap();

                let mut pos = 0;
                while pos + 7 <= es_data.len() {
                    if es_data[pos] != 0xFF || (es_data[pos + 1] & 0xF0) != 0xF0 {
                        break;
                    }
                    let protection_absent = (es_data[pos + 1] & 0x01) != 0;
                    let header_len = if protection_absent { 7 } else { 9 };
                    if pos + header_len > es_data.len() {
                        break;
                    }
                    let frame_len = (((es_data[pos + 3] & 0x03) as usize) << 11)
                        | ((es_data[pos + 4] as usize) << 3)
                        | ((es_data[pos + 5] as usize) >> 5);
                    if frame_len < header_len || pos + frame_len > es_data.len() {
                        break;
                    }
                    let adts = &es_data[pos..pos + frame_len];
                    pos += frame_len;

                    match decoder.decode_frame(adts) {
                        Ok(d) => {
                            decoded_frames.push(Decoded {
                                planar: d.planar,
                                sample_rate: decoder.sample_rate().unwrap_or(48_000),
                                channels: decoder.channels().unwrap_or(2),
                            });
                        }
                        Err(_) => continue,
                    }
                }
            }

            // ── Phase 2: feed PCM into encoder ──
            for d in decoded_frames {
                if !self.codecs_ready {
                    // Resolve the encoder target. When a transcode block is
                    // present it wins: build the planar transcoder first and
                    // let its output format drive the encoder. Any
                    // audio_encode.sample_rate / channels fields fold in as
                    // fallbacks for fields the transcode block leaves unset.
                    if let Some(ref tj_in) = self.transcode_cfg {
                        let merged = TranscodeJson {
                            sample_rate: tj_in.sample_rate.or(self.sample_rate_override),
                            channels: tj_in.channels.or(self.channels_override),
                            ..tj_in.clone()
                        };
                        match PlanarAudioTranscoder::new(
                            d.sample_rate,
                            d.channels,
                            &merged,
                        ) {
                            Ok(tc) => {
                                self.resolved_sample_rate = tc.out_sample_rate();
                                self.resolved_channels = tc.out_channels();
                                self.transcoder = Some(tc);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "TsAudioReplacer: transcode init failed ({e}); \
                                     dropping this PES"
                                );
                                return Err(());
                            }
                        }
                    } else {
                        self.resolved_sample_rate =
                            self.sample_rate_override.unwrap_or(d.sample_rate);
                        self.resolved_channels =
                            self.channels_override.unwrap_or(d.channels);
                    }
                    self.accumulator =
                        vec![Vec::new(); self.resolved_channels as usize];
                    self.init_encoder()?;
                    self.codecs_ready = true;
                }
                // Apply transcode if present; otherwise forward source PCM
                // directly. Both paths produce planar f32 at
                // `resolved_channels` channels.
                let shuffled_planar: Vec<Vec<f32>> =
                    if let Some(ref mut tc) = self.transcoder {
                        match tc.process(&d.planar) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!(
                                    "TsAudioReplacer: transcode process failed ({e}); \
                                     dropping frame"
                                );
                                continue;
                            }
                        }
                    } else {
                        d.planar
                    };
                for ch in 0..self.resolved_channels as usize {
                    if ch < shuffled_planar.len() {
                        self.accumulator[ch]
                            .extend_from_slice(&shuffled_planar[ch]);
                    } else if !shuffled_planar.is_empty() {
                        self.accumulator[ch].extend(
                            std::iter::repeat(0.0f32)
                                .take(shuffled_planar[0].len()),
                        );
                    }
                }
                self.drain_encoder(output)?;
            }
        }

        #[cfg(not(feature = "fdk-aac"))]
        {
            let _ = es_data;
            let _ = pts;
            return Err(());
        }

        Ok(())
    }

    /// Open the target encoder, using the source sample-rate / channels
    /// (plus overrides) fixed by the first successful decode.
    fn init_encoder(&mut self) -> Result<(), ()> {
        let target_sr = self.resolved_sample_rate;
        let target_ch = self.resolved_channels;

        match self.codec {
            AudioCodec::AacLc | AudioCodec::HeAacV1 | AudioCodec::HeAacV2 => {
                #[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
                {
                    let profile = match self.codec {
                        AudioCodec::AacLc => aac_codec::AacProfile::AacLc,
                        AudioCodec::HeAacV1 => aac_codec::AacProfile::HeAacV1,
                        AudioCodec::HeAacV2 => aac_codec::AacProfile::HeAacV2,
                        _ => unreachable!(),
                    };
                    let cfg = aac_codec::EncoderConfig {
                        profile,
                        sample_rate: target_sr,
                        channels: target_ch,
                        bitrate: self.bitrate_kbps * 1000,
                        afterburner: true,
                        sbr_signaling: aac_codec::SbrSignaling::default(),
                        transport: aac_codec::TransportType::Adts,
                    };
                    self.aac_encoder = Some(
                        aac_audio::AacEncoder::open(&cfg).map_err(|_| ())?,
                    );
                    return Ok(());
                }
                #[cfg(not(all(feature = "video-thumbnail", feature = "fdk-aac")))]
                {
                    return Err(());
                }
            }
            AudioCodec::Mp2 | AudioCodec::Ac3 => {
                #[cfg(feature = "video-thumbnail")]
                {
                    let codec_type = match self.codec {
                        AudioCodec::Mp2 => video_codec::AudioCodecType::Mp2,
                        AudioCodec::Ac3 => video_codec::AudioCodecType::Ac3,
                        _ => unreachable!(),
                    };
                    let cfg = video_codec::AudioEncoderConfig {
                        codec: codec_type,
                        sample_rate: target_sr,
                        channels: target_ch,
                        bitrate_kbps: self.bitrate_kbps,
                    };
                    self.av_encoder =
                        Some(video_engine::AudioEncoder::open(&cfg).map_err(|_| ())?);
                    return Ok(());
                }
                #[cfg(not(feature = "video-thumbnail"))]
                {
                    return Err(());
                }
            }
            AudioCodec::Opus => Err(()),
        }
    }

    /// Pull as many encoded frames as the encoder has ready given the
    /// current PCM accumulator, re-packetize them as TS, and emit.
    fn drain_encoder(&mut self, output: &mut Vec<u8>) -> Result<(), ()> {
        let audio_pid = match self.audio_pid {
            Some(p) => p,
            None => return Err(()),
        };

        // AAC branch — fdk-aac encoder.
        #[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
        {
            if let Some(ref mut enc) = self.aac_encoder {
                let frame_size = enc.frame_size() as usize;
                while self
                    .accumulator
                    .first()
                    .map(|c| c.len())
                    .unwrap_or(0)
                    >= frame_size
                {
                    let frame: Vec<Vec<f32>> = self
                        .accumulator
                        .iter_mut()
                        .map(|ch| ch.drain(..frame_size).collect())
                        .collect();
                    match enc.encode_frame(&frame) {
                        Ok(encoded) => {
                            let pes = build_audio_pes(&encoded.bytes, self.out_pts_90k);
                            let pkts = packetize_ts(audio_pid, &pes, &mut self.out_audio_cc);
                            for p in &pkts {
                                output.extend_from_slice(p);
                            }
                            let sr = self.resolved_sample_rate as u64;
                            if sr > 0 {
                                self.out_pts_90k +=
                                    (encoded.num_samples as u64) * 90_000 / sr;
                            }
                        }
                        Err(_) => {
                            // Drop this frame on error and continue.
                        }
                    }
                }
                return Ok(());
            }
        }

        // MP2 / AC-3 branch — libavcodec via video-engine.
        #[cfg(feature = "video-thumbnail")]
        {
            if let Some(ref mut enc) = self.av_encoder {
                let frame_size = enc.frame_size();
                while self
                    .accumulator
                    .first()
                    .map(|c| c.len())
                    .unwrap_or(0)
                    >= frame_size
                {
                    let frame: Vec<Vec<f32>> = self
                        .accumulator
                        .iter_mut()
                        .map(|ch| ch.drain(..frame_size).collect())
                        .collect();
                    match enc.encode_frame(&frame) {
                        Ok(frames) => {
                            for ef in frames {
                                let pes = build_audio_pes(&ef.data, self.out_pts_90k);
                                let pkts =
                                    packetize_ts(audio_pid, &pes, &mut self.out_audio_cc);
                                for p in &pkts {
                                    output.extend_from_slice(p);
                                }
                                if enc.sample_rate() > 0 {
                                    self.out_pts_90k += (ef.num_samples as u64) * 90_000
                                        / enc.sample_rate() as u64;
                                }
                            }
                        }
                        Err(_) => {}
                    }
                }
                return Ok(());
            }
        }

        Err(())
    }
}

// ────────────────────────── TS / PES helpers ──────────────────────────

/// Parse the PMT for the first audio stream. Returns `(audio_pid,
/// stream_type)`. Mirrors `parse_pmt_av_pids` in output_hls.rs but drops
/// the video PID (we don't need it here — video is untouched).
fn parse_pmt_audio(pkt: &[u8]) -> Option<(u16, u8)> {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return None;
    }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return None;
    }

    let section_length = (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let st = pkt[pos];
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

        // AAC(0x0F), MPEG-1(0x03), MPEG-2(0x04), AC-3(0x81), private(0x06).
        if matches!(st, 0x0F | 0x03 | 0x04 | 0x81 | 0x06) {
            return Some((es_pid, st));
        }
        pos += 5 + es_info_len;
    }
    None
}

/// Rewrite the audio stream_type in a PMT TS packet in place and
/// recompute the section CRC32.
fn rewrite_pmt_audio_stream_type(pkt: &mut [u8], audio_pid: u16, new_stream_type: u8) {
    let mut offset = 4;
    if ts_has_adaptation(pkt) {
        let af_len = pkt[4] as usize;
        offset = 5 + af_len;
    }
    if offset >= TS_PACKET_SIZE {
        return;
    }

    let pointer = pkt[offset] as usize;
    offset += 1 + pointer;
    if offset + 12 > TS_PACKET_SIZE || pkt[offset] != 0x02 {
        return;
    }

    let section_start = offset;
    let section_length =
        (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
    let program_info_length =
        (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);
    let data_start = offset + 12 + program_info_length;
    let data_end = (offset + 3 + section_length)
        .min(TS_PACKET_SIZE)
        .saturating_sub(4);

    let mut pos = data_start;
    while pos + 5 <= data_end {
        let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
        let es_info_len = (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);
        if es_pid == audio_pid {
            pkt[pos] = new_stream_type;
        }
        pos += 5 + es_info_len;
    }

    let crc_offset = section_start + 3 + section_length - 4;
    if crc_offset + 4 <= TS_PACKET_SIZE {
        let crc = mpeg2_crc32(&pkt[section_start..crc_offset]);
        pkt[crc_offset] = (crc >> 24) as u8;
        pkt[crc_offset + 1] = (crc >> 16) as u8;
        pkt[crc_offset + 2] = (crc >> 8) as u8;
        pkt[crc_offset + 3] = crc as u8;
    }
}

/// Extract the elementary-stream payload and PTS (90 kHz) from a complete
/// PES packet. Returns `None` on malformed input.
fn extract_pes_audio(pes: &[u8]) -> Option<(Vec<u8>, u64)> {
    if pes.len() < 9 || pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
        return None;
    }
    let header_data_len = pes[8] as usize;
    let es_start = 9 + header_data_len;
    if es_start >= pes.len() {
        return None;
    }
    let pts_dts_flags = (pes[7] >> 6) & 0x03;
    let pts = if pts_dts_flags >= 2 && pes.len() >= 14 {
        parse_pts(&pes[9..14])
    } else {
        0
    };
    Some((pes[es_start..].to_vec(), pts))
}

/// Decode the 5-byte PTS in a PES optional header.
fn parse_pts(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;
    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) << 15)
        | (b3 << 7)
        | (b4 >> 1)
}

/// Wrap an encoded audio frame in a PES packet with a PTS header.
fn build_audio_pes(audio_data: &[u8], pts: u64) -> Vec<u8> {
    let pes_len = 3 + 5 + audio_data.len();
    let mut pes = Vec::with_capacity(14 + audio_data.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    pes.push(0xC0); // audio stream_id
    pes.extend_from_slice(&(pes_len as u16).to_be_bytes());
    pes.push(0x80); // marker bits + no scramble
    pes.push(0x80); // PTS present, no DTS
    pes.push(5);    // PES header data length

    let pts = pts & 0x1_FFFF_FFFF;
    pes.push(0x21 | (((pts >> 30) as u8) & 0x0E));
    pes.push((pts >> 22) as u8);
    pes.push(0x01 | (((pts >> 15) as u8) & 0xFE));
    pes.push((pts >> 7) as u8);
    pes.push(0x01 | (((pts as u8) & 0x7F) << 1));

    pes.extend_from_slice(audio_data);
    pes
}

/// Packetize a PES payload into one or more 188-byte TS packets with the
/// given PID, advancing the caller's continuity-counter.
fn packetize_ts(pid: u16, pes: &[u8], cc: &mut u8) -> Vec<[u8; 188]> {
    let mut packets = Vec::new();
    let mut offset = 0;
    let mut is_first = true;

    while offset < pes.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];

        let pusi: u8 = if is_first { 1 } else { 0 };
        let current_cc = *cc;
        *cc = (*cc + 1) & 0x0F;

        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = (pusi << 6) | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = pid as u8;

        let remaining = pes.len() - offset;
        let payload_capacity = TS_PACKET_SIZE - 4;

        if remaining >= payload_capacity {
            pkt[3] = 0x10 | current_cc;
            pkt[4..TS_PACKET_SIZE]
                .copy_from_slice(&pes[offset..offset + payload_capacity]);
            offset += payload_capacity;
        } else {
            let stuff_len = payload_capacity - remaining;
            if stuff_len == 1 {
                pkt[3] = 0x30 | current_cc;
                pkt[4] = 0;
                pkt[5..5 + remaining].copy_from_slice(&pes[offset..]);
            } else {
                pkt[3] = 0x30 | current_cc;
                pkt[4] = (stuff_len - 1) as u8;
                if stuff_len > 1 {
                    pkt[5] = 0x00;
                    for i in 6..4 + stuff_len {
                        pkt[i] = 0xFF;
                    }
                }
                pkt[4 + stuff_len..4 + stuff_len + remaining]
                    .copy_from_slice(&pes[offset..]);
            }
            offset += remaining;
        }

        is_first = false;
        packets.push(pkt);
    }

    packets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::AudioEncodeConfig;

    fn enc(codec: &str) -> AudioEncodeConfig {
        AudioEncodeConfig {
            codec: codec.into(),
            bitrate_kbps: None,
            sample_rate: None,
            channels: None,
        }
    }

    #[test]
    fn rejects_opus_because_no_ts_mapping() {
        assert!(matches!(
            TsAudioReplacer::new(&enc("opus"), None),
            Err(TsAudioReplaceError::UnsupportedCodec(_))
        ));
    }

    #[test]
    fn rejects_unknown_codec() {
        assert!(matches!(
            TsAudioReplacer::new(&enc("flac"), None),
            Err(TsAudioReplaceError::UnknownCodec(_))
        ));
    }

    #[test]
    fn accepts_aac_lc_and_mp2_and_ac3() {
        assert!(TsAudioReplacer::new(&enc("aac_lc"), None).is_ok());
        assert!(TsAudioReplacer::new(&enc("mp2"), None).is_ok());
        assert!(TsAudioReplacer::new(&enc("ac3"), None).is_ok());
    }

    #[test]
    fn process_empty_input_is_noop() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn process_misaligned_input_is_passthrough() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        let input = vec![0x00u8; 100]; // not 188-aligned
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn process_unknown_pid_passes_through_verbatim() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        // PID 0x1FFF (null) — not the PAT, not a PMT, not audio.
        pkt[1] = 0x1F;
        pkt[2] = 0xFF;
        pkt[3] = 0x10; // payload only, CC=0

        let mut out = Vec::new();
        r.process(&pkt, &mut out);
        assert_eq!(&out[..], &pkt[..]);
    }

    #[test]
    fn packetize_ts_round_trip_single_packet() {
        let pes = vec![0xABu8; 100]; // fits in one TS payload
        let mut cc = 0u8;
        let pkts = packetize_ts(0x100, &pes, &mut cc);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0][0], TS_SYNC_BYTE);
        // PUSI set, PID high bits = 0x01
        assert_eq!(pkts[0][1] & 0x40, 0x40);
        assert_eq!(cc, 1);
    }

    #[test]
    fn build_audio_pes_has_pts_and_stream_id() {
        let pes = build_audio_pes(&[1, 2, 3, 4], 0x1234_5678);
        assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01]);
        assert_eq!(pes[3], 0xC0); // audio stream_id
        assert_eq!(pes[7], 0x80); // PTS flag
        assert_eq!(pes[8], 5);    // PES header data len
        // last 4 bytes should be the ES payload
        assert_eq!(&pes[pes.len() - 4..], &[1, 2, 3, 4]);
    }
}
