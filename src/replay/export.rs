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
//! Stateless, a chunk planned the export afresh and read its offset against
//! that plan, so a download was whole only if every chunk's plan was the
//! first one's. A start the edge resolves for itself — a whole-recording
//! export starts at the first segment retention has kept — moves when
//! retention prunes that segment mid-download, and a chunk planned from the
//! new start served its offset from media one segment further on: a file of
//! the right length with a segment's worth cut out of the middle and nothing
//! to say so.
//!
//! So a download carries its own place in the plan. Every chunk but the last
//! hands out a [`Resume`] in its ack — the segment and the byte in it where the
//! next chunk starts, and where the plan ends — and a manager that follows it
//! (one that sends `pin_plan_start: true`) sends it back, untouched, with the
//! next request. That chunk is read from the cursor to the plan's end, with no
//! start resolved again: the segments behind the cursor, delivered already and
//! the first retention removes, are never looked at, while one still owed that
//! has gone is refused rather than stepped over, and an export with no end
//! given stops at the end its first chunk announced however far the recording
//! has grown since.
//!
//! A manager from before the cursor sends neither field, and the first chunk's
//! request again with only the offset moved. Every chunk it asks for without a
//! start, the first included, is planned from the first entry `index.bin`
//! names, which never moves: a recording retention has pruned is refused at the
//! first chunk, before that manager has answered the browser, and a prune
//! during the download fails the chunk by name rather than splicing the file.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
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
    /// Where the next chunk starts — see the module docs. `None` on the last
    /// chunk, and for an export built whole and cached (MP4), which has no
    /// plan per chunk.
    pub resume: Option<Resume>,
}

impl ExportChunk {
    /// Add where the next chunk starts to this chunk's export ack, as
    /// `resume` — nothing on the last chunk, or for an export with no plan per
    /// chunk. A manager that follows it sends it back with the next request;
    /// one that does not ignores it.
    pub fn ack_resume(&self, ack: &mut serde_json::Value) {
        if let Some(resume) = self.resume {
            ack["resume"] = serde_json::json!(resume);
        }
    }
}

/// Where a download's next chunk starts and where its plan ends: handed out
/// with every chunk but the last and sent back with the next request — see the
/// module docs. The manager does not read it, so it is this edge's to define,
/// and it is read back as strictly as anything else that arrives over the wire
/// ([`ChunkCursor::from_request`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resume {
    /// The segment the next chunk starts in, and the byte in it.
    segment: u32,
    offset: u64,
    /// The plan's last segment, and the byte its last span ends at.
    end_segment: u32,
    end_offset: u64,
    /// What the plan has left from the cursor to its end. A segment no entry
    /// names is stepped over when it is not on disk — a gap that never held
    /// media — so the count is what tells such a segment that has gone with
    /// bytes still owed from it.
    remaining: u64,
}

impl Resume {
    /// Refuse one this edge would never have handed out: a cursor at or past
    /// the end it names, or more left than an export may hold.
    fn checked(self) -> Result<Self> {
        if (self.segment, self.offset) >= (self.end_segment, self.end_offset)
            || self.remaining == 0
            || self.remaining > MAX_EXPORT_TOTAL_BYTES
        {
            bail!("replay_invalid_field: export resume {self:?} names no place in an export");
        }
        Ok(self)
    }
}

/// What a chunk request says about the plan it is read from — see the module
/// docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChunkCursor {
    /// `pin_plan_start`: the manager follows the cursor, so its first chunk of
    /// a whole recording may start where retention has left it.
    pins: bool,
    /// `resume`: where the previous chunk's ack said this one starts.
    resume: Option<Resume>,
}

