// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TS → fragmented-MP4 remuxer for the Recordings library export path.
//!
//! Operators downloading a clip for review through QuickTime / VLC /
//! editing tooling want a `.mp4` file, not a `.ts`. The recording
//! engine stores everything as MPEG-TS on disk, so the export path
//! re-uses [`crate::engine::ts_demux::TsDemuxer`] to recover access
//! units + AAC frames and the `engine::cmaf::fmp4` builders to wrap
//! them in a single `ftyp + moov + moof + mdat` fragmented-MP4 stream.
//!
//! ## Scope
//!
//! - **Video**: H.264 (`avc1`) and HEVC (`hvc1`) — both packaged via the
//!   existing CMAF box writer (`engine::cmaf::fmp4::VideoTrack::from_h264` /
//!   `from_h265`). MPEG-1 / MPEG-2 video has no first-class MP4 mapping
//!   wired today and falls back to `replay_export_format_unsupported`.
//! - **Audio**: AAC (`mp4a` + `esds`), AC-3 (`ac-3` + `dac3`), E-AC-3
//!   (`ec-3` + `dec3`), MP2 (`mp4a` + `esds` with MPEG-1-Audio OTI).
//!   Opus surfaces `replay_export_format_unsupported` because
//!   `engine::ts_demux` doesn't yet extract Opus PES payload.
//! - **PTS == DTS assumption.** ts_demux only recovers PTS today.
//!   B-frame streams (most main / high profile broadcast) may decode
//!   in incorrect display order in some players. The follow-up to
//!   recover DTS through PES parsing is tracked separately.
//! - **One-shot build, in-memory cache.** Exports cap at 256 MiB to
//!   keep the cache footprint bounded; over-cap clips fail with
//!   `replay_export_too_large` and the operator should download TS.
//! - **Cache TTL** is 5 minutes — long enough for the manager's
//!   chunk-by-chunk pulls to drain a 256 MiB clip on a slow link,
//!   short enough that an operator who walks away doesn't pin RAM.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::engine::cmaf::codecs::{
    Ac3FrameInfo, Mp2FrameInfo, aac_audio_specific_config, build_dac3_payload, build_dec3_payload,
    parse_ac3_syncframe, parse_eac3_syncframe, parse_mp2_frame, sample_rate_from_index,
};
use crate::engine::cmaf::fmp4::{
    AudioCodec, AudioTrack, Sample, VideoCodec as CmafVideoCodec, VideoTrack,
    build_progressive_mp4, video_sample_from_nalus,
};
use crate::engine::cmaf::nalu::{filter_frame_nalus_h264, filter_frame_nalus_h265};
use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};

/// MPEG-TS stream_type values relevant to the export format selection.
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_H265: u8 = 0x24;
/// MPEG-TS stream_type values for non-AAC audio codecs surfaced via
/// `DemuxedFrame::OtherAudio`. Match what `engine::ts_demux` recognises.
const STREAM_TYPE_MP2_A: u8 = 0x03; // ISO/IEC 11172-3 (MPEG-1 Audio)
const STREAM_TYPE_MP2_B: u8 = 0x04; // ISO/IEC 13818-3 (MPEG-2 Audio)
const STREAM_TYPE_AC3_A: u8 = 0x80;
const STREAM_TYPE_AC3_B: u8 = 0x81;
const STREAM_TYPE_AC3_C: u8 = 0xC1;
const STREAM_TYPE_EAC3_A: u8 = 0x87;
const STREAM_TYPE_EAC3_B: u8 = 0xC2;

use super::clips::ClipStore;
use super::export::{ExportChunk, MAX_CHUNK_BYTES};
use super::index::InMemoryIndex;

/// Per-export hard cap, applied to the **built file** and not only to the
/// source range it was built from.
///
/// It used to bound `plan.total_bytes()`, the TS byte range read off disk, on
/// the reasoning that a remux is roughly the size of its input. The all-intra
/// re-encode broke that: output size is now a function of resolution and CRF,
/// so a source range comfortably inside the cap can produce a file several
/// times it. Both ends are checked now — the read, so a huge read is refused
/// before it happens, and the essence the muxer is about to interleave, so the
/// cap bounds what is actually held and handed on.
const MP4_EXPORT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Cache TTL after the last access. Operators draining a 200 MiB clip
/// on a slow link finish well within 5 min; an operator who walks
/// away doesn't pin RAM forever.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Ceiling on everything the cache holds at once.
///
/// The TTL alone bounded nothing. It was written for one operator draining one
/// download, where a single entry expires on its own; it is not a bound when
/// entries arrive faster than they expire, and `evict_expired` runs only from
/// inside a lookup or an insert — so the last entry of a burst is pinned until
/// somebody exports again, which on a node that stops cutting is for ever.
///
/// Sized at two maximum exports: enough that a caller re-reading the chunk it
/// just read still hits the cache, small enough that the resident set cannot be
/// walked up simply by asking for more clips.
const CACHE_MAX_BYTES: usize = 2 * MP4_EXPORT_MAX_BYTES;

/// And a count, so many small exports cannot do by number what the byte
/// ceiling stops them doing by size.
const CACHE_MAX_ENTRIES: usize = 8;

/// How many exports may be built at once, process-wide.
///
/// A cut is a full decode plus a libx264 all-intra encode of the whole window —
/// tens of seconds of CPU with auto thread counts, so one already contends for
/// every core. There is one clip poller per passthrough CMAF output and no cap
/// on DVR sessions per node, and the operator export commands reach the same
/// builder, so without this N sessions clipping in the same five-second window
/// run N of those at once on a node that is also carrying live transport.
///
/// One permit, not a pool: a clip is a deliberate act with a human waiting, so
/// seconds of queueing are invisible — and the thing being protected, wire
/// pacing on the live flows, is not.
fn build_slot() -> &'static tokio::sync::Semaphore {
    static SLOT: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SLOT.get_or_init(|| tokio::sync::Semaphore::new(1))
}

