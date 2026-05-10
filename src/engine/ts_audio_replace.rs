// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

    /// Monotonic 5-bit PMT version counter for the rewritten PMT.
    /// Initialized at 1; bumped (mod 32) on every `reset_source_state()`.
    /// Stamped onto every emitted PMT via
    /// [`crate::engine::ts_parse::set_psi_version`] so receivers always
    /// see a different version when the rewrite changes audio
    /// stream_type — without it, an `A → B → A` round-trip leaves
    /// receivers' cached PMT pointing at the wrong codec.
    out_psi_version: u8,

    /// Running output PTS in 90 kHz ticks. Anchored to the first decoded
    /// PES PTS; advanced by encoded frame sample counts. Used as the
    /// fallback when the source-PTS queue is exhausted (e.g. encoder
    /// catch-up bursts).
    out_pts_90k: u64,
    /// True until the first decoded PES anchors `out_pts_90k`.
    out_pts_anchored: bool,
    /// Source PES PTSes pending output, one entry per source decoded
    /// frame fed into the encoder. Drained in FIFO order on each
    /// emitted output frame, so output PES PTS values track source's
    /// monotonic clock instead of `out_pts_90k`'s sample-counted clock.
    /// This is the path that keeps audio in lock with video when the
    /// video encoder has its own pipeline delay — the audio replacer
    /// anchors each output PES to the source PES's intrinsic PTS rather
    /// than to wall-time accumulation.
    src_pts_queue: std::collections::VecDeque<u64>,

    /// Lazily constructed AAC-LC / ADTS decoder. Opened on the first PES
    /// flush once we know the source is AAC.
    #[cfg(feature = "fdk-aac")]
    aac_decoder: Option<aac_audio::AacDecoder>,

    /// Lazily constructed FFmpeg-backed decoder for non-AAC sources
    /// (MP2 / AC-3 / E-AC-3). Opened on the first PES flush once we
    /// know the source codec from the PMT. Mirrors the AAC slot above —
    /// only one is populated at a time per replacer instance.
    #[cfg(feature = "video-thumbnail")]
    ff_decoder: Option<video_engine::AudioDecoder>,

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

    /// One-shot "input was switched" request. The flow's per-output
    /// switch watcher flips this to `true` when the active input
    /// changes; the replacer consumes it on the next `process()` entry
    /// and runs the same `reset_source_state()` path that fires on
    /// codec/PID change. Without this, a same-codec same-PID swap leaves
    /// stale decoder + PTS-anchor state from the previous input and the
    /// receiver hears wrong-epoch PTS audio frames against a master-
    /// clock-paced PCR.
    external_reset: Arc<AtomicBool>,
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
            out_psi_version: 1,
            out_pts_90k: 0,
            out_pts_anchored: false,
            src_pts_queue: std::collections::VecDeque::with_capacity(64),
            #[cfg(feature = "fdk-aac")]
            aac_decoder: None,
            #[cfg(feature = "video-thumbnail")]
            ff_decoder: None,
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
            external_reset: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Shared handle to the one-shot "input was switched" request flag.
    /// The flow's per-output switch watcher sets this to `true` when the
    /// active input changes; the replacer consumes and clears it on
    /// entry to its next `process()` call and runs the same
    /// `reset_source_state()` path that fires on codec/PID change. The
    /// audio counterpart of `TsVideoReplacer::external_reset_handle()`.
    /// Idempotent under rapid repeated switches — collapses into a
    /// single reset.
    #[allow(dead_code)]
    pub fn external_reset_handle(&self) -> Arc<AtomicBool> {
        self.external_reset.clone()
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

        // External "input was switched" trigger — see field doc on
        // `external_reset`. Same-codec same-PID swaps don't fire the
        // codec/PID-change reset path, so without this hook stale
        // decoder + PTS-anchor state leaks across the boundary.
        if self.external_reset.swap(false, Ordering::Relaxed) {
            self.reset_source_state("input switched");
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

            // Learn the PMT PID from every PAT. A PMT-PID change means
            // the input switched to an input with a different program
            // layout — reset source-side state so the pipeline
            // re-learns everything from the new program.
            if pid == PAT_PID && ts_pusi(pkt) {
                let mut programs = parse_pat_programs(pkt);
                if !programs.is_empty() {
                    programs.sort_by_key(|(num, _)| *num);
                    let new_pmt_pid = programs[0].1;
                    if self.pmt_pid != Some(new_pmt_pid) {
                        if self.pmt_pid.is_some() {
                            self.audio_pid = None;
                            self.source_stream_type = 0;
                            self.reset_source_state("PMT PID changed");
                        }
                        self.pmt_pid = Some(new_pmt_pid);
                    }
                }
            }

            // Once we know the PMT PID, parse it (and rewrite the
            // broadcast copy) on every PUSI. Re-read audio_pid and
            // source_stream_type on every PMT so input switches
            // between inputs with different audio codecs / PIDs are
            // handled seamlessly.
            if let Some(pmt_pid) = self.pmt_pid {
                if pid == pmt_pid && ts_pusi(pkt) {
                    if let Some((apid, ast)) = parse_pmt_audio(pkt) {
                        let codec_changed =
                            self.source_stream_type != 0 && self.source_stream_type != ast;
                        let pid_changed =
                            self.audio_pid.is_some() && self.audio_pid != Some(apid);
                        if codec_changed || pid_changed {
                            self.reset_source_state(&format!(
                                "source changed: stream_type {:#04x} -> {:#04x}, pid {:?} -> {}",
                                self.source_stream_type, ast, self.audio_pid, apid
                            ));
                        }
                        self.audio_pid = Some(apid);
                        self.source_stream_type = ast;
                    }
                    // Rewrite the PMT stream_type whenever the source codec
                    // is one we can replace (AAC family via fdk-aac, or
                    // MP2/AC-3/E-AC-3 via the FFmpeg-backed audio decoder).
                    // Anything else falls through to passthrough, with the
                    // PMT preserved so downstream decoders see the truth.
                    let can_replace = source_replaceable(self.source_stream_type);
                    if can_replace {
                        let mut rewritten = pkt.to_vec();
                        if let Some(apid) = self.audio_pid {
                            // Neutralise the AAC descriptor (0x7C) when re-encoding
                            // to an AAC-family target — the source descriptor's
                            // profile/level may not match the encoder we run, and
                            // a strict broadcast decoder (e.g. Appear) refuses to
                            // bring up audio on the mismatch.
                            let aac_pal = match self.codec {
                                AudioCodec::AacLc
                                | AudioCodec::HeAacV1
                                | AudioCodec::HeAacV2 => Some(0xFE),
                                _ => None,
                            };
                            rewrite_pmt_audio_stream_type(
                                &mut rewritten,
                                apid,
                                self.target_stream_type,
                                aac_pal,
                            );
                            // Stamp the per-replacer monotonic version so
                            // receivers re-parse on every codec change.
                            crate::engine::ts_parse::set_psi_version(
                                &mut rewritten,
                                self.out_psi_version,
                            );
                        }
                        output.extend_from_slice(&rewritten);
                    } else {
                        output.extend_from_slice(pkt);
                    }
                    continue;
                }
            }

            // Audio packets: route to the PES accumulator only when the
            // source codec is one we can actually decode. Anything else
            // falls through to the passthrough branch below — losing the
            // re-encode is preferable to dropping audio entirely.
            if Some(pid) == self.audio_pid && source_replaceable(self.source_stream_type) {
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

    /// Drop every pipeline stage that depends on the current source
    /// stream — decoder, transcoder, encoder, resolved format,
    /// accumulator, PES buffer, PTS anchor. Called when the source
    /// audio codec or PID changes mid-flow (seamless input switching
    /// between inputs with different audio codecs, or a PAT/PMT
    /// program re-layout).
    ///
    /// The target codec itself (`self.codec` / `self.target_stream_type`)
    /// is preserved — that's the output's configured codec, which
    /// never changes.
    fn reset_source_state(&mut self, reason: &str) {
        tracing::info!(
            "ts_audio_replace: {reason}; reopening audio decoder / encoder"
        );
        self.pes_buffer.clear();
        self.pes_started = false;
        // Re-anchor output PTS to the new input's first PES so the
        // audio stays aligned with the video replacer, which also
        // re-anchors on the video-PID codec swap.
        self.out_pts_anchored = false;
        #[cfg(feature = "fdk-aac")]
        {
            self.aac_decoder = None;
        }
        #[cfg(feature = "video-thumbnail")]
        {
            self.ff_decoder = None;
        }
        // Encoder and transcoder are both keyed off the input sample
        // rate / channels (resolved from the first decode). The new
        // input may have a different format, so tear them down and
        // let `init_encoder` / transcoder lazy-open rebuild them.
        #[cfg(all(feature = "video-thumbnail", feature = "fdk-aac"))]
        {
            self.aac_encoder = None;
        }
        #[cfg(feature = "video-thumbnail")]
        {
            self.av_encoder = None;
        }
        self.transcoder = None;
        self.accumulator.clear();
        self.src_pts_queue.clear();
        self.resolved_channels = 0;
        self.resolved_sample_rate = 0;
        self.codecs_ready = false;
        // Bump the rewritten-PMT version (mod 32) so receivers see a
        // distinct version on the next PMT and re-parse — without this,
        // `A → B → A` round-trips leave the receiver's cached PMT
        // pointing at B's audio codec when A was already the cached
        // version_number.
        self.out_psi_version = (self.out_psi_version.wrapping_add(1)) & 0x1F;
    }

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

        // ── Phase 1: decode every codec frame in this PES ──
        //
        // AAC (stream_type 0x0F) is decoded in-process via fdk-aac. The
        // ADTS framing lives inside `es_data` itself.
        //
        // MP2 / AC-3 / E-AC-3 (0x03/0x04, 0x80/0x81/0xC1, 0x87/0xC2) are
        // decoded via libavcodec — `engine::audio_decode::split_audio_codec_frames`
        // walks the PES on the codec's sync word, then each access unit
        // is fed to the FFmpeg decoder one at a time
        // (`avcodec_send_packet` decodes only one AU per call).
        struct Decoded {
            planar: Vec<Vec<f32>>,
            sample_rate: u32,
            channels: u8,
        }
        let mut decoded_frames: Vec<Decoded> = Vec::new();

        #[cfg(feature = "fdk-aac")]
        if self.source_stream_type == 0x0F {
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

        #[cfg(feature = "video-thumbnail")]
        if let Some(ff_codec) = crate::engine::audio_decode::ff_codec_for_stream_type(
            self.source_stream_type,
        ) {
            if self.ff_decoder.is_none() {
                self.ff_decoder = Some(
                    video_engine::AudioDecoder::open(ff_codec).map_err(|_| ())?,
                );
            }
            let decoder = self.ff_decoder.as_mut().unwrap();
            for au in
                crate::engine::audio_decode::split_audio_codec_frames(&es_data, ff_codec)
            {
                if decoder.send_packet(au, pts as i64).is_err() {
                    continue;
                }
                while let Ok(frame) = decoder.receive_frame() {
                    decoded_frames.push(Decoded {
                        planar: frame.planar,
                        sample_rate: frame.sample_rate,
                        channels: frame.channels,
                    });
                }
            }
        }

        if decoded_frames.is_empty()
            && self.source_stream_type != 0
            && !source_replaceable(self.source_stream_type)
        {
            // Source codec is something other than the four we decode
            // (e.g. SMPTE 302M `0x06` LPCM). Nothing to re-encode.
            return Err(());
        }

        // ── Phase 2: feed PCM into encoder ──
        {
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
                // Push the source PES PTS once per decoded frame so the
                // emit path can pop it instead of guessing the PTS from
                // a sample-count anchor. For 1:1 sample-count mappings
                // (AC-3 → AC-3, AAC → AAC at the same sample rate) this
                // collapses any offset between the audio replacer's
                // internal clock and the source's own clock down to
                // zero — the output PES carries source's intrinsic PTS
                // verbatim. For mixed mappings (AAC → AC-3) the queue
                // is still ahead of advancing-from-anchor: each output
                // PES's PTS lands within ~1 source-frame-duration of
                // the correct value (vs. up to encoder-buffer-depth
                // worth of drift accumulated from a stale anchor).
                self.src_pts_queue.push_back(pts);
                self.drain_encoder(output)?;
            }
        }

        let _ = pts;
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
                            // Prefer source PES PTS from the queue when
                            // available; fall back to the sample-counted
                            // anchor when the queue is exhausted (encoder
                            // catch-up burst).
                            let pts_for_pes = self
                                .src_pts_queue
                                .pop_front()
                                .unwrap_or(self.out_pts_90k);
                            let pes = build_audio_pes(&encoded.bytes, pts_for_pes);
                            let pkts = packetize_ts(audio_pid, &pes, &mut self.out_audio_cc);
                            for p in &pkts {
                                output.extend_from_slice(p);
                            }
                            let sr = self.resolved_sample_rate as u64;
                            if sr > 0 {
                                // Keep the fallback clock in sync from the
                                // last source PTS we popped, so a later
                                // queue-exhausted emit can still produce a
                                // monotonic value.
                                self.out_pts_90k = pts_for_pes
                                    .wrapping_add((encoded.num_samples as u64) * 90_000 / sr);
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
                                let pts_for_pes = self
                                    .src_pts_queue
                                    .pop_front()
                                    .unwrap_or(self.out_pts_90k);
                                let pes = build_audio_pes(&ef.data, pts_for_pes);
                                let pkts =
                                    packetize_ts(audio_pid, &pes, &mut self.out_audio_cc);
                                for p in &pkts {
                                    output.extend_from_slice(p);
                                }
                                if enc.sample_rate() > 0 {
                                    self.out_pts_90k = pts_for_pes.wrapping_add(
                                        (ef.num_samples as u64) * 90_000
                                            / enc.sample_rate() as u64,
                                    );
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

/// True when the source `stream_type` is one we know how to decode and
/// re-encode: AAC ADTS (0x0F) via fdk-aac, or MP2 / AC-3 / E-AC-3 via
/// the FFmpeg-backed audio decoder. Anything else falls through to
/// passthrough (no PMT rewrite, audio bytes preserved).
fn source_replaceable(stream_type: u8) -> bool {
    matches!(
        stream_type,
        0x0F | 0x03 | 0x04 | 0x80 | 0x81 | 0x87 | 0xC1 | 0xC2,
    )
}

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
///
/// When `new_aac_profile_and_level` is `Some`, additionally walk the
/// audio ES descriptor list and rewrite the body of any AAC descriptor
/// (tag 0x7C, ETSI TS 101 154 Annex G) to that value. The audio
/// re-encoder targets a fixed codec, but the source PMT's descriptor
/// is inherited verbatim from the input — when those don't agree (e.g.
/// source advertises HE-AAC but we re-encode to AAC-LC), strict
/// broadcast decoders refuse the audio output. Callers pass `0xFE`
/// ("no audio profile and level defined") to neutralise the descriptor;
/// decoders then fall back to the ADTS sync header inside the ES.
fn rewrite_pmt_audio_stream_type(
    pkt: &mut [u8],
    audio_pid: u16,
    new_stream_type: u8,
    new_aac_profile_and_level: Option<u8>,
) {
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
            if let Some(pal) = new_aac_profile_and_level {
                let desc_start = pos + 5;
                let desc_end = (desc_start + es_info_len).min(data_end);
                let mut dpos = desc_start;
                while dpos + 2 <= desc_end {
                    let tag = pkt[dpos];
                    let dlen = pkt[dpos + 1] as usize;
                    if dpos + 2 + dlen > desc_end {
                        break;
                    }
                    if tag == 0x7C && dlen >= 1 {
                        pkt[dpos + 2] = pal;
                    }
                    dpos += 2 + dlen;
                }
            }
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

/// Decode the 5-byte PTS / DTS in a PES optional header per ISO/IEC
/// 13818-1 §2.4.3.7. See `ts_video_replace::parse_pts` for the bit-by-
/// bit layout — this is the same parser kept in the audio module to
/// avoid a cross-module dependency for one helper.
fn parse_pts(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;
    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) & 0x7F) << 15
        | (b3 << 7)
        | ((b4 >> 1) & 0x7F)
}

/// Wrap an encoded audio frame in a PES packet with a PTS header.
///
/// The five-byte PTS encoding is spec-compliant per ISO/IEC 13818-1
/// §2.4.3.7 — pts bits 32, 30, and 15 land in the right slots. An
/// earlier version of this routine had off-by-one shift bugs that
/// silently dropped pts bits 30 and 15 in the encoded output, which
/// caused standard receivers (Appear, VLC, ffmpeg) to lose audio PTS
/// lock once `pts` exceeded 32 768 ticks (~ 364 ms at 90 kHz).
fn build_audio_pes(audio_data: &[u8], pts: u64) -> Vec<u8> {
    let pes_len = 3 + 5 + audio_data.len();
    let mut pes = Vec::with_capacity(14 + audio_data.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    pes.push(0xC0); // audio stream_id
    pes.extend_from_slice(&(pes_len as u16).to_be_bytes());
    // Marker bits '10' + data_alignment_indicator=1. Each encoded
    // audio frame (one ADTS frame for AAC, one MP2/AC-3 frame) is
    // emitted as its own PES, so the payload starts at an access-unit
    // boundary. ETSI TS 101 154 §C.4 requires this for broadcast.
    pes.push(0x84);
    pes.push(0x80); // PTS present, no DTS
    pes.push(5);    // PES header data length

    let pts = pts & 0x1_FFFF_FFFF;
    // 0x20 = '0010' marker for PTS-only timestamp role; OR in the top
    // 3 bits of pts (pts[32..30] in result bits 3..1) and the trailing
    // marker bit '1' at bit 0.
    pes.push(0x20 | (((pts >> 29) as u8) & 0x0E) | 0x01);
    pes.push(((pts >> 22) & 0xFF) as u8);
    pes.push((((pts >> 14) as u8) & 0xFE) | 0x01);
    pes.push(((pts >> 7) & 0xFF) as u8);
    pes.push((((pts << 1) as u8) & 0xFE) | 0x01);

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
            silent_fallback: false,
            opus_vbr_mode: None,
            opus_fec: false,
            opus_dtx: false,
            opus_frame_duration_ms: None,
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

    /// Build a single-program PAT TS packet (PMT at `pmt_pid`).
    fn synth_pat(pmt_pid: u16) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, pid hi=0
        pkt[2] = 0x00;
        pkt[3] = 0x10; // payload only, CC=0
        pkt[4] = 0x00; // pointer
        let s = 5;
        pkt[s] = 0x00; // table_id = PAT
        let section_length: u16 = 13; // txid(2)+vsn(1)+sec#(1)+last#(1) + entry(4) + CRC(4)
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00;
        pkt[s + 4] = 0x01;
        pkt[s + 5] = 0xC1;
        pkt[s + 6] = 0x00;
        pkt[s + 7] = 0x00;
        pkt[s + 8] = 0x00;
        pkt[s + 9] = 0x01;
        pkt[s + 10] = 0xE0 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[s + 11] = pmt_pid as u8;
        let crc = mpeg2_crc32(&pkt[s..s + 12]);
        pkt[s + 12] = (crc >> 24) as u8;
        pkt[s + 13] = (crc >> 16) as u8;
        pkt[s + 14] = (crc >> 8) as u8;
        pkt[s + 15] = crc as u8;
        pkt
    }

    /// Build a minimal PMT TS packet with exactly one audio ES entry.
    fn synth_pmt_audio(pmt_pid: u16, audio_pid: u16, stream_type: u8) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = pmt_pid as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let s = 5;
        pkt[s] = 0x02; // table_id = PMT
        let section_length: u16 = 18;
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00;
        pkt[s + 4] = 0x01;
        pkt[s + 5] = 0xC1;
        pkt[s + 6] = 0x00;
        pkt[s + 7] = 0x00;
        pkt[s + 8] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F); // PCR_PID = audio_pid
        pkt[s + 9] = audio_pid as u8;
        pkt[s + 10] = 0xF0;
        pkt[s + 11] = 0x00;
        pkt[s + 12] = stream_type;
        pkt[s + 13] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 14] = audio_pid as u8;
        pkt[s + 15] = 0xF0;
        pkt[s + 16] = 0x00;
        let crc = mpeg2_crc32(&pkt[s..s + 17]);
        pkt[s + 17] = (crc >> 24) as u8;
        pkt[s + 18] = (crc >> 16) as u8;
        pkt[s + 19] = (crc >> 8) as u8;
        pkt[s + 20] = crc as u8;
        pkt
    }

    #[test]
    fn synth_pat_round_trips_through_parser() {
        let pkt = synth_pat(0x1000);
        assert_eq!(parse_pat_programs(&pkt), vec![(1u16, 0x1000u16)]);
    }

    #[test]
    fn synth_pmt_round_trips_through_parser() {
        // AAC (0x0F)
        let pkt = synth_pmt_audio(0x1000, 0x0101, 0x0F);
        assert_eq!(parse_pmt_audio(&pkt), Some((0x0101, 0x0F)));
        // AC-3 (0x81)
        let pkt = synth_pmt_audio(0x1000, 0x0101, 0x81);
        assert_eq!(parse_pmt_audio(&pkt), Some((0x0101, 0x81)));
    }

    /// Seamless input switching between inputs with different audio
    /// codecs (AAC → AC-3) must reset the decoder / encoder /
    /// transcoder so the new source's PCM isn't fed into an
    /// encoder initialised for the old format.
    #[test]
    fn codec_change_on_pmt_update_resets_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        assert_eq!(r.source_stream_type, 0x0F);
        assert_eq!(r.audio_pid, Some(0x0101));

        // Simulate state built up from streaming the old input: the
        // codecs are open, output PTS has been anchored, some PCM has
        // been queued in the accumulator. The reset path must wipe
        // all of this.
        r.codecs_ready = true;
        r.out_pts_anchored = true;
        r.resolved_sample_rate = 48_000;
        r.resolved_channels = 2;
        r.accumulator = vec![vec![0.5f32; 1024], vec![0.5f32; 1024]];

        // Input switch: same PMT / audio PID, but the new input is
        // AC-3 (stream_type 0x81).
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x81), &mut out);

        assert_eq!(r.source_stream_type, 0x81, "new source codec learned");
        assert!(
            !r.codecs_ready,
            "codecs_ready must be cleared so encoders re-init for new input"
        );
        assert!(
            !r.out_pts_anchored,
            "PTS must re-anchor to the new input's timeline"
        );
        assert_eq!(r.resolved_sample_rate, 0);
        assert_eq!(r.resolved_channels, 0);
        assert!(r.accumulator.is_empty(), "stale PCM must be dropped");
    }

    /// PID-only change (same codec, different audio PID) must also
    /// reset the pipeline — the old decoder PES buffer and PCM
    /// accumulator belong to a different elementary stream.
    #[test]
    fn audio_pid_change_on_pmt_update_resets_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        r.codecs_ready = true;
        r.out_pts_anchored = true;

        r.process(&synth_pmt_audio(0x1000, 0x0102, 0x0F), &mut out);

        assert_eq!(r.audio_pid, Some(0x0102));
        assert!(!r.codecs_ready);
        assert!(!r.out_pts_anchored);
    }

    /// Regression guard: unchanged PMTs arriving many times per second
    /// must not flip the reset path, otherwise every frame would pay
    /// the cost of closing and reopening the decoder + encoder.
    #[test]
    fn repeated_unchanged_pmt_does_not_reset_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.codecs_ready = true;
        r.out_pts_anchored = true;
        r.resolved_sample_rate = 48_000;

        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);

        assert!(r.codecs_ready, "unchanged PMT must not reset codec state");
        assert!(r.out_pts_anchored);
        assert_eq!(r.resolved_sample_rate, 48_000);
    }

    /// A PAT that relocates the program to a different PMT PID must
    /// also trigger the reset path (same kind of program-level
    /// discontinuity the video replacer handles).
    #[test]
    fn pmt_pid_change_on_pat_update_resets_source_state() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();

        r.process(&synth_pat(0x1000), &mut out);
        r.process(&synth_pmt_audio(0x1000, 0x0101, 0x0F), &mut out);
        r.codecs_ready = true;
        r.out_pts_anchored = true;

        r.process(&synth_pat(0x1001), &mut out);

        assert_eq!(r.pmt_pid, Some(0x1001));
        assert_eq!(r.audio_pid, None, "audio_pid must be cleared pending new PMT");
        assert!(!r.codecs_ready);
        assert!(!r.out_pts_anchored);
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

    /// Each encoded audio frame is emitted as its own PES, so the
    /// payload starts at an access-unit boundary — ETSI TS 101 154
    /// §C.4 requires `data_alignment_indicator = 1` for broadcast.
    #[test]
    fn build_audio_pes_sets_data_alignment_indicator() {
        let pes = build_audio_pes(&[0u8; 16], 0);
        // Byte 6 carries the marker + DAI flag. Bit 2 (0x04) is DAI.
        assert_eq!(pes[6] & 0x04, 0x04, "data_alignment_indicator must be 1");
    }

    /// Build a PMT TS packet with one audio ES carrying an
    /// ISO-639 language descriptor + an AAC descriptor (0x7C) with the
    /// caller-supplied `profile_and_level` body byte. Mirrors the
    /// real-world DVB shape we see on Sky Sports recordings.
    fn synth_pmt_audio_with_aac_desc(
        pmt_pid: u16,
        audio_pid: u16,
        stream_type: u8,
        aac_profile_and_level: u8,
    ) -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = pmt_pid as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let s = 5;
        pkt[s] = 0x02; // table_id = PMT
        // 9 (header) + 5 (ES fixed) + 9 (descriptors: 0x0A len 4 + 0x7C len 1) + 4 (CRC) = 27
        let section_length: u16 = 27;
        pkt[s + 1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        pkt[s + 2] = section_length as u8;
        pkt[s + 3] = 0x00;
        pkt[s + 4] = 0x01;
        pkt[s + 5] = 0xC1;
        pkt[s + 6] = 0x00;
        pkt[s + 7] = 0x00;
        pkt[s + 8] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 9] = audio_pid as u8;
        pkt[s + 10] = 0xF0;
        pkt[s + 11] = 0x00;
        pkt[s + 12] = stream_type;
        pkt[s + 13] = 0xE0 | ((audio_pid >> 8) as u8 & 0x1F);
        pkt[s + 14] = audio_pid as u8;
        // ES_info_length = 9 (4-byte ISO-639 desc + 1-byte AAC desc + 2x 2-byte tag/len)
        pkt[s + 15] = 0xF0;
        pkt[s + 16] = 0x09;
        // ISO-639 language descriptor (0x0A), len 4: "eng" + audio_type 0
        pkt[s + 17] = 0x0A;
        pkt[s + 18] = 0x04;
        pkt[s + 19] = b'e';
        pkt[s + 20] = b'n';
        pkt[s + 21] = b'g';
        pkt[s + 22] = 0x00;
        // AAC descriptor (0x7C), len 1: profile_and_level
        pkt[s + 23] = 0x7C;
        pkt[s + 24] = 0x01;
        pkt[s + 25] = aac_profile_and_level;
        let crc = mpeg2_crc32(&pkt[s..s + 26]);
        pkt[s + 26] = (crc >> 24) as u8;
        pkt[s + 27] = (crc >> 16) as u8;
        pkt[s + 28] = (crc >> 8) as u8;
        pkt[s + 29] = crc as u8;
        pkt
    }

    /// Re-encoding to AAC-LC must neutralise the inherited AAC
    /// descriptor's profile_and_level so a strict broadcast decoder
    /// doesn't refuse audio output when the source advertised HE-AAC
    /// but we emit AAC-LC.
    #[test]
    fn pmt_aac_descriptor_neutralised_for_aac_target() {
        let mut r = TsAudioReplacer::new(&enc("aac_lc"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        // Source advertises HE-AAC L3 (0x51) but the ES (we don't
        // feed any here) would carry plain AAC-LC after re-encode.
        let pmt = synth_pmt_audio_with_aac_desc(0x1000, 0x0101, 0x0F, 0x51);
        r.process(&pmt, &mut out);
        assert_eq!(out.len(), 188);
        // ts header (4) + pointer (1) + table_id (1) + section_length (2)
        //   + program_number (2) + version (1) + section# (2) + PCR_PID (2)
        //   + program_info_length (2) = 17 → ES entry starts here.
        // ES: stream_type (1) + es_pid (2) + es_info_length (2) = 5 → desc list at 22.
        // Desc list: ISO-639 (2 + 4 = 6 bytes) → AAC tag at 28, len at 29, body at 30.
        assert_eq!(out[5 + 23], 0x7C, "AAC descriptor tag still present");
        assert_eq!(out[5 + 24], 0x01, "AAC descriptor body length unchanged");
        assert_eq!(
            out[5 + 25],
            0xFE,
            "AAC profile_and_level neutralised (was 0x51 HE-AAC, now 0xFE = unspecified)"
        );
        // Section CRC must be valid after the in-place rewrite.
        let section_length =
            (((out[5 + 1] & 0x0F) as usize) << 8) | (out[5 + 2] as usize);
        let crc_off = 5 + 3 + section_length - 4;
        let computed = mpeg2_crc32(&out[5..crc_off]);
        let stored = ((out[crc_off] as u32) << 24)
            | ((out[crc_off + 1] as u32) << 16)
            | ((out[crc_off + 2] as u32) << 8)
            | (out[crc_off + 3] as u32);
        assert_eq!(computed, stored, "CRC32 must match after descriptor rewrite");
    }

    /// Re-encoding to a non-AAC target (MP2 / AC-3) must NOT touch the
    /// AAC descriptor body — the descriptor is irrelevant to a non-AAC
    /// stream_type but we still preserve the source bytes verbatim.
    #[test]
    fn pmt_aac_descriptor_left_alone_for_non_aac_target() {
        let mut r = TsAudioReplacer::new(&enc("ac3"), None).unwrap();
        let mut out = Vec::new();
        r.process(&synth_pat(0x1000), &mut out);
        out.clear();
        let pmt = synth_pmt_audio_with_aac_desc(0x1000, 0x0101, 0x0F, 0x51);
        r.process(&pmt, &mut out);
        assert_eq!(out.len(), 188);
        assert_eq!(out[5 + 25], 0x51, "non-AAC target leaves descriptor body intact");
    }
}
