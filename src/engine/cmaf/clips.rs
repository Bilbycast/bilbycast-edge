// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Clip export — the edge half.
//!
//! An operator marks a moment in the browser and asks for so many seconds
//! either side of it. The relay records that as a job and serves it back; this
//! polls for the jobs, assembles the media and PUTs the finished clip where the
//! portal can hand it out.
//!
//! **Why the edge and not the relay.** Cutting means understanding the media,
//! and the relay's contract is that it never parses any: it stores and serves
//! opaque bytes like an HTTP cache. The edge already demuxes, encodes and
//! writes fMP4, so the knowledge lives here.
//!
//! **Why polling and not a command.** The job is a file the relay already
//! serves, and this process already holds that origin's URL and ingest token
//! because it PUTs segments there every two seconds. Polling needs no new
//! message on the manager socket, survives a manager outage, and is idempotent
//! by construction — a job stays pending until its media exists, so a crash
//! mid-cut simply means it is picked up again.
//!
//! **What "covering" means today.** The clip is assembled from whole segments:
//! the init segment followed by every segment that overlaps the window. The
//! out-point is therefore up to one segment late and the in-point up to one
//! segment early. Trimming to the exact frame needs the leading GOP
//! re-encoded — the passthrough rendition only carries a keyframe every two
//! seconds — which is a separate piece of work on top of this one.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::upload::http_put;

/// How often to ask the origin whether anything is waiting.
///
/// A clip is a deliberate act with a human on the other end, so seconds of
/// latency are invisible; this is sized to be inaudible in the request log of
/// a relay serving segments every two seconds.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The relay's job record. A subset — the edge only needs the window.
#[derive(Debug, Clone, Deserialize)]
pub struct ClipRecord {
    pub name: String,
    pub at: String,
    pub pre_secs: u32,
    pub post_secs: u32,
    #[serde(default)]
    pub ready: bool,
}

/// One entry of the media playlist: when it starts, and how long it runs.
#[derive(Debug, Clone)]
struct SegmentEntry {
    uri: String,
    start: DateTime<Utc>,
    duration: f64,
}

/// Parse the served media playlist into dated segments.
///
/// Every segment carries its own `EXT-X-PROGRAM-DATE-TIME` (edge#139), so a
/// wall-clock window maps onto segments without having to model the timeline:
/// the dates are the timeline, and they are the same ones the player used to
/// place the mark.
fn parse_playlist(body: &str) -> Vec<SegmentEntry> {
    let mut out = Vec::new();
    let mut pending_date: Option<DateTime<Utc>> = None;
    let mut pending_dur: Option<f64> = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pending_date = DateTime::parse_from_rfc3339(rest.trim())
                .ok()
                .map(|d| d.with_timezone(&Utc));
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_dur = rest.trim_end_matches(',').trim().parse::<f64>().ok();
        } else if !line.is_empty() && !line.starts_with('#') {
            if let (Some(start), Some(duration)) = (pending_date, pending_dur) {
                // The origin rewrites segment URIs to carry the *viewer's*
                // token, because the browser fetching this playlist needs one.
                // Keeping it would send the edge back with a credential that is
                // not its own — and appending its own on top produced a doubled
                // query and a 403 against the live origin. Take the name only;
                // this process authenticates with its ingest token in a header.
                let uri = line.split(['?', '#']).next().unwrap_or(line).to_string();
                out.push(SegmentEntry {
                    uri,
                    start,
                    duration,
                });
            }
            pending_date = None;
            pending_dur = None;
        }
    }
    out
}

/// The segments overlapping `[from, to]`, in playlist order.
///
/// Overlap rather than containment: a window that starts halfway through a
/// segment still needs that segment, or the clip opens after the moment the
/// operator marked.
fn covering<'a>(
    segments: &'a [SegmentEntry],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<&'a SegmentEntry> {
    segments
        .iter()
        .filter(|s| {
            let end = s.start + chrono::Duration::milliseconds((s.duration * 1000.0) as i64);
            end > from && s.start < to
        })
        .collect()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("build reqwest client")
}

