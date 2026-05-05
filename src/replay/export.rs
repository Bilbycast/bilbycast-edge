// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Chunked TS export from on-disk recordings.
//!
//! The Recordings library tab in the manager UI lets operators
//! download a clip (or a PTS-bounded slice of a recording) as a
//! standard MPEG-TS file. The WS protocol caps a single response at
//! 5 MiB, so the manager pulls the bytes out of the edge in
//! ~1 MiB-base64-sized chunks via repeat calls. Each call re-opens
//! the on-disk reader stateless-ly — `InMemoryIndex::load` is cheap
//! (~24 B/IDR) and the segment files are already byte-aligned, so a
//! pull-based design avoids per-session bookkeeping on the edge.

use anyhow::{Context, Result, anyhow, bail};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

use super::clips::ClipStore;
use super::index::InMemoryIndex;

/// 188 byte MPEG-TS packet — exports stay packet-aligned so any
/// downstream demuxer can ingest the file without resyncing on the
/// first byte.
const TS_PACKET: usize = 188;

/// Default chunk cap (1 MiB) — paired with the WS 5 MiB frame limit
/// after base64 inflation (≈ 4/3) plus JSON envelope overhead.
pub const DEFAULT_CHUNK_BYTES: u64 = 1024 * 1024;

/// Hard cap on a single chunk request to keep the WS frame under 5 MiB
/// even with worst-case base64 + envelope overhead.
pub const MAX_CHUNK_BYTES: u64 = 3 * 1024 * 1024;

/// Hard cap on a whole-recording export — operators who want bigger
/// pulls should mark a clip first. Avoids accidental terabyte
/// downloads of long-running compliance recordings.
pub const MAX_EXPORT_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// One [`export_clip`] / [`export_recording`] response.
#[derive(Debug, Clone)]
pub struct ExportChunk {
    /// Raw TS bytes — caller base64-encodes for the WS envelope.
    pub data: Vec<u8>,
    /// Total bytes in the export (across all chunks). Stable across
    /// chunks of the same export so the manager can render a
    /// progress bar.
    pub total_bytes: u64,
    /// Byte offset of `data[0]` within the export.
    pub byte_offset: u64,
    /// `true` on the last chunk — the next byte_offset would be
    /// `>= total_bytes`. Caller stops calling.
    pub eof: bool,
}

/// Resolved byte plan for an export — first/last segment id and the
/// byte offset to start / stop at within those segments.
#[derive(Debug, Clone, Copy)]
pub struct BytePlan {
    start_segment: u32,
    start_offset_in_segment: u64,
    end_segment: u32,
    /// Exclusive end offset within the last segment.
    end_offset_in_segment: u64,
    total_bytes: u64,
}

