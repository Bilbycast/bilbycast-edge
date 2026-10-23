// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The CMAF output end to end, against an origin of its own whose PUTs can
//! stall: what the intake and arrival dating promise, held by the real loop
//! rather than by its pieces.
//!
//! The pieces have tests of their own — the intake's queue and its wait
//! (`intake::tests`), `CmafState::arrival_now` and the flow clock
//! (`date_tests::a_backlog_in_the_intake_is_dated_by_arrival`) — and the last
//! of those says what it cannot hold: the `closed_at = state.arrival_now()`
//! lines in `push_video_sample` and `handle_ll_cmaf`, reached only through a
//! segmenter and an origin. These drive `spawn_cmaf_output` from a flow's
//! broadcast to an HTTP origin, so either line edited back to `Utc::now()` —
//! or the low-latency open row's, which then publishes the backlog's open
//! rows with no date at all — or the intake taken out of the path, fails
//! here.
//!
//! What they do not cover. The feed is synthetic H.264 — real parameter sets,
//! filler slices — because a default build has no H.264 encoder and the
//! passthrough path never decodes a picture; so no re-encode, and no audio, so
//! the fragments carry one track and the init waits out `AUDIO_DETECT_GRACE`.
//! No DASH, no encryption, one rendition, one stall. And they take real time,
//! about 25 s each, because the dating under test is wall-clock dating.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use chrono::{DateTime, Utc};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::config::models::CmafOutputConfig;
use crate::engine::packet::RtpPacket;
use crate::engine::rtmp::ts_mux::TsMuxer;
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};

/// 25 fps, a one-second GOP, a one-second segment: segment `n` is GOP `n`.
const FRAME: Duration = Duration::from_millis(40);
const GOP: u64 = 25;
/// Long enough that a segment dated as of its processing rather than its
/// arrival is out by more than `EPOCH_REANCHOR_SECS` (10 s).
const STALL: Duration = Duration::from_secs(12);
/// The segment whose PUT stalls — late enough that the init and the flow's
/// clock are settled before it.
const STALLED_SEGMENT: u64 = 4;
/// Twenty-one seconds of feed: the stall ends about 17 s in, and the rest is
/// dated live.
const FRAMES: u64 = 21 * GOP;
/// The flow's broadcast ring, which a stall of `STALL` overruns many times.
const RING: usize = 64;

/// What the origin saw: every PUT, and when the one that stalled held the
/// output up.
struct Origin {
    stall_path: String,
    puts: Mutex<Vec<(String, Bytes)>>,
    stalled: Mutex<Option<(Instant, Instant)>>,
}

async fn serve(State(origin): State<Arc<Origin>>, method: Method, uri: Uri, body: Bytes) -> StatusCode {
    // No window to restore and no clip queue: a fresh stream on an origin
    // that does not cut clips.
    if method != Method::PUT {
        return StatusCode::NOT_FOUND;
    }
    let path = uri.path().to_string();
    // Answered only once the body is in, so the output waits the whole stall
    // for this PUT's response — the low-latency close's `finish()` included.
    if path == origin.stall_path && origin.stalled.lock().unwrap().is_none() {
        let began = Instant::now();
        tokio::time::sleep(STALL).await;
        *origin.stalled.lock().unwrap() = Some((began, Instant::now()));
    }
    origin.puts.lock().unwrap().push((path, body));
    StatusCode::OK
}

/// One access unit in Annex B: SPS, PPS and an IDR slice on a GOP's first
/// frame, a non-IDR slice otherwise. The slices are filler — every byte odd,
/// so none can read as a start code — which passthrough carries untouched.
fn access_unit(frame: u64) -> Vec<u8> {
    let mut au = Vec::new();
    let mut nal = |bytes: &[u8]| {
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(bytes);
    };
    let filler = |head: &[u8], len: usize| -> Vec<u8> {
        head.iter().copied().chain((0..len).map(|i| ((frame as usize * 7 + i) as u8) | 1)).collect()
    };
    if frame.is_multiple_of(GOP) {
        nal(&[0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x01, 0x41, 0xfb, 0x01, 0x10]);
        nal(&[0x68, 0xce, 0x06, 0xe2]);
        nal(&filler(&[0x65, 0x88], 1_500));
    } else {
        nal(&filler(&[0x41, 0x9a], 300));
    }
    au
}

