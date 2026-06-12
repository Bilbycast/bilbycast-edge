// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cross-essence media-timeline anchor for ST 2110 receive flows.
//!
//! A 2110 flow carries its essences on separate RTP sessions whose
//! timestamps tick at different clock rates (video 90 kHz, audio at the
//! sample rate). The synthesised TS carriers each essence input produces
//! must put their PES timelines on a **common** origin or every
//! downstream consumer inherits a frozen A/V offset: the legacy
//! behaviour anchored each input at PTS 0 on its first received packet,
//! which bakes the (audio path latency − video path latency) of the
//! sender into the PES labels permanently — the 2026-06-12 "+160 ms
//! audio late, zero drift" defect.
//!
//! [`SharedMediaTimeline`] is one per flow, shared by every 2110 input.
//! The first essence to receive a packet defines the timeline origin;
//! every other essence resolves its own first wire timestamp onto that
//! timeline by picking the wrap-candidate nearest a monotonic-clock
//! projection. Because both bilbycast senders (which stamp both essences
//! from the source PES timeline) and conformant ST 2110 senders (which
//! stamp both from PTP) put their essences on one shared timeline, the
//! nearest candidate lands within transport-latency distance of the
//! projection and the cross-essence offsets in the resolved PES labels
//! equal the true media offsets. Senders with unrelated per-essence RTP
//! epochs (plain RFC 3550 random offsets) resolve implausibly far away
//! and fall back to the projection itself — which reproduces the legacy
//! arrival-anchored behaviour.

use std::sync::Mutex;

/// A resolved candidate further than this from the projected timeline
/// (10 s in 90 kHz ticks) means the sender's essences do not share an
/// RTP epoch — fall back to arrival anchoring.
const TIMELINE_TRUST_WINDOW_90K: i64 = 10 * 90_000;

/// The 33-bit PES PTS wrap in 90 kHz ticks (2³³). bilbycast senders
/// stamp each essence's RTP timestamp from the source PES timeline,
/// which itself wraps at 2³³ — so two essences' raw values can differ
/// by combinations of *both* their RTP moduli and this source wrap.
/// The candidate search spans the lattice of the two (e.g. for 48 kHz
/// audio joining a video anchor the lattice spacing is gcd-driven:
/// 2²⁹ ticks ≈ 99 min — still vastly wider than the trust window, so
/// the in-window candidate is unique). PTP-epoch senders sit at the
/// m = 0 lattice points and are unaffected.
const SOURCE_WRAP_90K: f64 = 8_589_934_592.0;
/// Source-wrap multiples searched. ±16 covers every reachable lattice
/// residue for the supported audio rates (48 / 96 / 44.1 kHz).
const SOURCE_WRAP_SEARCH: i64 = 16;

/// Origin offset returned for the first essence. Keeps every resolved
/// timeline value comfortably positive even when a later essence's
/// media time precedes the anchor by up to the trust window.
const TIMELINE_BASE_90K: i64 = 10 * 90_000;

struct Anchor {
    /// Raw 90 kHz value of the anchoring essence's first timestamp.
    raw_90k: i64,
    /// The anchoring essence's RTP wrap modulus in 90 kHz ticks — the
    /// anchor's raw value is itself truncated by an unknown multiple of
    /// this, so the joining essence's candidate search must span the
    /// JOINT lattice of both moduli (plus the source wrap). Without it,
    /// an audio-first anchor (the common RX ordering — the 1 ms-ptime
    /// -30 packet beats the first reassembled video frame) is
    /// unsolvable for most timeline origins.
    modulus_90k: f64,
    /// `util::time::now_us()` when the anchor was taken — the common
    /// process-monotonic clock all inputs share.
    mono_us: u64,
}

/// One per flow. See module docs.
pub struct SharedMediaTimeline {
    anchor: Mutex<Option<Anchor>>,
}

impl SharedMediaTimeline {
    pub fn new() -> Self {
        Self { anchor: Mutex::new(None) }
    }

