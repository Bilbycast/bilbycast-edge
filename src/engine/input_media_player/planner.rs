// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Playlist compatibility planner (Phase 2 of the media-player redesign).
//!
//! Given the content-derived [`AssetManifest`] of each playlist entry, the
//! planner classifies **every adjacent boundary** — including the last→first
//! wrap when `loop_playback` is set — before playout starts, so an operator
//! sees an incompatible splice at save/start time rather than as an on-air
//! glitch. It answers one question per boundary: *can the player cross from
//! asset A to asset B, and if so, how much has to change?*
//!
//! The planner is a **pure function over manifests** ([`plan_playlist`]) so it
//! is fully unit-testable without touching files, the demuxer, or the
//! scheduler. The WS layer gathers manifests and calls it.
//!
//! ## Transition classes (plan §7.1)
//!
//! | Class | Meaning |
//! |---|---|
//! | `native_continuous` | Same program/codec-config/clock contract — PSI stable, timestamps continue, no decoder reset. |
//! | `native_discontinuity` | Remuxable without transcode, but decoder/program state changes — new codec config, PMT bump if layout changes, one discontinuity indication, start at a random-access point. |
//! | `normalise_required` | A native transition would violate the selected output contract — decode/process/encode to the playlist profile. |
//! | `unsupported` | No valid native or normalised path on this node. |
//!
//! ## Policy (plan §7.3)
//!
//! Under **native** policy the planner preserves each file's elementary
//! format and signals boundaries; format changes are `native_discontinuity`,
//! never silently transcoded. Under **normalised** policy every item targets
//! one output profile, so any format difference is `normalise_required`.
//! `on_incompatible` is `reject` only for now — automatic fallback to
//! transcode would silently consume hardware and is deliberately not implicit.

use serde::{Deserialize, Serialize};

use crate::media::manifest::{AssetManifest, ManifestMediaType, ProbeState};

/// Output policy for the whole playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackPolicy {
    pub mode: PolicyMode,
    #[serde(default)]
    pub on_incompatible: OnIncompatible,
}

impl Default for PlaybackPolicy {
    fn default() -> Self {
        PlaybackPolicy { mode: PolicyMode::Native, on_incompatible: OnIncompatible::Reject }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Preserve each file's supported elementary format; signal boundaries.
    Native,
    /// Convert every item to one explicit output profile.
    Normalised,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnIncompatible {
    /// Reject save/start on an unsupported boundary. The only mode for now.
    #[default]
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionClass {
    NativeContinuous,
    NativeDiscontinuity,
    NormaliseRequired,
    Unsupported,
}

/// Per-entry readiness in the planned playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetState {
    /// Manifest present and `probe_state == Ready`.
    Ready,
    /// File matched no supported kind, or the probe errored.
    Unsupported,
    /// No manifest at all — file missing from the library, or not yet probed.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPlan {
    pub index: usize,
    pub asset_state: AssetState,
    /// The detected product label when known (e.g. "MP4/MOV"), for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub from_index: usize,
    pub to_index: usize,
    pub classification: TransitionClass,
    /// Machine-readable reasons the boundary is not `native_continuous`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// Actions the transition engine must take on a native crossing (PMT
    /// bump, discontinuity indicator, decoder reinit).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_actions: Vec<String>,
    /// Operator-facing remediation for an `unsupported` / `normalise_required`
    /// boundary. Empty for clean native crossings.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remediation: String,
}

/// A coarse resource plan. Phase 2 fills the shape; Phase 6 (normalised)
/// populates the encode/session detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResourcePlan {
    /// True when the playlist needs any decode/encode (normalised mode or a
    /// normalise_required boundary). Native remux needs none.
    pub requires_transcode: bool,
}

/// The full planner result returned to the manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPlaylist {
    /// True when every entry is `Ready` and no boundary is `Unsupported`
    /// (and, under native policy, none is `NormaliseRequired`).
    pub valid: bool,
    pub entries: Vec<EntryPlan>,
    pub boundaries: Vec<Boundary>,
    pub resource_plan: ResourcePlan,
}