/// What one run left to check.
struct Run {
    /// When each GOP's first frame went onto the broadcast.
    gop_sent: Vec<DateTime<Utc>>,
    /// When every frame did.
    frame_sent: Vec<Instant>,
    puts: Vec<(String, Bytes)>,
    stalled: Option<(Instant, Instant)>,
    packets_dropped: u64,
    reanchored: bool,
}

/// Publish `FRAMES` frames in real time into a CMAF output whose origin stalls
/// one segment's PUT for `STALL`.
async fn run_against_a_stalling_origin(flow_id: &str, low_latency: bool) -> Run {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = Arc::new(Origin {
        stall_path: format!("/live/seg-{STALLED_SEGMENT:05}.m4s"),
        puts: Mutex::new(Vec::new()),
        stalled: Mutex::new(None),
    });
    let app = axum::Router::new().fallback(serve).with_state(origin.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let config: CmafOutputConfig = serde_json::from_value(serde_json::json!({
        "id": "o",
        "name": "o",
        "ingest_url": format!("http://{addr}/live"),
        "segment_duration_secs": 1.0,
        "max_segments": 60,
        "manifests": ["hls"],
        "low_latency": low_latency,
        "chunk_duration_ms": 250,
    }))
    .unwrap();
    let (tx, _) = broadcast::channel::<RtpPacket>(RING);
    let stats = Arc::new(OutputStatsAccumulator::new("o".into(), "o".into(), "cmaf".into()));
    let flow_stats = Arc::new(FlowStatsAccumulator::new(flow_id.into(), flow_id.into(), "test".into()));
    let (events, _events_rx) = crate::manager::events::event_channel();
    let cancel = CancellationToken::new();
    let output = super::spawn_cmaf_output(
        config,
        &tx,
        stats.clone(),
        cancel.clone(),
        events,
        flow_id.to_string(),
        flow_stats,
    );

    // The flow: one message per frame, on the frame's own deadline.
    let mut muxer = TsMuxer::new();
    let mut gop_sent = Vec::new();
    let mut frame_sent = Vec::new();
    let start = tokio::time::Instant::now();
    for frame in 0..FRAMES {
        tokio::time::sleep_until(start + FRAME * frame as u32).await;
        let pts = 900_000 + frame * 3_600;
        let packets = muxer.mux_video(&access_unit(frame), pts, pts, frame.is_multiple_of(GOP));
        let data: Vec<u8> = packets.iter().flat_map(|p| p.iter().copied()).collect();
        if frame.is_multiple_of(GOP) {
            gop_sent.push(Utc::now());
        }
        frame_sent.push(Instant::now());
        tx.send(RtpPacket {
            data: data.into(),
            sequence_number: frame as u16,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: None,
            upstream_leg_id: None,
            sender_timestamp_us: None,
        })
        .unwrap();
    }
    // The last whole segments through the origin, then stop.
    tokio::time::sleep(Duration::from_secs(2)).await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), output).await.unwrap().unwrap();
    server.abort();

    let reanchored = super::lock_flow_clocks()
        .get(flow_id)
        .is_some_and(|clock| clock.last_reanchor.is_some());
    let puts = std::mem::take(&mut *origin.puts.lock().unwrap());
    let stalled = *origin.stalled.lock().unwrap();
    Run {
        gop_sent,
        frame_sent,
        puts,
        stalled,
        packets_dropped: stats.packets_dropped.load(std::sync::atomic::Ordering::Relaxed),
        reanchored,
    }
}

/// `seg-NNNNN.m4s` → `NNNNN`.
fn segment_seq(uri: &str) -> Option<u64> {
    uri.rsplit('/').next()?.strip_prefix("seg-")?.strip_suffix(".m4s")?.parse().ok()
}