#[derive(Clone)]
struct CachedMp4 {
    bytes: std::sync::Arc<Vec<u8>>,
    last_access: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, CachedMp4>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedMp4>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn evict_expired(map: &mut HashMap<String, CachedMp4>) {
    let now = Instant::now();
    map.retain(|_, v| now.duration_since(v.last_access) < CACHE_TTL);
}

/// Drop least-recently-used entries until the map is inside both ceilings.
fn evict_to_budget(map: &mut HashMap<String, CachedMp4>) {
    fn over(m: &HashMap<String, CachedMp4>) -> bool {
        m.len() > CACHE_MAX_ENTRIES
            || m.values().map(|v| v.bytes.len()).sum::<usize>() > CACHE_MAX_BYTES
    }
    while over(map) {
        let Some(oldest) = map
            .iter()
            .min_by_key(|(_, v)| v.last_access)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        map.remove(&oldest);
    }
}

fn lookup_cached(key: &str) -> Option<std::sync::Arc<Vec<u8>>> {
    let mut map = cache().lock().ok()?;
    evict_expired(&mut map);
    let entry = map.get_mut(key)?;
    entry.last_access = Instant::now();
    Some(entry.bytes.clone())
}

fn insert_cached(key: String, bytes: std::sync::Arc<Vec<u8>>) {
    if let Ok(mut map) = cache().lock() {
        evict_expired(&mut map);
        map.insert(
            key,
            CachedMp4 {
                bytes,
                last_access: Instant::now(),
            },
        );
        evict_to_budget(&mut map);
    }
}

/// Build (or reuse a cached) fMP4 byte buffer for the given clip and
/// return the chunk starting at `byte_offset`.
pub async fn export_clip_mp4_chunk(
    recording_id: &str,
    clip_id: &str,
    byte_offset: u64,
    max_bytes: u64,
) -> Result<ExportChunk> {
    let cache_key = format!("clip:{recording_id}:{clip_id}");
    let bytes = if let Some(b) = lookup_cached(&cache_key) {
        b
    } else {
        let b = build_clip_mp4(recording_id, clip_id).await?;
        let arc = std::sync::Arc::new(b);
        insert_cached(cache_key, arc.clone());
        arc
    };
    Ok(slice_export_chunk(&bytes, byte_offset, max_bytes))
}

/// Same as [`export_clip_mp4_chunk`] but for a whole-recording PTS
/// range. Whole-recording MP4 exports inherit the same 256 MiB cap.
pub async fn export_recording_mp4_chunk(
    recording_id: &str,
    from_pts_90khz: Option<u64>,
    to_pts_90khz: Option<u64>,
    byte_offset: u64,
    max_bytes: u64,
) -> Result<ExportChunk> {
    if let (Some(from), Some(to)) = (from_pts_90khz, to_pts_90khz)
        && to <= from {
            bail!("replay_invalid_range");
        }
    let cache_key = format!(
        "rec:{recording_id}:{}:{}",
        from_pts_90khz.unwrap_or(0),
        to_pts_90khz.unwrap_or(u64::MAX),
    );
    let bytes = if let Some(b) = lookup_cached(&cache_key) {
        b
    } else {
        let b = build_recording_mp4(recording_id, from_pts_90khz, to_pts_90khz).await?;
        let arc = std::sync::Arc::new(b);
        insert_cached(cache_key, arc.clone());
        arc
    };
    Ok(slice_export_chunk(&bytes, byte_offset, max_bytes))
}

/// Build one whole MP4 and hand it over, without touching the cache.
///
/// For a caller that consumes the bytes once, in order, immediately — the DVR
/// clip exporter, which uploads them and drops them. Through the chunked API
/// that caller paid twice: the finished file was `Arc`-pinned in the cache for
/// five minutes, while the loop copied the same bytes out slice by slice into a
/// second full-size `Vec` to PUT. Two live copies of every clip at the moment
/// of upload, and the first of them unreachable but unfreeable — which is why
/// the `malloc_trim` after each cut could not reclaim it.
#[cfg(feature = "replay")]
pub async fn export_recording_mp4_whole(
    recording_id: &str,
    from_pts_90khz: Option<u64>,
    to_pts_90khz: Option<u64>,
) -> Result<Vec<u8>> {
    if let (Some(from), Some(to)) = (from_pts_90khz, to_pts_90khz)
        && to <= from
    {
        bail!("replay_invalid_range");
    }
    build_recording_mp4(recording_id, from_pts_90khz, to_pts_90khz).await
}

fn slice_export_chunk(bytes: &[u8], byte_offset: u64, max_bytes: u64) -> ExportChunk {
    let total = bytes.len() as u64;
    if byte_offset >= total {
        return ExportChunk {
            data: Vec::new(),
            total_bytes: total,
            byte_offset,
            eof: true,
        };
    }
    let want = if max_bytes == 0 {
        super::export::DEFAULT_CHUNK_BYTES
    } else {
        max_bytes.min(MAX_CHUNK_BYTES)
    };
    let take = want.min(total - byte_offset) as usize;
    let start = byte_offset as usize;
    let data = bytes[start..start + take].to_vec();
    ExportChunk {
        data,
        total_bytes: total,
        byte_offset,
        eof: byte_offset + take as u64 >= total,
    }
}

async fn build_clip_mp4(recording_id: &str, clip_id: &str) -> Result<Vec<u8>> {
    let dir = super::recording_dir(recording_id);
    let store = ClipStore::open(recording_id, &dir).await?;
    let clip = store
        .get(clip_id)
        .await
        .ok_or_else(|| anyhow!("replay_clip_not_found"))?;
    if clip.out_pts_90khz <= clip.in_pts_90khz {
        bail!("replay_invalid_range");
    }
    build_recording_mp4(recording_id, Some(clip.in_pts_90khz), Some(clip.out_pts_90khz)).await
}

async fn build_recording_mp4(
    recording_id: &str,
    from_pts_90khz: Option<u64>,
    to_pts_90khz: Option<u64>,
) -> Result<Vec<u8>> {
    let dir = super::recording_dir(recording_id);
    let index = InMemoryIndex::load(&dir.join("index.bin"))
        .await
        .context("load index for mp4 export")?;
    let from_pts = from_pts_90khz
        .or_else(|| index.entries.first().map(|e| e.pts_90khz))
        .ok_or_else(|| anyhow!("replay_no_index"))?;

    // A range that crosses a recorder restart cannot be exported at all.
    //
    // Checked here rather than only in the clip exporter, so every consumer of
    // the builder inherits it. The index's own timeline is continuous across a
    // restart — the writer resumes its counter — but the media underneath is
    // not: the PCR in the TS begins again with the process. Muxing across the
    // join yields a file that plays and lies about its own length, measured at
    // 70,567 seconds from a thirty-second request, which is discovered in an
    // edit suite rather than here.
    if index.spans_discontinuity(from_pts, to_pts_90khz.unwrap_or(u64::MAX)) {
        bail!("replay_export_spans_restart");
    }

    // Pull the whole TS byte range into memory first. Refused up front past the
    // cap so a huge read never happens; the built file is checked again below,
    // because the re-encode makes output size independent of input size.
    let plan = super::export::plan_for_pts_range_pub(&index, &dir, from_pts, to_pts_90khz).await?;
    if plan.total_bytes() > MP4_EXPORT_MAX_BYTES as u64 {
        bail!("replay_export_too_large");
    }
    let ts = super::export::read_full_range(&dir, plan).await?;
    drop(index);

    // Everything from here is CPU: a software decode of every frame, a libx264
    // all-intra encode of every frame, and an interleaving mux — seconds to
    // minutes of it, with no await in the middle.
    //
    // Running that inline on a runtime worker is what the house rule against
    // blocking the data path forbids, and the rest of this subsystem already
    // knows it: the live CMAF re-encoder goes through `timed_block_in_place!`
    // and the filmstrip writer through `spawn_blocking`. This was the one
    // encoder call site that did neither, and the longest-running one.
    //
    // The permit is taken around the same section rather than around the whole
    // function so the queue forms before the CPU is committed, not after.
    let _permit = build_slot()
        .acquire()
        .await
        .map_err(|_| anyhow!("replay_export_failed: the export slot closed"))?;
    tokio::task::spawn_blocking(move || build_mp4_from_ts(&ts))
        .await
        .map_err(|e| anyhow!("replay_export_failed: {e}"))?
}

/// The CPU half of an export: demux, re-encode, mux. No IO, no await.
fn build_mp4_from_ts(ts: &[u8]) -> Result<Vec<u8>> {
    // Demux + collect frames. We keep the whole list in memory because
    // we only know each sample's duration after seeing the next
    // sample's PTS. For the 256 MiB cap this is bounded and small.
    let mut demux = TsDemuxer::new(None);
    let frames = demux.demux(ts);

    // Per-frame video tuple: (pts, nalus, is_keyframe). Codec is implied
    // by `demux.video_stream_type()` after demux completes.
    let mut video_pts: Vec<(u64, Vec<Vec<u8>>, bool)> = Vec::new();
    // AAC frames arrive pre-split, one (pts, raw_aac) per frame.
    let mut aac_pts: Vec<(u64, Vec<u8>)> = Vec::new();
    // Non-AAC audio arrives as a single PES blob per emit. Each blob
    // typically contains 1–2 codec frames concatenated; we split on the
    // codec sync word and assign per-frame PTS by extrapolating from the
    // PES PTS using the parsed frame's sample rate + frame size.
    let mut other_audio: Vec<(u64, u8, Vec<u8>)> = Vec::new();
    for f in frames {
        match f {
            DemuxedFrame::H264 { nalus, pts, is_keyframe } => {
                video_pts.push((pts, nalus, is_keyframe));
            }
            DemuxedFrame::H265 { nalus, pts, is_keyframe } => {
                video_pts.push((pts, nalus, is_keyframe));
            }
            DemuxedFrame::Aac { data, pts } => {
                aac_pts.push((pts, data));
            }
            DemuxedFrame::OtherAudio { stream_type, data, pts } => {
                other_audio.push((pts, stream_type, data));
            }
            DemuxedFrame::Mpeg2 { .. } => {
                // No fragmented-MP4 mapping wired today — operators with
                // MPEG-2 sources should download TS.
                bail!("replay_export_format_unsupported");
            }
            DemuxedFrame::Opus { .. } => {
                // Opus → MP4 carriage (`Opus`-in-`fmp4` boxes) is not
                // wired today. Display playback decodes Opus through
                // libopus directly; export-to-MP4 is the missing path.
                bail!("replay_export_format_unsupported");
            }
            // Stream discontinuity is metadata for stateful decoders; the
            // export pipeline pulls samples in PTS order regardless.
            DemuxedFrame::Discontinuity | DemuxedFrame::Scte35(_) => {}
        }
    }
    if video_pts.is_empty() {
        bail!("replay_no_video_frames");
    }

    // Build the right `VideoTrack` flavour from the cached parameter
    // sets. `demux.video_stream_type()` is the authoritative codec
    // identifier — it reflects the PMT, not the heuristics.
    let video_track = match demux.video_stream_type() {
        STREAM_TYPE_H264 => {
            let sps = demux
                .cached_sps()
                .ok_or_else(|| anyhow!("replay_export_failed: missing H.264 SPS"))?
                .to_vec();
            let pps = demux
                .cached_pps()
                .ok_or_else(|| anyhow!("replay_export_failed: missing H.264 PPS"))?
                .to_vec();
            VideoTrack::from_h264(sps, pps)
        }
        STREAM_TYPE_H265 => {
            let vps = demux
                .cached_h265_vps()
                .ok_or_else(|| anyhow!("replay_export_failed: missing HEVC VPS"))?
                .to_vec();
            let sps = demux
                .cached_h265_sps()
                .ok_or_else(|| anyhow!("replay_export_failed: missing HEVC SPS"))?
                .to_vec();
            let pps = demux
                .cached_h265_pps()
                .ok_or_else(|| anyhow!("replay_export_failed: missing HEVC PPS"))?
                .to_vec();
            VideoTrack::from_h265(vps, sps, pps)
        }
        _ => {
            bail!("replay_export_format_unsupported");
        }
    };

    // Build the audio track + per-sample list. AAC takes the
    // already-split frames; AC-3 / E-AC-3 / MP2 split each PES blob on
    // the codec's sync word and assign per-frame PTS from frame size.
    let (audio_track, audio_samples_built) = if !aac_pts.is_empty() {
        let (profile, sr_idx, ch_cfg) = demux
            .cached_aac_config()
            .ok_or_else(|| anyhow!("replay_export_failed: missing AAC config"))?;
        let asc = aac_audio_specific_config(profile, sr_idx, ch_cfg);
        let sample_rate = sample_rate_from_index(sr_idx);
        let track = AudioTrack::aac(asc, sample_rate, ch_cfg.max(1) as u16, 128_000);
        // AAC frame duration is always 1024 samples (LC) at the
        // configured sample rate, expressed in the audio track's
        // own timescale (== sample_rate).
        let samples: Vec<Sample> = aac_pts
            .iter()
            .map(|(_pts, data)| Sample {
                duration: 1024,
                data: data.clone(),
                composition_time_offset: 0,
                is_sync: true,
            })
            .collect();
        (Some(track), samples)
    } else if !other_audio.is_empty() {
        let stream_type = other_audio[0].1;
        match stream_type {
            STREAM_TYPE_AC3_A | STREAM_TYPE_AC3_B | STREAM_TYPE_AC3_C => {
                build_ac3_track(&other_audio, /*eac3=*/ false)?
            }
            STREAM_TYPE_EAC3_A | STREAM_TYPE_EAC3_B => {
                build_ac3_track(&other_audio, /*eac3=*/ true)?
            }
            STREAM_TYPE_MP2_A | STREAM_TYPE_MP2_B => build_mp2_track(&other_audio)?,
            _ => bail!("replay_export_format_unsupported"),
        }
    } else {
        (None, Vec::new())
    };

    // Sort video PTS to defend against minor packet reordering at the
    // PES boundary; demux already returns frames in order in practice.
    video_pts.sort_by_key(|t| t.0);

    // ── Re-encode all-intra ────────────────────────────────────────────────
    //
    // The source is long-GOP: one IDR every ~49 frames, the rest differences
    // from what came before. That is right for transport and wrong for the
    // job a clip is for. Stepping backward through it means decoding from the
    // keyframe every time, which VLC does badly enough to look like the
    // picture is breaking up — and no container fixes it, because it is a
    // property of the codec settings, not the file.
    //
    // So every frame becomes an IDR. The clip costs about three times the
    // bitrate and a few seconds of x264, and in exchange it steps exactly,
    // forward and back, in anything that opens it.
    //
    // Audio is left alone: AAC frames are already independently decodable.
    let reencoded = reencode_all_intra(&video_pts, &video_track, &demux);
    let (video_pts, video_track, reencode_trimmed_ticks) = match reencoded {
        Ok((frames, track, trimmed)) => {
            // Shadowing does not drop. The source list is a whole clip's worth
            // of NAL payloads and the re-encoded one is larger again, so
            // letting the old binding live to the end of the function held two
            // full copies of the essence for no reason.
            drop(video_pts);
            (frames, track, trimmed)
        }
        Err(e) => {
            // A clip that exists beats no clip. The operator gets the source
            // GOP structure and the stepping that comes with it, and the
            // reason is in the log rather than in a failed export.
            tracing::warn!(
                error = %e,
                "replay export: could not re-encode all-intra;                  falling back to the source's own GOP structure"
            );
            (video_pts, video_track, 0)
        }
    };

    // Build samples — codec-specific NAL filter strips parameter sets
    // (and AUD on HEVC) out of the per-frame payload, since they live
    // in the sample entry's `avcC` / `hvcC` extradata in the init
    // segment instead.
    //
    // Selected by what the TRACK carries, not by what the source did. The
    // re-encoder emits H.264 whatever went in, so deriving this from the
    // demuxer ran an HEVC source's re-encoded H.264 through the HEVC filter,
    // whose `(byte0 >> 1) & 0x3F` test reads an H.264 SPS as type 51 and
    // strips nothing — leaving the parameter sets in-band in every sample of
    // an `avc1`/`avcC` track, which is what `avc3` exists for and what strict
    // players refuse. The live CMAF path already picks by the encoder's family.
    let is_h265 = matches!(video_track.codec, CmafVideoCodec::H265);
    let mut video_samples: Vec<Sample> = Vec::with_capacity(video_pts.len());
    for (i, (pts, nalus, is_sync)) in video_pts.iter().enumerate() {
        let next_pts = video_pts.get(i + 1).map(|t| t.0).unwrap_or(*pts + 3000);
        let duration = (next_pts.saturating_sub(*pts)) as u32;
        let frame_nalus = if is_h265 {
            filter_frame_nalus_h265(nalus)
        } else {
            filter_frame_nalus_h264(nalus)
        };
        if frame_nalus.is_empty() {
            continue;
        }
        let data = video_sample_from_nalus(&frame_nalus);
        video_samples.push(Sample {
            duration: duration.max(1),
            data,
            composition_time_offset: 0,
            is_sync: *is_sync,
        });
    }
    if video_samples.is_empty() {
        bail!("replay_no_video_frames");
    }
    // Anchor video DTS at 0 so the timeline reads "0:00" in players.
    let _video_pts_base = video_pts.first().map(|t| t.0).unwrap_or(0);

    let audio_samples = audio_samples_built;

    // ── Open on a keyframe ─────────────────────────────────────────────────
    //
    // The byte range starts on an IDR — that is what the index entry is — but
    // the demux ahead of this can still yield samples before the first one it
    // marks as sync. Handing those to a player is what made an export open
    // pixelated: there is no keyframe to decode against until the next real
    // IDR arrives, about a GOP and a half in.
    //
    // Measured on the rig: a 30s clip carried 240 samples of which the leading
    // 41 decoded to nothing at all, and the first sync sample sat at 41.
    //
    // The re-encoder does the same trim on the way in, for the same reason —
    // it cannot decode those frames either — and what it discards has to be
    // counted here too. It marks every frame it emits as sync, so `first_sync`
    // is 0 on the re-encoded path and the sum below is over an empty slice: the
    // audio drain was therefore skipped on exactly the path every release
    // artefact takes, and every clip shipped with the audio leading the video
    // by the length of the discarded lead-in. Tens of times gate 3's ±40 ms.
    let first_sync = video_samples.iter().position(|s| s.is_sync).unwrap_or(0);
    let dropped_ticks = leading_trim_ticks(reencode_trimmed_ticks, &video_samples, first_sync);
    if first_sync > 0 {
        tracing::debug!(
            dropped = first_sync, ticks = dropped_ticks,
            "replay export: trimming to the first keyframe"
        );
        video_samples.drain(..first_sync);
    }

    // Audio follows the video by time, not by sample count: the two tracks
    // start together at decode time zero, so dropping the same *duration*
    // keeps them in step. Rounded down, so audio never starts late.
    let mut audio_samples = audio_samples;
    if dropped_ticks > 0 && !audio_samples.is_empty() {
        let audio_ts = audio_track.as_ref().map(|a| a.sample_rate as u64).unwrap_or(0);
        let drop_n = audio_samples_to_drop(
            dropped_ticks,
            video_track.timescale,
            audio_ts,
            &audio_samples,
        );
        audio_samples.drain(..drop_n);
    }

    // ── A progressive file, not a fragmented one ───────────────────────────
    //
    // An exported clip is downloaded and opened, not streamed against a
    // manifest, so it is written the way every tool expects a file to be: one
    // `moov` carrying real sample tables, then one `mdat`.
    //
    // Fragments were the first answer here and they were the wrong shape. A
    // fragmented file without `sidx` or `mfra` gives a player nothing to seek
    // by, so VLC either would not scrub or walked the whole file to do it —
    // and one fragment per GOP, while better structured, does not fix that
    // because the index still is not there. `stss` is what a player actually
    // builds a seek from, and only a progressive file has one.
    //
    // `moov` is written ahead of `mdat` so the tables are readable without
    // fetching the whole clip first.
    //
    // Checked here, on the essence about to be interleaved, because this is the
    // last point at which refusing is cheap: `build_progressive_mp4` lays the
    // whole payload out and then copies it into the finished buffer, so past
    // this line an over-cap export costs two more full copies before anyone can
    // say no. The cap on the source range upstream does not cover it — the
    // re-encode decoupled output size from input size.
    //
    // It is also what keeps the 32-bit `stco` offsets and the 32-bit `mdat`
    // size honest: at 256 MiB the file cannot approach the 4 GiB boundary where
    // those would silently truncate into a file that parses and decodes
    // garbage.
    let payload_bytes: usize = video_samples.iter().map(|s| s.data.len()).sum::<usize>()
        + audio_samples.iter().map(|s| s.data.len()).sum::<usize>();
    if payload_bytes > MP4_EXPORT_MAX_BYTES {
        bail!("replay_export_too_large");
    }

    let buf = build_progressive_mp4(
        &video_track,
        &video_samples,
        audio_track.as_ref(),
        &audio_samples,
    );
    Ok(buf)
}

/// How much of the front of the clip the video lost, in video ticks.
///
/// Two trims, one number. The muxer drops whatever precedes the first sync
/// sample, and the all-intra re-encoder has already dropped the same lead-in
/// on its own way in — for the same reason, because neither can decode it.
///
/// Counting only the first is what made every clip on a release artefact lose
/// lip sync: the re-encoder marks every frame it emits as sync, so `first_sync`
/// is 0, the slice below is empty, and the audio drain that keeps the two
/// tracks starting together never ran. Both tracks are written anchored at
/// decode time zero with no edit list, so the audio simply led the picture by
/// the length of the discarded lead-in.
fn leading_trim_ticks(reencode_trimmed: u64, video_samples: &[Sample], first_sync: usize) -> u64 {
    reencode_trimmed
        + video_samples[..first_sync.min(video_samples.len())]
            .iter()
            .map(|s| s.duration as u64)
            .sum::<u64>()
}

/// How many leading audio samples to drop so the two tracks still start
/// together after the video lost `dropped_ticks` off its front.
///
/// Audio follows the video by *time*, not by sample count, and rounds down so
/// the audio never starts late. Returns 0 when there is no audio timescale to
/// convert through, which is the "no audio track" case.
fn audio_samples_to_drop(
    dropped_ticks: u64,
    video_timescale: u32,
    audio_rate: u64,
    audio: &[Sample],
) -> usize {
    if audio_rate == 0 || video_timescale == 0 {
        return 0;
    }
    let want = dropped_ticks * audio_rate / video_timescale as u64;
    let mut acc = 0u64;
    let mut drop_n = 0usize;
    for sm in audio {
        if acc + sm.duration as u64 > want {
            break;
        }
        acc += sm.duration as u64;
        drop_n += 1;
    }
    drop_n
}

/// The typical gap between one frame and the next, in 90 kHz ticks.
///
/// A median rather than a mean: the source drops frames when the link is
/// lossy, and one long gap must not stretch the cadence used to time the
/// frames the encoder was still holding at the end.
fn median_step(frames: &[(u64, Vec<Vec<u8>>, bool)]) -> Option<u64> {
    if frames.len() < 2 {
        return None;
    }
    let mut gaps: Vec<u64> = frames.windows(2).map(|w| w[1].0.saturating_sub(w[0].0)).collect();
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2]).filter(|g| *g > 0)
}