/// Plan a playlist from the per-entry manifests. `manifests[i]` is `None`
/// when entry `i` has no manifest (missing / unprobed file). Boundaries are
/// emitted for each adjacent pair `i → i+1`, plus the wrap `last → 0` when
/// `loop_playback` and there are ≥ 2 entries.
pub fn plan_playlist(
    manifests: &[Option<AssetManifest>],
    loop_playback: bool,
    policy: &PlaybackPolicy,
) -> PlannedPlaylist {
    let entries: Vec<EntryPlan> = manifests
        .iter()
        .enumerate()
        .map(|(index, m)| {
            let (asset_state, product_kind) = match m {
                None => (AssetState::Missing, None),
                Some(m) if m.probe_state == ProbeState::Ready => {
                    (AssetState::Ready, m.product_kind.clone())
                }
                Some(_) => (AssetState::Unsupported, None),
            };
            EntryPlan { index, asset_state, product_kind }
        })
        .collect();

    let mut boundaries = Vec::new();
    let n = manifests.len();
    if n >= 2 {
        for i in 0..(n - 1) {
            boundaries.push(classify_boundary(i, i + 1, &manifests[i], &manifests[i + 1], policy));
        }
        if loop_playback {
            boundaries.push(classify_boundary(
                n - 1,
                0,
                &manifests[n - 1],
                &manifests[0],
                policy,
            ));
        }
    }

    let all_ready = entries.iter().all(|e| e.asset_state == AssetState::Ready);
    let any_bad_boundary = boundaries.iter().any(|b| {
        b.classification == TransitionClass::Unsupported
            || b.classification == TransitionClass::NormaliseRequired
    });
    let requires_transcode = policy.mode == PolicyMode::Normalised
        || boundaries
            .iter()
            .any(|b| b.classification == TransitionClass::NormaliseRequired);

    PlannedPlaylist {
        valid: all_ready && !any_bad_boundary,
        entries,
        boundaries,
        resource_plan: ResourcePlan { requires_transcode },
    }
}

/// Classify one boundary from `a` → `b`.
fn classify_boundary(
    from_index: usize,
    to_index: usize,
    a: &Option<AssetManifest>,
    b: &Option<AssetManifest>,
    policy: &PlaybackPolicy,
) -> Boundary {
    // An unusable endpoint makes the whole boundary unsupported — you cannot
    // splice to or from a file you cannot play.
    let (a, b) = match (a.as_ref(), b.as_ref()) {
        (Some(a), Some(b))
            if a.probe_state == ProbeState::Ready && b.probe_state == ProbeState::Ready =>
        {
            (a, b)
        }
        _ => {
            return Boundary {
                from_index,
                to_index,
                classification: TransitionClass::Unsupported,
                reasons: vec!["endpoint_not_playable".to_string()],
                native_actions: Vec::new(),
                remediation:
                    "One or both assets at this boundary are missing or unsupported. Replace them \
                     with a supported MP4/MOV, TS File, or Still Image."
                        .to_string(),
            };
        }
    };

    // Gather the differences that matter for a splice.
    let diff = compare(a, b);

    // Under normalised policy, any difference at all is normalise_required —
    // every item is forced to the output profile, so differing inputs are
    // expected and handled by the transcode pipeline (not an error).
    if policy.mode == PolicyMode::Normalised {
        return Boundary {
            from_index,
            to_index,
            classification: TransitionClass::NormaliseRequired,
            reasons: if diff.reasons.is_empty() {
                vec!["normalised_policy".to_string()]
            } else {
                diff.reasons
            },
            native_actions: Vec::new(),
            remediation: String::new(),
        };
    }

    // Native policy.
    if diff.reasons.is_empty() {
        // Identical elementary contract — the cleanest crossing.
        return Boundary {
            from_index,
            to_index,
            classification: TransitionClass::NativeContinuous,
            reasons: Vec::new(),
            native_actions: Vec::new(),
            remediation: String::new(),
        };
    }

    // A remuxable change: signal it. This is the common broadcast case —
    // different resolution / frame rate / SPS / audio layout across a
    // playlist. The transition engine handles it with a discontinuity, a PMT
    // bump when the layout changed, and a decoder reinit; it always starts
    // the new source at a random-access point.
    let mut native_actions = vec!["discontinuity".to_string(), "decoder_reinit".to_string()];
    if diff.layout_changed {
        native_actions.insert(0, "pmt_version_bump".to_string());
    }
    Boundary {
        from_index,
        to_index,
        classification: TransitionClass::NativeDiscontinuity,
        reasons: diff.reasons,
        native_actions,
        remediation: String::new(),
    }
}

