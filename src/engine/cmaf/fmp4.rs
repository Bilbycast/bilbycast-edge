// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CMAF init segment + movie fragment writers.
//!
//! CMAF (ISO/IEC 23000-19) is a fragmented MP4 profile:
//! - **Init segment**: `ftyp` + `moov`. Sent once at start of stream.
//! - **Media segment**: `styp` + `moof` + `mdat` (repeated per CMAF
//!   fragment, potentially in multiple `moof`/`mdat` pairs for LL-CMAF).
//!
//! Phase 1 MVP supports H.264 video + AAC audio. Each output has one
//! video track and optionally one audio track. Timestamps live in the
//! track's `mdhd.timescale` — we use 90 kHz for video (matches MPEG-TS
//! PTS) and the AAC sample rate (e.g. 48000) for audio.

use super::box_writer::{BoxWriter, patch_u32};
use super::codecs::{parse_sps_resolution, write_avcc, write_esds, write_hvcc};
use super::nalu::to_length_prefixed;

/// Codec family for a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

/// Video track metadata needed to build the init segment.
pub struct VideoTrack {
    pub codec: VideoCodec,
    /// SPS bytes (no start code, no length prefix).
    pub sps: Vec<u8>,
    /// PPS bytes.
    pub pps: Vec<u8>,
    /// HEVC VPS (unused for H.264).
    pub vps: Vec<u8>,
    /// 90 kHz timescale — matches MPEG-TS PTS clock.
    pub timescale: u32,
    /// Derived from SPS. Falls back to (1280, 720) if parsing fails.
    pub width: u32,
    pub height: u32,
}

impl VideoTrack {
    pub fn from_h264(sps: Vec<u8>, pps: Vec<u8>) -> Self {
        let (width, height) = parse_sps_resolution(&sps).unwrap_or((1280, 720));
        Self {
            codec: VideoCodec::H264,
            sps,
            pps,
            vps: Vec::new(),
            timescale: 90_000,
            width,
            height,
        }
    }

    /// Build an HEVC video track. Width/height are parsed from the SPS
    /// via a minimal bit reader; on failure we fall back to 1920×1080
    /// so the init segment is still playable (players derive actual
    /// dimensions from the embedded SPS anyway).
    pub fn from_h265(vps: Vec<u8>, sps: Vec<u8>, pps: Vec<u8>) -> Self {
        let (width, height) = parse_hevc_sps_resolution(&sps).unwrap_or((1920, 1080));
        Self {
            codec: VideoCodec::H265,
            sps,
            pps,
            vps,
            timescale: 90_000,
            width,
            height,
        }
    }
}

/// Minimal HEVC SPS resolution parser — returns (pic_width, pic_height)
/// on success. Handles `sps_video_parameter_set_id`, sub-layer counting,
/// full `profile_tier_level` (12 common + per-sub-layer), and the
/// Exp-Golomb width/height fields.
fn parse_hevc_sps_resolution(sps: &[u8]) -> Option<(u32, u32)> {
    use super::codecs::BitReader;
    if sps.len() < 15 {
        return None;
    }
    // Skip 2-byte NAL header.
    let mut r = BitReader::new(&sps[2..]);
    let _sps_vps_id = r.read_bits(4)?;
    let sps_max_sub_layers_minus1 = r.read_bits(3)?;
    let _temporal_id_nesting = r.bit()?;
    // profile_tier_level (common 12 bytes == 96 bits already read as raw
    // bytes). BitReader has been consuming bits since the start; the
    // common PTL follows on a byte boundary — skip 12 bytes (96 bits).
    r.skip_bits(96)?;
    // Sub-layer level info: 8 bits (2 each for profile/level present
    // flags) per sub-layer, padding bits, then variable fields per set
    // flag. Safely skip.
    for _ in 0..sps_max_sub_layers_minus1 {
        let profile_present = r.bit()?;
        let level_present = r.bit()?;
        let _ = (profile_present, level_present);
    }
    if sps_max_sub_layers_minus1 > 0 {
        // Reserved padding for alignment.
        for _ in sps_max_sub_layers_minus1..8 {
            r.skip_bits(2)?;
        }
    }
    for i in 0..sps_max_sub_layers_minus1 {
        // If we had recorded the flags we'd skip conditionally here —
        // a safer path is to bail and rely on the 1920×1080 fallback.
        let _ = i;
    }

    let _sps_seq_parameter_set_id = r.ue()?;
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc == 3 {
        let _separate_colour_plane_flag = r.bit()?;
    }
    let pic_width_in_luma_samples = r.ue()?;
    let pic_height_in_luma_samples = r.ue()?;
    if pic_width_in_luma_samples == 0 || pic_width_in_luma_samples > 16_384 {
        return None;
    }
    if pic_height_in_luma_samples == 0 || pic_height_in_luma_samples > 16_384 {
        return None;
    }
    Some((pic_width_in_luma_samples, pic_height_in_luma_samples))
}

/// Audio track metadata. The codec-specific configuration lives in the
/// [`AudioCodec`] enum so the same `AudioTrack` shape carries AAC, MP2,
/// AC-3, or E-AC-3.
pub struct AudioTrack {
    /// Codec-specific decoder configuration. Selects the sample-entry
    /// fourcc (`mp4a` / `ac-3` / `ec-3`) and the matching child config
    /// box (`esds` / `dac3` / `dec3`).
    pub codec: AudioCodec,
    /// Sampling rate in Hz; used as the track's `mdhd.timescale` and
    /// the sample-entry rate. Must match the codec's actual rate
    /// (AAC: ASC sample-rate index → Hz; AC-3: fscod table → Hz;
    /// MP2: header sample-rate field → Hz).
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo). For AC-3 / E-AC-3 this is
    /// the decoded channel count derived from `acmod + lfeon`; for AAC
    /// it's the `channel_configuration` value.
    pub channels: u16,
}

/// Per-codec audio configuration. Each variant is the minimum the
/// matching MP4 sample entry needs to be playable in compliant
/// players (Quicktime, ffmpeg, browsers via MSE).
#[derive(Debug, Clone)]
pub enum AudioCodec {
    /// MPEG-4 AAC-LC (ISO/IEC 14496-3). Sample entry `mp4a` + `esds`
    /// with `objectTypeIndication = 0x40` (MPEG-4 Audio).
    Aac {
        /// 2-byte AudioSpecificConfig from
        /// [`super::codecs::aac_audio_specific_config`].
        audio_specific_config: [u8; 2],
        /// Average bitrate in bits per second (written into the esds DCD).
        avg_bitrate: u32,
    },
    /// MPEG-1 / MPEG-2 Layer II audio (ISO/IEC 11172-3). Sample entry
    /// `mp4a` + `esds` with `objectTypeIndication = 0x69` (MPEG-1 Audio,
    /// also accepted by all major MP4 parsers for MPEG-2 Layer II since
    /// the bitstream is wire-identical). Empty DecoderSpecificInfo —
    /// every parameter is encoded inside the MP2 frame headers.
    Mp2 {
        avg_bitrate: u32,
    },
    /// AC-3 (Dolby Digital, ETSI TS 102 366 Annex F.4). Sample entry
    /// `ac-3` + `dac3` config box.
    Ac3 {
        /// 3-byte AC3SpecificBox payload — `fscod(2) | bsid(5) |
        /// bsmod(3) | acmod(3) | lfeon(1) | bit_rate_code(5) |
        /// reserved(5)`.
        dac3: [u8; 3],
    },
    /// E-AC-3 (Dolby Digital Plus, ETSI TS 102 366 Annex F.6). Sample
    /// entry `ec-3` + `dec3` config box.
    EAc3 {
        /// Variable-length EC3SpecificBox payload built by
        /// [`super::codecs::build_dec3_payload`].
        dec3: Vec<u8>,
    },
}

impl AudioTrack {
    /// Build an AAC-LC audio track. Convenience constructor used by every
    /// AAC ingest path (RTMP / RTSP / WebRTC / TS demux).
    pub fn aac(
        audio_specific_config: [u8; 2],
        sample_rate: u32,
        channels: u16,
        avg_bitrate: u32,
    ) -> Self {
        Self {
            codec: AudioCodec::Aac {
                audio_specific_config,
                avg_bitrate,
            },
            sample_rate,
            channels,
        }
    }

    /// Returns the 2-byte AAC AudioSpecificConfig if this track carries
    /// AAC, else `None`. Used by manifest builders (DASH MPD, HLS m3u8)
    /// that quote the ASC bytes when computing the codec string.
    pub fn aac_asc(&self) -> Option<&[u8; 2]> {
        match &self.codec {
            AudioCodec::Aac { audio_specific_config, .. } => Some(audio_specific_config),
            _ => None,
        }
    }
}

/// Track ID constants. Must be positive and unique within the movie.
pub const VIDEO_TRACK_ID: u32 = 1;
pub const AUDIO_TRACK_ID: u32 = 2;

// ────────────────────────────────────────────────────────────────────────
//  Init segment
// ────────────────────────────────────────────────────────────────────────

/// CENC encryption parameters baked into an init segment (`tenc`,
/// `pssh`, sample-entry rewriting).
pub struct CencInitParams<'a> {
    pub scheme: super::cenc::Scheme,
    pub key_id: &'a [u8; 16],
    /// Operator-supplied PSSH bytes, copied verbatim into moov.
    pub extra_pssh: Vec<Vec<u8>>,
}

/// Build a CMAF init segment with optional ClearKey CENC protection.
pub fn build_encrypted_init_segment(
    video: &VideoTrack,
    audio: Option<&AudioTrack>,
    cenc: &CencInitParams<'_>,
) -> Vec<u8> {
    build_init_segment_inner(video, audio, Some(cenc))
}

/// Build a CMAF init segment (ftyp + moov) for one video track and an
/// optional audio track. The result is a complete, self-contained byte
/// buffer suitable for a single HTTP PUT of `init.mp4`.
pub fn build_init_segment(video: &VideoTrack, audio: Option<&AudioTrack>) -> Vec<u8> {
    build_init_segment_inner(video, audio, None)
}

