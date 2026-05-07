// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tarball extraction + atomic symlink swap + version-dir GC.
//!
//! Disk layout (matches `docs/upgrade.md`):
//!
//! ```text
//! /opt/bilbycast/edge/
//! ├── current → versions/0.45.4/        # atomic swap target
//! ├── previous → versions/0.45.3/       # populated after first upgrade
//! ├── versions/
//! │   ├── 0.45.3/bilbycast-edge
//! │   └── 0.45.4/bilbycast-edge
//! ├── state.json                        # see state.rs
//! ├── config.json                       # operational config
//! └── secrets.json                      # 0600, machine-key encrypted
//! ```
//!
//! ### Atomicity
//!
//! `rename(2)` on the same filesystem is the only operation we rely on
//! for atomicity. The flow is:
//!
//! 1. Extract the tarball into `versions/<new>.partial/` (so a crash
//!    during extraction leaves an obvious orphan that the GC sweep can
//!    delete on next boot).
//! 2. Rename `versions/<new>.partial/` → `versions/<new>/`.
//! 3. Create `current.tmp` as a symlink to `versions/<new>/`.
//! 4. Rename `current.tmp` → `current`.
//! 5. Rotate the old `current` target into `previous` (best-effort —
//!    preserved across restarts so the watchdog can roll back).
//!
//! The kernel guarantees rename(2) atomicity for steps 2 and 4, so
//! a power-loss at any point leaves the system either fully
//! upgraded, fully on the old version, or with a sweep-able partial
//! that is unambiguously not the live target.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::manifest::Manifest;

/// Default name of the binary inside the tarball.
const BINARY_NAME: &str = "bilbycast-edge";

/// Extract `tarball_bytes`, populate `versions/<new>/`, and atomically
/// flip the `current` symlink. Returns the absolute path of
/// `versions/<new>/`.
pub fn stage_new_version(
    install_root: &Path,
    new_version: &str,
    tarball_bytes: &[u8],
    manifest: &Manifest,
) -> Result<PathBuf> {
    if new_version != manifest.version {
        bail!(
            "internal error: stage_new_version called with new_version={new_version} but \
             manifest.version={}",
            manifest.version
        );
    }
    fs::create_dir_all(install_root.join("versions"))
        .with_context(|| format!("creating {}/versions", install_root.display()))?;

    let final_dir = install_root.join("versions").join(new_version);
    let partial_dir = install_root
        .join("versions")
        .join(format!("{new_version}.partial"));

    // Clean any stale partial from a previous failed attempt.
    if partial_dir.exists() {
        fs::remove_dir_all(&partial_dir)
            .with_context(|| format!("removing stale partial {}", partial_dir.display()))?;
    }
    if final_dir.exists() {
        // Either we already staged this version (idempotent re-run) or a
        // previous run left it behind. Safe to remove only if it's not
        // the target of `current`. The watchdog re-stage path is the
        // realistic case here.
        let current_link = install_root.join("current");
        if current_link.read_link().ok().as_deref() != Some(final_dir.as_path()) {
            fs::remove_dir_all(&final_dir).with_context(|| {
                format!("removing pre-existing {}", final_dir.display())
            })?;
        } else {
            // Already current — short-circuit. The caller will detect
            // this via the version comparison upstream.
            return Ok(final_dir);
        }
    }

    fs::create_dir(&partial_dir)
        .with_context(|| format!("creating {}", partial_dir.display()))?;
    extract_tarball(tarball_bytes, &partial_dir)
        .context("tarball extraction failed")?;

    // Locate the binary inside the extracted tree. Releases ship the
    // binary nested one level (e.g.
    // `bilbycast-edge-0.45.4-x86_64-linux-full/bilbycast-edge`); we
    // search depth-1 so either layout works.
    let binary_src = locate_binary(&partial_dir, BINARY_NAME)
        .ok_or_else(|| anyhow!("binary {BINARY_NAME:?} not found in extracted tarball"))?;
    let mut perms = fs::metadata(&binary_src)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&binary_src, perms)?;
    // Hoist the binary to the top level of versions/<new>/ so the
    // launcher symlink can use a stable relative path.
    if binary_src.parent() != Some(&partial_dir) {
        let dest = partial_dir.join(BINARY_NAME);
        if dest != binary_src {
            fs::rename(&binary_src, &dest)
                .with_context(|| format!("hoist binary to {}", dest.display()))?;
        }
    }

    // fsync the partial dir before the rename so the kernel commits the
    // file metadata to disk in the right order.
    if let Ok(d) = fs::File::open(&partial_dir) {
        let _ = d.sync_all();
    }

    // Atomic step: rename partial → final.
    fs::rename(&partial_dir, &final_dir).with_context(|| {
        format!(
            "rename {} → {}",
            partial_dir.display(),
            final_dir.display()
        )
    })?;

    // Atomic step: swap `current` symlink. `previous` is rotated from
    // the existing `current` first so the watchdog has a rollback
    // target.
    swap_current_symlink(install_root, &final_dir)
        .context("symlink swap failed")?;

    // Best-effort GC of orphans (older versions, stale partials).
    if let Err(e) = gc_versions(install_root, new_version, /* keep = */ 3) {
        tracing::warn!("upgrade GC swept with errors: {e:#}");
    }

    Ok(final_dir)
}

