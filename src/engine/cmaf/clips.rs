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
    /// Already given up on. Skipped rather than retried for the life of the
    /// session.
    #[serde(default)]
    pub failed: bool,
}

/// How many times a clip is attempted before it is called impossible.
///
/// Some failures are worth retrying — the origin restarting mid-fetch, a
/// segment not yet uploaded. Most are not, and a clip that can never be cut
/// was previously attempted every five seconds until the session ended,
/// logging a warning each time and telling the viewer it was still "being
/// cut".
const MAX_ATTEMPTS: u32 = 3;

/// Does this failure have any prospect of coming good?
///
/// The window aging out and the clip being too large are settled facts: the
/// media is gone, or it will be exactly as large next time. Retrying either
/// wastes the edge's time and delays the operator learning the truth.
fn is_permanent(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("outside the")
        || e.contains("413")
        || e.contains("too large")
        || e.contains("empty after mapping")
        || e.contains("no dated segments")
        // Retrying cannot move the restart. Without this the operator waits
        // through three attempts for an answer that was settled at the first.
        || e.contains("spans a recorder restart")
}

/// Tell the origin a clip cannot be produced, so it stops being pending.
async fn report_failure(base: &str, auth: Option<&str>, name: &str, reason: &str) {
    let url = format!("{base}/clips/{}.mp4/failed", urlencoding_light(name));
    let mut req = client()
        .post(&url)
        .json(&serde_json::json!({ "reason": reason }));
    if let Some(t) = auth {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => tracing::warn!(clip = %name, status = r.status().as_u16(),
            "clip exporter: origin would not record the failure"),
        Err(e) => tracing::warn!(clip = %name, error = %e,
            "clip exporter: could not reach the origin to record the failure"),
    }
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

/// Fetch a stream's current media playlist from the origin.
///
/// Exposed for the output-start path, which reads back the previous run's
/// manifest so a restart does not republish an empty window. Same credential
/// and same helper the clip poller uses.
pub(super) async fn fetch_manifest(base: &str, auth: Option<&str>) -> Result<Vec<u8>> {
    http_get(&format!("{base}/manifest.m3u8"), auth).await
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

/// A wall-clock instant as a PTS in the recording, via its anchor.
///
/// `pts = anchor_pts + (wall - anchor_wall) * 90_000 / 1_000_000`, computed in
/// i128 so a mark well before the recording began cannot wrap a u64 into a
/// range near the end of time and hand the exporter something absurd. `None`
/// means "before this recording started", which is a real answer: the media
/// does not exist and the caller falls back rather than cutting nonsense.
fn pts_for_wall(anchor_wall_us: i64, anchor_pts: u64, wall_us: i64) -> Option<u64> {
    let delta_us = (wall_us as i128) - (anchor_wall_us as i128);
    let ticks = anchor_pts as i128 + delta_us * 90_000 / 1_000_000;
    u64::try_from(ticks).ok()
}

/// Cut exactly, from the local replay recording.
///
/// Returns `Ok(None)` when there is nothing to cut from — no recording for
/// this flow, or one made before the wall-clock anchor existed — so the caller
/// can fall back to whole segments rather than fail.
///
/// The mapping is the anchor written on the recording's first indexed frame:
/// `pts = anchor_pts + (wall - anchor_wall) * 90_000`. Measured on the rig at
/// -36ms against the CMAF published dates, inside one frame at 25fps, where
/// `created_at_unix` was out by anywhere from half a second to nineteen.
#[cfg(feature = "replay")]
async fn cut_exact(flow_id: &str, rec: &ClipRecord) -> Result<Option<Vec<u8>>> {
    let dir = crate::replay::recording_dir(flow_id);
    let Ok(raw) = tokio::fs::read(dir.join("recording.json")).await else {
        return Ok(None);
    };
    let meta: serde_json::Value = serde_json::from_slice(&raw)?;
    let (Some(anchor_wall_us), Some(anchor_pts)) = (
        meta.get("anchor_wall_us").and_then(|v| v.as_i64()),
        meta.get("anchor_pts_90khz").and_then(|v| v.as_u64()),
    ) else {
        tracing::info!(
            flow_id, clip = %rec.name,
            "clip exporter: recording has no wall-clock anchor; \
             falling back to whole segments"
        );
        return Ok(None);
    };

    let at: DateTime<Utc> = DateTime::parse_from_rfc3339(&rec.at)?.with_timezone(&Utc);
    let (Some(from), Some(to)) = (
        pts_for_wall(
            anchor_wall_us,
            anchor_pts,
            (at - chrono::Duration::seconds(rec.pre_secs as i64)).timestamp_micros(),
        ),
        pts_for_wall(
            anchor_wall_us,
            anchor_pts,
            (at + chrono::Duration::seconds(rec.post_secs as i64)).timestamp_micros(),
        ),
    ) else {
        return Ok(None);
    };
    if to <= from {
        bail!("clip '{}': the window is empty after mapping to PTS", rec.name);
    }

    // A moment that spans a recorder restart cannot be exported at all.
    //
    // The index's own timeline is continuous across a restart — the writer
    // resumes its counter — but the *media* is not: the PCR in the TS begins
    // again with the process. Muxing across the join produced a playable file
    // declaring itself 7.5 hours long from 30 seconds of video.
    //
    // Handing it to the segment fallback was the first answer here, and it is
    // wrong: the relay's CMAF renditions restart with the same edge, so their
    // media timeline resets at exactly the same instant. Measured, rather than
    // assumed — the fallback answered a 30-second request with a file
    // declaring 70,567 seconds. Both paths have the same join in them.
    //
    // So this is refused, with words an operator can act on. Producing a file
    // that plays but lies about its own length is the worse outcome: it is
    // discovered in an edit suite, not here.
    let index = crate::replay::index::InMemoryIndex::load(&dir.join("index.bin"))
        .await
        .unwrap_or_default();
    if index.spans_discontinuity(from, to) {
        bail!(
            "clip '{}' spans a recorder restart, and the media either side of it \
             is two separate timelines — move the mark clear of the restart and \
             export it again",
            rec.name
        );
    }

    // Ask for one random-access point past the end, so the clip covers the
    // window instead of stopping short of it.
    //
    // The exporter bounds a range with `find_floor` at both ends. At the start
    // that rounds outward and the opening moment is safe. At the end it rounds
    // *inward*: the range stops at the last random-access point at or before
    // `to`, so up to a whole GOP of what was asked for is missing — measured
    // at 26.35s of a 30s request. Naming the next point instead makes the
    // exporter's floor land exactly on it.
    //
    // Done here rather than in `plan_pts_range`, which is shared with the
    // manager's mark-in/mark-out export and has its own settled semantics.
    let to_covering = index.first_after(to).unwrap_or(to);

    // The exporter chunks; a clip is wanted whole.
    let mut out: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    loop {
        let chunk = match crate::replay::export_mp4::export_recording_mp4_chunk(
            flow_id,
            Some(from),
            Some(to_covering),
            offset,
            8 * 1024 * 1024,
        )
        .await
        {
            Ok(c) => c,
            // The recording could not serve this moment — which is exactly
            // what the segment fallback is for, so hand it over rather than
            // failing the export.
            //
            // The common cause is retention: the index still names a segment
            // that has since been pruned, and the exporter answers "stat
            // segment …: No such file". The relay's origin window is
            // configured independently and often still holds the media, so a
            // clip that the recorder has aged out of is frequently still
            // cuttable — just on segment boundaries instead of the frame.
            //
            // Any other failure lands here too, and deliberately: a coarser
            // clip beats no clip, and the reason is logged either way.
            Err(e) => {
                tracing::warn!(
                    flow_id, clip = %rec.name, error = %format!("{e:#}"),
                    "clip exporter: the recording could not serve this moment;                      cutting from whole segments instead"
                );
                return Ok(None);
            }
        };
        let got = chunk.data.len() as u64;
        out.extend_from_slice(&chunk.data);
        if chunk.eof || got == 0 {
            break;
        }
        offset += got;
    }
    if out.is_empty() {
        return Ok(None);
    }
    tracing::info!(
        flow_id, clip = %rec.name, from_pts = from, to_pts = to, bytes = out.len(),
        "clip exporter: cut exactly from the replay recording"
    );
    Ok(Some(out))
}

#[cfg(not(feature = "replay"))]
async fn cut_exact(_flow_id: &str, _rec: &ClipRecord) -> Result<Option<Vec<u8>>> {
    Ok(None)
}

/// Assemble and upload one clip.
async fn cut_one(base: &str, auth: Option<&str>, flow_id: &str, rec: &ClipRecord) -> Result<usize> {
    // Exact if the recorder is running for this flow; whole segments if not.
    // The fallback is not a lesser mode to be ashamed of — it needs no second
    // copy of the media on the edge — but it lands on segment boundaries, so
    // prefer the cut that lands on the frame.
    if let Some(bytes) = cut_exact(flow_id, rec).await? {
        let target = format!("{base}/clips/{}.mp4", urlencoding_light(&rec.name));
        let n = bytes.len();
        http_put(&target, bytes, "video/mp4", auth).await?;
        return Ok(n);
    }
    cut_from_segments(base, auth, rec).await
}

/// Assemble from whole segments — the fallback when nothing is recorded locally.
async fn cut_from_segments(base: &str, auth: Option<&str>, rec: &ClipRecord) -> Result<usize> {
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
pub async fn run(
    base_url: String,
    auth_token: Option<String>,
    flow_id: String,
    cancel: tokio_util::sync::CancellationToken,
) {
    let base = base_url.trim_end_matches('/').to_string();
    let auth = auth_token.as_deref();
    // Attempts per clip, in memory only: a restart is a fresh chance, which is
    // the right default when the reason for failing may have been the restart.
    let mut attempts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    // Whether the "cannot read the clip list" warning has already been said
    // for the current spell of failures. Cleared by the first success, so a
    // fault that comes back is reported again.
    let mut quiet = false;
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
            Ok(b) => {
                quiet = false;
                b
            }
            Err(e) => {
                // A relay that predates clip export answers 404, and saying so
                // every five seconds helps nobody. Everything else does need
                // saying: swallowing all of it hid a 403 on every single poll
                // — the exporter was locked out of its own work queue and the
                // log looked perfectly healthy. Said once per spell, so a
                // persistent fault is visible without becoming a firehose.
                let msg = format!("{e:#}");
                if !msg.contains("404") && !quiet {
                    tracing::warn!(
                        origin = %base, error = %msg,
                        "clip exporter: cannot read the clip list; nothing will be cut \
                         until this clears"
                    );
                    quiet = true;
                }
                continue;
            }
        };
        let records: Vec<ClipRecord> = match serde_json::from_slice(&listing) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(origin = %base, error = %e, "clip exporter: unreadable clip listing");
                continue;
            }
        };

        for rec in records.iter().filter(|r| !r.ready && !r.failed) {
            match cut_one(&base, auth, &flow_id, rec).await {
                Ok(bytes) => {
                    attempts.remove(&rec.name);
                    tracing::info!(
                        clip = %rec.name, bytes, pre = rec.pre_secs, post = rec.post_secs,
                        "clip exporter: cut and uploaded"
                    );
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let n = attempts.entry(rec.name.clone()).or_insert(0);
                    *n += 1;
                    if is_permanent(&msg) || *n >= MAX_ATTEMPTS {
                        tracing::warn!(
                            clip = %rec.name, attempts = *n, error = %msg,
                            "clip exporter: giving up on this clip"
                        );
                        report_failure(&base, auth, &rec.name, &msg).await;
                        attempts.remove(&rec.name);
                    } else {
                        tracing::info!(
                            clip = %rec.name, attempt = *n, error = %msg,
                            "clip exporter: could not cut; will try again"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clip spanning a recorder restart is refused once, not three times.
    ///
    /// Retrying cannot move the restart, and the operator waiting through
    /// three attempts learns nothing they could not have been told at the
    /// first. Both paths carry the same join — the recorder's TS and the
    /// relay's CMAF renditions restart with the same process — so there is no
    /// fallback left to try.
    #[test]
    fn a_clip_across_a_restart_is_not_retried() {
        assert!(
            is_permanent("clip 'Goal' spans a recorder restart, and the media either side"),
            "the operator would wait through three attempts for a settled answer"
        );
        // And the transient cases must stay retryable: a segment that has not
        // been uploaded yet is exactly what a second attempt fixes.
        for e in [
            "GET https://origin/manifest.m3u8 returned HTTP 503",
            "connection reset by peer",
            "GET https://origin/seg-00042.m4s returned HTTP 404",
        ] {
            assert!(!is_permanent(e), "{e} must still be retried");
        }
    }

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

    /// The anchor measured on the rig, mapped back.
    ///
    /// Real values from bilby-z440: the recording anchored PTS 158400 to
    /// 1788827202551745 us. A mark one second later must land exactly 90000
    /// ticks on, and the identity case must return the anchor untouched — an
    /// error of one 90 kHz tick here is invisible in review and wrong in every
    /// clip.
    #[test]
    fn a_wall_instant_maps_to_the_pts_the_anchor_implies() {
        let aw = 1_788_827_202_551_745_i64;
        let ap = 158_400_u64;
        assert_eq!(pts_for_wall(aw, ap, aw), Some(ap), "the anchor itself must not move");
        assert_eq!(pts_for_wall(aw, ap, aw + 1_000_000), Some(ap + 90_000), "one second");
        assert_eq!(pts_for_wall(aw, ap, aw + 40_000), Some(ap + 3_600), "one frame at 25fps");
        assert_eq!(
            pts_for_wall(aw, ap, aw - 1_000_000),
            Some(ap - 90_000),
            "a second before the anchor is still inside the recording"
        );
    }

    /// A mark from before the recording began has no media behind it.
    ///
    /// The subtraction must not wrap: a u64 underflow would ask the exporter
    /// for a range near the end of time, which reads as a corrupt request
    /// rather than an absent one.
    #[test]
    fn a_mark_before_the_recording_started_is_refused_not_wrapped() {
        let aw = 1_788_827_202_551_745_i64;
        let ap = 158_400_u64; // 1.76s of media before the anchor
        // Two seconds earlier is past the start of the recording.
        assert_eq!(pts_for_wall(aw, ap, aw - 2_000_000), None);
        // An hour earlier certainly is.
        assert_eq!(pts_for_wall(aw, ap, aw - 3_600_000_000), None);
    }

    /// Some failures are worth retrying and most are not.
    ///
    /// A window that has aged out and a clip that is too large are settled
    /// facts — retrying either burns the edge's time and delays the operator
    /// learning the truth. A refused connection might be the origin restarting.
    #[test]
    fn a_settled_failure_is_not_retried_and_a_transient_one_is() {
        assert!(is_permanent("clip 'x': the window .. is outside the 1800 segments"));
        assert!(is_permanent("PUT https://relay/clips/x.mp4 returned HTTP 413 — clip too large"));
        assert!(is_permanent("clip 'x': the window is empty after mapping to PTS"));
        assert!(is_permanent("clip 'x': the playlist carries no dated segments"));

        assert!(!is_permanent("error sending request for url: connection refused"));
        assert!(!is_permanent("GET https://relay/manifest.m3u8 returned HTTP 503"));
        assert!(!is_permanent("operation timed out"));
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