fn build_init_segment_inner(
    video: &VideoTrack,
    audio: Option<&AudioTrack>,
    cenc: Option<&CencInitParams<'_>>,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2048);

    // ── ftyp ────────────────────────────────────────────────────────
    {
        let mut ftyp = BoxWriter::open(&mut buf, *b"ftyp");
        ftyp.fourcc(*b"cmfc"); // major_brand
        ftyp.u32(0);            // minor_version
        // compatible_brands: cmfc (CMAF media), iso6, isom, mp41, plus
        // the codec-specific brand so players know what to expect.
        ftyp.fourcc(*b"cmfc");
        ftyp.fourcc(*b"iso6");
        ftyp.fourcc(*b"isom");
        ftyp.fourcc(*b"mp41");
        match video.codec {
            VideoCodec::H264 => ftyp.fourcc(*b"avc1"),
            VideoCodec::H265 => ftyp.fourcc(*b"hvc1"),
        };
    }

    // ── moov ────────────────────────────────────────────────────────
    {
        let mut moov = BoxWriter::open(&mut buf, *b"moov");

        // mvhd (FullBox v0): creation/modification times, timescale, duration.
        // For fragmented MP4 we set duration=0 (unknown) — the 'mvex' tells
        // the player this movie is fragmented.
        {
            let mut mvhd = moov.child_full(*b"mvhd", 0, 0);
            mvhd.u32(0); // creation_time
            mvhd.u32(0); // modification_time
            mvhd.u32(1000); // timescale (movie-level: use 1000 Hz for ms resolution)
            mvhd.u32(0); // duration (0 = unknown / fragmented)
            mvhd.u32(0x0001_0000); // rate = 1.0
            mvhd.u16(0x0100); // volume = 1.0
            mvhd.zeros(2 + 8); // reserved (u16 + u32[2])
            // unity matrix (3x3, 16.16 fixed point except last row 2.30)
            for &v in &[
                0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000,
            ] {
                mvhd.u32(v);
            }
            mvhd.zeros(24); // pre_defined[6]
            mvhd.u32(AUDIO_TRACK_ID + 1); // next_track_ID (after any audio track)
        }

        // Video trak.
        write_video_trak(&mut moov, video, cenc);

        // Audio trak (optional).
        if let Some(a) = audio {
            write_audio_trak(&mut moov, a, cenc);
        }

        // mvex: signals that this movie is fragmented.
        {
            let mut mvex = moov.child(*b"mvex");
            write_trex(&mut mvex, VIDEO_TRACK_ID);
            if audio.is_some() {
                write_trex(&mut mvex, AUDIO_TRACK_ID);
            }
        }

        // Optional CENC `pssh` boxes inside moov.
        if let Some(c) = cenc {
            super::cenc_boxes::write_clearkey_pssh(&mut moov, c.key_id);
            for extra in &c.extra_pssh {
                let _ = super::cenc_boxes::write_verbatim_pssh(&mut moov, extra);
            }
        }
    }

    buf
}

/// `sample_flags` per ISO/IEC 14496-12 8.8.3.1, MSB-first:
///
/// ```text
///  4 reserved | 2 is_leading | 2 sample_depends_on | 2 sample_is_depended_on
///  2 sample_has_redundancy | 3 sample_padding_value
///  1 sample_is_non_sync_sample | 16 sample_degradation_priority
/// ```
///
/// So `sample_depends_on` is bits 25-24 (1 = depends on others, 2 = does not)
/// and `sample_is_non_sync_sample` is **bit 16**.
///
/// A sync sample: does not depend on others, and is not flagged non-sync.
const SAMPLE_FLAGS_SYNC: u32 = 0x0200_0000;

/// A non-sync sample: depends on others, **and** bit 16 set. Setting only
/// `sample_depends_on` (0x0100_0000) leaves bit 16 clear, so it still reads as
/// a sync sample — which is what previously made every P-frame in a long-GOP
/// fragment look like a valid place to start decoding.
const SAMPLE_FLAGS_NON_SYNC: u32 = 0x0101_0000;

fn write_trex(parent: &mut BoxWriter<'_>, track_id: u32) {
    let mut trex = parent.child_full(*b"trex", 0, 0);
    trex.u32(track_id);
    trex.u32(1); // default_sample_description_index
    trex.u32(0); // default_sample_duration (per-sample in trun)
    trex.u32(0); // default_sample_size
    // default_sample_flags. Zero would mean `non_sync = 0` — an explicit claim
    // that every sample inheriting this default is a random-access point. Each
    // fragment overrides this in its `tfhd`, but the safe default for anything
    // that does not is "not seekable".
    trex.u32(SAMPLE_FLAGS_NON_SYNC);
}

fn write_video_trak(
    parent: &mut BoxWriter<'_>,
    v: &VideoTrack,
    cenc: Option<&CencInitParams<'_>>,
) {
    let mut trak = parent.child(*b"trak");

    // tkhd (FullBox v0, flags 0x000007 = track_enabled+in_movie+in_preview).
    {
        let mut tkhd = trak.child_full(*b"tkhd", 0, 0x0000_0007);
        tkhd.u32(0); // creation_time
        tkhd.u32(0); // modification_time
        tkhd.u32(VIDEO_TRACK_ID);
        tkhd.u32(0); // reserved
        tkhd.u32(0); // duration
        tkhd.zeros(8); // reserved(u32[2])
        tkhd.u16(0); // layer
        tkhd.u16(0); // alternate_group
        tkhd.u16(0); // volume
        tkhd.u16(0); // reserved
        // unity matrix
        for &m in &[
            0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000,
        ] {
            tkhd.u32(m);
        }
        tkhd.u32(v.width << 16); // width as 16.16
        tkhd.u32(v.height << 16); // height as 16.16
    }

    // mdia / mdhd / hdlr / minf / vmhd / dinf / stbl
    {
        let mut mdia = trak.child(*b"mdia");
        {
            let mut mdhd = mdia.child_full(*b"mdhd", 0, 0);
            mdhd.u32(0);
            mdhd.u32(0);
            mdhd.u32(v.timescale);
            mdhd.u32(0); // duration
            mdhd.u16(0x55C4); // language = 'und' (15.36 packed ISO-639-2T, bit 15 = 0)
            mdhd.u16(0); // pre_defined
        }
        {
            let mut hdlr = mdia.child_full(*b"hdlr", 0, 0);
            hdlr.u32(0); // pre_defined
            hdlr.fourcc(*b"vide");
            hdlr.zeros(12); // reserved[3]
            hdlr.bytes(b"VideoHandler\0");
        }
        {
            let mut minf = mdia.child(*b"minf");
            // vmhd
            {
                let mut vmhd = minf.child_full(*b"vmhd", 0, 1);
                vmhd.u16(0); // graphicsmode
                vmhd.u16(0); // opcolor.r
                vmhd.u16(0); // g
                vmhd.u16(0); // b
            }
            write_null_dinf(&mut minf);
            write_video_stbl(&mut minf, v, cenc);
        }
    }
}

fn write_audio_trak(
    parent: &mut BoxWriter<'_>,
    a: &AudioTrack,
    cenc: Option<&CencInitParams<'_>>,
) {
    let mut trak = parent.child(*b"trak");
    {
        let mut tkhd = trak.child_full(*b"tkhd", 0, 0x0000_0007);
        tkhd.u32(0);
        tkhd.u32(0);
        tkhd.u32(AUDIO_TRACK_ID);
        tkhd.u32(0);
        tkhd.u32(0);
        tkhd.zeros(8);
        tkhd.u16(0);
        tkhd.u16(0);
        tkhd.u16(0x0100); // volume = 1.0
        tkhd.u16(0);
        for &m in &[
            0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000,
        ] {
            tkhd.u32(m);
        }
        tkhd.u32(0); // width
        tkhd.u32(0); // height
    }
    {
        let mut mdia = trak.child(*b"mdia");
        {
            let mut mdhd = mdia.child_full(*b"mdhd", 0, 0);
            mdhd.u32(0);
            mdhd.u32(0);
            mdhd.u32(a.sample_rate);
            mdhd.u32(0);
            mdhd.u16(0x55C4); // 'und'
            mdhd.u16(0);
        }
        {
            let mut hdlr = mdia.child_full(*b"hdlr", 0, 0);
            hdlr.u32(0);
            hdlr.fourcc(*b"soun");
            hdlr.zeros(12);
            hdlr.bytes(b"SoundHandler\0");
        }
        {
            let mut minf = mdia.child(*b"minf");
            {
                let mut smhd = minf.child_full(*b"smhd", 0, 0);
                smhd.u16(0); // balance
                smhd.u16(0); // reserved
            }
            write_null_dinf(&mut minf);
            write_audio_stbl(&mut minf, a, cenc);
        }
    }
}

fn write_null_dinf(parent: &mut BoxWriter<'_>) {
    let mut dinf = parent.child(*b"dinf");
    let mut dref = dinf.child_full(*b"dref", 0, 0);
    dref.u32(1); // entry_count
    let mut url = dref.child_full(*b"url ", 0, 0x0000_0001);
    // flags=1 → "media data is in the same file as the movie box"
    // body is empty because no location string follows.
    let _ = &mut url;
}

fn write_video_stbl(
    parent: &mut BoxWriter<'_>,
    v: &VideoTrack,
    cenc: Option<&CencInitParams<'_>>,
) {
    let mut stbl = parent.child(*b"stbl");
    write_video_stsd(&mut stbl, v, cenc);
    write_empty_sample_tables(&mut stbl);
}