/// What the published playlists said about each segment.
#[derive(Default)]
struct Published {
    /// Each dated row: its segment and its date.
    dates: Vec<(u64, DateTime<Utc>)>,
    /// The segments of rows published with no date.
    undated: Vec<u64>,
    /// Whether any playlist declared a discontinuity.
    discontinuity: bool,
}

fn published(run: &Run) -> Published {
    let mut out = Published::default();
    for (path, body) in &run.puts {
        if !path.ends_with("/manifest.m3u8") {
            continue;
        }
        let mut pdt = None;
        for line in std::str::from_utf8(body).unwrap().lines() {
            if line == "#EXT-X-DISCONTINUITY" {
                out.discontinuity = true;
            } else if let Some(at) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
                pdt = Some(DateTime::parse_from_rfc3339(at).unwrap().with_timezone(&Utc));
            } else if !line.starts_with('#')
                && let Some(seq) = segment_seq(line)
            {
                match pdt.take() {
                    Some(at) => out.dates.push((seq, at)),
                    None => out.undated.push(seq),
                }
            }
        }
    }
    out
}

/// What a stalling origin must not do to the output, whichever path closes
/// its segments.
fn assert_rode_out_the_stall(run: &Run) {
    let (began, ended) = run.stalled.expect("the origin never stalled a PUT");
    assert!(ended - began >= STALL);
    let sent_meanwhile = run.frame_sent.iter().filter(|&&t| (began..ended).contains(&t)).count();
    assert!(
        sent_meanwhile > RING,
        "only {sent_meanwhile} messages went out during the stall: the ring alone would have held them"
    );

    // Nothing lost, though the broadcast ring was overrun many times over.
    assert_eq!(run.packets_dropped, 0, "the stall cost packets");
    let mut segments: Vec<u64> = run
        .puts
        .iter()
        .filter_map(|(path, _)| segment_seq(path))
        .collect();
    segments.sort_unstable();
    segments.dedup();
    // Every segment the feed closed — the last GOP's stays open, and a
    // low-latency output may have begun its PUT.
    let closed = FRAMES / GOP - 1;
    assert_eq!(segments[..closed as usize], (0..closed).collect::<Vec<_>>(), "a segment went missing");
    assert!(segments.len() as u64 <= closed + 1, "segments past the feed: {segments:?}");

    // Every date on the arrival timeline — the backlog's segments, closed in
    // a burst twelve seconds late, included — and the flow's epoch never
    // re-anchored.
    let Published { dates, undated, discontinuity } = published(run);
    assert!(!discontinuity, "a playlist declared a discontinuity over a slow origin");
    assert!(!run.reanchored, "the flow's epoch re-anchored over a slow origin");
    // Only a low-latency output's first open row may go bare: nothing has
    // closed, so the flow has no clock to date it by.
    assert!(
        undated.iter().all(|&seq| seq == 0),
        "rows published with no date: {undated:?}"
    );
    let dated: std::collections::BTreeSet<u64> = dates.iter().map(|(seq, _)| *seq).collect();
    assert!(
        dated.range(STALLED_SEGMENT + 1..).count() >= 10,
        "too few segments after the stall were dated to check: {dated:?}"
    );
    // Within half a second or so of when each segment's first frame was
    // published, -500 to +750 ms — the closing frame's 40 ms in the demuxer,
    // and whatever a loaded test run adds — where dating by processing put the
    // backlog's eleven seconds out. A bound on the stall's effect, not a
    // measure of how accurate the dates are.
    for (seq, pdt) in dates {
        let off = (pdt - run.gop_sent[seq as usize]).num_milliseconds();
        assert!(
            (-500..750).contains(&off),
            "segment {seq} dated {off} ms from when its first frame was published"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_origin_costs_latency_not_packets_or_dates() {
    let run = run_against_a_stalling_origin("cmaf-arrival-e2e", false).await;
    assert_rode_out_the_stall(&run);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_origin_costs_a_low_latency_output_latency_not_packets_or_dates() {
    let run = run_against_a_stalling_origin("cmaf-arrival-e2e-ll", true).await;
    assert_rode_out_the_stall(&run);
}
