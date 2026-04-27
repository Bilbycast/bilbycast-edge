// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Clip metadata persistence for one recording.
//!
//! Clips live in `<recording_dir>/clips.json` as a flat JSON array.
//! Writes are atomic (write to `clips.json.tmp` + fsync + rename) and
//! the writer holds a `Mutex` so concurrent mark_outs serialise. The
//! file size is small (a few hundred bytes per clip) so a full rewrite
//! per mark is cheap.
//!
//! `clip_id` is `clp_<26-char ULID-ish>` — generated edge-side from
//! 96 bits of the kernel CSPRNG so two edges can't collide. (We don't
//! pull in the `ulid` crate; this is a 16-byte random + base32 encode.)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use rand::RngExt;
use serde::Deserialize;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use super::ClipInfo;

/// Recording-scoped clip store. Holds the recording_id + path so callers
/// can `mark_out`, `delete`, `list` without re-resolving the path each
/// time. The inner Mutex guards mutating ops so two simultaneous
/// `mark_out`s on the same recording can't lose entries through a
/// last-write-wins race.
#[derive(Clone)]
pub struct ClipStore {
    inner: Arc<Mutex<ClipStoreInner>>,
}

struct ClipStoreInner {
    path: PathBuf,
    clips: Vec<ClipInfo>,
}

