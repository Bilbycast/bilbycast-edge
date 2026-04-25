// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! MP4 / MOV demuxer for the file-backed media-player input.
//!
//! Pulls H.264 video and AAC audio elementary streams off the container
//! using the pure-Rust `mp4` crate, converts H.264 samples from AVCC
//! (length-prefixed NALs) to Annex-B (start-code prefixed), wraps AAC
//! samples in ADTS, and re-muxes both into a fresh MPEG-TS stream via
//! the shared [`crate::engine::rtmp::ts_mux::TsMuxer`].
//!
//! HEVC and other codecs are deliberately out of scope for the first
//! cut — the `mp4` crate's HevcConfig surface doesn't expose the
//! VPS/SPS/PPS we'd need for a clean Annex-B emit. Operators with HEVC
//! masters can transcode to .ts and use a [`MediaPlayerSource::Ts`] entry
//! instead, which is the lowest-CPU path anyway.

use std::collections::BinaryHeap;
use std::path::Path;

use anyhow::{Result, anyhow};
use bytes::BytesMut;
use mp4::{AudioObjectType, MediaType, SampleFreqIndex, TrackType};
use tokio::time::{Duration, Instant};

use super::{BUNDLE_SIZE, PlayerSession, emit_bundle};
use crate::engine::rtmp::ts_mux::TsMuxer;

/// Stream type byte stamped on the synthesised PMT for AAC ADTS audio.
const STREAM_TYPE_AAC: u8 = 0x0F;
/// Stream type byte stamped on the synthesised PMT for H.264 video.
const STREAM_TYPE_H264: u8 = 0x1B;

pub async fn play_mp4_file(path: &Path, session: &mut PlayerSession<'_>) -> Result<()> {
    // The `mp4` crate is sync-only — open + parse on a blocking thread,
    // then drive sample emission back in the async task with paced sleeps.
    let path_owned = path.to_path_buf();
    let demux: DemuxResult = tokio::task::spawn_blocking(move || demux_file(&path_owned))
        .await
        .map_err(|e| anyhow!("mp4 spawn_blocking join failed: {e}"))??;

    play_demuxed(demux, session).await
}

struct DemuxResult {
    video: Option<TrackData>,
    audio: Option<TrackData>,
}

struct TrackData {
    timescale: u32,
    samples: Vec<DemuxedSample>,
    /// SPS / PPS for video; ADTS profile + sr_index + ch_config for audio.
    extra: TrackExtra,
}

enum TrackExtra {
    Avc {
        sps: Vec<u8>,
        pps: Vec<u8>,
    },
    Aac {
        adts_profile: u8,
        sr_index: u8,
        ch_config: u8,
    },
}

struct DemuxedSample {
    /// Start time in track timescale units (DTS for video, PTS for audio).
    start_time: u64,
    /// Composition offset (PTS - DTS) in track timescale units. Video only.
    rendering_offset: i32,
    /// Marks an IDR / sync sample. SPS+PPS are prepended on these.
    is_sync: bool,
    /// Container-stripped sample bytes — AVCC for video, raw AAC AU for audio.
    bytes: Vec<u8>,
}

fn demux_file(path: &Path) -> Result<DemuxResult> {
    let f = std::fs::File::open(path).map_err(|e| anyhow!("open {}: {e}", path.display()))?;
    let size = f.metadata()?.len();
    let reader = std::io::BufReader::new(f);
    let mut mp4 = mp4::Mp4Reader::read_header(reader, size)
        .map_err(|e| anyhow!("mp4 header parse {}: {e}", path.display()))?;

    let mut video = None;
    let mut audio = None;

    // Snapshot track ids + types up-front so the borrow on `mp4` is free
    // when we read samples below.
    let mut order: Vec<(u32, TrackType, MediaType)> = Vec::new();
    for (id, track) in mp4.tracks().iter() {
        let tt = match track.track_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let mt = match track.media_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        order.push((*id, tt, mt));
    }

    for (id, tt, mt) in order {
        match (tt, mt) {
            (TrackType::Video, MediaType::H264) => {
                if video.is_some() {
                    continue;
                }
                video = Some(read_avc_track(&mut mp4, id)?);
            }
            (TrackType::Video, _) => {
                // HEVC / VP9 / etc — skip.
                tracing::warn!(
                    track_id = id,
                    media_type = %mt,
                    "media-player MP4: video track is not H.264, skipping"
                );
            }
            (TrackType::Audio, MediaType::AAC) => {
                if audio.is_some() {
                    continue;
                }
                audio = Some(read_aac_track(&mut mp4, id)?);
            }
            _ => {}
        }
    }

    if video.is_none() && audio.is_none() {
        return Err(anyhow!(
            "MP4 has no H.264 or AAC tracks — re-encode the file or use a .ts source instead"
        ));
    }

    Ok(DemuxResult { video, audio })
}

