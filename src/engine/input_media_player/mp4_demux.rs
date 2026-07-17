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
//!
//! **Fragmented MP4 (fMP4 — `moof`/`traf`/`trun`) is also out of scope**
//! and is rejected up-front (see [`demux_file`]): the `mp4` crate (0.14)
//! cannot address samples that live in movie-fragment boxes — its
//! `sample_offset()` returns `tfhd.base_data_offset.unwrap_or(0)` for
//! every sample, which is `0` under the near-universal `default_base_moof`
//! flag, so every "sample" is read from file offset 0 and the real coded
//! slices are lost. Left unchecked that produces an *undecodable* TS
//! (SPS/PPS/SEI but no picture slices → receivers report "unspecified
//! size") emitted at wire speed (fragment sample durations decode as 0, so
//! nothing paces the player). Same remedy as HEVC: re-mux to a plain MP4
//! (`ffmpeg -i in.mp4 -c copy -movflags +faststart out.mp4`) or to a `.ts`
//! source.

use std::collections::BinaryHeap;
use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow};
use bytes::{Bytes, BytesMut};
use mp4::{AudioObjectType, MediaType, SampleFreqIndex, TrackType};

use super::{
    DEFAULT_FALLBACK_BITRATE_BPS, PACER_QUEUE_CAP, PacerMsg, PlayerSession, emit_to_pacer,
    run_paced_emitter,
};
use crate::engine::rtmp::ts_mux::TsMuxer;

/// Stream type byte stamped on the synthesised PMT for AAC ADTS audio.
const STREAM_TYPE_AAC: u8 = 0x0F;
/// Stream type byte stamped on the synthesised PMT for H.264 video.
const STREAM_TYPE_H264: u8 = 0x1B;

pub async fn play_mp4_file(
    path: &Path,
    paced_bitrate_bps: Option<u64>,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    // The `mp4` crate is sync-only — open + parse on a blocking thread,
    // then drive sample emission back in the async task with paced sleeps.
    let path_owned = path.to_path_buf();
    let demux: DemuxResult = tokio::task::spawn_blocking(move || demux_file(&path_owned))
        .await
        .map_err(|e| anyhow!("mp4 spawn_blocking join failed: {e}"))??;

    play_demuxed(path, demux, paced_bitrate_bps, session).await
}

/// Per-sample presentation duration (µs) for a track, i.e. how long each
/// sample stays on screen before the next same-track sample. The pacer
/// spreads a sample's TS bundles across this window so a large frame's
/// packet burst is smoothed across the frame's own airtime rather than
/// landing instantaneously (issue #67).
///
/// Derived from consecutive `start_time` deltas (in track timescale units).
/// The final sample has no successor, so it reuses the previous sample's
/// duration (or a 33 ms ≈ 30 fps default for a single-sample track).
fn sample_durations_us(samples: &[DemuxedSample], timescale: u32) -> Vec<u64> {
    let n = samples.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let dur = if i + 1 < n {
            let d = samples[i + 1].start_time.saturating_sub(samples[i].start_time);
            ts_to_us(d, timescale)
        } else if i > 0 {
            // Last sample: reuse the previous inter-sample delta.
            let d = samples[i].start_time.saturating_sub(samples[i - 1].start_time);
            ts_to_us(d, timescale)
        } else {
            0
        };
        // Guard against a zero/degenerate duration (identical timestamps, or
        // a single-sample track) so the pacer always has a non-zero spread
        // window — otherwise a whole sample would collapse back to a burst.
        out.push(if dur == 0 { 33_000 } else { dur });
    }
    out
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

    // Reject fragmented MP4 (fMP4) BEFORE reading any samples. Movie
    // fragments (`moof`/`traf`/`trun`) are parsed into `Mp4Track::trafs` by
    // the `mp4` crate, but its sample addressing for them is broken:
    // `sample_offset()` returns `tfhd.base_data_offset.unwrap_or(0)` — the
    // same value for every sample, and `0` under the `default_base_moof`
    // flag that `ffmpeg -movflags frag_keyframe+empty_moov`,
    // MSE/MediaRecorder and DASH/HLS/CMAF packagers all emit — so every
    // sample is read from file offset 0 (the `ftyp` box) and the coded
    // slices never make it out. Playing it anyway yields a TS carrying only
    // SPS/PPS/SEI and no picture slices (undecodable — ffprobe/mediamtx
    // report "unspecified size" / profile unknown), emitted at wire speed
    // because fragment sample durations decode as 0. Fail loudly with an
    // actionable remedy instead.
    if mp4.tracks().values().any(|t| !t.trafs.is_empty()) {
        return Err(anyhow!(
            "fragmented MP4 (fMP4 / moof) is not supported by the media player: {} \
             stores its coded frames in movie-fragment boxes the demuxer cannot address, \
             which would emit an undecodable stream. Re-mux to a plain (unfragmented) MP4 \
             with `ffmpeg -i in.mp4 -c copy -movflags +faststart out.mp4`, or transcode to \
             MPEG-TS and use a `ts` source instead",
            path.display()
        ));
    }

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