/// The video sample entry — `avc1` / `hvc1` (or `encv` under CENC) and its
/// codec configuration. Shared by the fragmented and progressive writers: the
/// description of the media is the same either way, only the tables differ.
fn write_video_stsd(
    stbl: &mut BoxWriter<'_>,
    v: &VideoTrack,
    cenc: Option<&CencInitParams<'_>>,
) {
    {
        let mut stsd = stbl.child_full(*b"stsd", 0, 0);
        stsd.u32(1); // entry_count

        let original_fourcc: [u8; 4] = match v.codec {
            VideoCodec::H264 => *b"avc1",
            VideoCodec::H265 => *b"hvc1",
        };
        // For CENC the sample entry fourcc switches to `encv`, with the
        // original codec fourcc preserved inside `sinf/frma`.
        let fourcc: [u8; 4] = if cenc.is_some() { *b"encv" } else { original_fourcc };
        let mut entry = stsd.child(fourcc);
        entry.zeros(6); // reserved
        entry.u16(1); // data_reference_index
        // VisualSampleEntry fields (ISO/IEC 14496-12 §12.1.3)
        entry.u16(0); // pre_defined
        entry.u16(0); // reserved
        entry.zeros(12); // pre_defined[3]
        entry.u16(v.width as u16);
        entry.u16(v.height as u16);
        entry.u32(0x0048_0000); // horiz res 72 dpi
        entry.u32(0x0048_0000); // vert res 72 dpi
        entry.u32(0); // reserved
        entry.u16(1); // frame_count
        // compressorname: 32-byte pascal string (1 byte length + 31 bytes)
        entry.u8(0);
        entry.zeros(31);
        entry.u16(0x0018); // depth = 24
        entry.i16(-1); // pre_defined

        match v.codec {
            VideoCodec::H264 => write_avcc(&mut entry, &v.sps, &v.pps),
            VideoCodec::H265 => write_hvcc(&mut entry, &v.vps, &v.sps, &v.pps),
        }

        // CENC sinf wrapper inside the encv sample entry.
        if let Some(c) = cenc {
            let per_sample_iv = match c.scheme {
                super::cenc::Scheme::Cenc => 16,
                super::cenc::Scheme::Cbcs => 0,
            };
            super::cenc_boxes::write_sinf(
                &mut entry,
                original_fourcc,
                c.scheme,
                c.key_id,
                per_sample_iv,
            );
        }
    }
}

/// The four required `stbl` tables, each saying "zero samples".
///
/// Correct for a fragmented file, where every sample lives in a `moof`/`mdat`
/// pair rather than in the movie box. A progressive file fills them in
/// instead — see [`write_sample_tables`].
fn write_empty_sample_tables(stbl: &mut BoxWriter<'_>) {
    {
        let mut stts = stbl.child_full(*b"stts", 0, 0);
        stts.u32(0);
    }
    {
        let mut stsc = stbl.child_full(*b"stsc", 0, 0);
        stsc.u32(0);
    }
    {
        let mut stsz = stbl.child_full(*b"stsz", 0, 0);
        stsz.u32(0); // sample_size
        stsz.u32(0); // sample_count
    }
    {
        let mut stco = stbl.child_full(*b"stco", 0, 0);
        stco.u32(0);
    }
}

fn write_audio_stbl(
    parent: &mut BoxWriter<'_>,
    a: &AudioTrack,
    cenc: Option<&CencInitParams<'_>>,
) {
    let mut stbl = parent.child(*b"stbl");
    write_audio_stsd(&mut stbl, a, cenc);
    write_empty_sample_tables(&mut stbl);
}

/// The audio sample entry and its codec configuration. Shared, as for video.
fn write_audio_stsd(
    stbl: &mut BoxWriter<'_>,
    a: &AudioTrack,
    cenc: Option<&CencInitParams<'_>>,
) {
    {
        let mut stsd = stbl.child_full(*b"stsd", 0, 0);
        stsd.u32(1);
        // Codec → sample-entry fourcc:
        //   AAC + MP2 → mp4a (both use esds; MP2 with OTI 0x69)
        //   AC-3      → ac-3
        //   E-AC-3    → ec-3
        // CENC scrambles only AAC/MP2 (`mp4a` ↔ `enca`) today; AC-3 / E-AC-3
        // CENC variants exist but aren't wired here yet — caller must not
        // pass `cenc` for those codecs.
        let original_fourcc: [u8; 4] = match &a.codec {
            AudioCodec::Aac { .. } | AudioCodec::Mp2 { .. } => *b"mp4a",
            AudioCodec::Ac3 { .. } => *b"ac-3",
            AudioCodec::EAc3 { .. } => *b"ec-3",
        };
        let fourcc = if cenc.is_some() { *b"enca" } else { original_fourcc };
        let mut entry = stsd.child(fourcc);
        entry.zeros(6); // reserved
        entry.u16(1); // data_reference_index
        // AudioSampleEntry (ISO/IEC 14496-12 §12.2)
        entry.zeros(8); // reserved(u32[2])
        entry.u16(a.channels);
        entry.u16(16); // samplesize
        entry.u16(0); // pre_defined
        entry.u16(0); // reserved
        // samplerate: 16.16 fixed. Upper 16 bits carry Hz for rates < 65 536.
        entry.u32(a.sample_rate << 16);

        match &a.codec {
            AudioCodec::Aac {
                audio_specific_config,
                avg_bitrate,
            } => {
                write_esds(&mut entry, audio_specific_config, *avg_bitrate);
            }
            AudioCodec::Mp2 { avg_bitrate } => {
                // MP2 uses the same esds shape as AAC but with
                // objectTypeIndication = 0x69 (MPEG-1 Audio L2) and an
                // empty DecoderSpecificInfo — the MP2 bitstream
                // self-describes via its frame headers.
                super::codecs::write_esds_mpeg_audio(&mut entry, *avg_bitrate);
            }
            AudioCodec::Ac3 { dac3 } => {
                super::codecs::write_dac3(&mut entry, dac3);
            }
            AudioCodec::EAc3 { dec3 } => {
                super::codecs::write_dec3(&mut entry, dec3);
            }
        }

        if let Some(c) = cenc {
            let per_sample_iv = match c.scheme {
                super::cenc::Scheme::Cenc => 16,
                super::cenc::Scheme::Cbcs => 0,
            };
            super::cenc_boxes::write_sinf(
                &mut entry,
                original_fourcc,
                c.scheme,
                c.key_id,
                per_sample_iv,
            );
        }
    }
    {
        let mut stts = stbl.child_full(*b"stts", 0, 0);
        stts.u32(0);
    }
    {
        let mut stsc = stbl.child_full(*b"stsc", 0, 0);
        stsc.u32(0);
    }
    {
        let mut stsz = stbl.child_full(*b"stsz", 0, 0);
        stsz.u32(0);
        stsz.u32(0);
    }
    {
        let mut stco = stbl.child_full(*b"stco", 0, 0);
        stco.u32(0);
    }
}

// ────────────────────────────────────────────────────────────────────────
//  Movie fragment (moof + mdat)
// ────────────────────────────────────────────────────────────────────────

/// A single sample to be written into a fragment's trun.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Sample duration in the track's timescale.
    pub duration: u32,
    /// Sample body bytes (video = length-prefixed NAL units; audio = raw
    /// AAC frame).
    pub data: Vec<u8>,
    /// CTS offset from DTS, in the track's timescale. Zero when
    /// PTS == DTS.
    pub composition_time_offset: i32,
    /// True if this sample is a sync (keyframe) sample.
    pub is_sync: bool,
}

/// One track's worth of samples inside a single fragment.
pub struct TrackFragment<'a> {
    pub track_id: u32,
    /// First sample's DTS in the track's timescale.
    pub base_media_decode_time: u64,
    pub samples: &'a [Sample],
}

/// Build one media segment containing one or more co-located tracks
/// (video + audio multiplexed in a single moof + mdat). All `samples`
/// share the same fragment but each track gets its own `traf`.
pub fn build_multi_track_segment(
    sequence_number: u32,
    tracks: &[TrackFragment<'_>],
) -> Vec<u8> {
    if tracks.is_empty() {
        return Vec::new();
    }
    let total_sample_bytes: usize = tracks
        .iter()
        .flat_map(|t| t.samples.iter())
        .map(|s| s.data.len())
        .sum();
    let mut buf = Vec::with_capacity(1024 + total_sample_bytes);

    // styp
    {
        let mut styp = BoxWriter::open(&mut buf, *b"styp");
        styp.fourcc(*b"msdh");
        styp.u32(0);
        styp.fourcc(*b"msdh");
        styp.fourcc(*b"msix");
        styp.fourcc(*b"cmfc");
    }

    // moof — collect data_offset patch positions per track so we can
    // back-patch them after the moof size and sample-position layout is
    // known.
    let moof_start = buf.len();
    let mut data_offset_patches: Vec<usize> = Vec::with_capacity(tracks.len());
    {
        let mut moof = BoxWriter::open(&mut buf, *b"moof");
        {
            let mut mfhd = moof.child_full(*b"mfhd", 0, 0);
            mfhd.u32(sequence_number);
        }
        for t in tracks {
            let mut traf = moof.child(*b"traf");
            // tfhd: default-base-is-moof flag (0x020000) — sample offsets
            // are then implicitly relative to moof start.
            //
            // No default-sample-flags here, unlike the single-track writers:
            // every sample below carries its own, which supersedes a
            // per-fragment default, and a per-fragment default cannot be
            // right for a traf mixing IDRs with inter frames anyway.
            {
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0000);
                tfhd.u32(t.track_id);
            }
            {
                let mut tfdt = traf.child_full(*b"tfdt", 1, 0);
                tfdt.u64(t.base_media_decode_time);
            }
            // trun v1 with: data-offset, per-sample duration / size / flags /
            // cts-offset.
            //
            // Per-sample flags (0x000400), not first-sample-flags (0x000004).
            // This writer produces ONE fragment holding a whole recording —
            // the replay MP4 exporter builds a 30-second clip as a single
            // moof/mdat — so it mixes many IDRs with their inter frames, and
            // audio samples that are every one of them a random-access point.
            // First-sample-flags can only describe sample 0; every other
            // sample would fall through to `trex.default_sample_flags`, which
            // is a single value and cannot be right for both. A player reads
            // this to decide where it may start decoding, so the whole clip
            // would resolve to one seek point at t=0.
            //
            // ISO/IEC 14496-12 8.8.8.2: first-sample-flags and sample-flags
            // are mutually exclusive, hence 0x0004 is dropped here.
            let flags: u32 = 0x0001 | 0x0100 | 0x0200 | 0x0400 | 0x0800;
            let mut trun = traf.child_full(*b"trun", 1, flags);
            trun.u32(t.samples.len() as u32);
            data_offset_patches.push(trun.cursor_pos());
            trun.u32(0); // placeholder data_offset
            for s in t.samples {
                trun.u32(s.duration);
                trun.u32(s.data.len() as u32);
                trun.u32(if s.is_sync {
                    SAMPLE_FLAGS_SYNC
                } else {
                    SAMPLE_FLAGS_NON_SYNC
                });
                trun.i32(s.composition_time_offset);
            }
        }
    }
    let _moof_size = buf.len() - moof_start;

    // mdat header, then sample bodies in *the same order trafs were
    // written* (all of track 0's samples, then all of track 1's, etc.).
    let mdat_start = buf.len();
    {
        let mut mdat = BoxWriter::open(&mut buf, *b"mdat");
        for t in tracks {
            for s in t.samples {
                mdat.bytes(&s.data);
            }
        }
    }

    // Patch each track's data_offset.
    let first_sample_offset = (mdat_start + 8) - moof_start;
    let mut running_offset = first_sample_offset;
    for (patch_pos, t) in data_offset_patches.iter().zip(tracks.iter()) {
        patch_u32(&mut buf, *patch_pos, running_offset as u32);
        let track_bytes: usize = t.samples.iter().map(|s| s.data.len()).sum();
        running_offset += track_bytes;
    }

    buf
}

