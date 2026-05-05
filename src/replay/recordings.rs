// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Library-style enumeration + deletion of on-disk recordings.
//!
//! The replay writer ([`super::writer`]) handles the live recording's
//! lifecycle while a flow has it armed. Once a flow's recording is
//! turned off (or the flow is deleted entirely), the segments on disk
//! still exist under `<replay_root>/<recording_id>/` — they keep
//! occupying disk and they keep being playable via the orphan-recovery
//! `list_clips { recording_id }` path. This module exposes those
//! recordings to the manager UI's "Recordings" library tab so operators
//! can browse, export, or delete them without re-arming the writer.

use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::clips::ClipStore;
use super::index::InMemoryIndex;

/// One entry in the `list_recordings` response. Fields chosen for the
/// manager UI's Recordings table — recording_id, owning flow / armed
/// state / orphan classification are decorated by the dispatcher; the
/// purely on-disk view lives here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSummary {
    /// Directory name under `<replay_root>/`.
    pub recording_id: String,
    /// Number of finalized `<NNNNNN>.ts` segments on disk. Excludes
    /// `.tmp/` partial writes — those don't count as durable.
    pub segment_count: u32,
    /// Sum of segment file sizes in bytes. Source of truth for the
    /// per-recording disk meter; the writer's `bytes_written` counter
    /// is lifetime-cumulative and includes pruned data.
    pub total_bytes: u64,
    /// First IDR's PTS (90 kHz) from `index.bin`, or `None` if the
    /// index is empty — fresh recording with no IDR yet.
    pub first_pts_90khz: Option<u64>,
    /// Last IDR's PTS. Subtract from `first_pts_90khz` for an
    /// approximate playable-duration display.
    pub last_pts_90khz: Option<u64>,
    /// Recording-level created-at from `recording.json`. `None` when the
    /// meta file is missing or unreadable (pre-cleanup orphan).
    pub created_at_unix: Option<u64>,
    /// `mtime` of the most recent segment, in Unix seconds. Drives the
    /// "Last activity" column in the UI.
    pub last_modified_unix: u64,
    /// Number of clips persisted in `clips.json`. `0` for recordings
    /// the operator never marked.
    pub clip_count: u32,
}

/// Walk the replay root and produce one [`RecordingSummary`] per
/// recording directory. Errors from individual recordings (corrupt
/// meta, unreadable index) are swallowed in favour of returning a
/// best-effort row — the operator can still see the directory exists
/// and delete it, even if its index is corrupt.
pub async fn enumerate(replay_root: &Path) -> Vec<RecordingSummary> {
    let mut out = Vec::new();
    let mut dir = match fs::read_dir(replay_root).await {
        Ok(d) => d,
        Err(_) => return out,
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
        let Some(file_type) = entry.file_type().await.ok() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let summary = summarize_recording(&name, &path).await;
        out.push(summary);
    }
    out.sort_by(|a, b| b.last_modified_unix.cmp(&a.last_modified_unix));
    out
}

/// Lightweight summary used by the Prometheus `/metrics` scrape path —
/// counts segments + sums their byte sizes, but skips the
/// `index.bin` and `clips.json` reads. Single `read_dir` per
/// recording so a 15 s scrape doesn't pay for the full `enumerate()`
/// payload when the manager UI is the only thing that reads PTS span
/// + clip count.
#[derive(Debug, Clone)]
pub struct LiteRecordingSummary {
    pub recording_id: String,
    /// Reserved for future per-recording-segment-count gauges; the
    /// initial /metrics surface only emits aggregate counts.
    #[allow(dead_code)]
    pub segment_count: u32,
    pub total_bytes: u64,
}

pub async fn enumerate_lite(replay_root: &Path) -> Vec<LiteRecordingSummary> {
    let mut out = Vec::new();
    let Ok(mut dir) = fs::read_dir(replay_root).await else {
        return out;
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
        let Some(file_type) = entry.file_type().await.ok() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let mut segment_count: u32 = 0;
        let mut total_bytes: u64 = 0;
        if let Ok(mut entries) = fs::read_dir(entry.path()).await {
            while let Ok(Some(e)) = entries.next_entry().await {
                let n = e.file_name();
                let Some(s) = n.to_str() else { continue };
                let Some(stem) = s.strip_suffix(".ts") else { continue };
                if stem.parse::<u32>().is_err() {
                    continue;
                }
                if let Ok(meta) = e.metadata().await {
                    segment_count = segment_count.saturating_add(1);
                    total_bytes = total_bytes.saturating_add(meta.len());
                }
            }
        }
        out.push(LiteRecordingSummary {
            recording_id: name,
            segment_count,
            total_bytes,
        });
    }
    out
}

