// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Segment reader for replay playback.
//!
//! Iterates rolling TS segments inside a recording directory, optionally
//! constrained to a clip's `[in_pts, out_pts]` range. Yields 188-byte
//! TS packets in 7-packet bundles (1316 bytes) so the rest of the
//! engine sees the same shape it does from a live input.
//!
//! Phase 1 only supports forward playback at 1.0x — the
//! [`paced_replayer`] module handles PTS pacing on top of this reader.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader, SeekFrom};

use super::ClipInfo;
use super::clips::ClipStore;
use super::index::{InMemoryIndex, IndexEntry};
use super::{segment_path, segments_on_disk, segments_pending};

/// 188-byte MPEG-TS packet size.
const TS_PACKET: usize = 188;

/// Resolved playback range. Either the entire recording (`whole`) or a
/// clip's slice. Phase-1 callers materialise this with
/// [`Reader::open_recording`] / [`Reader::open_clip`].
#[derive(Debug, Clone)]
pub struct PlaybackRange {
    /// Inclusive lower bound (90 kHz PTS) — start of playback.
    pub from_pts_90khz: u64,
    /// Inclusive upper bound. `u64::MAX` = play to end of recording.
    pub to_pts_90khz: u64,
    /// Index entry the reader landed on for the first IDR.
    pub start_entry: IndexEntry,
}

/// Stateful segment reader. Holds the recording directory + the loaded
/// in-memory index + the current open file. Single-threaded — playback
/// is one consumer per replay input task.
pub struct Reader {
    pub recording_dir: PathBuf,
    pub index: InMemoryIndex,
    pub clips: ClipStore,
    pub current_segment: Option<u32>,
    pub current_file: Option<BufReader<File>>,
    pub range: PlaybackRange,
}

impl Reader {
    /// Open the entire recording (no clip range).
    pub async fn open_recording(recording_id: &str) -> Result<Self> {
        Self::open_recording_at(recording_id, super::recording_dir(recording_id)).await
    }