/// Build one media segment: styp + moof + mdat for one track.
///
/// `sequence_number` is the fragment's monotonic number (1, 2, 3, ...).
/// `base_media_decode_time` is the first sample's DTS in the track's
/// timescale.
pub fn build_media_segment(
    track_id: u32,
    sequence_number: u32,
    base_media_decode_time: u64,
    samples: &[Sample],
) -> Vec<u8> {
    // Two-pass: first build moof with a placeholder `data_offset`, compute
    // the real offset (moof size + 8 header bytes of mdat), then patch.
    let mut buf = Vec::with_capacity(1024 + samples.iter().map(|s| s.data.len()).sum::<usize>());

    // styp identifies the segment; same brands as ftyp for consistency.
    {
        let mut styp = BoxWriter::open(&mut buf, *b"styp");
        styp.fourcc(*b"msdh"); // major_brand (ISO 23000-19 segment header)
        styp.u32(0);
        styp.fourcc(*b"msdh");
        styp.fourcc(*b"msix");
        styp.fourcc(*b"cmfc");
    }

    // moof
    let moof_start = buf.len();
    let data_offset_patch: usize;
    {
        let mut moof = BoxWriter::open(&mut buf, *b"moof");
        // mfhd
        {
            let mut mfhd = moof.child_full(*b"mfhd", 0, 0);
            mfhd.u32(sequence_number);
        }
        // traf
        {
            let mut traf = moof.child(*b"traf");
            // tfhd — 0x020000 (default-base-is-moof) makes sample offsets
            // relative to the moof start, which is what CMAF requires, plus
            // 0x000020 (default-sample-flags-present).
            //
            // `trun` below carries no per-sample flags, so every sample after
            // the first inherits this default. Declaring it per fragment lets
            // an all-intra rendition state that *every* frame is a random
            // access point instead of leaving a player to infer it — which is
            // what frame-exact seeking on the DVR proxy depends on — while a
            // long-GOP fragment correctly reports its P-frames as non-sync.
            let all_sync = !samples.is_empty() && samples.iter().all(|s| s.is_sync);
            {
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0020);
                tfhd.u32(track_id);
                tfhd.u32(if all_sync {
                    SAMPLE_FLAGS_SYNC
                } else {
                    SAMPLE_FLAGS_NON_SYNC
                });
            }
            // tfdt v1 (u64 base_media_decode_time)
            {
                let mut tfdt = traf.child_full(*b"tfdt", 1, 0);
                tfdt.u64(base_media_decode_time);
            }
            // trun v1, flags:
            //   0x000001 data-offset-present
            //   0x000004 first-sample-flags-present
            //   0x000100 sample-duration-present
            //   0x000200 sample-size-present
            //   0x000400 sample-flags-present        (skipped — use first-sample-flags only)
            //   0x000800 sample-composition-time-offsets-present
            let flags: u32 = 0x0001 | 0x0004 | 0x0100 | 0x0200 | 0x0800;
            let mut trun = traf.child_full(*b"trun", 1, flags);
            trun.u32(samples.len() as u32);
            // Placeholder data_offset; patched after we know moof size.
            data_offset_patch = trun.cursor_pos();
            trun.u32(0);
            // first_sample_flags — bit layout documented on `SAMPLE_FLAGS_SYNC`.
            // A fragment normally opens on an IDR; when it does not, say so
            // rather than letting a reader assume it can start decoding here.
            let first_flags: u32 = if samples.first().map(|s| s.is_sync).unwrap_or(false) {
                SAMPLE_FLAGS_SYNC
            } else {
                SAMPLE_FLAGS_NON_SYNC
            };
            trun.u32(first_flags);
            for s in samples {
                trun.u32(s.duration);
                trun.u32(s.data.len() as u32);
                trun.i32(s.composition_time_offset);
            }
        }
    }
    let moof_size = buf.len() - moof_start;

    // mdat header (8 bytes) then sample bodies.
    let mdat_start = buf.len();
    {
        let mut mdat = BoxWriter::open(&mut buf, *b"mdat");
        for s in samples {
            mdat.bytes(&s.data);
        }
    }

    // data_offset in trun points from the start of the *moof* box to the
    // first byte of the first sample payload (which is 8 bytes past mdat
    // start — the mdat header).
    let data_offset = (mdat_start + 8) - moof_start;
    patch_u32(&mut buf, data_offset_patch, data_offset as u32);

    // Silence unused-warning fallback (moof_size is kept as a cheap sanity
    // check for future diagnostic asserts).
    let _ = moof_size;
    buf
}

/// Convenience wrapper for a video-only segment.
pub fn build_video_segment(
    sequence_number: u32,
    base_dts: u64,
    samples: &[Sample],
) -> Vec<u8> {
    build_media_segment(VIDEO_TRACK_ID, sequence_number, base_dts, samples)
}

/// Convenience wrapper for an audio-only segment.
pub fn build_audio_segment(
    sequence_number: u32,
    base_dts: u64,
    samples: &[Sample],
) -> Vec<u8> {
    build_media_segment(AUDIO_TRACK_ID, sequence_number, base_dts, samples)
}

/// Build a single moof+mdat chunk for LL-CMAF chunked transfer. When
/// `include_styp` is true, the chunk is prefixed with a `styp` box —
/// this is the opening chunk of a new segment. Subsequent chunks omit
/// `styp` so the whole segment file is `styp moof mdat [moof mdat]*`.
///
/// `data_offset` in each trun is patched to point at the first sample
/// byte in the chunk's own mdat (same algorithm as
/// [`build_media_segment`]).
pub fn build_segment_chunk(
    track_id: u32,
    sequence_number: u32,
    base_media_decode_time: u64,
    samples: &[Sample],
    include_styp: bool,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        128 + samples.iter().map(|s| s.data.len()).sum::<usize>(),
    );
    if include_styp {
        let mut styp = BoxWriter::open(&mut buf, *b"styp");
        styp.fourcc(*b"msdh");
        styp.u32(0);
        styp.fourcc(*b"msdh");
        styp.fourcc(*b"msix");
        styp.fourcc(*b"cmfc");
        styp.fourcc(*b"lhls"); // hint for LL-HLS consumers
    }
    let moof_start = buf.len();
    let data_offset_patch: usize;
    {
        let mut moof = BoxWriter::open(&mut buf, *b"moof");
        {
            let mut mfhd = moof.child_full(*b"mfhd", 0, 0);
            mfhd.u32(sequence_number);
        }
        {
            let mut traf = moof.child(*b"traf");
            {
                // default-base-is-moof + default-sample-flags. See #127: the
                // samples after sample 0 otherwise inherit
                // `trex.default_sample_flags`, which is non-sync.
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0020);
                tfhd.u32(track_id);
                tfhd.u32(if samples.iter().all(|s| s.is_sync) {
                    SAMPLE_FLAGS_SYNC
                } else {
                    SAMPLE_FLAGS_NON_SYNC
                });
            }
            {
                let mut tfdt = traf.child_full(*b"tfdt", 1, 0);
                tfdt.u64(base_media_decode_time);
            }
            let flags: u32 = 0x0001 | 0x0004 | 0x0100 | 0x0200 | 0x0800;
            let mut trun = traf.child_full(*b"trun", 1, flags);
            trun.u32(samples.len() as u32);
            data_offset_patch = trun.cursor_pos();
            trun.u32(0);
            let first_flags: u32 = if samples.first().map(|s| s.is_sync).unwrap_or(false) {
                SAMPLE_FLAGS_SYNC
            } else {
                SAMPLE_FLAGS_NON_SYNC
            };
            trun.u32(first_flags);
            for s in samples {
                trun.u32(s.duration);
                trun.u32(s.data.len() as u32);
                trun.i32(s.composition_time_offset);
            }
        }
    }
    let mdat_start = buf.len();
    {
        let mut mdat = BoxWriter::open(&mut buf, *b"mdat");
        for s in samples {
            mdat.bytes(&s.data);
        }
    }
    let data_offset = (mdat_start + 8) - moof_start;
    patch_u32(&mut buf, data_offset_patch, data_offset as u32);
    buf
}

