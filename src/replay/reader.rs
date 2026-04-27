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

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use tokio::fs::{File, read_dir};
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader, SeekFrom};

use super::ClipInfo;
use super::clips::ClipStore;
use super::index::{InMemoryIndex, IndexEntry};

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
        let dir = super::recording_dir(recording_id);
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Err(anyhow!("replay_clip_not_found"));
        }
        let index = InMemoryIndex::load(&dir.join("index.bin")).await
            .with_context(|| format!("load index for recording {recording_id}"))?;
        let clips = ClipStore::open(recording_id, &dir).await?;
        let start = index.entries.first().cloned()
            .ok_or_else(|| anyhow!("replay_no_index"))?;
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
        r.scope_to_clip(&clip)?;
        Ok(r)
    }

    fn scope_to_clip(&mut self, clip: &ClipInfo) -> Result<()> {
        let start = self.index.find_floor(clip.in_pts_90khz)
            .ok_or_else(|| anyhow!("replay_no_index"))?;
        self.range = PlaybackRange {
            from_pts_90khz: clip.in_pts_90khz,
            to_pts_90khz: clip.out_pts_90khz,
            start_entry: start,
        };
        Ok(())
    }

    /// Reseek to the IDR ≤ `target_pts`. Closes the current segment file
    /// (if open) — the next `read_bundle` call will reopen at the
    /// computed offset.
    pub async fn scrub_to(&mut self, target_pts: u64) -> Result<IndexEntry> {
        let entry = self.index.find_floor(target_pts)
            .ok_or_else(|| anyhow!("replay_no_index"))?;
        self.range.start_entry = entry;
        self.range.from_pts_90khz = entry.pts_90khz;
        self.current_segment = None;
        self.current_file = None;
        Ok(entry)
    }

    /// Read the next bundle of `chunks * 188` bytes from the current
    /// segment, advancing across segment boundaries. Returns `None` on
    /// end-of-range or end-of-recording.
    pub async fn read_bundle(&mut self, chunks: usize) -> Result<Option<Vec<u8>>> {
        let want = TS_PACKET * chunks;
        // Open segment + seek to start_entry.byte_offset on first call.
        if self.current_segment.is_none() {
            let entry = self.range.start_entry;
            let path = self.recording_dir.join(format!("{:06}.ts", entry.segment_id));
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

    async fn advance_segment(&mut self) -> Result<bool> {
        let next_id = self.current_segment.unwrap_or(0).wrapping_add(1);
        // Probe whether next_id exists.
        let path = self.recording_dir.join(format!("{:06}.ts", next_id));
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(false);
        }
        let file = File::open(&path).await?;
        self.current_segment = Some(next_id);
        self.current_file = Some(BufReader::new(file));
        Ok(true)
    }

    /// List segment files actually present on disk for this recording,
    /// sorted by segment id.
    #[allow(dead_code)]
    pub async fn list_segments(&self) -> Result<Vec<u32>> {
        let mut ids = Vec::new();
        let mut dir = read_dir(&self.recording_dir).await?;
        while let Ok(Some(e)) = dir.next_entry().await {
            let name = e.file_name();
            let s = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if let Some(stem) = s.strip_suffix(".ts") {
                if let Ok(id) = stem.parse::<u32>() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
}