async fn summarize_recording(recording_id: &str, dir: &Path) -> RecordingSummary {
    let mut segment_count: u32 = 0;
    let mut total_bytes: u64 = 0;
    let mut last_modified_unix: u64 = 0;

    if let Ok(mut entries) = fs::read_dir(dir).await {
        while let Ok(Some(e)) = entries.next_entry().await {
            let name = e.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Some(stem) = name_str.strip_suffix(".ts") else {
                continue;
            };
            if stem.parse::<u32>().is_err() {
                continue;
            }
            if let Ok(meta) = e.metadata().await {
                segment_count = segment_count.saturating_add(1);
                total_bytes = total_bytes.saturating_add(meta.len());
                if let Ok(modified) = meta.modified() {
                    if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                        last_modified_unix = last_modified_unix.max(d.as_secs());
                    }
                }
            }
        }
    }

    let (first_pts_90khz, last_pts_90khz) = match InMemoryIndex::load(&dir.join("index.bin")).await {
        Ok(idx) => match idx.span() {
            Some((f, l)) => (Some(f), Some(l)),
            None => (None, None),
        },
        Err(_) => (None, None),
    };

    let created_at_unix = match fs::read(dir.join("recording.json")).await {
        Ok(bytes) => serde_json::from_slice::<RecordingMetaCreatedAt>(&bytes)
            .ok()
            .map(|m| m.created_at_unix),
        Err(_) => None,
    };

    let clip_count = match ClipStore::open(recording_id, dir).await {
        Ok(store) => store.list().await.len() as u32,
        Err(_) => 0,
    };

    RecordingSummary {
        recording_id: recording_id.to_string(),
        segment_count,
        total_bytes,
        first_pts_90khz,
        last_pts_90khz,
        created_at_unix,
        last_modified_unix,
        clip_count,
    }
}

/// Subset of `recording.json` we actually need for the listing — kept
/// local so the public `RecordingMeta` shape can evolve without
/// rippling here.
#[derive(Deserialize)]
struct RecordingMetaCreatedAt {
    created_at_unix: u64,
}

/// Recursively delete a recording directory. Validates `recording_id`
/// against the storage_id alphanumeric-plus-`._-` rule before any path
/// join — defence-in-depth against path traversal even though the
/// dispatcher also validates.
///
/// Returns the number of bytes freed (sum of segment file sizes prior
/// to deletion), zero when the directory didn't exist.
pub async fn delete_recording(replay_root: &Path, recording_id: &str) -> Result<u64> {
    if recording_id.is_empty() || recording_id.len() > 64 {
        bail!("recording_id length out of range (1..=64)");
    }
    for ch in recording_id.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-';
        if !ok {
            bail!("recording_id contains invalid character '{ch}'");
        }
    }
    let dir = replay_root.join(recording_id);
    if !fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(0);
    }
    let bytes = sum_dir_bytes(&dir).await;
    fs::remove_dir_all(&dir).await?;
    Ok(bytes)
}

async fn sum_dir_bytes(dir: &Path) -> u64 {
    let mut acc: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(mut entries) = fs::read_dir(&path).await else {
            continue;
        };
        while let Ok(Some(e)) = entries.next_entry().await {
            let Ok(ft) = e.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                if let Ok(m) = e.metadata().await {
                    acc = acc.saturating_add(m.len());
                }
            }
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn write_segment(dir: &Path, id: u32, bytes: &[u8]) {
        let path = dir.join(format!("{:06}.ts", id));
        fs::write(&path, bytes).await.unwrap();
    }

    #[tokio::test]
    async fn enumerate_lists_directories_with_segment_totals() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rec = root.join("show-a");
        fs::create_dir_all(&rec).await.unwrap();
        write_segment(&rec, 0, &[0u8; 188]).await;
        write_segment(&rec, 1, &[0u8; 376]).await;

        let listed = enumerate(root).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].recording_id, "show-a");
        assert_eq!(listed[0].segment_count, 2);
        assert_eq!(listed[0].total_bytes, 564);
    }

    #[tokio::test]
    async fn enumerate_skips_dot_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".lost+found")).await.unwrap();
        fs::create_dir_all(root.join("real-recording")).await.unwrap();

        let listed = enumerate(root).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].recording_id, "real-recording");
    }

    #[tokio::test]
    async fn delete_recording_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let err = delete_recording(root, "../escape").await.unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[tokio::test]
    async fn delete_recording_returns_bytes_freed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rec = root.join("rec1");
        fs::create_dir_all(&rec).await.unwrap();
        write_segment(&rec, 0, &[0u8; 1024]).await;
        write_segment(&rec, 1, &[0u8; 2048]).await;

        let freed = delete_recording(root, "rec1").await.unwrap();
        assert_eq!(freed, 3072);
        assert!(!rec.exists());
    }

    #[tokio::test]
    async fn delete_recording_missing_dir_is_zero() {
        let tmp = TempDir::new().unwrap();
        let freed = delete_recording(tmp.path(), "never-existed").await.unwrap();
        assert_eq!(freed, 0);
    }
}
