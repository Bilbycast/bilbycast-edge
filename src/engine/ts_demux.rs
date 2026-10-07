// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MPEG-TS demuxer for WebRTC output.
//!
//! Extracts H.264 NAL units and Opus audio frames from an MPEG-TS stream
//! carried in `RtpPacket` broadcast channel messages. Uses PAT/PMT parsing
//! from `ts_parse.rs` to discover elementary stream PIDs.

use std::collections::HashMap;

use crate::engine::ts_parse::*;

/// Stream type constants from ISO/IEC 13818-1.
const STREAM_TYPE_MPEG1_VIDEO: u8 = 0x01;
const STREAM_TYPE_MPEG2_VIDEO: u8 = 0x02;
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_H265: u8 = 0x24;
const STREAM_TYPE_PRIVATE: u8 = 0x06;
const STREAM_TYPE_AAC_ADTS: u8 = 0x0F;
/// MPEG-TS `stream_type` for AAC carried over LATM/LOAS — the framing
/// every Australian / Asian DVB-T AAC service uses (e.g. Seven AU
/// program 1334) and a common HE-AAC contribution carriage. Each PES
/// is a sequence of LOAS audio-sync-stream frames: 11-bit sync `0x2B7`
/// + 13-bit length + LATM `AudioMuxElement`. Decoded via libavcodec's
/// `AAC_LATM` codec (see `bilbycast-ffmpeg-video-rs`); fdk-aac stays on
/// ADTS.
const STREAM_TYPE_AAC_LATM: u8 = 0x11;

/// Audio codec carried on `stream_type = 0x06` (PES private data),
/// disambiguated by a DVB / Opus / AC-4 descriptor in the PMT ES-info loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateAudioKind {
    /// DVB AC-3 — ETSI TS 101 154 § 5.3, descriptor tag `0x6A`.
    Ac3,
    /// DVB E-AC-3 — ETSI TS 101 154 § 5.3, descriptor tag `0x7A`.
    Eac3,
    /// Opus — registration descriptor `0x05` with `Opus` identifier.
    Opus,
    /// LATM/LOAS AAC — DVB AAC descriptor (tag `0x7C`) on a private
    /// stream. Routed as effective `stream_type = 0x11` so the LATM
    /// PES + decoder paths handle it.
    AacLatm,
    /// Dolby AC-4 — ETSI TS 101 154 § 5.7 / ATSC A/342-2, descriptor
    /// tag `0xAC` (also signalled via registration descriptor `0x05`
    /// with `AC-4` identifier). Always carried on `stream_type = 0x06`;
    /// no ATSC private stream-type is assigned. Detect-and-passthrough
    /// only — bilbycast cannot decode or encode AC-4 (no open-source
    /// codec exists; FFmpeg ships only an AC-4 raw demuxer/parser).
    Ac4,
}

/// Synthetic MPEG-TS stream-type marker we attach to AC-4 PIDs so the
/// downstream pipeline (PES routing, assembler keying, libavcodec
/// selection) has a single non-`0x06` value to dispatch on. ISO/IEC
/// 13818-1 leaves `0xAC` unassigned, and we deliberately reuse the
/// AC-4 descriptor tag value to make tracing self-documenting.
pub const SYNTHETIC_STREAM_TYPE_AC4: u8 = 0xAC;

/// Extracted media frame from the TS demuxer.
pub enum DemuxedFrame {
    /// Stream discontinuity signal — the demuxer detected that the upstream
    /// source changed (today: PMT `version_number` advanced, which is what
    /// `TsContinuityFixer::on_switch` guarantees on every operator switch via
    /// its monotonic counter). Consumers that own decoder state (the local-
    /// display output) should flush their decoders and reset PTS / clock
    /// anchors so the new stream starts cleanly without waiting for the next
    /// natural IDR to time out a stale reference-frame queue. Re-muxing
    /// consumers (RTMP / WebRTC TS demuxers) can ignore this variant; their
    /// downstream encoder pipelines re-anchor on the next IDR independently.
    /// Emitted at most once per discontinuity event, at the head of the
    /// `demux()` return Vec.
    Discontinuity,
    /// Complete SCTE-35 `splice_info_section` from a PMT stream_type 0x86 PID.
    // The section bytes are read only by the SDI VANC SCTE-35 injection path
    // (`engine::output_sdi`, `#[cfg(feature = "sdi-decklink")]`); every other
    // consumer matches `Scte35(_)`. The variant is always constructed, so keep
    // the payload and silence the unused-field lint when SDI is compiled out.
    #[cfg_attr(not(feature = "sdi-decklink"), allow(dead_code))]
    Scte35(Vec<u8>),
    /// Complete H.264 access unit (one or more NALUs in Annex B format).
    H264 {
        /// NAL units with 0x00000001 start codes stripped.
        /// Each entry is a single NALU (header byte + body).
        nalus: Vec<Vec<u8>>,
        /// Presentation timestamp in 90 kHz clock ticks — 0 when the PES
        /// carried none (see `pts_known`).
        pts: u64,
        /// Whether this is a keyframe (contains IDR NALU).
        is_keyframe: bool,
        /// Whether the PES carried a PTS. MPEG-TS needs one only every
        /// 700 ms, and some encoders stamp only their I pictures: a
        /// decoder fed `pts` for such an AU hands its picture back stamped
        /// 0, which is neither the picture's time nor a frame to measure a
        /// rate on.
        pts_known: bool,
    },
    /// Complete H.265 / HEVC access unit (one or more NALUs in Annex B format).
    H265 {
        /// NAL units with 0x00000001 start codes stripped.
        nalus: Vec<Vec<u8>>,
        /// Presentation timestamp in 90 kHz clock ticks — 0 when the PES
        /// carried none (see `pts_known`).
        pts: u64,
        /// Whether this is a keyframe (IDR_W_RADL, IDR_N_LP, or CRA_NUT).
        is_keyframe: bool,
        /// Whether the PES carried a PTS (see `H264::pts_known`).
        pts_known: bool,
    },
    /// Complete MPEG-1 / MPEG-2 video access unit. The bitstream is fed
    /// to the libavcodec `mpeg2video` decoder verbatim — there are no
    /// parameter-set NALUs in this codec; instead each GoP starts with
    /// a `sequence_header` (`0x000001B3`) optionally followed by a
    /// `sequence_extension` (`0x000001B5`) and the first I-picture, and
    /// the decoder picks them off the input bytestream itself.
    Mpeg2 {
        /// Raw elementary stream bytes — the payload after the PES
        /// header, with no further framing.
        es: Vec<u8>,
        /// Presentation timestamp in 90 kHz clock ticks — 0 when the PES
        /// carried none (see `pts_known`).
        pts: u64,
        /// `true` when this AU contains an I-picture
        /// (`picture_coding_type == 1` in the picture header). Drives
        /// thumbnail-anchor and replay-IDR-index selection.
        is_keyframe: bool,
        /// Whether the PES carried a PTS (see `H264::pts_known`).
        pts_known: bool,
    },
    /// Opus audio access unit, payload-bearing. Carries one or more
    /// Opus-in-MPEG-TS packetised frames (per the Opus-in-MPEG-TS spec —
    /// 11-bit `control_header_prefix = 0x3FF` sentinel, followed by
    /// flag-driven optional fields, followed by a self-delimited Opus
    /// frame). Consumers walk the PES payload, strip control headers,
    /// and feed each Opus frame to libopus / libavcodec one at a time.
    Opus {
        /// PES payload bytes (post-PES-header). The ETSI / ISO opus-in-TS
        /// control headers ride in front of each Opus frame inside this
        /// buffer.
        #[cfg(any(all(feature = "display", target_os = "linux"), feature = "webrtc"))]
        data: Vec<u8>,
        /// Presentation timestamp in 90 kHz clock ticks — 0 when the PES
        /// carried none (see `pts_known`).
        #[cfg(any(all(feature = "display", target_os = "linux"), feature = "webrtc"))]
        pts: u64,
        /// Whether the PES carried a PTS. The WebRTC passthrough continues
        /// a PES without one from where the previous one's packets ended
        /// rather than stamping it 0.
        #[cfg(any(all(feature = "display", target_os = "linux"), feature = "webrtc"))]
        #[cfg_attr(not(feature = "webrtc"), allow(dead_code))]
        pts_known: bool,
    },
    /// One AAC access unit (raw, ADTS header stripped), emitted as soon as
    /// it is complete — cut across PES boundaries, so an AU that straddles
    /// two PES is one frame like any other.
    Aac {
        /// Raw AAC frame data (without ADTS header).
        data: Vec<u8>,
        /// Presentation timestamp in 90 kHz clock ticks: its PES's PTS when
        /// it is the first AU that commences in that PES, else where the
        /// samples before it end.
        pts: u64,
    },
    /// Non-AAC compressed audio frame (MP2 / AC-3 / E-AC-3 / AC-4) —
    /// payload is the elementary stream the FFmpeg audio decoder consumes
    /// (concatenated frames pre-PES). Only emitted when the audio PID's
    /// stream type is one we recognise. Consumers that don't handle the
    /// reported codec should ignore the variant — AC-4 in particular has
    /// no decoder available and is surfaced solely for passthrough,
    /// labelling, and per-PID stats.
    OtherAudio {
        /// MPEG-TS stream_type from the PMT (0x03/0x04 = MP2,
        /// 0x80/0x81/0xC1 = AC-3, 0x87/0xC2 = E-AC-3,
        /// 0xAC = AC-4 [synthetic — see [`SYNTHETIC_STREAM_TYPE_AC4`]]).
        stream_type: u8,
        /// The whole access units that commenced in one PES —
        /// concatenated, an AU that straddles into the next PES included,
        /// never a fragment (MP2 / AC-3 / E-AC-3 / LATM, cut by the shared
        /// `audio_au` cutter). AC-4 is the whole PES payload. The receiver
        /// splits on codec sync words (0x0B 0x77 for AC-3 / E-AC-3,
        /// MPEG-1 sync for MP2, LOAS sync for LATM).
        data: Vec<u8>,
        /// Presentation timestamp in 90 kHz clock ticks: that PES's PTS.
        pts: u64,
    },
}

/// Verdict of [`sniff_annexb_codec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SniffedVideoCodec {
    H264,
    Hevc,
}

/// Best-effort H.264-vs-HEVC discrimination from one access unit's
/// Annex-B NAL units, independent of the PMT `stream_type` byte.
///
/// The demuxer labels access units purely from the PMT, so a PMT that
/// lies (assembly config typo, external encoder fault, source codec
/// change without a PMT update) feeds wrong-codec NALs to downstream
/// decoders, which fail silently. This sniffer gives consumers an
/// independent opinion so they can raise an event and recover.
///
/// Scoring uses NAL-header shapes that are valid in one codec and
/// invalid (or spec-reserved) in the other:
/// - H.264 (1-byte header `f(1) | nal_ref_idc(2) | type(5)`): SPS (7),
///   PPS (8) and IDR (5) must carry `nal_ref_idc != 0`; the equivalent
///   byte values land on reserved HEVC types (50–52).
/// - HEVC (2-byte header `f(1) | type(6) | layer(6) | tid+1(3)`):
///   VPS/SPS/PPS/AUD (32–35) and IRAP slices (16–21) with a valid
///   `nuh_temporal_id_plus1 != 0` and `layer_id == 0`; their first
///   bytes decode as H.264 types that either violate `nal_ref_idc`
///   constraints (e.g. SEI with ref_idc set) or are rare partitions.
///
/// Returns `None` when the AU carries no decisive NALU (pure
/// inter-frame AUs without parameter sets often don't) — callers
/// accumulate over multiple AUs.
pub fn sniff_annexb_codec(nalus: &[Vec<u8>]) -> Option<SniffedVideoCodec> {
    let mut h264: u32 = 0;
    let mut hevc: u32 = 0;
    for nalu in nalus {
        if nalu.len() < 2 {
            continue;
        }
        let b0 = nalu[0];
        if b0 & 0x80 != 0 {
            continue; // forbidden_zero_bit set — corrupt either way
        }
        let h264_type = b0 & 0x1F;
        let h264_ref_idc = (b0 >> 5) & 0x03;
        let hevc_type = (b0 >> 1) & 0x3F;
        let hevc_layer_id = ((b0 & 0x01) << 5) | (nalu[1] >> 3);
        let hevc_tid_ok = nalu[1] & 0x07 != 0;

        if matches!(h264_type, 5 | 7 | 8) && h264_ref_idc != 0 {
            h264 += 2;
        }
        if hevc_tid_ok && hevc_layer_id == 0 {
            if matches!(hevc_type, 32..=35) {
                hevc += 2;
            } else if matches!(hevc_type, 16..=21) {
                hevc += 1;
            }
        }
    }
    // Unanimous evidence decides outright; otherwise require a strong
    // majority. The majority arm matters because one decisive HEVC NAL
    // aliases an H.264-voting byte: IDR_N_LP (type 20, layer 0) is
    // first-byte 0x28 = H.264 PPS with nal_ref_idc=1 — so an AUD-less
    // closed-GOP HEVC IRAP AU ([VPS, SPS, PPS, IDR_N_LP] = hevc 7,
    // h264 2) would never reach a unanimous verdict and a lying PMT
    // over that bitstream class would stay undetected forever.
    if h264 >= 2 && hevc == 0 {
        Some(SniffedVideoCodec::H264)
    } else if hevc >= 2 && h264 == 0 {
        Some(SniffedVideoCodec::Hevc)
    } else if h264 >= 4 && h264 >= 3 * hevc {
        Some(SniffedVideoCodec::H264)
    } else if hevc >= 4 && hevc >= 3 * h264 {
        Some(SniffedVideoCodec::Hevc)
    } else {
        None
    }
}

