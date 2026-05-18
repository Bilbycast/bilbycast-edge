// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-flow bandwidth-profile resolution.
//!
//! [`crate::config::models::BandwidthProfile`] sets the slot count of
//! the flow's broadcast-class channels (main fan-out, per-input
//! pre-broadcast, fixer command channel). The profile can be set
//! explicitly on [`crate::config::models::FlowConfig::bandwidth_profile`];
//! when unset, the runtime auto-derives it from the flow's inputs.
//!
//! Auto-derivation rule:
//!
//! - Any input of type [`InputConfig::St2110_20`], [`InputConfig::St2110_23`],
//!   or [`InputConfig::MxlVideo`] → [`BandwidthProfile::Uncompressed`].
//!   These are 1–12 Gbps uncompressed video essences; the Standard
//!   tier's 56 ms jitter budget at 3 Gbps is too tight when the
//!   tokio runtime stalls for ~30 ms under load.
//! - Everything else → [`BandwidthProfile::Standard`].
//!   ST 2110-30/-31 audio, ST 2110-40 ANC, MXL audio/anc, all TS-
//!   carrying inputs (SRT / RTP / UDP / RIST / RTMP / RTSP /
//!   MediaPlayer / Replay / Bonded), WebRTC. All fit comfortably in
//!   16 384 slots (3.4 s headroom at 50 Mbps, 344 ms at 500 Mbps).
//!
//! A flow can have multiple inputs (Hitless redundancy, PID-bus
//! assembly). The resolved profile is the widest class across all
//! members — one ST 2110-20 leg in a flow promotes the whole flow's
//! channels to Uncompressed.
//!
//! Operator overrides win unconditionally: setting
//! `bandwidth_profile: high_bitrate` on a compressed-HEVC contribution
//! flow lets the operator buy extra headroom without the auto-policy
//! knowing about the source's particular bitrate.

use crate::config::models::{BandwidthProfile, FlowConfig, InputConfig, InputDefinition};

/// Bandwidth class implied by a single input's type.
///
/// Uncompressed essence (ST 2110-20/-23 video, MXL video) pulls the
/// flow's profile up to Uncompressed; everything else is Standard.
pub fn class_for_input(input: &InputConfig) -> BandwidthProfile {
    match input {
        InputConfig::St2110_20(_)
        | InputConfig::St2110_23(_)
        | InputConfig::MxlVideo(_) => BandwidthProfile::Uncompressed,
        _ => BandwidthProfile::Standard,
    }
}