/// Re-encode a cut so every frame is an IDR.
///
/// Returns the new frame list, a track description built from the **encoder's**
/// parameter sets — not the source's: the SPS/PPS change with the encode, and
/// an `avcC` carrying the old ones describes a stream that no longer exists —
/// and **how many source ticks were discarded off the front**.
///
/// That last value is not bookkeeping. This function trims the undecodable
/// lead-in before the first keyframe and then marks every frame it emits as
/// sync, so the caller's own "open on a keyframe" trim finds nothing left to do
/// and its compensating audio drain never runs. Without the number coming back
/// out, the audio kept every frame from the start of the byte range while the
/// video started a GOP later, and both tracks were written anchored at decode
/// time zero with no edit list — audio ahead of picture by the length of the
/// trim, on every clip, on every build that has an encoder.
///
/// Errors rather than panicking on anything missing, because the caller's
/// answer to a failed re-encode is to ship the source GOP instead — a clip
/// that steps poorly is worth more than no clip.
#[cfg(feature = "media-codecs")]
fn reencode_all_intra(
    frames: &[(u64, Vec<Vec<u8>>, bool)],
    source: &VideoTrack,
    demux: &TsDemuxer,
) -> Result<(Vec<(u64, Vec<Vec<u8>>, bool)>, VideoTrack, u64)> {
    use crate::config::models::VideoEncodeConfig;
    use crate::engine::cmaf::encode::VideoReencoder;

    if frames.len() < 2 {
        bail!("nothing to re-encode");
    }

    // Which NAL semantics the *source* frames carry. The encoder needs this
    // to decode them; what it emits is H.264 either way.
    let src_codec = match demux.video_stream_type() {
        STREAM_TYPE_H265 => CmafVideoCodec::H265,
        _ => CmafVideoCodec::H264,
    };
    // Start at the first real IDR.
    //
    // The byte range opens on one, but the demux ahead of this still yields
    // frames before the first it marks as sync — the tail of the previous GOP,
    // reassembled from a PES that began before the cut. They are not
    // decodable, and feeding them to the decoder is what made the first
    // version of this fail on packet one and fall back every single time.
    let start = frames.iter().position(|f| f.2).unwrap_or(0);
    // Measured before the slice, and handed back, so the caller can drain the
    // same amount of audio. See this function's doc comment.
    let trimmed_ticks = if start > 0 {
        frames[start].0.saturating_sub(frames[0].0)
    } else {
        0
    };
    let frames = &frames[start..];
    if frames.len() < 2 {
        bail!("no keyframe to start from");
    }

    // Measured on the TRIMMED list, and that matters: taking the span across
    // the frames that were about to be dropped and then applying it to the
    // ones that were kept stretched every clip by the share the trim removed
    // — 31.57s for a 30s request.
    let span = frames.last().unwrap().0.saturating_sub(frames[0].0);
    let fps_num = if span > 0 {
        (((frames.len() - 1) as u64 * 90_000 * 1000) / span) as u32
    } else {
        25_000
    };

    let cfg = VideoEncodeConfig {
        // libx264 specifically, not `h264_auto`: a hardware encoder is tuned
        // for streaming and several will not honour a one-frame GOP at all,
        // which would quietly give back the long-GOP clip this exists to
        // avoid. A clip is short and this runs once, so the CPU cost is
        // affordable where it would not be on a live output.
        codec: "x264".to_string(),
        source_video_pid: None,
        width: Some(source.width),
        height: Some(source.height),
        fps_num: Some(fps_num),
        fps_den: Some(1000),
        bitrate_kbps: None,
        // Every frame a keyframe. This is the whole point.
        gop_size: Some(1),
        preset: Some("veryfast".to_string()),
        profile: Some("high".to_string()),
        chroma: None,
        bit_depth: None,
        // All-intra costs roughly three times the bitrate for the same look,
        // and a clip is short, so this is a quality decision rather than a
        // bandwidth one. CRF keeps it honest across easy and hard material
        // instead of spending a fixed bitrate on both.
        rate_control: Some("crf".to_string()),
        crf: Some(20),
        max_bitrate_kbps: None,
        bframes: Some(0),
        refs: Some(1),
        level: None,
        tune: None,
        color_primaries: None,
        color_transfer: None,
        color_matrix: None,
        color_range: None,
        hw_decode: None,
    };

    let mut enc = VideoReencoder::new(&cfg, "clip-export")?;
    let mut out: Vec<(u64, Vec<Vec<u8>>, bool)> = Vec::with_capacity(frames.len());
    let mut rejected = 0usize;
    for (pts, nalus, is_key) in frames {
        match enc.encode_frame(nalus, *pts, *is_key, src_codec) {
            Ok(Some(f)) => out.push((*pts, f.nalus, true)),
            // Buffered. The encoder will hand it back later, or on flush.
            Ok(None) => {}
            // One bad frame is not worth losing the clip over — a dropped
            // frame is a blink, an abandoned re-encode is a clip that cannot
            // be stepped through at all. Counted so it is not silent.
            Err(_) => rejected += 1,
        }
    }
    if rejected > 0 {
        tracing::debug!(rejected, of = frames.len(), "replay export: frames the decoder refused");
    }
    // Anything the encoder is still holding. Without this the tail of every
    // clip is missing, by however deep the encoder buffers.
    //
    // These come back with no timestamp of their own, so they continue the
    // cadence of the frames that did. Everything else keeps the PTS it was
    // demuxed with — re-stamping the lot on an even step was wrong: the
    // source drops frames when the link is lossy, so an even step spread 749
    // frames across the 31.4s they really covered and every clip came out
    // longer than it was asked for.
    let step = median_step(&out).unwrap_or(3600);
    let mut next = out.last().map(|f| f.0 + step).unwrap_or(0);
    for f in enc.flush()? {
        out.push((next, f.nalus, true));
        next += step;
    }
    if out.is_empty() {
        bail!("the encoder produced no frames");
    }

    // The encoder was built without a global header, so SPS and PPS ride
    // in-band on every IDR — and every frame is one now. Take them from the
    // first output frame; the sample builder strips them from the payload,
    // because in MP4 they belong in `avcC` rather than in the samples.
    let mut sps = None;
    let mut pps = None;
    for n in &out[0].1 {
        match n.first().map(|b| b & 0x1F) {
            Some(7) if sps.is_none() => sps = Some(n.clone()),
            Some(8) if pps.is_none() => pps = Some(n.clone()),
            _ => {}
        }
    }
    let (Some(sps), Some(pps)) = (sps, pps) else {
        bail!("the encoder's first frame carried no SPS/PPS");
    };
    let track = VideoTrack::from_h264(sps, pps);
    tracing::info!(
        frames = out.len(), width = track.width, height = track.height, trimmed_ticks,
        "replay export: re-encoded all-intra"
    );
    Ok((out, track, trimmed_ticks))
}

