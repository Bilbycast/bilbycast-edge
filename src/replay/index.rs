// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Binary timecode → byte-offset index for a single recording.
//!
//! One fixed-size 24-byte entry per IDR / GOP boundary. Append-only on
//! the writer side; memory-mapped read + binary search on the reader
//! side. The fixed size keeps the on-disk math trivial and lets us
//! mmap-and-scan without a deserialiser.
//!
//! Truncation tolerance: if the file length is not a 24-byte multiple
//! (a SIGKILL between `append` and `flush_and_sync` can leave a partial
//! entry on disk), `IndexWriter::open` and `InMemoryIndex::load` align
//! down to the last valid boundary. The trailing partial entry is
//! discarded — callers see a slightly shorter but coherent index, not
//! an open-error. A full segment-walk-and-rebuild would yield more
//! entries but isn't necessary for correctness in Phase 1; the lost
//! IDR is at most one IDR away from the previous one in the index.
//!
//! # Format
//!
//! Every entry is little-endian, packed without alignment padding:
//!
//! ```text
//! offset  size  field
//! 0       8     pts_90khz       u64
//! 8       4     smpte_tc        u32  (0xFFFFFFFF if unknown)
//! 12      4     segment_id      u32
//! 16      4     byte_offset     u32  (start of IDR in segment)
//! 20      4     flags           u32  (bit0=is_idr, bit1=pcr_disc, bit2=tc_valid)
//! ```
//!
//! Total: 24 bytes per entry. A few thousand entries per hour of HD
//! recording → tens of KB on disk.

use std::path::Path;

use anyhow::{Result, anyhow};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

/// On-disk size of a single index entry, in bytes.
pub const ENTRY_SIZE: usize = 24;

pub mod flag {
    pub const IS_IDR: u32 = 1 << 0;
    pub const PCR_DISCONTINUITY: u32 = 1 << 1;
    pub const SMPTE_TC_VALID: u32 = 1 << 2;
}

/// One in-memory index entry. Mirrors the on-disk layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub pts_90khz: u64,
    pub smpte_tc: u32,
    pub segment_id: u32,
    pub byte_offset: u32,
    pub flags: u32,
}

impl IndexEntry {
    #[allow(dead_code)]
    pub fn is_idr(&self) -> bool {
        self.flags & flag::IS_IDR != 0
    }
    #[allow(dead_code)]
    pub fn smpte_tc_valid(&self) -> bool {
        self.flags & flag::SMPTE_TC_VALID != 0
    }
    /// Pack the entry into a 24-byte buffer, little-endian.
    pub fn to_bytes(self) -> [u8; ENTRY_SIZE] {
        let mut buf = [0u8; ENTRY_SIZE];
        buf[0..8].copy_from_slice(&self.pts_90khz.to_le_bytes());
        buf[8..12].copy_from_slice(&self.smpte_tc.to_le_bytes());
        buf[12..16].copy_from_slice(&self.segment_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.byte_offset.to_le_bytes());
        buf[20..24].copy_from_slice(&self.flags.to_le_bytes());
        buf
    }
    /// Unpack a 24-byte buffer.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        if buf.len() < ENTRY_SIZE {
            return Err(anyhow!("index entry buffer too small"));
        }
        Ok(Self {
            pts_90khz: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            smpte_tc: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            segment_id: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            byte_offset: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
        })
    }
}

/// Append-only writer handle. Holds the open file in append mode; each
/// `append` is one O_APPEND write so concurrent writers (there shouldn't
/// be any, but defence in depth) don't interleave entries.
pub struct IndexWriter {
    file: File,
}