/// Access-unit framing of the selected audio PID, for the formats the
/// shared cutter frames (ADTS, LATM / LOAS, MPEG-1 audio, AC-3 / E-AC-3).
///
/// The audio PES are not parsed one by one: an AU that straddles two PES
/// (legal whenever `data_alignment_indicator` is 0, and done by some
/// broadcast muxers — Sky Sports Arena three times a loop) was lost, and
/// with it, the next PES then opening on its tail, every AU of the next PES
/// (the ADTS walk stopped at the first byte that was not a sync word). The
/// TS audio replacer's cutter (`engine::audio_au`) rebuilds the stream and
/// cuts whole AUs across PES boundaries, for every consumer of this
/// demuxer: the display, SDI, CMAF, RTMP, WebRTC, ST 2110-30 and replay
/// export.
struct AudioAuFramer {
    stream_type: u8,
    cutter: crate::engine::audio_au::AuCutter,
    /// `OtherAudio`: the AUs that commenced in the current PES, not yet
    /// emitted, and that PES's presentation time.
    group: Vec<u8>,
    group_pts: u64,
    /// Where the next AU is presented: the last PES PTS, the samples since
    /// it, at `rate` (0 = unknown — LATM does not say in its header).
    anchor: Option<(u64, u64, u32)>,
}

impl AudioAuFramer {
    fn new(stream_type: u8, fmt: crate::engine::audio_au::AuFormat) -> Self {
        Self {
            stream_type,
            cutter: crate::engine::audio_au::AuCutter::new(fmt),
            group: Vec::new(),
            group_pts: 0,
            anchor: None,
        }
    }

    /// Presentation time of `au` — its PES's PTS when it is the first AU
    /// that commences there, else where the samples before it end — and
    /// count its samples on. 0 until a PTS has been seen, as a PES without
    /// one always read.
    fn place(&mut self, au: &crate::engine::audio_au::CutAu) -> u64 {
        let h = au.header;
        if au.pes_start
            && let Some(pts) = au.pts
        {
            self.anchor = Some((pts, 0, h.sample_rate));
        }
        let Some((base, samples, rate)) = self.anchor.as_mut() else {
            return 0;
        };
        if *rate != h.sample_rate && h.sample_rate != 0 {
            // A rate change: count on from here at the new rate.
            let at = if *rate == 0 { *base } else { *base + *samples * 90_000 / *rate as u64 };
            *base = at;
            *samples = 0;
            *rate = h.sample_rate;
        }
        let pts = if *rate == 0 { *base } else { *base + *samples * 90_000 / *rate as u64 };
        *samples += h.samples as u64;
        pts & 0x1_FFFF_FFFF
    }

    /// The pending `OtherAudio` group, if any.
    fn take_group(&mut self) -> Option<DemuxedFrame> {
        if self.group.is_empty() {
            return None;
        }
        Some(DemuxedFrame::OtherAudio {
            stream_type: self.stream_type,
            data: std::mem::take(&mut self.group),
            pts: self.group_pts,
        })
    }
}

/// Per-PID PES reassembly state.
struct PesAssembler {
    /// Accumulated PES data.
    buffer: Vec<u8>,
    /// Whether we've seen the start of a PES packet.
    started: bool,
    /// Stream type from PMT.
    stream_type: u8,
}

/// Hard cap on a single PES reassembly buffer for a video elementary stream.
/// A stream that stops emitting a payload-unit-start (PUSI) — malformed or
/// adversarial — would otherwise grow this buffer at line rate until the
/// process is OOM-killed.
///
/// This bounds ONE coded access unit (frame), not throughput: 100s-of-Mbps
/// feeds pass through fine because each frame is flushed on the next PUSI.
/// The cap is sized well above the largest single I-frame of a high-bitrate
/// (~500 Mbps, long-GOP, low-fps) contribution feed (~20–25 MiB worst case),
/// while still fully defeating the unbounded-growth DoS.
const PES_ASSEMBLER_CAP_VIDEO: usize = 64 * 1024 * 1024;

/// Hard cap for non-video (audio / data / subtitle) PES reassembly. These
/// access units are small; the cap only exists to bound the same DoS.
const PES_ASSEMBLER_CAP_OTHER: usize = 1024 * 1024;

/// Cap for a PES buffer given its PMT `stream_type`.
fn pes_assembler_cap(stream_type: u8) -> usize {
    match stream_type {
        // MPEG-1/2 video, MPEG-4 part 2, H.264, HEVC, VVC, AVS2/AVS3, VC-1.
        0x01 | 0x02 | 0x10 | 0x1B | 0x24 | 0x33 | 0x42 | 0xD1 | 0xEA => PES_ASSEMBLER_CAP_VIDEO,
        _ => PES_ASSEMBLER_CAP_OTHER,
    }
}

/// MPEG-TS demuxer that extracts elementary stream frames.
pub struct TsDemuxer {
    /// Optional MPEG-TS program_number selector. If `None`, the demuxer
    /// locks onto the program with the lowest program_number found in the
    /// PAT (deterministic default for MPTS inputs). If `Some(N)`, only the
    /// PMT for program N is honoured; other programs are ignored.
    target_program: Option<u16>,
    /// Audio track selector for multi-audio-track programs. If `None`, the
    /// first audio PID in the PMT is used (default). If `Some(N)`, the Nth
    /// audio track (0-based) is selected.
    audio_track_index: Option<u8>,
    /// PMT PID we're locked onto (set after the first PAT is parsed).
    selected_pmt_pid: Option<u16>,
    /// Video PID (H.264 or H.265) discovered from the selected PMT.
    video_pid: Option<u16>,
    /// Video stream type.
    video_stream_type: u8,
    /// Audio PID discovered from the selected PMT (after track selection).
    audio_pid: Option<u16>,
    /// SCTE-35 section PID discovered from stream_type 0x86.
    scte35_pid: Option<u16>,
    /// Per-PID PES reassembly.
    pes_assemblers: HashMap<u16, PesAssembler>,
    /// AU framing of the audio PID, for the formats the shared cutter frames
    /// (`None`: its PES go through `pes_assemblers`, as Opus and AC-4 do).
    audio_au: Option<AudioAuFramer>,
    /// Cached SPS NALU for late joiners.
    cached_sps: Option<Vec<u8>>,
    /// Cached PPS NALU for late joiners.
    cached_pps: Option<Vec<u8>>,
    /// Cached HEVC VPS NALU.
    cached_h265_vps: Option<Vec<u8>>,
    /// Cached HEVC SPS NALU (distinct from `cached_sps` to avoid collision
    /// with H.264 when the PMT codec changes mid-flow).
    cached_h265_sps: Option<Vec<u8>>,
    /// Cached HEVC PPS NALU.
    cached_h265_pps: Option<Vec<u8>>,
    /// Cached AAC config: (profile, sample_rate_index, channel_config). Refreshed
    /// from each ADTS header so it always reflects the most recent AAC parameters
    /// and is preserved across PMT codec changes (an export window that crosses
    /// an input switch from AAC → AC-3 still has the AAC config available for
    /// the AAC samples it captured before the switch).
    cached_aac_config: Option<(u8, u8, u8)>,
    /// The `channel_config_code` the PMT signals for the selected audio
    /// track when it is Opus (`ts_parse::opus_channel_config_code`), `None`
    /// when that track is not Opus or its PMT entry carries no Opus
    /// extension descriptor. Refreshed on every PMT, so a switch to a 5.1
    /// Opus source on the same PID is seen.
    opus_channel_config: Option<u8>,
    /// Last PMT `version_number` (5 bits) seen for the locked program. A change
    /// trips `pending_discontinuity`, which surfaces a single
    /// [`DemuxedFrame::Discontinuity`] at the head of the next `demux()` return.
    /// `TsContinuityFixer::on_switch` advances this value monotonically on every
    /// operator switch, so this is a reliable "input switched" signal.
    pmt_version: Option<u8>,
    /// Set when a stream discontinuity has been observed (today: PMT version
    /// change) and a [`DemuxedFrame::Discontinuity`] is owed at the head of
    /// the next `demux()` return Vec. Cleared after that emission.
    pending_discontinuity: bool,
    /// Cross-packet section reassembly for the PAT. Without this, a PAT
    /// or PMT spanning more than one TS packet (real broadcast MPTS
    /// muxes — e.g. a 214-byte PMT with HEVC + AC-4 + 3× E-AC-3 +
    /// subtitle descriptor loops) silently truncated at 188 bytes and
    /// the demuxer never discovered the audio PIDs.
    pat_section: crate::engine::ts_parse::SectionAssembler,
    /// Cross-packet section reassembly for the selected PMT.
    pmt_section: crate::engine::ts_parse::SectionAssembler,
    /// program_number of the locked program (PMT sections are matched on
    /// it; a PMT PID may be shared by several programs).
    selected_program: Option<u16>,
    /// True when the PAT maps more than one program to the selected PMT
    /// PID. Only then must a PMT's program_number match exactly; on an
    /// unshared PID the first PMT is accepted as before (a mux whose PMT
    /// program_number disagrees with its PAT keeps working).
    selected_pmt_pid_shared: bool,
    scte35_section: crate::engine::ts_parse::SectionAssembler,
}

impl TsDemuxer {
    /// Create a new demuxer.
    ///
    /// `target_program` selects the MPEG-TS program to extract elementary
    /// streams from:
    /// - `None` — lock onto the lowest program_number in the PAT (deterministic
    ///   default; behaves identically for single-program TS).
    /// - `Some(N)` — extract only program N. If N is not present in the PAT,
    ///   no frames are produced until N appears.
    pub fn new(target_program: Option<u16>) -> Self {
        Self {
            target_program,
            audio_track_index: None,
            selected_pmt_pid: None,
            video_pid: None,
            video_stream_type: 0,
            audio_pid: None,
            scte35_pid: None,
            pes_assemblers: HashMap::new(),
            audio_au: None,
            cached_sps: None,
            cached_pps: None,
            cached_h265_vps: None,
            cached_h265_sps: None,
            cached_h265_pps: None,
            cached_aac_config: None,
            opus_channel_config: None,
            pmt_version: None,
            pending_discontinuity: false,
            pat_section: crate::engine::ts_parse::SectionAssembler::new(),
            pmt_section: crate::engine::ts_parse::SectionAssembler::new(),
            selected_program: None,
            selected_pmt_pid_shared: false,
            scte35_section: crate::engine::ts_parse::SectionAssembler::new(),
        }
    }