fn read_avc_track<R: std::io::Read + std::io::Seek>(
    mp4: &mut mp4::Mp4Reader<R>,
    id: u32,
) -> Result<TrackData> {
    let (timescale, sps, pps) = {
        let track = mp4
            .tracks()
            .get(&id)
            .ok_or_else(|| anyhow!("track {id} disappeared"))?;
        let timescale = track.timescale();
        let sps = track
            .sequence_parameter_set()
            .map_err(|e| anyhow!("avc track {id} SPS: {e}"))?
            .to_vec();
        let pps = track
            .picture_parameter_set()
            .map_err(|e| anyhow!("avc track {id} PPS: {e}"))?
            .to_vec();
        (timescale, sps, pps)
    };
    let count = mp4
        .sample_count(id)
        .map_err(|e| anyhow!("avc track {id} sample count: {e}"))?;
    let mut samples: Vec<DemuxedSample> = Vec::with_capacity(count as usize);
    for sid in 1..=count {
        if let Some(s) = mp4
            .read_sample(id, sid)
            .map_err(|e| anyhow!("avc sample {sid}: {e}"))?
        {
            samples.push(DemuxedSample {
                start_time: s.start_time,
                rendering_offset: s.rendering_offset,
                is_sync: s.is_sync,
                bytes: s.bytes.to_vec(),
            });
        }
    }
    Ok(TrackData {
        timescale,
        samples,
        extra: TrackExtra::Avc { sps, pps },
    })
}

fn read_aac_track<R: std::io::Read + std::io::Seek>(
    mp4: &mut mp4::Mp4Reader<R>,
    id: u32,
) -> Result<TrackData> {
    let (timescale, profile, sr, ch) = {
        let track = mp4
            .tracks()
            .get(&id)
            .ok_or_else(|| anyhow!("track {id} disappeared"))?;
        let timescale = track.timescale();
        let profile = track
            .audio_profile()
            .map_err(|e| anyhow!("aac track {id} profile: {e}"))?;
        let sr = track
            .sample_freq_index()
            .map_err(|e| anyhow!("aac track {id} freq index: {e}"))?;
        let ch = track
            .channel_config()
            .map_err(|e| anyhow!("aac track {id} channel config: {e}"))?;
        (timescale, profile, sr, ch)
    };
    let count = mp4
        .sample_count(id)
        .map_err(|e| anyhow!("aac track {id} sample count: {e}"))?;
    let mut samples: Vec<DemuxedSample> = Vec::with_capacity(count as usize);
    for sid in 1..=count {
        if let Some(s) = mp4
            .read_sample(id, sid)
            .map_err(|e| anyhow!("aac sample {sid}: {e}"))?
        {
            samples.push(DemuxedSample {
                start_time: s.start_time,
                rendering_offset: 0,
                is_sync: true,
                bytes: s.bytes.to_vec(),
            });
        }
    }
    let adts_profile = aot_to_adts_profile(profile);
    let sr_index = sr_index_to_u8(sr);
    let ch_config = ch as u8;
    Ok(TrackData {
        timescale,
        samples,
        extra: TrackExtra::Aac {
            adts_profile,
            sr_index,
            ch_config,
        },
    })
}

fn aot_to_adts_profile(aot: AudioObjectType) -> u8 {
    // ADTS profile field is the 0-indexed audio object type.
    // AAC-LC (AOT 2) → ADTS profile 1.
    let raw = aot as u8;
    raw.saturating_sub(1)
}

fn sr_index_to_u8(sr: SampleFreqIndex) -> u8 {
    sr as u8
}

