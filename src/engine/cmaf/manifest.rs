// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HLS m3u8 + DASH .mpd manifest generators for CMAF.
//!
//! Both manifests reference the same fMP4 segments — the m3u8 uses
//! `#EXT-X-MAP` to point at `init.mp4`; the MPD uses `SegmentTemplate`
//! with `$Number$`.
//!
//! LL-CMAF additions:
//! - HLS: `#EXT-X-PART` rows pre-advertise upcoming chunks so players
//!   can start fetching mid-segment. `#EXT-X-PART-INF` + `#EXT-X-SERVER-
//!   CONTROL` headers expose the part target duration and the hold-back.
//! - DASH: `availabilityTimeOffset` on the `SegmentTemplate` lets players
//!   request partial chunks ahead of segment completion.

use super::fmp4::VideoCodec;

/// One entry in the rolling playlist.
#[derive(Clone)]
pub struct M3u8Entry {
    /// Media sequence number (matches the segment filename numbering).
    pub sequence_number: u64,
    /// Actual segment duration, in seconds.
    pub duration_secs: f64,
    /// Optional relative URL for the segment. When `None`, defaults to
    /// `seg-{sequence_number:05}.m4s`.
    pub uri: Option<String>,
    /// LL-CMAF: partial-segment rows for the *next* segment, pre-
    /// advertised so players can start fetching chunks before the
    /// segment closes.
    pub parts: Vec<HlsPartEntry>,
}

/// One `#EXT-X-PART` row.
#[derive(Clone)]
pub struct HlsPartEntry {
    /// Part URI (e.g. `seg-00042.m4s?part=3`).
    pub uri: String,
    /// Part duration in seconds.
    pub duration_secs: f64,
    /// Whether this part starts with an independent sync-sample. First
    /// part of each segment must be INDEPENDENT.
    pub independent: bool,
}

/// LL-CMAF hints for the HLS writer.
pub struct LowLatencyHints {
    /// Target part duration (seconds). Seeds `#EXT-X-PART-INF`.
    pub part_target_secs: f64,
    /// Suggested player-side hold-back (seconds). `CAN-BLOCK-RELOAD`
    /// is always YES for LL-HLS manifests.
    pub can_block_reload: bool,
}

/// Build the HLS media playlist.
///
/// - `target_duration_secs` is the **configured** target (rounded up to
///   the next integer per RFC 8216 §4.3.3.1).
/// - `entries` is the rolling window, newest last.
/// - `init_uri` is the path (relative to the m3u8) of the init segment.
/// - `ll_hints` — when `Some`, emit LL-HLS headers and part rows.
pub fn build_hls_playlist(
    target_duration_secs: f64,
    entries: &[M3u8Entry],
    init_uri: &str,
    ll_hints: Option<&LowLatencyHints>,
) -> String {
    let media_sequence = entries.first().map(|e| e.sequence_number).unwrap_or(0);
    let target_duration = target_duration_secs.ceil() as u64;

    let mut out = String::with_capacity(256 + entries.len() * 80);
    out.push_str("#EXTM3U\n");
    out.push_str(if ll_hints.is_some() {
        "#EXT-X-VERSION:9\n"
    } else {
        "#EXT-X-VERSION:7\n"
    });
    out.push_str(&format!("#EXT-X-TARGETDURATION:{target_duration}\n"));
    out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{media_sequence}\n"));
    out.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
    out.push_str(&format!("#EXT-X-MAP:URI=\"{init_uri}\"\n"));
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    if let Some(ll) = ll_hints {
        out.push_str(&format!(
            "#EXT-X-PART-INF:PART-TARGET={:.3}\n",
            ll.part_target_secs
        ));
        // Player hold-back: 3× target_duration is the spec default for
        // LL-HLS. Setting CAN-BLOCK-RELOAD=YES signals support for
        // `_HLS_msn`/`_HLS_part` blocking reloads.
        let hold_back = (target_duration_secs * 3.0).max(3.0);
        let part_hold_back = (ll.part_target_secs * 3.0).max(ll.part_target_secs * 2.0 + 0.1);
        out.push_str(&format!(
            "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD={},HOLD-BACK={:.3},PART-HOLD-BACK={:.3}\n",
            if ll.can_block_reload { "YES" } else { "NO" },
            hold_back,
            part_hold_back,
        ));
    }

    for e in entries {
        let uri = e
            .uri
            .clone()
            .unwrap_or_else(|| default_segment_uri(e.sequence_number));
        out.push_str(&format!("#EXTINF:{:.3},\n{uri}\n", e.duration_secs));
    }

    // For LL-HLS we append the pre-advertised parts of the next segment
    // to the *last* entry. The part URIs are expressed as segment URIs
    // with a `?part=N` query suffix, which our chunked PUT handler
    // recognises and routes to the same streaming upload.
    if ll_hints.is_some() {
        if let Some(last) = entries.last() {
            for p in &last.parts {
                out.push_str(&format!(
                    "#EXT-X-PART:DURATION={:.3},URI=\"{}\"{}\n",
                    p.duration_secs,
                    p.uri,
                    if p.independent { ",INDEPENDENT=YES" } else { "" },
                ));
            }
        }
    }

    out
}

