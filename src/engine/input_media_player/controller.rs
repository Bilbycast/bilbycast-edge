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
//! **Default off.** The controller only runs when explicitly enabled
//! ([`controller_enabled`]); otherwise the legacy sequential `run()` loop is
//! used unchanged. This is the plan's Phase-3 rollback guarantee — the
//! real-time path does not change until an operator opts in.

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

/// Whether the new controller path is enabled. Gated on an environment
/// variable so it is a genuine runtime switch with no config-schema or
/// protocol change: set `BILBYCAST_MEDIA_PLAYER_CONTROLLER=1` on the edge to
/// opt an install in. Absent / any other value → the legacy loop runs.
///
/// Read once per input spawn (not cached process-wide) so flipping the env
/// and restarting a flow is enough to change behaviour.
pub fn controller_enabled() -> bool {
    matches!(
        std::env::var("BILBYCAST_MEDIA_PLAYER_CONTROLLER").ok().as_deref(),
        Some("1") | Some("true") | Some("on")
    )
}

/// Whether the Phase-4 bounded incremental MP4/MOV reader is used instead of
/// the whole-file demux. Off by default → the deployed whole-file path runs
/// unchanged. Set `BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4=1` to opt in.
pub fn incremental_mp4_enabled() -> bool {
    matches!(
        std::env::var("BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4").ok().as_deref(),
        Some("1") | Some("true") | Some("on")
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
    fn controller_flag_reads_env() {
        // SAFETY: single-threaded within this test; the var is exclusive to
        // the controller tests and cleared before returning.
        unsafe {
            std::env::remove_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER");
            assert!(!controller_enabled(), "absent → disabled");
            for v in ["1", "true", "on"] {
                std::env::set_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER", v);
                assert!(controller_enabled(), "{v} should enable");
            }
            std::env::set_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER", "0");
            assert!(!controller_enabled(), "0 → disabled");
            std::env::remove_var("BILBYCAST_MEDIA_PLAYER_CONTROLLER");
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
