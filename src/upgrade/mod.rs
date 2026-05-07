// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Remote upgrade machinery for bilbycast-edge.
//!
//! High-level flow (driven from `manager/client.rs::execute_command` arm
//! `upgrade_binary`):
//!
//! 1. Manager sends `upgrade_binary { version, channel, target_arch?, variant? }`.
//! 2. [`UpgradeCoordinator::stage`] validates against [`UpgradeConfig`],
//!    rejects with one of the structured `error_code` values from
//!    [`error_codes`] when policy denies the request.
//! 3. The coordinator constructs a deterministic GitHub release URL
//!    (host whitelist enforced in [`manifest::derive_release_base_url`]),
//!    fetches `manifest.json` + `manifest.sig.bundle`, runs
//!    [`verify::verify_manifest_bundle`] which checks the Sigstore
//!    keyless signature against the compiled-in [`trust::ALLOWED_SIGNERS`]
//!    allowlist, then downloads the matching tarball with streaming
//!    SHA-256 verification.
//! 4. [`apply::stage_new_version`] extracts to `versions/<new>/`,
//!    `chmod 0755` the binary, fsyncs, then atomically swaps the
//!    `current` symlink. State persists via [`state::StateFile`] under
//!    a `flock(2)` so concurrent staging is impossible (single-flight
//!    enforced both in-process and on-disk).
//! 5. The coordinator emits `upgrade_staged` then asks the caller to
//!    drain flows and `exit(0)`. systemd respawns into the new binary
//!    via the `current` symlink.
//! 6. On the next boot, [`watchdog::run_boot_watchdog`] inspects
//!    `state.json`. If it sees `status = pending_health` with
//!    `boot_attempts > max_boot_attempts`, it rolls the symlink back to
//!    `previous` and emits `upgrade_rolled_back` Critical, then exits
//!    so systemd respawns into the old binary.
//!
//! The coordinator owns no `tokio` task itself — it is invoked from the
//! manager-client task and returns once staging completes (success or
//! failure). The single-flight guard is a Mutex<bool> so two `command`
//! frames arriving back-to-back can't race.

#![allow(dead_code)]

pub mod apply;
pub mod download;
pub mod manifest;
pub mod state;
pub mod trust;
pub mod verify;
pub mod watchdog;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::Mutex;

use crate::config::models::UpgradeConfig;
use crate::manager::events::{category, EventSender, EventSeverity};

#[allow(unused_imports)]
pub use manifest::{Manifest, ManifestArtefact};
pub use state::{InstallState, UpgradeStatus};
#[allow(unused_imports)]
pub use trust::{AllowedSigner, ALLOWED_SIGNERS};

/// Stable error-code strings layered onto `command_ack.error_code` and
/// `event.details.error_code` so the manager UI can target specific
/// failure modes (and CI can grep for them in the verification matrix).
pub mod error_codes {
    pub const UPGRADE_DISABLED: &str = "upgrade_disabled";
    pub const UPGRADE_CHANNEL_NOT_ALLOWED: &str = "upgrade_channel_not_allowed";
    pub const UPGRADE_VERSION_TOO_OLD: &str = "upgrade_version_too_old";
    pub const UPGRADE_VERSION_INVALID: &str = "upgrade_version_invalid";
    pub const UPGRADE_SEQUENCE_TOO_OLD: &str = "upgrade_sequence_too_old";
    pub const UPGRADE_SIGNATURE_INVALID: &str = "upgrade_signature_invalid";
    pub const UPGRADE_IDENTITY_NOT_ALLOWED: &str = "upgrade_identity_not_allowed";
    pub const UPGRADE_REKOR_INVALID: &str = "upgrade_rekor_invalid";
    pub const UPGRADE_IN_PROGRESS: &str = "upgrade_in_progress";
    pub const UPGRADE_URL_INVALID: &str = "upgrade_url_invalid";
    pub const UPGRADE_CHECKSUM_MISMATCH: &str = "upgrade_checksum_mismatch";
    pub const UPGRADE_EXTRACT_FAILED: &str = "upgrade_extract_failed";
    pub const UPGRADE_DISK_FULL: &str = "upgrade_disk_full";
    pub const UPGRADE_NETWORK_ERROR: &str = "upgrade_network_error";
    pub const UPGRADE_MANIFEST_INVALID: &str = "upgrade_manifest_invalid";
    pub const UPGRADE_VARIANT_MISMATCH: &str = "upgrade_variant_mismatch";
    pub const UPGRADE_ARCH_MISMATCH: &str = "upgrade_arch_mismatch";
    pub const UPGRADE_ROLLED_BACK: &str = "upgrade_rolled_back";
    pub const UPGRADE_COMPLETED: &str = "upgrade_completed";
}