impl IndexWriter {
    /// Open or create the index file for append. If the existing file
    /// is truncated mid-entry (length not a multiple of `ENTRY_SIZE` —
    /// happens after a SIGKILL between `append` and the next
    /// `flush_and_sync`), truncate it down to the last valid 24-byte
    /// boundary so subsequent appends produce a coherent index instead
    /// of corrupting the partial entry. The reader uses the same rule.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Ok(meta) = tokio::fs::metadata(path).await {
            let len = meta.len();
            let aligned = len - (len % ENTRY_SIZE as u64);
            if aligned != len {
                let f = OpenOptions::new().write(true).open(path).await?;
                f.set_len(aligned).await?;
                drop(f);
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self { file })
    }

    /// Append a single entry.
    pub async fn append(&mut self, entry: IndexEntry) -> Result<()> {
        let buf = entry.to_bytes();
        self.file.write_all(&buf).await?;
        Ok(())
    }

    /// Flush + fsync. Call on segment roll boundaries.
    pub async fn flush_and_sync(&mut self) -> Result<()> {
        self.file.flush().await?;
        self.file.sync_all().await?;
        Ok(())
    }
}

/// In-memory index — loaded once on reader open, used for binary search.
#[derive(Debug, Clone, Default)]
pub struct InMemoryIndex {
    pub entries: Vec<IndexEntry>,
}

