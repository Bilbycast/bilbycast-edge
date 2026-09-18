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
//! **Two ways to cut.** The primary path cuts *exactly*, from the flow's own
//! Replay recording: the mark's wall-clock dates are mapped to PTS through
//! the recording's anchor and measured rate (`replay::clock`), the covering
//! range is read from the recording, decoded, re-encoded all-intra and muxed
//! as a progressive MP4 (`replay::export_mp4`). When there is no recording to
//! cut from — no recorder on the flow, a recording made before the anchor
//! existed, or a moment the recording has since aged out of — the clip is
//! assembled from whole segments instead: the init segment followed by every
//! segment that overlaps the window, so the out-point is up to one segment
//! late and the in-point up to one segment early. A coarser clip, not a
//! broken one.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::upload::http_put_within;
use crate::replay::clock::pts_for_wall;
use crate::manager::events::{EventSeverity, category};

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

/// Statuses that mean "this endpoint is not a clip queue and never will be".
///
/// 404 is the bilbycast relay that predates clip export answering its own
/// not-found; 400 is an older relay whose object handler rejects `clips` for
/// having no file extension; 405 is a CDN ingest that accepts PUT and nothing
/// else. **Not 403.** On the bilbycast relay a 403 is a bearer it rejected —
/// this edge's `auth_token` no longer matching the relay's secret — and with
/// segment PUTs still landing nothing else about the session is loud, so
/// treating it as "not a clip origin" abandoned the queue in thirty seconds,
/// silently, in the one case a fix on the manager side would have cured. It
/// takes the fault branch below instead, which warns, raises the event and
/// keeps asking.
const NOT_A_CLIP_ORIGIN: [u16; 3] = [400, 404, 405];

/// How many consecutive such answers before the poller backs off.
///
/// Not one: a relay restarting can answer 404 for a poll or two. Six is half a
/// minute at the poll interval, which no restart outlasts, and after that the
/// endpoint is simply not one that serves clips — until it is upgraded, so the
/// poller does not stop, it slows to [`UNSUPPORTED_POLL_INTERVAL`].
const UNSUPPORTED_GIVE_UP: u32 = 6;

/// How often an origin that does not serve clips is asked again.
///
/// Asking a third-party packager every five seconds for the life of the flow
/// is somebody else's traffic; asking it never means a relay upgraded
/// mid-session is not noticed until the flow restarts. Once every five
/// minutes is 288 requests a day, and a queue picked up within five minutes of
/// the upgrade.
const UNSUPPORTED_POLL_INTERVAL: Duration = Duration::from_secs(300);

/// Does this failure have any prospect of coming good?
///
/// The window aging out and the clip being too large are settled facts: the
/// media is gone, or it will be exactly as large next time. Retrying either
/// wastes the edge's time and delays the operator learning the truth.
fn is_permanent(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("outside the")
        // "returned http 413", not a bare "413". The formatted chain embeds
        // the segment URI, and segment names are `seg-{seq:05}.m4s` — about
        // three per cent of five-digit sequence numbers contain the literal
        // 413, so a 30-second window had roughly a one-in-three chance of
        // containing one. A transient 503 on `seg-00413.m4s` was then called
        // settled and the viewer told the clip could not be produced, when a
        // retry would have delivered it. Clip names are quoted in these
        // messages too, which is how "Lap 413" made every failure permanent.
        || e.contains("returned http 413")
        || e.contains("clip too large")
        || e.contains("empty after mapping")
        || e.contains("no dated segments")
        // Retrying cannot move the restart. Without this the operator waits
        // through three attempts for an answer that was settled at the first.
        || e.contains("spans a recorder restart")
}

