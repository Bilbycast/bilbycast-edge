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
//!
//! Stateless, every chunk plans the export afresh and reads its offset
//! against that plan, so every chunk must plan from the same start. A start
//! the edge resolves for itself — a whole-recording export starts at the
//! first segment retention has kept — moves when retention prunes that
//! segment mid-download, and a chunk planned from the new start served its
//! offset from media one segment further on: a file of the right length with
//! a segment's worth cut out of the middle and nothing to say so. So each
//! chunk names the plan it was cut from ([`PlanStart`], acked as
//! `plan_start_segment_id` / `plan_start_pts_90khz`), the manager sends the
//! start back on every later chunk (`from_pts_90khz` when it had none, and
//! `expect_plan_start_segment_id`), and a chunk whose plan starts anywhere
//! else is refused rather than served.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

use super::clips::ClipStore;
use super::index::InMemoryIndex;
use super::index::IndexEntry;
use super::{SegmentState, segment_path, segment_state, segments_on_disk};

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
    /// Where the plan this chunk was cut from starts, for the caller to pin
    /// the rest of the export to — see the module docs. `None` for an export
    /// built whole and cached (MP4), which has no plan per chunk.
    pub plan_start: Option<PlanStart>,
}

impl ExportChunk {
    /// Add where this chunk's plan starts to its export ack, as
    /// `plan_start_segment_id` / `plan_start_pts_90khz` — nothing for an export
    /// with no plan per chunk. A manager that pins them sends the PTS back as
    /// `from_pts_90khz` when its request had none, and the segment as
    /// `expect_plan_start_segment_id`, on every later chunk; one that does not
    /// ignores them.
    pub fn ack_plan_start(&self, ack: &mut serde_json::Value) {
        if let Some(start) = self.plan_start {
            ack["plan_start_segment_id"] = serde_json::json!(start.segment_id);
            ack["plan_start_pts_90khz"] = serde_json::json!(start.pts_90khz);
        }
    }
}

/// Where an export's plan starts: the segment its first span is in, and the
/// PTS it was resolved from. A request naming the same PTS plans from the
/// same random-access point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanStart {
    pub segment_id: u32,
    pub pts_90khz: u64,
}

/// Resolved byte plan for an export: every segment it reads, in order, with
/// the window of each that falls in range.
///
/// The segments are the ones on disk when the plan was made, and the total is
/// their windows added up, so the total is exactly what a read of the plan
/// returns. It was worked out separately, and the two disagreed: a missing
/// middle segment was left out of both, silently, and a missing last segment
/// was counted and not read, so an export declared bytes it never delivered.
#[derive(Debug, Clone)]
pub struct BytePlan {
    start: PlanStart,
    spans: Vec<SegmentSpan>,
    total_bytes: u64,
}

/// One segment's part in an export: bytes `lo..hi` of segment `id`.
#[derive(Debug, Clone, Copy)]
struct SegmentSpan {
    id: u32,
    lo: u64,
    hi: u64,
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
    read_chunk(dir, &plan, 0, plan.total_bytes).await.map(|c| c.data)
}

/// Read up to `max_bytes` of the export starting at `byte_offset` from
/// the export's logical zero. Re-opens the index + segment file on
/// every call — stateless against the writer. `expect_plan_start` is the
/// segment the export's first chunk was planned from, on a later chunk of a
/// download that pins it — see [`check_plan_start`].
pub async fn export_clip_chunk(
    recording_id: &str,
    clip_id: &str,
    byte_offset: u64,
    max_bytes: u64,
    expect_plan_start: Option<u64>,
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
    check_plan_start(&plan, expect_plan_start)?;
    read_chunk(&dir, &plan, byte_offset, clamp_chunk(max_bytes)).await
}