impl ClipStore {
    /// Open or create the clip store for a given recording. Loads
    /// existing entries; an absent or empty file is treated as no clips.
    pub async fn open(recording_id: &str, recording_dir: &Path) -> Result<Self> {
        let path = recording_dir.join("clips.json");
        let clips = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let bytes = tokio::fs::read(&path).await
                .with_context(|| format!("read {}", path.display()))?;
            if bytes.is_empty() {
                Vec::new()
            } else {
                #[derive(Deserialize)]
                struct File {
                    #[serde(default)]
                    clips: Vec<ClipInfo>,
                }
                // Accept either `{ "clips": [...] }` or a bare array for
                // forward-compat with simpler exports.
                let parsed: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse {}", path.display()))?;
                if parsed.is_array() {
                    serde_json::from_value(parsed)?
                } else {
                    let f: File = serde_json::from_value(parsed)?;
                    f.clips
                }
            }
        } else {
            Vec::new()
        };
        let _ = recording_id; // accepted for future scoping; currently the path is the source of truth
        Ok(Self {
            inner: Arc::new(Mutex::new(ClipStoreInner { path, clips })),
        })
    }

    /// Snapshot of all clips. Cheap (clones the small Vec).
    pub async fn list(&self) -> Vec<ClipInfo> {
        self.inner.lock().await.clips.clone()
    }

    /// Look up a clip by ID.
    pub async fn get(&self, clip_id: &str) -> Option<ClipInfo> {
        self.inner.lock().await.clips.iter().find(|c| c.id == clip_id).cloned()
    }

    /// Create a new clip from an in/out range. Generates the clip_id
    /// edge-side, persists with fsync, returns the materialised entry.
    pub async fn create(
        &self,
        recording_id: &str,
        in_pts: u64,
        out_pts: u64,
        smpte_in: Option<String>,
        smpte_out: Option<String>,
        name: Option<String>,
        description: Option<String>,
        created_by: Option<String>,
        tags: Vec<String>,
    ) -> Result<ClipInfo> {
        if out_pts <= in_pts {
            return Err(anyhow!(
                "clip out_pts ({out_pts}) must be greater than in_pts ({in_pts})"
            ));
        }
        validate_clip_strings(name.as_deref(), description.as_deref())?;
        let normalised_tags = validate_and_normalise_tags(&tags)?;
        let id = generate_clip_id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let info = ClipInfo {
            id,
            recording_id: recording_id.to_string(),
            // Default name = "Clip <yyyy-mm-dd HH:MM:SS>" if absent.
            name: name.unwrap_or_else(|| {
                use chrono::{TimeZone, Utc};
                let dt = Utc.timestamp_opt(now as i64, 0).single();
                match dt {
                    Some(d) => format!("Clip {}", d.format("%Y-%m-%d %H:%M:%S")),
                    None => "Clip".to_string(),
                }
            }),
            description,
            in_pts_90khz: in_pts,
            out_pts_90khz: out_pts,
            smpte_in,
            smpte_out,
            created_at_unix: now,
            created_by,
            tags: normalised_tags,
        };
        // Hold the lock across write_atomic so two concurrent mark_outs
        // can't race on the same `clips.json.tmp` filename. Disk I/O is
        // fast; mark_outs are operator-driven and low-frequency.
        let mut inner = self.inner.lock().await;
        inner.clips.push(info.clone());
        write_atomic(&inner.path, &inner.clips).await?;
        Ok(info)
    }

    /// Rename a clip and / or update its description. Returns the
    /// updated `ClipInfo` on success, or `None` when the clip is not
    /// found. Persists `clips.json` with fsync before returning.
    pub async fn rename(
        &self,
        clip_id: &str,
        new_name: Option<String>,
        new_description: Option<String>,
    ) -> Result<Option<ClipInfo>> {
        validate_clip_strings(new_name.as_deref(), new_description.as_deref())?;
        let mut inner = self.inner.lock().await;
        let updated: Option<ClipInfo> = inner.clips.iter_mut().find(|c| c.id == clip_id).map(|c| {
            if let Some(n) = new_name { c.name = n; }
            if let Some(d) = new_description { c.description = Some(d); }
            c.clone()
        });
        if updated.is_none() {
            return Ok(None);
        }
        write_atomic(&inner.path, &inner.clips).await?;
        Ok(updated)
    }

    /// Phase 2 — general-purpose update: name / description / tags /
    /// in-point / out-point. Any combination of fields may be `Some`;
    /// `None` means "leave the existing value untouched". Returns the
    /// updated `ClipInfo` on success or `None` when the clip ID is
    /// unknown. Inverted PTS ranges, oversize names/descriptions, and
    /// invalid tags are rejected before persistence — error strings
    /// carry the matching `replay_invalid_*` code so the WS dispatcher
    /// can lift them onto `command_ack.error_code`.
    ///
    /// **SMPTE TC handling on trim.** The IDR index doesn't carry
    /// SMPTE strings (just PTS / segment / offset / flags), so the
    /// edge can't cheaply re-derive a fresh `HH:MM:SS:FF` for a new
    /// in/out PTS. When `new_in_pts` is set, `smpte_in` is cleared
    /// (and likewise for `smpte_out`); the manager UI will show "—"
    /// until the operator re-marks. Persisting a stale SMPTE that no
    /// longer matches the PTS would mislead operators worse than the
    /// blank.
    pub async fn update(
        &self,
        clip_id: &str,
        new_name: Option<String>,
        new_description: Option<String>,
        new_tags: Option<Vec<String>>,
        new_in_pts: Option<u64>,
        new_out_pts: Option<u64>,
    ) -> Result<Option<ClipInfo>> {
        validate_clip_strings(new_name.as_deref(), new_description.as_deref())?;
        let normalised_tags = match new_tags.as_ref() {
            Some(t) => Some(validate_and_normalise_tags(t)?),
            None => None,
        };
        let mut inner = self.inner.lock().await;
        // Compute the post-update range without mutating, so we can
        // reject inverted ranges before half-applying the patch.
        let target = match inner.clips.iter().find(|c| c.id == clip_id).cloned() {
            Some(c) => c,
            None => return Ok(None),
        };
        let prospective_in = new_in_pts.unwrap_or(target.in_pts_90khz);
        let prospective_out = new_out_pts.unwrap_or(target.out_pts_90khz);
        if prospective_out <= prospective_in {
            return Err(anyhow!(
                "replay_invalid_range: out_pts ({prospective_out}) must be greater than \
                 in_pts ({prospective_in})"
            ));
        }
        let updated: Option<ClipInfo> =
            inner.clips.iter_mut().find(|c| c.id == clip_id).map(|c| {
                if let Some(n) = new_name { c.name = n; }
                if let Some(d) = new_description { c.description = Some(d); }
                if let Some(t) = normalised_tags { c.tags = t; }
                if let Some(p) = new_in_pts {
                    c.in_pts_90khz = p;
                    c.smpte_in = None;
                }
                if let Some(p) = new_out_pts {
                    c.out_pts_90khz = p;
                    c.smpte_out = None;
                }
                c.clone()
            });
        if updated.is_none() {
            return Ok(None);
        }
        write_atomic(&inner.path, &inner.clips).await?;
        Ok(updated)
    }

    /// Delete a clip. Returns true if the clip existed.
    pub async fn delete(&self, clip_id: &str) -> Result<bool> {
        let mut inner = self.inner.lock().await;
        let before = inner.clips.len();
        inner.clips.retain(|c| c.id != clip_id);
        if inner.clips.len() == before {
            return Ok(false);
        }
        write_atomic(&inner.path, &inner.clips).await?;
        Ok(true)
    }
}

