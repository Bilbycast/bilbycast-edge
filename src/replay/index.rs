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
    pub fn to_bytes(&self) -> [u8; ENTRY_SIZE] {
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
        let idx = match self.entries.binary_search_by_key(&target_pts, |e| e.pts_90khz) {
            Ok(i) => i,
            Err(0) => return Some(self.entries[0]),
            Err(i) => i - 1,
        };
        Some(self.entries[idx])
    }

    /// First and last PTS in the index, if any.
    #[allow(dead_code)]
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
}