async fn play_demuxed(
    path: &Path,
    d: DemuxResult,
    paced_bitrate_bps: Option<u64>,
    session: &mut PlayerSession<'_>,
) -> Result<()> {
    let has_video = d.video.is_some();
    let has_audio = d.audio.is_some();
    let mut ts_mux = TsMuxer::new();
    if let Some(po) = session.pid_overrides {
        if let Some(entry) = po.get(&1) {
                ts_mux.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
            }
    }
    ts_mux.set_has_video(has_video);
    if has_video {
        ts_mux.set_video_stream_type(STREAM_TYPE_H264);
    }
    if has_audio {
        ts_mux.set_has_audio(true);
        ts_mux.set_audio_stream(STREAM_TYPE_AAC, None);
    } else {
        ts_mux.set_has_audio(false);
    }

    // ── Smooth-splice continuity ──────────────────────────────────────
    // Register this file's layout with the playlist-wide continuity. If
    // the layout differs from the previous file the PMT version_number
    // bumps; otherwise the receiver sees a stable PMT and skips the
    // decoder re-init. Then seed the muxer's CC counters from the
    // previous file's last wire values so the on-wire CC sequence stays
    // continuous, and apply the per-file PTS offset so the emitted
    // timeline picks up immediately after the previous file's last PTS.
    const VIDEO_PID: u16 = 0x0100;
    const AUDIO_PID: u16 = 0x0101;
    const PAT_PID: u16 = 0x0000;
    const PMT_PID: u16 = 0x1000;
    let layout = super::StreamLayout {
        video_pid: has_video.then_some(VIDEO_PID),
        video_stream_type: has_video.then_some(STREAM_TYPE_H264),
        audio_pid: has_audio.then_some(AUDIO_PID),
        audio_stream_type: has_audio.then_some(STREAM_TYPE_AAC),
    };
    session.cont.update_layout(layout);
    let cc_pat = session.cont.last_cc.get(&PAT_PID).copied().unwrap_or(0xFF);
    let cc_pmt = session.cont.last_cc.get(&PMT_PID).copied().unwrap_or(0xFF);
    let cc_video = session.cont.last_cc.get(&VIDEO_PID).copied().unwrap_or(0xFF);
    let cc_audio = session.cont.last_cc.get(&AUDIO_PID).copied().unwrap_or(0xFF);
    ts_mux.seed_cc(cc_pat, cc_pmt, cc_video, cc_audio);
    ts_mux.set_pmt_version(session.cont.pmt_version);
    let pts_offset_90k = session.cont.next_target_output_pts_90k;
    let mut max_emitted_pts_90k: u64 = pts_offset_90k;

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
    // carries a track tag (so we know whether to mux as video or audio) and
    // its presentation duration (so the pacer can spread the sample's bundles
    // across the sample's own airtime — see `sample_durations_us`).
    let video_durs = d.video.as_ref().map(|t| sample_durations_us(&t.samples, t.timescale));
    let audio_durs = d.audio.as_ref().map(|t| sample_durations_us(&t.samples, t.timescale));
    let mut heap: BinaryHeap<Item> = BinaryHeap::new();
    if let Some(ref t) = d.video {
        let durs = video_durs.as_ref().unwrap();
        for (i, s) in t.samples.iter().enumerate() {
            heap.push(Item {
                wall_us: ts_to_us(s.start_time, t.timescale),
                dur_us: durs[i],
                track: Track::Video,
                index: i,
            });
        }
    }
    if let Some(ref t) = d.audio {
        let durs = audio_durs.as_ref().unwrap();
        for (i, s) in t.samples.iter().enumerate() {
            heap.push(Item {
                wall_us: ts_to_us(s.start_time, t.timescale),
                dur_us: durs[i],
                track: Track::Audio,
                index: i,
            });
        }
    }

    let video_samples = d.video.as_ref().map(|t| &t.samples);
    let audio_samples = d.audio.as_ref().map(|t| &t.samples);

    // ── Sample-presentation-time-anchored OS-thread pacer ───────────────
    //
    // See `MEDIA_PLAYER_BURSTY_MP4_ISSUE` (#67): this loop reads whole
    // samples (already fully demuxed into `d`) and muxes them to TS. A large
    // compressed sample (e.g. a several-hundred-KB IDR) produces hundreds of
    // TS packets; emitting them all in one synchronous scheduler slice races
    // far ahead of any downstream output's real wire rate and can overflow
    // the broadcast channel / downstream queues (silent playout failure).
    //
    // The fix paces every bundle through the OS-thread pacer, but — unlike
    // `play_ts_file`'s byte-rate mode — with **explicit per-bundle deadlines
    // derived from the MP4 sample presentation timeline**:
    //
    //   * Each sample's FIRST bundle is anchored to the sample's true
    //     presentation time (`epoch + wall_us`). The muxer puts the PCR in
    //     the first TS packet of every video frame, so the PCR-bearing
    //     packet lands at exactly its presentation time → PCR-vs-wall stays
    //     linear → TR-101290 PCR_AC / PCR_repetition pass (+0).
    //   * The sample's REMAINING bundles are spread evenly across the
    //     sample's own presentation duration (`dur_us`), so a large frame's
    //     packet burst is smoothed across the frame's airtime instead of
    //     landing instantaneously (#67 fixed).
    //
    // Why not the byte-rate pacer used for TS? Byte-rate pacing decouples
    // packet arrival from the sample-timestamp PCR clock; on VBR content the
    // byte rate ≠ the instantaneous PCR rate, so PCR-packet arrival drifts
    // non-linearly and TR-101290 PCR_AC fails. Verified on hardware
    // (`big_buck_bunny_1080p_h264.mp4`, bilby-bite): byte-rate pacing grew
    // `pcr_accuracy_errors` continuously through steady playback; this
    // deadline mode holds them at zero, matching stock's per-sample timing.
    //
    // Because the MP4 path no longer paces by a bitrate at all, issue #68's
    // rate-inheritance concern is structurally moot here — there is no rate
    // to carry across a playlist transition. (`SpliceContinuity`'s
    // source-scoped carried rate still protects the TS-passthrough path.)
    let source_id = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    let _ = paced_bitrate_bps; // not consulted in deadline mode

    let (pacer_tx, pacer_rx) = std::sync::mpsc::sync_channel::<PacerMsg>(PACER_QUEUE_CAP);
    let broadcast_for_pacer = session.per_input_tx.clone();
    let cancel_for_pacer = session.cancel.clone();
    let media_stats_for_pacer = session.media_stats.clone();
    let pacer_thread_name = format!("media-pacer-{source_id}");
    let pacer_thread = std::thread::Builder::new()
        .name(pacer_thread_name.clone())
        .spawn(move || {
            run_paced_emitter(
                pacer_thread_name,
                pacer_rx,
                broadcast_for_pacer,
                // Byte-rate is unused in deadline mode; pass a nominal so the
                // pacer's `bitrate_bps.max(1)` guard never divides by zero.
                DEFAULT_FALLBACK_BITRATE_BPS,
                cancel_for_pacer,
                media_stats_for_pacer,
            );
        })
        .with_context(|| "spawn media-player mp4 pacer thread")?;
    let mut pending_src_bytes: u64 = 0;

    // Wall-clock anchor: presentation time 0 maps to `epoch_ns`. Every
    // bundle's deadline is `epoch_ns + presentation_offset_ns`. The preroll
    // keeps the first deadline just far enough in the future that the pacer
    // thread has received the bundle before its deadline. Both this producer
    // and the pacer read the same monotonic clock, so the deadlines are
    // directly comparable across the thread boundary.
    const PREROLL_NS: u64 = 50_000_000; // 50 ms, matches run_paced_emitter
    let epoch_ns = crate::engine::wire_emit::monotonic_now_ns().saturating_add(PREROLL_NS);
    let mut bundle = BytesMut::with_capacity(session.bundle_size);

    'sample: while let Some(it) = heap.pop() {
        if session.cancel.is_cancelled() {
            break;
        }

        let is_video = matches!(it.track, Track::Video);
        let chunks: Vec<bytes::Bytes> = match it.track {
            Track::Video => {
                let s = &video_samples.unwrap()[it.index];
                let pts_90 = (ts_to_90khz(
                    s.start_time as i64 + s.rendering_offset as i64,
                    video_timescale,
                )
                .wrapping_add(pts_offset_90k))
                    & 0x1_FFFF_FFFF;
                let dts_90 =
                    (ts_to_90khz(s.start_time as i64, video_timescale)
                        .wrapping_add(pts_offset_90k))
                        & 0x1_FFFF_FFFF;
                let annex_b = avcc_to_annex_b(&s.bytes, &sps_nal, &pps_nal, s.is_sync);
                if pts_90 > max_emitted_pts_90k {
                    max_emitted_pts_90k = pts_90;
                }
                session.media_stats.video_samples_read.fetch_add(1, Ordering::Relaxed);
                session
                    .media_stats
                    .largest_video_sample_bytes
                    .fetch_max(annex_b.len() as u64, Ordering::Relaxed);
                ts_mux.mux_video(&annex_b, pts_90, dts_90, s.is_sync)
            }
            Track::Audio => {
                let s = &audio_samples.unwrap()[it.index];
                let pts_90 = (ts_to_90khz(s.start_time as i64, audio_timescale)
                    .wrapping_add(pts_offset_90k))
                    & 0x1_FFFF_FFFF;
                let (profile, sr_idx, ch_cfg) =
                    aac_extra.expect("audio path implies aac_extra");
                let mut adts =
                    build_adts_header(profile, sr_idx, ch_cfg, s.bytes.len());
                adts.extend_from_slice(&s.bytes);
                if pts_90 > max_emitted_pts_90k {
                    max_emitted_pts_90k = pts_90;
                }
                session.media_stats.audio_samples_read.fetch_add(1, Ordering::Relaxed);
                ts_mux.mux_audio_pre_adts(&adts, pts_90)
            }
        };

        // Group this sample's TS packets into bundles, flushing any carry so
        // the sample's FIRST bundle begins with its FIRST TS packet (the
        // PCR-bearing one for video). Bundles never span a sample boundary,
        // so per-bundle deadline anchoring stays exact.
        debug_assert!(bundle.is_empty(), "bundle must be flushed at each sample boundary");
        let mut sample_bundles: Vec<Bytes> = Vec::new();
        for ts_pkt in &chunks {
            bundle.extend_from_slice(ts_pkt);
            if bundle.len() >= session.bundle_size {
                let data = std::mem::replace(
                    &mut bundle,
                    BytesMut::with_capacity(session.bundle_size),
                )
                .freeze();
                sample_bundles.push(data);
            }
        }
        if !bundle.is_empty() {
            let data = std::mem::replace(
                &mut bundle,
                BytesMut::with_capacity(session.bundle_size),
            )
            .freeze();
            sample_bundles.push(data);
        }

        // Emit each bundle at `anchor + (i/N) * dur`, so bundle 0 (with the
        // PCR) lands at the sample's true presentation time and the rest are
        // spread across the sample's airtime.
        let n = sample_bundles.len() as u64;
        let anchor_ns = epoch_ns.saturating_add(it.wall_us.saturating_mul(1_000));
        let dur_ns = it.dur_us.saturating_mul(1_000);
        for (i, data) in sample_bundles.into_iter().enumerate() {
            let target_ns = anchor_ns.saturating_add(dur_ns.saturating_mul(i as u64) / n.max(1));
            if !emit_to_pacer(
                data,
                session,
                &pacer_tx,
                0, // byte-rate unused in deadline mode
                &mut pending_src_bytes,
                Some(target_ns),
            )
            .await
            {
                break 'sample;
            }
        }

        let now_ms = crate::util::time::now_us() / 1000;
        if is_video {
            session.media_stats.video_samples_emitted.fetch_add(1, Ordering::Relaxed);
            session.media_stats.last_video_emit_ms.store(now_ms, Ordering::Relaxed);
        } else {
            session.media_stats.audio_samples_emitted.fetch_add(1, Ordering::Relaxed);
            session.media_stats.last_audio_emit_ms.store(now_ms, Ordering::Relaxed);
        }
    }

    // Tear down the OS-thread pacer: dropping the sender lets the thread's
    // `recv_timeout` poll see `Disconnected` on its next iteration (within
    // 50 ms). Join from a blocking-friendly context so this thread isn't
    // pinned on a tokio worker while the pacer drains its remaining queue.
    drop(pacer_tx);
    let _ = tokio::task::spawn_blocking(move || pacer_thread.join()).await;

    // Spill the muxer's final CC values back into continuity so the next
    // file picks up the wire CC sequence without a reset, then publish
    // the high-water-mark PTS so the next file's offset starts from there.
    let (cc_pat, cc_pmt, cc_video, cc_audio) = ts_mux.current_cc();
    session.cont.last_cc.insert(PAT_PID, cc_pat);
    session.cont.last_cc.insert(PMT_PID, cc_pmt);
    if has_video {
        session.cont.last_cc.insert(VIDEO_PID, cc_video);
    }
    if has_audio {
        session.cont.last_cc.insert(AUDIO_PID, cc_audio);
    }
    session.cont.close_file(max_emitted_pts_90k);
    Ok(())
}