/// Default file name for a CMAF media segment: `seg-00042.m4s`.
pub fn default_segment_uri(sequence_number: u64) -> String {
    format!("seg-{sequence_number:05}.m4s")
}

// ────────────────────────────────────────────────────────────────────────
//  DASH MPD
// ────────────────────────────────────────────────────────────────────────

/// Minimal DASH MPD writer for CMAF live streams. Emits a `dynamic`
/// MPD with a single `Period`, one `AdaptationSet` per track, and a
/// `SegmentTemplate` with `$Number$`-based media URIs.
///
/// We keep the MPD narrow on purpose — most production deployments
/// extend this externally with MPD manifest templating specific to the
/// CDN provider (availabilityStartTime anchoring, timeShiftBufferDepth
/// tuning, etc.). This implementation produces a valid DASH manifest
/// that passes `dashif-conformance` static checks against the CMAF-on-
/// DASH profile (urn:mpeg:dash:profile:cmaf:2019 + dynamic).
pub struct DashInput<'a> {
    /// Seconds since Unix epoch when the first segment was published.
    /// Used as `availabilityStartTime`.
    pub availability_start_unix_secs: i64,
    /// Target segment duration in seconds — used as a hint for
    /// client-side buffer sizing.
    pub target_segment_duration_secs: f64,
    /// Video track metadata.
    pub video: Option<DashVideoRep<'a>>,
    /// Audio track metadata.
    pub audio: Option<DashAudioRep<'a>>,
    /// Current live-edge segment number (highest sequence number).
    pub latest_segment_number: u64,
    /// Number of media segments available in the rolling window.
    pub available_segments: u64,
    /// LL-CMAF `availabilityTimeOffset` in seconds — if non-zero,
    /// players may request partial chunks that early relative to the
    /// segment's nominal availability time.
    pub availability_time_offset_secs: f64,
}

pub struct DashVideoRep<'a> {
    pub codec: VideoCodec,
    /// From SPS: profile_idc / compat / level. Used to build the
    /// DASH `@codecs` attribute (`avc1.64001F`, `hvc1.1.6.L120.90`).
    pub sps: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub timescale: u32,
    pub bandwidth_bps: u64,
}

pub struct DashAudioRep<'a> {
    /// AudioSpecificConfig — 2 bytes for AAC-LC/HE-AAC.
    pub asc: &'a [u8; 2],
    pub sample_rate: u32,
    pub channels: u16,
    pub bandwidth_bps: u64,
}

