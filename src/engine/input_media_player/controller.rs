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

/// Node-wide media-player defaults, resolved from `config.tuning` (with the
/// deprecated environment variables as the layer *below* it) and installed at
/// boot and on every `update_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPlayerDefaults {
    /// Node default for [`controller_enabled`].
    pub controller: bool,
    /// Node default for PCR-anchored TS playout pacing.
    pub pcr_deadlines: bool,
}

impl Default for MediaPlayerDefaults {
    fn default() -> Self {
        Self {
            controller: true,
            pcr_deadlines: true,
        }
    }
}

// Initialised to the built-in defaults rather than a "not yet installed"
// sentinel, because these two have no third state: unit tests and any binary
// that never loads a config get exactly the documented default. Relaxed
// ordering is sufficient — each is an independent advisory flag read at input
// spawn, and a spawn racing an `update_config` legitimately sees either value.
static NODE_CONTROLLER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static NODE_PCR_DEADLINES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Install the node-wide media-player defaults.
///
/// Atomics rather than a `OnceLock` deliberately: the probe switches next door
/// used a `OnceLock` and silently swallowed every pushed change, so a Tuning
/// value validated, persisted, echoed on `GetConfig` and did nothing until the
/// process restarted. Called from `main.rs` at boot **and** from
/// `manager::client` on `update_config`, so a push reaches the next input spawn.
pub fn install_media_player_defaults(defaults: MediaPlayerDefaults) {
    use std::sync::atomic::Ordering;
    NODE_CONTROLLER.store(defaults.controller, Ordering::Relaxed);
    NODE_PCR_DEADLINES.store(defaults.pcr_deadlines, Ordering::Relaxed);
}

/// The installed node-wide defaults, or the built-in ones if
/// [`install_media_player_defaults`] was never called.
pub fn media_player_defaults() -> MediaPlayerDefaults {
    use std::sync::atomic::Ordering;
    MediaPlayerDefaults {
        controller: NODE_CONTROLLER.load(Ordering::Relaxed),
        pcr_deadlines: NODE_PCR_DEADLINES.load(Ordering::Relaxed),
    }
}

/// Whether the operator-control (transition) path is enabled for a player.
///
/// **On by default** as of the 2026-07 media-player hardening. Resolution
/// order:
///  1. The per-input config field [`MediaPlayerInputConfig::operator_control`]
///     (`cfg`) wins when set — an explicit `Some(true)`/`Some(false)`.
///  2. Otherwise the node-wide default — `tuning.media_player_controller`,
///     with the deprecated `BILBYCAST_MEDIA_PLAYER_CONTROLLER` beneath it.
///  3. Otherwise → enabled (the default).
///
/// Read once per input spawn (not cached process-wide) so a pushed config
/// change plus a flow restart is enough to change behaviour. Also read on
/// every health tick to decide whether `media-player-control-v1` is
/// advertised, so turning it off node-wide withdraws the manager's **Next**
/// button rather than leaving one that refuses.
///
/// [`MediaPlayerInputConfig::operator_control`]: crate::config::models::MediaPlayerInputConfig::operator_control
pub fn controller_enabled(cfg: Option<bool>) -> bool {
    cfg.unwrap_or_else(|| media_player_defaults().controller)
}

/// Whether TS playout paces on deadlines anchored to the asset's own PCR.
///
/// Same two-layer resolution as [`controller_enabled`]: per-input
/// [`MediaPlayerInputConfig::pcr_deadlines`], then
/// `tuning.media_player_pcr_deadlines`, then the built-in `true`.
///
/// [`MediaPlayerInputConfig::pcr_deadlines`]: crate::config::models::MediaPlayerInputConfig::pcr_deadlines
pub fn pcr_deadlines_enabled(cfg: Option<bool>) -> bool {
    cfg.unwrap_or_else(|| media_player_defaults().pcr_deadlines)
}