#[derive(Eq, PartialEq)]
struct Item {
    /// Presentation time of this sample, in µs from the file start.
    wall_us: u64,
    /// Presentation duration of this sample (µs until the next same-track
    /// sample). The pacer spreads this sample's TS bundles across this
    /// window so a large sample's packet burst is smoothed across the
    /// frame's own airtime instead of landing instantaneously.
    dur_us: u64,
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
    use crate::engine::packet::RtpPacket;
    use tokio::sync::broadcast;

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

    fn avc_sample(payload_len: usize, start_time: u64, is_sync: bool) -> DemuxedSample {
        let payload = vec![0xABu8; payload_len];
        let mut bytes = Vec::with_capacity(4 + payload_len);
        bytes.extend_from_slice(&(payload_len as u32).to_be_bytes());
        bytes.extend_from_slice(&payload);
        DemuxedSample {
            start_time,
            rendering_offset: 0,
            is_sync,
            bytes,
        }
    }

    #[test]
    fn sample_durations_us_uses_deltas_and_reuses_last() {
        // 30 fps @ 90 kHz timescale → 3000 ticks = 33_333 µs per frame.
        let samples = vec![
            avc_sample(10, 0, true),
            avc_sample(10, 3_000, false),
            avc_sample(10, 6_000, false),
        ];
        let durs = sample_durations_us(&samples, 90_000);
        assert_eq!(durs, vec![33_333, 33_333, 33_333], "each frame ~33.3 ms; last reuses prev");
    }