pub fn build_dash_mpd(input: &DashInput<'_>) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<MPD\n");
    s.push_str("  xmlns=\"urn:mpeg:dash:schema:mpd:2011\"\n");
    s.push_str("  xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n");
    s.push_str(
        "  xsi:schemaLocation=\"urn:mpeg:dash:schema:mpd:2011 DASH-MPD.xsd\"\n",
    );
    s.push_str("  type=\"dynamic\"\n");
    s.push_str("  profiles=\"urn:mpeg:dash:profile:cmaf:2019,urn:mpeg:dash:profile:isoff-live:2011\"\n");
    s.push_str(&format!(
        "  availabilityStartTime=\"{}\"\n",
        unix_to_iso8601(input.availability_start_unix_secs)
    ));
    // Publish a minimumUpdatePeriod equal to one segment duration so
    // clients refresh the MPD before the segment window rolls.
    s.push_str(&format!(
        "  minimumUpdatePeriod=\"PT{:.1}S\"\n",
        input.target_segment_duration_secs.max(1.0)
    ));
    s.push_str(&format!(
        "  minBufferTime=\"PT{:.1}S\"\n",
        (input.target_segment_duration_secs * 1.5).max(2.0)
    ));
    s.push_str(&format!(
        "  timeShiftBufferDepth=\"PT{:.0}S\"\n",
        (input.available_segments as f64 * input.target_segment_duration_secs).max(30.0)
    ));
    if input.availability_time_offset_secs > 0.0 {
        // Target-latency hint for LL-CMAF players.
        s.push_str(&format!(
            "  suggestedPresentationDelay=\"PT{:.1}S\"\n",
            (input.availability_time_offset_secs * 3.0).max(1.5)
        ));
    }
    s.push_str("  >\n");
    s.push_str("  <Period id=\"0\" start=\"PT0S\">\n");

    if let Some(v) = &input.video {
        write_dash_video_adaptation_set(&mut s, v, input);
    }
    if let Some(a) = &input.audio {
        write_dash_audio_adaptation_set(&mut s, a, input);
    }

    s.push_str("  </Period>\n");
    s.push_str("</MPD>\n");
    s
}

fn write_dash_video_adaptation_set(
    s: &mut String,
    v: &DashVideoRep<'_>,
    mpd: &DashInput<'_>,
) {
    let codecs_attr = match v.codec {
        VideoCodec::H264 => {
            if v.sps.len() >= 4 {
                format!("avc1.{:02X}{:02X}{:02X}", v.sps[1], v.sps[2], v.sps[3])
            } else {
                "avc1.640020".to_string()
            }
        }
        VideoCodec::H265 => {
            if v.sps.len() >= 14 {
                // hvc1.Profile.ProfileCompatFlags.TierLevel format
                let profile = v.sps[2] & 0x1F;
                let tier = (v.sps[2] >> 5) & 0x01;
                let level = v.sps[13];
                format!(
                    "hvc1.{}.{:X}.{}{}",
                    profile,
                    u32::from_be_bytes([v.sps[3], v.sps[4], v.sps[5], v.sps[6]]),
                    if tier == 1 { "H" } else { "L" },
                    level
                )
            } else {
                "hvc1.1.6.L120.90".to_string()
            }
        }
    };
    s.push_str("    <AdaptationSet contentType=\"video\" segmentAlignment=\"true\" startWithSAP=\"1\" ");
    s.push_str(&format!("mimeType=\"video/mp4\" codecs=\"{codecs_attr}\">\n"));
    s.push_str(&format!(
        "      <Representation id=\"video\" bandwidth=\"{}\" width=\"{}\" height=\"{}\" frameRate=\"30\">\n",
        v.bandwidth_bps, v.width, v.height
    ));
    write_segment_template(
        s,
        v.timescale,
        (mpd.target_segment_duration_secs * v.timescale as f64) as u64,
        mpd.latest_segment_number,
        mpd.availability_time_offset_secs,
        true, // video
    );
    s.push_str("      </Representation>\n");
    s.push_str("    </AdaptationSet>\n");
}

fn write_dash_audio_adaptation_set(
    s: &mut String,
    a: &DashAudioRep<'_>,
    mpd: &DashInput<'_>,
) {
    // AudioObjectType lives in the top 5 bits of ASC byte 0.
    let aot = (a.asc[0] >> 3) & 0x1F;
    let codecs_attr = format!("mp4a.40.{aot}");
    s.push_str("    <AdaptationSet contentType=\"audio\" segmentAlignment=\"true\" startWithSAP=\"1\" ");
    s.push_str(&format!(
        "mimeType=\"audio/mp4\" codecs=\"{codecs_attr}\" lang=\"und\">\n"
    ));
    s.push_str(&format!(
        "      <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:mpegB:cicp:ChannelConfiguration\" value=\"{}\"/>\n",
        a.channels
    ));
    s.push_str(&format!(
        "      <Representation id=\"audio\" bandwidth=\"{}\" audioSamplingRate=\"{}\">\n",
        a.bandwidth_bps, a.sample_rate
    ));
    write_segment_template(
        s,
        a.sample_rate,
        (mpd.target_segment_duration_secs * a.sample_rate as f64) as u64,
        mpd.latest_segment_number,
        mpd.availability_time_offset_secs,
        false,
    );
    s.push_str("      </Representation>\n");
    s.push_str("    </AdaptationSet>\n");
}