impl BytePlan {
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

/// Public wrapper around `plan_pts_range` for siblings (e.g. the MP4
/// remuxer) that need to compute the byte plan ahead of a one-shot
/// read.
pub async fn plan_for_pts_range_pub(
    index: &InMemoryIndex,
    dir: &std::path::Path,
    from_pts_90khz: u64,
    to_pts_90khz: Option<u64>,
) -> Result<BytePlan> {
    plan_pts_range(index, dir, from_pts_90khz, to_pts_90khz).await
}

/// Read the full byte range described by `plan` into memory in one
/// go. Used by the MP4 remuxer where streaming-build isn't currently
/// implemented.
pub async fn read_full_range(dir: &std::path::Path, plan: BytePlan) -> Result<Vec<u8>> {
    read_chunk(dir, plan, 0, plan.total_bytes).await.map(|c| c.data)
}

/// Read up to `max_bytes` of the export starting at `byte_offset` from
/// the export's logical zero. Re-opens the index + segment file on
/// every call — stateless against the writer.
pub async fn export_clip_chunk(
    recording_id: &str,
    clip_id: &str,
    byte_offset: u64,
    max_bytes: u64,
) -> Result<ExportChunk> {
    let dir = super::recording_dir(recording_id);
    if !fs::try_exists(&dir).await.unwrap_or(false) {
        return Err(anyhow!("replay_clip_not_found"));
    }
    let store = ClipStore::open(recording_id, &dir).await?;
    let clip = store.get(clip_id).await
        .ok_or_else(|| anyhow!("replay_clip_not_found"))?;
    if clip.out_pts_90khz <= clip.in_pts_90khz {
        bail!("replay_invalid_range");
    }
    let index = InMemoryIndex::load(&dir.join("index.bin")).await
        .context("load index for export")?;
    let plan = plan_pts_range(&index, &dir, clip.in_pts_90khz, Some(clip.out_pts_90khz)).await?;
    read_chunk(&dir, plan, byte_offset, clamp_chunk(max_bytes)).await
}

/// Read up to `max_bytes` of a recording-level export, optionally
/// bounded by `from_pts` / `to_pts`. Total bytes is capped at
/// [`MAX_EXPORT_TOTAL_BYTES`] — over-cap exports must be carved into
/// clips first.
pub async fn export_recording_chunk(
    recording_id: &str,
    from_pts_90khz: Option<u64>,
    to_pts_90khz: Option<u64>,
    byte_offset: u64,
    max_bytes: u64,
) -> Result<ExportChunk> {
    if let (Some(from), Some(to)) = (from_pts_90khz, to_pts_90khz) {
        if to <= from {
            bail!("replay_invalid_range");
        }
    }
    let dir = super::recording_dir(recording_id);
    if !fs::try_exists(&dir).await.unwrap_or(false) {
        return Err(anyhow!("replay_clip_not_found"));
    }
    let index = InMemoryIndex::load(&dir.join("index.bin")).await
        .context("load index for export")?;
    let from = from_pts_90khz
        .or_else(|| index.entries.first().map(|e| e.pts_90khz))
        .ok_or_else(|| anyhow!("replay_no_index"))?;
    let plan = plan_pts_range(&index, &dir, from, to_pts_90khz).await?;
    if plan.total_bytes > MAX_EXPORT_TOTAL_BYTES {
        bail!("replay_export_too_large");
    }
    read_chunk(&dir, plan, byte_offset, clamp_chunk(max_bytes)).await
}

fn clamp_chunk(requested: u64) -> u64 {
    if requested == 0 {
        DEFAULT_CHUNK_BYTES
    } else {
        requested.min(MAX_CHUNK_BYTES)
    }
}

/// Resolve the export's byte plan from a PTS range. `to_pts = None`
/// means "to end of recording". Snaps the start to the IDR ≤ from_pts
/// (so the export plays from a clean GOP), and the end to the IDR ≤
/// to_pts (slightly under-includes — matches the behaviour the JKL
/// player applies on playback so the file plays back identically to
/// what the operator scrubbed).
async fn plan_pts_range(
    index: &InMemoryIndex,
    dir: &std::path::Path,
    from_pts_90khz: u64,
    to_pts_90khz: Option<u64>,
) -> Result<BytePlan> {
    let start_entry = index.find_floor(from_pts_90khz)
        .ok_or_else(|| anyhow!("replay_no_index"))?;
    let end_segment;
    let end_offset_in_segment;
    if let Some(to) = to_pts_90khz {
        let end_entry = index.find_floor(to)
            .ok_or_else(|| anyhow!("replay_no_index"))?;
        end_segment = end_entry.segment_id;
        end_offset_in_segment = end_entry.byte_offset as u64;
    } else {
        let last_seg = scan_last_segment_id(dir).await
            .ok_or_else(|| anyhow!("replay_no_segments"))?;
        let last_path = dir.join(format!("{:06}.ts", last_seg));
        let last_size = fs::metadata(&last_path).await
            .with_context(|| format!("stat segment {}", last_path.display()))?
            .len();
        end_segment = last_seg;
        end_offset_in_segment = last_size;
    }
    let total = total_bytes_in_range(
        dir,
        start_entry.segment_id,
        start_entry.byte_offset as u64,
        end_segment,
        end_offset_in_segment,
    ).await?;
    Ok(BytePlan {
        start_segment: start_entry.segment_id,
        start_offset_in_segment: start_entry.byte_offset as u64,
        end_segment,
        end_offset_in_segment,
        total_bytes: total,
    })
}

async fn scan_last_segment_id(dir: &std::path::Path) -> Option<u32> {
    let mut max_id: Option<u32> = None;
    let mut entries = fs::read_dir(dir).await.ok()?;
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name();
        let Some(s) = name.to_str() else { continue };
        let Some(stem) = s.strip_suffix(".ts") else { continue };
        if let Ok(id) = stem.parse::<u32>() {
            max_id = Some(max_id.map_or(id, |cur| cur.max(id)));
        }
    }
    max_id
}