    /// Create a new demuxer with audio track selection.
    ///
    /// `audio_track_index` selects which audio track to extract when the
    /// PMT contains multiple audio elementary streams:
    /// - `None` — use the first audio track found (default).
    /// - `Some(0)` — first audio track, `Some(1)` — second, etc.
    ///
    /// If the requested track index exceeds the number of audio tracks in
    /// the PMT, the first audio track is used as a fallback.
    pub fn with_audio_track(target_program: Option<u16>, audio_track_index: Option<u8>) -> Self {
        Self {
            target_program,
            audio_track_index,
            selected_pmt_pid: None,
            video_pid: None,
            video_stream_type: 0,
            audio_pid: None,
            scte35_pid: None,
            pes_assemblers: HashMap::new(),
            audio_au: None,
            cached_sps: None,
            cached_pps: None,
            cached_h265_vps: None,
            cached_h265_sps: None,
            cached_h265_pps: None,
            cached_aac_config: None,
            opus_channel_config: None,
            pmt_version: None,
            pending_discontinuity: false,
            pat_section: crate::engine::ts_parse::SectionAssembler::new(),
            pmt_section: crate::engine::ts_parse::SectionAssembler::new(),
            selected_program: None,
            selected_pmt_pid_shared: false,
            scte35_section: crate::engine::ts_parse::SectionAssembler::new(),
        }
    }

    /// Get the cached SPS NALU (for SDP or late-joiner keyframe injection).
    pub fn cached_sps(&self) -> Option<&[u8]> {
        self.cached_sps.as_deref()
    }

    /// Get the cached PPS NALU.
    pub fn cached_pps(&self) -> Option<&[u8]> {
        self.cached_pps.as_deref()
    }

    /// Get the cached HEVC VPS NALU.
    pub fn cached_h265_vps(&self) -> Option<&[u8]> {
        self.cached_h265_vps.as_deref()
    }

    /// Get the cached HEVC SPS NALU.
    pub fn cached_h265_sps(&self) -> Option<&[u8]> {
        self.cached_h265_sps.as_deref()
    }

    /// Get the cached HEVC PPS NALU.
    pub fn cached_h265_pps(&self) -> Option<&[u8]> {
        self.cached_h265_pps.as_deref()
    }

    /// Source video stream_type as seen in the PMT (0x1B = H.264, 0x24 = HEVC, 0 = unknown).
    #[allow(dead_code)]
    pub fn video_stream_type(&self) -> u8 {
        self.video_stream_type
    }

    /// Video PID currently locked from the selected program's PMT.
    /// `None` until the first PMT carrying a video ES is parsed.
    /// Used by the local-display output to surface the active video PID
    /// on the confidence overlay for broadcast-pro monitoring.
    #[allow(dead_code)]
    pub fn video_pid(&self) -> Option<u16> {
        self.video_pid
    }

    /// Audio PID currently locked from the selected program's PMT
    /// (post-`audio_track_index` selection). `None` until the first PMT
    /// carrying audio ES (or before track selection lands on an audio
    /// stream) is parsed. Same display-overlay use case as
    /// [`Self::video_pid`].
    #[allow(dead_code)]
    pub fn audio_pid(&self) -> Option<u16> {
        self.audio_pid
    }

    /// Active program number, if the demuxer was constructed with one.
    /// Returns `None` when defaulting to the lowest program in the PAT.
    #[allow(dead_code)]
    pub fn target_program(&self) -> Option<u16> {
        self.target_program
    }

    /// Get the cached AAC config: (profile, sample_rate_index, channel_config).
    /// Parsed from the first ADTS header encountered.
    pub fn cached_aac_config(&self) -> Option<(u8, u8, u8)> {
        self.cached_aac_config
    }

    /// The Opus `channel_config_code` the PMT signals for the selected
    /// audio track — `None` when it is not Opus or signals none (see
    /// `opus_channel_config`).
    #[cfg(feature = "webrtc")]
    pub fn opus_channel_config(&self) -> Option<u8> {
        self.opus_channel_config
    }

    /// Process TS payload bytes (from an RtpPacket, after RTP header stripping).
    /// Returns any completed frames.
    ///
    /// If a stream discontinuity was detected during processing (PMT
    /// `version_number` change — what `TsContinuityFixer::on_switch` produces
    /// on every operator switch), exactly one [`DemuxedFrame::Discontinuity`]
    /// is emitted at the head of the returned Vec so consumers can flush
    /// decoder state before the next frame is fed in.
    pub fn demux(&mut self, ts_data: &[u8]) -> Vec<DemuxedFrame> {
        let mut frames = Vec::new();
        let mut offset = 0;

        while offset + TS_PACKET_SIZE <= ts_data.len() {
            let pkt = &ts_data[offset..offset + TS_PACKET_SIZE];
            if pkt[0] == TS_SYNC_BYTE {
                let mut new_frames = self.process_ts_packet(pkt);
                if self.pending_discontinuity && !new_frames.is_empty() {
                    frames.push(DemuxedFrame::Discontinuity);
                    self.pending_discontinuity = false;
                }
                frames.append(&mut new_frames);
            }
            offset += TS_PACKET_SIZE;
        }

        // If we detected a discontinuity but no completed frames came out of
        // this datagram (typical: the PMT-only packet that signalled the
        // switch carries no ES), drain it now so the consumer flushes
        // before the next frame.
        if self.pending_discontinuity {
            frames.insert(0, DemuxedFrame::Discontinuity);
            self.pending_discontinuity = false;
        }

        frames
    }

    /// Extract a TS packet's PSI payload slice (after the adaptation
    /// field, pointer_field still included — `SectionAssembler::feed`
    /// honours it), or `None` when the packet carries no payload.
    fn psi_payload(pkt: &[u8]) -> Option<&[u8]> {
        if !ts_has_payload(pkt) {
            return None;
        }
        let offset = if ts_has_adaptation(pkt) {
            5 + pkt[4] as usize
        } else {
            4
        };
        if offset >= TS_PACKET_SIZE {
            return None;
        }
        Some(&pkt[offset..])
    }

    fn process_ts_packet(&mut self, pkt: &[u8]) -> Vec<DemuxedFrame> {
        let pid = ts_pid(pkt);

        // PAT — pick the program we want to lock onto. Sections are
        // reassembled across TS packets so muxes whose PAT / PMT exceed
        // one packet (big DTT / cable line-ups) still parse.
        if pid == PAT_PID {
            let Some(payload) = Self::psi_payload(pkt) else {
                return Vec::new();
            };
            // The assembler yields every section the packet completed;
            // keep the last complete PAT.
            let Some(mut programs) = self
                .pat_section
                .feed(ts_pusi(pkt), payload)
                .filter(|s| s.first() == Some(&0x00))
                .map(crate::engine::ts_parse::parse_pat_section_programs)
                .last()
            else {
                return Vec::new();
            };
            if programs.is_empty() {
                return Vec::new();
            }
            // Sort by program_number ascending so the default (lowest) is
            // deterministic across runs.
            programs.sort_by_key(|(num, _)| *num);
            let selected = match self.target_program {
                Some(target) => programs.iter().find(|(num, _)| *num == target).copied(),
                None => programs.first().copied(),
            };
            self.selected_program = selected.map(|(num, _)| num);
            let new_pmt_pid = selected.map(|(_, pid)| pid);
            self.selected_pmt_pid_shared = new_pmt_pid
                .is_some_and(|pmt| programs.iter().filter(|(_, p)| *p == pmt).count() > 1);
            if new_pmt_pid != self.selected_pmt_pid {
                if let Some(new_pid) = new_pmt_pid {
                    tracing::info!(
                        "TS demux: locked onto program{} (PMT PID 0x{:04X})",
                        self.target_program
                            .map(|n| format!(" {}", n))
                            .unwrap_or_default(),
                        new_pid
                    );
                }
                // PMT PID change after we'd already locked onto a different
                // one is a stream discontinuity (operator switched program,
                // or a re-mux re-numbered PIDs). The very first lock-on
                // (`selected_pmt_pid` was None) is not — there's nothing
                // for the consumer to flush yet.
                let was_locked = self.selected_pmt_pid.is_some();
                self.selected_pmt_pid = new_pmt_pid;
                self.video_pid = None;
                self.audio_pid = None;
                self.scte35_pid = None;
                self.pes_assemblers.clear();
                self.audio_au = None;
                self.pmt_version = None;
                if was_locked {
                    self.pending_discontinuity = true;
                }
            }
            return Vec::new();
        }

        // PMT — only honour the PMT for our locked program. The PID may
        // carry other tables ahead of it (ATSC / DigiCipher 0xC0 sections)
        // and other programs' PMTs. The program's PMT is chosen exactly as
        // the transcode stages choose it (`ts_pmt_edit::pmt_index`): an
        // exact program_number match wins; on a PID the PAT gives to no
        // other program the first PMT is accepted too.
        if Some(pid) == self.selected_pmt_pid {
            if let Some(payload) = Self::psi_payload(pkt) {
                let sections: Vec<Vec<u8>> = self
                    .pmt_section
                    .feed(ts_pusi(pkt), payload)
                    .filter(|s| s.first() == Some(&0x02))
                    .map(|s| s.to_vec())
                    .collect();
                let chosen = match self.selected_program {
                    Some(p) => crate::engine::ts_pmt_edit::pmt_index(
                        &sections,
                        p,
                        self.selected_pmt_pid_shared,
                    ),
                    None => (!sections.is_empty()).then_some(0),
                };
                if let Some(i) = chosen {
                    self.parse_pmt(&sections[i]);
                }
            }
            return Vec::new();
        }

        // Video PID
        if Some(pid) == self.video_pid {
            return self.process_es_packet(pkt, pid);
        }

        // Audio PID
        if Some(pid) == self.audio_pid {
            if self.audio_au.is_some() {
                return self.process_audio_packet(pkt);
            }
            return self.process_es_packet(pkt, pid);
        }

        if Some(pid) == self.scte35_pid {
            let Some(payload) = Self::psi_payload(pkt) else { return Vec::new(); };
            return self.scte35_section
                .feed(ts_pusi(pkt), payload)
                .filter(|section| section.first() == Some(&0xfc))
                .map(|section| DemuxedFrame::Scte35(section.to_vec()))
                .collect();
        }

        Vec::new()
    }

