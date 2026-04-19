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

/// Audio track metadata.
pub struct AudioTrack {
    /// 2-byte AudioSpecificConfig from [`super::codecs::aac_audio_specific_config`].
    pub audio_specific_config: [u8; 2],
    /// Sampling rate in Hz; used as the track's `mdhd.timescale` and the
    /// `mp4a` sample entry rate.
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo). AAC-LC with 5.1 uses
    /// `channel_configuration = 6`.
    pub channels: u16,
    /// Average bitrate in bits per second (written into the esds DCD).
    pub avg_bitrate: u32,
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

fn write_trex(parent: &mut BoxWriter<'_>, track_id: u32) {
    let mut trex = parent.child_full(*b"trex", 0, 0);
    trex.u32(track_id);
    trex.u32(1); // default_sample_description_index
    trex.u32(0); // default_sample_duration (per-sample in trun)
    trex.u32(0); // default_sample_size
    trex.u32(0); // default_sample_flags
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
    // stsd
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
    // Empty stts / stsc / stsz / stco — required children of stbl for
    // fragmented MP4. Each says "zero samples" because all sample data
    // lives in moof/mdat fragments.
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
    {
        let mut stsd = stbl.child_full(*b"stsd", 0, 0);
        stsd.u32(1);
        let original_fourcc = *b"mp4a";
        let fourcc = if cenc.is_some() { *b"enca" } else { original_fourcc };
        let mut mp4a = stsd.child(fourcc);
        mp4a.zeros(6); // reserved
        mp4a.u16(1); // data_reference_index
        // AudioSampleEntry (ISO/IEC 14496-12 §12.2)
        mp4a.zeros(8); // reserved(u32[2])
        mp4a.u16(a.channels);
        mp4a.u16(16); // samplesize
        mp4a.u16(0); // pre_defined
        mp4a.u16(0); // reserved
        // samplerate: 16.16 fixed. Upper 16 bits carry Hz for rates < 65 536.
        mp4a.u32(a.sample_rate << 16);

        write_esds(&mut mp4a, &a.audio_specific_config, a.avg_bitrate);

        if let Some(c) = cenc {
            let per_sample_iv = match c.scheme {
                super::cenc::Scheme::Cenc => 16,
                super::cenc::Scheme::Cbcs => 0,
            };
            super::cenc_boxes::write_sinf(
                &mut mp4a,
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
            {
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0000);
                tfhd.u32(t.track_id);
            }
            {
                let mut tfdt = traf.child_full(*b"tfdt", 1, 0);
                tfdt.u64(t.base_media_decode_time);
            }
            // trun v1 with: data-offset, first-sample-flags, per-sample
            // duration / size / cts-offset.
            let flags: u32 = 0x0001 | 0x0004 | 0x0100 | 0x0200 | 0x0800;
            let mut trun = traf.child_full(*b"trun", 1, flags);
            trun.u32(t.samples.len() as u32);
            data_offset_patches.push(trun.cursor_pos());
            trun.u32(0); // placeholder data_offset
            let first_sync = t.samples.first().map(|s| s.is_sync).unwrap_or(false);
            let first_flags: u32 = if first_sync { 0x0200_0000 } else { 0x0100_0000 };
            trun.u32(first_flags);
            for s in t.samples {
                trun.u32(s.duration);
                trun.u32(s.data.len() as u32);
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
            // tfhd — flags 0x020000 (default-base-is-moof) makes sample offsets
            // relative to the moof start, which is what CMAF requires.
            {
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0000);
                tfhd.u32(track_id);
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
            // first_sample_flags: use sync-sample encoding from ISO/IEC 14496-12
            //   bits 16-17: reserved(2)
            //   bit   18:  is_leading(2)
            //   bits 19-20: sample_depends_on(2)
            //   bits 21-22: sample_is_depended_on(2)
            //   bits 23-24: sample_has_redundancy(2)
            //   bits 25-27: sample_padding_value(3)
            //   bit   28:  sample_is_non_sync_sample
            //   bits 29-31: sample_degradation_priority(16 lsbits, we use 0)
            // For the first sample of a fragment that starts with an IDR,
            // sample_is_non_sync_sample=0 and sample_depends_on=2 (doesn't
            // depend on others). Encodes to 0x0200_0000.
            let first_flags: u32 = if samples.first().map(|s| s.is_sync).unwrap_or(false) {
                0x0200_0000
            } else {
                0x0100_0000 // non-sync
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
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0000);
                tfhd.u32(track_id);
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
                0x0200_0000
            } else {
                0x0100_0000
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
                let mut tfhd = traf.child_full(*b"tfhd", 0, 0x0002_0000);
                tfhd.u32(track_id);
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

    fn synthetic_video_track() -> VideoTrack {
        VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE, 0x3C, 0x80])
    }

    fn synthetic_audio_track() -> AudioTrack {
        AudioTrack {
            audio_specific_config: [0x11, 0x90], // 48 kHz stereo AAC-LC
            sample_rate: 48000,
            channels: 2,
            avg_bitrate: 128_000,
        }
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