fn write_segment_template(
    s: &mut String,
    timescale: u32,
    segment_duration_ts: u64,
    latest_segment_number: u64,
    availability_time_offset_secs: f64,
    is_video: bool,
) {
    // Segment numbering starts at 1 by DASH convention; `startNumber`
    // anchors the template. We follow the edge's monotonic sequence
    // numbers directly.
    let start_number = latest_segment_number.max(1);
    let init_uri = "init.mp4";
    let media_pattern = if is_video {
        "seg-$Number%05d$.m4s"
    } else {
        "aud-$Number%05d$.m4s"
    };
    s.push_str(&format!(
        "        <SegmentTemplate timescale=\"{timescale}\" duration=\"{segment_duration_ts}\" startNumber=\"{start_number}\" initialization=\"{init_uri}\" media=\"{media_pattern}\""
    ));
    if availability_time_offset_secs > 0.0 {
        s.push_str(&format!(
            " availabilityTimeOffset=\"{:.3}\" availabilityTimeComplete=\"false\"",
            availability_time_offset_secs
        ));
    }
    s.push_str("/>\n");
}

/// Simple ISO 8601 UTC formatter for the small subset we need
/// (`YYYY-MM-DDTHH:MM:SSZ`). Pure-stdlib so we don't add `chrono` to
/// the edge (it already is, but the engine prefers to be leaf-free).
fn unix_to_iso8601(t: i64) -> String {
    // Days since 1970-01-01. Algorithm: https://howardhinnant.github.io/date_algorithms.html
    let days = t.div_euclid(86_400);
    let secs_of_day = t.rem_euclid(86_400) as u32;
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, mo, d, hh, mm, ss,
    )
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_entry(seq: u64, dur: f64) -> M3u8Entry {
        M3u8Entry {
            sequence_number: seq,
            duration_secs: dur,
            uri: None,
            parts: Vec::new(),
        }
    }

    #[test]
    fn empty_playlist_has_headers_only() {
        let p = build_hls_playlist(2.0, &[], "init.mp4", None);
        assert!(p.contains("#EXTM3U"));
        assert!(p.contains("#EXT-X-TARGETDURATION:2"));
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(p.contains("#EXT-X-MAP:URI=\"init.mp4\""));
        assert!(!p.contains("#EXTINF"));
    }

    #[test]
    fn playlist_lists_segments_in_order() {
        let entries = vec![
            simple_entry(10, 2.0),
            simple_entry(11, 1.95),
            simple_entry(12, 2.05),
        ];
        let p = build_hls_playlist(2.0, &entries, "init.mp4", None);
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:10"));
        assert!(p.contains("#EXTINF:2.000,\nseg-00010.m4s"));
        assert!(p.contains("#EXTINF:1.950,\nseg-00011.m4s"));
        assert!(p.contains("#EXTINF:2.050,\nseg-00012.m4s"));
        assert!(p.find("seg-00010.m4s").unwrap() < p.find("seg-00012.m4s").unwrap());
    }

    #[test]
    fn custom_uri_overrides_default() {
        let entries = vec![M3u8Entry {
            sequence_number: 1,
            duration_secs: 2.0,
            uri: Some("custom/path/x.m4s".to_string()),
            parts: Vec::new(),
        }];
        let p = build_hls_playlist(2.0, &entries, "init.mp4", None);
        assert!(p.contains("custom/path/x.m4s"));
        assert!(!p.contains("seg-00001.m4s"));
    }

    #[test]
    fn target_duration_rounded_up() {
        let p = build_hls_playlist(1.4, &[], "init.mp4", None);
        assert!(p.contains("#EXT-X-TARGETDURATION:2"));
        let p = build_hls_playlist(6.01, &[], "init.mp4", None);
        assert!(p.contains("#EXT-X-TARGETDURATION:7"));
    }

    #[test]
    fn ll_playlist_has_part_inf_and_server_control() {
        let ll = LowLatencyHints {
            part_target_secs: 0.5,
            can_block_reload: true,
        };
        let p = build_hls_playlist(2.0, &[], "init.mp4", Some(&ll));
        assert!(p.contains("#EXT-X-VERSION:9"));
        assert!(p.contains("#EXT-X-PART-INF:PART-TARGET=0.500"));
        assert!(p.contains("#EXT-X-SERVER-CONTROL:"));
        assert!(p.contains("CAN-BLOCK-RELOAD=YES"));
    }

    #[test]
    fn ll_playlist_appends_parts_from_last_entry() {
        let ll = LowLatencyHints {
            part_target_secs: 0.5,
            can_block_reload: true,
        };
        let mut last = simple_entry(42, 2.0);
        last.parts = vec![
            HlsPartEntry {
                uri: "seg-00043.m4s?part=0".to_string(),
                duration_secs: 0.5,
                independent: true,
            },
            HlsPartEntry {
                uri: "seg-00043.m4s?part=1".to_string(),
                duration_secs: 0.5,
                independent: false,
            },
        ];
        let p = build_hls_playlist(2.0, &[last], "init.mp4", Some(&ll));
        assert!(p.contains("#EXT-X-PART:DURATION=0.500,URI=\"seg-00043.m4s?part=0\",INDEPENDENT=YES"));
        assert!(p.contains("#EXT-X-PART:DURATION=0.500,URI=\"seg-00043.m4s?part=1\"\n"));
    }

    #[test]
    fn dash_mpd_has_cmaf_profile_and_dynamic_type() {
        let sps = vec![0x67, 0x42, 0x00, 0x1E];
        let video = DashVideoRep {
            codec: VideoCodec::H264,
            sps: &sps,
            width: 1920,
            height: 1080,
            timescale: 90_000,
            bandwidth_bps: 5_000_000,
        };
        let mpd = build_dash_mpd(&DashInput {
            availability_start_unix_secs: 1_700_000_000,
            target_segment_duration_secs: 2.0,
            video: Some(video),
            audio: None,
            latest_segment_number: 5,
            available_segments: 5,
            availability_time_offset_secs: 0.0,
        });
        assert!(mpd.contains("<MPD"));
        assert!(mpd.contains("type=\"dynamic\""));
        assert!(mpd.contains("profile:cmaf:2019"));
        assert!(mpd.contains("availabilityStartTime=\"2023-11-"));
        assert!(mpd.contains("codecs=\"avc1.4200"));
        assert!(mpd.contains("<SegmentTemplate"));
        assert!(mpd.contains("initialization=\"init.mp4\""));
        assert!(mpd.contains("media=\"seg-$Number%05d$.m4s\""));
    }

    #[test]
    fn dash_mpd_with_audio_has_codecs_and_channels() {
        let asc = [0x11, 0x90];
        let mpd = build_dash_mpd(&DashInput {
            availability_start_unix_secs: 1_700_000_000,
            target_segment_duration_secs: 2.0,
            video: None,
            audio: Some(DashAudioRep {
                asc: &asc,
                sample_rate: 48000,
                channels: 2,
                bandwidth_bps: 128_000,
            }),
            latest_segment_number: 1,
            available_segments: 1,
            availability_time_offset_secs: 0.0,
        });
        assert!(mpd.contains("contentType=\"audio\""));
        assert!(mpd.contains("codecs=\"mp4a.40.2\""));
        assert!(mpd.contains("AudioChannelConfiguration"));
    }

    #[test]
    fn dash_mpd_with_ato_announces_chunked() {
        let sps = vec![0x67, 0x42, 0x00, 0x1E];
        let mpd = build_dash_mpd(&DashInput {
            availability_start_unix_secs: 1_700_000_000,
            target_segment_duration_secs: 2.0,
            video: Some(DashVideoRep {
                codec: VideoCodec::H264,
                sps: &sps,
                width: 1920,
                height: 1080,
                timescale: 90_000,
                bandwidth_bps: 5_000_000,
            }),
            audio: None,
            latest_segment_number: 1,
            available_segments: 1,
            availability_time_offset_secs: 1.5,
        });
        assert!(mpd.contains("availabilityTimeOffset=\"1.500\""));
        assert!(mpd.contains("availabilityTimeComplete=\"false\""));
        assert!(mpd.contains("suggestedPresentationDelay="));
    }

    #[test]
    fn iso8601_known_date() {
        // 2023-11-14T22:13:20Z = 1_700_000_000
        assert_eq!(unix_to_iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