fn extract_tarball(tarball_bytes: &[u8], dest: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    let cursor = Cursor::new(tarball_bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    // Reject absolute paths and `..` traversals — `tar` handles this
    // when `set_unpack_xattrs(false)` + manual entry inspection, but
    // tar-rs's default is already to refuse `..` segments. Belt + braces:
    let entries = archive.entries().context("opening tar entries")?;
    for entry in entries {
        let mut entry = entry.context("reading tar entry")?;
        let path = entry.path().context("decoding tar entry path")?.into_owned();
        if path.is_absolute() {
            bail!("tar entry has absolute path {:?} — refusing", path);
        }
        if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("tar entry has '..' segment {:?} — refusing", path);
        }
        let target = dest.join(&path);
        // Ensure the unpack target stays under `dest`.
        if !target.starts_with(dest) {
            bail!(
                "tar entry would escape extraction root: {:?} → {}",
                path,
                target.display()
            );
        }
        entry
            .unpack(&target)
            .with_context(|| format!("unpacking {} to {}", path.display(), target.display()))?;
    }
    Ok(())
}

fn locate_binary(root: &Path, name: &str) -> Option<PathBuf> {
    // Direct hit.
    let direct = root.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    // Depth-1 search.
    for entry in fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let nested = p.join(name);
            if nested.is_file() {
                return Some(nested);
            }
        }
    }
    None
}

fn swap_current_symlink(install_root: &Path, new_target: &Path) -> Result<()> {
    let current = install_root.join("current");
    let previous = install_root.join("previous");
    let tmp = install_root.join("current.tmp");

    // Rotate current → previous (if there's an existing target).
    if let Ok(existing_target) = current.read_link() {
        // Remove any stale `previous` link first.
        let _ = fs::remove_file(&previous);
        std::os::unix::fs::symlink(&existing_target, &previous).with_context(|| {
            format!("creating previous → {}", existing_target.display())
        })?;
    }

    // Build the new `current.tmp` and rename onto `current`.
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(new_target, &tmp)
        .with_context(|| format!("symlink {} → {}", tmp.display(), new_target.display()))?;
    fs::rename(&tmp, &current)
        .with_context(|| format!("rename {} → {}", tmp.display(), current.display()))?;
    // fsync the install root so the rename is durable.
    if let Ok(d) = fs::File::open(install_root) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Sweep `versions/*` keeping the newest `keep` entries plus the
/// currently-symlinked target. Removes anything else, including
/// `*.partial` orphans.
pub fn gc_versions(install_root: &Path, current_version: &str, keep: usize) -> Result<()> {
    let versions_dir = install_root.join("versions");
    let entries: Vec<(PathBuf, std::time::SystemTime)> = fs::read_dir(&versions_dir)
        .with_context(|| format!("reading {}", versions_dir.display()))?
        .filter_map(|r| r.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            if name == current_version {
                return None; // Always keep the live one.
            }
            if name.ends_with(".partial") {
                // Always sweep partials.
                let _ = fs::remove_dir_all(&path);
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((path, mtime))
        })
        .collect();

    // Sort newest first, drop everything past `keep` entries.
    let mut sorted = entries;
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (path, _) in sorted.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(&path);
    }
    Ok(())
}

/// Apply a previously-staged manual upgrade. Reads `state.json`, finds
/// the most recent `versions/<v>/` whose status is `StagedManual`, and
/// flips the symlink. Called from a SIGUSR1 handler in `main.rs`.
pub fn manual_apply_pending(install_root: &Path) -> Result<()> {
    let state_path = super::state::path(install_root);
    let mut state = super::state::load(&state_path)?;
    if state.status != super::state::UpgradeStatus::StagedManual {
        bail!(
            "no manual-staged upgrade pending (state.status = {:?})",
            state.status
        );
    }
    let target = install_root.join("versions").join(&state.current_version);
    if !target.is_dir() {
        bail!("staged version directory missing: {}", target.display());
    }
    swap_current_symlink(install_root, &target)?;
    state.status = super::state::UpgradeStatus::PendingHealth;
    state.boot_attempts = 0;
    super::state::save(&state_path, &state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn locate_binary_finds_nested() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("bilbycast-edge-0.45.4-x86_64-linux-full");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("bilbycast-edge"), b"fake").unwrap();
        let p = locate_binary(dir.path(), "bilbycast-edge").unwrap();
        assert!(p.ends_with("bilbycast-edge"));
    }

    #[test]
    fn swap_current_symlink_creates_previous() {
        let dir = tempdir().unwrap();
        let v1 = dir.path().join("versions").join("0.45.3");
        let v2 = dir.path().join("versions").join("0.45.4");
        fs::create_dir_all(&v1).unwrap();
        fs::create_dir_all(&v2).unwrap();

        // Initial swap — no `previous` yet.
        swap_current_symlink(dir.path(), &v1).unwrap();
        assert_eq!(
            fs::read_link(dir.path().join("current")).unwrap(),
            v1
        );

        // Second swap — `previous` rotates from old `current`.
        swap_current_symlink(dir.path(), &v2).unwrap();
        assert_eq!(
            fs::read_link(dir.path().join("current")).unwrap(),
            v2
        );
        assert_eq!(
            fs::read_link(dir.path().join("previous")).unwrap(),
            v1
        );
    }
}
