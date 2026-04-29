// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Replay store: continuous flow recording to disk + playback as a
//! fresh input.
//!
//! The store is a flat directory on the edge's local disk holding one
//! sub-directory per recording. Each sub-directory holds rolling
//! 188-byte-aligned MPEG-TS segments (`NNNNNN.ts`), a binary timecode →
//! byte-offset index (`index.bin`), a `recording.json` metadata file,
//! and a `clips.json` list of named in/out marks.
//!
//! # On-disk layout
//!
//! ```text
//! <replay_root>/
//!   <recording_id>/
//!     000000.ts            ← rolling TS segment (188-byte aligned)
//!     000001.ts
//!     ...
//!     recording.json       ← created_at, segment_seconds, schema_version
//!     index.bin            ← binary index, fixed-size 24-byte entries
//!     clips.json           ← [{id, name, in_pts, out_pts, ...}]
//!     .tmp/                ← in-flight segment writes
//! ```
//!
//! # Reuse
//!
//! - Disk I/O discipline mirrors [`crate::media`]: chunked write,
//!   `fsync_all` on close, atomic rename out of `.tmp/`.
//! - The recording writer is a sibling broadcast subscriber on the
//!   flow's `broadcast::Sender<RtpPacket>` — never blocks the data
//!   path. Drop-on-lag with a Critical event mirrors
//!   [`crate::engine::output_udp`].
//! - SMPTE timecode extraction reuses
//!   [`crate::engine::content_analysis::timecode::TimecodeTracker`].

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub mod clips;
pub mod filmstrip;
pub mod index;
pub mod paced_replayer;
pub mod reader;
pub mod writer;

/// Resolve the replay storage root. Mirrors [`crate::media::media_dir`]
/// with a `replay/` suffix and a `BILBYCAST_REPLAY_DIR` override.
pub fn replay_root() -> PathBuf {
    if let Ok(p) = std::env::var("BILBYCAST_REPLAY_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("bilbycast").join("replay");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".bilbycast").join("replay");
    }
    PathBuf::from("./replay")
}

/// Per-recording directory under the replay root.
pub fn recording_dir(recording_id: &str) -> PathBuf {
    replay_root().join(recording_id)
}

/// Filesystem free / total bytes for the replay root, via `statvfs(3)`.
/// Cross-platform across Linux + macOS (the only edge targets).
/// Returns `None` if the root doesn't exist yet or `statvfs` fails — the
/// caller treats that as "no signal" (don't surface a meter the operator
/// can't trust). Surfaced on the `recording_status` WS response so the
/// manager UI can render a disk meter even when the recorder is armed
/// without a `max_bytes` cap.
pub fn replay_disk_usage() -> Option<(u64, u64)> {
    use std::ffi::CString;
    let root = replay_root();
    if !root.exists() {
        return None;
    }
    let bytes = root.as_os_str().to_str()?;
    let c_path = CString::new(bytes).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let block_size = stat.f_frsize as u64;
    let free = (stat.f_bavail as u64).saturating_mul(block_size);
    let total = (stat.f_blocks as u64).saturating_mul(block_size);
    if total == 0 {
        return None;
    }
    Some((free, total))
}

/// Persisted clip descriptor. One entry per (in, out) mark, materialised
/// on `mark_out` and persisted (with fsync) into `clips.json` before the
/// `command_ack` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipInfo {
    /// `clp_<ulid>` — generated edge-side, stable across restarts.
    pub id: String,
    /// Recording this clip belongs to.
    pub recording_id: String,
    /// Operator-supplied label (max 256 chars).
    pub name: String,
    /// Optional longer description (max 4096 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// In-point as 90 kHz PTS (PCR-derived).
    pub in_pts_90khz: u64,
    /// Out-point as 90 kHz PTS.
    pub out_pts_90khz: u64,
    /// Optional SMPTE TC at the in-point ("HH:MM:SS:FF").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smpte_in: Option<String>,
    /// Optional SMPTE TC at the out-point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smpte_out: Option<String>,
    /// Created-at as Unix seconds.
    pub created_at_unix: u64,
    /// Optional opaque user identifier captured from the WS command's
    /// originating session (kept as a free-form string so the manager
    /// can decide how to attribute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// Phase 2 — operator-supplied short labels (e.g., `GOAL`, `FOUL`,
    /// `OFFSIDE`, `VAR-CHECK`) used by the manager UI's quick-tag bar
    /// and tag-pill clip-list filter. Bounds are validated at the WS
    /// dispatcher (`replay_invalid_tag`) — each tag matches
    /// `^[A-Z0-9_-]{1,32}$`, max 16 tags per clip. Empty by default
    /// for legacy clips loaded from disk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl ClipInfo {
    /// Duration of the clip in milliseconds (rounded down). PTS arithmetic
    /// is mod-2^33 in a real broadcast stream; the recorder's writer
    /// hands us a monotonic 64-bit PTS so we can subtract directly.
    pub fn duration_ms(&self) -> u64 {
        if self.out_pts_90khz <= self.in_pts_90khz {
            return 0;
        }
        // 90 kHz → ms: divide by 90.
        (self.out_pts_90khz - self.in_pts_90khz) / 90
    }
}