    /// Parse a complete PMT **section** (starting at `table_id`, as
    /// delivered by the `SectionAssembler`) to discover video and audio
    /// PIDs. Multi-packet PMTs parse in full — the previous
    /// packet-level parser truncated at 188 bytes and never saw ES
    /// entries past the first packet on real broadcast MPTS muxes.
    ///
    /// Collects all audio elementary streams in PMT order, then selects the
    /// one at `audio_track_index` (or the first if unset / out of range).
    fn parse_pmt(&mut self, pkt: &[u8]) {
        let offset = 0usize;
        if pkt.len() < 16 || pkt[offset] != 0x02 {
            return; // Not a complete PMT section
        }

        let section_length =
            (((pkt[offset + 1] & 0x0F) as usize) << 8) | (pkt[offset + 2] as usize);
        if offset + 3 + section_length > pkt.len() || section_length < 13 {
            return;
        }
        // PMT version_number — 5 bits at offset+5, bits [5:1].
        // `TsContinuityFixer::on_switch` advances this monotonically (mod 32)
        // on every operator switch, including switches to dead inputs, so any
        // change is a reliable "input switched" signal. The first PMT we see
        // is recorded but does NOT trigger a discontinuity (consumers haven't
        // started decoding yet).
        let version = (pkt[offset + 5] >> 1) & 0x1F;
        match self.pmt_version {
            Some(prev) if prev != version => {
                tracing::debug!(
                    prev_version = prev,
                    new_version = version,
                    "TS demux: PMT version change → discontinuity",
                );
                self.pending_discontinuity = true;
            }
            _ => {}
        }
        self.pmt_version = Some(version);
        let program_info_length =
            (((pkt[offset + 10] & 0x0F) as usize) << 8) | (pkt[offset + 11] as usize);

        let data_start = offset + 12 + program_info_length;
        let data_end = (offset + 3 + section_length)
            .min(pkt.len())
            .saturating_sub(4);

        // Collect all audio tracks in PMT order for track selection.
        let mut audio_tracks: Vec<(u16, u8)> = Vec::new(); // (es_pid, stream_type)
        // The Opus tracks' channel_config_code, if signalled.
        let mut opus_codes: Vec<(u16, Option<u8>)> = Vec::new();

        let mut pos = data_start;
        while pos + 5 <= data_end {
            let stream_type = pkt[pos];
            let es_pid = ((pkt[pos + 1] as u16 & 0x1F) << 8) | pkt[pos + 2] as u16;
            let es_info_length =
                (((pkt[pos + 3] & 0x0F) as usize) << 8) | (pkt[pos + 4] as usize);

            // Walk ES-info descriptors. DVB carries AC-3 / E-AC-3 / Opus on
            // `stream_type = 0x06` (PES private data) plus a codec-specific
            // descriptor — that's how every non-ATSC broadcaster (UK Sky,
            // European DTT, AU DVB-S/T) signals these. ATSC additionally
            // uses 0x80/0x81/0x87/etc. directly. Detect the descriptor and
            // synthesise the matching ATSC-style stream_type so the rest
            // of the pipeline (assembler keying, parse_pes routing,
            // libavcodec selection) handles both signalling styles
            // uniformly. Tags per ETSI TS 101 154 § 5.3 (AC-3 0x6A,
            // E-AC-3 0x7A) and the Opus-in-MPEG-TS spec (registration
            // descriptor with `Opus` identifier).
            let desc_start = pos + 5;
            let desc_end = (desc_start + es_info_length).min(data_end);
            let descriptors = &pkt[desc_start..desc_end];
            let dvb_audio_kind = if stream_type == STREAM_TYPE_PRIVATE {
                self.detect_private_audio_descriptor(descriptors)
            } else {
                None
            };
            // Effective stream_type used for routing / assembler keying.
            let effective_type = match dvb_audio_kind {
                Some(PrivateAudioKind::Ac3) => 0x81,
                Some(PrivateAudioKind::Eac3) => 0x87,
                Some(PrivateAudioKind::Opus) => STREAM_TYPE_PRIVATE,
                Some(PrivateAudioKind::AacLatm) => STREAM_TYPE_AAC_LATM,
                Some(PrivateAudioKind::Ac4) => SYNTHETIC_STREAM_TYPE_AC4,
                None => stream_type,
            };

            match stream_type {
                0x86 => {
                    self.scte35_pid = Some(es_pid);
                }
                STREAM_TYPE_MPEG1_VIDEO | STREAM_TYPE_MPEG2_VIDEO => {
                    let pid_changed = self.video_pid != Some(es_pid);
                    let codec_changed = self.video_stream_type != stream_type;
                    if pid_changed || codec_changed {
                        tracing::info!(
                            "TS demux: video PID 0x{:04X} (stream_type=0x{:02X}, MPEG-2)",
                            es_pid,
                            stream_type
                        );
                        self.video_pid = Some(es_pid);
                        self.video_stream_type = stream_type;
                        self.pes_assemblers.insert(
                            es_pid,
                            PesAssembler {
                                buffer: Vec::with_capacity(256 * 1024),
                                started: false,
                                stream_type,
                            },
                        );
                        // No SPS/PPS to drop for MPEG-2, but the H.264 /
                        // H.265 caches must clear so a previous-stream
                        // anchor doesn't bleed into the new bitstream.
                        self.cached_sps = None;
                        self.cached_pps = None;
                        self.cached_h265_vps = None;
                        self.cached_h265_sps = None;
                        self.cached_h265_pps = None;
                    }
                }
                STREAM_TYPE_H264 | STREAM_TYPE_H265 => {
                    // PID change OR codec change on the same PID — both
                    // happen on operator input switching (HEVC source ↔
                    // H.264 source often share PID 0x100). The PES
                    // assembler caches the codec, so a stale stream_type
                    // routes the new bitstream through the wrong NALU
                    // parser and decoded frames stop flowing.
                    let pid_changed = self.video_pid != Some(es_pid);
                    let codec_changed = self.video_stream_type != stream_type;
                    if pid_changed || codec_changed {
                        tracing::info!(
                            "TS demux: video PID 0x{:04X} (stream_type=0x{:02X})",
                            es_pid,
                            stream_type
                        );
                        self.video_pid = Some(es_pid);
                        self.video_stream_type = stream_type;
                        self.pes_assemblers.insert(
                            es_pid,
                            PesAssembler {
                                buffer: Vec::with_capacity(256 * 1024),
                                started: false,
                                stream_type,
                            },
                        );
                        // The codec parameter sets we cached for the
                        // previous stream are obsolete. Drop them so the
                        // next IDR's SPS/PPS/VPS becomes the new anchor.
                        self.cached_sps = None;
                        self.cached_pps = None;
                        self.cached_h265_vps = None;
                        self.cached_h265_sps = None;
                        self.cached_h265_pps = None;
                    }
                }
                STREAM_TYPE_PRIVATE if dvb_audio_kind.is_some() => {
                    if dvb_audio_kind == Some(PrivateAudioKind::Opus) {
                        opus_codes.push((
                            es_pid,
                            crate::engine::ts_parse::opus_channel_config_code(descriptors),
                        ));
                    }
                    // Use the synthesised ATSC-style marker for AC-3 /
                    // E-AC-3 / AC-4; Opus stays on 0x06 (its parse_pes
                    // arm gates on `STREAM_TYPE_PRIVATE if Some(pid) ==
                    // audio_pid`). AC-4 carries `SYNTHETIC_STREAM_TYPE_AC4`
                    // (0xAC) so the assembler / parse_pes route AC-4 PES
                    // through the OtherAudio passthrough path rather than
                    // mis-handling it as Opus.
                    audio_tracks.push((es_pid, effective_type));
                }
                STREAM_TYPE_AAC_ADTS | STREAM_TYPE_AAC_LATM => {
                    audio_tracks.push((es_pid, stream_type));
                }
                // MP2 (0x03/0x04), AC-3 (0x80/0x81/0xC1), E-AC-3 (0x87/0xC2)
                // — surface so the local-display ALSA path can decode
                // via libavcodec.
                0x03 | 0x04 | 0x80 | 0x81 | 0x87 | 0xC1 | 0xC2 => {
                    audio_tracks.push((es_pid, stream_type));
                }
                // Some non-DVB ATSC 3.0 muxers stamp AC-4 directly at
                // `stream_type = 0xAC` (the AC-4 descriptor tag value)
                // instead of `0x06 + descriptor`. Surface that flavour
                // here too — handled identically downstream.
                SYNTHETIC_STREAM_TYPE_AC4 => {
                    audio_tracks.push((es_pid, SYNTHETIC_STREAM_TYPE_AC4));
                }
                _ => {}
            }

            pos += 5 + es_info_length;
        }

        // Select the audio track based on audio_track_index.
        if !audio_tracks.is_empty() {
            let idx = self
                .audio_track_index
                .map(|i| (i as usize).min(audio_tracks.len() - 1))
                .unwrap_or(0);
            let (selected_pid, selected_type) = audio_tracks[idx];
            let opus_code = opus_codes
                .iter()
                .find(|(pid, _)| *pid == selected_pid && selected_type == STREAM_TYPE_PRIVATE)
                .and_then(|(_, code)| *code);
            if let Some(code) = opus_code
                && self.opus_channel_config != opus_code
            {
                tracing::info!(
                    "TS demux: Opus audio PID 0x{:04X} channel_config_code 0x{:02X}",
                    selected_pid,
                    code,
                );
            }
            self.opus_channel_config = opus_code;

            // Same PID-or-codec change story as video — switching from an
            // AC-3 source to an AAC source on the same audio PID would
            // otherwise keep the AC-3 PES assembler and route AAC bytes
            // through the wrong path.
            let audio_pid_changed = self.audio_pid != Some(selected_pid);
            let audio_codec_changed = self
                .audio_pid
                .and_then(|pid| self.pes_assemblers.get(&pid))
                .is_none_or(|a| a.stream_type != selected_type);
            if audio_pid_changed || audio_codec_changed {
                let codec_name = match selected_type {
                    STREAM_TYPE_AAC_ADTS => "AAC",
                    STREAM_TYPE_AAC_LATM => "AAC-LATM",
                    STREAM_TYPE_PRIVATE => "Opus",
                    0x03 | 0x04 => "MP2",
                    0x80 | 0x81 | 0xC1 => "AC-3",
                    0x87 | 0xC2 => "E-AC-3",
                    SYNTHETIC_STREAM_TYPE_AC4 => "AC-4 (passthrough — no decoder)",
                    _ => "unknown",
                };
                tracing::info!(
                    "TS demux: {} audio PID 0x{:04X} (track {}/{} selected{})",
                    codec_name,
                    selected_pid,
                    idx,
                    audio_tracks.len(),
                    self.audio_track_index
                        .map(|i| format!(", requested index {i}"))
                        .unwrap_or_default(),
                );
                self.audio_pid = Some(selected_pid);
                self.audio_au = crate::engine::audio_au::AuFormat::for_stream_type(selected_type)
                    .map(|fmt| AudioAuFramer::new(selected_type, fmt));
                self.pes_assemblers.insert(
                    selected_pid,
                    PesAssembler {
                        buffer: Vec::with_capacity(16 * 1024),
                        started: false,
                        stream_type: selected_type,
                    },
                );
                // `cached_aac_config` is intentionally kept across codec
                // changes — every ADTS header refreshes it, and the export
                // path needs the AAC config to remain available for the
                // frames it captured before the switch.
            }
        }
    }

    /// Inspect the ES-info descriptor loop for an audio codec carried on
    /// `stream_type = 0x06` (private_data).
    ///
    /// Delegates to the shared `ts_parse::descriptor_audio_kind` — the
    /// classifier the PSI catalogue / pid-override rewriter / PTS
    /// rewriter already use — so every TS pipeline in this binary
    /// surfaces the same audio PIDs. The hand-rolled parser this
    /// replaces missed the `registration_descriptor` forms ("AC-3" /
    /// "EAC3" / DVB AAC 0x7C…), which is what ffmpeg's `mpegts` muxer
    /// emits — the display path went silent on every such source while
    /// the catalogue happily classified it as audio (2026-06-11).
    ///
    /// Returns `None` for non-audio private streams and for audio
    /// families with no decode path here (DTS, SMPTE 302M) — those PIDs
    /// are not surfaced to the audio path.
    fn detect_private_audio_descriptor(&self, descriptors: &[u8]) -> Option<PrivateAudioKind> {
        use crate::engine::ts_parse::PrivateEsAudioKind;
        match crate::engine::ts_parse::descriptor_audio_kind(descriptors)? {
            PrivateEsAudioKind::Ac3 => Some(PrivateAudioKind::Ac3),
            PrivateEsAudioKind::Eac3 => Some(PrivateAudioKind::Eac3),
            PrivateEsAudioKind::Opus => Some(PrivateAudioKind::Opus),
            PrivateEsAudioKind::AacLatm => Some(PrivateAudioKind::AacLatm),
            PrivateEsAudioKind::Ac4 => Some(PrivateAudioKind::Ac4),
            // No decoder in this binary — don't surface to the audio
            // path (PES would route to a decoder that can't open).
            PrivateEsAudioKind::Dts | PrivateEsAudioKind::Smpte302m => None,
        }
    }

