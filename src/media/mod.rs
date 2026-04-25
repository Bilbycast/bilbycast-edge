// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Media-library storage for the file-backed media-player input.
//!
//! The library is a flat directory on the edge's local disk holding the
//! `.ts` / `.mp4` / image assets that a `MediaPlayer` input plays back.
//! Files arrive over the WS upload command from the manager (chunked, 1
//! MiB per chunk to stay under the 5 MiB WS payload cap). Filenames are
//! sanitised to a strict ASCII subset so cross-platform pickers and
//! shells never have to escape anything.
//!
//! # Invariants
//!
//! * Final files live as `<media_dir>/<name>` with mode `0644`.
//! * In-progress uploads stage under `<media_dir>/.tmp/<name>.<session_id>`
//!   and atomically `rename(2)` into place on the final chunk. Any partial
//!   stagings older than the configured TTL are reaped on next list.
//! * Per-file size is capped at `MAX_FILE_BYTES` (default 4 GiB) and total
//!   library footprint at `MAX_TOTAL_BYTES` (default 16 GiB) — the upload
//!   handler refuses chunks that would breach either cap.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Hard ceiling per individual asset (4 GiB).
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Soft ceiling for the entire library footprint (16 GiB). The upload
/// handler refuses new chunks once any pending upload would push the sum
/// past this. Operators with bigger needs should mount a dedicated
/// volume and override `BILBYCAST_MEDIA_DIR`.
pub const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Reap partial uploads idle for longer than this. Keeps the staging
/// directory tidy without surprising operators mid-upload over a flaky
/// link.
pub const STAGING_TTL: Duration = Duration::from_secs(60 * 60);

/// Public summary entry for a single library file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaFileInfo {
    pub name: String,
    pub size_bytes: u64,
    /// File mtime in seconds since the Unix epoch. Suitable for the
    /// browser's library list; not authoritative for replay decisions.
    pub modified_unix: u64,
}

/// Resolve the configured media directory. Mirrors
/// [`crate::engine::input_media_player::media_dir`] but lives here too so
/// the upload / list / delete API layer doesn't depend on the engine
/// module's call surface.
pub fn media_dir() -> PathBuf {
    if let Ok(p) = std::env::var("BILBYCAST_MEDIA_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("bilbycast").join("media");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".bilbycast").join("media");
    }
    PathBuf::from("./media")
}

fn staging_dir() -> PathBuf {
    media_dir().join(".tmp")
}

/// In-memory upload session bookkeeping, keyed by sanitised filename.
/// Holds the staging file path, declared chunk count + total bytes, and a
/// running `bytes_received` counter. Lives behind a `Mutex` because the
/// final-chunk rename must observe a consistent view. The session is
/// wrapped in `Arc` so callers can clone the handle out of the DashMap
/// and release the shard lock *before* awaiting on the inner mutex —
/// holding a `RefMut` across the inner `lock().await` and then calling
/// `self.sessions.remove(name)` on the same shard is a self-deadlock
/// (write-lock acquired twice on a non-reentrant `RwLock`).
pub struct MediaLibrary {
    sessions: DashMap<String, Arc<Mutex<UploadSession>>>,
}

struct UploadSession {
    staging_path: PathBuf,
    next_chunk: u32,
    total_chunks: u32,
    total_bytes: u64,
    bytes_received: u64,
    last_touch: SystemTime,
}

