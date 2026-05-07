// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `state.json` schema + atomic read/write under a `flock(2)` advisory
//! lock.
//!
//! The state file lives at `<install_root>/state.json` (default
//! `/opt/bilbycast/edge/state.json`) and is the on-disk record the
//! upgrade machinery uses to drive:
//!
//! * which version is "current" (matches the symlink),
//! * which version is "previous" (rollback target),
//! * how many boot attempts have been made on the current version,
//! * the last-installed manifest sequence (rollback / replay defence),
//! * and the timestamp of the last healthy beat.
//!
//! Concurrent writes are guarded by `fs2::FileExt::lock_exclusive` so
//! the boot watchdog and the staging task can't trample each other.
//! Updates fsync the file before flipping any symlink.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Lifecycle state of the currently installed version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeStatus {
    /// Initial install — the binary that was unpacked here is the only
    /// one and has booted at least once. No upgrade in flight.
    Stable,
    /// A new version was just staged. Symlink has been swapped; the
    /// next boot must produce a healthy manager beat within the watchdog
    /// window or the boot watchdog rolls back.
    PendingHealth,
    /// Boot watchdog reverted the symlink. The previous-version binary
    /// is now running. This status is set just before exit so the next
    /// boot can emit the `upgrade_rolled_back` Critical event.
    RolledBack,
    /// Operator-staged-but-not-applied (manual_only path). The binary
    /// is on disk under `versions/<v>/` but the symlink still points at
    /// the old version. A SIGUSR1 to the running process completes the
    /// swap.
    StagedManual,
}

/// Persistent state — everything serialised to `state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    /// Version that the `current` symlink points at right now.
    pub current_version: String,
    /// Version the `previous` symlink points at, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    /// Channel + variant + arch the current install was sourced from.
    pub channel: String,
    pub variant: String,
    pub arch: String,
    /// Lifecycle state — drives the watchdog.
    pub status: UpgradeStatus,
    /// How many times the current version has booted in `pending_health`.
    /// Bumped before any other init on every boot; the watchdog rolls
    /// back when this exceeds `max_boot_attempts`.
    #[serde(default)]
    pub boot_attempts: u32,
    /// When the current version was extracted onto disk. Used for log
    /// correlation and the manager UI's "staged 12 minutes ago" badge.
    pub staged_at: DateTime<Utc>,
    /// Timestamp of the most recent `auth_ok` from the manager. Updated
    /// only by the manager-client task; the watchdog reads it to decide
    /// whether the new binary is "healthy enough" to flip status to
    /// `Stable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_health_at: Option<DateTime<Utc>>,
    /// Last installed manifest sequence. Defends against rollback /
    /// replay even when semver alone would otherwise allow it. Strictly
    /// monotonic — every accepted upgrade must carry a higher sequence
    /// than the previous one.
    #[serde(default)]
    pub last_sequence: u64,
}

/// Path of `state.json` relative to the install root.
pub fn path(install_root: &Path) -> PathBuf {
    install_root.join("state.json")
}

/// Load `state.json` from the configured install root. Returns an
/// `Err` (not `Ok(None)`) when the file is missing — the caller decides
/// whether that's expected (first install) or fatal (post-staging).
pub fn load(state_path: &Path) -> Result<InstallState> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(state_path)
        .with_context(|| format!("opening {}", state_path.display()))?;
    file.lock_shared()
        .with_context(|| format!("flock(LOCK_SH) on {}", state_path.display()))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("reading {}", state_path.display()))?;
    let state: InstallState = serde_json::from_str(&buf).with_context(|| {
        format!("{} is not valid InstallState JSON", state_path.display())
    })?;
    let _ = file.unlock();
    Ok(state)
}

/// Try-load: return `None` if the file is missing or unreadable. Used
/// at coordinator startup where a fresh install legitimately has no
/// `state.json` yet.
pub fn try_load(install_root: &Path) -> Option<InstallState> {
    let p = path(install_root);
    if !p.exists() {
        return None;
    }
    load(&p).ok()
}