#[cfg(not(feature = "media-codecs"))]
fn reencode_all_intra(
    _frames: &[(u64, Vec<Vec<u8>>, bool)],
    _source: &VideoTrack,
    _demux: &TsDemuxer,
) -> Result<(Vec<(u64, Vec<Vec<u8>>, bool)>, VideoTrack, u64)> {
    bail!("this build has no encoder")
}


/// Walk the AC-3 / E-AC-3 PES blobs collected during demux, split each
/// on the syncword, and produce an `AudioTrack` plus the per-frame
/// `Sample` list. The first parseable frame seeds the track config
/// (sample rate, channel count, dac3/dec3 payload); subsequent frames
/// just contribute samples (config drift is rare and not handled — if
/// it happens players usually re-init via the sample entry anyway).
fn build_ac3_track(
    blobs: &[(u64, u8, Vec<u8>)],
    eac3: bool,
) -> Result<(Option<AudioTrack>, Vec<Sample>)> {
    let mut samples: Vec<Sample> = Vec::new();
    let mut track: Option<AudioTrack> = None;
    let mut first_info: Option<Ac3FrameInfo> = None;

    for (_pes_pts, _stream_type, blob) in blobs {
        let mut off = 0;
        while off + 7 <= blob.len() {
            // Re-sync to the next syncword if present.
            if blob[off] != 0x0B || blob[off + 1] != 0x77 {
                off += 1;
                continue;
            }
            let info = if eac3 {
                parse_eac3_syncframe(&blob[off..])
            } else {
                parse_ac3_syncframe(&blob[off..])
            };
            let info = match info {
                Some(i) => i,
                None => {
                    off += 1;
                    continue;
                }
            };
            if info.frame_size == 0 || off + info.frame_size > blob.len() {
                break;
            }
            // Sample duration in the track's own timescale: AC-3 and
            // E-AC-3 frames carry exactly 1536 samples.
            let frame_data = blob[off..off + info.frame_size].to_vec();
            samples.push(Sample {
                duration: 1536,
                data: frame_data,
                composition_time_offset: 0,
                is_sync: true,
            });
            if first_info.is_none() {
                first_info = Some(info);
            }
            off += info.frame_size;
        }
    }

    if let Some(info) = first_info {
        let codec = if eac3 {
            AudioCodec::EAc3 { dec3: build_dec3_payload(&info) }
        } else {
            AudioCodec::Ac3 { dac3: build_dac3_payload(&info) }
        };
        track = Some(AudioTrack {
            codec,
            sample_rate: info.sample_rate,
            channels: info.channels,
        });
    }

    if samples.is_empty() {
        bail!("replay_export_format_unsupported");
    }
    Ok((track, samples))
}