    /// Resolve an essence's first wire timestamp onto the flow-shared
    /// media timeline, returning the 90 kHz PES anchor the essence
    /// should start its synthesised timeline at.
    ///
    /// `raw_90k` is the essence's first RTP timestamp mapped to 90 kHz
    /// ticks (video: the timestamp itself; audio: `ts × 90000 / rate`).
    /// `modulus_90k` is the essence's RTP wrap modulus in the same
    /// ticks (video: 2³²; audio: 2³² × 90000 / rate).
    pub fn resolve(&self, raw_90k: i64, modulus_90k: f64, label: &str) -> i64 {
        let now_us = crate::util::time::now_us();
        let mut guard = self.anchor.lock().unwrap();
        match guard.as_ref() {
            None => {
                *guard = Some(Anchor { raw_90k, modulus_90k, mono_us: now_us });
                tracing::info!(
                    raw_90k,
                    "{label}: anchoring the flow media timeline"
                );
                TIMELINE_BASE_90K
            }
            Some(a) => {
                // The joining essence's true timeline offset from the
                // anchor is `d = (raw + k·M_join + m·W) − (raw_anchor +
                // j·M_anchor)` for unknown wrap counts (k, m, j) — both
                // essences' raw stamps are truncated by their own RTP
                // moduli, and the source PES timeline itself wraps at
                // W = 2³³. Search the joint lattice for the candidate
                // nearest the monotonic-clock projection of elapsed
                // time (µs → 90 kHz is ×9/100 exact). The lattice
                // spacing for the supported rate pairs is ≥ 2²⁹ ticks
                // (≈ 99 min) — vastly wider than the trust window, so
                // the in-window candidate is unique.
                let elapsed_90k = (now_us.saturating_sub(a.mono_us) as i64) * 9 / 100;
                let mut best_d: i64 = elapsed_90k;
                let mut best_distance = i64::MAX;
                for j in -SOURCE_WRAP_SEARCH..=SOURCE_WRAP_SEARCH {
                    for m in -SOURCE_WRAP_SEARCH..=SOURCE_WRAP_SEARCH {
                        let base = raw_90k as f64 + m as f64 * SOURCE_WRAP_90K
                            - (a.raw_90k as f64 + j as f64 * a.modulus_90k);
                        let k = ((elapsed_90k as f64 - base) / modulus_90k).round();
                        let d = (base + k * modulus_90k).round() as i64;
                        let distance = (d - elapsed_90k).abs();
                        if distance < best_distance {
                            best_distance = distance;
                            best_d = d;
                        }
                    }
                }
                if best_distance <= TIMELINE_TRUST_WINDOW_90K {
                    tracing::info!(
                        offset_ms = best_d / 90,
                        "{label}: joined the flow media timeline"
                    );
                    TIMELINE_BASE_90K + best_d
                } else {
                    tracing::warn!(
                        distance_ms = best_distance / 90,
                        "{label}: wire timestamps do not share the flow's RTP epoch — \
                         falling back to arrival anchoring (cross-essence A/V offset \
                         will reflect path-latency differences)"
                    );
                    TIMELINE_BASE_90K + elapsed_90k
                }
            }
        }
    }
}