/// Resolve the effective bandwidth profile for a flow.
///
/// Returns the operator override on [`FlowConfig::bandwidth_profile`]
/// when set, otherwise the widest class across `inputs`. Empty input
/// lists collapse to [`BandwidthProfile::Standard`] (output-only / test
/// flow — no essence to size for).
pub fn resolve_for_flow(
    flow: &FlowConfig,
    inputs: &[&InputDefinition],
) -> BandwidthProfile {
    if let Some(explicit) = flow.bandwidth_profile {
        return explicit;
    }
    inputs
        .iter()
        .map(|i| class_for_input(&i.config))
        .max_by_key(|p| match p {
            BandwidthProfile::Standard => 0u8,
            BandwidthProfile::HighBitrate => 1,
            BandwidthProfile::Uncompressed => 2,
        })
        .unwrap_or(BandwidthProfile::Standard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::InputDefinition;

    /// Build a minimal FlowConfig with optional `bandwidth_profile`.
    /// Constructed via JSON to avoid coupling to non-public Default impls
    /// across the dozens of config sub-types — only the field under test
    /// matters here.
    fn flow(profile: Option<&str>) -> FlowConfig {
        let json = match profile {
            None => r#"{"id":"f","name":"f","input_ids":[],"output_ids":[]}"#.to_string(),
            Some(p) => format!(
                r#"{{"id":"f","name":"f","input_ids":[],"output_ids":[],"bandwidth_profile":"{}"}}"#,
                p
            ),
        };
        serde_json::from_str(&json).expect("flow JSON")
    }

    fn input_json(id: &str, type_name: &str, extra: &str) -> InputDefinition {
        let json = if extra.is_empty() {
            format!(
                r#"{{"id":"{id}","name":"{id}","active":true,"type":"{type_name}"}}"#
            )
        } else {
            format!(
                r#"{{"id":"{id}","name":"{id}","active":true,"type":"{type_name}",{extra}}}"#
            )
        };
        serde_json::from_str(&json).expect("input JSON")
    }

    fn srt_input(id: &str) -> InputDefinition {
        input_json(
            id,
            "srt",
            r#""mode":"listener","local_addr":"0.0.0.0:9000""#,
        )
    }

    fn st2110_20_input(id: &str) -> InputDefinition {
        input_json(
            id,
            "st2110_20",
            r#""bind_addr":"232.0.0.1:50020","width":1920,"height":1080,"frame_rate_num":25,"frame_rate_den":1,"pixel_format":"yuv422_8bit","video_encode":{"codec":"x264","bitrate_kbps":20000}"#,
        )
    }

    fn st2110_30_input(id: &str) -> InputDefinition {
        input_json(
            id,
            "st2110_30",
            r#""bind_addr":"232.0.0.2:50030""#,
        )
    }

    fn mxl_video_input(id: &str) -> InputDefinition {
        input_json(
            id,
            "mxl_video",
            r#""domain_path":"/dev/shm/test","flow_name":"v","width":1920,"height":1080,"frame_rate_num":25,"frame_rate_den":1,"video_encode":{"codec":"x264","bitrate_kbps":20000}"#,
        )
    }

    #[test]
    fn empty_input_list_collapses_to_standard() {
        let f = flow(None);
        assert_eq!(resolve_for_flow(&f, &[]), BandwidthProfile::Standard);
    }

    #[test]
    fn ts_inputs_default_to_standard() {
        let f = flow(None);
        let i1 = srt_input("a");
        let i2 = srt_input("b");
        assert_eq!(
            resolve_for_flow(&f, &[&i1, &i2]),
            BandwidthProfile::Standard,
        );
    }

    #[test]
    fn st2110_30_audio_stays_standard() {
        let f = flow(None);
        let i = st2110_30_input("audio-only");
        assert_eq!(resolve_for_flow(&f, &[&i]), BandwidthProfile::Standard);
    }

    #[test]
    fn st2110_20_video_promotes_to_uncompressed() {
        let f = flow(None);
        let i = st2110_20_input("v");
        assert_eq!(resolve_for_flow(&f, &[&i]), BandwidthProfile::Uncompressed);
    }

    #[test]
    fn mxl_video_promotes_to_uncompressed() {
        let f = flow(None);
        let i = mxl_video_input("mxl-v");
        assert_eq!(resolve_for_flow(&f, &[&i]), BandwidthProfile::Uncompressed);
    }

    #[test]
    fn mixed_inputs_pick_widest_class() {
        // Hitless dual-leg: ST 2110-20 video + ST 2110-30 audio sidecar
        // wraps the flow's broadcast in Uncompressed even though most
        // of the legs are audio.
        let f = flow(None);
        let v = st2110_20_input("v");
        let a = st2110_30_input("a");
        let s = srt_input("backup-ts");
        assert_eq!(
            resolve_for_flow(&f, &[&a, &s, &v]),
            BandwidthProfile::Uncompressed,
        );
    }

    #[test]
    fn operator_override_wins() {
        // ST 2110-20 input but operator forced Standard — odd choice
        // but should be honoured (operator might be tight on memory and
        // running 1080p25 where 56 ms headroom is acceptable).
        let f = flow(Some("standard"));
        let v = st2110_20_input("v");
        assert_eq!(resolve_for_flow(&f, &[&v]), BandwidthProfile::Standard);

        // Conversely, TS-only flow but operator forced Uncompressed —
        // burns memory but is legal.
        let f = flow(Some("uncompressed"));
        let s = srt_input("ts");
        assert_eq!(resolve_for_flow(&f, &[&s]), BandwidthProfile::Uncompressed);
    }

    #[test]
    fn high_bitrate_for_operator_override_only() {
        // Auto never picks HighBitrate (only the two boundary classes).
        // It's an operator-only opt-in for 500 Mbps – 3 Gbps compressed.
        let f = flow(Some("high_bitrate"));
        let s = srt_input("ts");
        assert_eq!(resolve_for_flow(&f, &[&s]), BandwidthProfile::HighBitrate);
    }

    #[test]
    fn broadcast_capacity_values_match_doc_table() {
        assert_eq!(BandwidthProfile::Standard.broadcast_capacity(), 16_384);
        assert_eq!(BandwidthProfile::HighBitrate.broadcast_capacity(), 32_768);
        assert_eq!(BandwidthProfile::Uncompressed.broadcast_capacity(), 65_536);
    }
}