async fn http_get(url: &str, auth: Option<&str>) -> Result<Vec<u8>> {
    let mut req = client().get(url);
    if let Some(t) = auth {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        bail!("GET {url} returned HTTP {}", resp.status().as_u16());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Assemble and upload one clip.
async fn cut_one(base: &str, auth: Option<&str>, rec: &ClipRecord) -> Result<usize> {
    let at: DateTime<Utc> = DateTime::parse_from_rfc3339(&rec.at)
        .with_context(|| format!("clip '{}' has an unparseable timestamp", rec.name))?
        .with_timezone(&Utc);
    let from = at - chrono::Duration::seconds(rec.pre_secs as i64);
    let to = at + chrono::Duration::seconds(rec.post_secs as i64);

    let playlist = http_get(&format!("{base}/manifest.m3u8"), auth).await?;
    let playlist = String::from_utf8_lossy(&playlist);
    let segments = parse_playlist(&playlist);
    if segments.is_empty() {
        bail!("clip '{}': the playlist carries no dated segments", rec.name);
    }
    let wanted = covering(&segments, from, to);
    if wanted.is_empty() {
        // The window has aged out of the relay's DVR window. Nothing to cut,
        // and nothing that waiting will fix.
        bail!(
            "clip '{}': the window {from} .. {to} is outside the {} segments on the origin",
            rec.name,
            segments.len()
        );
    }

    // init.mp4 first, then the fragments: that concatenation *is* a playable
    // fragmented MP4, which is why no muxing is needed to produce one.
    let mut body = http_get(&format!("{base}/init.mp4"), auth).await?;
    for seg in &wanted {
        let url = if seg.uri.starts_with("http") {
            seg.uri.clone()
        } else {
            format!("{base}/{}", seg.uri)
        };
        body.extend_from_slice(&http_get(&url, auth).await?);
    }

    let target = format!("{base}/clips/{}.mp4", urlencoding_light(&rec.name));
    let bytes = body.len();
    http_put(&target, body, "video/mp4", auth).await?;
    Ok(bytes)
}

/// Percent-encode only what a path segment cannot carry.
///
/// Clip names are already restricted by the relay to alphanumerics, spaces,
/// dashes and brackets; a space is the only one a URL path minds.
fn urlencoding_light(name: &str) -> String {
    name.replace(' ', "%20")
}

/// Poll one origin for pending clips until cancelled.
pub async fn run(base_url: String, auth_token: Option<String>, cancel: tokio_util::sync::CancellationToken) {
    let base = base_url.trim_end_matches('/').to_string();
    let auth = auth_token.as_deref();
    tracing::info!(origin = %base, "clip exporter: watching for clip requests");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(origin = %base, "clip exporter stopping (cancelled)");
                return;
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        let listing = match http_get(&format!("{base}/clips"), auth).await {
            Ok(b) => b,
            // A relay that predates clip export answers 404. Not worth a warning
            // every five seconds.
            Err(_) => continue,
        };
        let records: Vec<ClipRecord> = match serde_json::from_slice(&listing) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(origin = %base, error = %e, "clip exporter: unreadable clip listing");
                continue;
            }
        };

        for rec in records.iter().filter(|r| !r.ready) {
            match cut_one(&base, auth, rec).await {
                Ok(bytes) => tracing::info!(
                    clip = %rec.name, bytes, pre = rec.pre_secs, post = rec.post_secs,
                    "clip exporter: cut and uploaded"
                ),
                // Left pending on purpose: the next pass retries. A window that
                // has aged out will keep failing, which is visible in the log
                // and honest — the clip genuinely cannot be produced.
                Err(e) => tracing::warn!(clip = %rec.name, error = %e, "clip exporter: could not cut"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    const PLAYLIST: &str = "#EXTM3U\n\
        #EXT-X-VERSION:7\n\
        #EXT-X-TARGETDURATION:2\n\
        #EXT-X-MEDIA-SEQUENCE:100\n\
        #EXT-X-MAP:URI=\"init.mp4\"\n\
        #EXT-X-PROGRAM-DATE-TIME:2026-09-07T10:00:00.000Z\n\
        #EXTINF:2.000,\n\
        seg-00100.m4s\n\
        #EXT-X-PROGRAM-DATE-TIME:2026-09-07T10:00:02.000Z\n\
        #EXTINF:2.000,\n\
        seg-00101.m4s\n\
        #EXT-X-PROGRAM-DATE-TIME:2026-09-07T10:00:04.000Z\n\
        #EXTINF:2.000,\n\
        seg-00102.m4s\n\
        #EXT-X-PROGRAM-DATE-TIME:2026-09-07T10:00:06.000Z\n\
        #EXTINF:2.000,\n\
        seg-00103.m4s\n";

    #[test]
    fn every_segment_is_read_with_its_own_date() {
        let segs = parse_playlist(PLAYLIST);
        assert_eq!(segs.len(), 4, "one entry per segment");
        assert_eq!(segs[0].uri, "seg-00100.m4s");
        assert_eq!(segs[0].start, t("2026-09-07T10:00:00Z"));
        assert_eq!(segs[3].start, t("2026-09-07T10:00:06Z"));
        assert!((segs[0].duration - 2.0).abs() < 1e-9);
    }

    /// A window that opens mid-segment still needs that segment.
    ///
    /// Containment rather than overlap would drop it, and the clip would open
    /// after the moment the operator marked — the one frame they care about.
    #[test]
    fn a_window_opening_mid_segment_keeps_that_segment() {
        let segs = parse_playlist(PLAYLIST);
        let got = covering(&segs, t("2026-09-07T10:00:03Z"), t("2026-09-07T10:00:05Z"));
        let names: Vec<&str> = got.iter().map(|s| s.uri.as_str()).collect();
        assert_eq!(names, vec!["seg-00101.m4s", "seg-00102.m4s"]);
    }

    #[test]
    fn a_window_outside_the_playlist_covers_nothing() {
        let segs = parse_playlist(PLAYLIST);
        assert!(covering(&segs, t("2026-09-07T09:00:00Z"), t("2026-09-07T09:00:10Z")).is_empty());
        assert!(covering(&segs, t("2026-09-07T11:00:00Z"), t("2026-09-07T11:00:10Z")).is_empty());
    }

    #[test]
    fn a_window_wider_than_the_playlist_takes_everything() {
        let segs = parse_playlist(PLAYLIST);
        assert_eq!(
            covering(&segs, t("2026-09-07T09:59:00Z"), t("2026-09-07T10:01:00Z")).len(),
            4
        );
    }

    /// An entry missing its date is skipped rather than mis-dated.
    ///
    /// Guessing from the previous entry's duration would place a segment on a
    /// timeline the playlist never claimed, and every clip cut near it would be
    /// silently wrong.
    #[test]
    fn an_undated_entry_is_not_guessed_at() {
        let mangled = PLAYLIST.replace("#EXT-X-PROGRAM-DATE-TIME:2026-09-07T10:00:02.000Z\n", "");
        let segs = parse_playlist(&mangled);
        assert_eq!(segs.len(), 3);
        assert!(segs.iter().all(|s| s.uri != "seg-00101.m4s"));
    }

    #[test]
    fn a_space_in_a_clip_name_survives_the_url() {
        assert_eq!(urlencoding_light("09-06-53-05 - Goal"), "09-06-53-05%20-%20Goal");
    }

    /// The origin hands out playlists whose segment URIs already carry a
    /// viewer token, because a browser needs one to fetch them.
    ///
    /// Measured against the live origin: keeping the query and appending the
    /// edge's own credentials produced `seg-x.m4s?token=A?token=B` and a 403 on
    /// every segment. The name is the only part of that line this process
    /// wants — it authenticates in a header.
    #[test]
    fn a_tokened_playlist_uri_is_reduced_to_its_name() {
        let tokened = "#EXTM3U\n\
            #EXT-X-PROGRAM-DATE-TIME:2026-09-07T10:00:00.000Z\n\
            #EXTINF:2.000,\n\
            seg-288675.m4s?token=1788824419.team-a-vs-team-b%2Cteam-a-vs-team-b-proxy.53385b8a\n";
        let segs = parse_playlist(tokened);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].uri, "seg-288675.m4s", "the token was carried into the fetch");
    }
}