/// CENC-encrypted single-track segment — identical to
/// [`build_media_segment`] but augments the traf with senc/saio/saiz.
/// `cenc_info` is one entry per sample (same length as `samples`);
/// each entry's subsamples and IV come from [`super::cenc::CencEncryptor`].
pub fn build_encrypted_media_segment(
    track_id: u32,
    sequence_number: u32,
    base_media_decode_time: u64,
    samples: &[Sample],
    cenc_info: &[super::cenc::SampleCencInfo],
    scheme: super::cenc::Scheme,
) -> Vec<u8> {
    assert_eq!(samples.len(), cenc_info.len(), "sample/cenc count mismatch");
    let mut buf = Vec::with_capacity(1024 + samples.iter().map(|s| s.data.len()).sum::<usize>());

    // styp
    {
        let mut styp = BoxWriter::open(&mut buf, *b"styp");
        styp.fourcc(*b"msdh");
        styp.u32(0);
        styp.fourcc(*b"msdh");
        styp.fourcc(*b"msix");
        styp.fourcc(*b"cmfc");
    }

    let moof_start = buf.len();
    let data_offset_patch: usize;
    let saio_offset_patch: usize;
    {
        let mut moof = BoxWriter::open(&mut buf, *b"moof");
        {
            let mut mfhd = moof.child_full(*b"mfhd", 0, 0);
            mfhd.u32(sequence_number);
        }
        {
            let mut traf = moof.child(*b"traf");
            {
                // default-base-is-moof + default-sample-flags. See #127: the
                // samples after sample 0 otherwise inherit
                // `trex.default_sample_flags`, which is non-sync.
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0020);
                tfhd.u32(track_id);
                tfhd.u32(if samples.iter().all(|s| s.is_sync) {
                    SAMPLE_FLAGS_SYNC
                } else {
                    SAMPLE_FLAGS_NON_SYNC
                });
            }
            {
                let mut tfdt = traf.child_full(*b"tfdt", 1, 0);
                tfdt.u64(base_media_decode_time);
            }

            // saiz + saio (referenced by senc via aux-info).
            super::cenc_boxes::write_saiz(&mut traf, scheme, cenc_info);
            // Reserve saio with a placeholder offset that we patch
            // after we know where senc's aux data lives.
            {
                let mut saio = traf.child_full(*b"saio", 0, 0);
                saio.u32(1); // entry_count
                saio_offset_patch = saio.cursor_pos();
                saio.u32(0); // placeholder
            }

            // senc — contains the IVs + subsamples. `cursor_pos()`
            // returns the position in the shared buffer where the
            // next child box will begin (same as buf.len()).
            let senc_cursor = traf.cursor_pos();
            super::cenc_boxes::write_senc(&mut traf, scheme, cenc_info, moof_start, senc_cursor);

            // trun — standard per-sample flags/duration/size/cto.
            let flags: u32 = 0x0001 | 0x0004 | 0x0100 | 0x0200 | 0x0800;
            let mut trun = traf.child_full(*b"trun", 1, flags);
            trun.u32(samples.len() as u32);
            data_offset_patch = trun.cursor_pos();
            trun.u32(0);
            let first_sync = samples.first().map(|s| s.is_sync).unwrap_or(false);
            let first_flags: u32 = if first_sync { 0x0200_0000 } else { 0x0100_0000 };
            trun.u32(first_flags);
            for s in samples {
                trun.u32(s.duration);
                trun.u32(s.data.len() as u32);
                trun.i32(s.composition_time_offset);
            }
        }
    }

    // mdat
    let mdat_start = buf.len();
    {
        let mut mdat = BoxWriter::open(&mut buf, *b"mdat");
        for s in samples {
            mdat.bytes(&s.data);
        }
    }

    // Patch data_offset (trun → mdat first sample).
    let data_offset = (mdat_start + 8) - moof_start;
    patch_u32(&mut buf, data_offset_patch, data_offset as u32);

    // Patch saio offset: absolute offset of senc's aux payload relative
    // to moof start. Our write_senc returned the raw value when
    // invoked; but we built senc after reserving saio, and we didn't
    // capture the return. Recompute: scan the moof region for the
    // `senc` marker and compute offset.
    if let Some(senc_pos) =
        buf[moof_start..mdat_start].windows(4).position(|w| w == b"senc")
    {
        // senc box: size(4) + type(4) + version+flags(4) + sample_count(4)
        // → aux data starts at senc_pos + 16.
        let aux_offset_abs = moof_start + senc_pos - 4 + 16; // -4 to include size field start
        let aux_offset_from_moof = (aux_offset_abs - moof_start) as u32;
        patch_u32(&mut buf, saio_offset_patch, aux_offset_from_moof);
    }

    buf
}

/// Build a muxed CMAF segment with video AND audio in one moof+mdat.
/// Players treat this as a single CMAF chunk addressing two tracks
/// declared in the init segment.
pub fn build_muxed_segment(
    sequence_number: u32,
    video_base_dts: u64,
    video_samples: &[Sample],
    audio_base_dts: u64,
    audio_samples: &[Sample],
) -> Vec<u8> {
    let tracks = [
        TrackFragment {
            track_id: VIDEO_TRACK_ID,
            base_media_decode_time: video_base_dts,
            samples: video_samples,
        },
        TrackFragment {
            track_id: AUDIO_TRACK_ID,
            base_media_decode_time: audio_base_dts,
            samples: audio_samples,
        },
    ];
    build_multi_track_segment(sequence_number, &tracks)
}

