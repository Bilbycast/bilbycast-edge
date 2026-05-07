// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Boot watchdog for staged upgrades.
//!
//! Lifecycle:
//!
//! ```text
//! systemd respawns into the new binary
//!     │
//!     ▼
//! main.rs calls run_boot_watchdog() BEFORE any flow init
//!     │
//!     ├── state.status == Stable          → return; no upgrade in flight
//!     ├── state.status == RolledBack      → emit upgrade_rolled_back Critical
//!     │                                       once on next manager auth
//!     │                                       (event sender saves it; the
//!     │                                       manager-client task drains)
//!     └── state.status == PendingHealth   → bump boot_attempts
//!                                           if > max_boot_attempts:
//!                                               revert symlink to `previous`,
//!                                               flip status to RolledBack,
//!                                               exit(1) — systemd respawns
//!                                               into the old binary.
//!                                           else:
//!                                               return; the manager-client
//!                                               task fires
//!                                               [`record_healthy_beat`] on
//!                                               first auth_ok, which after
//!                                               the boot_health_window flips
//!                                               state.status to Stable.
//! ```
//!
//! The watchdog is intentionally tiny — every byte of code here runs
//! before the rest of the edge boots, so a panic in this path is the
//! one failure mode the rest of the upgrade design can't catch.
//! Everything outside `bump_boot_attempts` is best-effort and falls
//! through cleanly when state.json is missing, malformed, or on a
//! filesystem the binary doesn't have permission to write.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;

use crate::config::models::UpgradeConfig;
use crate::manager::events::{category, EventSender, EventSeverity};

use super::state::{self, InstallState, UpgradeStatus};

/// Action the watchdog wants the rest of `main.rs` to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogOutcome {
    /// No upgrade in flight (status == Stable, or no state.json). Boot normally.
    Continue,
    /// We just rolled back. The events queued during this call should
    /// flush on first manager auth; the binary continues running on the
    /// reverted version.
    RolledBack {
        from_version: String,
        to_version: String,
    },
    /// The new binary is on its first or second boot since staging. The
    /// boot_attempts counter has been bumped; the manager-client task
    /// must call [`record_healthy_beat`] after the first successful auth.
    PendingHealth { attempt: u32 },
}

/// Run the boot watchdog. Called from `main.rs` immediately after
/// `load_config_split` and before any flow init.
///
/// `events` is used to queue the `upgrade_rolled_back` event when a
/// rollback is performed; the manager-client task drains the queue on
/// next auth so the manager UI sees the rollback.
pub fn run_boot_watchdog(
    upgrade_cfg: Option<&UpgradeConfig>,
    events: &EventSender,
) -> Result<WatchdogOutcome> {
    let Some(cfg) = upgrade_cfg else {
        return Ok(WatchdogOutcome::Continue);
    };
    if !cfg.enabled {
        return Ok(WatchdogOutcome::Continue);
    }

    let state_path = state::path(&cfg.install_root);
    if !state_path.exists() {
        // Brand new install — no upgrade history. The first staging run
        // will create state.json.
        return Ok(WatchdogOutcome::Continue);
    }

    let mut current = match state::load(&state_path) {
        Ok(s) => s,
        Err(e) => {
            // A corrupt state.json shouldn't prevent boot. Log + carry on
            // with `Continue`. The next staging run will overwrite it.
            tracing::warn!("upgrade watchdog: state.json unreadable, continuing: {e:#}");
            return Ok(WatchdogOutcome::Continue);
        }
    };

    match current.status {
        UpgradeStatus::Stable => Ok(WatchdogOutcome::Continue),
        UpgradeStatus::StagedManual => Ok(WatchdogOutcome::Continue),
        UpgradeStatus::RolledBack => {
            // We rolled back on the previous boot. Surface the event
            // once and flip back to Stable so we don't spam.
            let from = current
                .previous_version
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let to = current.current_version.clone();
            events.emit_with_details(
                EventSeverity::Critical,
                category::UPGRADE,
                format!(
                    "Upgrade to {from} rolled back automatically — running on {to}"
                ),
                None,
                serde_json::json!({
                    "error_code": "upgrade_rolled_back",
                    "from_version": from,
                    "to_version": to,
                }),
            );
            current.status = UpgradeStatus::Stable;
            current.boot_attempts = 0;
            let _ = state::save(&state_path, &current);
            Ok(WatchdogOutcome::RolledBack {
                from_version: from,
                to_version: to,
            })
        }
        UpgradeStatus::PendingHealth => {
            // Bump boot_attempts in place.
            let attempt = state::bump_boot_attempts(&state_path)?;
            if attempt > cfg.max_boot_attempts {
                // Out of budget — roll back.
                tracing::error!(
                    "upgrade watchdog: boot_attempts {attempt} exceeded \
                     max_boot_attempts {} — rolling back to previous version",
                    cfg.max_boot_attempts
                );
                if let Err(e) = revert_to_previous(&cfg.install_root, &mut current) {
                    tracing::error!("upgrade watchdog: revert FAILED: {e:#} — exiting anyway");
                }
                current.status = UpgradeStatus::RolledBack;
                let _ = state::save(&state_path, &current);
                events.emit_with_details(
                    EventSeverity::Critical,
                    category::UPGRADE,
                    format!(
                        "Upgrade to {} failed — reverted symlink to {}, exiting for respawn",
                        current.current_version,
                        current.previous_version.as_deref().unwrap_or("unknown"),
                    ),
                    None,
                    serde_json::json!({
                        "error_code": "upgrade_rolled_back",
                        "boot_attempts": attempt,
                        "from_version": current.current_version,
                        "to_version": current.previous_version,
                    }),
                );
                // The manager-client task hasn't started yet; the
                // emitted event sits in the bounded mpsc until the
                // OLD binary, after the next systemd respawn, picks
                // up `state.status == RolledBack` and flushes it on
                // first auth.
                std::process::exit(1);
            }
            Ok(WatchdogOutcome::PendingHealth { attempt })
        }
    }
}

