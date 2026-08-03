// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Player controller command surface (Phase 3b).
//!
//! Defines the per-input command channel the manager routes operator `Next`
//! through (mirroring the replay input's `ReplayCommand` design), the
//! readiness check that keeps a `Next` from cutting to an unplayable source,
//! and the runtime flag that gates the whole controller path.
//!
//! The controller loop itself lives in `input_media_player.rs` because it is
//! tightly coupled to that module's private `play_source` / `PlayerSession`.
//! Everything here is independent, small, and testable.
//!
//! **Default on** (as of the 2026-07 hardening). The controller runs unless a
//! player is explicitly opted out ([`controller_enabled`] — per-input
//! `operator_control: false`, or the node-wide
//! `BILBYCAST_MEDIA_PLAYER_CONTROLLER=0` escape hatch), in which case the
//! legacy sequential `run()` loop is used unchanged — the Phase-3 rollback
//! path is preserved as an opt-out rather than the default.

use tokio::sync::oneshot;

use super::transition::NextOutcome;
use crate::config::models::MediaPlayerSource;

/// Runtime commands delivered to a running media-player input. One variant
/// for now — operator `Next` — but the enum leaves room for future transport
/// controls without another channel.
#[derive(Debug)]
pub enum MediaPlayerCommand {
    /// Skip to the next playlist item. Carries the generation the caller
    /// believes is active (for idempotent retries / double-click safety) and
    /// a reply channel the controller answers on.
    Next {
        expected_generation: u64,
        request_id: String,
        reply: oneshot::Sender<NextOutcome>,
    },
}

/// Whether the operator-control (transition) path is enabled for a player.
///
/// **On by default** as of the 2026-07 media-player hardening. Resolution
/// order:
///  1. The per-input config field [`MediaPlayerInputConfig::operator_control`]
///     (`cfg`) wins when set — an explicit `Some(true)`/`Some(false)`.
///  2. Otherwise the node-wide `BILBYCAST_MEDIA_PLAYER_CONTROLLER` env var acts
///     as an escape hatch: `0` / `false` / `off` forces the legacy loop.
///  3. Otherwise → enabled (the new default).
///
/// Read once per input spawn (not cached process-wide) so editing the config
/// (or flipping the env) and restarting a flow is enough to change behaviour.
///
/// [`MediaPlayerInputConfig::operator_control`]: crate::config::models::MediaPlayerInputConfig::operator_control
pub fn controller_enabled(cfg: Option<bool>) -> bool {
    if let Some(explicit) = cfg {
        return explicit;
    }
    !matches!(
        std::env::var("BILBYCAST_MEDIA_PLAYER_CONTROLLER").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Whether the Phase-4 bounded incremental MP4/MOV reader is used instead of
/// the whole-file demux.
///
/// **On by default** as of the 2026-07 media-player hardening: the whole-file
/// demux loads an entire MP4 into RAM and was the prime contributor to the
/// media-player OOM (a 4 GiB asset is a 4 GiB resident spike); the bounded
/// reader keeps residency flat and was soak-validated on bilby-bite. The env
/// var is now an **opt-out** escape hatch for rollback:
/// `BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4=0` (also `false` / `off`) forces the
/// legacy whole-file path. Absent or any other value → incremental.
pub fn incremental_mp4_enabled() -> bool {
    !matches!(
        std::env::var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Cheap readiness check for a transition target: can this source be opened
/// right now? For TS and Still Image this is the whole story (open is fast),
/// so it is sufficient for the Phase-3b "never cut to a source that isn't
/// ready" guarantee without the concurrent read-ahead prepared-source that
/// Phase 4 adds. An MP4 that resolves and exists is considered ready here;
/// its heavier open cost is a Phase-4 concern.
///
/// Returns `false` when the file is missing or the name doesn't resolve — the
/// controller then declines the cut and keeps the current source on air
/// rather than cutting to dead air.
pub fn source_openable(source: &MediaPlayerSource) -> bool {
    let name = match source {
        MediaPlayerSource::Ts { name, .. } => name,
        MediaPlayerSource::Mp4 { name } => name,
        MediaPlayerSource::Image { name, .. } => name,
    };
    match super::resolve_media_path(name) {
        Ok(path) => path.is_file(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: env mutation is `unsafe` in this edition (process-global,
    // thread-unsafe). These two tests are the only ones touching this var, and
    // both fully set-then-clear it; grouped into one test so they can't race
    // each other across cargo's parallel threads.
    #[test]
    fn controller_defaults_on_config_wins_env_opts_out() {
        // SAFETY: single-threaded within this test; the var is exclusive to
        // the controller tests and cleared before returning.
        unsafe {
            std::env::remove_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER");
            // Default (no config override, no env) → enabled.
            assert!(controller_enabled(None), "absent → enabled (new default)");
            // Explicit per-input config wins regardless of env.
            assert!(controller_enabled(Some(true)), "config true → enabled");
            assert!(!controller_enabled(Some(false)), "config false → disabled");
            // Env escape hatch forces off only when config is unset.
            for v in ["0", "false", "off"] {
                std::env::set_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER", v);
                assert!(!controller_enabled(None), "env {v} → disabled");
                assert!(
                    controller_enabled(Some(true)),
                    "config true overrides env {v}"
                );
            }
            // Any non-opt-out env value keeps the default.
            for v in ["1", "true", "on"] {
                std::env::set_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER", v);
                assert!(controller_enabled(None), "env {v} → enabled");
            }
            std::env::remove_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER");
        }
    }

    #[test]
    fn incremental_mp4_defaults_on_and_opts_out() {
        // SAFETY: single-threaded within this test; the var is exclusive to
        // this test and cleared before returning.
        unsafe {
            std::env::remove_var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4");
            assert!(incremental_mp4_enabled(), "absent → incremental (new default)");
            for v in ["0", "false", "off"] {
                std::env::set_var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4", v);
                assert!(!incremental_mp4_enabled(), "{v} should force whole-file");
            }
            for v in ["1", "true", "on", "anything"] {
                std::env::set_var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4", v);
                assert!(incremental_mp4_enabled(), "{v} → incremental");
            }
            std::env::remove_var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4");
        }
    }

    #[test]
    fn missing_file_is_not_openable() {
        let src = MediaPlayerSource::Ts {
            name: "definitely-not-a-real-file-xyz.ts".to_string(),
            program_number: None,
        };
        assert!(!source_openable(&src));
    }
}