/// Write `state.json` atomically: write to a sibling tempfile, fsync,
/// rename onto the target. The tempfile is opened with `O_EXCL` so a
/// concurrent stage attempt errors out cleanly instead of silently
/// stomping each other.
pub fn save(state_path: &Path, state: &InstallState) -> Result<()> {
    let parent = state_path
        .parent()
        .ok_or_else(|| anyhow!("state path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating state parent {}", parent.display()))?;

    let tmp_path = parent.join(format!("state.json.tmp.{}", std::process::id()));
    let mut tmp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .with_context(|| format!("creating {}", tmp_path.display()))?;
    // Lock the destination (if it exists) so concurrent staging fails
    // fast rather than racing the rename.
    let dest_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_path)
        .ok();
    if let Some(ref f) = dest_lock {
        f.lock_exclusive().context("flock(LOCK_EX) on state.json")?;
    }
    let json = serde_json::to_vec_pretty(state).context("serialising InstallState")?;
    tmp.write_all(&json)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    tmp.flush().context("tempfile flush")?;
    tmp.sync_all().context("tempfile sync_all")?;
    drop(tmp);
    std::fs::rename(&tmp_path, state_path)
        .with_context(|| format!("rename {} → {}", tmp_path.display(), state_path.display()))?;
    // fsync the parent directory so the rename is durable across crash.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    if let Some(f) = dest_lock {
        let _ = f.unlock();
    }
    Ok(())
}

/// Mutate state under a write lock. The closure is called with a `&mut
/// InstallState`; on `Ok(())` the new state is persisted via
/// [`save`].
pub fn update(state_path: &Path, mutate: impl FnOnce(&mut InstallState) -> Result<()>) -> Result<()> {
    let mut state = load(state_path)?;
    mutate(&mut state)?;
    save(state_path, &state)
}

/// Open a stub state-lock file used to serialize boot-watchdog +
/// staging-task access. Held for the duration of a state read-modify-
/// write sequence. The file is never deleted; we just lock its
/// inode.
pub fn lock_file(install_root: &Path) -> Result<File> {
    let p = install_root.join(".state.lock");
    std::fs::create_dir_all(install_root).ok();
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .with_context(|| format!("opening {}", p.display()))?;
    f.lock_exclusive().context("flock(LOCK_EX) on .state.lock")?;
    Ok(f)
}

/// Re-open a state file for in-place increment (`boot_attempts += 1`)
/// without taking the full state lock. Used by the watchdog before any
/// other init so that a new binary that crashes during init can still
/// be detected next boot.
pub fn bump_boot_attempts(state_path: &Path) -> Result<u32> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(state_path)?;
    file.lock_exclusive()?;
    let mut buf = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut buf)?;
    let mut state: InstallState = serde_json::from_str(&buf)?;
    state.boot_attempts = state.boot_attempts.saturating_add(1);
    let new_attempts = state.boot_attempts;
    let json = serde_json::to_vec_pretty(&state)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&json)?;
    file.flush()?;
    file.sync_all()?;
    let _ = file.unlock();
    Ok(new_attempts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> InstallState {
        InstallState {
            current_version: "0.45.4".into(),
            previous_version: Some("0.45.3".into()),
            channel: "stable".into(),
            variant: "default".into(),
            arch: "x86_64-linux".into(),
            status: UpgradeStatus::PendingHealth,
            boot_attempts: 0,
            staged_at: Utc::now(),
            last_health_at: None,
            last_sequence: 142,
        }
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.json");
        let s = sample();
        save(&p, &s).unwrap();
        let got = load(&p).unwrap();
        assert_eq!(got.current_version, "0.45.4");
        assert_eq!(got.last_sequence, 142);
    }

    #[test]
    fn bump_boot_attempts_increments() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.json");
        save(&p, &sample()).unwrap();
        let n1 = bump_boot_attempts(&p).unwrap();
        let n2 = bump_boot_attempts(&p).unwrap();
        assert_eq!(n1, 1);
        assert_eq!(n2, 2);
    }

    #[test]
    fn try_load_returns_none_when_absent() {
        let dir = tempdir().unwrap();
        assert!(try_load(dir.path()).is_none());
    }
}