/// Structured error returned from staging. The optional `code` lifts onto
/// `command_ack.error_code` so the manager UI can render a targeted error.
#[derive(Debug, Clone)]
pub struct UpgradeError {
    pub message: String,
    pub code: &'static str,
}

impl UpgradeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { message: message.into(), code }
    }
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for UpgradeError {}

/// Return type of a successful staging run.
#[derive(Debug, Clone)]
pub struct StagedUpgrade {
    pub from_version: String,
    pub to_version: String,
    pub channel: String,
    pub variant: String,
    pub arch: String,
    /// Path to `versions/<new>/` after extraction succeeded.
    pub install_dir: PathBuf,
}

/// Owns the single-flight guard and orchestrates the staging pipeline.
///
/// The coordinator is cheap to construct — there's no background task. The
/// `Arc<Mutex<()>>` guard is the only state. Callers spawn a `tokio::spawn`
/// around `stage()` if they want to ack the manager command immediately
/// while staging proceeds in the background.
#[derive(Clone)]
pub struct UpgradeCoordinator {
    config: Arc<tokio::sync::RwLock<UpgradeConfig>>,
    events: EventSender,
    /// Current binary version (read from `CARGO_PKG_VERSION`).
    current_version: String,
    /// Mutex<bool> implements the single-flight guard. We hold the lock for
    /// the duration of staging; a second concurrent `stage()` returns
    /// `upgrade_in_progress`.
    in_flight: Arc<Mutex<bool>>,
}

impl UpgradeCoordinator {
    pub fn new(config: UpgradeConfig, events: EventSender, current_version: String) -> Self {
        Self {
            config: Arc::new(tokio::sync::RwLock::new(config)),
            events,
            current_version,
            in_flight: Arc::new(Mutex::new(false)),
        }
    }

    /// Update the in-memory config (e.g. after `update_config` swaps the
    /// upgrades block).
    pub async fn set_config(&self, config: UpgradeConfig) {
        let mut g = self.config.write().await;
        *g = config;
    }

    /// Returns `true` if upgrades are currently enabled. Used by the
    /// manager command dispatcher to short-circuit before parsing.
    pub async fn enabled(&self) -> bool {
        self.config.read().await.enabled
    }