/// Acknowledgement returned by `mark_in`. Captures both the PTS the
/// recorder is currently observing and the SMPTE TC at that PTS, when
/// known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkInAck {
    pub pts_90khz: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smpte_tc: Option<String>,
}

/// Acknowledgement returned by `scrub_playback`. Reports the PTS the
/// reader landed on (snapped to the nearest IDR ≤ requested PTS) plus
/// the segment + offset, useful for UI debugging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubAck {
    pub pts_90khz: u64,
    pub segment_id: u32,
    pub byte_offset: u32,
}

/// Commands routed from the WS dispatcher to the recording writer task.
/// Each variant carries a oneshot channel for the dispatcher to await
/// the result before sending `command_ack`.
#[derive(Debug)]
pub enum RecordingCommand {
    /// Arm the writer. Idempotent — already-armed writers reply with the
    /// existing recording_id.
    Start {
        reply: oneshot::Sender<Result<String>>,
    },
    /// Disarm the writer (closes the current segment, fsyncs, leaves the
    /// recording on disk for later playback).
    Stop {
        reply: oneshot::Sender<Result<()>>,
    },
    /// Set the in-point. `explicit_pts` lets the operator mark a non-live
    /// point (rare in JKL workflows but useful for scripted pipelines).
    /// Defaults to whatever PTS the recorder is currently observing.
    MarkIn {
        explicit_pts: Option<u64>,
        reply: oneshot::Sender<Result<MarkInAck>>,
    },
    /// Materialise a clip from the current in-point to "now" (or
    /// `explicit_out` when set). Persists `clips.json` with fsync.
    MarkOut {
        name: Option<String>,
        description: Option<String>,
        explicit_out_pts: Option<u64>,
        created_by: Option<String>,
        reply: oneshot::Sender<Result<ClipInfo>>,
    },
}

/// Commands routed from the WS dispatcher to the active replay input
/// task. The dispatcher resolves the active input via the FlowRuntime
/// before sending — only the active replay input ever receives these.
#[derive(Debug)]
pub enum ReplayCommand {
    /// Pre-load a clip without starting playback.
    Cue {
        clip_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Start playback. With `clip_id` set, plays that clip's range.
    /// With `from_pts`/`to_pts`, plays the explicit range. Both unset =
    /// play from current cued position (or beginning of recording).
    ///
    /// Phase 2.4 — `speed` clamps to `(0.0, 1.0]`; the pacer rewrites
    /// PCR + PES PTS/DTS so the downstream decoder's clock advances at
    /// wall-clock rate even at sub-1.0× speed. `None` = 1.0× (unchanged
    /// from Phase 1). Phase 2.5 — `start_at_unix_ms` is a future
    /// wall-clock anchor; the input task sleeps until that instant
    /// before flipping to playing, used by the multi-cam sync group to
    /// align N members within ~1 frame.
    Play {
        clip_id: Option<String>,
        from_pts_90khz: Option<u64>,
        to_pts_90khz: Option<u64>,
        speed: Option<f32>,
        start_at_unix_ms: Option<u64>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Stop playback. The replay input goes idle (NULL-PID padding).
    Stop {
        reply: oneshot::Sender<Result<()>>,
    },
    /// Seek to PTS. Snaps to nearest IDR ≤ requested PTS.
    Scrub {
        pts_90khz: u64,
        reply: oneshot::Sender<Result<ScrubAck>>,
    },
    /// Phase 2.4 — change playback speed live. `speed ∈ (0.0, 1.0]`.
    /// Re-anchors the pacer (`pcr_in/out` snap to the most recent
    /// observed PCR) so the output clock stays continuous across the
    /// speed change.
    SetSpeed {
        speed: f32,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Phase 2.4 — frame step. `direction = Forward` pumps exactly
    /// one PES access unit on the video PID and pauses; `Backward`
    /// snaps to the previous IDR via the in-memory index (coarse —
    /// frame-accurate backward step is Phase 3).
    StepFrame {
        direction: StepDirection,
        reply: oneshot::Sender<Result<ScrubAck>>,
    },
}

/// Frame-step direction for [`ReplayCommand::StepFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepDirection {
    Forward,
    Backward,
}