/// Whether the Phase-4 bounded incremental MP4/MOV reader is used instead of
/// the whole-file demux.
///
/// **Unconditional in release builds**, and deliberately *not* a `tuning`
/// field. The whole-file demux this can select loads an entire MP4 into RAM —
/// a 4 GiB asset is a 4 GiB resident spike, which is the media-player OOM the
/// bounded reader was written to fix. A knob whose "off" position is a known
/// out-of-memory does not belong on an operator's screen, so it was not
/// migrated to config with its two siblings; it survives under
/// `cfg(debug_assertions)` for diagnostics only, the same treatment
/// `BILBYCAST_TESTBED_SHARED_WALLCLOCK` got in the same migration.
///
/// The whole-file path itself is retained (it backs a large body of tests and
/// the `DemuxCache` byte-loop) — only the operator-reachable route to it is
/// closed.
pub fn incremental_mp4_enabled() -> bool {
    #[cfg(debug_assertions)]
    if matches!(
        std::env::var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4")
            .ok()
            .as_deref(),
        Some("0") | Some("false") | Some("off")
    ) {
        tracing::warn!(
            "BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4 is set — falling back to the \
             whole-file MP4 demux, which holds the entire asset resident. Debug \
             builds only; a release binary ignores this."
        );
        return false;
    }
    true
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

    /// The node defaults are process-global atomics, so a test that installs
    /// one must not interleave with another that reads it. Same reasoning as
    /// `config::env_compat`'s `ENV_GUARD`; taken by **every** test that
    /// touches them, not only the ones that write — a reader outside the
    /// critical section is the flake, and that exact mistake had to be fixed
    /// twice already in this migration.
    static DEFAULTS_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Install `defaults`, run `f`, then restore the built-in defaults —
    /// including on panic, so one failed assertion cannot leave the whole
    /// process with the controller disabled.
    fn with_defaults<T>(defaults: MediaPlayerDefaults, f: impl FnOnce() -> T) -> T {
        let _guard = DEFAULTS_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        install_media_player_defaults(defaults);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        install_media_player_defaults(MediaPlayerDefaults::default());
        match out {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn controller_defaults_on_and_per_input_beats_the_node_default() {
        let _guard = DEFAULTS_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Nothing installed → the built-in default.
        assert!(controller_enabled(None), "absent → enabled (the default)");
        assert!(controller_enabled(Some(true)), "config true → enabled");
        assert!(!controller_enabled(Some(false)), "config false → disabled");
    }

    #[test]
    fn the_node_default_answers_only_when_the_input_is_silent() {
        with_defaults(
            MediaPlayerDefaults {
                controller: false,
                ..Default::default()
            },
            || {
                assert!(!controller_enabled(None), "node default off → disabled");
                // The per-input field must win, or the node-wide switch would
                // silently override an operator's explicit per-input choice —
                // the inverted precedence this migration exists to prevent.
                assert!(
                    controller_enabled(Some(true)),
                    "explicit per-input true must beat a node default of false"
                );
            },
        );
        // ...and the guard restored it.
        assert!(controller_enabled(None), "default restored after the test");
    }

    #[test]
    fn pcr_deadlines_resolve_the_same_way() {
        with_defaults(
            MediaPlayerDefaults {
                pcr_deadlines: false,
                ..Default::default()
            },
            || {
                assert!(!pcr_deadlines_enabled(None), "node default off → byte-rate");
                assert!(
                    pcr_deadlines_enabled(Some(true)),
                    "explicit per-input true must beat a node default of false"
                );
            },
        );
        assert!(pcr_deadlines_enabled(None), "default restored");
        assert!(!pcr_deadlines_enabled(Some(false)), "config false → byte-rate");
    }

    #[test]
    fn incremental_mp4_is_unconditional_in_release_and_divertible_in_debug() {
        // SAFETY: single-threaded within this test; the var is exclusive to
        // this test and cleared before returning.
        unsafe {
            std::env::remove_var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4");
            assert!(incremental_mp4_enabled(), "absent → incremental (the default)");
            for v in ["0", "false", "off"] {
                std::env::set_var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4", v);
                // The whole point of the debug gate: a release binary cannot be
                // talked into the whole-file path (and its 4 GiB resident spike)
                // by an environment variable, but a debug build still can, so
                // the whole-file demux stays reachable for diagnostics.
                assert_eq!(
                    incremental_mp4_enabled(),
                    !cfg!(debug_assertions),
                    "{v}: debug diverts to whole-file, release ignores it"
                );
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