async fn play_demuxed(d: DemuxResult, session: &mut PlayerSession<'_>) -> Result<()> {
    let mut ts_mux = TsMuxer::new();
    if d.video.is_some() {
        ts_mux.set_has_video(true);
        ts_mux.set_video_stream_type(STREAM_TYPE_H264);
    } else {
        ts_mux.set_has_video(false);
    }
    if d.audio.is_some() {
        ts_mux.set_has_audio(true);
        ts_mux.set_audio_stream(STREAM_TYPE_AAC, None);
    } else {
        ts_mux.set_has_audio(false);
    }

    // Pre-build SPS / PPS NALs in Annex-B form once — every IDR sample
    // emits with these prepended.
    let (sps_nal, pps_nal) = match d.video.as_ref().map(|t| &t.extra) {
        Some(TrackExtra::Avc { sps, pps }) => (
            annex_b_nal(sps),
            annex_b_nal(pps),
        ),
        _ => (Vec::new(), Vec::new()),
    };

    let video_timescale = d.video.as_ref().map(|t| t.timescale).unwrap_or(90_000);
    let audio_timescale = d.audio.as_ref().map(|t| t.timescale).unwrap_or(48_000);
    let aac_extra = d
        .audio
        .as_ref()
        .and_then(|t| match &t.extra {
            TrackExtra::Aac { adts_profile, sr_index, ch_config } => {
                Some((*adts_profile, *sr_index, *ch_config))
            }
            _ => None,
        });

    // Merge video + audio sample lists by emission wall-time. Each `Item`
    // carries a track tag so we know whether to mux as video or audio.
    let mut heap: BinaryHeap<Item> = BinaryHeap::new();
    if let Some(ref t) = d.video {
        for (i, s) in t.samples.iter().enumerate() {
            heap.push(Item {
                wall_us: ts_to_us(s.start_time, t.timescale),
                track: Track::Video,
                index: i,
            });
        }
    }
    if let Some(ref t) = d.audio {
        for (i, s) in t.samples.iter().enumerate() {
            heap.push(Item {
                wall_us: ts_to_us(s.start_time, t.timescale),
                track: Track::Audio,
                index: i,
            });
        }
    }

    let video_samples = d.video.as_ref().map(|t| &t.samples);
    let audio_samples = d.audio.as_ref().map(|t| &t.samples);

    let start_wall = Instant::now();
    let mut bundle = BytesMut::with_capacity(BUNDLE_SIZE);
    let mut out_rtp_ts: u32 = 0;

    while let Some(it) = heap.pop() {
        if session.cancel.is_cancelled() {
            break;
        }
        let target = start_wall + Duration::from_micros(it.wall_us);
        tokio::select! {
            _ = session.cancel.cancelled() => break,
            _ = tokio::time::sleep_until(target) => {}
        }

        let chunks: Vec<bytes::Bytes> = match it.track {
            Track::Video => {
                let s = &video_samples.unwrap()[it.index];
                let pts_90 = ts_to_90khz(
                    s.start_time as i64 + s.rendering_offset as i64,
                    video_timescale,
                );
                let dts_90 = ts_to_90khz(s.start_time as i64, video_timescale);
                let annex_b = avcc_to_annex_b(&s.bytes, &sps_nal, &pps_nal, s.is_sync);
                out_rtp_ts = pts_90 as u32;
                ts_mux.mux_video(&annex_b, pts_90, dts_90, s.is_sync)
            }
            Track::Audio => {
                let s = &audio_samples.unwrap()[it.index];
                let pts_90 = ts_to_90khz(s.start_time as i64, audio_timescale);
                let (profile, sr_idx, ch_cfg) =
                    aac_extra.expect("audio path implies aac_extra");
                let mut adts =
                    build_adts_header(profile, sr_idx, ch_cfg, s.bytes.len());
                adts.extend_from_slice(&s.bytes);
                out_rtp_ts = pts_90 as u32;
                ts_mux.mux_audio_pre_adts(&adts, pts_90)
            }
        };

        for ts_pkt in chunks {
            bundle.extend_from_slice(&ts_pkt);
            if bundle.len() >= BUNDLE_SIZE {
                emit_bundle(&mut bundle, session, out_rtp_ts);
            }
        }
    }

    if !bundle.is_empty() {
        emit_bundle(&mut bundle, session, out_rtp_ts);
    }
    Ok(())
}

#[derive(Eq, PartialEq)]
struct Item {
    wall_us: u64,
    track: Track,
    index: usize,
}