/// Read up to `max_bytes` of a recording-level export, optionally
/// bounded by `from_pts` / `to_pts`. Total bytes is capped at
/// [`MAX_EXPORT_TOTAL_BYTES`] — over-cap exports must be carved into
/// clips first. `expect_plan_start` as for [`export_clip_chunk`].
pub async fn export_recording_chunk(
    recording_id: &str,
    from_pts_90khz: Option<u64>,
    to_pts_90khz: Option<u64>,
    byte_offset: u64,
    max_bytes: u64,
    expect_plan_start: Option<u64>,
) -> Result<ExportChunk> {
    if let (Some(from), Some(to)) = (from_pts_90khz, to_pts_90khz)
        && to <= from {
            bail!("replay_invalid_range");
        }
    export_recording_chunk_at(
        &super::recording_dir(recording_id),
        from_pts_90khz,
        to_pts_90khz,
        byte_offset,
        max_bytes,
        expect_plan_start,
    )
    .await
}

/// [`export_recording_chunk`] from a named directory rather than the replay
/// root's, so a test can export a recording of its own.
async fn export_recording_chunk_at(
    dir: &Path,
    from_pts_90khz: Option<u64>,
    to_pts_90khz: Option<u64>,
    byte_offset: u64,
    max_bytes: u64,
    expect_plan_start: Option<u64>,
) -> Result<ExportChunk> {
    if !fs::try_exists(dir).await.unwrap_or(false) {
        return Err(anyhow!("replay_clip_not_found"));
    }
    let index = InMemoryIndex::load(&dir.join("index.bin")).await
        .context("load index for export")?;
    let from = match from_pts_90khz {
        Some(from) => from,
        // From the first media retention kept, not the first entry, which
        // names a file it has removed on any recording old enough — for the
        // first chunk, and for a later one whose request names the plan the
        // first was cut from, which is refused below if that start has moved.
        None if byte_offset == 0 || expect_plan_start.is_some() => {
            super::reader::recording_start(&index, dir).await?.pts_90khz
        }
        // A later chunk from a manager that pins nothing: it sends the first
        // chunk's request again with only the offset moved. The first media
        // retention kept is not a start it can keep — a prune between two
        // chunks moves it, and this chunk would be read against a plan a
        // segment further on. The first entry never moves, as index entries
        // are never removed; planned from it, a prune fails the chunk by name
        // rather than splicing the file.
        None => index.entries.first().ok_or_else(|| anyhow!("replay_no_index"))?.pts_90khz,
    };
    let plan = plan_pts_range(&index, dir, from, to_pts_90khz).await?;
    check_plan_start(&plan, expect_plan_start)?;
    if plan.total_bytes > MAX_EXPORT_TOTAL_BYTES {
        bail!("replay_export_too_large");
    }
    read_chunk(dir, &plan, byte_offset, clamp_chunk(max_bytes)).await
}