/// Hand the allocator's free heap back to the operating system.
///
/// Cutting a clip is a spike, not a working set: for a thirty-second export
/// the process transiently holds the source frames, the re-encoded frames, the
/// interleaved payload and the finished file — around 300 MB, all of it freed
/// the moment the clip is uploaded.
///
/// Freed to *the process*, that is. glibc keeps large runs of it in its arenas
/// rather than returning them, and because each cut allocates a slightly
/// different shape the arenas fragment instead of being reused. Measured on
/// the rig: eight clips took the edge from 2.4 GB to 4.5 GB and kept climbing,
/// on a box with 15 GB shared with a second edge — a demo of a dozen clips
/// would have run it out of memory.
///
/// `malloc_trim` is the one call that fixes that, and it is worth making here:
/// the cut is over and it runs once per clip rather than in any hot loop.
///
/// It is not a complete answer on its own, and the export cache is why: an
/// entry the cache still holds is a **live** allocation, which `malloc_trim`
/// cannot return however often it is called. The clip exporter therefore takes
/// the uncached whole-file path (`export_recording_mp4_whole`), so by the time
/// this runs there genuinely is nothing of the clip left alive.
///
/// Awaited on a blocking thread rather than called inline: the call takes each
/// arena's lock in turn and runs for tens of milliseconds on a fragmented heap,
/// which is not something to do on a runtime worker of a process carrying live
/// transport.
#[cfg(target_env = "gnu")]
async fn release_free_heap() {
    let _ = tokio::task::spawn_blocking(|| {
        // SAFETY: `malloc_trim` inspects the allocator's own free lists and
        // releases whole pages back to the kernel. It takes no pointer from us
        // and cannot invalidate any live allocation.
        unsafe {
            libc::malloc_trim(0);
        }
    })
    .await;
}

/// Nothing to do where the allocator is not glibc — musl and macOS return
/// pages on free.
#[cfg(not(target_env = "gnu"))]
async fn release_free_heap() {}

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
fn covering(
    segments: &[SegmentEntry],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<&SegmentEntry> {
    segments
        .iter()
        .filter(|s| {
            let end = s.start + chrono::Duration::milliseconds((s.duration * 1000.0) as i64);
            end > from && s.start < to
        })
        .collect()
}

/// The process-wide client the segment uploads already use.
///
/// This module built a fresh `reqwest::Client` per request. That is not just
/// pool churn: `ClientBuilder::build()` constructs the TLS config eagerly, and
/// on Linux the platform verifier walks and parses the whole system trust store
/// doing it — synchronously, on the calling thread, whether the origin is
/// `http` or `https`. A five-second poll re-read `/etc/ssl/certs` every five
/// seconds for the life of every DVR session, and a segment-fallback cut did it
/// once per covering segment, ~31 times serially for a 60 s clip, each with a
/// fresh TCP and TLS handshake to a relay that may be across the internet. The
/// shared client pays it once per process and keeps its connections.
fn client() -> &'static reqwest::Client {
    super::upload::client()
}

/// Most bytes `http_get` will take from the origin.
///
/// A playlist of `MAX_PLAYLIST_SEGMENTS` rows is a few hundred KiB and a
/// segment is a couple of MiB; this is generous for both. Without a ceiling the
/// only bound was the request timeout, so a wrong, compromised or MITM'd origin
/// — `ingest_url` is validated for scheme and length and nothing else, and
/// plain `http://` is permitted — could answer with a multi-gigabyte body that
/// `resp.bytes()` would allocate in full on a node whose whole design point is
/// not to disturb the media path. `upgrade::download` has capped its equivalent
/// fetch from the beginning, for the same reason.
const MAX_GET_BYTES: usize = 64 * 1024 * 1024;

/// Fetch a stream's current media playlist from the origin.
///
/// Exposed for the output-start path, which reads back the previous run's
/// manifest so a restart does not republish an empty window. Same credential
/// and same helper the clip poller uses.
pub(super) async fn fetch_manifest(base: &str, auth: Option<&str>) -> Result<Vec<u8>> {
    http_get(&format!("{base}/manifest.m3u8"), auth).await
}