/// The material differences between two ready manifests, from a splice's
/// point of view.
struct Diff {
    /// Human/machine reasons the two are not identical.
    reasons: Vec<String>,
    /// Whether the PID/stream layout changed (video or audio added/removed) —
    /// the trigger for a PMT version bump.
    layout_changed: bool,
}

fn first_stream<'a>(m: &'a AssetManifest, t: ManifestMediaType) -> Option<&'a crate::media::manifest::ManifestStream> {
    m.streams.iter().find(|s| s.media_type == t)
}

fn compare(a: &AssetManifest, b: &AssetManifest) -> Diff {
    let mut reasons = Vec::new();
    let mut layout_changed = false;

    if a.detected_kind != b.detected_kind {
        reasons.push("source_kind_changed".to_string());
    }

    let av = first_stream(a, ManifestMediaType::Video);
    let bv = first_stream(b, ManifestMediaType::Video);
    match (av, bv) {
        (Some(_), None) => {
            reasons.push("video_removed".to_string());
            layout_changed = true;
        }
        (None, Some(_)) => {
            reasons.push("video_added".to_string());
            layout_changed = true;
        }
        (Some(x), Some(y)) => {
            if x.codec != y.codec {
                reasons.push("video_codec_changed".to_string());
            }
            if x.width != y.width || x.height != y.height {
                reasons.push("video_resolution_changed".to_string());
            }
            // Rational frame-rate comparison — different cadence is a real
            // difference the planner surfaces, but it is NOT unsupported:
            // sample timestamps express a 24↔25↔29.97↔30↔50↔59.94 change and
            // the native path signals the boundary (plan §7.2).
            if !frame_rate_equal(x.frame_rate_num, x.frame_rate_den, y.frame_rate_num, y.frame_rate_den) {
                reasons.push("video_frame_rate_changed".to_string());
            }
            // Codec-config change (SPS/PPS) forces a decoder reinit even at
            // the same resolution.
            if x.extradata_fingerprint.is_some()
                && y.extradata_fingerprint.is_some()
                && x.extradata_fingerprint != y.extradata_fingerprint
            {
                reasons.push("video_codec_config_changed".to_string());
            }
            if x.profile != y.profile && x.profile.is_some() && y.profile.is_some() {
                reasons.push("video_profile_changed".to_string());
            }
        }
        (None, None) => {}
    }

    let aa = first_stream(a, ManifestMediaType::Audio);
    let ba = first_stream(b, ManifestMediaType::Audio);
    match (aa, ba) {
        (Some(_), None) => {
            reasons.push("audio_removed".to_string());
            layout_changed = true;
        }
        (None, Some(_)) => {
            reasons.push("audio_added".to_string());
            layout_changed = true;
        }
        (Some(x), Some(y)) => {
            if x.codec != y.codec {
                reasons.push("audio_codec_changed".to_string());
            }
            if x.sample_rate != y.sample_rate && x.sample_rate.is_some() && y.sample_rate.is_some() {
                reasons.push("audio_sample_rate_changed".to_string());
            }
            if x.channels != y.channels && x.channels.is_some() && y.channels.is_some() {
                reasons.push("audio_channels_changed".to_string());
            }
        }
        (None, None) => {}
    }

    Diff { reasons, layout_changed }
}