    /// [`Self::open_recording`] in a named directory rather than the replay
    /// root's, so a test can read a recording of its own.
    pub(crate) async fn open_recording_at(recording_id: &str, dir: PathBuf) -> Result<Self> {
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Err(anyhow!("replay_clip_not_found"));
        }
        let index = InMemoryIndex::load(&dir.join("index.bin")).await
            .with_context(|| format!("load index for recording {recording_id}"))?;
        let clips = ClipStore::open(recording_id, &dir).await?;
        let start = recording_start(&index, &dir).await?;
        Ok(Self {
            recording_dir: dir,
            index,
            clips,
            current_segment: None,
            current_file: None,
            range: PlaybackRange {
                from_pts_90khz: start.pts_90khz,
                to_pts_90khz: u64::MAX,
                start_entry: start,
            },
        })
    }

    /// Open a recording scoped to a specific clip's range.
    pub async fn open_clip(recording_id: &str, clip_id: &str) -> Result<Self> {
        let mut r = Self::open_recording(recording_id).await?;
        let clip = r.clips.get(clip_id).await
            .ok_or_else(|| anyhow!("replay_clip_not_found"))?;
        r.scope_to_clip(&clip).await?;
        Ok(r)
    }

    async fn scope_to_clip(&mut self, clip: &ClipInfo) -> Result<()> {
        let start = self.playable_from(clip.in_pts_90khz, clip.out_pts_90khz).await?;
        self.range = PlaybackRange {
            // Where it really starts, when the in-point's media is gone and
            // playback lands after it.
            from_pts_90khz: clip.in_pts_90khz.max(start.pts_90khz),
            to_pts_90khz: clip.out_pts_90khz,
            start_entry: start,
        };
        Ok(())
    }

    /// Reseek to the IDR ≤ `target_pts` — or, when the media there is no
    /// longer on disk, to the first one after it that is (see
    /// [`Self::playable_from`]). Closes the current segment file (if open) —
    /// the next `read_bundle` call will reopen at the computed offset.
    pub async fn scrub_to(&mut self, target_pts: u64) -> Result<IndexEntry> {
        let entry = self.playable_from(target_pts, self.range.to_pts_90khz).await?;
        self.range.start_entry = entry;
        self.range.from_pts_90khz = entry.pts_90khz;
        self.current_segment = None;
        self.current_file = None;
        Ok(entry)
    }

    /// The index entry playback of `target` starts at: the random-access
    /// point at or before it, as ever, while its segment is on disk; when it
    /// is not, the first random-access point after it whose segment is — so
    /// long as that comes before `before`, the end of the range being played.
    ///
    /// The index goes on naming segments the disk no longer holds: those
    /// retention removed, and those a crash or a failed close lost (see
    /// "Segment ids" in `replay::writer`). A cue or scrub into one resolved
    /// to the entry naming it and failed when playback tried to open the file.
    ///
    /// Landing after the hole rather than refusing: it is where playback
    /// started before the hole arrives anyway, since a reader steps over a
    /// missing segment to the next one on disk ([`Self::advance_segment`]),
    /// and the acknowledgement names the entry landed on, so the operator
    /// sees how far it moved. Not on the last random-access point before the
    /// hole, which may be minutes of media the operator did not ask for. Past
    /// the end of the range there is nothing of it left to play, and that is
    /// refused with `replay_segment_missing` — or, when what comes next in
    /// range is a segment the live writer has yet to put in place (see
    /// [`super::SegmentState`]), with `replay_segment_not_ready`, since the
    /// same request plays once it has closed. A recording directory that has
    /// gone altogether has no media left in it either.
    async fn playable_from(&self, target: u64, before: u64) -> Result<IndexEntry> {
        let floor = self.index.find_floor(target)
            .ok_or_else(|| anyhow!("replay_no_index"))?;
        if tokio::fs::try_exists(segment_path(&self.recording_dir, floor.segment_id)).await.unwrap_or(false) {
            return Ok(floor);
        }
        // Pending first: a segment the closer renames between the two listings
        // is then in one or the other.
        let pending = segments_pending(&self.recording_dir).await;
        let on_disk = finished_segments(&self.recording_dir).await?;
        let in_range = |list: &[u32]| {
            self.index
                .next_present(floor.pts_90khz, |id| list.binary_search(&id).is_ok())
                .filter(|next| next.pts_90khz < before)
        };
        if let Some(next) = in_range(&on_disk) {
            return Ok(next);
        }
        if in_range(&pending).is_some() {
            return Err(anyhow!(
                "replay_segment_not_ready: segment {:06}.ts, which index.bin names at PTS {}, \
                 is still being recorded — it can be played once it has closed",
                floor.segment_id, floor.pts_90khz,
            ));
        }
        Err(anyhow!(
            "replay_segment_missing: segment {:06}.ts, which index.bin names at PTS {}, \
             is not on disk, and no recorded media follows it in range",
            floor.segment_id, floor.pts_90khz,
        ))
    }

    /// Read the next bundle of `chunks * 188` bytes from the current
    /// segment, advancing across segment boundaries. Returns `None` on
    /// end-of-range or end-of-recording.
    pub async fn read_bundle(&mut self, chunks: usize) -> Result<Option<Vec<u8>>> {
        let want = TS_PACKET * chunks;
        // Open segment + seek to start_entry.byte_offset on first call.
        if self.current_segment.is_none() {
            let entry = self.range.start_entry;
            let path = segment_path(&self.recording_dir, entry.segment_id);
            let mut file = File::open(&path).await
                .with_context(|| format!("open segment {}", path.display()))?;
            file.seek(SeekFrom::Start(entry.byte_offset as u64)).await?;
            self.current_segment = Some(entry.segment_id);
            self.current_file = Some(BufReader::new(file));
        }
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            let file = self.current_file.as_mut()
                .ok_or_else(|| anyhow!("reader: current_file unset after open path"))?;
            let n = file.read(&mut buf[filled..]).await?;
            if n == 0 {
                // Segment EOF — advance to next.
                if !self.advance_segment().await? {
                    if filled == 0 {
                        return Ok(None);
                    }
                    buf.truncate(filled);
                    break;
                }
            } else {
                filled += n;
            }
        }
        Ok(Some(buf))
    }

    /// Move on to the segment after the current one: the next id or, past a
    /// gap in the ids, the next segment on disk. `false` at the end of the
    /// recording.
    ///
    /// A gap is not the end. A crash leaves one where the segments it lost
    /// were, and so does a close that failed (see "Segment ids" in
    /// `replay::writer`); stopping at the first missing id ended the playback
    /// of everything recorded after it. Only a miss lists the directory, so
    /// the common case is still one probe.
    async fn advance_segment(&mut self) -> Result<bool> {
        let current = self.current_segment.unwrap_or(0);
        let mut next_id = current.wrapping_add(1);
        let mut path = segment_path(&self.recording_dir, next_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let after = segments_on_disk(&self.recording_dir).await.unwrap_or_default().into_iter().find(|&id| id > current);
            let Some(id) = after else {
                return Ok(false);
            };
            next_id = id;
            path = segment_path(&self.recording_dir, next_id);
        }
        let file = File::open(&path).await?;
        self.current_segment = Some(next_id);
        self.current_file = Some(BufReader::new(file));
        Ok(true)
    }
}