async fn http_get(url: &str, auth: Option<&str>) -> Result<Vec<u8>> {
    let mut req = client().get(url).timeout(Duration::from_secs(60));
    if let Some(t) = auth {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let mut resp = req.send().await?;
    if !resp.status().is_success() {
        bail!("GET {url} returned HTTP {}", resp.status().as_u16());
    }
    // Refuse on the declared length where there is one, and again as the body
    // arrives where there is not — a chunked response declares nothing.
    if let Some(len) = resp.content_length()
        && len > MAX_GET_BYTES as u64
    {
        bail!("GET {url} declared {len} bytes, over the {MAX_GET_BYTES} cap");
    }
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() + chunk.len() > MAX_GET_BYTES {
            bail!("GET {url} body exceeds the {MAX_GET_BYTES} byte cap mid-stream");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Cut exactly, from the local replay recording.
///
/// Returns `Ok(None)` when there is nothing to cut from — no recording for
/// this flow, or one made before the wall-clock anchor existed — so the caller
/// can fall back to whole segments rather than fail.
///
/// The mapping is the recording's own line through the anchor written on its
/// first indexed frame and the rolling sample the writer re-takes every minute
/// — the measured rate when the span is long enough to trust, the nominal
/// 90 kHz otherwise; see `replay::clock`. The one-point nominal form was
/// measured on the rig at -36 ms against the CMAF published dates, inside one
/// frame at 25 fps, where `created_at_unix` was out by anywhere from half a
/// second to nineteen — and drifted by the source's clock offset times the age
/// of the recording, which the second point removes.
#[cfg(feature = "replay")]
async fn cut_exact(
    flow_id: &str,
    flow_stats: &crate::stats::collector::FlowStatsAccumulator,
    rec: &ClipRecord,
) -> Result<Option<Vec<u8>>> {
    // The recording is filed under the recorder's `storage_id`, which is the
    // flow id only by default. An operator may name one — the flow modal
    // offers it, and the manager keeps theirs when it arms a DVR session on
    // top — and looking under the flow id then found no recording at all, so
    // every clip fell back to whole segments while the session read healthy.
    // The writer publishes the id it is really using on the flow's stats.
    let recording_id = flow_stats
        .recording_id
        .get()
        .map(String::as_str)
        .unwrap_or(flow_id);
    let dir = crate::replay::recording_dir(recording_id);
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

    // The second point of the mapping, when the recording has run long enough
    // to have taken one. Absent on a young recording and on one written before
    // this pair existed; `pts_for_wall` then falls back to the nominal rate.
    let recent = meta
        .get("recent_wall_us")
        .and_then(|v| v.as_i64())
        .zip(meta.get("recent_pts_90khz").and_then(|v| v.as_u64()));

    let at: DateTime<Utc> = DateTime::parse_from_rfc3339(&rec.at)?.with_timezone(&Utc);
    let (Some(from), Some(to)) = (
        pts_for_wall(
            anchor_wall_us,
            anchor_pts,
            recent,
            (at - chrono::Duration::seconds(rec.pre_secs as i64)).timestamp_micros(),
        ),
        pts_for_wall(
            anchor_wall_us,
            anchor_pts,
            recent,
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
    //
    // Resolved BEFORE the restart guard below, so the guard is asked about the
    // range that will actually be exported. Asked about `to` instead, it could
    // not see a join sitting between `to` and `to_covering` — media the widened
    // range does include — and that is precisely the entry a mark landing on a
    // random-access point produces.
    let to_covering = index.first_after(to).unwrap_or(to);

    if index.spans_discontinuity(from, to_covering) {
        bail!(
            "clip '{}' spans a recorder restart, and the media either side of it \
             is two separate timelines — move the mark clear of the restart and \
             export it again",
            rec.name
        );
    }

    // Asked for whole, not chunked.
    //
    // The chunked entry point exists because the manager's WS transport carries
    // a download in base64 frames. This caller uploads the file in one PUT, and
    // going through the chunked API cost it two live copies of every clip —
    // the cache pinning the finished build for five minutes while this loop
    // copied the same bytes out slice by slice into a second full-size buffer.
    let out = match crate::replay::export_mp4::export_recording_mp4_whole(
        recording_id,
        Some(from),
        Some(to_covering),
    )
    .await
    {
        Ok(b) => b,
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
                "clip exporter: the recording could not serve this moment; \
                 cutting from whole segments instead"
            );
            return Ok(None);
        }
    };
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
async fn cut_exact(
    _flow_id: &str,
    _flow_stats: &crate::stats::collector::FlowStatsAccumulator,
    _rec: &ClipRecord,
) -> Result<Option<Vec<u8>>> {
    Ok(None)
}

/// How long a clip upload is given: a minute, plus the body at 2 Mbit/s.
///
/// The shared client's 30 s covers the whole exchange, body included, and was
/// sized for a two-second segment. A 30 s clip is ~93 MB all-intra and needs
/// a 25 Mbit/s uplink to land inside it; the 60 s maximum needs 50. Below
/// that — a cellular or Starlink link, the field case — every clip timed out,
/// was re-encoded and re-sent twice more, then called failed. The floor rate
/// is the slowest link worth waiting for; above the cap the link is the
/// problem and the retry will find out.
fn clip_upload_budget(bytes: usize) -> Duration {
    const FLOOR_BYTES_PER_SEC: u64 = 2_000_000 / 8;
    const CAP: Duration = Duration::from_secs(15 * 60);
    let body = Duration::from_secs(bytes as u64 / FLOOR_BYTES_PER_SEC);
    (Duration::from_secs(60) + body).min(CAP)
}

/// Assemble and upload one clip.
async fn cut_one(
    base: &str,
    auth: Option<&str>,
    flow_id: &str,
    flow_stats: &crate::stats::collector::FlowStatsAccumulator,
    rec: &ClipRecord,
) -> Result<usize> {
    // Exact if the recorder is running for this flow; whole segments if not.
    // The fallback is not a lesser mode to be ashamed of — it needs no second
    // copy of the media on the edge — but it lands on segment boundaries, so
    // prefer the cut that lands on the frame.
    if let Some(bytes) = cut_exact(flow_id, flow_stats, rec).await? {
        let target = format!("{base}/clips/{}.mp4", urlencoding_light(&rec.name));
        let n = bytes.len();
        let budget = clip_upload_budget(n);
        match http_put_within(&target, bytes, "video/mp4", auth, Some(budget)).await {
            Ok(()) => return Ok(n),
            // The exact cut was too big for the origin to accept — try the
            // coarse one before calling the clip impossible.
            //
            // The relay's 256 MiB body limit was sized against the *source*
            // bytes a clip is cut from; the all-intra re-encode is a quality
            // decision made after that, so a near-maximum window on a
            // high-bitrate feed can produce a file the origin refuses. The
            // segment path assembles from the origin's own rendition and is
            // exactly what that number was sized for, so it very likely
            // succeeds — but `?` propagated the 413 straight out, `is_permanent`
            // matched it, and the operator was told the clip could not be
            // produced after minutes of CPU and ~800 MB of uplink.
            // "returned HTTP 413", never a bare "413": the message carries
            // the clip's URL, so a clip named "Lap 413" would otherwise send
            // every failed upload down this branch — `is_permanent` learned
            // the same lesson.
            Err(e) if format!("{e:#}").contains("returned HTTP 413") => {
                tracing::warn!(
                    flow_id, clip = %rec.name, bytes = n, error = %format!("{e:#}"),
                    "clip exporter: the origin refused the exact cut as too large; \
                     falling back to whole segments"
                );
            }
            Err(e) => return Err(e),
        }
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
    let budget = clip_upload_budget(bytes);
    http_put_within(&target, body, "video/mp4", auth, Some(budget)).await?;
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
    flow_stats: std::sync::Arc<crate::stats::collector::FlowStatsAccumulator>,
    event_sender: crate::manager::events::EventSender,
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
    // Consecutive answers that say "this endpoint is not a clip queue".
    let mut unsupported = 0u32;
    tracing::info!(origin = %base, "clip exporter: watching for clip requests");

    loop {
        let interval = if unsupported >= UNSUPPORTED_GIVE_UP {
            UNSUPPORTED_POLL_INTERVAL
        } else {
            POLL_INTERVAL
        };
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(origin = %base, "clip exporter stopping (cancelled)");
                return;
            }
            _ = tokio::time::sleep(interval) => {}
        }

        let listing = match http_get(&format!("{base}/clips"), auth).await {
            Ok(b) => {
                quiet = false;
                b
            }
            Err(e) => {
                // An origin that does not serve clips at all, and one that is
                // temporarily unwell, are different problems.
                //
                // The poller is spawned for every passthrough CMAF output, and
                // `ingest_url` is any operator-supplied http(s) URL — a
                // third-party packager, an S3-compatible store, a relay on an
                // older release. None of those will ever answer this, and
                // asking them 17 280 times a day is somebody else's traffic.
                // The answer is not always 404 either: a relay that predates
                // clip export routes the path to its object handler, whose
                // name validator wants a dot, and replies 400; S3 without
                // ListBucket replies 403.
                //
                // So a run of those backs off to a slow poll and says so once
                // — in the log and on the Events page, because a DVR session
                // pointed at such an origin will never cut a clip and nothing
                // else would say why — while anything else stays a transient
                // fault and keeps polling. Swallowing the lot was the original
                // sin here — it hid a 403 on every single poll while the log
                // looked perfectly healthy.
                let msg = format!("{e:#}");
                if NOT_A_CLIP_ORIGIN
                    .iter()
                    .any(|code| msg.contains(&format!("HTTP {code}")))
                {
                    unsupported = unsupported.saturating_add(1);
                    if unsupported == UNSUPPORTED_GIVE_UP {
                        tracing::warn!(
                            origin = %base, error = %msg,
                            "clip exporter: this origin does not serve clips; asking every \
                             five minutes from now on"
                        );
                        event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::CMAF,
                            format!(
                                "Clip export unsupported on flow '{flow_id}': the origin \
                                 does not serve a clip queue, so no marked clip will be cut"
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "clip_export_unsupported",
                                "origin": base,
                                "error": msg,
                                "consecutive": unsupported,
                            }),
                        );
                    }
                    continue;
                }
                // Anything else breaks the run: the counter is of consecutive
                // not-a-clip-origin answers, and a relay that flaps between
                // 404 and 503 while it restarts is not one that has settled.
                unsupported = 0;
                if !quiet {
                    tracing::warn!(
                        origin = %base, error = %msg,
                        "clip exporter: cannot read the clip list; nothing will be cut \
                         until this clears"
                    );
                    // And on the Events page, not only in the log.
                    //
                    // An exporter locked out of its own queue can never mark
                    // anything failed either — only the edge writes that
                    // terminal state — so the viewer's page reads "being cut"
                    // indefinitely for clips that are never coming, the
                    // manager's session row reads healthy, and the only way to
                    // find it is to SSH to the node.
                    event_sender.emit_flow_with_details(
                        EventSeverity::Warning,
                        category::CMAF,
                        format!(
                            "Clip export blocked on flow '{flow_id}': the origin's clip \
                             queue cannot be read, so no marked clip will be cut"
                        ),
                        &flow_id,
                        serde_json::json!({
                            "error_code": "clip_export_blocked",
                            "origin": base,
                            "error": msg,
                        }),
                    );
                    quiet = true;
                }
                continue;
            }
        };
        unsupported = 0;
        let records: Vec<ClipRecord> = match serde_json::from_slice(&listing) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(origin = %base, error = %e, "clip exporter: unreadable clip listing");
                continue;
            }
        };

        for rec in records.iter().filter(|r| !r.ready && !r.failed) {
            // Cancellation is checked inside the batch, not only around the
            // sleep. A batch can be up to the relay's 100 pending clips, each a
            // multi-second all-intra encode, and the poller is spawned
            // detached — so a flow stop or a config push that restarts the
            // output left the old exporter grinding through the whole queue
            // while a new one, spawned for the replacement output, polled the
            // same queue. Two pollers then cut the same record, and their
            // uploads share one `.part` file on the origin.
            if cancel.is_cancelled() {
                tracing::info!(origin = %base, "clip exporter stopping (cancelled) mid-batch");
                return;
            }
            let outcome = cut_one(&base, auth, &flow_id, &flow_stats, rec).await;
            // Whichever way it went, the spike is over — give the pages back
            // before moving to the next one, so a run of exports does not
            // accumulate a working set none of them still needs.
            release_free_heap().await;
            match outcome {
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
                        // The viewer is told, via the relay's record. The
                        // operator is told here — the manager has no clip
                        // surface at all, so without this a run of failed
                        // exports is invisible to the person running the event.
                        event_sender.emit_flow_with_details(
                            EventSeverity::Warning,
                            category::CMAF,
                            format!(
                                "Clip '{}' on flow '{flow_id}' could not be cut: {msg}",
                                rec.name
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "clip_export_failed",
                                "clip": rec.name,
                                "attempts": *n,
                                "error": msg,
                            }),
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

    /// A segment number is not a status code, and neither is a clip name.
    ///
    /// The formatted chain embeds the segment URI and the clip name, and
    /// segments are `seg-{seq:05}.m4s` — about three per cent of five-digit
    /// sequence numbers contain the literal 413, so a 30-second window had
    /// roughly a one-in-three chance of holding one. Matching a bare "413"
    /// turned a transient fetch failure on such a segment into a settled one,
    /// and the viewer was told the clip could not be produced when a retry
    /// would have delivered it.
    #[test]
    fn a_sequence_number_that_looks_like_a_status_code_is_still_retried() {
        for e in [
            "GET https://origin/seg-00413.m4s returned HTTP 503",
            "GET https://origin/seg-41300.m4s returned HTTP 500",
            "PUT https://relay/clips/Lap%20413.mp4 returned HTTP 502 — bad gateway",
        ] {
            assert!(!is_permanent(e), "{e} must still be retried");
        }
        // The real one still is permanent.
        assert!(is_permanent(
            "PUT https://relay/clips/Lap%20413.mp4 returned HTTP 413 — clip too large"
        ));
    }

    /// The upload budget follows the body, and never the segment client's 30 s.
    ///
    /// A 30 s clip is ~93 MB all-intra. Under the shared client's deadline it
    /// needed a 25 Mbit/s uplink to land; on anything slower it timed out,
    /// was re-encoded and re-sent twice more, and was then called failed.
    #[test]
    fn the_upload_budget_scales_with_the_clip() {
        assert_eq!(clip_upload_budget(0), Duration::from_secs(60));
        // 93 MB at 2 Mbit/s is 372 s, plus the minute.
        assert_eq!(clip_upload_budget(93_000_000), Duration::from_secs(60 + 372));
        // And it is capped: a 256 MiB clip at the floor rate would be over
        // eighteen minutes, which is a link problem the retry should find.
        assert_eq!(clip_upload_budget(256 << 20), Duration::from_secs(15 * 60));
    }

    /// The exact-cut fallback keys on the status the origin returned, not on
    /// a bare "413" — which the clip's own URL, and so a clip called
    /// "Lap 413", would match on every failed upload.
    #[test]
    fn the_too_large_fallback_reads_the_status_not_the_url() {
        let put_error = |url: &str, code: u16| format!("PUT {url} returned HTTP {code} — body");
        let lap = "https://relay/origin/feed/clips/Lap%20413.mp4";
        assert!(!put_error(lap, 502).contains("returned HTTP 413"));
        assert!(put_error(lap, 502).contains("413"), "the bare match would have fired");
        assert!(put_error(lap, 413).contains("returned HTTP 413"));
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