    #[test]
    fn sample_durations_us_single_sample_falls_back_to_default() {
        let samples = vec![avc_sample(10, 0, true)];
        // No successor and no predecessor → 33 ms default so the pacer still
        // has a non-zero spread window (never collapses back to a burst).
        assert_eq!(sample_durations_us(&samples, 90_000), vec![33_000]);
    }

    /// End-to-end regression for #67: a single oversized IDR must have its TS
    /// bundles spread across real wall-clock time by the deadline pacer, not
    /// delivered in one synchronous burst. Before the fix, `play_demuxed`
    /// pushed every bundle for a sample straight onto the broadcast channel
    /// with no `.await` between them — the entire ~50 KB IDR's ~38 bundles
    /// arrived within microseconds of each other.
    ///
    /// The IDR here has a 100 ms presentation duration (next frame 100 ms
    /// later), so the deadline pacer spreads its bundles across ~100 ms.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_idr_sample_bundles_are_paced_not_bursted() {
        const IDR_BYTES: usize = 49_996; // + 4-byte length prefix = 50_000
        let sps = vec![0x67, 0x42, 0x00, 0x1E];
        let pps = vec![0x68, 0xCE, 0x3C, 0x80];
        // `avcc_to_annex_b` prepends SPS+PPS (each in its own 4-byte start
        // code) on sync samples; `largest_video_sample_bytes` records that
        // expanded size, since that's what hits the muxer/pacer.
        let idr_annex_b_len = IDR_BYTES + 4 + (4 + sps.len()) + (4 + pps.len());
        let video = TrackData {
            timescale: 90_000,
            samples: vec![
                avc_sample(IDR_BYTES, 0, true),
                avc_sample(96, 9_000, false), // 100 ms later — the IDR's spread window
            ],
            extra: TrackExtra::Avc { sps, pps },
        };
        let demux = DemuxResult { video: Some(video), audio: None };