impl ChunkCursor {
    /// Read from an export request. A `resume` that is not one this edge hands
    /// out is refused, `replay_invalid_field`.
    pub fn from_request(request: &serde_json::Value) -> Result<Self> {
        let resume = match request.get("resume") {
            None | Some(serde_json::Value::Null) => None,
            Some(resume) => Some(
                Resume::deserialize(resume)
                    .map_err(|e| anyhow!("replay_invalid_field: export resume: {e}"))?
                    .checked()?,
            ),
        };
        Ok(Self { pins: request["pin_plan_start"].as_bool().unwrap_or(false), resume })
    }
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
    /// Where the plan's first byte falls in the export: 0 for a plan of the
    /// whole of it, and the offset a chunk asks for when it is read from its
    /// cursor ([`plan_from_resume`]).
    base: u64,
    spans: Vec<SegmentSpan>,
    /// The bytes in `spans`.
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

    /// Where the plan stands `at` bytes in: the next chunk's [`Resume`], or
    /// `None` at its end.
    fn resume_at(&self, at: u64) -> Option<Resume> {
        let last = self.spans.last()?;
        let mut start = 0;
        for span in &self.spans {
            let end = start + (span.hi - span.lo);
            if at < end {
                return Some(Resume {
                    segment: span.id,
                    offset: span.lo + (at - start),
                    end_segment: last.id,
                    end_offset: last.hi,
                    remaining: self.total_bytes - at,
                });
            }
            start = end;
        }
        None
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
/// every call — stateless against the writer. A chunk naming a cursor is read
/// from it; one that names none plans from the clip's in-point.
pub async fn export_clip_chunk(
    recording_id: &str,
    clip_id: &str,
    byte_offset: u64,
    max_bytes: u64,
    cursor: ChunkCursor,
) -> Result<ExportChunk> {
    export_clip_chunk_at(&super::recording_dir(recording_id), recording_id, clip_id, byte_offset, max_bytes, cursor)
        .await
}

/// [`export_clip_chunk`] from a named directory rather than the replay root's,
/// so a test can export a clip of its own.
async fn export_clip_chunk_at(
    dir: &Path,
    recording_id: &str,
    clip_id: &str,
    byte_offset: u64,
    max_bytes: u64,
    cursor: ChunkCursor,
) -> Result<ExportChunk> {
    if !fs::try_exists(dir).await.unwrap_or(false) {
        return Err(anyhow!("replay_clip_not_found"));
    }
    let store = ClipStore::open(recording_id, dir).await?;
    let clip = store.get(clip_id).await
        .ok_or_else(|| anyhow!("replay_clip_not_found"))?;
    if clip.out_pts_90khz <= clip.in_pts_90khz {
        bail!("replay_invalid_range");
    }
    let index = InMemoryIndex::load(&dir.join("index.bin")).await
        .context("load index for export")?;
    // From the cursor, not the clip's in-point: a trim during the download
    // moves the in-point, and a chunk planned from the new one read its offset
    // against other media.
    let plan = match cursor.resume {
        Some(resume) => plan_from_resume(&index, dir, resume, byte_offset).await?,
        None => plan_pts_range(&index, dir, clip.in_pts_90khz, Some(clip.out_pts_90khz)).await?,
    };
    read_chunk(dir, &plan, byte_offset, clamp_chunk(max_bytes)).await
}

/// Read up to `max_bytes` of a recording-level export, optionally
/// bounded by `from_pts` / `to_pts`. Total bytes is capped at
/// [`MAX_EXPORT_TOTAL_BYTES`] — over-cap exports must be carved into
/// clips first. `cursor` as for [`export_clip_chunk`].
pub async fn export_recording_chunk(
    recording_id: &str,
    from_pts_90khz: Option<u64>,
    to_pts_90khz: Option<u64>,
    byte_offset: u64,
    max_bytes: u64,
    cursor: ChunkCursor,
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
        cursor,
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
    cursor: ChunkCursor,
) -> Result<ExportChunk> {
    if !fs::try_exists(dir).await.unwrap_or(false) {
        return Err(anyhow!("replay_clip_not_found"));
    }
    let index = InMemoryIndex::load(&dir.join("index.bin")).await
        .context("load index for export")?;
    if let Some(resume) = cursor.resume {
        let plan = plan_from_resume(&index, dir, resume, byte_offset).await?;
        return read_chunk(dir, &plan, byte_offset, clamp_chunk(max_bytes)).await;
    }
    let from = match from_pts_90khz {
        Some(from) => from,
        // The first chunk of a download that will follow the cursor: from the
        // first media retention kept, not the first entry, which names a file
        // retention has removed on any recording old enough.
        None if cursor.pins && byte_offset == 0 => {
            super::reader::recording_start(&index, dir).await?.pts_90khz
        }
        // A manager that follows no cursor sends the first chunk's request
        // again with only the offset moved, so whatever this plans from, every
        // chunk of its download plans from too. The first media retention kept
        // is not a start it can keep — a prune between two chunks moves it, and
        // the later chunk read its offset a segment further on. The first entry
        // never moves, as index entries are never removed: a recording pruned
        // already is refused at the first chunk, before that manager has
        // answered the browser, and a prune mid-download fails the chunk.
        None => {
            let first = *index.entries.first().ok_or_else(|| anyhow!("replay_no_index"))?;
            if stat_segment(dir, first.segment_id).await? == SegmentState::Lost {
                bail!(
                    "replay_segment_missing: the recording's first segment, {:06}.ts, is no \
                     longer on disk, and an export with no start given is read from it on every \
                     chunk by the manager asking for this one — export a clip or a from/to range, \
                     or upgrade the manager",
                    first.segment_id,
                );
            }
            first.pts_90khz
        }
    };
    let plan = plan_pts_range(&index, dir, from, to_pts_90khz).await?;
    if plan.total_bytes > MAX_EXPORT_TOTAL_BYTES {
        bail!("replay_export_too_large");
    }
    read_chunk(dir, &plan, byte_offset, clamp_chunk(max_bytes)).await
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
    Ok(BytePlan { base: 0, spans, total_bytes })
}

/// The rest of a download's plan, from the cursor its previous chunk handed
/// out to the end that chunk's plan named, read from export offset `base` —
/// never a plan resolved afresh, which is how a download came to be spliced.
///
/// Nothing behind the cursor is looked at. It is delivered, and it is what
/// retention removes first: planned again from the start on every chunk, a
/// download of a recording at its retention limit failed the first time a
/// segment closed — every segment or so — with every byte it still owed on
/// disk. A segment still owed that has gone is refused,
/// `replay_segment_missing`: by name when `index.bin` names it, and by the
/// count when it held no random-access point, since a gap no entry names is
/// stepped over here as the plan stepped over it.
async fn plan_from_resume(
    index: &InMemoryIndex,
    dir: &Path,
    resume: Resume,
    base: u64,
) -> Result<BytePlan> {
    let foreign = || anyhow!("replay_invalid_field: export resume {resume:?} names bytes this recording does not hold");
    // The end first: it bounds the walk, so a cursor naming an end this
    // recording does not have costs one look, not one per id up to it.
    match stat_segment(dir, resume.end_segment).await? {
        SegmentState::Finished(len) if resume.end_offset <= len => {}
        SegmentState::Finished(_) => return Err(foreign()),
        state => return Err(still_owed(state, resume.end_segment)),
    }
    let mut spans = Vec::new();
    let mut total_bytes = 0u64;
    for id in resume.segment..=resume.end_segment {
        let len = match stat_segment(dir, id).await? {
            SegmentState::Finished(len) => len,
            SegmentState::Lost if id != resume.segment && !index.entries.iter().any(|e| e.segment_id == id) => {
                continue;
            }
            state => return Err(still_owed(state, id)),
        };
        let lo = if id == resume.segment { resume.offset } else { 0 };
        let hi = if id == resume.end_segment { resume.end_offset } else { len };
        if lo > len {
            return Err(foreign());
        }
        if hi > lo {
            spans.push(SegmentSpan { id, lo, hi });
            total_bytes += hi - lo;
        }
    }
    if total_bytes < resume.remaining {
        bail!(
            "replay_segment_missing: {} bytes the export has still to deliver, from segment \
             {:06}.ts on, are no longer on disk — lost, or removed by retention, since it began; \
             export it again",
            resume.remaining - total_bytes,
            resume.segment,
        );
    }
    if total_bytes > resume.remaining {
        return Err(foreign());
    }
    Ok(BytePlan { base, spans, total_bytes })
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

/// The refusal of a resumed chunk whose segment `id`, still owed, is not in
/// place — see [`plan_from_resume`].
fn still_owed(state: SegmentState, id: u32) -> anyhow::Error {
    match state {
        SegmentState::Lost => anyhow!(
            "replay_segment_missing: segment {id:06}.ts, which the export has still to deliver, \
             is no longer on disk — lost, or removed by retention, since it began; export it again"
        ),
        state => not_exportable(state, id, None),
    }
}

/// Read up to `max_bytes` of `plan` from export offset `byte_offset`, which is
/// at or past the plan's `base`.
async fn read_chunk(
    dir: &Path,
    plan: &BytePlan,
    byte_offset: u64,
    max_bytes: u64,
) -> Result<ExportChunk> {
    let total_bytes = plan.base.saturating_add(plan.total_bytes);
    let at = byte_offset.saturating_sub(plan.base);
    if at >= plan.total_bytes {
        return Ok(ExportChunk {
            data: Vec::new(),
            total_bytes,
            byte_offset,
            eof: true,
            resume: None,
        });
    }
    let want = max_bytes.min(plan.total_bytes - at);
    // Round to TS-packet alignment so the file stays demuxable when
    // the manager concatenates chunks.
    let want_aligned = (want / TS_PACKET as u64).max(1) * TS_PACKET as u64;
    let want = want_aligned.min(plan.total_bytes - at);

    let mut buf = Vec::with_capacity(want as usize);
    let mut span_start_in_plan: u64 = 0;
    let mut cursor = at;

    for span in &plan.spans {
        let span_end_in_plan = span_start_in_plan + (span.hi - span.lo);
        let span_start = std::mem::replace(&mut span_start_in_plan, span_end_in_plan);

        // Skip whole segments that are below the cursor.
        if cursor >= span_end_in_plan {
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
        // it, and the chunk fails.
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

    let next = at + buf.len() as u64;
    Ok(ExportChunk {
        data: buf,
        total_bytes,
        byte_offset,
        eof: next >= plan.total_bytes,
        resume: plan.resume_at(next),
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

    /// A manager that follows the cursor.
    const FOLLOWS: ChunkCursor = ChunkCursor { pins: true, resume: None };
    /// A manager from before the cursor.
    const OLDER: ChunkCursor = ChunkCursor { pins: false, resume: None };

    /// One 188-byte chunk at `offset`, asked for with `cursor`.
    async fn pull_chunk(
        dir: &std::path::Path,
        from: Option<u64>,
        to: Option<u64>,
        offset: u64,
        cursor: ChunkCursor,
    ) -> Result<ExportChunk> {
        export_recording_chunk_at(dir, from, to, offset, 188, cursor).await
    }

    /// A download as a manager that follows the cursor pulls it: `pin_plan_start`
    /// on every request, and each ack's `resume` sent back with the next — through
    /// the request JSON, as the manager sends it.
    struct Download {
        data: Vec<u8>,
        total: Option<u64>,
        resume: Option<serde_json::Value>,
    }

    impl Download {
        fn new() -> Self {
            Self { data: Vec::new(), total: None, resume: None }
        }

        fn cursor(&self) -> ChunkCursor {
            let mut request = serde_json::json!({ "pin_plan_start": true });
            if let Some(resume) = &self.resume {
                request["resume"] = resume.clone();
            }
            ChunkCursor::from_request(&request).unwrap()
        }

        /// Take one chunk's ack; `true` once it was the last.
        fn take(&mut self, c: ExportChunk) -> bool {
            assert_eq!(c.byte_offset, self.data.len() as u64, "a chunk came back for another offset");
            assert_eq!(*self.total.get_or_insert(c.total_bytes), c.total_bytes, "the total moved mid-download");
            let mut ack = serde_json::json!({});
            c.ack_resume(&mut ack);
            self.resume = ack.get("resume").cloned();
            assert_eq!(self.resume.is_none(), c.eof, "a resume on the last chunk, or none before it");
            self.data.extend_from_slice(&c.data);
            assert!(c.eof || !c.data.is_empty(), "a chunk short of the end came back empty");
            c.eof
        }

        async fn recording(mut self, dir: &std::path::Path, from: Option<u64>, to: Option<u64>) -> Result<(Vec<u8>, u64)> {
            loop {
                let c = pull_chunk(dir, from, to, self.data.len() as u64, self.cursor()).await?;
                if self.take(c) {
                    return Ok((self.data, self.total.unwrap()));
                }
            }
        }
    }

    /// Every chunk of an export, end to end, as a manager that follows the
    /// cursor pulls it.
    async fn pull_all(dir: &std::path::Path, from: Option<u64>, to: Option<u64>) -> Result<(Vec<u8>, u64)> {
        Download::new().recording(dir, from, to).await
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

    /// A whole-recording export by a manager that follows the cursor starts
    /// at the first segment retention kept. From the first entry, which names
    /// a file retention has removed, it failed on every recording old enough
    /// to have been pruned.
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

    /// Retention removing what a download has delivered costs it nothing. It
    /// planned every chunk from the start again — the first segment on disk,
    /// or the one the first chunk was cut from — so on a recording at its
    /// retention limit, which loses its oldest segment every time one closes,
    /// the download died at the first prune with every byte it still owed on
    /// disk; earlier still, it read its offset against the new start and
    /// spliced the file.
    #[tokio::test]
    async fn retention_removing_what_a_download_has_delivered_costs_it_nothing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        let mut download = Download::new();
        // Into segment 2: two and a half segments delivered.
        while download.data.len() < 5 * 188 {
            let c = pull_chunk(dir, None, None, download.data.len() as u64, download.cursor()).await.unwrap();
            assert!(!download.take(c));
        }
        // Retention prunes the two segments behind it.
        fs::remove_file(dir.join("000000.ts")).await.unwrap();
        fs::remove_file(dir.join("000001.ts")).await.unwrap();
        let (data, _) = download.recording(dir, None, None).await.unwrap();
        assert_eq!(data, segments.concat());
    }

    /// A segment a download still owes that goes is refused by name — the
    /// one the cursor is in as much as one ahead of it — rather than stepped
    /// over.
    #[tokio::test]
    async fn a_segment_still_owed_that_goes_fails_the_download() {
        for gone in ["000002.ts", "000003.ts"] {
            let tmp = TempDir::new().unwrap();
            let dir = tmp.path();
            five_segments(dir).await;
            let mut download = Download::new();
            while download.data.len() < 5 * 188 {
                let c = pull_chunk(dir, None, None, download.data.len() as u64, download.cursor()).await.unwrap();
                download.take(c);
            }
            fs::remove_file(dir.join(gone)).await.unwrap();
            let err = download.recording(dir, None, None).await.expect_err("refused");
            assert!(err.to_string().starts_with("replay_segment_missing"), "{gone}: {err}");
            assert!(err.to_string().contains(&gone[..6]), "{gone}: {err}");
        }
    }

    /// One still owed that no entry names — a segment with no random-access
    /// point in it — is refused too, by the count: stepped over as the plan
    /// steps over a gap no entry names, it spliced the file.
    #[tokio::test]
    async fn an_owed_segment_no_entry_names_that_goes_fails_the_download() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        five_segments(dir).await;
        let entries: Vec<IndexEntry> = [0u32, 1, 3, 4].iter().map(|&id| idx(id as u64 * 90_000, id, 0)).collect();
        fs::remove_file(dir.join("index.bin")).await.unwrap();
        make_index(dir, &entries).await;
        let mut download = Download::new();
        while download.data.len() < 188 {
            let c = pull_chunk(dir, None, None, download.data.len() as u64, download.cursor()).await.unwrap();
            download.take(c);
        }
        fs::remove_file(dir.join("000002.ts")).await.unwrap();
        let err = download.recording(dir, None, None).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
    }

    /// An export with no end given stops where its first chunk said it would:
    /// segments the recording closes during the download are not in it, and
    /// the total the manager sent the browser as its length holds.
    #[tokio::test]
    async fn a_download_ends_where_its_first_chunk_said() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        let mut download = Download::new();
        let c = pull_chunk(dir, None, None, 0, download.cursor()).await.unwrap();
        download.take(c);
        write_segment(dir, 5, &[0x47; 2 * 188]).await;
        let (data, total) = download.recording(dir, None, None).await.unwrap();
        assert_eq!(total, 5 * 2 * 188);
        assert_eq!(data, segments.concat());
    }

    /// A clip trimmed during its download is still downloaded as the first
    /// chunk planned it. Every chunk planned from the clip's in-point, so a
    /// trim to a later random-access point in the same segment read the rest
    /// against the moved start: a file missing what lay between the two.
    #[tokio::test]
    async fn a_clip_trimmed_during_its_download_downloads_as_it_began() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        // Segment 1 gets a second random-access point a packet in.
        let mut entries: Vec<IndexEntry> = (0..5u32).map(|id| idx(id as u64 * 90_000, id, 0)).collect();
        entries.insert(2, idx(135_000, 1, 188));
        fs::remove_file(dir.join("index.bin")).await.unwrap();
        make_index(dir, &entries).await;
        let store = ClipStore::open("rec", dir).await.unwrap();
        let clip = store.create("rec", 90_000, 360_000, None, None, None, None, None, Vec::new()).await.unwrap();

        let mut download = Download::new();
        let c = export_clip_chunk_at(dir, "rec", &clip.id, 0, 188, download.cursor()).await.unwrap();
        download.take(c);
        store.update(&clip.id, None, None, None, Some(135_000), None).await.unwrap();
        loop {
            let at = download.data.len() as u64;
            let c = export_clip_chunk_at(dir, "rec", &clip.id, at, 188, download.cursor()).await.unwrap();
            if download.take(c) {
                break;
            }
        }
        assert_eq!(download.data, segments[1..4].concat());
    }

    /// A manager from before the cursor sends the first chunk's request again
    /// with only the offset moved, so every chunk it asks for is planned from
    /// the first entry `index.bin` names, which never moves: a download
    /// nothing prunes is whole, a prune mid-download fails it rather than
    /// splicing it, and a recording pruned already is refused at the first
    /// chunk — before that manager has sent the browser a length — where it
    /// was refused only at the second, after the download had begun.
    #[tokio::test]
    async fn a_manager_that_follows_no_cursor_is_refused_before_it_has_begun() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let segments = five_segments(dir).await;
        let mut data = Vec::new();
        loop {
            let c = pull_chunk(dir, None, None, data.len() as u64, OLDER).await.unwrap();
            assert!(c.resume.is_some() != c.eof, "a resume on the last chunk, or none before it");
            data.extend_from_slice(&c.data);
            if c.eof {
                break;
            }
        }
        assert_eq!(data, segments.concat());

        pull_chunk(dir, None, None, 0, OLDER).await.unwrap();
        fs::remove_file(dir.join("000000.ts")).await.unwrap();
        let err = pull_chunk(dir, None, None, 188, OLDER).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
        // Pruned already: the first chunk is refused, and says what to do.
        let err = pull_chunk(dir, None, None, 0, OLDER).await.expect_err("refused at the first chunk");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
        assert!(err.to_string().contains("upgrade the manager"), "{err}");
        // A cursor-following manager's request past the first chunk that
        // carries no cursor is read as an older one's.
        let err = pull_chunk(dir, None, None, 188, FOLLOWS).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
    }

    /// A `resume` comes back over the wire, so anything this edge would not
    /// have handed out is refused before it is read from — and an end this
    /// recording does not have costs one look, not a walk up to it.
    #[tokio::test]
    async fn a_resume_this_edge_did_not_hand_out_is_refused() {
        let ok = serde_json::json!({ "segment": 1, "offset": 0, "end_segment": 4, "end_offset": 376, "remaining": 1504 });
        let with = |key: &str, value: serde_json::Value| {
            let mut resume = ok.clone();
            resume[key] = value;
            serde_json::json!({ "pin_plan_start": true, "resume": resume })
        };
        let cursor = ChunkCursor::from_request(&serde_json::json!({ "resume": ok.clone() })).unwrap();
        assert!(cursor.resume.is_some());
        assert!(ChunkCursor::from_request(&serde_json::json!({ "resume": null })).unwrap().resume.is_none());
        for request in [
            with("extra", serde_json::json!(1)),
            with("segment", serde_json::json!(u64::from(u32::MAX) + 1)),
            with("offset", serde_json::json!(-1)),
            with("segment", serde_json::json!("1")),
            with("segment", serde_json::json!(5)),
            with("remaining", serde_json::json!(0)),
            with("remaining", serde_json::json!(MAX_EXPORT_TOTAL_BYTES + 1)),
        ] {
            let err = ChunkCursor::from_request(&request).expect_err("refused");
            assert!(err.to_string().starts_with("replay_invalid_field"), "{request}: {err}");
        }

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        five_segments(dir).await;
        for resume in [
            // An end this recording never closed: no walk to segment 4 billion.
            serde_json::json!({ "segment": 0, "offset": 0, "end_segment": u32::MAX, "end_offset": 1, "remaining": 1 }),
            // Past what the segments hold.
            serde_json::json!({ "segment": 1, "offset": 0, "end_segment": 4, "end_offset": 377, "remaining": 1505 }),
            serde_json::json!({ "segment": 1, "offset": 400, "end_segment": 4, "end_offset": 376, "remaining": 1104 }),
            // More than lies between the two.
            serde_json::json!({ "segment": 1, "offset": 0, "end_segment": 4, "end_offset": 376, "remaining": 1 }),
        ] {
            let cursor = ChunkCursor::from_request(&serde_json::json!({ "resume": resume })).unwrap();
            let err = pull_chunk(dir, None, None, 188, cursor).await.expect_err("refused");
            let err = err.to_string();
            assert!(err.starts_with("replay_invalid_field") || err.starts_with("replay_segment_missing"), "{resume}: {err}");
        }
    }

    /// The ack names where the next chunk starts under `resume` on every
    /// chunk but the last, and an export with no plan per chunk adds nothing,
    /// so a manager sees today's payload from it.
    #[tokio::test]
    async fn the_ack_names_where_the_next_chunk_starts() {
        let tmp = TempDir::new().unwrap();
        five_segments(tmp.path()).await;
        let chunk = pull_chunk(tmp.path(), Some(100_000), None, 0, FOLLOWS).await.unwrap();
        let mut ack = serde_json::json!({ "eof": chunk.eof });
        chunk.ack_resume(&mut ack);
        assert_eq!(
            ack["resume"],
            serde_json::json!({ "segment": 1, "offset": 188, "end_segment": 4, "end_offset": 376, "remaining": 1316 }),
        );
        assert!(ack["resume"].to_string().len() < 1024, "the manager refuses a resume past 1 KiB");
        let last = pull_chunk(tmp.path(), Some(100_000), None, 1504, FOLLOWS).await.unwrap();
        assert!(last.eof);
        let mut ack = serde_json::json!({});
        last.ack_resume(&mut ack);
        assert_eq!(ack, serde_json::json!({}));
        ExportChunk { resume: None, ..chunk }.ack_resume(&mut ack);
        assert_eq!(ack, serde_json::json!({}));
    }

    #[tokio::test]
    async fn export_recording_chunk_rejects_inverted_range() {
        // Inverted range short-circuits before any filesystem touch,
        // so the test doesn't need a populated replay root.
        let err = export_recording_chunk("nonexistent", Some(500), Some(100), 0, 188, ChunkCursor::default())
            .await.unwrap_err();
        assert_eq!(err.to_string(), "replay_invalid_range");
    }
}