impl Default for SharedMediaTimeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO_MODULUS_90K: f64 = 4_294_967_296.0;
    /// 48 kHz audio: 2^32 RTP ticks × 90000/48000.
    const AUDIO48_MODULUS_90K: f64 = 4_294_967_296.0 * 90_000.0 / 48_000.0;

    /// Two essences stamped from one source timeline resolve to PES
    /// anchors whose difference equals the true media offset, even
    /// when the audio side's scaled value wrapped its modulus.
    #[test]
    fn shared_epoch_essences_align() {
        let tl = SharedMediaTimeline::new();
        // Source timeline value at the video anchor instant.
        let media_90k: i64 = 123_456_789;
        let video_raw = media_90k % (VIDEO_MODULUS_90K as i64);
        let v = tl.resolve(video_raw, VIDEO_MODULUS_90K, "video");
        // Audio essence: same timeline, 160 ms staler content, RTP at
        // 48 kHz (mod 2^32) mapped back to 90 kHz by the caller.
        let audio_media_90k = media_90k - 160 * 90;
        let audio_ts_48k = (audio_media_90k as i128 * 48_000 / 90_000) % (1i128 << 32);
        let audio_raw_90k = (audio_ts_48k * 90_000 / 48_000) as i64;
        let a = tl.resolve(audio_raw_90k, AUDIO48_MODULUS_90K, "audio");
        // Sub-tick rounding from the 48 kHz mapping is acceptable.
        assert!(
            ((a - v) - (-160 * 90)).abs() <= 2,
            "audio-video anchor delta {} != -14400",
            a - v
        );
    }

    /// The cross-modulus case: the shared PES timeline value at anchor
    /// time exceeds 2³², so the video raw stamp is truncated mod 2³²
    /// while the 48 kHz audio stamp wrapped differently — alignment
    /// needs combined source-wrap + RTP-modulus lattice candidates
    /// (m = −8, k = 8 for this configuration).
    #[test]
    fn cross_modulus_lattice_aligns_large_timeline_values() {
        let tl = SharedMediaTimeline::new();
        let media_90k: i64 = 5_000_000_000; // > 2^32, < 2^33
        let video_raw = media_90k % (VIDEO_MODULUS_90K as i64);
        let v = tl.resolve(video_raw, VIDEO_MODULUS_90K, "video");
        let audio_media_90k = media_90k - 160 * 90;
        let audio_ts_48k = (audio_media_90k as i128 * 48_000 / 90_000) % (1i128 << 32);
        let audio_raw_90k = (audio_ts_48k * 90_000 / 48_000) as i64;
        let a = tl.resolve(audio_raw_90k, AUDIO48_MODULUS_90K, "audio");
        assert!(
            ((a - v) - (-160 * 90)).abs() <= 2,
            "audio-video anchor delta {} != -14400",
            a - v
        );
    }

    /// Audio anchors FIRST — the common RX ordering (1 ms-ptime -30
    /// packets beat the first reassembled video frame). The anchor's
    /// own modulus truncation must be part of the lattice solve or
    /// most timeline origins are unsolvable for the joining video.
    /// Sweep origins across the full 33-bit range, including values
    /// where the audio stamp wrapped its modulus (the case the
    /// single-sided search provably failed ~7/8 of the time).
    #[test]
    fn audio_anchors_first_video_joins() {
        for origin in [
            123_456_789i64,
            4_400_000_000,  // > 2^32
            5_000_000_000,
            8_300_000_000,  // near 2^33
            8_053_063_680 + 90_000, // just past the audio modulus
        ] {
            let tl = SharedMediaTimeline::new();
            let audio_ts_48k = (origin as i128 * 48_000 / 90_000) % (1i128 << 32);
            let audio_raw_90k = (audio_ts_48k * 90_000 / 48_000) as i64;
            let a = tl.resolve(audio_raw_90k, AUDIO48_MODULUS_90K, "audio");
            // Video frame 160 ms NEWER than the first audio packet.
            let video_media = (origin + 160 * 90) % (SOURCE_WRAP_90K as i64);
            let video_raw = video_media % (VIDEO_MODULUS_90K as i64);
            let v = tl.resolve(video_raw, VIDEO_MODULUS_90K, "video");
            assert!(
                ((v - a) - 160 * 90).abs() <= 2,
                "origin {origin}: video-audio anchor delta {} != +14400",
                v - a
            );
        }
    }

    /// Wrap disambiguation: a video value that wrapped 2^32 while the
    /// projection sits just past the boundary still lands next to it.
    #[test]
    fn wrap_candidates_resolve_to_nearest() {
        let tl = SharedMediaTimeline::new();
        let anchor = (VIDEO_MODULUS_90K as i64) - 90_000; // 1 s before video wrap
        let v = tl.resolve(anchor, VIDEO_MODULUS_90K, "video");
        assert_eq!(v, TIMELINE_BASE_90K);
        // Second essence's media time is 2 s later — its raw value
        // wrapped to a small number.
        let raw = anchor + 2 * 90_000 - (VIDEO_MODULUS_90K as i64);
        let r = tl.resolve(raw, VIDEO_MODULUS_90K, "video2");
        assert_eq!(r - v, 2 * 90_000);
    }

    /// Unrelated epochs (random RFC 3550 offsets) fall back to the
    /// projection — anchor distance reflects arrival time, not the
    /// bogus timestamp.
    #[test]
    fn unrelated_epoch_falls_back_to_projection() {
        let tl = SharedMediaTimeline::new();
        let v = tl.resolve(500_000_000, VIDEO_MODULUS_90K, "video");
        // A value half a modulus away can't be a shared-timeline peer.
        let r = tl.resolve(
            500_000_000 + (VIDEO_MODULUS_90K as i64) / 2,
            VIDEO_MODULUS_90K,
            "audio",
        );
        // Projection elapsed is ~0 in-test, so the fallback lands at
        // the anchor (within scheduling slop).
        assert!((r - v).abs() < 90_000, "fallback should land near the anchor: {}", r - v);
    }
}