/// Refuse a chunk whose plan does not start in the segment the export's first
/// chunk was planned from: its offset would be read against other media.
fn check_plan_start(plan: &BytePlan, expected: Option<u64>) -> Result<()> {
    match expected {
        Some(id) if id != u64::from(plan.start.segment_id) => bail!(
            "replay_segment_missing: the export's start, segment {id:06}.ts, was removed while \
             it was being downloaded — the rest of it would come from segment {:06}.ts on; \
             export it again",
            plan.start.segment_id,
        ),
        _ => Ok(()),
    }
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
///
/// **A segment `index.bin` names inside the range must be on disk**, or the
/// export is refused. One that is not was lost — to a crash, or to a close
/// that failed — or retention has removed it, and the media it held is not
/// in the export whatever happens next: `replay_segment_missing`. Skipping
/// it, as this did, produced a file short of what was asked for by however
/// much of the range that segment covered, with nothing to say so: a clip
/// spanning the hole played straight across it. The DVR clip cutter, which
/// reads through here, takes the refusal like any other exact-cut failure and
/// falls back to whole segments from the origin.
///
/// One the live writer has yet to put in place — open, or waiting on its
/// closer — is refused too, but as `replay_segment_not_ready`: the same
/// request succeeds once it closes. A range at the live edge reaches it as a
/// matter of course, a mark-out at "now" first of all.
///
/// The start's own segment is looked at first, whatever the end: from the
/// open segment or a lost tail, with no `to`, the end is the last segment on
/// disk, before the start, and the plan came out empty — an export of no
/// bytes at all, answered as a success.
///
/// A gap no entry names is stepped over, as playback steps over it: a segment
/// with no random-access point in it whose close failed, or the hole an older
/// build left in the ids at every restart, which lost nothing.
async fn plan_pts_range(
    index: &InMemoryIndex,
    dir: &Path,
    from_pts_90khz: u64,
    to_pts_90khz: Option<u64>,
) -> Result<BytePlan> {
    let start = index.find_floor(from_pts_90khz)
        .ok_or_else(|| anyhow!("replay_no_index"))?;
    let start_len = match stat_segment(dir, start.segment_id).await? {
        SegmentState::Finished(len) => len,
        state => return Err(not_exportable(state, start.segment_id, Some(&start))),
    };
    let (end_segment, end_offset) = match to_pts_90khz {
        Some(to) => {
            let end = index.find_floor(to)
                .ok_or_else(|| anyhow!("replay_no_index"))?;
            (end.segment_id, end.byte_offset as u64)
        }
        // To the end of the last segment on disk.
        None => {
            let last = segments_on_disk(dir).await
                .with_context(|| format!("list segments in {}", dir.display()))?
                .last()
                .copied()
                .ok_or_else(|| anyhow!("replay_no_segments"))?;
            (last, u64::MAX)
        }
    };
    let mut spans = Vec::new();
    let mut total_bytes = 0u64;
    if start.segment_id <= end_segment {
        for id in start.segment_id..=end_segment {
            let len = if id == start.segment_id {
                start_len
            } else {
                let named = index.entries.iter().find(|e| e.segment_id == id);
                match stat_segment(dir, id).await? {
                    SegmentState::Finished(len) => len,
                    SegmentState::Lost if named.is_none() => continue,
                    state => return Err(not_exportable(state, id, named)),
                }
            };
            let lo = if id == start.segment_id { (start.byte_offset as u64).min(len) } else { 0 };
            let hi = if id == end_segment { end_offset.min(len) } else { len };
            if hi > lo {
                spans.push(SegmentSpan { id, lo, hi });
                total_bytes += hi - lo;
            }
        }
    }
    Ok(BytePlan {
        start: PlanStart { segment_id: start.segment_id, pts_90khz: from_pts_90khz },
        spans,
        total_bytes,
    })
}

async fn stat_segment(dir: &Path, id: u32) -> Result<SegmentState> {
    segment_state(dir, id).await
        .with_context(|| format!("stat segment {}", segment_path(dir, id).display()))
}

/// The refusal of a range that needs segment `id`, which is not in place —
/// see [`plan_pts_range`]. `named` is the entry `index.bin` names it by.
fn not_exportable(state: SegmentState, id: u32, named: Option<&IndexEntry>) -> anyhow::Error {
    let at = named.map(|e| format!(", which index.bin names at PTS {},", e.pts_90khz)).unwrap_or_default();
    match state {
        SegmentState::Pending => anyhow!(
            "replay_segment_not_ready: segment {id:06}.ts{at} is still being recorded — \
             the range can be exported once it has closed"
        ),
        _ => anyhow!(
            "replay_segment_missing: segment {id:06}.ts{at} is not on disk — lost to a crash \
             or a failed close, or removed by retention — so the range cannot be exported whole"
        ),
    }
}

async fn read_chunk(
    dir: &Path,
    plan: &BytePlan,
    byte_offset: u64,
    max_bytes: u64,
) -> Result<ExportChunk> {
    if byte_offset >= plan.total_bytes {
        return Ok(ExportChunk {
            data: Vec::new(),
            total_bytes: plan.total_bytes,
            byte_offset,
            eof: true,
            plan_start: Some(plan.start),
        });
    }
    let want = max_bytes.min(plan.total_bytes - byte_offset);
    // Round to TS-packet alignment so the file stays demuxable when
    // the manager concatenates chunks.
    let want_aligned = (want / TS_PACKET as u64).max(1) * TS_PACKET as u64;
    let want = want_aligned.min(plan.total_bytes - byte_offset);

    let mut buf = Vec::with_capacity(want as usize);
    let mut span_start_in_export: u64 = 0;
    let mut cursor = byte_offset;

    for span in &plan.spans {
        let span_end_in_export = span_start_in_export + (span.hi - span.lo);
        let span_start = std::mem::replace(&mut span_start_in_export, span_end_in_export);

        // Skip whole segments that are below the cursor.
        if cursor >= span_end_in_export {
            continue;
        }
        let local_start = span.lo + cursor.saturating_sub(span_start);
        let take = want.saturating_sub(buf.len() as u64).min(span.hi - local_start);
        if take == 0 {
            continue;
        }

        // Not skipped when it has gone: the plan counted it, and a read that
        // left it out would hand back fewer bytes than the total promised.
        // Gone between the plan and the read means retention has just removed
        // it; the next request plans afresh from the same start — the one the
        // download pinned, or the first entry for a caller that pins nothing
        // (see the module docs) — and refuses the range by name.
        let path = segment_path(dir, span.id);
        let mut f = fs::File::open(&path).await
            .with_context(|| format!("open segment {}", path.display()))?;
        f.seek(SeekFrom::Start(local_start)).await?;
        let prev_len = buf.len();
        buf.resize(prev_len + take as usize, 0);
        f.read_exact(&mut buf[prev_len..]).await?;

        cursor = span_start + (local_start - span.lo) + take;
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
        plan_start: Some(plan.start),
    })
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
        assert_eq!(plan.spans.iter().map(|s| s.id).collect::<Vec<_>>(), [0, 1, 2]);
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
        let c0 = read_chunk(dir, &plan, 0, 188).await.unwrap();
        assert_eq!(c0.data.len(), 188);
        assert_eq!(c0.byte_offset, 0);
        assert!(!c0.eof);

        let c1 = read_chunk(dir, &plan, 188, 188).await.unwrap();
        assert_eq!(c1.data.len(), 188);
        assert_eq!(c1.data, payload[188..376]);

        let c2 = read_chunk(dir, &plan, 376, 188).await.unwrap();
        assert_eq!(c2.data, payload[376..564]);
        assert!(c2.eof);

        let past_eof = read_chunk(dir, &plan, 564, 188).await.unwrap();
        assert!(past_eof.data.is_empty());
        assert!(past_eof.eof);
    }

    /// Every chunk of an export, end to end, as the manager pulls it: the
    /// plan's start pinned from the first chunk's ack on.
    async fn pull_all(dir: &std::path::Path, from: Option<u64>, to: Option<u64>) -> Result<(Vec<u8>, u64)> {
        let mut data = Vec::new();
        let mut pin: Option<PlanStart> = None;
        loop {
            let c = pull_chunk(dir, from, to, data.len() as u64, pin).await?;
            pin = pin.or(c.plan_start);
            data.extend_from_slice(&c.data);
            if c.eof {
                return Ok((data, c.total_bytes));
            }
            assert!(!c.data.is_empty(), "a chunk short of the end came back empty: the total promised bytes the read never delivered");
        }
    }

    /// One 188-byte chunk at `offset`, as a manager pinning `pin` asks for it
    /// — or, with no pin, as one that sends the same request every time.
    async fn pull_chunk(
        dir: &std::path::Path,
        from: Option<u64>,
        to: Option<u64>,
        offset: u64,
        pin: Option<PlanStart>,
    ) -> Result<ExportChunk> {
        let from = from.or(pin.map(|p| p.pts_90khz));
        export_recording_chunk_at(dir, from, to, offset, 188, pin.map(|p| p.segment_id.into())).await
    }

    /// Five two-packet segments, each indexed at its start, every byte
    /// distinct; returns them by id.
    async fn five_segments(dir: &std::path::Path) -> Vec<Vec<u8>> {
        let mut segments = Vec::new();
        let mut entries = Vec::new();
        for id in 0..5u32 {
            let seg: Vec<u8> = (0..2 * 188).map(|i| (id as usize * 37 + i) as u8).collect();
            write_segment(dir, id, &seg).await;
            segments.push(seg);
            entries.push(idx(id as u64 * 90_000, id, 0));
        }
        make_index(dir, &entries).await;
        segments
    }

    /// An export that meets a segment `index.bin` names but the disk lacks —
    /// lost to a crash or a failed close — is refused by name. It skipped the
    /// segment and handed back a file short of the range with nothing to say
    /// so, and the DVR clip cutter, which reads through here, cut a clip that
    /// played straight across the hole.
    #[tokio::test]
    async fn an_export_refuses_a_segment_the_index_names_but_the_disk_lacks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        five_segments(dir).await;
        fs::remove_file(dir.join("000002.ts")).await.unwrap();
        for (from, to) in [(None, None), (Some(0), Some(400_000)), (Some(100_000), Some(200_000))] {
            let err = pull_all(dir, from, to).await.expect_err("refused");
            assert!(err.to_string().starts_with("replay_segment_missing"), "{from:?}..{to:?}: {err}");
        }
        // The range ending part-way into a segment that has gone: refused,
        // rather than that segment's bytes up to the end point counted in the
        // total and then not read. Segment 2 gets a second random-access point
        // a packet in, for the range to end on.
        let mut entries: Vec<IndexEntry> = (0..5u32).map(|id| idx(id as u64 * 90_000, id, 0)).collect();
        entries.insert(3, idx(225_000, 2, 188));
        fs::remove_file(dir.join("index.bin")).await.unwrap();
        make_index(dir, &entries).await;
        let err = pull_all(dir, Some(0), Some(225_000)).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
        // A range clear of the hole exports as before.
        let (data, total) = pull_all(dir, Some(270_000), None).await.unwrap();
        assert_eq!(data.len() as u64, total);
        assert_eq!(total, 4 * 188);
    }

    /// A segment the live writer has yet to put in place — the open one, or
    /// one waiting on its closer — is not lost: the export is refused as not
    /// ready, and goes through once it has closed. With no writer at work the
    /// same staging file is a crash's leftover, and the segment is lost.
    #[tokio::test]
    async fn a_segment_still_being_recorded_is_not_ready_rather_than_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        fs::create_dir(dir.join(".tmp")).await.unwrap();
        fs::rename(dir.join("000004.ts"), dir.join(".tmp/000004.ts")).await.unwrap();
        let writer = crate::replay::writer::hold_as_writer(dir);
        for (from, to) in [(Some(0), Some(360_000)), (Some(360_000), None)] {
            let err = pull_all(dir, from, to).await.expect_err("refused");
            assert!(err.to_string().starts_with("replay_segment_not_ready"), "{from:?}..{to:?}: {err}");
        }
        // Closed: the same range exports.
        fs::rename(dir.join(".tmp/000004.ts"), dir.join("000004.ts")).await.unwrap();
        let (data, _) = pull_all(dir, Some(0), Some(360_000)).await.unwrap();
        assert_eq!(data, segments[..4].concat());
        // Nobody writing: a staging file is what a crash left.
        fs::rename(dir.join("000004.ts"), dir.join(".tmp/000004.ts")).await.unwrap();
        drop(writer);
        let err = pull_all(dir, Some(0), Some(360_000)).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
    }

    /// An export from a lost tail, with no end given, is refused: the end is
    /// the last segment on disk, before the start, and the plan came out empty
    /// — no bytes, answered as a success.
    #[tokio::test]
    async fn an_export_from_a_lost_tail_is_refused_rather_than_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        five_segments(dir).await;
        fs::remove_file(dir.join("000004.ts")).await.unwrap();
        let err = pull_all(dir, Some(360_000), None).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
    }

    /// A gap no entry names is stepped over, and the total is what the
    /// export then reads — down to the last byte, with the end reported.
    #[tokio::test]
    async fn the_total_is_exactly_what_an_export_reads() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        // Segment 2, with no entry naming it: what an older build's restart
        // left in the ids, or a segment with no random-access point whose
        // close failed.
        let entries: Vec<IndexEntry> = [0u32, 1, 3, 4].iter().map(|&id| idx(id as u64 * 90_000, id, 0)).collect();
        fs::remove_file(dir.join("index.bin")).await.unwrap();
        make_index(dir, &entries).await;
        fs::remove_file(dir.join("000002.ts")).await.unwrap();
        let (data, total) = pull_all(dir, None, None).await.unwrap();
        assert_eq!(data.len() as u64, total);
        assert_eq!(data, [&segments[0][..], &segments[1], &segments[3], &segments[4]].concat());
    }

    /// A whole-recording export starts at the first segment retention kept.
    /// From the first entry, which names a file retention has removed, it
    /// failed on every recording old enough to have been pruned.
    #[tokio::test]
    async fn a_whole_recording_export_starts_where_retention_left_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        fs::remove_file(dir.join("000000.ts")).await.unwrap();
        fs::remove_file(dir.join("000001.ts")).await.unwrap();
        let (data, _) = pull_all(dir, None, None).await.unwrap();
        assert_eq!(data, segments[2..].concat());
    }

    /// Retention pruning a whole-recording export's first segment mid-download
    /// fails the download by name; it does not splice it. Planned afresh from
    /// the first segment still on disk, every later chunk read its offset a
    /// segment further on: a file of the right length with a segment cut out
    /// of the middle. A chunk pinned to the first chunk's start plans from
    /// there; one naming only the start's segment is refused for planning
    /// from another.
    #[tokio::test]
    async fn retention_mid_download_fails_the_download_rather_than_splicing_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        five_segments(dir).await;
        let first = pull_chunk(dir, None, None, 0, None).await.unwrap();
        let pin = first.plan_start.expect("a TS export names its plan's start");
        assert_eq!(pin, PlanStart { segment_id: 0, pts_90khz: 0 });
        // Retention prunes segment 0.
        fs::remove_file(dir.join("000000.ts")).await.unwrap();
        let err = pull_chunk(dir, None, None, 188, Some(pin)).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
        let err = export_recording_chunk_at(dir, None, None, 188, 188, Some(0)).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
        assert!(err.to_string().contains("removed while it was being downloaded"), "{err}");
    }

    /// A manager that pins nothing sends the first chunk's request again with
    /// only the offset moved. The later chunks plan from the first entry
    /// `index.bin` names, which never moves, so a prune mid-download fails
    /// the chunk by name — and a download nothing prunes is whole.
    #[tokio::test]
    async fn a_download_that_pins_nothing_fails_on_a_prune_rather_than_splicing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        let mut data = Vec::new();
        loop {
            let c = pull_chunk(dir, None, None, data.len() as u64, None).await.unwrap();
            data.extend_from_slice(&c.data);
            if c.eof {
                break;
            }
        }
        assert_eq!(data, segments.concat());

        pull_chunk(dir, None, None, 0, None).await.unwrap();
        fs::remove_file(dir.join("000000.ts")).await.unwrap();
        let err = pull_chunk(dir, None, None, 188, None).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
    }

    /// The ack carries the plan's start under the names the manager pins,
    /// and an export with no plan per chunk adds nothing, so a manager sees
    /// today's payload from it.
    #[tokio::test]
    async fn the_ack_names_the_plans_start() {
        let tmp = TempDir::new().unwrap();
        five_segments(tmp.path()).await;
        let chunk = pull_chunk(tmp.path(), Some(100_000), None, 0, None).await.unwrap();
        let mut ack = serde_json::json!({ "eof": chunk.eof });
        chunk.ack_plan_start(&mut ack);
        assert_eq!(ack["plan_start_segment_id"], 1);
        assert_eq!(ack["plan_start_pts_90khz"], 100_000);
        let mut ack = serde_json::json!({});
        ExportChunk { plan_start: None, ..chunk }.ack_plan_start(&mut ack);
        assert_eq!(ack, serde_json::json!({}));
    }

    #[tokio::test]
    async fn export_recording_chunk_rejects_inverted_range() {
        // Inverted range short-circuits before any filesystem touch,
        // so the test doesn't need a populated replay root.
        let err = export_recording_chunk("nonexistent", Some(500), Some(100), 0, 188, None)
            .await.unwrap_err();
        assert_eq!(err.to_string(), "replay_invalid_range");
    }
}