/// Where a replay — or an export — of the whole recording in `dir` starts: the
/// first index entry whose segment is still on disk.
///
/// The first entry outright named a file retention had removed, on every
/// recording old enough to have been pruned, and a whole-recording replay then
/// failed at its first read.
///
/// A recording whose first segment is still open has nothing to start on yet,
/// and says so (`replay_segment_not_ready`) rather than that it was lost.
pub(crate) async fn recording_start(index: &InMemoryIndex, dir: &Path) -> Result<IndexEntry> {
    if index.entries.is_empty() {
        return Err(anyhow!("replay_no_index"));
    }
    let pending = segments_pending(dir).await;
    let on_disk = segments_on_disk(dir).await?;
    if let Some(start) = index.first_present(|id| on_disk.binary_search(&id).is_ok()) {
        return Ok(start);
    }
    if index.first_present(|id| pending.binary_search(&id).is_ok()).is_some() {
        return Err(anyhow!(
            "replay_segment_not_ready: the recording's first segment is still being recorded"
        ));
    }
    Err(anyhow!("replay_segment_missing: no segment index.bin names is on disk"))
}

/// The finished segments in a recording directory, none when the directory
/// itself has gone — a recording deleted under a replay of it.
async fn finished_segments(dir: &Path) -> Result<Vec<u32>> {
    match segments_on_disk(dir).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        listed => listed.with_context(|| format!("list segments in {}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::index::{IndexWriter, flag};

    /// A gap in the segment ids is stepped over, not taken for the end: what
    /// was recorded after a crash, or after a segment whose close failed, is
    /// still played.
    #[tokio::test]
    async fn playback_steps_over_a_gap_in_the_segment_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("rec");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // Segments 0, 1, 4 and 5: 2 and 3 were lost.
        let mut expected = Vec::new();
        for id in [0u32, 1, 4, 5] {
            let seg: Vec<u8> = (0..2 * TS_PACKET).map(|i| (id as usize * 31 + i) as u8).collect();
            tokio::fs::write(dir.join(format!("{id:06}.ts")), &seg).await.unwrap();
            expected.extend_from_slice(&seg);
        }
        let mut index = IndexWriter::open(&dir.join("index.bin")).await.unwrap();
        index
            .append(IndexEntry { pts_90khz: 0, smpte_tc: 0, segment_id: 0, byte_offset: 0, flags: flag::IS_IDR })
            .await
            .unwrap();
        index.flush_and_sync().await.unwrap();

        let mut reader = Reader::open_recording_at("rec", dir).await.unwrap();
        let mut played = Vec::new();
        while let Some(bundle) = reader.read_bundle(1).await.unwrap() {
            played.extend_from_slice(&bundle);
        }
        assert_eq!(played.len(), expected.len(), "playback stopped at the gap");
        assert_eq!(played, expected);
    }

    /// A recording in `parent/rec` of two-packet segments `0..count`, each
    /// indexed at its start, at 90 000 ticks a segment — with the segments
    /// `gone` then removed, as retention or a crash leaves them. Returns each
    /// segment's bytes, by id.
    async fn recording(parent: &Path, count: u32, gone: &[u32]) -> (PathBuf, Vec<Vec<u8>>) {
        let dir = parent.join("rec");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let mut index = IndexWriter::open(&dir.join("index.bin")).await.unwrap();
        let mut segments = Vec::new();
        for id in 0..count {
            let seg: Vec<u8> = (0..2 * TS_PACKET).map(|i| (id as usize * 31 + i) as u8).collect();
            tokio::fs::write(segment_path(&dir, id), &seg).await.unwrap();
            segments.push(seg);
            index
                .append(IndexEntry {
                    pts_90khz: id as u64 * 90_000,
                    smpte_tc: 0,
                    segment_id: id,
                    byte_offset: 0,
                    flags: flag::IS_IDR,
                })
                .await
                .unwrap();
        }
        index.flush_and_sync().await.unwrap();
        for &id in gone {
            tokio::fs::remove_file(segment_path(&dir, id)).await.unwrap();
        }
        (dir, segments)
    }

    async fn play(reader: &mut Reader) -> Vec<u8> {
        let mut played = Vec::new();
        while let Some(bundle) = reader.read_bundle(1).await.unwrap() {
            played.extend_from_slice(&bundle);
        }
        played
    }

    /// Retention removes the oldest segments and leaves their index entries,
    /// so a replay of the whole recording starts at the first segment it
    /// kept. It started at the first entry, and failed to open the file it
    /// named on every recording old enough to have been pruned.
    #[tokio::test]
    async fn a_whole_recording_replay_starts_at_the_first_segment_retention_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, segments) = recording(tmp.path(), 5, &[0, 1]).await;
        let mut reader = Reader::open_recording_at("rec", dir.clone()).await.unwrap();
        assert_eq!(reader.range.start_entry.segment_id, 2);
        assert_eq!(play(&mut reader).await, segments[2..].concat());

        // Nothing left on disk at all: a named refusal, not a failed read.
        let (dir, _) = recording(&tmp.path().join("all"), 3, &[0, 1, 2]).await;
        let err = Reader::open_recording_at("rec", dir).await.err().expect("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
    }

    /// A cue or scrub into a window a crash lost lands on the first media
    /// after it still on disk, and says so; it resolved to the entry naming
    /// the lost segment and failed when playback opened the file.
    #[tokio::test]
    async fn a_scrub_into_a_lost_window_lands_on_the_next_media_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        // Segments 2 and 3 lost, 0, 1 and 4 kept.
        let (dir, segments) = recording(tmp.path(), 5, &[2, 3]).await;
        let mut reader = Reader::open_recording_at("rec", dir).await.unwrap();

        // Media still there: the random-access point at or before, as ever.
        let landed = reader.scrub_to(100_000).await.unwrap();
        assert_eq!((landed.segment_id, landed.pts_90khz), (1, 90_000));

        // Into the hole: after it, onto segment 4, and playback reads it.
        let landed = reader.scrub_to(200_000).await.unwrap();
        assert_eq!((landed.segment_id, landed.pts_90khz), (4, 360_000));
        assert_eq!(reader.range.from_pts_90khz, 360_000);
        assert_eq!(play(&mut reader).await, segments[4]);

        // A clip lying wholly in the hole has nothing left to play.
        let clip = |in_pts, out_pts| ClipInfo {
            id: "c".into(),
            recording_id: "rec".into(),
            name: "c".into(),
            description: None,
            in_pts_90khz: in_pts,
            out_pts_90khz: out_pts,
            smpte_in: None,
            smpte_out: None,
            created_at_unix: 0,
            created_by: None,
            tags: Vec::new(),
        };
        let err = reader.scope_to_clip(&clip(190_000, 300_000)).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
        // One reaching past it starts where the media resumes.
        reader.scope_to_clip(&clip(190_000, 400_000)).await.unwrap();
        assert_eq!(reader.range.start_entry.segment_id, 4);
        assert_eq!(reader.range.from_pts_90khz, 360_000);
    }

    /// A scrub onto a segment the live writer has yet to put in place is
    /// refused as not ready, not as lost — and so is a replay of a recording
    /// whose first segment is still open. With no writer at work, the same
    /// staging file is a crash's leftover.
    #[tokio::test]
    async fn a_scrub_onto_a_segment_still_being_recorded_is_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, _) = recording(tmp.path(), 4, &[]).await;
        tokio::fs::create_dir(dir.join(".tmp")).await.unwrap();
        for id in [2, 3] {
            tokio::fs::rename(segment_path(&dir, id), segment_path(&dir.join(".tmp"), id)).await.unwrap();
        }
        let mut reader = Reader::open_recording_at("rec", dir.clone()).await.unwrap();
        let writer = crate::replay::writer::hold_as_writer(&dir);
        let err = reader.scrub_to(280_000).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_not_ready"), "{err}");
        drop(writer);
        let err = reader.scrub_to(280_000).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");

        let (dir, _) = recording(&tmp.path().join("fresh"), 1, &[]).await;
        tokio::fs::create_dir(dir.join(".tmp")).await.unwrap();
        tokio::fs::rename(segment_path(&dir, 0), segment_path(&dir.join(".tmp"), 0)).await.unwrap();
        let _writer = crate::replay::writer::hold_as_writer(&dir);
        let err = Reader::open_recording_at("rec", dir).await.err().expect("refused");
        assert!(err.to_string().starts_with("replay_segment_not_ready"), "{err}");
    }

    /// A crash that lost the tail leaves nothing after the hole to land on.
    #[tokio::test]
    async fn a_scrub_into_a_lost_tail_is_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, _) = recording(tmp.path(), 4, &[2, 3]).await;
        let mut reader = Reader::open_recording_at("rec", dir).await.unwrap();
        let err = reader.scrub_to(280_000).await.expect_err("refused");
        assert!(err.to_string().starts_with("replay_segment_missing"), "{err}");
    }
}