    /// Run the full staging pipeline. On success, the caller should drain
    /// flows and exit so systemd respawns into the new binary.
    pub async fn stage(
        &self,
        version: &str,
        channel: &str,
        target_arch: Option<&str>,
        variant: Option<&str>,
    ) -> Result<StagedUpgrade, UpgradeError> {
        // ── Single-flight guard ──
        let mut g = match self.in_flight.try_lock() {
            Ok(g) => g,
            Err(_) => {
                return Err(UpgradeError::new(
                    error_codes::UPGRADE_IN_PROGRESS,
                    "another upgrade is already staging",
                ));
            }
        };
        *g = true;

        let cfg = self.config.read().await.clone();
        // ── Policy gate ──
        if !cfg.enabled {
            return Err(UpgradeError::new(
                error_codes::UPGRADE_DISABLED,
                "upgrades.enabled = false on this node",
            ));
        }
        if !cfg.allowed_channels.iter().any(|c| c == channel) {
            return Err(UpgradeError::new(
                error_codes::UPGRADE_CHANNEL_NOT_ALLOWED,
                format!(
                    "channel {channel:?} not in allowed_channels {:?}",
                    cfg.allowed_channels
                ),
            ));
        }

        // Parse + bound-check requested version against current + min_version.
        let target_v = semver::Version::parse(version).map_err(|e| {
            UpgradeError::new(
                error_codes::UPGRADE_VERSION_INVALID,
                format!("requested version {version:?} is not valid semver: {e}"),
            )
        })?;
        let current_v = semver::Version::parse(&self.current_version).map_err(|e| {
            UpgradeError::new(
                error_codes::UPGRADE_VERSION_INVALID,
                format!(
                    "current version {:?} is not valid semver: {e}",
                    self.current_version
                ),
            )
        })?;
        manifest::check_version_window(&target_v, &current_v, &cfg).map_err(|e| {
            UpgradeError::new(error_codes::UPGRADE_VERSION_TOO_OLD, e.to_string())
        })?;

        // Resolve arch + variant.
        let arch = target_arch
            .map(|s| s.to_string())
            .unwrap_or_else(detect_arch);
        let variant = variant.unwrap_or(default_variant()).to_string();

        // Refuse cross-arch installs unless the operator explicitly asked
        // for one (e.g. provisioning a fleet snapshot from a control node).
        // The default-arch path always matches.
        if arch != detect_arch() && target_arch.is_some() {
            self.events.emit(
                EventSeverity::Warning,
                category::UPGRADE,
                format!(
                    "Manager requested cross-arch install: arch={arch}, host={}",
                    detect_arch()
                ),
            );
        }

        // ── Construct + verify URLs ──
        let release_base = manifest::derive_release_base_url(version)
            .map_err(|e| UpgradeError::new(error_codes::UPGRADE_URL_INVALID, e.to_string()))?;
        let manifest_url = format!("{release_base}/manifest.json");
        let bundle_url = format!("{release_base}/manifest.sig.bundle");

        self.events.emit_with_details(
            EventSeverity::Info,
            category::UPGRADE,
            format!("Staging upgrade to {version} ({channel}/{arch}/{variant})"),
            None,
            serde_json::json!({
                "error_code": "upgrade_started",
                "from_version": self.current_version,
                "to_version": version,
                "channel": channel,
                "arch": arch,
                "variant": variant,
            }),
        );

        // ── Fetch manifest + bundle ──
        let manifest_bytes = download::fetch_text(&manifest_url, MANIFEST_FETCH_TIMEOUT)
            .await
            .map_err(|e| {
                UpgradeError::new(
                    error_codes::UPGRADE_NETWORK_ERROR,
                    format!("manifest fetch failed: {e}"),
                )
            })?;
        let bundle_bytes = download::fetch_text(&bundle_url, MANIFEST_FETCH_TIMEOUT)
            .await
            .map_err(|e| {
                UpgradeError::new(
                    error_codes::UPGRADE_NETWORK_ERROR,
                    format!("bundle fetch failed: {e}"),
                )
            })?;

        // ── Verify Sigstore signature ──
        verify::verify_manifest_bundle(
            manifest_bytes.as_bytes(),
            bundle_bytes.as_bytes(),
            ALLOWED_SIGNERS,
        )
        .await
        .map_err(map_verify_error)?;

        // ── Parse + cross-check manifest fields ──
        let manifest: Manifest =
            serde_json::from_slice(manifest_bytes.as_bytes()).map_err(|e| {
                UpgradeError::new(
                    error_codes::UPGRADE_MANIFEST_INVALID,
                    format!("manifest is not valid JSON: {e}"),
                )
            })?;
        manifest
            .validate_request(version, channel, "edge")
            .map_err(|e| UpgradeError::new(error_codes::UPGRADE_MANIFEST_INVALID, e.to_string()))?;

        // Sequence monotonicity guard against rollback / replay even when
        // semver alone would otherwise allow it.
        let state_path = state::path(&cfg.install_root);
        let prev_state = state::load(&state_path).ok();
        if let Some(ref s) = prev_state {
            if manifest.sequence <= s.last_sequence {
                return Err(UpgradeError::new(
                    error_codes::UPGRADE_SEQUENCE_TOO_OLD,
                    format!(
                        "manifest sequence {} ≤ last installed sequence {}",
                        manifest.sequence, s.last_sequence
                    ),
                ));
            }
        }

        let artefact = manifest
            .pick_artefact(&arch, &variant)
            .ok_or_else(|| {
                UpgradeError::new(
                    error_codes::UPGRADE_ARCH_MISMATCH,
                    format!(
                        "manifest has no artefact for arch={arch} variant={variant}"
                    ),
                )
            })?;

        // ── Download tarball with streaming SHA-256 ──
        let tarball_bytes =
            download::fetch_with_sha256(&artefact.url, &artefact.sha256, TARBALL_FETCH_TIMEOUT)
                .await
                .map_err(map_download_error)?;

        self.events.emit_with_details(
            EventSeverity::Info,
            category::UPGRADE,
            format!("Downloaded upgrade tarball ({} bytes)", tarball_bytes.len()),
            None,
            serde_json::json!({
                "error_code": "upgrade_downloaded",
                "to_version": version,
                "size_bytes": tarball_bytes.len(),
            }),
        );

        if cfg.manual_only {
            self.events.emit_with_details(
                EventSeverity::Info,
                category::UPGRADE,
                "manual_only set — staging complete, awaiting SIGUSR1 to apply",
                None,
                serde_json::json!({ "error_code": "upgrade_staged_manual" }),
            );
            // Bail before symlink swap so a human operator approves the apply.
            return Err(UpgradeError::new(
                "upgrade_staged_manual",
                "staged successfully; manual_only is set and the symlink swap is deferred \
                 until the operator sends SIGUSR1 to the running process",
            ));
        }

        // ── Extract + atomic symlink swap ──
        let install_dir =
            apply::stage_new_version(&cfg.install_root, version, &tarball_bytes, &manifest)
                .map_err(map_apply_error)?;

        // Persist new state — pending_health, boot_attempts = 0, sequence
        // bumped. The boot watchdog reads this on next start.
        let prev_version = prev_state
            .as_ref()
            .map(|s| s.current_version.clone())
            .unwrap_or_else(|| self.current_version.clone());
        let new_state = InstallState {
            current_version: version.to_string(),
            previous_version: Some(prev_version.clone()),
            channel: channel.to_string(),
            variant: variant.clone(),
            arch: arch.clone(),
            status: UpgradeStatus::PendingHealth,
            boot_attempts: 0,
            staged_at: chrono::Utc::now(),
            last_health_at: None,
            last_sequence: manifest.sequence,
        };
        state::save(&state_path, &new_state).map_err(|e| {
            UpgradeError::new(
                error_codes::UPGRADE_EXTRACT_FAILED,
                format!("state.json write failed: {e}"),
            )
        })?;

        self.events.emit_with_details(
            EventSeverity::Info,
            category::UPGRADE,
            format!("Upgrade staged to {version} — draining flows, will exit for respawn"),
            None,
            serde_json::json!({
                "error_code": "upgrade_staged",
                "from_version": self.current_version,
                "to_version": version,
                "channel": channel,
                "arch": arch,
                "variant": variant,
            }),
        );

        Ok(StagedUpgrade {
            from_version: self.current_version.clone(),
            to_version: version.to_string(),
            channel: channel.to_string(),
            variant,
            arch,
            install_dir,
        })
    }
}