/// Atomic + durable JSON write: write to `<path>.tmp`, fsync, rename
/// over `<path>`, fsync the directory. Mirrors `crate::media`'s
/// upload-completion path.
async fn write_atomic(path: &Path, clips: &[ClipInfo]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    tokio::fs::create_dir_all(parent).await
        .with_context(|| format!("create_dir_all {}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({ "clips": clips }))?;
    {
        let mut f: File = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .await
            .with_context(|| format!("open {}", tmp.display()))?;
        f.write_all(&bytes).await?;
        f.flush().await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    // Best-effort directory fsync — not all platforms require it but on
    // Linux this guarantees the rename is durable on a hard reset.
    if let Ok(d) = File::open(parent).await {
        let _ = d.sync_all().await;
    }
    Ok(())
}

/// Generate a `clp_` + base32 16-byte random ID. We don't pull in the
/// `ulid` crate — the only thing we need is collision resistance per
/// edge node, and 128 bits of CSPRNG output is plenty.
/// Length-validate the operator-supplied `name` and `description` on
/// clip create / rename. The caller already documents the limits in
/// the `ClipInfo` struct comments; enforcing them at the WS entry
/// point keeps a malicious manager from persisting megabyte-sized
/// strings into `clips.json`. The error string maps onto the
/// `replay_invalid_field` error code on the dispatcher.
pub(crate) const CLIP_NAME_MAX: usize = 256;
pub(crate) const CLIP_DESCRIPTION_MAX: usize = 4096;

pub(crate) fn validate_clip_strings(
    name: Option<&str>,
    description: Option<&str>,
) -> Result<()> {
    if let Some(n) = name {
        if n.len() > CLIP_NAME_MAX {
            return Err(anyhow!(
                "replay_invalid_field: clip name length {} exceeds max {}",
                n.len(),
                CLIP_NAME_MAX
            ));
        }
        if n.contains(['\n', '\r', '\0']) {
            return Err(anyhow!(
                "replay_invalid_field: clip name must not contain control characters"
            ));
        }
    }
    if let Some(d) = description {
        if d.len() > CLIP_DESCRIPTION_MAX {
            return Err(anyhow!(
                "replay_invalid_field: clip description length {} exceeds max {}",
                d.len(),
                CLIP_DESCRIPTION_MAX
            ));
        }
    }
    Ok(())
}

/// Phase 2 — tag bounds enforced at the WS dispatcher. Each tag
/// matches `^[A-Z0-9_-]{1,32}$`; the list is dedup'd and capped at
/// `CLIP_TAGS_MAX`. The manager UI offers a fixed set today; the
/// edge stores whatever the manager sends so per-group tag config
/// later doesn't need an edge release.
pub(crate) const CLIP_TAGS_MAX: usize = 16;
pub(crate) const CLIP_TAG_LEN_MAX: usize = 32;

pub(crate) fn validate_and_normalise_tags(tags: &[String]) -> Result<Vec<String>> {
    if tags.len() > CLIP_TAGS_MAX {
        return Err(anyhow!(
            "replay_invalid_tag: clip carries {} tags, max {}",
            tags.len(),
            CLIP_TAGS_MAX
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for t in tags {
        if t.is_empty() || t.len() > CLIP_TAG_LEN_MAX {
            return Err(anyhow!(
                "replay_invalid_tag: tag '{}' length {} not in [1, {}]",
                t,
                t.len(),
                CLIP_TAG_LEN_MAX
            ));
        }
        if !t
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(anyhow!(
                "replay_invalid_tag: tag '{}' must match [A-Z0-9_-]+",
                t
            ));
        }
        if !out.iter().any(|x| x == t) {
            out.push(t.clone());
        }
    }
    Ok(out)
}

fn generate_clip_id() -> String {
    const ALPHA: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ"; // Crockford-like
    let mut buf = [0u8; 16];
    rand::rng().fill(&mut buf);
    // Base32 of 16 bytes is 26 chars (with no padding).
    let mut out = String::with_capacity(4 + 26);
    out.push_str("clp_");
    let mut bits: u64 = 0;
    let mut nbits: u32 = 0;
    for b in buf {
        bits = (bits << 8) | (b as u64);
        nbits += 8;
        while nbits >= 5 {
            nbits -= 5;
            let v = ((bits >> nbits) & 0x1F) as usize;
            out.push(ALPHA[v] as char);
        }
    }
    if nbits > 0 {
        let v = ((bits << (5 - nbits)) & 0x1F) as usize;
        out.push(ALPHA[v] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn create_persist_reload_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = ClipStore::open("rec_a", tmp.path()).await.unwrap();
        assert!(store.list().await.is_empty());
        let info = store.create(
            "rec_a", 1_000, 91_000,
            Some("00:00:00:00".into()),
            Some("00:00:01:00".into()),
            Some("First".into()), None, Some("user_alice".into()),
            Vec::new(),
        ).await.unwrap();
        assert_eq!(info.duration_ms(), 1_000);
        assert!(info.id.starts_with("clp_"));

        // Reload from disk.
        let store2 = ClipStore::open("rec_a", tmp.path()).await.unwrap();
        let clips = store2.list().await;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, info.id);

        // Delete is durable.
        assert!(store2.delete(&info.id).await.unwrap());
        let store3 = ClipStore::open("rec_a", tmp.path()).await.unwrap();
        assert!(store3.list().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mark_outs_dont_lose_entries() {
        let tmp = TempDir::new().unwrap();
        let store = ClipStore::open("rec_b", tmp.path()).await.unwrap();
        let mut handles = Vec::new();
        for i in 0..20u64 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                s.create(
                    "rec_b", i * 90_000, (i + 1) * 90_000,
                    None, None,
                    Some(format!("Clip {i}")), None, None,
                    Vec::new(),
                ).await.unwrap()
            }));
        }
        for h in handles {
            let _ = h.await.unwrap();
        }
        let store2 = ClipStore::open("rec_b", tmp.path()).await.unwrap();
        assert_eq!(store2.list().await.len(), 20);
    }

    #[test]
    fn rejects_inverted_range() {
        // Use a runtime to await — but the rejection happens before any
        // disk I/O so we can construct the store with a plain temp dir.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = TempDir::new().unwrap();
            let store = ClipStore::open("rec_c", tmp.path()).await.unwrap();
            let err = store.create(
                "rec_c", 100, 50,
                None, None, None, None, None,
                Vec::new(),
            ).await.unwrap_err();
            assert!(format!("{err}").contains("must be greater"));
        });
    }

    #[test]
    fn validate_tags_accepts_clean_set() {
        let tags = vec!["GOAL".into(), "FOUL".into(), "VAR-CHECK".into()];
        let out = validate_and_normalise_tags(&tags).unwrap();
        assert_eq!(out, tags);
    }

    #[test]
    fn validate_tags_dedupes() {
        let tags = vec!["GOAL".into(), "GOAL".into(), "FOUL".into()];
        let out = validate_and_normalise_tags(&tags).unwrap();
        assert_eq!(out, vec!["GOAL".to_string(), "FOUL".to_string()]);
    }

    #[test]
    fn validate_tags_rejects_lowercase() {
        let tags = vec!["goal".into()];
        let err = validate_and_normalise_tags(&tags).unwrap_err();
        assert!(format!("{err}").contains("replay_invalid_tag"));
    }

    #[test]
    fn validate_tags_rejects_oversize() {
        let tags = vec!["A".repeat(33)];
        let err = validate_and_normalise_tags(&tags).unwrap_err();
        assert!(format!("{err}").contains("replay_invalid_tag"));
    }

    #[test]
    fn validate_tags_rejects_too_many() {
        let tags: Vec<String> = (0..17u32).map(|i| format!("T{i:02}")).collect();
        let err = validate_and_normalise_tags(&tags).unwrap_err();
        assert!(format!("{err}").contains("replay_invalid_tag"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn update_clip_round_trip() {
        let tmp = TempDir::new().unwrap();
        let store = ClipStore::open("rec_d", tmp.path()).await.unwrap();
        let info = store
            .create(
                "rec_d", 1_000, 91_000,
                Some("00:00:00:00".into()), Some("00:00:01:00".into()),
                Some("Original".into()), None, None,
                Vec::new(),
            )
            .await
            .unwrap();
        // Add tags via update — leaves PTS bounds untouched.
        let updated = store
            .update(
                &info.id,
                None,
                None,
                Some(vec!["GOAL".into(), "VAR-CHECK".into()]),
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.tags, vec!["GOAL".to_string(), "VAR-CHECK".to_string()]);
        assert_eq!(updated.in_pts_90khz, 1_000);
        assert_eq!(updated.smpte_in.as_deref(), Some("00:00:00:00"));

        // Trim out-PTS — SMPTE-out clears (Phase 2 honest semantics).
        let trimmed = store
            .update(
                &info.id, None, None, None, None,
                Some(81_000),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(trimmed.out_pts_90khz, 81_000);
        assert!(trimmed.smpte_out.is_none());
        // Tags survive an in/out trim.
        assert_eq!(trimmed.tags, vec!["GOAL".to_string(), "VAR-CHECK".to_string()]);

        // Inverted prospective range is rejected.
        let err = store
            .update(&info.id, None, None, None, Some(90_000), Some(80_000))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("replay_invalid_range"));

        // Reload — tags + trimmed bounds round-trip via clips.json.
        let reloaded = ClipStore::open("rec_d", tmp.path()).await.unwrap();
        let clips = reloaded.list().await;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].tags, vec!["GOAL".to_string(), "VAR-CHECK".to_string()]);
        assert_eq!(clips[0].out_pts_90khz, 81_000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn update_clip_unknown_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = ClipStore::open("rec_e", tmp.path()).await.unwrap();
        let res = store
            .update(
                "clp_NONEXISTENT", Some("x".into()), None, None, None, None,
            )
            .await
            .unwrap();
        assert!(res.is_none());
    }
}