#[derive(Eq, PartialEq, Clone, Copy)]
enum Track {
    Video,
    Audio,
}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is max-heap; invert so smallest wall_us comes first.
        other.wall_us.cmp(&self.wall_us)
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn ts_to_us(t: u64, timescale: u32) -> u64 {
    if timescale == 0 {
        return 0;
    }
    t.saturating_mul(1_000_000) / timescale as u64
}

fn ts_to_90khz(t: i64, timescale: u32) -> u64 {
    if timescale == 0 {
        return 0;
    }
    let v = t.max(0) as u64;
    v.saturating_mul(90_000) / timescale as u64
}

/// Wrap a single NAL in 4-byte Annex-B start code.
fn annex_b_nal(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len() + 4);
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    out.extend_from_slice(nal);
    out
}

/// Convert an MP4 AVCC sample (length-prefixed NALs) to Annex-B form.
/// On sync samples, the SPS + PPS NALs are prepended once at the head of
/// the output buffer so receivers can decode standalone.
fn avcc_to_annex_b(avcc: &[u8], sps_nal: &[u8], pps_nal: &[u8], is_sync: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + sps_nal.len() + pps_nal.len() + 16);
    if is_sync {
        out.extend_from_slice(sps_nal);
        out.extend_from_slice(pps_nal);
    }
    let mut pos = 0;
    while pos + 4 <= avcc.len() {
        let len = u32::from_be_bytes([
            avcc[pos],
            avcc[pos + 1],
            avcc[pos + 2],
            avcc[pos + 3],
        ]) as usize;
        pos += 4;
        if pos + len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&avcc[pos..pos + len]);
        pos += len;
    }
    out
}

/// Build a 7-byte ADTS header (no CRC) for an AAC AU of `payload_len` bytes.
fn build_adts_header(profile: u8, sr_index: u8, ch_config: u8, payload_len: usize) -> Vec<u8> {
    let frame_len = (payload_len + 7) as u32;
    let mut h = vec![0u8; 7];
    h[0] = 0xFF;
    // sync 0xFFF + ID=0 (MPEG-4) + Layer=00 + protection_absent=1 = 0xF1.
    h[1] = 0xF1;
    h[2] = ((profile & 0x03) << 6) | ((sr_index & 0x0F) << 2) | ((ch_config >> 2) & 0x01);
    h[3] = ((ch_config & 0x03) << 6) | (((frame_len >> 11) as u8) & 0x03);
    h[4] = ((frame_len >> 3) & 0xFF) as u8;
    h[5] = (((frame_len & 0x07) as u8) << 5) | 0x1F;
    h[6] = 0xFC;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_to_annex_b_appends_start_codes_and_prepends_sps_pps_on_sync() {
        // 4-byte length prefix + 3-byte NAL of value 0xAB
        let sample = [0u8, 0, 0, 3, 0xAB, 0xCD, 0xEF];
        let sps_nal = annex_b_nal(&[0x67, 0x42]);
        let pps_nal = annex_b_nal(&[0x68, 0xCE]);
        let out = avcc_to_annex_b(&sample, &sps_nal, &pps_nal, true);
        assert!(out.starts_with(&[0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE]));
        assert!(out.ends_with(&[0, 0, 0, 1, 0xAB, 0xCD, 0xEF]));
    }

    #[test]
    fn adts_header_frame_length_round_trips() {
        // AAC-LC, 48 kHz, stereo, 100-byte payload → frame_length = 107.
        let h = build_adts_header(1, 3, 2, 100);
        let frame_len_field = (((h[3] & 0x03) as u32) << 11)
            | ((h[4] as u32) << 3)
            | ((h[5] as u32) >> 5);
        assert_eq!(frame_len_field, 107);
        // First two bytes are the sync word + ID/protection.
        assert_eq!(h[0], 0xFF);
        assert_eq!(h[1], 0xF1);
    }

    #[test]
    fn adts_channel_config_split_across_byte_boundary() {
        // ch_config = 7 (7.1 surround) → bit 2 in byte[2], bits 0-1 in byte[3].
        let h = build_adts_header(1, 3, 7, 50);
        let recovered = ((h[2] & 0x01) << 2) | ((h[3] >> 6) & 0x03);
        assert_eq!(recovered, 7);
    }
}