/// 30 s is more than enough for a tiny manifest fetch on any link with > 64 kbps.
const MANIFEST_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Tarballs hit ~120 MB for the `-full` variant; allow up to 10 minutes for
/// truly bandwidth-starved nodes (e.g. cellular bonding under load).
const TARBALL_FETCH_TIMEOUT: Duration = Duration::from_secs(600);

/// Detect the running host's release-channel arch tag.
pub fn detect_arch() -> String {
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-linux".to_string()
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-linux".to_string()
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-darwin".to_string()
    } else {
        // Fall back to the canonical Rust target triple. The release pipeline
        // doesn't ship binaries for unsupported targets, so the manifest
        // lookup will fail with `upgrade_arch_mismatch` — which is the right
        // outcome rather than guessing at a tarball that doesn't exist.
        format!(
            "{}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS,
        )
    }
}

/// Detect the running build's variant tag based on compiled-in features.
fn default_variant() -> &'static str {
    if cfg!(any(
        feature = "video-encoder-x264",
        feature = "video-encoder-x265",
    )) {
        "full"
    } else {
        "default"
    }
}

fn map_verify_error(e: anyhow::Error) -> UpgradeError {
    let s = e.to_string();
    let code = if s.contains("identity") {
        error_codes::UPGRADE_IDENTITY_NOT_ALLOWED
    } else if s.contains("rekor") {
        error_codes::UPGRADE_REKOR_INVALID
    } else {
        error_codes::UPGRADE_SIGNATURE_INVALID
    };
    UpgradeError::new(code, s)
}