    /// Process a TS packet of the audio PID through its AU framer: an
    /// `Aac` frame per ADTS AU (header stripped) the moment it is complete,
    /// and for the other formats an `OtherAudio` per PES — the whole AUs
    /// that commenced in it, including one that straddled into the next —
    /// with that PES's PTS.
    fn process_audio_packet(&mut self, pkt: &[u8]) -> Vec<DemuxedFrame> {
        let mut out = Vec::new();
        let Some(a) = self.audio_au.as_mut() else {
            return out;
        };
        // A PES boundary with nothing pending: every AU of the previous PES
        // is out, so its group is complete (the latency a per-PES parse had).
        if ts_has_payload(pkt) && ts_pusi(pkt) && a.cutter.buffered() == 0 {
            out.extend(a.take_group());
        }
        if !a.cutter.push_packet(pkt) {
            return out;
        }
        while let Some(au) = a.cutter.next(false) {
            let pts = a.place(&au);
            if a.stream_type == STREAM_TYPE_AAC_ADTS {
                let d = &au.data;
                let header_len = if d[1] & 0x01 != 0 { 7 } else { 9 };
                let cfg = ((d[2] >> 6) & 0x03, (d[2] >> 2) & 0x0F, ((d[2] & 0x01) << 2) | ((d[3] >> 6) & 0x03));
                if self.cached_aac_config != Some(cfg) {
                    tracing::debug!(
                        "AAC config: profile={}, sample_rate_idx={}, channels={}",
                        cfg.0 + 1, cfg.1, cfg.2,
                    );
                    self.cached_aac_config = Some(cfg);
                }
                out.push(DemuxedFrame::Aac { data: d[header_len..].to_vec(), pts });
            } else {
                if au.pes_start {
                    out.extend(a.take_group());
                }
                if a.group.is_empty() {
                    a.group_pts = pts;
                }
                a.group.extend_from_slice(&au.data);
            }
        }
        out
    }