/// Compare two rational frame rates. Missing rates (either side `None`) are
/// treated as "not known to differ" — we don't invent a cadence change from
/// absent data. Uses cross-multiplication to avoid float error.
fn frame_rate_equal(
    an: Option<u32>,
    ad: Option<u32>,
    bn: Option<u32>,
    bd: Option<u32>,
) -> bool {
    match (an, ad, bn, bd) {
        (Some(an), Some(ad), Some(bn), Some(bd)) if ad != 0 && bd != 0 => {
            (an as u64) * (bd as u64) == (bn as u64) * (ad as u64)
        }
        // If either side lacks a rate, don't claim a difference.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::manifest::{
        Compatibility, ContainerInfo, DetectedKind, FileIdentity, ManifestStream,
    };

    fn ident() -> FileIdentity {
        FileIdentity { size_bytes: 1, modified_unix_ms: 1, content_fingerprint: None }
    }

    fn video(codec: &str, w: u16, h: u16, fr: (u32, u32), extradata: &str) -> ManifestStream {
        ManifestStream {
            index: 0,
            media_type: ManifestMediaType::Video,
            codec: Some(codec.to_string()),
            profile: Some("high".to_string()),
            width: Some(w),
            height: Some(h),
            frame_rate_num: Some(fr.0),
            frame_rate_den: Some(fr.1),
            sample_rate: None,
            channels: None,
            language: None,
            extradata_fingerprint: Some(extradata.to_string()),
        }
    }

    fn audio(codec: &str, sr: u32, ch: u16) -> ManifestStream {
        ManifestStream {
            index: 1,
            media_type: ManifestMediaType::Audio,
            codec: Some(codec.to_string()),
            profile: None,
            width: None,
            height: None,
            frame_rate_num: None,
            frame_rate_den: None,
            sample_rate: Some(sr),
            channels: Some(ch),
            language: None,
            extradata_fingerprint: None,
        }
    }

    fn manifest(kind: DetectedKind, streams: Vec<ManifestStream>) -> AssetManifest {
        AssetManifest {
            schema_version: 1,
            name: "x".to_string(),
            file_identity: ident(),
            probe_state: ProbeState::Ready,
            detected_kind: Some(kind),
            product_kind: Some(kind.product_label().to_string()),
            error_code: None,
            container: Some(ContainerInfo {
                format_name: "x".to_string(),
                duration_90khz: None,
                bitrate_bps: None,
                fragmented: false,
                ts_packet_bytes: None,
            }),
            streams,
            compatibility: Compatibility::default(),
        }
    }

    fn unsupported() -> AssetManifest {
        let mut m = manifest(DetectedKind::Mp4, vec![]);
        m.probe_state = ProbeState::Unsupported;
        m.detected_kind = None;
        m
    }

    fn native() -> PlaybackPolicy {
        PlaybackPolicy::default()
    }

    #[test]
    fn identical_assets_are_native_continuous() {
        let a = manifest(
            DetectedKind::Mp4,
            vec![video("h264", 1920, 1080, (30000, 1001), "aaa"), audio("aac_lc", 48000, 2)],
        );
        let plan = plan_playlist(&[Some(a.clone()), Some(a)], false, &native());
        assert!(plan.valid);
        assert_eq!(plan.boundaries.len(), 1);
        assert_eq!(plan.boundaries[0].classification, TransitionClass::NativeContinuous);
        assert!(!plan.resource_plan.requires_transcode);
    }

    #[test]
    fn frame_rate_change_is_native_discontinuity_not_unsupported() {
        let a = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (24, 1), "aaa")]);
        let b = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (30000, 1001), "aaa")]);
        let plan = plan_playlist(&[Some(a), Some(b)], false, &native());
        let bnd = &plan.boundaries[0];
        assert_eq!(bnd.classification, TransitionClass::NativeDiscontinuity);
        assert!(bnd.reasons.contains(&"video_frame_rate_changed".to_string()));
        assert!(bnd.native_actions.contains(&"discontinuity".to_string()));
        // Frame-rate-only change doesn't alter PID layout → no PMT bump.
        assert!(!bnd.native_actions.contains(&"pmt_version_bump".to_string()));
        // A signalled native boundary is still a valid playlist.
        assert!(plan.valid);
    }

    #[test]
    fn same_fps_expressed_two_ways_is_continuous() {
        // 30000/1001 vs 60000/2002 are the same rate.
        let a = manifest(DetectedKind::Mp4, vec![video("h264", 1280, 720, (30000, 1001), "aaa")]);
        let b = manifest(DetectedKind::Mp4, vec![video("h264", 1280, 720, (60000, 2002), "aaa")]);
        let plan = plan_playlist(&[Some(a), Some(b)], false, &native());
        assert_eq!(plan.boundaries[0].classification, TransitionClass::NativeContinuous);
    }

    #[test]
    fn removing_audio_bumps_pmt_and_lists_reason() {
        let a = manifest(
            DetectedKind::Mp4,
            vec![video("h264", 1920, 1080, (25, 1), "aaa"), audio("aac_lc", 48000, 2)],
        );
        let b = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (25, 1), "aaa")]);
        let plan = plan_playlist(&[Some(a), Some(b)], false, &native());
        let bnd = &plan.boundaries[0];
        assert_eq!(bnd.classification, TransitionClass::NativeDiscontinuity);
        assert!(bnd.reasons.contains(&"audio_removed".to_string()));
        assert!(bnd.native_actions.contains(&"pmt_version_bump".to_string()));
    }

    #[test]
    fn codec_config_change_forces_reinit_at_same_resolution() {
        let a = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (25, 1), "aaa")]);
        let b = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (25, 1), "bbb")]);
        let plan = plan_playlist(&[Some(a), Some(b)], false, &native());
        let bnd = &plan.boundaries[0];
        assert_eq!(bnd.classification, TransitionClass::NativeDiscontinuity);
        assert!(bnd.reasons.contains(&"video_codec_config_changed".to_string()));
        assert!(bnd.native_actions.contains(&"decoder_reinit".to_string()));
    }

    #[test]
    fn unsupported_endpoint_makes_boundary_unsupported_and_playlist_invalid() {
        let a = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (25, 1), "aaa")]);
        let plan = plan_playlist(&[Some(a), Some(unsupported())], false, &native());
        assert_eq!(plan.boundaries[0].classification, TransitionClass::Unsupported);
        assert!(!plan.valid);
        assert_eq!(plan.entries[1].asset_state, AssetState::Unsupported);
        assert!(!plan.boundaries[0].remediation.is_empty());
    }

    #[test]
    fn missing_manifest_is_missing_entry_and_unsupported_boundary() {
        let a = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (25, 1), "aaa")]);
        let plan = plan_playlist(&[Some(a), None], false, &native());
        assert_eq!(plan.entries[1].asset_state, AssetState::Missing);
        assert_eq!(plan.boundaries[0].classification, TransitionClass::Unsupported);
        assert!(!plan.valid);
    }

    #[test]
    fn loop_adds_the_wrap_boundary() {
        let a = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (25, 1), "aaa")]);
        let b = manifest(DetectedKind::Ts, vec![video("h264", 1280, 720, (25, 1), "bbb")]);
        // No loop: one boundary (a→b). Loop: two (a→b, b→a).
        let no_loop = plan_playlist(&[Some(a.clone()), Some(b.clone())], false, &native());
        assert_eq!(no_loop.boundaries.len(), 1);
        let looped = plan_playlist(&[Some(a), Some(b)], true, &native());
        assert_eq!(looped.boundaries.len(), 2);
        assert_eq!(looped.boundaries[1].from_index, 1);
        assert_eq!(looped.boundaries[1].to_index, 0);
    }

    #[test]
    fn single_entry_has_no_boundaries() {
        let a = manifest(DetectedKind::Image, vec![video("raster", 1920, 1080, (0, 0), "")]);
        let plan = plan_playlist(&[Some(a)], true, &native());
        assert!(plan.boundaries.is_empty());
        assert!(plan.valid);
    }

    #[test]
    fn normalised_policy_makes_every_change_normalise_required() {
        let a = manifest(DetectedKind::Mp4, vec![video("h264", 1920, 1080, (24, 1), "aaa")]);
        let b = manifest(DetectedKind::Mp4, vec![video("h264", 1280, 720, (30, 1), "bbb")]);
        let policy = PlaybackPolicy { mode: PolicyMode::Normalised, on_incompatible: OnIncompatible::Reject };
        let plan = plan_playlist(&[Some(a), Some(b)], false, &policy);
        assert_eq!(plan.boundaries[0].classification, TransitionClass::NormaliseRequired);
        assert!(plan.resource_plan.requires_transcode);
        // Under native policy the plan is "valid" (signalled); under
        // normalised it is flagged as needing the transcode path, which Phase
        // 2 does not implement yet → not valid for start.
        assert!(!plan.valid);
    }

    #[test]
    fn frame_rate_equal_handles_missing_and_equivalent() {
        assert!(frame_rate_equal(Some(30000), Some(1001), Some(30000), Some(1001)));
        assert!(frame_rate_equal(Some(30000), Some(1001), Some(60000), Some(2002)));
        assert!(!frame_rate_equal(Some(24), Some(1), Some(25), Some(1)));
        // Missing either side → not a claimed difference.
        assert!(frame_rate_equal(None, None, Some(25), Some(1)));
        assert!(frame_rate_equal(Some(25), Some(1), None, None));
    }
}