fn map_download_error(e: anyhow::Error) -> UpgradeError {
    let s = e.to_string();
    let code = if s.contains("checksum") || s.contains("SHA-256") {
        error_codes::UPGRADE_CHECKSUM_MISMATCH
    } else {
        error_codes::UPGRADE_NETWORK_ERROR
    };
    UpgradeError::new(code, s)
}

fn map_apply_error(e: anyhow::Error) -> UpgradeError {
    let s = e.to_string();
    let code = if s.contains("No space left") || s.contains("disk full") || s.contains("ENOSPC") {
        error_codes::UPGRADE_DISK_FULL
    } else {
        error_codes::UPGRADE_EXTRACT_FAILED
    };
    UpgradeError::new(code, s)
}

// The category::UPGRADE constant lives in `crate::manager::events` so the
// events module remains the single source of truth for category strings.

/// Convenience: error → `(error, error_code)` tuple for the manager command
/// dispatcher to lift onto `command_ack`.
impl From<UpgradeError> for (String, &'static str) {
    fn from(e: UpgradeError) -> Self {
        (e.message, e.code)
    }
}

/// Helper for the dispatcher: construct an [`UpgradeError`] from an
/// arbitrary error chain by mapping the message into a network-error code.
/// Only used for the few call sites that already discarded the original
/// `UpgradeError`.
impl From<anyhow::Error> for UpgradeError {
    fn from(e: anyhow::Error) -> Self {
        UpgradeError::new(error_codes::UPGRADE_NETWORK_ERROR, e.to_string())
    }
}

/// Module-private helper: surface a download error as an [`UpgradeError`]
/// with the `network_error` code.
#[allow(dead_code)]
fn into_network_error(e: impl std::fmt::Display) -> UpgradeError {
    UpgradeError::new(error_codes::UPGRADE_NETWORK_ERROR, e.to_string())
}

/// Type alias for the upgrade module's result type, used internally.
type UpgradeResult<T> = std::result::Result<T, UpgradeError>;

/// Helper used by tests to build an `UpgradeCoordinator` that targets a
/// scratch `tempfile::TempDir` install root.
#[cfg(test)]
pub(crate) fn test_coordinator(install_root: PathBuf, current: &str) -> UpgradeCoordinator {
    let cfg = UpgradeConfig {
        enabled: true,
        allowed_channels: vec!["stable".to_string(), "nightly".to_string()],
        min_version: None,
        rollback_grace: 1,
        install_root,
        boot_health_window_secs: 120,
        max_boot_attempts: 3,
        manual_only: false,
    };
    let (events, _rx) = crate::manager::events::event_channel();
    UpgradeCoordinator::new(cfg, events, current.to_string())
}

/// Re-export of the boot watchdog entry point so `main.rs` can call it
/// without reaching into the submodule directly.
pub use watchdog::run_boot_watchdog;

/// Re-export of the manual-apply hook so a SIGUSR1 handler can drive it.
#[allow(unused_imports)]
pub use apply::manual_apply_pending;

/// Process-wide coordinator handle. Set once at startup via
/// [`install_global_coordinator`] and read by the manager command
/// dispatcher.
///
/// Centralising the handle here keeps the single-flight guard
/// (`UpgradeCoordinator::in_flight`) effective across the multiple
/// command frames that may arrive back-to-back: every dispatch grabs
/// the same `Arc<UpgradeCoordinator>` rather than constructing a fresh
/// one per call.
static GLOBAL: std::sync::OnceLock<UpgradeCoordinator> = std::sync::OnceLock::new();

/// Install the process-wide coordinator. Idempotent — subsequent calls
/// are silently ignored (the first `set` wins). Safe to call from
/// `main.rs` after the upgrade config has been loaded; safe to skip if
/// the operator has not configured `upgrades`.
pub fn install_global_coordinator(coord: UpgradeCoordinator) {
    let _ = GLOBAL.set(coord);
}

/// Borrow the process-wide coordinator. Returns `None` if no upgrade
/// config was wired at startup — the manager command dispatcher then
/// returns `unknown_action`-equivalent so the manager UI degrades.
pub fn global_coordinator() -> Option<&'static UpgradeCoordinator> {
    GLOBAL.get()
}

#[allow(unused)]
fn _ensure_anyhow_in_scope() -> Result<()> {
    Err(anyhow!("placeholder"))
}