async fn total_bytes_in_range(
    dir: &std::path::Path,
    start_seg: u32,
    start_off: u64,
    end_seg: u32,
    end_off: u64,
) -> Result<u64> {
    if start_seg > end_seg || (start_seg == end_seg && start_off >= end_off) {
        return Ok(0);
    }
    if start_seg == end_seg {
        return Ok(end_off - start_off);
    }
    // First segment: from start_off to its file size.
    let first_path = dir.join(format!("{:06}.ts", start_seg));
    let first_size = fs::metadata(&first_path).await
        .with_context(|| format!("stat segment {}", first_path.display()))?
        .len();
    let mut acc = first_size.saturating_sub(start_off);
    // Middle segments: full size each.
    for id in (start_seg + 1)..end_seg {
        let p = dir.join(format!("{:06}.ts", id));
        if let Ok(meta) = fs::metadata(&p).await {
            acc = acc.saturating_add(meta.len());
        }
    }
    // Last segment: 0..end_off.
    acc = acc.saturating_add(end_off);
    Ok(acc)
}

async fn read_chunk(
    dir: &std::path::Path,
    plan: BytePlan,
    byte_offset: u64,
    max_bytes: u64,
) -> Result<ExportChunk> {
    if byte_offset >= plan.total_bytes {
        return Ok(ExportChunk {
            data: Vec::new(),
            total_bytes: plan.total_bytes,
            byte_offset,
            eof: true,
        });
    }
    let want = max_bytes.min(plan.total_bytes - byte_offset);
    // Round to TS-packet alignment so the file stays demuxable when
    // the manager concatenates chunks.
    let want_aligned = (want / TS_PACKET as u64).max(1) * TS_PACKET as u64;
    let want = want_aligned.min(plan.total_bytes - byte_offset);

    let mut buf = Vec::with_capacity(want as usize);
    let mut absolute_export_offset: u64 = 0;
    let mut cursor = byte_offset;

    for seg_id in plan.start_segment..=plan.end_segment {
        let seg_path = dir.join(format!("{:06}.ts", seg_id));
        let meta = match fs::metadata(&seg_path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let seg_len = meta.len();
        let (seg_lo, seg_hi) = seg_window(&plan, seg_id, seg_len);
        let seg_bytes = seg_hi - seg_lo;
        let seg_start_in_export = absolute_export_offset;
        let seg_end_in_export = absolute_export_offset + seg_bytes;
        absolute_export_offset = seg_end_in_export;

        // Skip whole segments that are below the cursor.
        if cursor >= seg_end_in_export {
            continue;
        }
        // Stop once we've filled the chunk.
        if buf.len() as u64 >= want {
            break;
        }

        let local_start = if cursor > seg_start_in_export {
            seg_lo + (cursor - seg_start_in_export)
        } else {
            seg_lo
        };
        let remaining = want.saturating_sub(buf.len() as u64);
        let available = seg_hi - local_start;
        let take = remaining.min(available);
        if take == 0 {
            continue;
        }

        let mut f = fs::File::open(&seg_path).await
            .with_context(|| format!("open segment {}", seg_path.display()))?;
        f.seek(SeekFrom::Start(local_start)).await?;
        let prev_len = buf.len();
        buf.resize(prev_len + take as usize, 0);
        f.read_exact(&mut buf[prev_len..]).await?;

        cursor = seg_start_in_export + (local_start - seg_lo) + take;
        if buf.len() as u64 >= want {
            break;
        }
    }

    let new_offset = byte_offset + buf.len() as u64;
    let eof = new_offset >= plan.total_bytes;
    Ok(ExportChunk {
        data: buf,
        total_bytes: plan.total_bytes,
        byte_offset,
        eof,
    })
}

fn seg_window(plan: &BytePlan, seg_id: u32, seg_len: u64) -> (u64, u64) {
    let lo = if seg_id == plan.start_segment {
        plan.start_offset_in_segment.min(seg_len)
    } else {
        0
    };
    let hi = if seg_id == plan.end_segment {
        plan.end_offset_in_segment.min(seg_len)
    } else {
        seg_len
    };
    (lo, hi.max(lo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::index::{IndexEntry, IndexWriter};
    use tempfile::TempDir;

    async fn write_segment(dir: &std::path::Path, id: u32, bytes: &[u8]) {
        let path = dir.join(format!("{:06}.ts", id));
        fs::write(&path, bytes).await.unwrap();
    }

    fn idx(pts: u64, seg: u32, off: u32) -> IndexEntry {
        IndexEntry {
            pts_90khz: pts,
            segment_id: seg,
            byte_offset: off,
            smpte_tc: 0,
            flags: 0,
        }
    }

    async fn make_index(dir: &std::path::Path, entries: &[IndexEntry]) {
        let mut w = IndexWriter::open(&dir.join("index.bin")).await.unwrap();
        for e in entries {
            w.append(*e).await.unwrap();
        }
        w.flush_and_sync().await.unwrap();
    }

    #[tokio::test]
    async fn plan_total_bytes_for_full_segment_range() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // Three 188-byte segments with one IDR each at offset 0.
        for i in 0..3u32 {
            write_segment(dir, i, &[0u8; 188]).await;
        }
        make_index(dir, &[idx(0, 0, 0), idx(90_000, 1, 0), idx(180_000, 2, 0)]).await;
        let index = InMemoryIndex::load(&dir.join("index.bin")).await.unwrap();
        // Whole-recording plan (to_pts = None) should cover all three segments.
        let plan = plan_pts_range(&index, dir, 0, None).await.unwrap();
        assert_eq!(plan.start_segment, 0);
        assert_eq!(plan.end_segment, 2);
        assert_eq!(plan.total_bytes, 3 * 188);
    }

    #[tokio::test]
    async fn read_chunk_alignment_and_eof() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let payload: Vec<u8> = (0..(188 * 3)).map(|i| (i % 251) as u8).collect();
        write_segment(dir, 0, &payload[0..188]).await;
        write_segment(dir, 1, &payload[188..376]).await;
        write_segment(dir, 2, &payload[376..564]).await;
        make_index(dir, &[idx(0, 0, 0), idx(90_000, 1, 0), idx(180_000, 2, 0)]).await;
        let index = InMemoryIndex::load(&dir.join("index.bin")).await.unwrap();
        let plan = plan_pts_range(&index, dir, 0, None).await.unwrap();

        // Pull one TS packet at a time.
        let c0 = read_chunk(dir, plan, 0, 188).await.unwrap();
        assert_eq!(c0.data.len(), 188);
        assert_eq!(c0.byte_offset, 0);
        assert!(!c0.eof);

        let c1 = read_chunk(dir, plan, 188, 188).await.unwrap();
        assert_eq!(c1.data.len(), 188);
        assert_eq!(c1.data, payload[188..376]);

        let c2 = read_chunk(dir, plan, 376, 188).await.unwrap();
        assert_eq!(c2.data, payload[376..564]);
        assert!(c2.eof);

        let past_eof = read_chunk(dir, plan, 564, 188).await.unwrap();
        assert!(past_eof.data.is_empty());
        assert!(past_eof.eof);
    }

    #[tokio::test]
    async fn export_recording_chunk_rejects_inverted_range() {
        // Inverted range short-circuits before any filesystem touch,
        // so the test doesn't need a populated replay root.
        let err = export_recording_chunk("nonexistent", Some(500), Some(100), 0, 188)
            .await.unwrap_err();
        assert_eq!(err.to_string(), "replay_invalid_range");
    }
}