/// MP2 counterpart of [`build_ac3_track`]. MP2 frames carry 1152 samples
/// (Layer II) at the configured sample rate.
fn build_mp2_track(blobs: &[(u64, u8, Vec<u8>)]) -> Result<(Option<AudioTrack>, Vec<Sample>)> {
    let mut samples: Vec<Sample> = Vec::new();
    let mut first_info: Option<Mp2FrameInfo> = None;

    for (_pes_pts, _stream_type, blob) in blobs {
        let mut off = 0;
        while off + 4 <= blob.len() {
            if blob[off] != 0xFF || (blob[off + 1] & 0xE0) != 0xE0 {
                off += 1;
                continue;
            }
            let info = match parse_mp2_frame(&blob[off..]) {
                Some(i) => i,
                None => {
                    off += 1;
                    continue;
                }
            };
            if info.frame_size == 0 || off + info.frame_size > blob.len() {
                break;
            }
            samples.push(Sample {
                duration: 1152,
                data: blob[off..off + info.frame_size].to_vec(),
                composition_time_offset: 0,
                is_sync: true,
            });
            if first_info.is_none() {
                first_info = Some(info);
            }
            off += info.frame_size;
        }
    }

    if samples.is_empty() {
        bail!("replay_export_format_unsupported");
    }
    let info = first_info.expect("samples non-empty implies at least one info");
    let track = AudioTrack {
        codec: AudioCodec::Mp2 { avg_bitrate: info.avg_bitrate },
        sample_rate: info.sample_rate,
        channels: info.channels,
    };
    Ok((Some(track), samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::engine::cmaf::fmp4::AudioCodec as FmpAudioCodec;

    #[test]
    fn slice_export_chunk_handles_eof() {
        let bytes = vec![1u8, 2, 3, 4, 5];
        let mut got = slice_export_chunk(&bytes, 0, 2);
        assert_eq!(got.data, vec![1, 2]);
        assert!(!got.eof);
        got = slice_export_chunk(&bytes, 4, 2);
        assert_eq!(got.data, vec![5]);
        assert!(got.eof);
        got = slice_export_chunk(&bytes, 5, 2);
        assert!(got.data.is_empty());
        assert!(got.eof);
    }

    /// Construct a synthetic AC-3 PES blob with two valid syncframes
    /// (48 kHz / 192 kbps / 5.1 / LFE on) and verify
    /// [`build_ac3_track`] recovers the right `AudioTrack` shape and
    /// per-frame sample list.
    #[test]
    fn build_ac3_track_splits_concatenated_syncframes() {
        // 48 kHz 192 kbps frame size = 4 × 192 = 768 bytes.
        let frame_size = 768usize;
        let mut frame = vec![0u8; frame_size];
        frame[0] = 0x0B;
        frame[1] = 0x77;
        // fscod=0, frmsizecod=20 (192 kbps)
        frame[4] = (0u8 << 6) | 20;
        // bsid=8, bsmod=0
        frame[5] = (8 << 3) | 0;
        // acmod=7 (3/2), cmixlev/surmixlev present, lfeon at byte 6 LSB
        frame[6] = (7 << 5) | 0x01; // acmod=7, lfeon=1

        // Two consecutive frames inside one PES blob.
        let mut blob = Vec::with_capacity(frame_size * 2);
        blob.extend_from_slice(&frame);
        blob.extend_from_slice(&frame);
        let blobs = vec![(0u64, super::STREAM_TYPE_AC3_A, blob)];

        let (track, samples) = build_ac3_track(&blobs, /*eac3=*/ false)
            .expect("AC-3 track build");
        let track = track.expect("non-empty track");
        match track.codec {
            FmpAudioCodec::Ac3 { .. } => {}
            _ => panic!("expected AC-3 codec"),
        }
        assert_eq!(track.sample_rate, 48_000);
        assert_eq!(track.channels, 6);
        assert_eq!(samples.len(), 2);
        // AC-3 frame duration is 1536 samples in the track timescale.
        assert!(samples.iter().all(|s| s.duration == 1536 && s.is_sync));
        assert!(samples.iter().all(|s| s.data.len() == frame_size));
    }

    /// Same property end-to-end for MP2 — ensure sample-rate / channel
    /// detection and per-frame sample list survive a multi-frame blob.
    #[test]
    fn build_mp2_track_recovers_48k_stereo_192kbps() {
        // MPEG-1 Layer II, 48 kHz, 192 kbps, stereo. Frame size = 576.
        let frame_size = 576usize;
        let mut frame = vec![0u8; frame_size];
        frame[0] = 0xFF;
        frame[1] = 0xFD;
        frame[2] = 0xA4; // bitrate idx 10 (192 kbps), sr idx 1 (48 kHz)
        frame[3] = 0x00; // mode = stereo
        let mut blob = Vec::with_capacity(frame_size * 2);
        blob.extend_from_slice(&frame);
        blob.extend_from_slice(&frame);
        let blobs = vec![(0u64, super::STREAM_TYPE_MP2_A, blob)];

        let (track, samples) = build_mp2_track(&blobs).expect("MP2 track build");
        let track = track.expect("non-empty track");
        match track.codec {
            FmpAudioCodec::Mp2 { avg_bitrate } => assert_eq!(avg_bitrate, 192_000),
            _ => panic!("expected MP2 codec"),
        }
        assert_eq!(track.sample_rate, 48_000);
        assert_eq!(track.channels, 2);
        assert_eq!(samples.len(), 2);
        // MP2 frame duration is 1152 samples in the track timescale.
        assert!(samples.iter().all(|s| s.duration == 1152 && s.is_sync));
    }

    /// Synthetic AC-3 blob whose first byte is junk before the syncword
    /// — parser must re-sync rather than dropping the frame.
    #[test]
    fn build_ac3_track_resyncs_past_leading_garbage() {
        let frame_size = 768usize;
        let mut frame = vec![0u8; frame_size];
        frame[0] = 0x0B;
        frame[1] = 0x77;
        frame[4] = 20;
        frame[5] = (8 << 3) | 0;
        frame[6] = (7 << 5) | 0x01;

        let mut blob = Vec::with_capacity(frame_size + 4);
        blob.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        blob.extend_from_slice(&frame);
        let blobs = vec![(0u64, super::STREAM_TYPE_AC3_B, blob)];

        let (_track, samples) =
            build_ac3_track(&blobs, false).expect("re-sync past garbage");
        assert_eq!(samples.len(), 1);
    }

    fn cached(key: &str, bytes: usize) {
        insert_cached(key.to_string(), std::sync::Arc::new(vec![0u8; bytes]));
    }

    fn cache_len() -> usize {
        cache().lock().expect("cache").len()
    }

    fn cache_bytes() -> usize {
        cache().lock().expect("cache").values().map(|v| v.bytes.len()).sum()
    }

    fn clear_cache() {
        cache().lock().expect("cache").clear();
    }

    fn sample(duration: u32, is_sync: bool) -> Sample {
        Sample { duration, data: vec![0u8; 8], composition_time_offset: 0, is_sync }
    }

    /// The re-encoder's own lead-in trim has to reach the audio drain.
    ///
    /// This is the shape every release artefact produces: `reencode_all_intra`
    /// drops the undecodable frames before the first keyframe and marks every
    /// frame it emits as sync, so the muxer's own trim finds nothing to do.
    /// Counting only the muxer's trim gave `dropped_ticks == 0`, the audio
    /// drain was skipped, and the clip shipped with the audio leading the
    /// picture by the whole lead-in — 41 samples of 240 on the rig, which at
    /// 25 fps is 1.6 s against gate 3's ±40 ms.
    #[test]
    fn the_reencoders_lead_in_trim_is_counted_against_the_audio() {
        // What the re-encoder hands back: 10 frames at 3600 ticks, all sync,
        // having already discarded 40 source frames (144 000 ticks) ahead of
        // the first keyframe.
        let video: Vec<Sample> = (0..10).map(|_| sample(3600, true)).collect();
        let first_sync = video.iter().position(|s| s.is_sync).unwrap_or(0);
        assert_eq!(first_sync, 0, "the re-encoded list opens on a sync sample");

        let ticks = leading_trim_ticks(144_000, &video, first_sync);
        assert_eq!(ticks, 144_000, "the re-encoder's trim must survive to here");

        // 1.6 s of 48 kHz AAC is 76 800 samples; at 1024 per frame that is 75
        // whole frames the audio has to give up to stay in step.
        let audio: Vec<Sample> = (0..200).map(|_| sample(1024, true)).collect();
        assert_eq!(audio_samples_to_drop(ticks, 90_000, 48_000, &audio), 75);
    }

    /// And the muxer's own trim still counts on the fallback path, where the
    /// re-encode failed and the source GOP structure is shipped as-is.
    #[test]
    fn the_muxers_own_trim_still_counts_when_there_was_no_re_encode() {
        let mut video: Vec<Sample> = (0..3).map(|_| sample(3600, false)).collect();
        video.extend((0..10).map(|_| sample(3600, true)));
        let first_sync = video.iter().position(|s| s.is_sync).expect("a keyframe");
        assert_eq!(leading_trim_ticks(0, &video, first_sync), 3 * 3600);
    }

    /// No audio track means nothing to drain, and no divide by its timescale.
    #[test]
    fn a_video_only_clip_drains_no_audio() {
        assert_eq!(audio_samples_to_drop(144_000, 90_000, 0, &[]), 0);
    }

    /// The cache is bounded by size, not only by age.
    ///
    /// The 5-minute TTL is not a bound when entries arrive faster than they
    /// expire — and a viewer may commission fifty cuts in one POST. Before the
    /// LRU, a burst pinned every one of them for five minutes, in live
    /// allocations `malloc_trim` cannot return.
    ///
    /// Serialised with the sibling cache tests by running them in one `#[test]`:
    /// the cache is process-global and `cargo test` is threaded.
    #[test]
    fn the_export_cache_is_bounded_by_bytes_and_by_count() {
        clear_cache();

        // Byte ceiling: three maximum-size entries cannot all be resident.
        for i in 0..3 {
            cached(&format!("big-{i}"), MP4_EXPORT_MAX_BYTES);
        }
        assert!(
            cache_bytes() <= CACHE_MAX_BYTES,
            "the cache held {} bytes, over the {CACHE_MAX_BYTES} ceiling",
            cache_bytes()
        );
        assert!(
            lookup_cached("big-0").is_none(),
            "the least recently used entry should have been evicted first"
        );

        // Entry ceiling: many small exports cannot do by number what the byte
        // ceiling stops them doing by size.
        clear_cache();
        for i in 0..(CACHE_MAX_ENTRIES + 4) {
            cached(&format!("small-{i}"), 16);
        }
        assert_eq!(cache_len(), CACHE_MAX_ENTRIES);
        assert!(lookup_cached("small-0").is_none(), "oldest out first");
        assert!(
            lookup_cached(&format!("small-{}", CACHE_MAX_ENTRIES + 3)).is_some(),
            "the newest entry must survive"
        );

        clear_cache();
    }
}