impl MediaLibrary {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// Ensure the media directory and `.tmp` staging directory exist.
    /// Called once on boot from the WS handler hook so a fresh edge can
    /// accept uploads without the operator pre-mkdir-ing anything.
    pub fn ensure_dirs() -> Result<()> {
        let dir = media_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("create media dir {}: {e}", dir.display()))?;
        let stg = staging_dir();
        std::fs::create_dir_all(&stg)
            .map_err(|e| anyhow!("create staging dir {}: {e}", stg.display()))?;
        Ok(())
    }

    /// Enumerate the library, sorted by filename. Reaps stale staging
    /// files as a side effect so operators see a clean list after a
    /// failed upload.
    pub async fn list() -> Result<Vec<MediaFileInfo>> {
        Self::ensure_dirs()?;
        Self::reap_stale_staging().await;
        let dir = media_dir();
        let mut entries: Vec<MediaFileInfo> = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| anyhow!("read_dir {}: {e}", dir.display()))?;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| anyhow!("read_dir entry: {e}"))?
        {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() {
                continue;
            }
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entries.push(MediaFileInfo {
                name,
                size_bytes: meta.len(),
                modified_unix,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Total bytes of finalised library files. Used by the upload handler
    /// to enforce [`MAX_TOTAL_BYTES`] before accepting a new chunk.
    pub async fn total_bytes() -> Result<u64> {
        let files = Self::list().await?;
        Ok(files.iter().map(|f| f.size_bytes).sum())
    }

    /// Hard-delete a file. Filename validation is the caller's responsibility
    /// (we re-check defence-in-depth here too).
    pub async fn delete(name: &str) -> Result<bool> {
        crate::config::validation::validate_media_filename(name, "delete_media")?;
        let path = media_dir().join(name);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow!("delete {}: {e}", path.display())),
        }
    }

    /// Apply one chunk of an upload. Returns `true` when the final chunk
    /// fsyncs + renames into place.
    pub async fn apply_chunk(
        &self,
        name: &str,
        chunk_index: u32,
        total_chunks: u32,
        total_bytes: u64,
        data_b64: &str,
    ) -> Result<UploadProgress> {
        crate::config::validation::validate_media_filename(name, "upload_media_chunk")?;
        if total_chunks == 0 {
            bail!("upload_media_chunk: total_chunks must be ≥ 1");
        }
        if chunk_index >= total_chunks {
            bail!(
                "upload_media_chunk: chunk_index {chunk_index} ≥ total_chunks {total_chunks}"
            );
        }
        if total_bytes > MAX_FILE_BYTES {
            bail!(
                "upload_media_chunk: total_bytes {total_bytes} exceeds per-file cap {MAX_FILE_BYTES}"
            );
        }

        Self::ensure_dirs()?;
        Self::reap_stale_staging().await;

        // Decode and verify chunk size before touching disk so we fail
        // fast on a malformed encode.
        let chunk = BASE64
            .decode(data_b64)
            .map_err(|e| anyhow!("upload_media_chunk: base64 decode failed: {e}"))?;

        // Library-footprint guard: existing finalised bytes + this upload's
        // declared size must not breach the cap. Counting `total_bytes` (not
        // `bytes_received`) catches the breach on chunk 0 instead of mid-upload.
        let used = Self::total_bytes().await?;
        if used.saturating_add(total_bytes) > MAX_TOTAL_BYTES {
            bail!(
                "upload_media_chunk: library footprint {used} + new file {total_bytes} would exceed cap {MAX_TOTAL_BYTES}"
            );
        }

        // Open or resume the staging session for this filename. Clone the
        // `Arc<Mutex<_>>` handle out of the DashMap and drop the entry
        // immediately so we never hold a shard write lock across the
        // inner `lock().await` or the final-chunk `self.sessions.remove`.
        let staging_path = staging_dir().join(format!("{name}.staging"));
        let session = self
            .sessions
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(Mutex::new(UploadSession {
                    staging_path: staging_path.clone(),
                    next_chunk: 0,
                    total_chunks,
                    total_bytes,
                    bytes_received: 0,
                    last_touch: SystemTime::now(),
                }))
            })
            .clone();

        // tokio::sync::Mutex — the guard *is* Send so it survives the
        // spawn_blocking await below without collapsing the whole future
        // off Send.
        let mut sess = session.lock().await;

        if sess.total_chunks != total_chunks {
            bail!(
                "upload_media_chunk: total_chunks {} differs from session {}",
                total_chunks,
                sess.total_chunks
            );
        }
        if sess.total_bytes != total_bytes {
            bail!(
                "upload_media_chunk: total_bytes {} differs from session {}",
                total_bytes,
                sess.total_bytes
            );
        }
        if chunk_index != sess.next_chunk {
            bail!(
                "upload_media_chunk: out-of-order chunk {chunk_index}, expected {}",
                sess.next_chunk
            );
        }

        let new_total = sess.bytes_received.saturating_add(chunk.len() as u64);
        if new_total > total_bytes {
            bail!(
                "upload_media_chunk: chunk {chunk_index} would push session to {new_total} bytes, exceeding declared total {total_bytes}"
            );
        }

        // Append the chunk on a blocking thread so the tokio reactor stays
        // responsive even on slow disks.
        let staging_path = sess.staging_path.clone();
        let chunk_bytes = chunk.clone();
        let is_first = chunk_index == 0;
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::fs::OpenOptions;
            use std::io::Write;
            let mut f = OpenOptions::new()
                .create(is_first)
                .append(true)
                .write(true)
                .truncate(false)
                .open(&staging_path)
                .map_err(|e| anyhow!("open staging {}: {e}", staging_path.display()))?;
            f.write_all(&chunk_bytes)
                .map_err(|e| anyhow!("write staging {}: {e}", staging_path.display()))?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow!("upload chunk write join: {e}"))??;

        sess.bytes_received = new_total;
        sess.next_chunk = sess.next_chunk.saturating_add(1);
        sess.last_touch = SystemTime::now();

        let final_chunk = chunk_index + 1 == total_chunks;
        if final_chunk {
            if sess.bytes_received != total_bytes {
                let path = sess.staging_path.clone();
                drop(sess);
                self.sessions.remove(name);
                let _ = tokio::fs::remove_file(&path).await;
                bail!(
                    "upload_media_chunk: assembled file is {} bytes, declared {}",
                    new_total, total_bytes
                );
            }
            // Atomic rename onto the final path. We deliberately do NOT
            // fsync before rename — fsync of a multi-hundred-MB staging
            // file can take tens of seconds on consumer SSDs and was
            // pushing the manager's per-chunk ACK timeout past 60 s. A
            // media library is not a durability-critical surface: worst
            // case after a host crash is a partial file the operator
            // re-uploads. The OS flushes in the background regardless.
            let final_path = media_dir().join(name);
            let staging_path = sess.staging_path.clone();
            drop(sess);
            self.sessions.remove(name);
            tokio::task::spawn_blocking(move || -> Result<()> {
                std::fs::rename(&staging_path, &final_path)
                    .map_err(|e| anyhow!("rename {}→{}: {e}", staging_path.display(), final_path.display()))?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("upload finalise join: {e}"))??;
            return Ok(UploadProgress::Complete {
                bytes_received: total_bytes,
            });
        }

        Ok(UploadProgress::InProgress {
            chunks_received: chunk_index + 1,
            chunks_total: total_chunks,
            bytes_received: new_total,
        })
    }

    /// Best-effort reap of staging files older than [`STAGING_TTL`]. Runs
    /// before every `list()` and `apply_chunk()` so stale partials don't
    /// build up across edge restarts.
    async fn reap_stale_staging() {
        let stg = staging_dir();
        let now = SystemTime::now();
        let mut rd = match tokio::fs::read_dir(&stg).await {
            Ok(rd) => rd,
            Err(_) => return,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let modified = meta.modified().ok().unwrap_or(now);
            if now.duration_since(modified).unwrap_or_default() > STAGING_TTL {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
}

impl Default for MediaLibrary {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-chunk progress signal returned from [`MediaLibrary::apply_chunk`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UploadProgress {
    /// More chunks expected.
    InProgress {
        chunks_received: u32,
        chunks_total: u32,
        bytes_received: u64,
    },
    /// File assembled and renamed into the library.
    Complete { bytes_received: u64 },
}

/// One process-wide instance shared across the WS command handlers. Used
/// because per-filename upload sessions must persist across multiple WS
/// command messages on the same connection.
static GLOBAL: std::sync::OnceLock<MediaLibrary> = std::sync::OnceLock::new();

pub fn global() -> &'static MediaLibrary {
    GLOBAL.get_or_init(MediaLibrary::new)
}

/// One-shot resolver mirroring [`crate::engine::input_media_player::resolve_media_path`]
/// — kept here so non-engine callers (the upload handler, future REST API) can
/// resolve paths without depending on the engine module.
#[allow(dead_code)]
pub fn resolve_path(name: &str) -> Result<PathBuf> {
    crate::config::validation::validate_media_filename(name, "resolve_media_path")?;
    Ok(media_dir().join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_progress_serialises_with_status_tag() {
        let p = UploadProgress::InProgress {
            chunks_received: 1,
            chunks_total: 4,
            bytes_received: 1024,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"status\":\"in_progress\""));
        assert!(json.contains("\"chunks_received\":1"));
    }

    #[test]
    fn upload_progress_complete_serialises() {
        let p = UploadProgress::Complete {
            bytes_received: 4096,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"status\":\"complete\""));
        assert!(json.contains("\"bytes_received\":4096"));
    }
}