/// Called from the manager-client task after the first successful
/// `auth_ok` on the new binary. Persists `last_health_at = now`. After
/// `boot_health_window_secs` of healthy beats, [`finalize_if_stable`]
/// promotes the install to `Stable` and emits `upgrade_completed`.
pub fn record_healthy_beat(install_root: &Path) {
    let p = state::path(install_root);
    let _ = state::update(&p, |s| {
        if s.status == UpgradeStatus::PendingHealth {
            s.last_health_at = Some(Utc::now());
        }
        Ok(())
    });
}

/// Periodic check (called from the same place that emits health beats)
/// that promotes `PendingHealth → Stable` once `boot_health_window_secs`
/// of clean health has accumulated since `staged_at`.
pub fn finalize_if_stable(
    install_root: &Path,
    cfg: &UpgradeConfig,
    events: &EventSender,
) {
    let p = state::path(install_root);
    let Ok(state) = state::load(&p) else { return; };
    if state.status != UpgradeStatus::PendingHealth {
        return;
    }
    let Some(last) = state.last_health_at else { return; };
    let healthy_for = (Utc::now() - state.staged_at).num_seconds().max(0) as u32;
    if healthy_for < cfg.boot_health_window_secs {
        return;
    }
    // Be sure the most recent beat is fresh too.
    let lag = (Utc::now() - last).num_seconds();
    if lag > 60 {
        return;
    }
    let from = state
        .previous_version
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let to = state.current_version.clone();
    let _ = state::update(&p, |s| {
        s.status = UpgradeStatus::Stable;
        Ok(())
    });
    events.emit_with_details(
        EventSeverity::Info,
        category::UPGRADE,
        format!("Upgrade to {to} stable after {healthy_for}s of healthy beats"),
        None,
        serde_json::json!({
            "error_code": "upgrade_completed",
            "from_version": from,
            "to_version": to,
            "healthy_seconds": healthy_for,
        }),
    );
}

/// Periodic background task — wraps [`finalize_if_stable`] in a tokio
/// loop with a 5 s tick. Spawn once from `main.rs` after the upgrade
/// coordinator is constructed.
pub async fn run_watchdog_periodic(
    install_root: std::path::PathBuf,
    cfg: UpgradeConfig,
    events: EventSender,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                finalize_if_stable(&install_root, &cfg, &events);
            }
        }
    }
}

/// Revert the `current` symlink to point at `state.previous_version`.
fn revert_to_previous(install_root: &Path, state: &mut InstallState) -> Result<()> {
    let prev_v = state
        .previous_version
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no previous_version to revert to"))?;
    let prev_target = install_root.join("versions").join(&prev_v);
    if !prev_target.is_dir() {
        anyhow::bail!(
            "previous version directory missing: {} — cannot revert",
            prev_target.display()
        );
    }
    let current = install_root.join("current");
    let tmp = install_root.join("current.tmp");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&prev_target, &tmp)?;
    fs::rename(&tmp, &current)?;
    // Swap the role labels so the next attempt has `previous` =
    // the failed version.
    let failed = std::mem::replace(&mut state.current_version, prev_v);
    state.previous_version = Some(failed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::events::event_channel;
    use tempfile::tempdir;

    fn cfg_for(install_root: &Path) -> UpgradeConfig {
        UpgradeConfig {
            enabled: true,
            allowed_channels: vec!["stable".to_string()],
            min_version: None,
            rollback_grace: 1,
            install_root: install_root.to_path_buf(),
            boot_health_window_secs: 60,
            max_boot_attempts: 3,
            manual_only: false,
        }
    }

    #[test]
    fn no_state_returns_continue() {
        let dir = tempdir().unwrap();
        let cfg = cfg_for(dir.path());
        let (sender, _rx) = event_channel();
        let outcome = run_boot_watchdog(Some(&cfg), &sender).unwrap();
        assert_eq!(outcome, WatchdogOutcome::Continue);
    }

    #[test]
    fn rollback_state_flushes_event_then_resets() {
        let dir = tempdir().unwrap();
        let cfg = cfg_for(dir.path());
        let (sender, mut rx) = event_channel();

        let s = InstallState {
            current_version: "0.45.3".into(),
            previous_version: Some("0.45.4".into()),
            channel: "stable".into(),
            variant: "default".into(),
            arch: "x86_64-linux".into(),
            status: UpgradeStatus::RolledBack,
            boot_attempts: 0,
            staged_at: Utc::now(),
            last_health_at: None,
            last_sequence: 1,
        };
        state::save(&state::path(dir.path()), &s).unwrap();

        let outcome = run_boot_watchdog(Some(&cfg), &sender).unwrap();
        match outcome {
            WatchdogOutcome::RolledBack { .. } => {}
            other => panic!("expected RolledBack, got {other:?}"),
        }
        let event = rx.try_recv().expect("event delivered");
        assert_eq!(event.category, category::UPGRADE);
        assert_eq!(event.severity, EventSeverity::Critical);
        // Status now reset to Stable.
        let after = state::load(&state::path(dir.path())).unwrap();
        assert_eq!(after.status, UpgradeStatus::Stable);
    }
}