        let (tx, mut rx) = broadcast::channel::<RtpPacket>(4096);
        let reader = tokio::spawn(async move {
            let mut arrivals: Vec<std::time::Instant> = Vec::new();
            while rx.recv().await.is_ok() {
                arrivals.push(std::time::Instant::now());
            }
            arrivals
        });

        let stats = std::sync::Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "test-flow".into(),
            "test-flow-name".into(),
            "media_player".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut seq_num: u16 = 0;
        let mut cont = super::super::SpliceContinuity::default();
        let mut transcoder: Option<crate::engine::input_transcode::InputTranscoder> = None;
        let (events, _events_rx) = crate::manager::events::event_channel();
        let media_stats = std::sync::Arc::new(crate::stats::collector::MediaPlayerStats::default());

        cont.open_file("synthetic-idr.mp4");
        {
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
                transcoder: &mut transcoder,
                pid_overrides: None,
                post: &mut None,
                splice_gap_signal: None,
                bundle_size: super::super::BUNDLE_SIZE,
                media_stats: &media_stats,
                events: &events,
                flow_id: "test-flow",
                input_id: "test-input",
            };
            play_demuxed(Path::new("synthetic-idr.mp4"), demux, None, &mut session)
                .await
                .unwrap();
        }
        drop(tx);

        let arrivals = reader.await.unwrap();
        assert!(
            arrivals.len() >= 30,
            "expected the ~50 KB IDR to split into many bundles, got {}",
            arrivals.len()
        );
        assert_eq!(
            media_stats.largest_video_sample_bytes.load(Ordering::Relaxed) as usize,
            idr_annex_b_len,
            "largest_video_sample_bytes must reflect the IDR's Annex-B size (incl. prepended SPS/PPS)"
        );
        assert_eq!(
            media_stats.video_samples_read.load(Ordering::Relaxed),
            2,
            "both video samples must be read"
        );

        // Core regression assertion: the IDR's bundles must be spread across
        // real wall-clock time (its ~100 ms presentation window), not
        // delivered in one scheduler tick. Compare the first arrival against
        // one well before the tail (margin for scheduler jitter).
        let spread_index = arrivals.len().saturating_sub(5).max(1);
        let spread = arrivals[spread_index].duration_since(arrivals[0]);
        assert!(
            spread > std::time::Duration::from_millis(30),
            "IDR bundles arrived within {spread:?} of each other — burst was not paced \
             (expected spread over a meaningful fraction of the ~100 ms sample window)"
        );
    }

    /// The deadline pacer must anchor each video sample's FIRST bundle to the
    /// sample's true presentation time. That is what keeps PCR-packet arrival
    /// linear vs PCR value (TR-101290 PCR_AC / PCR_repetition pass) — the
    /// property the byte-rate pacer broke on VBR content (verified regressing
    /// on hardware). Here we drive three evenly-spaced video frames and assert
    /// that the wall-clock arrival of each frame's first bundle tracks its
    /// presentation interval, within a tolerance far tighter than the 100 ms
    /// TR-101290 threshold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_bundle_of_each_frame_lands_at_its_presentation_time() {
        // 3 small single-bundle frames, 90 ms apart (8100 ticks @ 90 kHz).
        const FRAME_TICKS: u64 = 8_100;
        const FRAME_MS: u128 = 90;
        let video = TrackData {
            timescale: 90_000,
            samples: vec![
                avc_sample(200, 0, true),
                avc_sample(200, FRAME_TICKS, false),
                avc_sample(200, 2 * FRAME_TICKS, false),
            ],
            extra: TrackExtra::Avc { sps: vec![0x67, 0x42], pps: vec![0x68, 0xCE] },
        };
        let demux = DemuxResult { video: Some(video), audio: None };

        let (tx, mut rx) = broadcast::channel::<RtpPacket>(1024);
        let reader = tokio::spawn(async move {
            let mut arrivals: Vec<std::time::Instant> = Vec::new();
            while rx.recv().await.is_ok() {
                arrivals.push(std::time::Instant::now());
            }
            arrivals
        });

        let stats = std::sync::Arc::new(crate::stats::collector::FlowStatsAccumulator::new(
            "test-flow".into(),
            "test-flow-name".into(),
            "media_player".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut seq_num: u16 = 0;
        let mut cont = super::super::SpliceContinuity::default();
        let mut transcoder: Option<crate::engine::input_transcode::InputTranscoder> = None;
        let (events, _events_rx) = crate::manager::events::event_channel();
        let media_stats = std::sync::Arc::new(crate::stats::collector::MediaPlayerStats::default());

        cont.open_file("three-frames.mp4");
        {
            let mut session = PlayerSession {
                seq_num: &mut seq_num,
                per_input_tx: &tx,
                stats: &stats,
                cancel: &cancel,
                cont: &mut cont,
                transcoder: &mut transcoder,
                pid_overrides: None,
                post: &mut None,
                splice_gap_signal: None,
                bundle_size: super::super::BUNDLE_SIZE,
                media_stats: &media_stats,
                events: &events,
                flow_id: "test-flow",
                input_id: "test-input",
            };
            play_demuxed(Path::new("three-frames.mp4"), demux, None, &mut session)
                .await
                .unwrap();
        }
        drop(tx);

        let arrivals = reader.await.unwrap();
        // Each frame is small enough to be a single bundle (+ PAT/PMT on the
        // first), so consecutive arrivals are ~one frame interval apart.
        assert!(arrivals.len() >= 3, "expected at least one bundle per frame, got {}", arrivals.len());
        // The gap between the first bundles of consecutive frames should
        // track the 90 ms presentation interval (generously ±40 ms for
        // scheduler jitter — still far inside the 100 ms PCR_AC threshold).
        let gap_1 = arrivals[1].duration_since(arrivals[0]).as_millis();
        assert!(
            (FRAME_MS.saturating_sub(40)..=FRAME_MS + 40).contains(&gap_1),
            "frame-to-frame arrival gap {gap_1} ms should track the {FRAME_MS} ms presentation \
             interval (anchoring keeps PCR arrival linear)"
        );
    }
}