    /// Process a TS packet belonging to a known ES PID (video or audio).
    fn process_es_packet(&mut self, pkt: &[u8], pid: u16) -> Vec<DemuxedFrame> {
        if !ts_has_payload(pkt) {
            return Vec::new();
        }

        let pusi = ts_pusi(pkt);
        let payload_start = ts_payload_offset(pkt);
        if payload_start >= TS_PACKET_SIZE {
            return Vec::new();
        }
        let payload = &pkt[payload_start..];

        // Extract the PES data to parse before mutably borrowing assembler
        let completed_pes = if pusi {
            if let Some(assembler) = self.pes_assemblers.get(&pid) {
                if assembler.started && !assembler.buffer.is_empty() {
                    let pes_data = assembler.buffer.clone();
                    let stream_type = assembler.stream_type;
                    Some((pes_data, stream_type))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Parse any completed PES (before we modify the assembler)
        let result = completed_pes
            .map(|(pes_data, stream_type)| self.parse_pes(&pes_data, pid, stream_type))
            .unwrap_or_default();

        // Now update the assembler
        if let Some(assembler) = self.pes_assemblers.get_mut(&pid) {
            if pusi {
                assembler.buffer.clear();
                assembler.buffer.extend_from_slice(payload);
                assembler.started = true;
            } else if assembler.started {
                if assembler
                    .buffer
                    .len()
                    .saturating_add(payload.len())
                    > pes_assembler_cap(assembler.stream_type)
                {
                    // No PUSI arrived to flush this PES — malformed or
                    // adversarial. Drop the partial buffer and resync on the
                    // next PUSI rather than growing without bound (OOM guard).
                    assembler.buffer.clear();
                    assembler.started = false;
                } else {
                    assembler.buffer.extend_from_slice(payload);
                }
            }
        }

        result
    }

    /// Parse a complete PES packet and extract the elementary stream frame(s).
    fn parse_pes(&mut self, pes: &[u8], pid: u16, stream_type: u8) -> Vec<DemuxedFrame> {
        // PES header: 0x000001 + stream_id(1) + length(2) + flags(2) + header_data_len(1)
        if pes.len() < 9 || pes[0] != 0x00 || pes[1] != 0x00 || pes[2] != 0x01 {
            return Vec::new();
        }

        let header_data_len = pes[8] as usize;
        let es_start = 9 + header_data_len;
        if es_start >= pes.len() {
            return Vec::new();
        }

        // Parse PTS if present
        let pts_dts_flags = (pes[7] >> 6) & 0x03;
        let pts = if pts_dts_flags >= 2 && pes.len() >= 14 {
            Some(parse_pts(&pes[9..14]))
        } else {
            None
        };

        let es_data = &pes[es_start..];

        match stream_type {
            STREAM_TYPE_MPEG1_VIDEO | STREAM_TYPE_MPEG2_VIDEO => {
                if es_data.is_empty() {
                    return Vec::new();
                }
                // MPEG-2 picture_coding_type lives at bits 13..15 of the
                // 4-byte header that follows `0x00000100`. Type 1 = I-frame.
                let is_keyframe = mpeg2_au_is_keyframe(es_data);
                vec![DemuxedFrame::Mpeg2 {
                    es: es_data.to_vec(),
                    pts: pts.unwrap_or(0),
                    is_keyframe,
                    pts_known: pts.is_some(),
                }]
            }
            STREAM_TYPE_H264 => {
                let nalus = self.extract_h264_nalus(es_data);
                if nalus.is_empty() {
                    return Vec::new();
                }
                let is_keyframe = nalus.iter().any(|n| !n.is_empty() && (n[0] & 0x1F) == 5);
                vec![DemuxedFrame::H264 {
                    nalus,
                    pts: pts.unwrap_or(0),
                    is_keyframe,
                    pts_known: pts.is_some(),
                }]
            }
            STREAM_TYPE_H265 => {
                let nalus = self.extract_h265_nalus(es_data);
                if nalus.is_empty() {
                    return Vec::new();
                }
                // HEVC NAL unit type is bits 1..6 of the first header byte.
                // All IRAP types are valid random-access points: BLA_W_LP = 16,
                // BLA_W_RADL = 17, BLA_N_LP = 18 (bitstream splicers emit
                // these), IDR_W_RADL = 19, IDR_N_LP = 20, CRA_NUT = 21.
                let is_keyframe = nalus.iter().any(|n| {
                    if n.is_empty() {
                        return false;
                    }
                    let nt = (n[0] >> 1) & 0x3F;
                    matches!(nt, 16..=21)
                });
                vec![DemuxedFrame::H265 {
                    nalus,
                    pts: pts.unwrap_or(0),
                    is_keyframe,
                    pts_known: pts.is_some(),
                }]
            }
            STREAM_TYPE_PRIVATE if Some(pid) == self.audio_pid => {
                vec![DemuxedFrame::Opus {
                    #[cfg(any(all(feature = "display", target_os = "linux"), feature = "webrtc"))]
                    data: es_data.to_vec(),
                    #[cfg(any(all(feature = "display", target_os = "linux"), feature = "webrtc"))]
                    pts: pts.unwrap_or(0),
                    #[cfg(any(all(feature = "display", target_os = "linux"), feature = "webrtc"))]
                    pts_known: pts.is_some(),
                }]
            }
            // ADTS, MP2, AC-3 / E-AC-3 and AAC-LATM never get here: the
            // audio PID's AU framer cuts them across PES boundaries
            // (`process_audio_packet`). AC-4 (synthetic 0xAC — passthrough
            // only, no decoder available) is surfaced per PES so consumers
            // can label it; they must ignore the bytes and leave the audio
            // track silent.
            SYNTHETIC_STREAM_TYPE_AC4 if Some(pid) == self.audio_pid => {
                vec![DemuxedFrame::OtherAudio {
                    stream_type,
                    data: es_data.to_vec(),
                    pts: pts.unwrap_or(0),
                }]
            }
            _ => Vec::new(),
        }
    }

    /// Extract individual H.264 NALUs from Annex B byte stream.
    /// Splits on 0x00000001 or 0x000001 start codes.
    fn extract_h264_nalus(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let raw = split_annex_b_nalus(data);
        let mut nalus = Vec::with_capacity(raw.len());
        for nalu in raw {
            let nalu_type = nalu[0] & 0x1F;
            // Cache SPS/PPS for late joiners
            if nalu_type == 7 {
                self.cached_sps = Some(nalu.clone());
            } else if nalu_type == 8 {
                self.cached_pps = Some(nalu.clone());
            }
            nalus.push(nalu);
        }
        nalus
    }

    /// Extract individual HEVC NALUs from Annex B byte stream, caching
    /// VPS / SPS / PPS for late joiners.
    fn extract_h265_nalus(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let raw = split_annex_b_nalus(data);
        let mut nalus = Vec::with_capacity(raw.len());
        for nalu in raw {
            let nalu_type = (nalu[0] >> 1) & 0x3F;
            match nalu_type {
                32 => self.cached_h265_vps = Some(nalu.clone()),
                33 => self.cached_h265_sps = Some(nalu.clone()),
                34 => self.cached_h265_pps = Some(nalu.clone()),
                _ => {}
            }
            nalus.push(nalu);
        }
        nalus
    }
}

/// Locate the first `picture_start_code` (`0x00000100`) in an MPEG-2
/// access unit and read `picture_coding_type` from the 6-bit field at
/// bits 13..15 of the 4-byte payload that follows. Type 1 = I-picture
/// (random-access point); 2 = P, 3 = B. Returns `false` when no picture
/// header is found in the AU (parameter-only AUs, partial reassembly).
pub(crate) fn mpeg2_au_is_keyframe(es: &[u8]) -> bool {
    let mut i = 0;
    while i + 6 <= es.len() {
        if es[i] == 0x00
            && es[i + 1] == 0x00
            && es[i + 2] == 0x01
            && es[i + 3] == 0x00
        {
            // picture header layout (after the 4-byte start code):
            // bits 0..9   temporal_reference (10 bits)
            // bits 10..12 picture_coding_type (3 bits)
            // ...
            // The 3-bit coding type spans the low bit of byte[i+4] and
            // top 2 bits of byte[i+5].
            let b4 = es[i + 4];
            let b5 = es[i + 5];
            let coding_type = ((b4 & 0x01) << 2) | ((b5 >> 6) & 0x03);
            return coding_type == 1;
        }
        i += 1;
    }
    false
}

/// Split an Annex-B byte stream into individual NAL units. Start codes
/// (0x000001 or 0x00000001) are stripped from each emitted slice.
///
/// Shared between H.264 and HEVC paths — Annex-B framing is identical; only
/// the NAL-header parsing differs.
pub(crate) fn split_annex_b_nalus(data: &[u8]) -> Vec<Vec<u8>> {
    let mut nalus = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let (start_code_len, found) = if i + 3 < data.len()
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x00
            && data[i + 3] == 0x01
        {
            (4, true)
        } else if i + 2 < data.len()
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x01
        {
            (3, true)
        } else {
            (0, false)
        };

        if !found {
            i += 1;
            continue;
        }

        let nalu_start = i + start_code_len;
        let mut nalu_end = data.len();
        let mut j = nalu_start + 1;
        while j + 2 < data.len() {
            if data[j] == 0x00
                && data[j + 1] == 0x00
                && (data[j + 2] == 0x01
                    || (j + 3 < data.len() && data[j + 2] == 0x00 && data[j + 3] == 0x01))
            {
                nalu_end = j;
                break;
            }
            j += 1;
        }

        if nalu_start < nalu_end {
            let nalu = &data[nalu_start..nalu_end];
            if !nalu.is_empty() {
                nalus.push(nalu.to_vec());
            }
        }

        i = nalu_end;
    }

    nalus
}

/// Parse a 5-byte PTS field from PES header.
fn parse_pts(data: &[u8]) -> u64 {
    ((data[0] as u64 & 0x0E) << 29)
        | ((data[1] as u64) << 22)
        | ((data[2] as u64 & 0xFE) << 14)
        | ((data[3] as u64) << 7)
        | ((data[4] as u64) >> 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VH1.ts: a 0xC0 section sits at the pointer target of every PMT-PID
    /// packet, the PMT behind it. The demuxer (RTMP / WebRTC / display /
    /// SDI / 2110 egress / thumbnails) used to take the first section only
    /// and never found an ES.
    #[test]
    fn pmt_behind_a_private_section_is_demuxed() {
        use crate::engine::ts_test_fixtures::{vh1_pat_packet, vh1_pmt_packet};
        let mut demux = TsDemuxer::new(None);
        let mut ts = vh1_pat_packet().to_vec();
        ts.extend_from_slice(&vh1_pmt_packet());
        demux.demux(&ts);
        assert_eq!(demux.video_pid(), Some(0x0E0F));
        assert_eq!(demux.audio_pid(), Some(0x0E10));
    }

    /// A single-program mux whose PMT carries a program_number other than
    /// the one its PAT names (remuxed / misconfigured encoders) must still
    /// demux: the PMT PID is not shared, so the first PMT on it is the
    /// program's — the rule `ts_pmt_edit::pmt_index` applies everywhere
    /// else. On a PID the PAT shares between programs, only an exact match
    /// counts.
    #[test]
    fn pmt_program_number_mismatch_falls_back_on_an_unshared_pid() {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        let pmt = pmt_section(2, 0, 0x101, &[], &[(0x1B, 0x101, &[]), (0x0F, 0x102, &[])]);
        let pmt_pkt = packetize_sections(0x100, &[&pmt], 0)[0];

        let mut demux = TsDemuxer::new(None);
        let mut ts = pat_packet(&[(1, 0x100)], 0, 0).to_vec();
        ts.extend_from_slice(&pmt_pkt);
        demux.demux(&ts);
        assert_eq!(demux.video_pid(), Some(0x101), "unshared PID: first PMT accepted");
        assert_eq!(demux.audio_pid(), Some(0x102));

        // Shared PID (programs 1 and 3 both on 0x100): program 2's PMT is
        // no one's.
        let mut demux = TsDemuxer::new(None);
        let mut ts = pat_packet(&[(1, 0x100), (3, 0x100)], 0, 0).to_vec();
        ts.extend_from_slice(&pmt_pkt);
        demux.demux(&ts);
        assert_eq!(demux.video_pid(), None, "shared PID: exact match only");
    }

    /// A video PES without a PTS (MPEG-TS needs one only every 700 ms; some
    /// encoders stamp only their I pictures) is surfaced as such: `pts` 0
    /// with `pts_known` false, where it used to be indistinguishable from a
    /// picture stamped 0.
    #[test]
    fn a_video_pes_without_a_pts_says_so() {
        use crate::engine::ts_test_fixtures::{packetize_sections, pat_packet, pmt_section};
        let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[])]);
        let mut ts = pat_packet(&[(1, 0x1000)], 0, 0).to_vec();
        ts.extend_from_slice(&packetize_sections(0x1000, &[&pmt], 0)[0]);
        let au: &[u8] = &[0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x41, 0x9A, 0x00];
        for (cc, pts) in [Some(900_000u64), None, Some(907_200)].into_iter().enumerate() {
            let mut pes = vec![0, 0, 1, 0xE0, 0, 0, 0x80];
            match pts {
                Some(p) => {
                    pes.extend_from_slice(&[0x80, 5]);
                    pes.extend_from_slice(&[
                        0x21 | ((p >> 29) as u8 & 0x0E),
                        (p >> 22) as u8,
                        ((p >> 14) as u8 & 0xFE) | 1,
                        (p >> 7) as u8,
                        ((p << 1) as u8) | 1,
                    ]);
                }
                None => pes.extend_from_slice(&[0x00, 0]),
            }
            pes.extend_from_slice(au);
            // One packet: adaptation-field stuffing, then the PES.
            let stuffing = 184 - pes.len();
            let mut pkt = vec![0x47, 0x41, 0x00, 0x30 | cc as u8, (stuffing - 1) as u8, 0x00];
            pkt.extend(std::iter::repeat_n(0xFF, stuffing - 2));
            pkt.extend_from_slice(&pes);
            ts.extend_from_slice(&pkt);
        }
        let mut demux = TsDemuxer::new(None);
        let got: Vec<(u64, bool)> = demux
            .demux(&ts)
            .into_iter()
            .filter_map(|f| match f {
                DemuxedFrame::H264 { pts, pts_known, .. } => Some((pts, pts_known)),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec![(900_000, true), (0, false)]);
    }

    /// H.264 parameter sets + IDR (SPS 0x67 / PPS 0x68 / IDR 0x65)
    /// sniff decisively as H.264; HEVC VPS/SPS/PPS (0x40/0x42/0x44 with
    /// valid tid) sniff as HEVC; non-decisive slices return None.
    #[test]
    fn sniff_annexb_codec_discriminates() {
        let h264_idr_au: Vec<Vec<u8>> = vec![
            vec![0x67, 0x64, 0x00, 0x28], // SPS
            vec![0x68, 0xEB, 0xEC, 0xB2], // PPS
            vec![0x65, 0x88, 0x84, 0x00], // IDR slice
        ];
        assert_eq!(sniff_annexb_codec(&h264_idr_au), Some(SniffedVideoCodec::H264));

        let hevc_idr_au: Vec<Vec<u8>> = vec![
            vec![0x40, 0x01, 0x0C, 0x01], // VPS
            vec![0x42, 0x01, 0x01, 0x01], // SPS
            vec![0x44, 0x01, 0xC1, 0x72], // PPS
            vec![0x26, 0x01, 0xAF, 0x06], // IDR_W_RADL
        ];
        assert_eq!(sniff_annexb_codec(&hevc_idr_au), Some(SniffedVideoCodec::Hevc));

        // Pure inter AUs carry no decisive markers.
        let h264_p_au: Vec<Vec<u8>> = vec![vec![0x41, 0x9A, 0x00, 0x01]];
        assert_eq!(sniff_annexb_codec(&h264_p_au), None);
        let hevc_trail_au: Vec<Vec<u8>> = vec![vec![0x02, 0x01, 0xD0, 0x09]];
        assert_eq!(sniff_annexb_codec(&hevc_trail_au), None);

        // AUD-less closed-GOP HEVC: IDR_N_LP (0x28) aliases an
        // H.264-PPS-shaped byte, so unanimity is impossible — the
        // strong-majority arm must still call it HEVC (x265
        // no-open-gop output through our own TsMuxer looks like this).
        let hevc_idr_n_lp_au: Vec<Vec<u8>> = vec![
            vec![0x40, 0x01, 0x0C, 0x01], // VPS
            vec![0x42, 0x01, 0x01, 0x01], // SPS
            vec![0x44, 0x01, 0xC1, 0x72], // PPS
            vec![0x28, 0x01, 0xAF, 0x06], // IDR_N_LP
        ];
        assert_eq!(
            sniff_annexb_codec(&hevc_idr_n_lp_au),
            Some(SniffedVideoCodec::Hevc)
        );
        // An IDR_N_LP slice alone stays ambiguous (it IS ambiguous).
        let lone_idr_n_lp: Vec<Vec<u8>> = vec![vec![0x28, 0x01, 0xAF, 0x06]];
        assert_eq!(sniff_annexb_codec(&lone_idr_n_lp), None);

        // Corrupt (forbidden bit) NALUs never vote.
        let corrupt: Vec<Vec<u8>> = vec![vec![0xE7, 0x64, 0x00, 0x28]];
        assert_eq!(sniff_annexb_codec(&corrupt), None);
    }

    #[test]
    fn test_annex_b_nalu_extraction() {
        let mut demux = TsDemuxer::new(None);

        // Annex B stream with two NALUs: SPS and PPS
        let data = [
            0x00, 0x00, 0x00, 0x01, // start code
            0x67, 0x42, 0x00, 0x1E, // SPS (type 7)
            0x00, 0x00, 0x00, 0x01, // start code
            0x68, 0xCE, 0x38, 0x80, // PPS (type 8)
        ];

        let nalus = demux.extract_h264_nalus(&data);
        assert_eq!(nalus.len(), 2);
        assert_eq!(nalus[0][0] & 0x1F, 7); // SPS
        assert_eq!(nalus[1][0] & 0x1F, 8); // PPS

        // SPS/PPS should be cached
        assert!(demux.cached_sps.is_some());
        assert!(demux.cached_pps.is_some());
    }

    #[test]
    fn test_three_byte_start_code() {
        let mut demux = TsDemuxer::new(None);

        let data = [
            0x00, 0x00, 0x01, // 3-byte start code
            0x65, 0x01, 0x02, // IDR (type 5)
        ];

        let nalus = demux.extract_h264_nalus(&data);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0][0] & 0x1F, 5); // IDR
    }

    /// Build a 188-byte PAT TS packet for one program (program_number=1).
    fn build_pat(pmt_pid: u16) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40; // PUSI=1, PID=0
        pkt[2] = 0x00;
        pkt[3] = 0x10; // AFC=01 payload-only, CC=0
        pkt[4] = 0x00; // pointer_field
        // PAT section: table_id(1) + 2 + ts_id(2) + version/cni(1) + sect#(1)
        // + last_sect(1) + 1 entry × 4 bytes + CRC(4) = 5 (post-len header) + 4 + 4
        let section_length = 5 + 4 + 4;
        pkt[5] = 0x00; // table_id PAT
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1; // reserved + version=0 + current_next=1
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0x00; // program_number=1 high
        pkt[14] = 0x01; // program_number=1 low
        pkt[15] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
        pkt[16] = (pmt_pid & 0xFF) as u8;
        // CRC over section body
        let crc = mpeg2_crc32(&pkt[5..17]);
        pkt[17] = (crc >> 24) as u8;
        pkt[18] = (crc >> 16) as u8;
        pkt[19] = (crc >> 8) as u8;
        pkt[20] = crc as u8;
        pkt
    }

    /// Build a 188-byte PMT TS packet declaring one H.264 video PID and one
    /// AAC audio PID, with the supplied 5-bit `version_number`.
    fn build_pmt(pmt_pid: u16, video_pid: u16, audio_pid: u16, version: u8) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F); // PUSI=1
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10; // AFC=01 payload-only, CC=0
        pkt[4] = 0x00; // pointer_field
        // PMT section: header(9) + program_info_len(2) + 2 ES descriptors @ 5 bytes + CRC(4)
        let section_length = 9 + 5 * 2 + 4;
        pkt[5] = 0x02; // table_id PMT
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00; // program_number=1
        pkt[9] = 0x01;
        pkt[10] = 0xC1 | ((version & 0x1F) << 1); // reserved + version + current_next=1
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | (((video_pid >> 8) as u8) & 0x1F); // PCR_PID = video
        pkt[14] = (video_pid & 0xFF) as u8;
        pkt[15] = 0xF0; // program_info_length=0
        pkt[16] = 0x00;
        // ES loop
        // Video: stream_type 0x1B (H.264)
        pkt[17] = 0x1B;
        pkt[18] = 0xE0 | (((video_pid >> 8) as u8) & 0x1F);
        pkt[19] = (video_pid & 0xFF) as u8;
        pkt[20] = 0xF0;
        pkt[21] = 0x00;
        // Audio: stream_type 0x0F (AAC ADTS)
        pkt[22] = 0x0F;
        pkt[23] = 0xE0 | (((audio_pid >> 8) as u8) & 0x1F);
        pkt[24] = (audio_pid & 0xFF) as u8;
        pkt[25] = 0xF0;
        pkt[26] = 0x00;
        let crc = mpeg2_crc32(&pkt[5..27]);
        pkt[27] = (crc >> 24) as u8;
        pkt[28] = (crc >> 16) as u8;
        pkt[29] = (crc >> 8) as u8;
        pkt[30] = crc as u8;
        pkt
    }

    fn discontinuity_count(frames: &[DemuxedFrame]) -> usize {
        frames
            .iter()
            .filter(|f| matches!(f, DemuxedFrame::Discontinuity))
            .count()
    }

    #[test]
    fn first_pmt_does_not_trigger_discontinuity() {
        let mut demux = TsDemuxer::new(None);
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(0x100));
        buf.extend_from_slice(&build_pmt(0x100, 0x200, 0x201, 0));

        let frames = demux.demux(&buf);
        assert_eq!(
            discontinuity_count(&frames),
            0,
            "first PMT seen must not emit Discontinuity (decoder hasn't started)",
        );
        assert_eq!(demux.pmt_version, Some(0));
    }

    #[test]
    fn pmt_version_change_emits_discontinuity_frame() {
        let mut demux = TsDemuxer::new(None);
        let mut buf1 = Vec::new();
        buf1.extend_from_slice(&build_pat(0x100));
        buf1.extend_from_slice(&build_pmt(0x100, 0x200, 0x201, 0));
        let _ = demux.demux(&buf1);

        // Second PMT with bumped version (what TsContinuityFixer::on_switch emits).
        let frames = demux.demux(&build_pmt(0x100, 0x200, 0x201, 1));
        assert_eq!(
            discontinuity_count(&frames),
            1,
            "PMT version bump must emit exactly one Discontinuity frame",
        );
        assert_eq!(demux.pmt_version, Some(1));
    }

    #[test]
    fn discontinuity_emitted_once_per_event_not_per_packet() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let _ = demux.demux(&build_pmt(0x100, 0x200, 0x201, 0));

        // Three PMTs in a row at the same new version — only the first
        // should trigger a Discontinuity.
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pmt(0x100, 0x200, 0x201, 1));
        buf.extend_from_slice(&build_pmt(0x100, 0x200, 0x201, 1));
        buf.extend_from_slice(&build_pmt(0x100, 0x200, 0x201, 1));

        let frames = demux.demux(&buf);
        assert_eq!(
            discontinuity_count(&frames),
            1,
            "repeated PMTs at the same version must emit one Discontinuity, not many",
        );

        // Subsequent demux calls at the same version produce nothing.
        let frames = demux.demux(&build_pmt(0x100, 0x200, 0x201, 1));
        assert_eq!(discontinuity_count(&frames), 0);
    }

    #[test]
    fn pmt_pid_change_emits_discontinuity_frame() {
        let mut demux = TsDemuxer::new(None);
        // Lock onto the first program (PMT PID 0x100).
        let _ = demux.demux(&build_pat(0x100));
        let _ = demux.demux(&build_pmt(0x100, 0x200, 0x201, 0));

        // Operator changed program in the upstream — new PAT points at a
        // different PMT PID. process_ts_packet's PAT branch resets ES state
        // and trips the discontinuity flag.
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_pat(0x101));
        buf.extend_from_slice(&build_pmt(0x101, 0x202, 0x203, 0));

        let frames = demux.demux(&buf);
        assert_eq!(
            discontinuity_count(&frames),
            1,
            "PMT PID change must emit Discontinuity",
        );
    }

    #[test]
    fn discontinuity_emitted_when_pmt_carries_no_es() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let _ = demux.demux(&build_pmt(0x100, 0x200, 0x201, 0));

        // A datagram that carries only a bumped PMT — no ES packets.
        // The Discontinuity must still surface so the consumer flushes
        // before the next ES frame arrives in a later datagram.
        let frames = demux.demux(&build_pmt(0x100, 0x200, 0x201, 1));
        assert_eq!(discontinuity_count(&frames), 1);
        assert_eq!(
            frames.len(),
            1,
            "only the Discontinuity should appear; no ES frames yet",
        );
    }

    /// Build a 188-byte PMT TS packet declaring one H.264 video PID and
    /// one private-data audio PID with a single ES descriptor of `tag` /
    /// `payload`. Used to exercise the DVB AC-3 / E-AC-3 / Opus routing
    /// in `parse_pmt`.
    fn build_pmt_private_audio(
        pmt_pid: u16,
        video_pid: u16,
        audio_pid: u16,
        desc_tag: u8,
        desc_payload: &[u8],
    ) -> [u8; TS_PACKET_SIZE] {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F);
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        let desc_total = 2 + desc_payload.len();
        // header(9) + program_info_len(2) + video ES(5) + audio ES(5 + desc) + CRC(4)
        let section_length = 9 + 5 + (5 + desc_total) + 4;
        pkt[5] = 0x02;
        pkt[6] = 0xB0 | (((section_length >> 8) as u8) & 0x0F);
        pkt[7] = (section_length & 0xFF) as u8;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE0 | (((video_pid >> 8) as u8) & 0x1F);
        pkt[14] = (video_pid & 0xFF) as u8;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        // Video ES — H.264.
        pkt[17] = 0x1B;
        pkt[18] = 0xE0 | (((video_pid >> 8) as u8) & 0x1F);
        pkt[19] = (video_pid & 0xFF) as u8;
        pkt[20] = 0xF0;
        pkt[21] = 0x00;
        // Audio ES — stream_type 0x06 (private_data) + descriptor.
        pkt[22] = STREAM_TYPE_PRIVATE;
        pkt[23] = 0xE0 | (((audio_pid >> 8) as u8) & 0x1F);
        pkt[24] = (audio_pid & 0xFF) as u8;
        pkt[25] = 0xF0 | (((desc_total >> 8) as u8) & 0x0F);
        pkt[26] = (desc_total & 0xFF) as u8;
        pkt[27] = desc_tag;
        pkt[28] = desc_payload.len() as u8;
        for (i, b) in desc_payload.iter().enumerate() {
            pkt[29 + i] = *b;
        }
        let crc_end = 27 + desc_total;
        let crc = mpeg2_crc32(&pkt[5..crc_end]);
        pkt[crc_end] = (crc >> 24) as u8;
        pkt[crc_end + 1] = (crc >> 16) as u8;
        pkt[crc_end + 2] = (crc >> 8) as u8;
        pkt[crc_end + 3] = crc as u8;
        pkt
    }

    #[test]
    fn surfaces_pmt_announced_scte35_section() {
        let mut mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
        mux.set_scte35_stream(0x01fc);
        let section = crate::engine::scte35_encode::build_splice_insert_section(
            &crate::engine::st2110::scte104::Scte104Message {
                opcode: crate::engine::st2110::scte104::SpliceOpcode::SpliceStartNormal,
                splice_event_id: Some(7), pre_roll_ms: Some(500),
                break_duration_ms: None, auto_return: None, protocol_version: 0,
            },
            90_000,
        );
        let wire: Vec<u8> = mux.mux_scte35(&section).into_iter().flatten().collect();
        let frames = TsDemuxer::new(None).demux(&wire);
        assert!(frames.iter().any(|f| matches!(f, DemuxedFrame::Scte35(s) if s == &section)));
    }

    /// DVB AC-3 sources signal `stream_type = 0x06` plus an AC-3 descriptor
    /// (tag `0x6A`). The demuxer must recognise the descriptor, route the
    /// PID into `audio_pid`, and synthesise the ATSC `0x81` marker on the
    /// PES assembler so `parse_pes` lands the PES on the OtherAudio path
    /// (where libavcodec decodes AC-3). Reproduces Bug A from
    /// `testbed/quality/display-tests/DISPLAY_QUALITY_REPORT.md`.
    #[test]
    fn dvb_ac3_descriptor_routes_audio_pid() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        // AC-3 descriptor body is opaque to routing — any 1-byte payload
        // satisfies the `length >= 0` requirement.
        let pmt = build_pmt_private_audio(0x100, 0x200, 0x300, 0x6A, &[0x40]);
        let _ = demux.demux(&pmt);
        assert_eq!(demux.audio_pid, Some(0x300), "DVB AC-3 PID must be locked");
        let assembler = demux
            .pes_assemblers
            .get(&0x300)
            .expect("PES assembler must exist for DVB AC-3 PID");
        assert_eq!(
            assembler.stream_type, 0x81,
            "DVB AC-3 must surface to parse_pes as ATSC-style 0x81 stream_type",
        );
    }

    /// E-AC-3 carriage uses descriptor tag `0x7A`. Symmetric to the AC-3
    /// case: the demuxer must synthesise `0x87` so `parse_pes` lands the
    /// PES on the existing E-AC-3 OtherAudio arm.
    #[test]
    fn dvb_eac3_descriptor_routes_audio_pid() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let pmt = build_pmt_private_audio(0x100, 0x200, 0x300, 0x7A, &[0x00]);
        let _ = demux.demux(&pmt);
        assert_eq!(demux.audio_pid, Some(0x300));
        let assembler = demux.pes_assemblers.get(&0x300).expect("E-AC-3 PID");
        assert_eq!(assembler.stream_type, 0x87);
    }

    /// E-AC-3 signalled ONLY via a registration descriptor (`0x05` with
    /// `"EAC3"` — what ffmpeg's mpegts muxer and the flow assembler's
    /// descriptor copy-through emit) must route exactly like the DVB
    /// `0x7A` flavour. The pre-fix parser recognised only `Opus` /
    /// `AC-4` registration identifiers, so the display path was silent
    /// (no audio PID, no meter levels) on every such source
    /// (2026-06-11, S4 / France-mux Take).
    #[test]
    fn registration_eac3_descriptor_routes_audio_pid() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let pmt = build_pmt_private_audio(0x100, 0x200, 0x300, 0x05, b"EAC3");
        let _ = demux.demux(&pmt);
        assert_eq!(demux.audio_pid, Some(0x300), "reg-EAC3 PID must be locked");
        let assembler = demux.pes_assemblers.get(&0x300).expect("reg-EAC3 PID");
        assert_eq!(assembler.stream_type, 0x87);
    }

    /// A PMT section spanning two TS packets must reassemble and parse
    /// in full. The pre-fix packet-level parser truncated at 188 bytes,
    /// so ES entries in the continuation packet — the audio on every
    /// real broadcast MPTS with fat descriptor loops — were never
    /// discovered.
    #[test]
    fn multi_packet_pmt_reassembles_and_discovers_audio() {
        // Build a PMT section: video ES with a fat (170-byte) descriptor
        // loop pushing the audio ES entry into the second TS packet.
        let video_pid: u16 = 0x200;
        let audio_pid: u16 = 0x300;
        let fat = vec![0xEEu8; 168]; // descriptor body
        let mut es_loop: Vec<u8> = Vec::new();
        es_loop.extend_from_slice(&[
            0x1B,
            0xE0 | ((video_pid >> 8) as u8 & 0x1F),
            (video_pid & 0xFF) as u8,
            0xF0 | (((fat.len() + 2) >> 8) as u8 & 0x0F),
            ((fat.len() + 2) & 0xFF) as u8,
            0x0E, // maximum_bitrate descriptor tag (opaque to routing)
            fat.len() as u8,
        ]);
        es_loop.extend_from_slice(&fat);
        es_loop.extend_from_slice(&[
            STREAM_TYPE_PRIVATE,
            0xE0 | ((audio_pid >> 8) as u8 & 0x1F),
            (audio_pid & 0xFF) as u8,
            0xF0,
            0x03, // es_info_length = 3
            0x7A, // DVB E-AC-3 descriptor
            0x01,
            0x00,
        ]);
        let section_length = 9 + es_loop.len() + 4;
        let mut section: Vec<u8> = vec![
            0x02,
            0xB0 | ((section_length >> 8) as u8 & 0x0F),
            (section_length & 0xFF) as u8,
            0x00,
            0x01, // program 1
            0xC1,
            0x00,
            0x00,
            0xE0 | ((video_pid >> 8) as u8 & 0x1F),
            (video_pid & 0xFF) as u8, // PCR PID
            0xF0,
            0x00,
        ];
        section.extend_from_slice(&es_loop);
        section.extend_from_slice(&[0, 0, 0, 0]); // CRC (not validated)
        assert!(
            section.len() > 184,
            "test PMT must exceed one TS packet to exercise reassembly"
        );

        // Split into PUSI + continuation TS packets on PID 0x100.
        let mut pkt1 = [0xFFu8; TS_PACKET_SIZE];
        pkt1[0] = TS_SYNC_BYTE;
        pkt1[1] = 0x40 | 0x01; // PUSI, PID 0x100
        pkt1[2] = 0x00;
        pkt1[3] = 0x10;
        pkt1[4] = 0x00; // pointer_field
        let first_len = TS_PACKET_SIZE - 5;
        pkt1[5..].copy_from_slice(&section[..first_len]);
        let mut pkt2 = [0xFFu8; TS_PACKET_SIZE];
        pkt2[0] = TS_SYNC_BYTE;
        pkt2[1] = 0x01; // continuation, PID 0x100
        pkt2[2] = 0x00;
        pkt2[3] = 0x11;
        let rest = &section[first_len..];
        pkt2[4..4 + rest.len()].copy_from_slice(rest);

        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let _ = demux.demux(&pkt1);
        assert_eq!(
            demux.audio_pid, None,
            "audio must not resolve from a truncated first packet"
        );
        let _ = demux.demux(&pkt2);
        assert_eq!(demux.video_pid, Some(video_pid));
        assert_eq!(
            demux.audio_pid,
            Some(audio_pid),
            "audio ES in the continuation packet must be discovered"
        );
        assert_eq!(
            demux.pes_assemblers.get(&audio_pid).unwrap().stream_type,
            0x87,
            "descriptor-resolved E-AC-3 must survive reassembly"
        );
    }

    /// Opus reg descriptor (tag `0x05` with `Opus` identifier) keeps the
    /// `stream_type = 0x06` marker — its parse_pes arm gates on
    /// `STREAM_TYPE_PRIVATE if Some(pid) == self.audio_pid`.
    #[test]
    fn opus_registration_descriptor_routes_audio_pid() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let pmt = build_pmt_private_audio(0x100, 0x200, 0x300, 0x05, b"Opus");
        let _ = demux.demux(&pmt);
        assert_eq!(demux.audio_pid, Some(0x300));
        let assembler = demux.pes_assemblers.get(&0x300).expect("Opus PID");
        assert_eq!(assembler.stream_type, STREAM_TYPE_PRIVATE);
    }

    /// DVB AC-4 carriage uses descriptor tag `0xAC` (ETSI TS 101 154
    /// § 5.7 / ATSC A/342-2 § 6.2). The demuxer must synthesise the
    /// internal `SYNTHETIC_STREAM_TYPE_AC4` (0xAC) marker so `parse_pes`
    /// routes the PES through the OtherAudio passthrough path.
    #[test]
    fn dvb_ac4_descriptor_routes_audio_pid() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let pmt = build_pmt_private_audio(0x100, 0x200, 0x300, 0xAC, &[0x00, 0x00, 0x00]);
        let _ = demux.demux(&pmt);
        assert_eq!(demux.audio_pid, Some(0x300), "DVB AC-4 PID must be locked");
        let assembler = demux
            .pes_assemblers
            .get(&0x300)
            .expect("PES assembler must exist for DVB AC-4 PID");
        assert_eq!(
            assembler.stream_type, SYNTHETIC_STREAM_TYPE_AC4,
            "DVB AC-4 must surface to parse_pes as synthetic 0xAC stream_type",
        );
    }

    /// AC-4 may also be signalled via the registration descriptor
    /// (tag `0x05`) with `AC-4` as the format_identifier. Same routing
    /// outcome as the dedicated 0xAC tag.
    #[test]
    fn ac4_registration_descriptor_routes_audio_pid() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        let pmt = build_pmt_private_audio(0x100, 0x200, 0x300, 0x05, b"AC-4");
        let _ = demux.demux(&pmt);
        assert_eq!(demux.audio_pid, Some(0x300));
        let assembler = demux.pes_assemblers.get(&0x300).expect("AC-4 PID");
        assert_eq!(assembler.stream_type, SYNTHETIC_STREAM_TYPE_AC4);
    }

    /// Private streams without a recognised audio descriptor must NOT be
    /// surfaced to the audio path — that's where the legacy demuxer's
    /// "AC-3 silent on display" symptom came from when an unknown
    /// private-data stream sat next to an AAC PID.
    #[test]
    fn private_stream_without_audio_descriptor_is_ignored() {
        let mut demux = TsDemuxer::new(None);
        let _ = demux.demux(&build_pat(0x100));
        // Unknown descriptor tag — nothing on the audio path should fire.
        let pmt = build_pmt_private_audio(0x100, 0x200, 0x300, 0x52, &[0x00]);
        let _ = demux.demux(&pmt);
        assert_eq!(
            demux.audio_pid, None,
            "private stream without AC-3/E-AC-3/Opus descriptor must not be routed",
        );
    }

    #[test]
    fn test_pts_parsing() {
        // PTS = 0 encoded as: 0010_xxx1 xxxx_xxxx xxxx_xxx1 xxxx_xxxx xxxx_xxx1
        let data = [0x21, 0x00, 0x01, 0x00, 0x01];
        let pts = parse_pts(&data);
        assert_eq!(pts, 0);

        // PTS = 90000 (1 second at 90kHz)
        // 90000 = 0x15F90
        // Spread into 5 bytes with marker bits
        let pts_val: u64 = 90000;
        let b0 = (0x20 | ((pts_val >> 29) & 0x0E) as u8) | 0x01;
        let b1 = ((pts_val >> 22) & 0xFF) as u8;
        let b2 = (((pts_val >> 14) & 0xFE) as u8) | 0x01;
        let b3 = ((pts_val >> 7) & 0xFF) as u8;
        let b4 = (((pts_val & 0x7F) << 1) as u8) | 0x01;
        let encoded = [b0, b1, b2, b3, b4];
        let decoded = parse_pts(&encoded);
        assert_eq!(decoded, 90000);
    }

    // ── Audio AUs across PES boundaries ──

    /// PAT + PMT with one H.264 video PID and an audio PID of `audio_st`.
    fn pat_pmt_with_audio(audio_st: u8) -> Vec<u8> {
        let mut pmt = build_pmt(0x100, 0x200, 0x300, 0);
        pmt[22] = audio_st;
        let crc = mpeg2_crc32(&pmt[5..27]);
        pmt[27..31].copy_from_slice(&crc.to_be_bytes());
        let mut ts = build_pat(0x100).to_vec();
        ts.extend_from_slice(&pmt);
        ts
    }

    /// TS packets of one audio PES on PID 0x300 carrying `es` with `pts`.
    fn audio_pes_packets(es: &[u8], pts: u64, cc: &mut u8) -> Vec<u8> {
        crate::engine::ts_test_fixtures::pes_packets(0x300, 0xC0, es, pts, cc)
    }

    /// An AAC-LC 48 kHz stereo ADTS frame of `len` bytes, body `fill`.
    fn adts(len: usize, fill: u8) -> Vec<u8> {
        let mut f = vec![fill; len];
        f[0] = 0xFF;
        f[1] = 0xF1;
        f[2] = (1 << 6) | (3 << 2);
        f[3] = (2 << 6) | ((len >> 11) as u8 & 0x03);
        f[4] = (len >> 3) as u8;
        f[5] = ((len as u8) << 5) | 0x1F;
        f[6] = 0xFC;
        f
    }

    /// An MPEG-1 layer II 48 kHz 192 kbps stereo frame (576 bytes), body
    /// `fill`.
    fn mp2(fill: u8) -> Vec<u8> {
        let mut f = vec![fill; 576];
        f[..4].copy_from_slice(&[0xFF, 0xFD, 0xA4, 0x04]);
        f
    }

    /// An ADTS frame that straddles two PES — its head in one, its tail
    /// opening the next — is demuxed whole, and so is every frame after
    /// it, each at its own presentation time. The per-PES walk stopped at
    /// the first byte of each PES that was not a sync word: the straddling
    /// frame and the whole next PES were lost, on every path that decodes
    /// through this demuxer (display, SDI, CMAF, RTMP, WebRTC, ST 2110-30).
    #[test]
    fn an_adts_frame_straddling_two_pes_is_demuxed_with_the_rest() {
        const P0: u64 = 900_000;
        let frames: Vec<Vec<u8>> = (0..7).map(|k| adts(300 + 10 * k, 0x11 * (k as u8 + 1))).collect();
        let es: Vec<u8> = frames.concat();
        // PES 1: frames 0-2 and the head of 3; PES 2: the tail of 3, 4, 5;
        // PES 3: frame 6.
        let cut1 = frames[..3].iter().map(|f| f.len()).sum::<usize>() + 100;
        let cut2 = frames[..6].iter().map(|f| f.len()).sum::<usize>();
        let mut ts = pat_pmt_with_audio(0x0F);
        let mut cc = 0;
        ts.extend(audio_pes_packets(&es[..cut1], P0, &mut cc));
        ts.extend(audio_pes_packets(&es[cut1..cut2], P0 + 4 * 1920, &mut cc));
        ts.extend(audio_pes_packets(&es[cut2..], P0 + 6 * 1920, &mut cc));
        let mut demux = TsDemuxer::new(None);
        let out: Vec<(Vec<u8>, u64)> = ts
            .chunks(TS_PACKET_SIZE)
            .flat_map(|p| demux.demux(p))
            .filter_map(|f| match f {
                DemuxedFrame::Aac { data, pts } => Some((data, pts)),
                _ => None,
            })
            .collect();
        assert_eq!(out.len(), 7, "every frame, the straddling one included");
        for (k, (data, pts)) in out.iter().enumerate() {
            assert_eq!(data.as_slice(), &frames[k][7..], "frame {k}, ADTS header stripped");
            assert_eq!(*pts, P0 + k as u64 * 1920, "frame {k} at its own time");
        }
        assert_eq!(demux.cached_aac_config(), Some((1, 3, 2)));
    }

    /// MP2 (and AC-3 / E-AC-3 / LATM) surface as one `OtherAudio` per PES:
    /// the whole AUs that commenced in it — one that straddles into the next
    /// PES included — under that PES's PTS. The per-PES parse handed on the
    /// straddling AU in two pieces, and the consumers' frame splitters
    /// dropped both.
    #[test]
    fn mp2_frames_are_grouped_whole_by_the_pes_they_commence_in() {
        const P0: u64 = 900_000;
        let frames: Vec<Vec<u8>> = (0..7).map(|k| mp2(0x10 + k as u8)).collect();
        let es: Vec<u8> = frames.concat();
        let cut1 = 2 * 576 + 200;
        let cut2 = 4 * 576;
        let mut ts = pat_pmt_with_audio(0x03);
        let mut cc = 0;
        ts.extend(audio_pes_packets(&es[..cut1], P0, &mut cc));
        ts.extend(audio_pes_packets(&es[cut1..cut2], P0 + 3 * 2160, &mut cc));
        ts.extend(audio_pes_packets(&es[cut2..6 * 576], P0 + 4 * 2160, &mut cc));
        ts.extend(audio_pes_packets(&es[6 * 576..], P0 + 6 * 2160, &mut cc));
        let mut demux = TsDemuxer::new(None);
        let groups: Vec<(u8, Vec<u8>, u64)> = ts
            .chunks(TS_PACKET_SIZE)
            .flat_map(|p| demux.demux(p))
            .filter_map(|f| match f {
                DemuxedFrame::OtherAudio { stream_type, data, pts } => Some((stream_type, data, pts)),
                _ => None,
            })
            .collect();
        let expect = [
            (frames[..3].concat(), P0),
            (frames[3].clone(), P0 + 3 * 2160),
            (frames[4..6].concat(), P0 + 4 * 2160),
        ];
        assert_eq!(groups.len(), expect.len(), "the last PES's group waits for the next");
        for (k, ((st, data, pts), (want, want_pts))) in groups.iter().zip(expect.iter()).enumerate() {
            assert_eq!(*st, 0x03);
            assert!(data == want, "group {k}: whole AUs only ({} bytes vs {})", data.len(), want.len());
            assert_eq!(pts, want_pts, "group {k}");
        }
    }

    /// A lost TS packet on the audio PID does not glue the two halves of a
    /// damaged AU together: the cutter drops it and resyncs on the next
    /// header, and the frames around it come through.
    #[test]
    fn a_lost_audio_packet_costs_only_the_damaged_frame() {
        const P0: u64 = 900_000;
        let frames: Vec<Vec<u8>> = (0..6).map(|_| adts(400, 0x22)).collect();
        let mut ts = pat_pmt_with_audio(0x0F);
        let mut cc = 0;
        let pes = audio_pes_packets(&frames[..3].concat(), P0, &mut cc);
        // Lose the second packet of the first PES (inside frame 0 / 1).
        for (i, p) in pes.chunks(TS_PACKET_SIZE).enumerate() {
            if i != 2 {
                ts.extend_from_slice(p);
            }
        }
        ts.extend(audio_pes_packets(&frames[3..].concat(), P0 + 3 * 1920, &mut cc));
        let mut demux = TsDemuxer::new(None);
        let pts: Vec<u64> = ts
            .chunks(TS_PACKET_SIZE)
            .flat_map(|p| demux.demux(p))
            .filter_map(|f| match f {
                DemuxedFrame::Aac { pts, .. } => Some(pts),
                _ => None,
            })
            .collect();
        assert!(pts.ends_with(&[P0 + 3 * 1920, P0 + 4 * 1920, P0 + 5 * 1920]), "{pts:?}");
        assert!(pts.len() < 6, "the damaged frame is not passed on: {pts:?}");
    }
}