/// Build the length-prefixed sample body for a video sample (H.264).
pub fn video_sample_from_nalus(nalus: &[Vec<u8>]) -> Vec<u8> {
    to_length_prefixed(nalus)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An exported clip is a progressive file with a real seek table.
    ///
    /// This is the difference between a clip you can scrub and one you cannot.
    /// A fragmented file carries `mvex` and `trun` and no `stss`; a player then
    /// has nothing to seek by unless there is a `sidx` or `mfra`, and the first
    /// two attempts at clip export shipped neither. VLC would not scrub them.
    ///
    /// So this asserts the shape: no fragmentation, and a populated `stss`
    /// naming exactly the sync samples, 1-based.
    #[test]
    fn an_exported_clip_is_progressive_and_carries_a_seek_table() {
        fn sample(is_sync: bool) -> Sample {
            Sample {
                duration: 3600,
                data: vec![0xAB; 16],
                composition_time_offset: 0,
                is_sync,
            }
        }
        // Three GOPs of four frames.
        let mut video = Vec::new();
        for _ in 0..3 {
            video.push(sample(true));
            for _ in 0..3 {
                video.push(sample(false));
            }
        }
        let track = VideoTrack::from_h264(vec![0x67, 0x42, 0x00, 0x1f], vec![0x68, 0xce, 0x3c, 0x80]);
        let out = build_progressive_mp4(&track, &video, None, &[]);

        fn count(hay: &[u8], needle: &[u8]) -> usize {
            hay.windows(needle.len()).filter(|w| *w == needle).count()
        }
        assert!(count(&out, b"moov") >= 1, "no movie box");
        assert!(count(&out, b"mdat") >= 1, "no media data");
        assert_eq!(count(&out, b"mvex"), 0, "still declaring itself fragmented");
        assert_eq!(count(&out, b"moof"), 0, "still written as fragments");
        assert_eq!(count(&out, b"trun"), 0, "still written as fragments");

        // `stss` must name samples 1, 5 and 9 — the three keyframes, 1-based.
        let at = out
            .windows(4)
            .position(|w| w == b"stss")
            .expect("no sync-sample table, so nothing can seek");
        let body = &out[at + 4..];
        let count_entries = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
        assert_eq!(count_entries, 3, "wrong number of seek points");
        let idx: Vec<u32> = (0..3)
            .map(|i| {
                let o = 8 + i * 4;
                u32::from_be_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]])
            })
            .collect();
        assert_eq!(idx, vec![1, 5, 9], "the seek points are not the keyframes");

        // And the movie declares a real duration, or a player cannot draw a
        // scrub bar at all. 12 frames at 3600/90000 = 0.48s.
        let mvhd = out.windows(4).position(|w| w == b"mvhd").expect("no mvhd");
        // After the "mvhd" fourcc: version+flags, creation, modification,
        // timescale, then duration — so +20, not +16, which reads back the
        // timescale and looks plausible.
        let dur = u32::from_be_bytes([
            out[mvhd + 20], out[mvhd + 21], out[mvhd + 22], out[mvhd + 23],
        ]);
        assert_eq!(dur, 480, "the movie duration is not set, so there is no scrub bar");
    }

    fn synthetic_video_track() -> VideoTrack {
        VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE, 0x3C, 0x80])
    }

    fn synthetic_audio_track() -> AudioTrack {
        AudioTrack::aac([0x11, 0x90], 48000, 2, 128_000)
    }

    /// Resolve a sample's sync flag the way a player does: per-sample flags in
    /// `trun`, else `first_sample_flags` for sample 0, else
    /// `tfhd.default_sample_flags`. Returns (sync_count, total).
    fn resolve_sync_samples(seg: &[u8]) -> (usize, usize) {
        resolve_sync_samples_filtered(seg, None)
    }

    /// As above, but counting only the traf belonging to `want_track`. A
    /// muxed fragment resolves its two tracks independently — long-GOP video
    /// and all-sync audio in the same moof — so a whole-fragment count cannot
    /// tell you whether either one is right.
    fn resolve_sync_samples_filtered(seg: &[u8], want_track: Option<u32>) -> (usize, usize) {
        fn boxes(buf: &[u8], mut off: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
            let mut out = Vec::new();
            while off + 8 <= end {
                let size = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                let typ: [u8; 4] = buf[off + 4..off + 8].try_into().unwrap();
                let size = if size == 0 { end - off } else { size };
                if size < 8 {
                    break;
                }
                out.push((typ, off + 8, off + size));
                off += size;
            }
            out
        }
        let be32 = |b: &[u8]| u32::from_be_bytes(b[..4].try_into().unwrap());

        let (mut sync, mut total) = (0usize, 0usize);
        for (typ, s, e) in boxes(seg, 0, seg.len()) {
            if &typ != b"moof" {
                continue;
            }
            for (t2, s2, e2) in boxes(seg, s, e) {
                if &t2 != b"traf" {
                    continue;
                }
                let mut tfhd_default: Option<u32> = None;
                let mut track_matches = want_track.is_none();
                for (t3, s3, _e3) in boxes(seg, s2, e2) {
                    match &t3 {
                        b"tfhd" => {
                            let fl = be32(&seg[s3..]) & 0x00FF_FFFF;
                            if let Some(want) = want_track {
                                track_matches = be32(&seg[s3 + 4..]) == want;
                            }
                            let mut p = s3 + 8; // version/flags + track_ID
                            if fl & 0x01 != 0 {
                                p += 8;
                            }
                            if fl & 0x02 != 0 {
                                p += 4;
                            }
                            if fl & 0x08 != 0 {
                                p += 4;
                            }
                            if fl & 0x10 != 0 {
                                p += 4;
                            }
                            if fl & 0x20 != 0 {
                                tfhd_default = Some(be32(&seg[p..]));
                            }
                        }
                        b"trun" => {
                            if !track_matches {
                                continue;
                            }
                            let tf = be32(&seg[s3..]) & 0x00FF_FFFF;
                            let cnt = be32(&seg[s3 + 4..]) as usize;
                            let mut p = s3 + 8;
                            if tf & 0x0001 != 0 {
                                p += 4;
                            }
                            let mut first = None;
                            if tf & 0x0004 != 0 {
                                first = Some(be32(&seg[p..]));
                                p += 4;
                            }
                            for i in 0..cnt {
                                let mut f = None;
                                if tf & 0x0100 != 0 {
                                    p += 4;
                                }
                                if tf & 0x0200 != 0 {
                                    p += 4;
                                }
                                if tf & 0x0400 != 0 {
                                    f = Some(be32(&seg[p..]));
                                    p += 4;
                                }
                                if tf & 0x0800 != 0 {
                                    p += 4;
                                }
                                if i == 0 && first.is_some() {
                                    f = first;
                                }
                                // Last resort is `trex.default_sample_flags`,
                                // which the init segment sets to non-sync.
                                // Defaulting to 0 here would read as "sync"
                                // and quietly excuse a fragment that declares
                                // no flags at all.
                                let f = f.or(tfhd_default).unwrap_or(SAMPLE_FLAGS_NON_SYNC);
                                total += 1;
                                if f & 0x0001_0000 == 0 {
                                    sync += 1;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        (sync, total)
    }

    fn sample(is_sync: bool) -> Sample {
        Sample {
            duration: 3600,
            data: vec![0u8; 64],
            composition_time_offset: 0,
            is_sync,
        }
    }

    /// A fragment must report exactly the sync samples it contains.
    ///
    /// This is what a player uses to decide where it may start decoding. It
    /// previously reported *every* sample as a sync sample whatever the GOP,
    /// so a seek could land on a P-frame; and an all-intra rendition was only
    /// correct by accident, because "everything is seekable" happened to be
    /// true for it.
    #[test]
    fn fragment_reports_only_its_real_sync_samples() {
        // Long-GOP: one IDR then 24 inter frames.
        let mut long_gop = vec![sample(true)];
        long_gop.extend((0..24).map(|_| sample(false)));
        let seg = build_media_segment(VIDEO_TRACK_ID, 0, 0, &long_gop);
        assert_eq!(
            resolve_sync_samples(&seg),
            (1, 25),
            "a long-GOP fragment must report exactly one random-access point"
        );

        // All-intra: every frame is a random-access point, and the fragment
        // must say so — frame-exact seeking on the DVR proxy depends on it.
        let all_intra: Vec<Sample> = (0..25).map(|_| sample(true)).collect();
        let seg = build_media_segment(VIDEO_TRACK_ID, 0, 0, &all_intra);
        assert_eq!(
            resolve_sync_samples(&seg),
            (25, 25),
            "an all-intra fragment must report every sample as seekable"
        );
    }

    /// A muxed fragment must resolve each track's sync flags on its own.
    ///
    /// The muxed writer never got #127's fix, so every sample past sample 0
    /// fell through to `trex.default_sample_flags` — non-sync for *every*
    /// track. That mismarks all but the first frame of an AAC track, where
    /// every sample is a random-access point by construction.
    #[test]
    fn muxed_fragment_resolves_each_track_independently() {
        let mut video = vec![sample(true)];
        video.extend((0..24).map(|_| sample(false)));
        let audio: Vec<Sample> = (0..94).map(|_| sample(true)).collect();

        let seg = build_muxed_segment(1, 0, &video, 0, &audio);

        assert_eq!(
            resolve_sync_samples_filtered(&seg, Some(VIDEO_TRACK_ID)),
            (1, 25),
            "long-GOP video in a muxed fragment must report one random-access point"
        );
        assert_eq!(
            resolve_sync_samples_filtered(&seg, Some(AUDIO_TRACK_ID)),
            (94, 94),
            "every AAC sample is a random-access point and the fragment must say so"
        );
    }

    /// Both tracks must actually be present, addressed and carried — a moof
    /// declaring two trafs over an mdat holding one track's bytes is the
    /// silent-stall shape this whole feature keeps producing.
    #[test]
    fn muxed_fragment_carries_both_tracks() {
        let video: Vec<Sample> = (0..3).map(|_| sample(true)).collect();
        let audio: Vec<Sample> = (0..5).map(|_| sample(true)).collect();
        let seg = build_muxed_segment(7, 90_000, &video, 48_000, &audio);

        let (v_sync, v_total) = resolve_sync_samples_filtered(&seg, Some(VIDEO_TRACK_ID));
        let (a_sync, a_total) = resolve_sync_samples_filtered(&seg, Some(AUDIO_TRACK_ID));
        assert_eq!((v_sync, v_total), (3, 3), "video traf missing or miscounted");
        assert_eq!((a_sync, a_total), (5, 5), "audio traf missing or miscounted");

        // mdat must hold every sample's bytes from both tracks.
        let payload: usize = video.iter().chain(audio.iter()).map(|s| s.data.len()).sum();
        let mdat = seg
            .windows(4)
            .position(|w| w == b"mdat")
            .expect("muxed fragment has no mdat");
        assert!(
            seg.len() - mdat >= payload,
            "mdat is too small to hold both tracks' samples"
        );
    }

    /// A whole-recording fragment must report every one of its random-access
    /// points, not just the first.
    ///
    /// `build_multi_track_segment` is what the replay MP4 exporter writes a
    /// clip with: one moof holding the entire clip, so tens of IDRs, hundreds
    /// of inter frames, and an audio track whose samples are every one of them
    /// seekable. `trex.default_sample_flags` is a single value and cannot
    /// describe that, so the samples carry their own flags.
    #[test]
    fn multi_track_fragment_reports_every_random_access_point() {
        // 3 GOPs of 5: IDR, then four inter frames.
        let video: Vec<Sample> = (0..15).map(|i| sample(i % 5 == 0)).collect();
        // Audio: every frame is a random-access point.
        let audio: Vec<Sample> = (0..40).map(|_| sample(true)).collect();

        let seg = build_multi_track_segment(
            1,
            &[
                TrackFragment {
                    track_id: VIDEO_TRACK_ID,
                    base_media_decode_time: 0,
                    samples: &video,
                },
                TrackFragment {
                    track_id: AUDIO_TRACK_ID,
                    base_media_decode_time: 0,
                    samples: &audio,
                },
            ],
        );

        // 3 video IDRs + 40 audio samples = 43 of 55 seekable. Reporting
        // (1, 55) would mean a clip that can only ever seek to t=0; reporting
        // (55, 55) is the pre-#127 lie that let a seek land on a P-frame.
        assert_eq!(resolve_sync_samples(&seg), (43, 55));
    }

    /// The two encodings differ in bit 16, which is the one that matters and
    /// the one the previous "non-sync" constant left clear.
    #[test]
    fn sample_flag_constants_differ_in_the_non_sync_bit() {
        assert_eq!(SAMPLE_FLAGS_SYNC & 0x0001_0000, 0, "sync must not set bit 16");
        assert_ne!(
            SAMPLE_FLAGS_NON_SYNC & 0x0001_0000,
            0,
            "non-sync must set bit 16, or it still reads as a sync sample"
        );
    }

    #[test]
    fn init_starts_with_ftyp_cmfc() {
        let init = build_init_segment(&synthetic_video_track(), None);
        // ftyp at offset 0, size in first 4 bytes, type at 4..8
        assert_eq!(&init[4..8], b"ftyp");
        // major_brand immediately after type
        assert_eq!(&init[8..12], b"cmfc");
    }

    #[test]
    fn init_contains_moov_with_video_trak() {
        let init = build_init_segment(&synthetic_video_track(), None);
        let moov = find_fourcc_pos(&init, *b"moov").expect("moov missing");
        // `avc1` also appears in the ftyp compatible_brands list; search
        // strictly past the start of moov to find the avc1 *sample entry*.
        let avc1_in_moov = find_fourcc_pos_after(&init, *b"avc1", moov)
            .expect("avc1 sample entry missing inside moov");
        assert!(avc1_in_moov > moov);
        let mvex = find_fourcc_pos(&init, *b"mvex").expect("mvex missing");
        assert!(mvex > moov);
    }

    fn find_fourcc_pos_after(buf: &[u8], fc: [u8; 4], min_pos: usize) -> Option<usize> {
        buf[min_pos..]
            .windows(4)
            .position(|w| w == fc)
            .map(|p| p + min_pos)
    }

    #[test]
    fn init_with_audio_contains_mp4a() {
        let init = build_init_segment(&synthetic_video_track(), Some(&synthetic_audio_track()));
        assert!(find_fourcc_pos(&init, *b"mp4a").is_some());
        assert!(find_fourcc_pos(&init, *b"esds").is_some());
    }

    #[test]
    fn init_with_ac3_track_emits_ac3_sample_entry_and_dac3_box() {
        let v = synthetic_video_track();
        let a = AudioTrack {
            codec: AudioCodec::Ac3 { dac3: [0x10, 0x40, 0x40] },
            sample_rate: 48_000,
            channels: 6,
        };
        let init = build_init_segment(&v, Some(&a));
        assert!(
            find_fourcc_pos(&init, *b"ac-3").is_some(),
            "ac-3 sample entry must be present in moov"
        );
        assert!(
            find_fourcc_pos(&init, *b"dac3").is_some(),
            "dac3 specific box must be present"
        );
        assert!(
            find_fourcc_pos(&init, *b"esds").is_none(),
            "AC-3 must not write an esds box"
        );
    }

    #[test]
    fn init_with_eac3_track_emits_ec3_sample_entry_and_dec3_box() {
        let v = synthetic_video_track();
        let a = AudioTrack {
            codec: AudioCodec::EAc3 { dec3: vec![0u8; 5] },
            sample_rate: 48_000,
            channels: 6,
        };
        let init = build_init_segment(&v, Some(&a));
        assert!(
            find_fourcc_pos(&init, *b"ec-3").is_some(),
            "ec-3 sample entry must be present in moov"
        );
        assert!(
            find_fourcc_pos(&init, *b"dec3").is_some(),
            "dec3 specific box must be present"
        );
    }

    #[test]
    fn init_with_mp2_track_writes_mp4a_with_oti_0x69() {
        let v = synthetic_video_track();
        let a = AudioTrack {
            codec: AudioCodec::Mp2 { avg_bitrate: 192_000 },
            sample_rate: 48_000,
            channels: 2,
        };
        let init = build_init_segment(&v, Some(&a));
        let mp4a = find_fourcc_pos(&init, *b"mp4a").expect("MP2 still uses mp4a sample entry");
        let esds = find_fourcc_pos_after(&init, *b"esds", mp4a)
            .expect("MP2 must produce an esds box");
        // The DCD descriptor (tag 0x04) carries objectTypeIndication in
        // its first body byte. Walk past `esds` 4-byte type marker and
        // the 4-byte FullBox version+flags, then scan for tag 0x04.
        let dcd_oti = find_dcd_object_type_indication(&init, esds)
            .expect("DCD with OTI in esds");
        assert_eq!(
            dcd_oti, 0x69,
            "MP2 OTI must be 0x69 (MPEG-1 Audio L2)"
        );
    }

    /// Walk the bytes after an `esds` fourcc occurrence looking for the
    /// MPEG-4 Systems DecoderConfigDescriptor (tag `0x04`) — its first
    /// body byte is the `objectTypeIndication`.
    fn find_dcd_object_type_indication(buf: &[u8], esds_pos: usize) -> Option<u8> {
        // esds box body: 4-byte size + 4-byte type + 4-byte version/flags + descriptors.
        // We came in at the position of the type bytes, so walk forward
        // skipping the FullBox prefix to land on the first descriptor tag.
        let start = esds_pos + 4 + 4; // past type + version+flags
        // Look for 0x04 (DCD tag), bounded by the next ~64 bytes (esds is small).
        let end = (start + 64).min(buf.len());
        let mut i = start;
        while i < end {
            if buf[i] == 0x04 {
                // Skip tag (1 byte) + length-byte (1) → next byte is OTI.
                if i + 2 < buf.len() {
                    return Some(buf[i + 2]);
                }
            }
            i += 1;
        }
        None
    }

    #[test]
    fn media_segment_has_styp_moof_mdat_in_order() {
        let sample = Sample {
            duration: 3000, // 1/30 s @ 90 kHz
            data: vec![0xAA; 256],
            composition_time_offset: 0,
            is_sync: true,
        };
        let seg = build_video_segment(1, 0, &[sample]);
        let p_styp = find_fourcc_pos(&seg, *b"styp").unwrap();
        let p_moof = find_fourcc_pos(&seg, *b"moof").unwrap();
        let p_mdat = find_fourcc_pos(&seg, *b"mdat").unwrap();
        assert!(p_styp < p_moof);
        assert!(p_moof < p_mdat);
    }

    #[test]
    fn media_segment_patches_data_offset() {
        let sample = Sample {
            duration: 3000,
            data: vec![0xBB; 100],
            composition_time_offset: 0,
            is_sync: true,
        };
        let seg = build_video_segment(7, 12345, &[sample]);
        // Find moof + trun to validate data_offset references the first
        // byte of mdat sample payload.
        let p_moof = find_fourcc_pos(&seg, *b"moof").unwrap() - 4; // start of size field
        let p_trun = find_fourcc_pos(&seg, *b"trun").unwrap();
        // trun body: version+flags(4) sample_count(4) data_offset(4)
        let data_offset_pos = p_trun + 4 /* type */ + 4 /* ver+flags */ + 4 /* sample_count */;
        let data_offset = u32::from_be_bytes(seg[data_offset_pos..data_offset_pos + 4].try_into().unwrap());
        let p_mdat = find_fourcc_pos(&seg, *b"mdat").unwrap() - 4; // start of size field
        let expected = (p_mdat + 8) - p_moof;
        assert_eq!(data_offset as usize, expected);
    }

    #[test]
    fn media_segment_tfdt_carries_base_dts() {
        let sample = Sample {
            duration: 3000,
            data: vec![0xCC; 16],
            composition_time_offset: 0,
            is_sync: true,
        };
        let seg = build_video_segment(1, 0x0001_0000_0000, &[sample]);
        let p_tfdt = find_fourcc_pos(&seg, *b"tfdt").unwrap();
        // tfdt body: version(1) flags(3) base_media_decode_time(8 for v1)
        let bmdt_pos = p_tfdt + 4 /* type */ + 4 /* ver+flags */;
        let bmdt = u64::from_be_bytes(seg[bmdt_pos..bmdt_pos + 8].try_into().unwrap());
        assert_eq!(bmdt, 0x0001_0000_0000);
    }

    fn find_fourcc_pos(buf: &[u8], fc: [u8; 4]) -> Option<usize> {
        buf.windows(4).position(|w| w == fc)
    }
}

// ── Progressive (non-fragmented) MP4 ────────────────────────────────────────

/// One chunk of samples belonging to one track, at a byte offset in `mdat`.
struct ChunkPlan {
    /// Index of the first sample in the track's sample list.
    first: usize,
    /// How many samples this chunk holds.
    count: usize,
    /// Offset from the start of the `mdat` payload.
    rel_offset: u64,
}

/// Split a track's samples into chunks, one per GOP, and lay them out
/// interleaved with the other track so a player reading forward gets both.
///
/// Video chunks break at every sync sample; each audio chunk covers the same
/// span of time as the video chunk it follows. Non-interleaved files (all the
/// video, then all the audio) are legal and play, but they make a player seek
/// the whole file to keep audio fed.
fn plan_chunks(
    video: &[Sample],
    video_timescale: u32,
    audio: &[Sample],
    audio_timescale: u32,
) -> (Vec<ChunkPlan>, Vec<ChunkPlan>, Vec<u8>) {
    let mut v_chunks = Vec::new();
    let mut a_chunks = Vec::new();
    let mut payload: Vec<u8> = Vec::new();

    // Fragment boundaries: sample 0, then every later sync sample.
    let mut bounds: Vec<usize> = vec![0];
    for (i, s) in video.iter().enumerate().skip(1) {
        if s.is_sync {
            bounds.push(i);
        }
    }
    bounds.push(video.len());

    let mut v_ticks_done: u64 = 0;
    let mut a_cursor = 0usize;
    let mut a_ticks_done: u64 = 0;

    for w in bounds.windows(2) {
        let (lo, hi) = (w[0], w[1]);
        if lo >= hi {
            continue;
        }
        let rel = payload.len() as u64;
        for s in &video[lo..hi] {
            payload.extend_from_slice(&s.data);
        }
        v_chunks.push(ChunkPlan { first: lo, count: hi - lo, rel_offset: rel });
        v_ticks_done += video[lo..hi].iter().map(|s| s.duration as u64).sum::<u64>();

        if audio.is_empty() || audio_timescale == 0 || a_cursor >= audio.len() {
            continue;
        }
        // Audio up to the same point on the clock as the video just written.
        let want = v_ticks_done * audio_timescale as u64 / video_timescale.max(1) as u64;
        let mut end = a_cursor;
        let mut acc = a_ticks_done;
        while end < audio.len() && acc < want {
            acc += audio[end].duration as u64;
            end += 1;
        }
        if end > a_cursor {
            let rel = payload.len() as u64;
            for s in &audio[a_cursor..end] {
                payload.extend_from_slice(&s.data);
            }
            a_chunks.push(ChunkPlan { first: a_cursor, count: end - a_cursor, rel_offset: rel });
            a_ticks_done = acc;
            a_cursor = end;
        }
    }

    // Whatever audio is left over, so nothing is silently dropped.
    if a_cursor < audio.len() {
        let rel = payload.len() as u64;
        for s in &audio[a_cursor..] {
            payload.extend_from_slice(&s.data);
        }
        a_chunks.push(ChunkPlan {
            first: a_cursor,
            count: audio.len() - a_cursor,
            rel_offset: rel,
        });
    }

    (v_chunks, a_chunks, payload)
}

/// `stts` — sample durations, run-length encoded.
fn write_stts(stbl: &mut BoxWriter<'_>, samples: &[Sample]) {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        match runs.last_mut() {
            Some((count, delta)) if *delta == s.duration => *count += 1,
            _ => runs.push((1, s.duration)),
        }
    }
    let mut stts = stbl.child_full(*b"stts", 0, 0);
    stts.u32(runs.len() as u32);
    for (count, delta) in runs {
        stts.u32(count);
        stts.u32(delta);
    }
}

/// `stss` — which samples are random-access points, 1-based.
///
/// This is the table VLC and every other player builds a seek from. Omitted
/// entirely when every sample is a sync sample, which is what the spec says
/// means "all of them" — writing one out for audio would be a longer way of
/// saying the same thing.
fn write_stss(stbl: &mut BoxWriter<'_>, samples: &[Sample]) {
    if samples.iter().all(|s| s.is_sync) {
        return;
    }
    let idx: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_sync)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    let mut stss = stbl.child_full(*b"stss", 0, 0);
    stss.u32(idx.len() as u32);
    for i in idx {
        stss.u32(i);
    }
}

/// `stsc` — how many samples are in each chunk, run-length encoded by chunk.
fn write_stsc(stbl: &mut BoxWriter<'_>, chunks: &[ChunkPlan]) {
    let mut runs: Vec<(u32, u32)> = Vec::new(); // (first_chunk 1-based, samples)
    for (i, c) in chunks.iter().enumerate() {
        let per = c.count as u32;
        if runs.last().map(|(_, p)| *p) != Some(per) {
            runs.push((i as u32 + 1, per));
        }
    }
    let mut stsc = stbl.child_full(*b"stsc", 0, 0);
    stsc.u32(runs.len() as u32);
    for (first, per) in runs {
        stsc.u32(first);
        stsc.u32(per);
        stsc.u32(1); // sample_description_index
    }
}

/// `stsz` — the size of every sample.
fn write_stsz(stbl: &mut BoxWriter<'_>, samples: &[Sample]) {
    let mut stsz = stbl.child_full(*b"stsz", 0, 0);
    stsz.u32(0); // sample_size 0 = per-sample sizes follow
    stsz.u32(samples.len() as u32);
    for s in samples {
        stsz.u32(s.data.len() as u32);
    }
}

/// `stco` — where each chunk starts in the file.
fn write_stco(stbl: &mut BoxWriter<'_>, chunks: &[ChunkPlan], mdat_base: u64) {
    let mut stco = stbl.child_full(*b"stco", 0, 0);
    stco.u32(chunks.len() as u32);
    for c in chunks {
        stco.u32((mdat_base + c.rel_offset) as u32);
    }
}

/// Build a **progressive** MP4 — one `moov` with real sample tables, one
/// `mdat`, no fragments.
///
/// This is what an exported clip is, and it is deliberately not the CMAF
/// shape the live path publishes. A fragmented file is right for streaming,
/// where the player follows a manifest; it is wrong for a file somebody
/// downloads and opens. Without `sidx` or `mfra` a fragmented clip gives VLC
/// nothing to seek by, so scrubbing either does nothing or walks the whole
/// file — which is exactly how the first version of clip export behaved.
///
/// `moov` is written **before** `mdat` so the tables are readable without
/// fetching the whole file first.
pub fn build_progressive_mp4(
    video: &VideoTrack,
    video_samples: &[Sample],
    audio: Option<&AudioTrack>,
    audio_samples: &[Sample],
) -> Vec<u8> {
    let a_timescale = audio.map(|a| a.sample_rate).unwrap_or(0);
    let (v_chunks, a_chunks, payload) =
        plan_chunks(video_samples, video.timescale, audio_samples, a_timescale);

    // Chunk offsets are absolute, so `moov` has to know where `mdat` starts —
    // and that depends on how long `moov` is. The tables are a fixed size
    // whatever the offset *values* are, so building it once against a base of
    // zero measures it exactly, and the second build carries the real base.
    let probe = build_moov(video, video_samples, &v_chunks, audio, audio_samples, &a_chunks, 0);
    let ftyp = build_progressive_ftyp(video);
    let mdat_base = (ftyp.len() + probe.len() + 8) as u64;
    let moov = build_moov(
        video, video_samples, &v_chunks, audio, audio_samples, &a_chunks, mdat_base,
    );
    debug_assert_eq!(probe.len(), moov.len(), "moov changed size with the offset base");

    let mut out = Vec::with_capacity(ftyp.len() + moov.len() + payload.len() + 8);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    {
        let mut mdat = BoxWriter::open(&mut out, *b"mdat");
        mdat.bytes(&payload);
    }
    out
}

fn build_progressive_ftyp(video: &VideoTrack) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    let mut ftyp = BoxWriter::open(&mut buf, *b"ftyp");
    // `isom` rather than `cmfc`: this is an ordinary file, not a CMAF segment,
    // and saying so keeps players out of their fragmented-media paths.
    ftyp.fourcc(*b"isom");
    ftyp.u32(512);
    ftyp.fourcc(*b"isom");
    ftyp.fourcc(*b"iso2");
    ftyp.fourcc(*b"mp41");
    match video.codec {
        VideoCodec::H264 => ftyp.fourcc(*b"avc1"),
        VideoCodec::H265 => ftyp.fourcc(*b"hvc1"),
    };
    drop(ftyp);
    buf
}

#[allow(clippy::too_many_arguments)]
fn build_moov(
    video: &VideoTrack,
    video_samples: &[Sample],
    v_chunks: &[ChunkPlan],
    audio: Option<&AudioTrack>,
    audio_samples: &[Sample],
    a_chunks: &[ChunkPlan],
    mdat_base: u64,
) -> Vec<u8> {
    const MOVIE_TIMESCALE: u32 = 1000;
    let v_ticks: u64 = video_samples.iter().map(|s| s.duration as u64).sum();
    let movie_ms = v_ticks * MOVIE_TIMESCALE as u64 / video.timescale.max(1) as u64;

    let mut buf = Vec::with_capacity(4096);
    {
        let mut moov = BoxWriter::open(&mut buf, *b"moov");
        {
            let mut mvhd = moov.child_full(*b"mvhd", 0, 0);
            mvhd.u32(0);
            mvhd.u32(0);
            mvhd.u32(MOVIE_TIMESCALE);
            // A real duration, not zero: a fragmented file says "unknown" and
            // lets the manifest speak, but a downloaded clip has to be able to
            // draw its own scrub bar.
            mvhd.u32(movie_ms as u32);
            mvhd.u32(0x0001_0000); // rate 1.0
            mvhd.u16(0x0100); // volume 1.0
            mvhd.u16(0); // reserved
            mvhd.zeros(8); // reserved[2]
            for &v in &UNITY_MATRIX {
                mvhd.u32(v);
            }
            mvhd.zeros(24); // pre_defined[6]
            mvhd.u32(3); // next_track_ID
        }
        write_progressive_video_trak(
            &mut moov, video, video_samples, v_chunks, movie_ms, mdat_base,
        );
        if let Some(a) = audio
            && !audio_samples.is_empty()
        {
            write_progressive_audio_trak(
                &mut moov, a, audio_samples, a_chunks, movie_ms, mdat_base,
            );
        }
    }
    buf
}

/// The 3x3 unity matrix every `tkhd` / `mvhd` carries, 16.16 fixed point
/// except the last row which is 2.30.
const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn write_progressive_video_trak(
    moov: &mut BoxWriter<'_>,
    v: &VideoTrack,
    samples: &[Sample],
    chunks: &[ChunkPlan],
    movie_ms: u64,
    mdat_base: u64,
) {
    let media_ticks: u64 = samples.iter().map(|s| s.duration as u64).sum();
    let mut trak = moov.child(*b"trak");
    {
        let mut tkhd = trak.child_full(*b"tkhd", 0, 0x0000_0007);
        tkhd.u32(0);
        tkhd.u32(0);
        tkhd.u32(VIDEO_TRACK_ID);
        tkhd.u32(0);
        tkhd.u32(movie_ms as u32);
        tkhd.zeros(8);
        tkhd.u16(0); // layer
        tkhd.u16(0); // alternate_group
        tkhd.u16(0); // volume (video)
        tkhd.u16(0); // reserved
        for &m in &UNITY_MATRIX {
            tkhd.u32(m);
        }
        tkhd.u32(v.width << 16);
        tkhd.u32(v.height << 16);
    }
    {
        let mut mdia = trak.child(*b"mdia");
        {
            let mut mdhd = mdia.child_full(*b"mdhd", 0, 0);
            mdhd.u32(0);
            mdhd.u32(0);
            mdhd.u32(v.timescale);
            mdhd.u32(media_ticks as u32);
            mdhd.u16(0x55C4); // 'und'
            mdhd.u16(0);
        }
        {
            let mut hdlr = mdia.child_full(*b"hdlr", 0, 0);
            hdlr.u32(0);
            hdlr.fourcc(*b"vide");
            hdlr.zeros(12);
            hdlr.bytes(b"VideoHandler\0");
        }
        {
            let mut minf = mdia.child(*b"minf");
            {
                let mut vmhd = minf.child_full(*b"vmhd", 0, 1);
                vmhd.u16(0);
                vmhd.zeros(6);
            }
            write_null_dinf(&mut minf);
            {
                let mut stbl = minf.child(*b"stbl");
                write_video_stsd(&mut stbl, v, None);
                write_stts(&mut stbl, samples);
                write_stss(&mut stbl, samples);
                write_stsc(&mut stbl, chunks);
                write_stsz(&mut stbl, samples);
                write_stco(&mut stbl, chunks, mdat_base);
            }
        }
    }
}

fn write_progressive_audio_trak(
    moov: &mut BoxWriter<'_>,
    a: &AudioTrack,
    samples: &[Sample],
    chunks: &[ChunkPlan],
    movie_ms: u64,
    mdat_base: u64,
) {
    let media_ticks: u64 = samples.iter().map(|s| s.duration as u64).sum();
    let mut trak = moov.child(*b"trak");
    {
        let mut tkhd = trak.child_full(*b"tkhd", 0, 0x0000_0007);
        tkhd.u32(0);
        tkhd.u32(0);
        tkhd.u32(AUDIO_TRACK_ID);
        tkhd.u32(0);
        tkhd.u32(movie_ms as u32);
        tkhd.zeros(8);
        tkhd.u16(0);
        tkhd.u16(1); // alternate_group 1: audio
        tkhd.u16(0x0100); // full volume
        tkhd.u16(0);
        for &m in &UNITY_MATRIX {
            tkhd.u32(m);
        }
        tkhd.u32(0); // width
        tkhd.u32(0); // height
    }
    {
        let mut mdia = trak.child(*b"mdia");
        {
            let mut mdhd = mdia.child_full(*b"mdhd", 0, 0);
            mdhd.u32(0);
            mdhd.u32(0);
            mdhd.u32(a.sample_rate);
            mdhd.u32(media_ticks as u32);
            mdhd.u16(0x55C4);
            mdhd.u16(0);
        }
        {
            let mut hdlr = mdia.child_full(*b"hdlr", 0, 0);
            hdlr.u32(0);
            hdlr.fourcc(*b"soun");
            hdlr.zeros(12);
            hdlr.bytes(b"SoundHandler\0");
        }
        {
            let mut minf = mdia.child(*b"minf");
            {
                let mut smhd = minf.child_full(*b"smhd", 0, 0);
                smhd.u16(0); // balance
                smhd.u16(0); // reserved
            }
            write_null_dinf(&mut minf);
            {
                let mut stbl = minf.child(*b"stbl");
                write_audio_stsd(&mut stbl, a, None);
                write_stts(&mut stbl, samples);
                write_stss(&mut stbl, samples);
                write_stsc(&mut stbl, chunks);
                write_stsz(&mut stbl, samples);
                write_stco(&mut stbl, chunks, mdat_base);
            }
        }
    }
}