impl InMemoryIndex {
    /// Load the entire index. Returns an empty index if the file
    /// doesn't exist. If the file is truncated mid-entry (length not a
    /// multiple of `ENTRY_SIZE`) we read the largest 24-byte-aligned
    /// prefix and ignore the trailing partial entry — same rule
    /// `IndexWriter::open` applies on the writer side, so a SIGKILL
    /// between `append` and `flush_and_sync` is recoverable without
    /// operator intervention.
    pub async fn load(path: &Path) -> Result<Self> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(Self::default());
        }
        let mut file = File::open(path).await?;
        let len = file.metadata().await?.len();
        if len == 0 {
            return Ok(Self::default());
        }
        let aligned_len = len - (len % ENTRY_SIZE as u64);
        if aligned_len == 0 {
            return Ok(Self::default());
        }
        let count = (aligned_len as usize) / ENTRY_SIZE;
        let mut buf = vec![0u8; aligned_len as usize];
        file.seek(SeekFrom::Start(0)).await?;
        file.read_exact(&mut buf).await?;
        let mut entries = Vec::with_capacity(count);
        for chunk in buf.chunks_exact(ENTRY_SIZE) {
            entries.push(IndexEntry::from_bytes(chunk)?);
        }
        Ok(Self { entries })
    }

    /// Return the entry whose PTS is the largest ≤ `target_pts`. This is
    /// the IDR you'd seek to for a scrub — playback resumes at a clean
    /// GOP boundary. Falls back to the first entry if `target_pts` is
    /// before everything in the index.
    pub fn find_floor(&self, target_pts: u64) -> Option<IndexEntry> {
        if self.entries.is_empty() {
            return None;
        }
        // Binary search on PTS. Entries are append-order, which is also
        // PTS-monotonic in non-pathological streams (PCR discontinuities
        // are flagged but don't reset the in-memory PTS — the writer
        // accumulates a 64-bit pseudo-PTS that monotonically advances).
        //
        // Except on an index a **shipped binary already corrupted**. Before the
        // resume landed, every writer restart set the counter back to zero, so
        // any recording that restarted holds a high-PTS run followed by a low
        // one — and retention prunes `NNNNNN.ts` files only, never index
        // entries, so that state is permanent. `binary_search_by_key` is
        // documented as returning an unspecified result on unsorted input: a
        // scrub or a clip export against such a file gets an arbitrary entry,
        // which reads as the wrong media or a segment that no longer exists.
        //
        // The scan is O(n) on a file that is microseconds to read either way
        // (24 bytes per IDR, ~1 MB a day), and it is only reached on data the
        // binary search has no defined answer for.
        if !self.is_sorted() {
            return self.find_floor_linear(target_pts);
        }
        let idx = match self.entries.binary_search_by_key(&target_pts, |e| e.pts_90khz) {
            Ok(i) => i,
            Err(0) => return Some(self.entries[0]),
            Err(i) => i - 1,
        };
        Some(self.entries[idx])
    }

    /// Is the index PTS-monotonic, as every writer since the resume landed
    /// leaves it?
    fn is_sorted(&self) -> bool {
        self.entries.windows(2).all(|w| w[0].pts_90khz <= w[1].pts_90khz)
    }

    /// The largest entry at or below `target_pts`, without assuming order.
    ///
    /// Clamps at the low end the same way the binary search does — a target
    /// before everything resolves to the earliest entry — so the two agree on
    /// a sorted index and only their cost differs.
    fn find_floor_linear(&self, target_pts: u64) -> Option<IndexEntry> {
        let best = self
            .entries
            .iter()
            .filter(|e| e.pts_90khz <= target_pts)
            .max_by_key(|e| e.pts_90khz);
        match best {
            Some(e) => Some(*e),
            None => self.entries.iter().min_by_key(|e| e.pts_90khz).copied(),
        }
    }

    /// Does a PTS range contain a break in the media's own timeline?
    ///
    /// The index stays monotonic across a writer restart — the counter is
    /// resumed — but the TS underneath does not: its PCR begins again with the
    /// process. An entry carrying `PCR_DISCONTINUITY` marks that join. Cutting
    /// exactly across one muxes two timelines into a single track, which is how
    /// 30 seconds of video came out declaring a duration of seven hours.
    ///
    /// The first entry cannot be a join by this definition — there is nothing
    /// before it for the media to be discontinuous *with* — so it is skipped.
    pub fn spans_discontinuity(&self, from_pts: u64, to_pts: u64) -> bool {
        self.entries
            .iter()
            .skip(1)
            .any(|e| e.flags & flag::PCR_DISCONTINUITY != 0
                && e.pts_90khz > from_pts
                && e.pts_90khz < to_pts)
    }

    /// The first indexed PTS strictly after `pts`, if there is one.
    ///
    /// The exporter ends a range at `find_floor(to)` — the random-access point
    /// at or before the end — so a range asking for exactly the wanted window
    /// comes back a GOP short at the tail. Asking instead for the entry just
    /// past the end makes the floor land on it, and the clip covers what was
    /// requested. `None` when nothing follows, which means the recording ends
    /// inside the window and there is no more to include.
    pub fn first_after(&self, pts: u64) -> Option<u64> {
        self.entries
            .iter()
            .find(|e| e.pts_90khz > pts)
            .map(|e| e.pts_90khz)
    }

    /// First and last PTS in the index, if any.
    ///
    /// Positional rather than min/max, and deliberately: on every index a
    /// current writer produces those are the same thing, and on one a shipped
    /// binary already corrupted the *positional* pair is the honest answer —
    /// it describes where the file begins and ends, which is what a reader
    /// resuming or bounding it needs. Taking the maximum instead would pair a
    /// wall-clock anchor with a tick from a different run.
    pub fn span(&self) -> Option<(u64, u64)> {
        let first = self.entries.first()?;
        let last = self.entries.last()?;
        Some((first.pts_90khz, last.pts_90khz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(pts: u64, seg: u32, off: u32) -> IndexEntry {
        IndexEntry {
            pts_90khz: pts,
            smpte_tc: 0xFFFFFFFF,
            segment_id: seg,
            byte_offset: off,
            flags: flag::IS_IDR,
        }
    }

    /// A clip must cover the window it asked for, not stop a GOP short.
    ///
    /// The exporter bounds both ends with `find_floor`. At the tail that
    /// rounds inward, so a range naming the exact end loses everything back to
    /// the previous random-access point — 26.35s of a 30s request on the rig.
    /// Naming the next point instead makes that floor land on it.
    #[test]
    fn the_point_after_the_end_is_what_covers_the_window() {
        let mut idx = InMemoryIndex::default();
        for i in 0..5u64 {
            idx.entries.push(make_entry(i * 180_000, 0, (i * 1000) as u32));
        }
        // 0, 2s, 4s, 6s, 8s at 90kHz.

        // An end falling between points takes the next one, so the GOP that
        // contains the end is included rather than dropped.
        assert_eq!(idx.first_after(270_000), Some(360_000), "the tail GOP was dropped");
        // An end landing exactly on a point still needs the one after it:
        // `find_floor` would otherwise stop at that point, excluding its GOP.
        assert_eq!(idx.first_after(360_000), Some(540_000));
        // Past the end of the recording there is nothing more to include, and
        // the caller keeps the end it asked for.
        assert_eq!(idx.first_after(720_000), None);
        assert_eq!(idx.first_after(999_999), None);
    }

    /// A clip must not be cut across a break in the media's timeline.
    ///
    /// The index is monotonic across a writer restart because the counter is
    /// resumed, so nothing about the numbers says "the media restarted here".
    /// The flag does. Muxing across one produced a 30-second clip that
    /// declared a duration of 26,884 seconds — playable, and wrong in a way
    /// no counter reported.
    #[test]
    fn a_range_containing_a_restart_is_reported_as_spanning_one() {
        let mut idx = InMemoryIndex::default();
        idx.entries.push(make_entry(1_000, 0, 0));
        idx.entries.push(make_entry(90_000, 0, 100));
        let mut joined = make_entry(180_000, 1, 0);
        joined.flags |= flag::PCR_DISCONTINUITY;
        idx.entries.push(joined);
        idx.entries.push(make_entry(270_000, 1, 100));

        assert!(
            idx.spans_discontinuity(90_000, 270_000),
            "a range straddling the join must be refused the exact path"
        );
        // Either side of it on its own is fine.
        assert!(!idx.spans_discontinuity(1_000, 90_000), "the range before the join is clean");
        assert!(!idx.spans_discontinuity(180_000, 270_000), "the range after the join is clean");

        // The very first entry is not a join: nothing precedes it. A recording
        // whose opening frame were treated as one could never cut exactly.
        let mut first_flagged = InMemoryIndex::default();
        let mut e0 = make_entry(1_000, 0, 0);
        e0.flags |= flag::PCR_DISCONTINUITY;
        first_flagged.entries.push(e0);
        first_flagged.entries.push(make_entry(90_000, 0, 100));
        assert!(
            !first_flagged.spans_discontinuity(0, 90_000),
            "the first entry must not lock the whole recording out of exact cutting"
        );
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let e = IndexEntry {
            pts_90khz: 0x0123_4567_89AB_CDEF,
            smpte_tc: 0x01020304,
            segment_id: 42,
            byte_offset: 1880,
            flags: flag::IS_IDR | flag::SMPTE_TC_VALID,
        };
        let buf = e.to_bytes();
        let back = IndexEntry::from_bytes(&buf).unwrap();
        assert_eq!(e, back);
    }

    #[tokio::test]
    async fn append_load_query() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.bin");
        let mut writer = IndexWriter::open(&path).await.unwrap();
        for i in 0..10u64 {
            // 1-second cadence at 90 kHz
            let entry = make_entry(i * 90_000, (i / 4) as u32, (i as u32) * 1880);
            writer.append(entry).await.unwrap();
        }
        writer.flush_and_sync().await.unwrap();
        drop(writer);

        let idx = InMemoryIndex::load(&path).await.unwrap();
        assert_eq!(idx.entries.len(), 10);

        // Floor on exact PTS hits.
        let hit = idx.find_floor(5 * 90_000).unwrap();
        assert_eq!(hit.pts_90khz, 5 * 90_000);
        assert_eq!(hit.segment_id, 1);

        // Floor between PTSes returns the previous entry.
        let between = idx.find_floor(5 * 90_000 + 45_000).unwrap();
        assert_eq!(between.pts_90khz, 5 * 90_000);

        // Before the head of the index returns the head.
        let before = idx.find_floor(0).unwrap();
        assert_eq!(before.pts_90khz, 0);

        // After the tail returns the tail.
        let after = idx.find_floor(u64::MAX).unwrap();
        assert_eq!(after.pts_90khz, 9 * 90_000);

        // Span report.
        assert_eq!(idx.span(), Some((0, 9 * 90_000)));
    }

    #[tokio::test]
    async fn truncated_tail_is_recovered() {
        // Truncated mid-entry → load drops the partial entry and
        // returns the aligned prefix. Symmetric with `IndexWriter::open`
        // which `set_len`s the file to the same boundary on next start.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.bin");
        tokio::fs::write(&path, vec![0u8; ENTRY_SIZE * 3 + 5]).await.unwrap();
        let idx = InMemoryIndex::load(&path).await.unwrap();
        assert_eq!(idx.entries.len(), 3);

        // Empty file should also load as empty.
        let empty = tmp.path().join("empty.bin");
        tokio::fs::write(&empty, vec![0u8; 7]).await.unwrap();
        let idx = InMemoryIndex::load(&empty).await.unwrap();
        assert_eq!(idx.entries.len(), 0);

        // IndexWriter::open should align an existing partial file in place.
        let path2 = tmp.path().join("partial.bin");
        tokio::fs::write(&path2, vec![1u8; ENTRY_SIZE * 2 + 9]).await.unwrap();
        let _w = IndexWriter::open(&path2).await.unwrap();
        let len = tokio::fs::metadata(&path2).await.unwrap().len();
        assert_eq!(len, (ENTRY_SIZE * 2) as u64);
    }

    /// An index a shipped binary already corrupted still resolves a floor.
    ///
    /// Before the counter was resumed, every writer restart set it back to
    /// zero, so a recording that restarted holds a high-PTS run followed by a
    /// low one — and retention prunes `NNNNNN.ts` files only, never index
    /// entries, so that state is permanent for the life of the recording. Rust
    /// documents `binary_search_by_key` as returning an unspecified result on
    /// unsorted input, so a scrub or a clip export against such a file got an
    /// arbitrary entry: the wrong media, or a segment that no longer exists.
    #[test]
    fn a_pre_corrupted_index_still_resolves_a_floor() {
        // Run A: 86_400 .. 86_400 + 4 * 90_000. Run B restarts at zero.
        let mut idx = InMemoryIndex::default();
        for i in 0..5u64 {
            idx.entries.push(make_entry(86_400 + i * 90_000, i as u32, 0));
        }
        for i in 0..5u64 {
            idx.entries.push(make_entry(i * 90_000, 5 + i as u32, 0));
        }
        assert!(!idx.is_sorted(), "the fixture is supposed to be out of order");

        // A target inside run A resolves to run A's frame, not to whatever the
        // binary search happened to land on.
        let got = idx.find_floor(86_400 + 2 * 90_000 + 10).expect("a floor");
        assert_eq!(got.pts_90khz, 86_400 + 2 * 90_000);
        assert_eq!(got.segment_id, 2);

        // And one inside run B resolves to run B's.
        let got = idx.find_floor(3 * 90_000 + 10).expect("a floor");
        assert_eq!(got.pts_90khz, 3 * 90_000);
        assert_eq!(got.segment_id, 8);

        // Before everything clamps to the earliest, as the sorted path does.
        assert_eq!(idx.find_floor(0).expect("a floor").pts_90khz, 0);

        // A sorted index answers identically either way.
        let mut sorted = InMemoryIndex::default();
        for i in 0..5u64 {
            sorted.entries.push(make_entry(i * 90_000, i as u32, 0));
        }
        assert!(sorted.is_sorted());
        for t in [0, 1, 90_001, 4 * 90_000, 10 * 90_000] {
            assert_eq!(
                sorted.find_floor(t).map(|e| e.pts_90khz),
                sorted.find_floor_linear(t).map(|e| e.pts_90khz),
                "the two searches disagree at {t}"
            );
        }
    }
}
