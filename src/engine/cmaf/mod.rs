// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! CMAF / CMAF-LL HTTP-push output.
//!
//! Subscribes to the flow's broadcast channel, demuxes the MPEG-TS,
//! segments the source H.264/HEVC video + AAC audio into fragmented-MP4
//! (CMAF per ISO/IEC 23000-19), and uploads init + media segments plus
//! HLS m3u8 and/or DASH .mpd manifests to the operator-supplied
//! `ingest_url`.
//!
//! # Threading model
//!
//! - Broadcast subscriber loop: receives RTP packets, demuxes TS,
//!   buffers samples per track. **Never blocks** — codec work is
//!   delegated to spawn_blocking workers.
//! - Per-segment HTTP PUT: awaits without blocking the subscriber
//!   because the next packet is only fetched after the await point.
//!   For sustained throughput we offload uploads via tokio's I/O
//!   reactor (reqwest already does this internally).
//! - Codec workers (Phase 3 audio_encode / video_encode): wrap each
//!   encode call in `tokio::task::block_in_place` only when called
//!   from the subscriber loop.

mod box_writer;
mod cenc;
mod cenc_boxes;
mod clips;
pub(crate) mod codecs;
pub(crate) mod encode;
#[allow(dead_code)]
pub(crate) mod fmp4;
mod manifest;
#[allow(dead_code)]
pub(crate) mod nalu;
mod segmenter;
// The thumbnail track reuses the replay filmstrip's capture and JPEG
// encoding rather than duplicating them, so it exists only when `replay` does.
#[cfg(feature = "replay")]
mod thumbnails;
mod upload;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::CmafOutputConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{EgressMediaSummaryStatic, OutputStatsAccumulator};

use super::packet::RtpPacket;
use super::ts_demux::{DemuxedFrame, TsDemuxer};
use super::ts_program_filter::TsProgramFilter;

use codecs::aac_audio_specific_config;
use encode::{AudioReencoder, VideoReencoder};
use fmp4::{AudioTrack, Sample, VideoCodec, VideoTrack};
use manifest::{
    DashAudioRep, DashInput, DashVideoRep, HlsPartEntry, LowLatencyHints, M3u8Entry,
    build_dash_mpd, build_hls_playlist, default_segment_uri, init_generation_of,
    init_object_name, required_target_duration,
};
use segmenter::{
    AudioSegmenter, CompletedSegment, PushOutcome, SegmentKind, VideoSegmenter,
};
use upload::{ChunkedPutHandle, chunked_put, http_put};

/// Minimum RTP header size (no CSRC / no extensions).
const RTP_HEADER_MIN: usize = 12;

/// Spawn a CMAF output task.
pub fn spawn_cmaf_output(
    config: CmafOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    flow_stats: Arc<crate::stats::collector::FlowStatsAccumulator>,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();

    let mut egress_static = EgressMediaSummaryStatic {
        transport_mode: Some("cmaf".to_string()),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none(),
        audio_only: false,
        ..Default::default()
    };
    if let Some(ve) = config.video_encode.as_ref() {
        egress_static = egress_static.with_video_encode_target(ve);
    }
    if let Some(ae) = config.audio_encode.as_ref() {
        egress_static = egress_static.with_audio_encode_target(ae);
    }
    output_stats.set_egress_static(egress_static);

    // The thumbnail track is a sibling subscriber on the same broadcast, not a
    // stage in the media path: it must never be able to stall or fail the
    // output it sits beside.
    #[cfg(feature = "replay")]
    if let Some(th) = config.thumbnails.clone() {
        let interval = std::time::Duration::from_secs(th.interval_secs as u64);
        // The index may only describe sheets the origin still holds, so it is
        // bounded by the same window the playlist is.
        let window_secs = config
            .dvr_window_secs
            .unwrap_or(config.max_segments as f64 * config.segment_duration_secs);
        let window = std::time::Duration::from_secs_f64(window_secs.max(1.0));
        thumbnails::spawn_thumbnail_track(
            config.id.clone(),
            config.ingest_url.clone(),
            config.auth_token.clone(),
            broadcast_tx,
            crate::replay::filmstrip::CaptureSpec {
                width: th.width,
                height: th.height,
                ..Default::default()
            },
            interval,
            th.frames_per_sheet,
            window,
            Arc::new(thumbnails::ThumbnailStats::default()),
            event_sender.clone(),
            flow_id.clone(),
            cancel.clone(),
        );
    }
    // Say so rather than publish nothing. The config validates and the output
    // runs either way, so a build without `replay` would otherwise present as
    // a scrub bar that simply never shows a picture — indistinguishable from a
    // source the capture could not decode.
    #[cfg(not(feature = "replay"))]
    if config.thumbnails.is_some() {
        event_sender.emit_flow(
            EventSeverity::Warning,
            category::CONFIG,
            format!(
                "CMAF output '{}': `thumbnails` is configured but this build has no \
                 `replay` feature, which owns the frame capture it uses — no scrub \
                 preview will be published. Rebuild with `--features replay`.",
                config.id,
            ),
            &flow_id,
        );
    }

    tokio::spawn(async move {
        if let Err(e) = run(
            &config,
            &mut rx,
            output_stats,
            cancel,
            &event_sender,
            &flow_id,
            flow_stats,
        )
        .await
        {
            tracing::error!("CMAF output '{}' exited with error: {e}", config.id);
            event_sender.emit_flow(
                EventSeverity::Critical,
                category::CMAF,
                format!("CMAF output '{}' error: {e}", config.id),
                &flow_id,
            );
        }
    })
}

fn init_cenc_runtime(
    cfg: &crate::config::models::CencConfig,
) -> anyhow::Result<CencRuntime> {
    let scheme = cenc::Scheme::parse(&cfg.scheme)
        .ok_or_else(|| anyhow::anyhow!("unknown CENC scheme '{}'", cfg.scheme))?;
    let key = decode_hex16(&cfg.key, "key")?;
    let key_id = decode_hex16(&cfg.key_id, "key_id")?;
    let mut extra_pssh = Vec::with_capacity(cfg.pssh_boxes.len());
    for hex_pssh in &cfg.pssh_boxes {
        let bytes = hex_decode(hex_pssh)
            .map_err(|e| anyhow::anyhow!("invalid pssh hex: {e}"))?;
        if bytes.len() < 8 || &bytes[4..8] != b"pssh" {
            anyhow::bail!("pssh entry is not a valid pssh box");
        }
        extra_pssh.push(bytes);
    }
    Ok(CencRuntime {
        encryptor: cenc::CencEncryptor::new(scheme, key),
        scheme,
        key_id,
        extra_pssh,
    })
}

fn decode_hex16(hex_str: &str, label: &str) -> anyhow::Result<[u8; 16]> {
    let bytes = hex_decode(hex_str).map_err(|e| anyhow::anyhow!("{label} hex: {e}"))?;
    if bytes.len() != 16 {
        anyhow::bail!("{label} must decode to 16 bytes, got {}", bytes.len());
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err("odd hex length".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex byte 0x{c:02x}")),
    }
}

/// Wall clock corresponding to source PTS 0, per flow.
///
/// Keyed on the **flow**, not the output, and that is the point: two
/// renditions of one source are two CMAF outputs of one flow, they see the
/// same RTP packets, and `PtsUnwrap` does not rebase — so `base_dts_90k` is
/// the same number in both. Given one shared epoch they therefore publish the
/// *same* date for the same content, which is what lets a player put a
/// full-resolution still over a low-resolution picture and land on the frame
/// it replaced.
///
/// "The same number in both" holds only while both outputs have counted the
/// same 33-bit PTS wraps, and `PtsUnwrap` is per output. A rendition added —
/// or restarted under `UpdateFlow` — after a wrap counts none and reports the
/// same content 2^33 ticks lower, so [`FlowClock::align`] puts an incoming
/// position back on the flow's own lap before anything else looks at it.
static FLOW_EPOCHS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, FlowClock>>> =
    std::sync::OnceLock::new();

/// The flow-clock map, created on first use.
fn flow_clocks() -> &'static std::sync::Mutex<HashMap<String, FlowClock>> {
    FLOW_EPOCHS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Take the flow-clock lock, recovering from poisoning rather than unwrapping.
///
/// A panic somewhere else must not take an output down over a timestamp: what
/// sits behind this lock is a clock estimate, and the worst a recovered entry
/// can hold is one stale epoch, which the re-anchor test corrects.
fn lock_flow_clocks() -> std::sync::MutexGuard<'static, HashMap<String, FlowClock>> {
    match flow_clocks().lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// `d` seconds as a `chrono::Duration`.
fn secs(d: f64) -> chrono::Duration {
    chrono::Duration::nanoseconds((d * 1e9) as i64)
}

/// A flow's shared clock: the epoch, plus the epoch that was in force for each
/// of the last few segments.
///
/// The history is what keeps two renditions in exact agreement while the epoch
/// slews. Both date the same segment, but not at the same instant — one
/// encoder finishes before the other — so whichever arrives second would
/// otherwise see an epoch that had already moved on, and publish a date a slew
/// step away from its sibling. Remembering the epoch *per segment* makes the
/// second caller reproduce the first one's answer exactly.
///
/// A handful of entries is enough: the renditions run within a segment or two
/// of each other, and anything further behind has bigger problems than a
/// five-millisecond disagreement.
struct FlowClock {
    epoch: chrono::DateTime<chrono::Utc>,
    /// Low-passed estimate of the wall clock the samples imply, which is what
    /// the epoch is steered towards. See [`EPOCH_FILTER_GAIN`].
    filtered: chrono::DateTime<chrono::Utc>,
    /// The last media position this flow dated, after alignment. Incoming
    /// positions are brought onto the same 33-bit lap as this one — see
    /// [`FlowClock::align`].
    dts_ref: u64,
    /// `(base_dts_90k, epoch used, whether that segment re-anchored)`, oldest
    /// first.
    ///
    /// The flag is remembered alongside the epoch because a re-anchor is
    /// discovered exactly once — by whichever rendition closes the segment
    /// first — and the sibling closing that same segment is answered from this
    /// cache instead of taking a sample of its own. Left out, the sibling
    /// reproduced the date and published it bare: the identical jump, tagged
    /// on one playlist and silent on the other, so the two renditions
    /// disagreed about `#EXT-X-DISCONTINUITY-SEQUENCE` — which RFC 8216
    /// §4.3.3.3 makes a cross-rendition invariant, because a player switching
    /// renditions carries its count across.
    ///
    /// **That invariant holds only while the two renditions are less than one
    /// segment apart in wall time**, and the bound is set by
    /// [`FlowClock::reanchor`] clearing this cache. A rendition still holding a
    /// *pre*-restart segment to close when its sibling re-anchors finds nothing
    /// here, samples for itself, implies the old epoch and re-anchors back —
    /// after which the two take turns re-anchoring each other. Modelled on a
    /// 2 s-segment flow restarting at segment 20: 45 ms, 0.5 s, 1.5 s and 1.9 s
    /// of skew agree exactly; 2.5 s gives two date and two flag disagreements;
    /// 8 s gives six. Pinned by
    /// `renditions_agree_across_a_re_anchor_within_a_segment_of_skew`, and
    /// argued out in `docs/cmaf.md` — including why the clear stays (a source
    /// restarting at PTS 0 re-uses `base_dts` values this cache still holds,
    /// and the lookup below returns the *oldest* match, so keeping them would
    /// answer a post-restart segment out of the pre-restart epoch: an
    /// hour-stale date with no tag at all).
    recent: VecDeque<(u64, chrono::DateTime<chrono::Utc>, bool)>,
    /// When the timeline last re-anchored, and how many re-anchors have
    /// arrived in an unbroken run of them — see [`REANCHOR_THRASH_SECS`].
    last_reanchor: Option<chrono::DateTime<chrono::Utc>>,
    reanchor_run: u32,
}

/// How many segments' epochs to remember. Two renditions, a segment or two of
/// skew, and room to spare.
const FLOW_CLOCK_HISTORY: usize = 16;

/// One lap of the 33-bit MPEG-TS PTS clock, in 90 kHz ticks — 26 h 30 m.
const PTS_LAP_90K: u64 = 1 << 33;

/// How far the implied epoch may move before it is treated as a new timeline
/// rather than as jitter.
///
/// Scheduling noise is tens of milliseconds. A source restart, a PTS
/// discontinuity, or a flow reconfigured under the same id moves it by the
/// whole elapsed time, so there is a wide gap to sit in.
const EPOCH_REANCHOR_SECS: f64 = 10.0;

/// Two re-anchors closer together than this are not two source restarts.
const REANCHOR_THRASH_SECS: f64 = 60.0;

/// How long a run of rapid re-anchors gets before it is reported as a fault
/// rather than as news.
const REANCHOR_THRASH_RUN: u32 = 3;

/// The most the epoch may move for any one segment, tracking the source clock.
///
/// A media timeline is not a wall clock. Measured on the demo rig, the source
/// publishes 480.00 s of media in 480.22 s of real time — about 450 ppm slow —
/// so an epoch pinned once and held drifts 4.1 s across a 2h30m session, and
/// the operator's time-of-day readout drifts with it.
///
/// Holding it is wrong; re-sampling it per segment is what this replaced, and
/// put ~50 ms of publish jitter into every tag. So slew: move toward what each
/// sample implies, by at most this much. At 5 ms per 2 s segment the epoch can
/// track a source up to 2500 ppm out, while no single segment's date moves by
/// more than an eighth of a frame — far below anything the jitter used to do,
/// and monotonic rather than noisy.
///
/// Both renditions of a flow share the epoch, so they slew together and go on
/// publishing identical dates.
///
/// A source outside that 2500 ppm band cannot be tracked: the epoch falls
/// behind until the error crosses [`EPOCH_REANCHOR_SECS`] and snaps. That is
/// a real jump in the published dates, and it is why the snap is reported to
/// the playlist rather than made quietly — see
/// [`FlowClock::date_closed_segment`].
const EPOCH_SLEW_SECS: f64 = 0.005;

/// How much of each new sample the filtered estimate takes.
///
/// The slew above is a clamp on an error, and publish jitter is larger than
/// the clamp — so it bound on nearly every segment and the loop saturated on
/// noise instead of tracking the rate. It moved 5 ms toward whichever side the
/// jitter happened to fall, and only the *imbalance* corrected the drift:
/// measured, 407 ppm became 102 ppm rather than nothing.
///
/// So filter first and correct against the filtered value. At 0.02 with 2 s
/// segments the estimate has a time constant of about 200 s, which takes the
/// jitter well below the clamp and leaves the clamp free to do what it is for
/// — bounding how fast the epoch may move, not deciding how far.
const EPOCH_FILTER_GAIN: f64 = 0.02;

impl FlowClock {
    /// A clock founded on one sample. The flow has never been dated, so this
    /// segment is what places its timeline against wall clock.
    fn new(
        base_dts_90k: u64,
        seg_secs: f64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let implied = now - secs(seg_secs) - secs(base_dts_90k as f64 / 90_000.0);
        Self {
            epoch: implied,
            filtered: implied,
            dts_ref: base_dts_90k,
            recent: VecDeque::new(),
            last_reanchor: None,
            reanchor_run: 0,
        }
    }

    /// Bring a media position onto this flow's timeline, undoing a 33-bit PTS
    /// wrap that one output has counted and another has not.
    ///
    /// `PtsUnwrap` lives in the segmenter, so there is one per output *and*
    /// per track, and each starts counting from whatever it saw first. A
    /// 24/7 channel wraps every 2^33 ticks — 26 h 30 m — so a rendition added
    /// or restarted after a wrap reports the same content 2^33 ticks below its
    /// sibling. Uncorrected, the two imply epochs 26.5 hours apart: every
    /// sample from each re-anchors the other, `recent` is permanently cleared,
    /// the renditions can never agree, and the epoch collapses to
    /// `now - seg_secs` — the per-segment sampling this whole mechanism exists
    /// to replace — while the node logs a source restart twice per segment
    /// forever.
    ///
    /// Rebasing inside `PtsUnwrap` itself (returning `pts - first_pts`) would
    /// fix it at the source, and cannot be done: video and audio hold
    /// *separate* `PtsUnwrap` instances whose first samples are different
    /// frames, while `build_muxed_segment` writes both tracks'
    /// `base_media_decode_time` into one `moof` and relies on them sharing the
    /// source's absolute timeline. Rebasing each independently offsets audio
    /// from video by the gap between their first samples, permanently, on
    /// every flow. So the correction lives here, where it is a presentation
    /// detail that nothing downstream of the muxer can see.
    ///
    /// Anything within half a lap of the flow's last position is taken to be
    /// on that lap. A genuine restart moves by minutes or hours, nowhere near
    /// the 13-hour half-lap, so it survives this untouched for the re-anchor
    /// test to catch.
    fn align(&self, base_dts_90k: u64) -> u64 {
        let lap = PTS_LAP_90K as i128;
        let half = lap / 2;
        let mut delta = base_dts_90k as i128 - self.dts_ref as i128;
        // Round the gap to the nearest whole lap and take it out. Written as
        // arithmetic rather than a subtract-until-in-range loop because an
        // output restarted after a year of flow uptime is hundreds of laps
        // from the reference, and this runs on every segment.
        delta -= (delta + half).div_euclid(lap) * lap;
        // A position that would land before zero belongs to an output that
        // started less than a lap before the flow's own first segment; the
        // clamp costs that one segment its exact offset and nothing else.
        (self.dts_ref as i128 + delta).max(0) as u64
    }

    /// The date to publish for a segment that has just **closed**, steering
    /// the clock with it.
    ///
    /// Returns the date, and whether this sample re-anchored the media
    /// timeline — which the playlist has to declare with
    /// `#EXT-X-DISCONTINUITY`, because a re-anchor moves every date after it
    /// relative to every date before it.
    fn date_closed_segment(
        &mut self,
        base_dts_90k: u64,
        seg_secs: f64,
        now: chrono::DateTime<chrono::Utc>,
        flow_id: &str,
    ) -> (chrono::DateTime<chrono::Utc>, bool) {
        let base = self.align(base_dts_90k);
        self.dts_ref = base;
        let media_secs = base as f64 / 90_000.0;
        // What this sample says the epoch is: the segment closed about now, so
        // its first sample was `seg_secs` ago, and that sample sits
        // `media_secs` into the timeline.
        let implied = now - secs(seg_secs) - secs(media_secs);

        // The discontinuity test runs *before* the per-segment cache, and the
        // order is the whole point. The other way round, a source that
        // restarts while its `base_dts` is still one of the sixteen remembered
        // ones is absorbed in silence: the cache answers with the epoch from
        // an hour ago, the re-anchor branch is unreachable, `recent` is never
        // cleared and nothing is logged, so the operator gets no signal at
        // all. Sixteen entries is about 32 s at 2 s segments — precisely when
        // a flapping source restarts. The cache exists to make two renditions
        // agree about one segment; it must not outvote the detector.
        let off = (implied - self.epoch).num_milliseconds().abs() as f64 / 1000.0;
        if off > EPOCH_REANCHOR_SECS {
            self.reanchor(implied, now, flow_id);
            self.remember(base, true);
            return (self.epoch + secs(media_secs), true);
        }

        // This segment already dated by the other rendition: reproduce that
        // answer exactly rather than dating it against an epoch that has since
        // slewed — and reproduce the discontinuity it was published with too.
        //
        // A hard-coded `false` here is how a re-anchor reached only one
        // rendition of a flow. The sibling's sample lands within a few tens of
        // milliseconds of the epoch the first one just re-anchored to, so it
        // never trips the test above; it falls through to this cache and
        // published the identical hour-long jump with a clean `#EXTINF` step
        // and no tag at all.
        if let Some((_, was, disc)) = self.recent.iter().find(|(d, _, _)| *d == base) {
            return (*was + secs(media_secs), *disc);
        }

        // Filter, then steer towards the filtered value — see
        // `EPOCH_FILTER_GAIN`. Comparing against the raw sample let publish
        // jitter saturate the clamp, so the loop chased noise instead of the
        // rate.
        let raw = (implied - self.filtered).num_nanoseconds().unwrap_or(0) as f64 / 1e9;
        self.filtered +=
            chrono::Duration::nanoseconds((raw * EPOCH_FILTER_GAIN * 1e9) as i64);

        // Track the source clock rather than pinning to the first sample — see
        // `EPOCH_SLEW_SECS`. Bounded, so no one segment's date moves far.
        let err = (self.filtered - self.epoch).num_nanoseconds().unwrap_or(0) as f64 / 1e9;
        let step = err.clamp(-EPOCH_SLEW_SECS, EPOCH_SLEW_SECS);
        self.epoch += chrono::Duration::nanoseconds((step * 1e9) as i64);

        self.remember(base, false);
        (self.epoch + secs(media_secs), false)
    }

    /// The date to publish for a segment that has just **opened**, without
    /// touching the clock.
    ///
    /// The low-latency path advertises a segment while it is still being
    /// written, on every chunk emission, so it may only *read*. Dating an open
    /// segment through [`Self::date_closed_segment`] — which is what it did —
    /// hands the steering loop a sample that assumes the segment has closed
    /// when it has only just started: `implied` lands `seg_secs - chunk_secs`
    /// early, so every date the flow publishes is early by that much (1.8 s at
    /// 2 s segments and 200 ms chunks), from segment zero and permanently,
    /// because the founding `or_insert_with` sample is one of those. The
    /// biased call also *remembers* the segment, so the honest sample taken
    /// when it genuinely closes hits the cache above and is discarded. And
    /// both renditions of a flow share the epoch, so a correct
    /// non-low-latency sibling is dragged with it: the two agree, and both are
    /// wrong.
    ///
    /// Returns the date, and whether a *sibling* has already declared a
    /// discontinuity under this segment — never a discontinuity of its own,
    /// since this path takes no sample and so discovers nothing. Reproducing
    /// one is not declaring one: once the other rendition has re-anchored, the
    /// date this row is about to publish has moved with it, and a moved date
    /// with no tag is exactly the contradiction `#EXT-X-DISCONTINUITY` exists
    /// to close.
    ///
    /// Returns `None` when the position is on a timeline this clock does not
    /// describe — a source that has restarted since the last close. Its
    /// caller [`open_segment_date`] returns `None` for the other case, a flow
    /// that has closed nothing yet and so has no clock at all. Either way the
    /// row carries no `#EXT-X-PROGRAM-DATE-TIME`, which is spec-legal (the
    /// tag is optional under RFC 8216 §4.3.2.6)
    /// and self-healing: the first close settles it, one segment in. Seeding
    /// the epoch here instead would found the flow's clock — and its
    /// sibling's — on a sample taken at an arbitrary point inside a segment,
    /// carrying whatever the pipeline delay was at that instant, in exchange
    /// for one segment's worth of tag.
    fn date_open_segment(
        &self,
        base_dts_90k: u64,
        seg_secs: f64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<(chrono::DateTime<chrono::Utc>, bool)> {
        let base = self.align(base_dts_90k);
        let media_secs = base as f64 / 90_000.0;

        // `now` sits somewhere inside the open segment, so an honest sample
        // implies an epoch up to one segment *later* than the held one, and
        // never earlier. (The pipeline delay cancels: it is already baked into
        // the epoch.) Outside that band plus the re-anchor tolerance is a
        // timeline this clock does not describe — a source that has restarted
        // since the last close — and the honest answer is no date at all,
        // rather than an hour-stale one with no `#EXT-X-DISCONTINUITY` to
        // explain it, since only the close path may declare one.
        let off = (now - secs(media_secs) - self.epoch).num_milliseconds() as f64 / 1000.0;
        if !(-EPOCH_REANCHOR_SECS..=EPOCH_REANCHOR_SECS + seg_secs).contains(&off) {
            return None;
        }

        // If a sibling has already dated this segment, reproduce its answer —
        // and the discontinuity it declared — for the same reason a closing
        // segment does.
        if let Some((_, was, disc)) = self.recent.iter().find(|(d, _, _)| *d == base) {
            return Some((*was + secs(media_secs), *disc));
        }
        // Nothing has closed this segment, so nothing has found a re-anchor
        // under it. This path may not invent one.
        Some((self.epoch + secs(media_secs), false))
    }

    /// Take a sample too far out to be jitter as a new timeline.
    ///
    /// The log escalates deliberately. One of these is news — a source
    /// restarted, a flow was reconfigured under the same id — and INFO is the
    /// level for it. A *run* of them is a fault, and it used to read exactly
    /// like the routine case: before [`Self::align`], two renditions on
    /// opposite sides of a PTS wrap re-anchored each other twice per segment
    /// forever, every line naming a source restart that had not happened.
    /// Alignment closes that particular door; the escalation stays because a
    /// re-anchor loop must never again be indistinguishable from routine
    /// news.
    fn reanchor(
        &mut self,
        implied: chrono::DateTime<chrono::Utc>,
        now: chrono::DateTime<chrono::Utc>,
        flow_id: &str,
    ) {
        let rapid = self.last_reanchor.is_some_and(|t| {
            (now - t).num_milliseconds() as f64 / 1000.0 < REANCHOR_THRASH_SECS
        });
        self.reanchor_run = if rapid { self.reanchor_run + 1 } else { 1 };
        self.last_reanchor = Some(now);

        if !rapid {
            tracing::info!(
                flow_id,
                from = %self.epoch.to_rfc3339(),
                to = %implied.to_rfc3339(),
                "CMAF: media timeline re-anchored (a source restart or PTS discontinuity, not jitter)"
            );
        } else if self.reanchor_run == REANCHOR_THRASH_RUN
            || self.reanchor_run.is_multiple_of(100)
        {
            tracing::warn!(
                flow_id,
                runs = self.reanchor_run,
                from = %self.epoch.to_rfc3339(),
                to = %implied.to_rfc3339(),
                "CMAF: media timeline re-anchoring repeatedly — the published dates are unusable while it continues. Either two outputs of this flow disagree about the timeline, or the source restarts on every segment."
            );
        }

        self.epoch = implied;
        self.filtered = implied;
        // The clear is load-bearing, not tidiness. A source restarting at PTS 0
        // re-uses the `base_dts` values still sitting in this cache, and the
        // lookup takes the *oldest* match, so a retained entry would answer the
        // next post-restart segment with the epoch from before the restart — an
        // hour-stale date, published with no `#EXT-X-DISCONTINUITY`, which is
        // the exact failure the cache's own discontinuity flag was added to
        // close.
        //
        // The price is [`FlowClock::recent`]'s cross-rendition bound: a sibling
        // more than a segment behind loses the answer it was going to
        // reproduce. Widening that means honouring an entry only when the fresh
        // sample agrees with it to within `EPOCH_REANCHOR_SECS`, and consulting
        // the cache *before* the re-anchor test rather than after — the reverse
        // of an ordering that was itself a fix. Not done blind.
        self.recent.clear();
    }

    /// Remember the epoch this segment was dated with — and whether dating it
    /// re-anchored the timeline — so the flow's other rendition reproduces
    /// both exactly.
    fn remember(&mut self, base_dts_90k: u64, discontinuity: bool) {
        self.recent.push_back((base_dts_90k, self.epoch, discontinuity));
        while self.recent.len() > FLOW_CLOCK_HISTORY {
            self.recent.pop_front();
        }
    }
}

/// The date to publish for a closed segment starting at `base_dts_90k`, and
/// whether the media timeline re-anchored under it.
///
/// This used to be `Utc::now() - segment_duration`, sampled afresh for every
/// segment. That records when the edge got round to closing the segment, not
/// when the content happened, and it made the tag carry the scheduling and
/// pipeline delay between the two. Because only one date is written per
/// playlist — for whichever segment is currently first — that sample also
/// anchored the entire window, and was re-taken every time the window slid.
///
/// Measured on the demo rig before this change: each rendition's head date
/// wandered 27 ms (main) and 67 ms (proxy) against a steady clock, and the two
/// renditions placed the same segment 31-81 ms apart, moving ~50 ms from one
/// sample to the next. At 25 fps that is a picture that lands one to two
/// frames from where it was asked for, differently each time.
///
/// Now the media timeline decides, and wall clock is consulted exactly once
/// per flow to place it. `now` is passed in so the arithmetic can be tested
/// without a clock; the arithmetic itself lives on [`FlowClock`] so it can be
/// tested without the process-global map as well.
fn segment_date_marking(
    flow_id: &str,
    base_dts_90k: u64,
    seg_secs: f64,
    now: chrono::DateTime<chrono::Utc>,
) -> (chrono::DateTime<chrono::Utc>, bool) {
    let mut guard = lock_flow_clocks();
    let clock = guard
        .entry(flow_id.to_string())
        .or_insert_with(|| FlowClock::new(base_dts_90k, seg_secs, now));
    clock.date_closed_segment(base_dts_90k, seg_secs, now, flow_id)
}

/// [`segment_date_marking`] without the discontinuity flag. Every caller in
/// the output wants the flag, so this exists for the tests that predate it.
#[cfg(test)]
fn segment_date(
    flow_id: &str,
    base_dts_90k: u64,
    seg_secs: f64,
    now: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    segment_date_marking(flow_id, base_dts_90k, seg_secs, now).0
}

/// The date to publish for a segment that has just opened, and whether a
/// sibling rendition has already declared a discontinuity under it — read-only,
/// see [`FlowClock::date_open_segment`]. `None` when the flow has no epoch yet,
/// or when the one it has does not describe this segment's timeline.
fn open_segment_date(
    flow_id: &str,
    base_dts_90k: u64,
    seg_secs: f64,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(chrono::DateTime<chrono::Utc>, bool)> {
    let guard = lock_flow_clocks();
    guard.get(flow_id)?.date_open_segment(base_dts_90k, seg_secs, now)
}

/// Mid-loop output state.
struct CmafState {
    video_seg: Option<VideoSegmenter>,
    audio_seg: Option<AudioSegmenter>,
    /// Whether audio has produced its init data yet (e.g. AAC config
    /// observed). Init.mp4 is published only after both video + (if
    /// configured) audio are ready.
    audio_ready: bool,
    /// Latched at the first init.mp4 publish: does this output carry audio?
    ///
    /// `init.mp4` declares the track list, and a browser builds its decoders
    /// from it once. So the answer has to be decided *before* the first
    /// publish and then never change: declaring an audio track that no
    /// fragment fills stalls MSE silently (see #130), and muxing audio into
    /// fragments whose init never declared the track is just as broken.
    ///
    /// `None` until decided. The video track can materialise before the
    /// first audio frame arrives, so the decision waits out
    /// [`AUDIO_DETECT_GRACE`] before settling on video-only.
    audio_muxing: Option<bool>,
    /// When the video track first materialised — the clock that bounds the
    /// wait above. `None` until there is a video track.
    video_ready_at: Option<std::time::Instant>,
    /// Latched once the "audio arrived too late to be carried" warning has
    /// been raised, so it is said once per flow rather than on every frame.
    late_audio_warned: bool,
    /// Estimated video bitrate in bps (EWMA over emitted segments).
    video_bps_ewma: u64,
    /// Estimated audio bitrate in bps.
    audio_bps_ewma: u64,
    /// Wall-clock unix seconds of first segment emission.
    availability_start_unix: i64,
    /// True after init.mp4 has been published at least once. Controls the
    /// one-time log lines, and whether a low-latency output may start
    /// emitting chunks — not whether it is published again.
    init_uploaded: bool,
    /// The init generation this output is publishing — mirrors
    /// `VideoSegmenter::generation`, which is the authority: the segmenter
    /// stamps every segment it cuts, and this is synced from it before each
    /// init publish (`adopt_generation`). Zero is the ordinary life of a
    /// stream and keeps the name `init.mp4`. Each change past that publishes
    /// `init-{n}.mp4` and leaves the older ones in place: the rows already
    /// in the window still name them, and a viewer seeking back into that
    /// media needs them to decode it.
    init_generation: u32,
    /// `init_object_name(init_generation)`, kept so the hot paths do not
    /// format it per frame.
    init_uri: String,
    /// The bytes last published under `init_uri`, so a rotation can keep
    /// them: the periodic republish exists because an origin can lose what
    /// it holds, and after a rotation the window still names the older init.
    last_init_bytes: Option<Vec<u8>>,
    /// Older generations the window still references, republished alongside
    /// the current one on the same cadence and forgotten once no row names
    /// them. Only this process's own generations: a restored window's older
    /// inits were published by a previous run and their bytes are not here
    /// — unless the restore fetched them, which it tries to.
    init_history: Vec<PublishedInit>,
    /// The restored generation's init as the origin served it, until this
    /// run's own has been compared with it: on a mismatch it goes into
    /// `init_history`, because the restored rows keep naming it and nothing
    /// else holds its bytes.
    restored_current_init: Option<PublishedInit>,
    /// True while low-latency PUTs are failing to close, so the Warning
    /// fires once per failure episode rather than once per segment.
    ll_put_failing: bool,
    /// The codec-family-change Warning has been raised; it does not repeat
    /// on every frame of a source that is not coming back.
    family_change_warned: bool,
    /// True while init.mp4 uploads are failing. Two jobs: it shortens the
    /// republish interval to a retry interval, and it makes the manager
    /// Warning fire once per failure *episode* rather than once per attempt.
    init_upload_failing: bool,
    /// When init.mp4 was last published. `None` until the first upload.
    ///
    /// Publishing it exactly once made the output unrecoverable if the origin
    /// ever lost it — a restart, a cache wipe, a CDN eviction. Segments keep
    /// arriving and the manifest keeps listing them, but every player 404s on
    /// `#EXT-X-MAP` and can decode nothing, with no way back short of
    /// restarting the flow. The relay's own origin wipes its store on startup,
    /// so this was reachable just by restarting the relay.
    init_last_upload: Option<std::time::Instant>,
    /// Rolling window of muxed segments (newest last).
    playlist: VecDeque<M3u8Entry>,
    /// The first segment of this run joins a window a previous run wrote, and
    /// does not continue its media timeline: `base_dts` restarts with the
    /// process, and the flow clock is process-global so it cannot know. Set
    /// when a window was restored, and consumed by the first row published.
    restore_discontinuity: bool,
    /// The init the restored rows were published under, until it has been
    /// compared against this run's. See the check in `publish_init_if_due`.
    restored_init_fingerprint: Option<String>,
    /// This run's init identity, published on every manifest so the next run
    /// can make that comparison.
    init_fingerprint: Option<String>,
    /// Sequence number the next segment should take.
    ///
    /// Non-zero when a previous run's window was restored: numbering has to
    /// continue past what the origin already holds, or the new run overwrites
    /// the segments it just restored.
    resume_seq: u64,
    /// The largest `#EXT-X-TARGETDURATION` this output has ever published.
    ///
    /// A high-water mark rather than the current window's maximum, and the
    /// difference matters both ways. It has to *rise* the moment a row longer
    /// than the advertised target enters the window, or the playlist breaks
    /// RFC 8216 §4.3.3.1 against a row it is listing right now. It must not
    /// *fall* when that row is trimmed: a player reads this once and sizes its
    /// reload cadence, its buffer and — in low-latency mode — the hold-back it
    /// starts at from it, so a value that shrinks between reloads retracts a
    /// decision it has already acted on: the spec's model is a playlist a
    /// server appends to and trims, not one whose declared bounds move under a
    /// player. A window that alternates 2 s and 5 s segments would otherwise
    /// oscillate the tag on every trim.
    ///
    /// The cost of holding the high mark is a reload interval sized for the
    /// longest segment the flow has ever produced, which is the conservative
    /// direction. `0` until the first playlist is published.
    target_duration_published: u64,
    /// How many discontinuous entries have already been trimmed off the front
    /// of that window.
    ///
    /// This is `#EXT-X-DISCONTINUITY-SEQUENCE`: RFC 8216 §4.3.3.3 defines it
    /// as the count of discontinuities *before* the first segment the playlist
    /// still lists, and a player uses it to keep its own count consistent
    /// across reloads once the tagged row has aged out of the window. It only
    /// grows, so it is counted as entries leave rather than derived from what
    /// is left.
    discontinuities_trimmed: u64,
    /// Optional re-encoder for audio (Phase 3).
    audio_reencoder: Option<AudioReencoder>,
    /// Optional re-encoder for video (Phase 3).
    video_reencoder: Option<VideoReencoder>,
    /// LL-CMAF: in-flight chunked PUT for the current segment. None
    /// between segments.
    ll_current: Option<LlSegment>,
    /// CENC encryptor (Phase 5). When `Some`, video and audio
    /// samples get encrypted in place before muxing into the
    /// segment, and the init segment carries `tenc`/`pssh`.
    cenc: Option<CencRuntime>,
    /// Lazy FFmpeg-backed audio decoder for non-AAC sources
    /// (MP2 / AC-3 / E-AC-3). Opened on first `OtherAudio`.
    #[cfg(feature = "media-codecs")]
    ff_audio_decoder: Option<video_engine::AudioDecoder>,
}

struct CencRuntime {
    encryptor: cenc::CencEncryptor,
    scheme: cenc::Scheme,
    key_id: [u8; 16],
    extra_pssh: Vec<Vec<u8>>,
}

/// Rebuild the published window from what the origin already holds.
///
/// A CMAF output starts with an empty playlist, so a restart republishes a
/// manifest covering only what it has produced *since* — while the origin
/// still holds the previous hour. Every viewer's DVR history vanishes, marks
/// grey out because the player reads reachability from the playlist, and the
/// window refills only in real time. This is the mirror of the relay-side
/// failure where the origin holds less than the edge advertises.
///
/// So the previous manifest is read back and its rows become the starting
/// window. Best-effort by design: a fresh stream 404s, and an origin that
/// cannot be reached must not stop an output starting — the cost of getting
/// this wrong is a shorter window, and the cost of failing here is no output
/// at all.
///
/// Returns what the origin's playlist said — see [`RestoredWindow`] for the
/// four things it carries.
async fn restore_published_window(
    base: &str,
    auth: Option<&str>,
    limit: usize,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<RestoredWindow> {
    // Its own budget, and racing the cancel token.
    //
    // This is awaited between subscribing the broadcast receiver and entering
    // the packet loop, so every second it spends is a second the output is not
    // draining its channel — at the default capacity, about seventeen seconds
    // of headroom at 10 Mbps before the output opens on a `Lagged` and the
    // window gains a hole at exactly the join the restore exists to make
    // seamless. The clip client's 60 s request timeout is sized for pulling
    // whole segments, not for a startup preflight, and an origin that accepts
    // the connection and then stalls holds the whole of it.
    //
    // A window is a nicety; the media path is not. Three seconds is ample for
    // a manifest, and a stop arriving during it is honoured rather than waited
    // out.
    const RESTORE_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);
    let body = tokio::select! {
        _ = cancel.cancelled() => return None,
        r = tokio::time::timeout(RESTORE_BUDGET, clips::fetch_manifest(base, auth)) => match r {
            Ok(Ok(b)) => b,
            // Distinguished, because they are different problems. A fresh
            // stream 404s and that is the ordinary case; anything else means
            // the window this output was publishing is about to be replaced by
            // an empty one, and the run will renumber from seg-00000 over
            // segments the origin still holds and still serves.
            Ok(Err(e)) => {
                // "HTTP 404", not a bare "404": the message carries the
                // manifest's URL, and a stream or host with 404 in its name
                // would otherwise silence every real failure.
                let msg = format!("{e:#}");
                if !msg.contains("returned HTTP 404") {
                    tracing::warn!(
                        origin = %base, error = %msg,
                        "CMAF output: could not read back the window this stream was \
                         publishing; starting from an empty one, and segment numbering \
                         restarts at zero"
                    );
                }
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    origin = %base, budget_secs = RESTORE_BUDGET.as_secs(),
                    "CMAF output: the origin did not answer in time; starting from an \
                     empty window rather than holding the media path"
                );
                return None;
            }
        },
    };
    let text = String::from_utf8_lossy(&body);
    let mut window = parse_published_window(&text, limit)?;

    // The older inits the restored rows name. This run rebuilds only the
    // generation it continues; the ones before it were published by a
    // previous run, and without their bytes the 30 s republish could not
    // cover them — a relay that lost its store would come back with the live
    // edge decodable and every older row 404ing on its map. They are a few
    // kilobytes each and there are rarely more than one or two.
    //
    // The current generation's too: this run rebuilds it from its own track,
    // and if that turns out to differ, the mismatch opens a new generation
    // and the restored rows keep naming this one — which then has to be
    // republished from these bytes, since nothing else holds them.
    //
    // One deadline across the list, applied per GET, so an origin that
    // answers the first and stalls on the second still leaves the first in
    // hand — a single timeout over the whole fetch dropped everything read.
    const HELD_INITS_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
    let deadline = tokio::time::Instant::now() + HELD_INITS_BUDGET;
    // Newest first: if the budget runs out, the init the longest-lived rows
    // name — and the one a mismatch retires — is the one in hand.
    let mut wanted: Vec<String> = Vec::new();
    for name in window.rows.iter().rev().map(CmafState::row_init_name) {
        if !wanted.iter().any(|w| w == name) {
            wanted.push(name.to_string());
        }
    }
    let mut held = Vec::new();
    for name in wanted {
        let url = format!("{base}/{name}");
        let r = tokio::select! {
            _ = cancel.cancelled() => return None,
            r = tokio::time::timeout_at(deadline, clips::http_get(&url, auth)) => r,
        };
        match r {
            Ok(Ok(bytes)) => held.push(PublishedInit { uri: name, bytes }),
            Ok(Err(e)) => tracing::warn!(
                origin = %base, init = %name, error = %format!("{e:#}"),
                "CMAF output: an init the restored window names could not be read back; \
                 it will not be republished if the origin loses it"
            ),
            Err(_) => {
                tracing::warn!(
                    origin = %base, init = %name, budget_secs = HELD_INITS_BUDGET.as_secs(),
                    "CMAF output: the origin did not return an init the restored window \
                     names in time; it and any after it will not be republished"
                );
                break;
            }
        }
    }
    window.held_inits = held;
    Some(window)
}

/// What a served media playlist says about the window it is advertising.
/// An init object an older generation of this output published.
struct PublishedInit {
    uri: String,
    bytes: Vec<u8>,
}

struct RestoredWindow {
    rows: VecDeque<M3u8Entry>,
    /// The sequence number this run must carry on from.
    next_seq: u64,
    /// `#EXT-X-DISCONTINUITY-SEQUENCE` as served — how many discontinuities
    /// have already aged out of the window.
    discontinuity_sequence: u64,
    /// Which init these rows were published under, if the previous run said.
    /// `None` for a manifest written before this tag existed, which is treated
    /// the same as a match — there is nothing to compare, and refusing every
    /// such restore would cost the window for no evidence.
    init_fingerprint: Option<String>,
    /// The highest init generation the served playlist named — the one its
    /// newest rows decode against, and the one this run continues or, if its
    /// own init differs, rotates past.
    init_generation: u32,
    /// Every init the rows name, read back so this run can republish them.
    /// Empty when the parser built the window; the restore fills it.
    held_inits: Vec<PublishedInit>,
}

/// A short, stable name for exactly these init bytes.
///
/// Sixteen hex characters of SHA-256 — this is an equality check between two
/// runs of the same process family, not a defence against anyone choosing
/// bytes, and the tag it rides in has to stay small enough to be free on every
/// manifest publish.
fn init_fingerprint(init_bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(init_bytes);
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The tag the init fingerprint rides in.
///
/// A private `#EXT-` tag rather than a renamed `init.mp4` object: RFC 8216
/// §4.1 requires a client to ignore a tag it does not recognise, so it costs
/// players nothing, and the relay origin copies through every line it does not
/// itself rewrite. Renaming the object would have worked too, and would have
/// meant a second live init on the origin and a change to how the DASH
/// `initialization` template is written — much more surface for the same
/// answer.
const INIT_FINGERPRINT_TAG: &str = "#EXT-X-BILBYCAST-INIT:";

/// Write this run's init identity into a playlist, right after `#EXTM3U`.
///
/// Done here rather than inside `build_hls_playlist` so the tag costs the
/// playlist builder and its twenty-odd call sites nothing: it is a private
/// marker this edge writes for its own next run to read, not part of the HLS
/// the builder is responsible for getting right.
///
/// The generation rides with the hash, because the two are minted together
/// and the rows cannot stand in for it: a manifest published in the segment
/// after a rotation names the old generation on every row while its stamp
/// describes the new init — the one the next run must continue, or it would
/// publish under the old name and over the object every restored row decodes
/// against. `<hash>,gen=<n>`; a stamp with no generation is from a build
/// before rotations existed, and says nothing about it.
fn stamp_init_fingerprint(body: String, fingerprint: Option<&str>, generation: u32) -> String {
    let Some(fp) = fingerprint else {
        return body;
    };
    match body.find('\n') {
        Some(nl) => {
            let mut out = String::with_capacity(body.len() + fp.len() + 48);
            out.push_str(&body[..=nl]);
            out.push_str(INIT_FINGERPRINT_TAG);
            out.push_str(fp);
            out.push_str(",gen=");
            out.push_str(&generation.to_string());
            out.push('\n');
            out.push_str(&body[nl + 1..]);
            out
        }
        None => body,
    }
}

/// The longest `#EXTINF` this parser will accept, in seconds.
///
/// Validation bounds a configured segment duration to 1..=10 s, but the
/// segmenter cuts on the first IDR at or past the target, so a long-GOP
/// contribution encoder can legitimately produce rows several times that. Ten
/// minutes is past anything a real source does and still refuses what a
/// hand-written or corrupt manifest could carry: rows are copied back out
/// verbatim, so an unbounded or non-finite value would be republished as-is —
/// `NaN` parses happily and prints back as `#EXTINF:NaN,`, which every player
/// rejects, for the whole window until it slides out.
const MAX_RESTORED_EXTINF_SECS: f64 = 600.0;

/// The rows of a served media playlist, and what to carry on from.
///
/// `None` only when the playlist names no segment at all — a fresh stream. A
/// playlist whose every row is unusable still yields a window, an empty one,
/// carrying the number to continue from: the numbering is protected by every
/// `seg-N` the origin advertises, usable row or not, because a run that
/// restarted at zero would overwrite segments the origin still serves.
fn parse_published_window(text: &str, limit: usize) -> Option<RestoredWindow> {
    let mut rows: Vec<M3u8Entry> = Vec::new();
    let mut pdt: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut dur: Option<f64> = None;
    let mut pending_discontinuity = false;
    let mut discontinuity_sequence = 0u64;
    let mut init_fingerprint: Option<String> = None;
    // The map in force for the rows that follow, as `build_hls_playlist`
    // writes it: the head map, then one re-declared wherever a generation
    // starts. `None` is the stream's own `init.mp4`. Dropping these — which
    // is what treating the tag as a comment did — put every restored row
    // under whatever init this run publishes next, and after a rotation that
    // is not the one the older rows decode against.
    let mut current_map: Option<String> = None;
    let mut init_generation = 0u32;
    // The highest segment number the origin advertises, usable or not.
    let mut max_seq: Option<u64> = None;
    // Whether `#EXT-X-PART` lines followed the last row. A low-latency
    // playlist writes the segment still being uploaded exactly like a closed
    // one and hangs its parts beneath it; if the previous run died mid-PUT
    // that segment was never stored, so restoring the row advertises a 404
    // in the middle of the window for as long as the window lasts.
    let mut last_row_open = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(INIT_FINGERPRINT_TAG) {
            let (fp, attrs) = rest.trim().split_once(',').unwrap_or((rest.trim(), ""));
            if !fp.is_empty() {
                init_fingerprint = Some(fp.to_string());
            }
            if let Some(g) = attrs
                .split(',')
                .find_map(|a| a.trim().strip_prefix("gen="))
                .and_then(|g| g.parse::<u32>().ok())
                .filter(|g| *g <= manifest::MAX_INIT_GENERATION)
            {
                init_generation = init_generation.max(g);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            // `URI="init-2.mp4"`, possibly followed by a viewer token the
            // origin appended; the name is the part that matters.
            let uri = rest
                .split(',')
                .find_map(|attr| attr.trim().strip_prefix("URI="))
                .map(|v| v.trim_matches('"'))
                .map(|v| v.split(['?', '#']).next().unwrap_or(v))
                .map(|v| v.rsplit('/').next().unwrap_or(v).to_string());
            match uri.as_deref().and_then(init_generation_of) {
                Some(0) => current_map = None,
                Some(g) => {
                    init_generation = init_generation.max(g);
                    current_map = uri;
                }
                // A name this edge never writes: keep the row's map as it
                // was served rather than silently re-pointing it, and do
                // not let it steer the generation count.
                None => current_map = uri,
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pdt = chrono::DateTime::parse_from_rfc3339(rest.trim())
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc));
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            dur = rest
                .trim_end_matches(',')
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|d| d.is_finite() && *d > 0.0 && *d <= MAX_RESTORED_EXTINF_SECS);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
            // Restored, not reset. `CmafState::new` starts it at 0, so a
            // restart used to republish a playlist whose discontinuity count
            // went backwards while its media sequence carried on normally —
            // which RFC 8216 §6.3.3 makes an incompatible playlist change, and
            // players answer by resynchronising or resetting the media element.
            discontinuity_sequence = rest.trim().parse::<u64>().unwrap_or(0);
        } else if line == "#EXT-X-DISCONTINUITY" {
            // Carried onto the row it precedes. Dropping it asserted one
            // continuous timeline across a real media-timeline re-anchor whose
            // post-jump dates *were* restored: a viewer scrubbing back over the
            // join decodes across it with no reset, and one who reloads sees a
            // tag vanish from rows it had already parsed.
            pending_discontinuity = true;
        } else if line.starts_with("#EXT-X-PART:") {
            last_row_open = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            // The origin rewrites URIs to carry a viewer token; the name is
            // the part that matters, and the sequence number is in it.
            let uri = line.split(['?', '#']).next().unwrap_or(line).to_string();
            let seq = uri
                .rsplit('/')
                .next()
                .and_then(|n| n.strip_prefix("seg-"))
                .and_then(|n| n.split('.').next())
                .and_then(|n| n.parse::<u64>().ok());
            if let Some(seq) = seq {
                max_seq = max_seq.max(Some(seq));
            }
            if let (Some(seq), Some(d)) = (seq, dur) {
                rows.push(M3u8Entry {
                    sequence_number: seq,
                    duration_secs: d,
                    uri: Some(uri),
                    parts: Vec::new(),
                    init_uri: current_map.clone(),
                    program_date_time: pdt,
                    discontinuity: pending_discontinuity,
                });
                pending_discontinuity = false;
                last_row_open = false;
            }
            pdt = None;
            dur = None;
        }
    }
    // `checked_add`, because the release profile sets no overflow checks: a row
    // named `seg-18446744073709551615.m4s` would otherwise wrap `next_seq` to 0.
    // With no answer there is no number to carry on from, and no window either
    // — a manifest this edge could not have written is not one to trust.
    let next_seq = max_seq?.checked_add(1)?;
    if last_row_open {
        // The number is kept — `max_seq` counted it — so the segment the
        // previous run was uploading is neither advertised nor reused.
        rows.pop();
    }
    // Never restore more than this output is configured to advertise, or the
    // first trim would drop most of it anyway and the manifest would briefly
    // claim a window the retention policy does not keep. Counted the way
    // `trim_playlist` counts: a tagged row that leaves the window advances the
    // discontinuity sequence, or a player that loaded the served playlist sees
    // its count go backwards on the next fetch.
    if rows.len() > limit {
        let excess = rows.len() - limit;
        discontinuity_sequence += rows.drain(..excess).filter(|r| r.discontinuity).count() as u64;
    }
    Some(RestoredWindow {
        rows: rows.into_iter().collect(),
        next_seq,
        discontinuity_sequence,
        init_fingerprint,
        init_generation,
        held_inits: Vec::new(),
    })
}

/// LL-CMAF state held across the duration of one segment upload.
struct LlSegment {
    handle: ChunkedPutHandle,
    /// Sequence number of this segment.
    sequence_number: u64,
    /// Number of chunks emitted so far. The first chunk carries the
    /// `styp` prefix; subsequent chunks skip it.
    chunks_emitted: u32,
    /// Parts advertised on the current manifest for this segment.
    parts: Vec<HlsPartEntry>,
    /// 90 kHz DTS this segment starts at, so the row it contributes to the
    /// playlist is dated off the media timeline exactly as a closed segment
    /// is. It used to hold the wall clock at which the segment opened, which
    /// made the in-progress row disagree with every row around it by whatever
    /// the pipeline delay happened to be at that instant.
    base_dts_90k: u64,
    /// The init generation this segment was opened under, and whether it is
    /// the first of it — read off the segmenter at open, so the row this
    /// segment contributes is right whatever the output's state says by the
    /// time it closes. See `CompletedSegment::generation`.
    generation: u32,
    first_of_generation: bool,
    /// The filename this segment is being uploaded under.
    uri: String,
}

impl CmafState {
    fn new() -> Self {
        Self {
            video_seg: None,
            audio_seg: None,
            audio_ready: false,
            audio_muxing: None,
            video_ready_at: None,
            late_audio_warned: false,
            video_bps_ewma: 0,
            audio_bps_ewma: 0,
            availability_start_unix: 0,
            init_uploaded: false,
            init_generation: 0,
            init_uri: init_object_name(0),
            last_init_bytes: None,
            init_history: Vec::new(),
            restored_current_init: None,
            ll_put_failing: false,
            family_change_warned: false,
            init_upload_failing: false,
            init_last_upload: None,
            playlist: VecDeque::new(),
            restore_discontinuity: false,
            restored_init_fingerprint: None,
            init_fingerprint: None,
            resume_seq: 0,
            target_duration_published: 0,
            discontinuities_trimmed: 0,
            audio_reencoder: None,
            video_reencoder: None,
            ll_current: None,
            cenc: None,
            #[cfg(feature = "media-codecs")]
            ff_audio_decoder: None,
        }
    }

    /// The `#EXT-X-TARGETDURATION` to publish for `entries`, never below one
    /// already published — see [`CmafState::target_duration_published`].
    fn advertised_target_duration(
        &mut self,
        config_target_secs: f64,
        entries: &[M3u8Entry],
    ) -> u64 {
        self.target_duration_published = self
            .target_duration_published
            .max(required_target_duration(config_target_secs, entries));
        self.target_duration_published
    }

    /// Where the segment that just closed ended, on the media timeline.
    ///
    /// `push()` has already moved the segmenter on to the new segment — it
    /// runs in `handle_video`, before the close is published — so the base it
    /// reports now is the end of the one that just closed, and the difference
    /// between the two is that segment's real length.
    ///
    /// This is a named function rather than four lines at the call site
    /// because it is the whole of the fix for a closed low-latency row dated
    /// by the configured target instead of by how long it actually ran, and
    /// nothing could reach it: `closed_ll_entry` took the answer as a
    /// parameter and both of its tests handed it a literal, so reverting the
    /// derivation to `None` restored the bug with the suite green. Pinned by
    /// `the_closed_row_takes_its_length_from_the_segmenter`, which drives a
    /// real [`VideoSegmenter`] rather than passing the number in.
    ///
    /// `None` before the first IDR, when no segment is open yet;
    /// `closed_ll_entry` then falls back to the nominal length.
    fn closed_segment_end_dts_90k(&self) -> Option<u64> {
        self.video_seg
            .as_ref()
            .and_then(|vs| vs.open_segment_base_dts_90k())
    }

    /// Trim the playlist back to the advertised window, counting any
    /// discontinuity that leaves with an entry.
    ///
    /// The count has to be taken here rather than reconstructed later: once
    /// the tagged row is gone the playlist holds no trace of it, and a player
    /// that reloads across the trim would see its discontinuity count go
    /// backwards.
    fn trim_playlist(&mut self, window: usize) {
        while self.playlist.len() > window {
            match self.playlist.pop_front() {
                Some(e) if e.discontinuity => self.discontinuities_trimmed += 1,
                Some(_) => {}
                None => break,
            }
        }
        // Here and nowhere else: a row leaving the window is the only event
        // that can un-name a generation. Trimming at the rotation instead
        // forgot a generation whose only row was the segment still in flight.
        self.trim_init_history();
    }

    /// Does this run's init differ from the one the restored rows were
    /// published under?
    ///
    /// Answered once, on the first init this run builds. `None` when there is
    /// nothing to compare against — no fingerprint was restored, which is a
    /// manifest from before the tag existed, or a fresh stream.
    ///
    /// The first version of this check dropped the restored rows on a
    /// mismatch, because `init.mp4` was one fixed object about to be
    /// overwritten and the rows would have been advertised under parameter
    /// sets that did not describe them. With per-generation inits nothing is
    /// overwritten: a mismatch opens a new generation, this run's init goes
    /// up under a new name, and the restored rows keep decoding against the
    /// object they always named.
    fn restored_init_differs(&mut self, fingerprint: &str) -> Option<bool> {
        let prev = self.restored_init_fingerprint.take()?;
        // With no restored row there is nothing the previous init describes,
        // and nothing to keep apart from this run's.
        let any_restored = self.playlist.iter().any(|r| r.sequence_number < self.resume_seq);
        Some(any_restored && prev != fingerprint)
    }

    /// Point this run's own rows — numbered from `resume_seq` — that were
    /// cut under generation `from` at generation `to`. A segment can close
    /// before the first init is built (the audio-detection grace is three
    /// seconds and a segment is two), and if that init then opens a new
    /// generation the row was stamped with the old one while its media
    /// decodes against the new. Only rows of `from`: a row cut under an
    /// earlier generation of this run decodes against that one.
    fn relabel_own_rows(&mut self, from: u32, to: u32) {
        let was = init_object_name(from);
        let name = (to > 0).then(|| init_object_name(to));
        let resume_seq = self.resume_seq;
        for row in self
            .playlist
            .iter_mut()
            .filter(|r| r.sequence_number >= resume_seq && Self::row_init_name(r) == was)
        {
            row.init_uri = name.clone();
        }
    }

    /// Whether the last row in the window names a different init from the
    /// one a row of `generation` would — a change of map between consecutive
    /// rows is a break in the decode chain and the row has to say so, whether
    /// or not the segment that opened the generation was ever advertised.
    fn generation_break(&self, generation: u32) -> bool {
        let name = init_object_name(generation);
        self.playlist.back().is_some_and(|last| Self::row_init_name(last) != name)
    }

    /// The init object a row decodes against.
    fn row_init_name(row: &M3u8Entry) -> &str {
        row.init_uri.as_deref().unwrap_or("init.mp4")
    }

    /// Forget every held generation no row in the window names any more.
    fn trim_init_history(&mut self) {
        let playlist = &self.playlist;
        self.init_history
            .retain(|h| playlist.iter().any(|r| Self::row_init_name(r) == h.uri));
    }
}

async fn run(
    config: &CmafOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
    flow_stats: Arc<crate::stats::collector::FlowStatsAccumulator>,
) -> anyhow::Result<()> {
    tracing::info!(
        "CMAF output '{}' started -> {} (segment={}s, window={} segments (~{:.0}s), manifests={:?}, audio_encode={:?}, video_encode={:?})",
        config.id,
        config.ingest_url,
        config.segment_duration_secs,
        config.playlist_window_segments(),
        config.playlist_window_segments() as f64 * config.segment_duration_secs,
        config.manifests,
        config.audio_encode.as_ref().map(|a| &a.codec),
        config.video_encode.as_ref().map(|v| &v.codec),
    );
    event_sender.emit_flow(
        EventSeverity::Info,
        category::CMAF,
        format!(
            "CMAF output '{}' started (segment={}s, manifests={})",
            config.id,
            config.segment_duration_secs,
            config.manifests.join("+"),
        ),
        flow_id,
    );

    let base_url = config.ingest_url.trim_end_matches('/').to_string();

    // Clip export watches the passthrough rendition only.
    //
    // An operator exporting a moment wants the full-resolution picture, not the
    // 640x360 proxy that exists so scrubbing stays cheap — and the player asks
    // for clips against the main stream, so a proxy poller would find nothing
    // and spend a request every five seconds proving it.
    if config.video_encode.is_none() {
        tokio::spawn(clips::run(
            base_url.clone(),
            config.auth_token.clone(),
            // The flow names the recording an exact cut is read from, but only
            // by default: the recorder files under its `storage_id`, and the
            // exporter reads the id the writer really used off the flow's
            // stats rather than assuming the default.
            flow_id.to_string(),
            flow_stats,
            event_sender.clone(),
            cancel.clone(),
        ));
    }

    let init_name = "init.mp4".to_string();
    let init_url = format!("{base_url}/{init_name}");
    let m3u8_url = format!("{base_url}/manifest.m3u8");
    let mpd_url = format!("{base_url}/manifest.mpd");

    let publish_hls = config.manifests.iter().any(|m| m == "hls");
    let publish_dash = config.manifests.iter().any(|m| m == "dash");

    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "CMAF output '{}': program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    let mut demuxer = TsDemuxer::new(config.program_number);
    let mut state = CmafState::new();

    // Pick up the window this stream was already publishing.
    //
    // Without this a restart republishes a manifest covering only what it has
    // produced since, while the origin still holds the previous hour — so
    // every viewer's DVR history disappears and refills only in real time.
    // Segment numbering continues from where the old manifest left off, or
    // the new run would overwrite the very segments it just restored.
    // Nothing to resume means a fresh stream, or an origin that cannot be
    // reached. Neither is a reason to refuse to start: the cost is a shorter
    // window, and the cost of failing here would be no output at all.
    if let Some(restored) = restore_published_window(
        &base_url,
        config.auth_token.as_deref(),
        config.playlist_window_segments(),
        &cancel,
    )
    .await
    {
        tracing::info!(
            output = %config.id, segments = restored.rows.len(),
            next_seq = restored.next_seq,
            discontinuity_sequence = restored.discontinuity_sequence,
            "CMAF output: resumed the window the origin already holds"
        );
        state.playlist = restored.rows;
        state.resume_seq = restored.next_seq;
        state.discontinuities_trimmed = restored.discontinuity_sequence;
        state.restored_init_fingerprint = restored.init_fingerprint;
        // Continue the generation the newest restored rows decode against,
        // exactly as the numbering is continued: a run that started again at
        // `init.mp4` would overwrite the object the oldest rows name.
        state.init_generation = restored.init_generation;
        state.init_uri = init_object_name(restored.init_generation);
        // The current generation's bytes are held apart: this run publishes
        // that name itself unless its init differs, in which case they join
        // the republish set under the mismatch.
        let (current, older): (Vec<_>, Vec<_>) = restored
            .held_inits
            .into_iter()
            .partition(|h| h.uri == state.init_uri);
        state.init_history = older;
        state.restored_current_init = current.into_iter().next();
        // A manifest from before the stamp existed says nothing about its
        // init — but the object itself does, now that it has been read back,
        // so the comparison covers that window too rather than publishing
        // over it on trust.
        if state.restored_init_fingerprint.is_none()
            && let Some(kept) = state.restored_current_init.as_ref()
        {
            state.restored_init_fingerprint = Some(init_fingerprint(&kept.bytes));
        }
        state.restore_discontinuity = true;
    }

    // Pre-flight Phase 3 re-encoders.
    if let Some(enc_cfg) = &config.audio_encode {
        match AudioReencoder::new(enc_cfg, &cancel, &config.id, flow_id) {
            Ok(reenc) => {
                tracing::info!(
                    "CMAF output '{}': audio re-encode active codec={} silent_fallback={}",
                    config.id, enc_cfg.codec, reenc.has_silent_fallback(),
                );
                // Silent-fallback path: build the AudioSegmenter eagerly
                // using the declared target params so silent AAC frames
                // can flow into the current segment before any source
                // audio arrives. The ASC is synthesised from the
                // declared sample_rate / channels (AOT=2, AAC-LC).
                if reenc.has_silent_fallback() {
                    if let Some((profile, sr_idx, ch_cfg)) = reenc.silent_fallback_track() {
                        let asc = aac_audio_specific_config(profile, sr_idx, ch_cfg);
                        let sample_rate = codecs::sample_rate_from_index(sr_idx);
                        let track = AudioTrack::aac(
                            asc,
                            sample_rate,
                            ch_cfg as u16,
                            enc_cfg
                                .bitrate_kbps
                                .map(|k| k * 1000)
                                .unwrap_or(128_000),
                        );
                        state.audio_seg =
                            Some(AudioSegmenter::new_from_seq(track, config.segment_duration_secs, state.resume_seq));
                        state.audio_ready = true;
                        tracing::info!(
                            "CMAF output '{}': audio track pre-built for silent_fallback (sr={} ch={})",
                            config.id, sample_rate, ch_cfg,
                        );
                    } else {
                        tracing::warn!(
                            "CMAF output '{}': silent_fallback target sample_rate has no ADTS index — silent-track init deferred",
                            config.id,
                        );
                    }
                }
                state.audio_reencoder = Some(reenc);
            }
            Err(e) => {
                tracing::error!(
                    "CMAF output '{}': audio re-encoder init failed: {e}",
                    config.id,
                );
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::AUDIO_ENCODE,
                    format!("CMAF output '{}': audio_encode init failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }
    // Pre-flight CENC.
    if let Some(cenc_cfg) = &config.encryption {
        match init_cenc_runtime(cenc_cfg) {
            Ok(rt) => {
                tracing::info!(
                    "CMAF output '{}': CENC active scheme={} pssh_extras={}",
                    config.id,
                    cenc_cfg.scheme,
                    cenc_cfg.pssh_boxes.len(),
                );
                state.cenc = Some(rt);
            }
            Err(e) => {
                tracing::error!("CMAF output '{}': CENC init failed: {e}", config.id);
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::CMAF,
                    format!("CMAF output '{}': CENC init failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }

    if let Some(enc_cfg) = &config.video_encode {
        match VideoReencoder::new(enc_cfg, &config.id) {
            Ok(reenc) => {
                tracing::info!(
                    "CMAF output '{}': video re-encode active codec={}",
                    config.id, enc_cfg.codec
                );
                state.video_reencoder = Some(reenc);
            }
            Err(e) => {
                tracing::error!(
                    "CMAF output '{}': video re-encoder init failed: {e}",
                    config.id,
                );
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::VIDEO_ENCODE,
                    format!("CMAF output '{}': video_encode init failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }

    // Silence tick: only armed when the AudioReencoder was built with
    // `silent_fallback = true`.
    let mut silence_interval: Option<tokio::time::Interval> = state
        .audio_reencoder
        .as_ref()
        .and_then(|r| r.silence_chunk_duration())
        .map(|d| {
            let mut iv = tokio::time::interval(d);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            iv
        });

    loop {
        let silence_tick = async {
            match silence_interval.as_mut() {
                Some(iv) => { iv.tick().await; }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("CMAF output '{}' stopping (cancelled)", config.id);
                break;
            }
            _ = silence_tick => {
                if let Some(reenc) = state.audio_reencoder.as_mut() {
                    // Silence accompanies a picture, so it is laid down to
                    // where the picture is — the newest video DTS — and no
                    // further: nothing is emitted until video has arrived.
                    let target = state.video_seg.as_ref().and_then(VideoSegmenter::last_dts_90k);
                    match crate::timed_block_in_place!(
                        "cmaf.audio_silence_encode",
                        crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
                        { reenc.encode_silence_if_needed(target) }
                    ) {
                        Ok(frames) if !frames.is_empty() => {
                            buffer_audio_frames(&mut state, config, frames);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!(
                                "CMAF output '{}': silent-fallback encode error: {e}",
                                config.id
                            );
                        }
                    }
                }
                continue;
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        let payload = if packet.is_raw_ts {
                            &packet.data[..]
                        } else if packet.data.len() > RTP_HEADER_MIN {
                            &packet.data[RTP_HEADER_MIN..]
                        } else {
                            continue;
                        };

                        let ts_bytes: &[u8] = if let Some(ref mut f) = program_filter {
                            filter_scratch.clear();
                            f.filter_into(payload, &mut filter_scratch);
                            if filter_scratch.is_empty() {
                                continue;
                            }
                            &filter_scratch
                        } else {
                            payload
                        };

                        let frames = demuxer.demux(ts_bytes);
                        for frame in frames {
                            handle_frame(
                                frame,
                                &mut state,
                                &mut demuxer,
                                config,
                                &base_url,
                                &init_url,
                                &init_name,
                                &m3u8_url,
                                &mpd_url,
                                publish_hls,
                                publish_dash,
                                &stats,
                                event_sender,
                                flow_id,
                                packet.recv_time_us,
                            ).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "CMAF output '{}': broadcast lag, dropped {n} packets",
                            config.id,
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("CMAF output '{}' broadcast closed", config.id);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    frame: DemuxedFrame,
    state: &mut CmafState,
    demuxer: &mut TsDemuxer,
    config: &CmafOutputConfig,
    base_url: &str,
    init_url: &str,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    stats: &OutputStatsAccumulator,
    event_sender: &EventSender,
    flow_id: &str,
    recv_time_us: u64,
) {
    match frame {
        DemuxedFrame::H264 { nalus, pts, is_keyframe } => {
            handle_video(
                state,
                demuxer,
                VideoCodec::H264,
                nalus,
                pts,
                is_keyframe,
                config,
                base_url,
                init_url,
                init_name,
                m3u8_url,
                mpd_url,
                publish_hls,
                publish_dash,
                stats,
                event_sender,
                flow_id,
                recv_time_us,
            )
            .await;
        }
        DemuxedFrame::H265 { nalus, pts, is_keyframe } => {
            handle_video(
                state,
                demuxer,
                VideoCodec::H265,
                nalus,
                pts,
                is_keyframe,
                config,
                base_url,
                init_url,
                init_name,
                m3u8_url,
                mpd_url,
                publish_hls,
                publish_dash,
                stats,
                event_sender,
                flow_id,
                recv_time_us,
            )
            .await;
        }
        DemuxedFrame::Aac { data, pts } => {
            handle_audio_frame(state, demuxer, config, &data, pts, event_sender, flow_id);
        }
        DemuxedFrame::Opus { .. } => {}
        DemuxedFrame::OtherAudio { stream_type, data, pts } => {
            handle_other_audio_frame(
                state, config, stream_type, &data, pts, event_sender, flow_id,
            );
        }
        // CMAF egress requires H.264 / HEVC. MPEG-2 input would need a
        // transcode hop we don't have today; drop the AU so the audio
        // path keeps working.
        DemuxedFrame::Mpeg2 { .. } => {}
        // Stream discontinuity is metadata for stateful decoders; the
        // CMAF segmenter advances on its own GoP cadence and re-issues
        // an init segment on codec change.
        DemuxedFrame::Discontinuity | DemuxedFrame::Scte35(_) => {}
    }
}

/// Decode a non-AAC source audio PES (MP2 / AC-3 / E-AC-3) via FFmpeg
/// and feed the resulting PCM to the AAC re-encoder, replicating the
/// back half of [`handle_audio_frame`]. CMAF egress requires AAC, so
/// without an `audio_encode` block we drop the frame.
#[cfg(feature = "media-codecs")]
fn handle_other_audio_frame(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    stream_type: u8,
    data: &[u8],
    pts: u64,
    event_sender: &EventSender,
    flow_id: &str,
) {
    let Some(codec) =
        crate::engine::audio_decode::ff_codec_for_stream_type(stream_type)
    else {
        return;
    };
    if state.ff_audio_decoder.is_none() {
        match video_engine::AudioDecoder::open(codec) {
            Ok(d) => state.ff_audio_decoder = Some(d),
            Err(_) => return,
        }
    }
    let dec = state.ff_audio_decoder.as_mut().unwrap();

    let mut decoded: Vec<(Vec<Vec<f32>>, u32, u8)> = Vec::new();
    for au in crate::engine::audio_decode::split_audio_codec_frames(data, codec) {
        if dec.send_packet(au, pts as i64).is_err() {
            continue;
        }
        while let Ok(frame) = dec.receive_frame() {
            decoded.push((frame.planar, frame.sample_rate, frame.channels));
        }
    }
    if decoded.is_empty() {
        return;
    }

    // Lazy-build the audio track from the first decoded frame's params,
    // resolved against any operator-supplied target on `audio_encode`.
    if state.audio_seg.is_none() {
        let (_, src_sr, src_ch) = decoded[0].clone();
        let target_sr = config
            .audio_encode
            .as_ref()
            .and_then(|e| e.sample_rate)
            .unwrap_or(src_sr);
        let target_ch = config
            .audio_encode
            .as_ref()
            .and_then(|e| e.channels)
            .unwrap_or(src_ch);
        // AAC ASC: AOT=2 (LC), then sample-rate index + channel-config —
        // mirrors `aac_audio_specific_config`'s layout.
        let sr_idx = crate::engine::audio_decode::sr_index_from_hz(target_sr).unwrap_or(3);
        let asc = aac_audio_specific_config(1, sr_idx, target_ch);
        let track = AudioTrack::aac(
            asc,
            target_sr,
            target_ch as u16,
            config
                .audio_encode
                .as_ref()
                .and_then(|e| e.bitrate_kbps)
                .map(|k| k * 1000)
                .unwrap_or(128_000),
        );
        state.audio_seg =
                            Some(AudioSegmenter::new_from_seq(track, config.segment_duration_secs, state.resume_seq));
        state.audio_ready = true;
        tracing::info!(
            "CMAF output '{}': audio track detected (re-encoded from \
             stream_type 0x{:02X}) sr={} ch={}",
            config.id, stream_type, target_sr, target_ch,
        );
    }

    // CMAF wants AAC on the wire — without `audio_encode` we have no
    // way to transmux a non-AAC source.
    let video_at = state.video_seg.as_ref().and_then(VideoSegmenter::last_dts_90k);
    let Some(reenc) = state.audio_reencoder.as_mut() else {
        return;
    };
    reenc.mark_real_audio(pts, video_at);

    let mut frames_to_buffer: Vec<(Vec<u8>, u64)> = Vec::new();
    // A PES carries several access units under one PTS; each is submitted
    // at its own place in it, or the re-encoder reads the second and later
    // ones as the source standing still and re-anchors on every PES.
    let mut au_offset_ticks = 0u64;
    for (planar, sr, ch) in decoded {
        let au_pts = pts.saturating_add(au_offset_ticks);
        au_offset_ticks += planar.first().map_or(0, |c| c.len() as u64) * 90_000 / sr.max(1) as u64;
        match crate::timed_block_in_place!(
            "cmaf.audio_reencoder",
            crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
            { reenc.encode_planar(&planar, au_pts, sr, ch) }
        ) {
            Ok(out) => frames_to_buffer.extend(out),
            Err(e) => {
                tracing::warn!(
                    "CMAF output '{}': non-AAC audio re-encode failed: {e}",
                    config.id,
                );
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::AUDIO_ENCODE,
                    format!("CMAF output '{}': audio_encode error: {e}", config.id),
                    flow_id,
                );
            }
        }
    }
    buffer_audio_frames(state, config, frames_to_buffer);
}

#[cfg(not(feature = "media-codecs"))]
fn handle_other_audio_frame(
    _state: &mut CmafState,
    _config: &CmafOutputConfig,
    _stream_type: u8,
    _data: &[u8],
    _pts: u64,
    _event_sender: &EventSender,
    _flow_id: &str,
) {
    // No libavcodec bridge in this build; non-AAC sources cannot be
    // decoded → CMAF audio drops cleanly.
}

#[allow(clippy::too_many_arguments)]
async fn handle_video(
    state: &mut CmafState,
    demuxer: &TsDemuxer,
    codec: VideoCodec,
    nalus: Vec<Vec<u8>>,
    pts: u64,
    is_keyframe: bool,
    config: &CmafOutputConfig,
    base_url: &str,
    init_url: &str,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    stats: &OutputStatsAccumulator,
    event_sender: &EventSender,
    flow_id: &str,
    recv_time_us: u64,
) {
    // Passthrough: the samples we forward ARE the source stream, so the
    // demuxer's cached parameter sets describe them and the track can be built
    // now. With `video_encode` the samples are the re-encoder's output, whose
    // SPS/PPS differ (resolution, profile, level, coding tools) — building the
    // track from the source sets would hand the decoder parameter sets that do
    // not describe the bitstream, which decodes as macroblock garbage rather
    // than failing cleanly. Defer until the re-encoder has emitted its own.
    // A change of codec family is not a change of parameter sets: the init's
    // sample entry (`avc1` / `hvc1`) is committed with the track list, and a
    // browser needs `changeType()` to move between them, which no playlist
    // tag asks for. The samples are dropped — packing HEVC under an H.264
    // track decodes as garbage — and the operator is told once what the
    // remedy is. See docs/cmaf.md, Known limitations.
    if state.video_reencoder.is_none()
        && let Some(seg) = state.video_seg.as_ref()
        && seg.track.codec != codec
    {
        if !state.family_change_warned {
            state.family_change_warned = true;
            tracing::warn!(
                "CMAF output '{}': the source changed codec family ({:?} -> {:?}); the \
                 init has already declared {:?}, so the new samples are not published — \
                 restart the output to follow the source",
                config.id, seg.track.codec, codec, seg.track.codec,
            );
            event_sender.emit_flow(
                EventSeverity::Warning,
                category::CMAF,
                format!(
                    "CMAF output '{}': the source changed codec family to {codec:?}; \
                     restart the output to follow it",
                    config.id
                ),
                flow_id,
            );
        }
        return;
    }
    if state.video_reencoder.is_none()
        && !ensure_video_segmenter(
            state.resume_seq,
            state.init_generation,
            &mut state.video_seg,
            codec,
            demuxer,
            is_keyframe,
            config.segment_duration_secs,
            &config.id,
        )
    {
        return;
    }

    // Phase 3: video_encode hooks here. For passthrough we forward the
    // NALs directly to the segmenter; with `video_encode`, we run them
    // through the re-encoder (block_in_place around the codec call) and
    // forward the encoded NALs instead.
    let pushed_nalus: Vec<Vec<u8>>;
    let pushed_is_keyframe: bool;
    if let Some(reenc) = state.video_reencoder.as_mut() {
        let recoded = crate::timed_block_in_place!(
            "cmaf.video_reencoder",
            crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
            { reenc.encode_frame(&nalus, pts, is_keyframe, codec) }
        );
        match recoded {
            Ok(Some(out)) => {
                pushed_nalus = out.nalus;
                pushed_is_keyframe = out.is_keyframe;
            }
            Ok(None) => return, // encoder buffered the frame
            Err(e) => {
                tracing::warn!(
                    "CMAF output '{}': video re-encode failed: {e}",
                    config.id,
                );
                return;
            }
        }
    } else {
        pushed_nalus = nalus;
        pushed_is_keyframe = is_keyframe;
    }

    // The re-encoder is opened with `global_header = false`, so every IDR it
    // emits carries its SPS/PPS in-band. Those are the parameter sets that
    // actually describe these samples, so the track is built from them. They
    // are filtered back out of the frame before packing (`filter_frame_nalus_*`),
    // exactly as for passthrough.
    if state.video_reencoder.is_some() && state.video_seg.is_none() {
        // ...and parsed as the codec the ENCODER emits, which need not be the
        // source's. `x265` fed an H.264 source emits HEVC, whose parameter
        // sets are NAL types 32/33/34 rather than 7/8. Reading them as H.264
        // finds nothing, so the track is never built, every frame returns
        // here, and the output publishes no init and no segments at all —
        // silently, since nothing has failed.
        let encoded_codec = config
            .video_encode
            .as_ref()
            .and_then(|e| encode::encoded_codec_family(&e.codec))
            .unwrap_or(codec);
        if !ensure_video_segmenter_from_nalus(
            state.resume_seq,
            state.init_generation,
            &mut state.video_seg,
            encoded_codec,
            &pushed_nalus,
            config.segment_duration_secs,
            &config.id,
        ) {
            // No parameter sets yet — wait for the encoder's first IDR.
            return;
        }
    }

    let outcome: PushOutcome = state
        .video_seg
        .as_mut()
        .map(|s| s.push(&pushed_nalus, pts, pushed_is_keyframe))
        .unwrap_or(PushOutcome {
            completed_video: None,
            new_segment_started: false,
            completed_video_samples: None,
        });

    // ── LL-CMAF chunked streaming path ──────────────────────────────
    //
    // When `low_latency` is enabled we open a chunked PUT at the start
    // of each segment and emit moof+mdat chunks every
    // `chunk_duration_ms` of buffered samples. The chunk emission runs
    // inside the broadcast subscriber loop with `try_send` + drop-on-
    // full backpressure, so slow ingests can never stall the flow.
    //
    // In LL mode we RETURN early after chunked emission — we never
    // fall through to the whole-segment upload path below. When a new
    // segment starts we close the existing PUT and open a new one; the
    // manifest row gets appended at that point.
    if config.low_latency {
        handle_ll_cmaf(
            state,
            &outcome,
            config,
            base_url,
            init_url,
            init_name,
            m3u8_url,
            mpd_url,
            publish_hls,
            publish_dash,
            stats,
            event_sender,
            flow_id,
            recv_time_us,
        )
        .await;
        return;
    }

    // The clock is read *here*: after `push()` cut the segment a few
    // microseconds ago, and before anything on this path can wait on the
    // origin. It is only used when a segment actually closed, below.
    //
    // `date_closed_segment` reads its `now` as the instant the segment closed —
    // it subtracts the segment's length and its media position from it to imply
    // the flow's epoch. Sampling after the upload instead, which is what this
    // did, folded the origin's response time into that epoch. The upload
    // client's request timeout is 30 s (`upload.rs`), so a slow-but-*successful*
    // PUT could hand the clock a sample past `EPOCH_REANCHOR_SECS` and
    // re-anchor a timeline that never moved: a real `#EXT-X-DISCONTINUITY` on
    // both renditions and a real jump in the published dates, caused by nothing
    // but a busy origin. Modelled, a 10 s stall produces two tags — the stalled
    // segment and the one after it — before the clock settles.
    //
    // Above `publish_init_if_due` rather than below it for the same reason:
    // init.mp4 is republished every 30 s, and on an origin that is failing
    // those uploads the retry is a full request timeout parked directly between
    // the cut and the sample.
    //
    // It also takes the origin's latency out of every *ordinary* sample, where
    // the filter had been absorbing it, and stops two renditions publishing to
    // different origins disagreeing by the difference in their response times.
    let closed_at = chrono::Utc::now();

    // The init is published — first time, periodic republish, or a new
    // generation — AFTER the segment this push closed has entered the
    // window, never before. Everything the publish does to the window
    // assumes the closing segment is in it: a rotation retires the
    // generation that segment was cut under and forgets or drops what no
    // row names, and a restart's mismatch relabels this run's rows. Run
    // first, it saw a window one row short and got all three wrong.
    let Some(seg) = outcome.completed_video else {
        publish_init_if_due(
            state,
            config,
            init_url,
            InitEncryption::AsConfigured,
            AudioPolicy::MuxWhenPresent,
            event_sender,
            flow_id,
        )
        .await;
        return;
    };
    {
        // (Removed: a re-publish of init.mp4 to add an audio track once one
        // was detected. It existed to widen the moov after the fact, but the
        // track it added was never filled by any fragment — see the note at
        // the first init upload above. Restore it alongside audio muxing.)

        // Apply CENC by rebuilding the segment from the raw Sample
        // vector with per-sample encryption applied.
        let (segment_bytes, segment_kind, duration_90k) = if state.cenc.is_some()
            && let Some(completed) = outcome.completed_video_samples.as_ref()
        {
            encrypt_and_build_video_segment(state, &seg, completed)
            .map(|b| (b, SegmentKind::Video, seg.duration_90k))
            .unwrap_or((seg.bytes, seg.kind, seg.duration_90k))
        } else if state.audio_muxing == Some(true) {
            // Falls back to the video-only bytes when this segment happens to
            // have no audio buffered (a gap in the source, or the very first
            // segment). That stays playable: the fragment simply carries no
            // run for the audio track, which is legal and which MSE handles
            // as a gap rather than a failure.
            build_muxed_segment_for_seq(state, &seg, outcome.completed_video_samples.as_ref())
                .unwrap_or((seg.bytes, seg.kind, seg.duration_90k))
        } else {
            (seg.bytes, seg.kind, seg.duration_90k)
        };

        let uri = match segment_kind {
            SegmentKind::Audio => format!("aud-{:05}.m4s", seg.sequence_number),
            _ => default_segment_uri(seg.sequence_number),
        };
        let seg_url = format!("{base_url}/{uri}");
        let seg_bytes_len = segment_bytes.len() as u64;

        match http_put(&seg_url, segment_bytes, "video/mp4", config.auth_token.as_deref()).await {
            Ok(_) => {
                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                stats.bytes_sent.fetch_add(seg_bytes_len, Ordering::Relaxed);
                stats.record_latency(recv_time_us);
                let dur_s = (duration_90k as f64 / 90_000.0).max(0.001);
                let bps = ((seg_bytes_len as f64 * 8.0) / dur_s) as u64;
                state.video_bps_ewma = if state.video_bps_ewma == 0 {
                    bps
                } else {
                    (state.video_bps_ewma * 3 + bps) / 4
                };
                tracing::debug!(
                    "CMAF output '{}': uploaded {} ({} bytes)",
                    config.id, uri, seg_bytes_len,
                );
            }
            Err(e) => {
                tracing::warn!(
                    "CMAF output '{}': segment {} upload failed: {e}",
                    config.id, uri,
                );
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::CMAF,
                    format!("CMAF output '{}': segment upload failed: {e}", config.id),
                    flow_id,
                );
                // A segment the origin does not hold is not advertised. The
                // rotation the same IDR may have applied is not lost with it:
                // the segmenter carries the generation, and the init publish
                // below reads it from there.
                publish_init_if_due(
                    state,
                    config,
                    init_url,
                    InitEncryption::AsConfigured,
                    AudioPolicy::MuxWhenPresent,
                    event_sender,
                    flow_id,
                )
                .await;
                return;
            }
        }

        if state.availability_start_unix == 0 {
            state.availability_start_unix = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
        }

        let seg_secs = duration_90k as f64 / 90_000.0;
        // From the media timeline, through the flow's shared epoch — see
        // `segment_date_marking`. Sampling the wall clock here instead put the
        // scheduling and pipeline delay into the tag, and because only one
        // date is written per playlist it anchored the whole window on that
        // one sample.
        let (pdt, discontinuity) = segment_date_marking(
            flow_id,
            seg.base_dts_90k,
            seg_secs,
            closed_at,
        );
        // Taken whatever the clock said, or a clock-reported break on the
        // first own row would leave the restore's flag armed for the next.
        let restored_join = std::mem::take(&mut state.restore_discontinuity);
        // Against the last row actually in the window, not against the
        // segmenter's mark: a mark rides on the segment that opened the
        // generation, and if that segment's upload failed it went out with
        // it, leaving the next row to change the map with no tag.
        let generation_break = state.generation_break(seg.generation);
        state.playlist.push_back(M3u8Entry {
            sequence_number: seg.sequence_number,
            duration_secs: seg_secs,
            uri: Some(uri),
            parts: Vec::new(),
            // From the segment, not from the output's state: the segment
            // that closes at a parameter-set change is the last of the OLD
            // generation, and the state moves on only when the init publish
            // below adopts the new one. Generation 0 leaves this `None`, so
            // a stream whose encoder never changed writes exactly the
            // playlist it always did.
            init_uri: (seg.generation > 0).then(|| init_object_name(seg.generation)),
            program_date_time: Some(pdt),
            // The first row after a restored window always breaks the
            // timeline, whatever the flow clock believes: it was built fresh
            // with this process and has nothing to compare against. So does
            // the first row of a new generation: different parameter sets
            // are a break in the decode chain, and a player that carries its
            // decoder across one gets garbage.
            discontinuity: discontinuity
                || restored_join
                || seg.first_of_generation
                || generation_break,
        });
        state.trim_playlist(config.playlist_window_segments());

        // Publish init.mp4 the first time a video track is materialised, and
        // republish it periodically thereafter — with the closed row in the
        // window, see above.
        publish_init_if_due(
            state,
            config,
            init_url,
            InitEncryption::AsConfigured,
            AudioPolicy::MuxWhenPresent,
            event_sender,
            flow_id,
        )
        .await;

        // Every playlist names `init.mp4` in `#EXT-X-MAP`, so publishing one
        // before that object exists hands players a manifest they can fetch,
        // parse and then fail on. The first init can be up to
        // `AUDIO_DETECT_GRACE` behind the first segment, because the track
        // list cannot be committed until it is known whether the source has
        // audio. Segments are uploaded meanwhile — they are what the first
        // playlist will list.
        if state.init_uploaded {
            publish_manifests(
                state,
                config,
                init_name,
                m3u8_url,
                mpd_url,
                publish_hls,
                publish_dash,
                seg.sequence_number,
                event_sender,
                flow_id,
            )
            .await;
        }
    }
}

/// Buffer AAC frames for the muxed fragment, reporting any the segmenter had
/// to shed.
///
/// Shedding means the video track has not closed a segment in four segment
/// durations — no IDR, or no video at all. Nothing is being published in that
/// state, so the audio is genuinely dead, but it is a real symptom and must
/// not be dropped on the floor the way the return value used to be.
fn buffer_audio_frames<I: IntoIterator<Item = (Vec<u8>, u64)>>(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    frames: I,
) {
    // The track list is committed at the first `init.mp4` and a browser builds
    // its decoders from that file once, so audio arriving after it cannot be
    // adopted — the moov would have to widen under a player already running.
    // Say so, once: the operator's only remedy is a flow restart, and the sole
    // other signal is a missing " + audio" in a log line long since scrolled
    // past. docs/cmaf.md promises this warning.
    if state.audio_muxing == Some(false) && !state.late_audio_warned {
        state.late_audio_warned = true;
        // Say which of the three reasons applies, because only one of them is
        // worth restarting for. `low_latency` and `encryption` are structural:
        // those paths cannot carry audio at all, and telling an operator to
        // restart a flow sends them round a loop that ends where it started.
        let remedy = if config.low_latency {
            "low_latency outputs carry one track per chunk, so this output cannot carry audio \
             at all — use low_latency = false if the audio matters more than the latency"
        } else if state.cenc.is_some() {
            "encrypted outputs are video-only (audio encryption is unwired), so this output \
             cannot carry audio at all"
        } else {
            "the track list is committed at the first init.mp4 and a browser builds its \
             decoders from it once — restart the flow to pick the audio up"
        };
        tracing::warn!(
            "CMAF output '{}': audio appeared after init.mp4 committed the track list — it \
             will not be carried. {}.",
            config.id,
            remedy,
        );
    }
    let Some(seg) = state.audio_seg.as_mut() else {
        return;
    };
    let mut shed_90k = 0u64;
    for (data, pts) in frames {
        if let Some(dropped) = seg.push(&data, pts) {
            shed_90k += dropped.duration_90k;
        }
    }
    if shed_90k > 0 {
        tracing::warn!(
            "CMAF output '{}': shed {} ms of buffered audio — the video track \
             has not closed a segment, so there is no fragment to carry it",
            config.id,
            shed_90k / 90,
        );
    }
}

fn handle_audio_frame(
    state: &mut CmafState,
    demuxer: &TsDemuxer,
    config: &CmafOutputConfig,
    data: &[u8],
    pts: u64,
    event_sender: &EventSender,
    flow_id: &str,
) {
    let bitrate = config
        .audio_encode
        .as_ref()
        .and_then(|e| e.bitrate_kbps)
        .map(|k| k * 1000)
        .unwrap_or(128_000);

    // Passthrough: the track is the source's, from the demuxer-cached ADTS
    // config. With `audio_encode` it is the ENCODER's — built below, after
    // the first frame has settled what the encoder actually emits. A source
    // whose rate or layout differs from the configured target is converted
    // on the way in, so a track built from the source declared one channel
    // count (or rate) while every frame carried another, which a browser's
    // decoder refuses as a mid-stream change.
    if state.audio_seg.is_none() && state.audio_reencoder.is_none() {
        let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() else {
            return;
        };
        let asc = aac_audio_specific_config(profile, sr_idx, ch_cfg);
        let sample_rate = codecs::sample_rate_from_index(sr_idx);
        let track = AudioTrack::aac(asc, sample_rate, ch_cfg as u16, bitrate);
        state.audio_seg =
                            Some(AudioSegmenter::new_from_seq(track, config.segment_duration_secs, state.resume_seq));
        state.audio_ready = true;
        tracing::info!(
            "CMAF output '{}': audio track detected AAC sr={} ch={}",
            config.id, sample_rate, ch_cfg,
        );
    }

    // Phase 3: audio_encode hook — pump the source AAC frame through
    // the AudioReencoder and substitute the re-encoded frame(s).
    let video_at = state.video_seg.as_ref().and_then(VideoSegmenter::last_dts_90k);
    let frames_to_buffer: Vec<(Vec<u8>, u64)> = if let Some(reenc) = state.audio_reencoder.as_mut() {
        // Reset the silent-fallback drop watchdog — real audio is flowing —
        // and let it measure the picture's lead over the audio.
        reenc.mark_real_audio(pts, video_at);
        // Propagate the ADTS triplet from the demuxer so lazy decoder
        // construction inside AudioReencoder can succeed.
        if let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() {
            reenc.set_adts_config(profile, sr_idx, ch_cfg);
        }
        let out = match crate::timed_block_in_place!(
            "cmaf.audio_reencoder",
            crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
            { reenc.encode_aac_frame(data, pts) }
        ) {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!("CMAF output '{}': audio re-encode failed: {e}", config.id);
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::AUDIO_ENCODE,
                    format!("CMAF output '{}': audio_encode error: {e}", config.id),
                    flow_id,
                );
                Vec::new()
            }
        };
        // The encoder has now settled its target from the decoder's real
        // output — SBR-doubled rate and PS-widened layout included — and the
        // track is built from that, so the init describes the frames.
        if state.audio_seg.is_none()
            && let Some((profile, sr_idx, ch)) = reenc.encoder_track()
        {
            let sample_rate = codecs::sample_rate_from_index(sr_idx);
            let track =
                AudioTrack::aac(aac_audio_specific_config(profile, sr_idx, ch), sample_rate, ch as u16, bitrate);
            state.audio_seg =
                Some(AudioSegmenter::new_from_seq(track, config.segment_duration_secs, state.resume_seq));
            state.audio_ready = true;
            tracing::info!(
                "CMAF output '{}': audio track re-encoded to AAC sr={} ch={}",
                config.id, sample_rate, ch,
            );
        }
        out
    } else {
        vec![(data.to_vec(), pts)]
    };

    buffer_audio_frames(state, config, frames_to_buffer);
}

/// Encrypt `samples` in place and re-build the video segment with
/// senc/saio/saiz.
fn encrypt_and_build_video_segment(
    state: &mut CmafState,
    seg: &CompletedSegment,
    snapshot: &(u64, u64, Vec<fmp4::Sample>),
) -> Option<Vec<u8>> {
    use fmp4::{Sample as FSample, VideoCodec};
    let cenc_rt = state.cenc.as_mut()?;
    // The segment's, not the live track's: a rotation applied by the push
    // that cut this segment has already swapped the track, and the NAL
    // grammar the subsample split is computed under must be the one these
    // samples were filtered with.
    let v_track_codec = seg.codec;
    let (_seq, base, samples) = snapshot;
    let mut mutable: Vec<FSample> = samples.clone();
    let mut cenc_info = Vec::with_capacity(mutable.len());
    for s in mutable.iter_mut() {
        let info = cenc_rt
            .encryptor
            .encrypt_video_sample(&mut s.data, match v_track_codec {
                VideoCodec::H264 => cenc::VideoCodec::H264,
                VideoCodec::H265 => cenc::VideoCodec::H265,
            });
        cenc_info.push(info);
    }
    Some(fmp4::build_encrypted_media_segment(
        fmp4::VIDEO_TRACK_ID,
        seg.sequence_number as u32,
        *base,
        &mutable,
        &cenc_info,
        cenc_rt.scheme,
    ))
}

/// Build a muxed segment containing both video samples (already in
/// `seg.bytes` is video-only — we discard it and rebuild) and the
/// audio frames buffered up to the boundary. Returns `None` if the
/// caller should fall back to the video-only `seg.bytes`.
fn build_muxed_segment_for_seq(
    state: &mut CmafState,
    seg: &CompletedSegment,
    video_samples: Option<&(u64, u64, Vec<Sample>)>,
) -> Option<(Vec<u8>, SegmentKind, u64)> {
    let (v_seq, v_base, v_samples) = video_samples?;
    let (v_seq, v_base) = (*v_seq, *v_base);
    let a_seg = state.audio_seg.as_mut()?;

    // Convert video segment boundary DTS (90 kHz) into the audio
    // track's timescale.
    let boundary_video_dts = seg.base_dts_90k + seg.duration_90k;
    let boundary_audio_ts = boundary_video_dts * a_seg.track.sample_rate as u64 / 90_000;

    // Take whatever audio is buffered for this segment's span.
    let (_a_seq, a_base, a_samples) = a_seg.take_pending_samples(boundary_audio_ts)?;
    if a_samples.is_empty() {
        return None;
    }

    // Rebuild the fragment with both tracks. `seg.bytes` is a video-only
    // fMP4 and is discarded — the video samples that built it come back
    // through `PushOutcome::completed_video_samples`, so nothing has to be
    // re-parsed out of the serialised segment.
    //
    // The video sequence number is reused for the muxed fragment so the
    // fragment number keeps matching the segment number the playlist lists;
    // the audio segmenter's own counter is irrelevant here and dropped.
    let bytes = fmp4::build_muxed_segment(
        v_seq as u32,
        v_base,
        v_samples,
        a_base,
        &a_samples,
    );
    Some((bytes, SegmentKind::Muxed, seg.duration_90k))
}

/// Build the video track from parameter sets carried in an encoded frame.
///
/// Used for the `video_encode` path, where [`ensure_video_segmenter`]'s
/// demuxer-cached sets describe the *source* rather than what is being written.
/// Returns false until a frame carrying the sets shows up, which for a
/// `global_header = false` encoder is its first IDR.
fn ensure_video_segmenter_from_nalus(
    resume_seq: u64,
    resume_generation: u32,
    slot: &mut Option<VideoSegmenter>,
    codec: VideoCodec,
    nalus: &[Vec<u8>],
    segment_duration_secs: f64,
    output_id: &str,
) -> bool {
    if slot.is_some() {
        return true;
    }
    let track = match codec {
        VideoCodec::H264 => {
            let mut sps = None;
            let mut pps = None;
            for n in nalus {
                match nalu::h264_nal_type(n) {
                    7 => sps.get_or_insert_with(|| n.clone()),
                    8 => pps.get_or_insert_with(|| n.clone()),
                    _ => continue,
                };
            }
            match (sps, pps) {
                (Some(s), Some(p)) => VideoTrack::from_h264(s, p),
                _ => return false,
            }
        }
        VideoCodec::H265 => {
            let mut vps = None;
            let mut sps = None;
            let mut pps = None;
            for n in nalus {
                match nalu::h265_nal_type(n) {
                    32 => vps.get_or_insert_with(|| n.clone()),
                    33 => sps.get_or_insert_with(|| n.clone()),
                    34 => pps.get_or_insert_with(|| n.clone()),
                    _ => continue,
                };
            }
            match (vps, sps, pps) {
                (Some(v), Some(s), Some(p)) => VideoTrack::from_h265(v, s, p),
                _ => return false,
            }
        }
    };
    tracing::info!(
        "CMAF output '{}': video track from re-encoder {:?} {}x{}",
        output_id, track.codec, track.width, track.height,
    );
    let mut seg = VideoSegmenter::new_from_seq(track, segment_duration_secs, resume_seq);
    seg.set_generation(resume_generation);
    *slot = Some(seg);
    true
}

/// How often `init.mp4` is re-published.
///
/// It is ~1 KB, so the cost is negligible next to the media, and it bounds how
/// long an origin that lost it stays broken.
const INIT_REPUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long to wait before retrying a *failed* init.mp4 upload.
///
/// Shorter than the republish interval because until the first one lands the
/// output is unplayable, and much longer than a frame interval because the
/// attempt is driven from the video path: without a floor here an origin that
/// is down produces a PUT per frame — 50 a second at 1080p50, indefinitely.
const INIT_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Should `init.mp4` be published now — never published, or due a refresh?
fn init_publish_due(state: &CmafState) -> bool {
    match state.init_last_upload {
        None => true,
        Some(t) => {
            let interval = if state.init_upload_failing {
                INIT_RETRY_INTERVAL
            } else {
                INIT_REPUBLISH_INTERVAL
            };
            t.elapsed() >= interval
        }
    }
}

/// How long the first `init.mp4` waits for an audio track to appear before
/// committing to a video-only output.
///
/// Video and audio materialise independently: video needs an IDR (plus, on
/// the re-encode path, the encoder's first in-band SPS/PPS), audio needs one
/// frame carrying the codec config. Either can be first. Publishing the
/// instant video is ready would declare video-only for a source that does
/// have audio — and because the track list is latched, that choice would
/// stick for the life of the flow.
///
/// A segment is typically 2 s, so this costs at most one segment of startup
/// and only when the source turns out to be silent.
const AUDIO_DETECT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Decide, once, whether this output carries audio.
///
/// Returns `None` while the answer is still genuinely unknown — the caller
/// must hold off publishing `init.mp4` until it resolves.
fn resolve_audio_muxing(state: &mut CmafState) -> Option<bool> {
    if let Some(decided) = state.audio_muxing {
        return Some(decided);
    }
    // CENC first, before "audio exists": an encrypted output takes the
    // `encrypt_and_build_video_segment` branch, which rebuilds the fragment
    // from the video samples alone and never carries an audio run. Settling
    // on "audio is here" would declare a track those fragments cannot fill —
    // the #130 stall, on exactly the outputs that are hardest to debug.
    // (`encrypt_audio_sample` is written but unwired, so audio cannot be
    // carried encrypted either.)
    if state.cenc.is_some() {
        state.audio_muxing = Some(false);
        return Some(false);
    }
    // Audio is here — settle immediately, no reason to wait out the grace.
    if state.audio_seg.is_some() {
        state.audio_muxing = Some(true);
        return Some(true);
    }
    match state.video_ready_at {
        Some(t) if t.elapsed() >= AUDIO_DETECT_GRACE => {
            state.audio_muxing = Some(false);
            Some(false)
        }
        Some(_) => None,
        // No video track yet; the caller is not publishing init anyway.
        None => None,
    }
}

/// Whether this output's `init.mp4` may declare an audio track.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioPolicy {
    /// Carry audio if the source has any — the whole-segment path, whose
    /// fragments are built by `build_muxed_segment` and really do address
    /// both tracks.
    MuxWhenPresent,
    /// Never. The low-latency path emits chunks through
    /// `build_segment_chunk`, which writes a single traf for the video track
    /// and no audio run at all. An init declaring audio there is the #130
    /// stall exactly: MSE builds an audio decoder and waits forever for data
    /// no chunk carries, with nothing reporting an error.
    Never,
}

/// Whether this output's `init.mp4` may declare Common Encryption.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InitEncryption {
    /// Follow `state.cenc` — the whole-segment path, which really does encrypt
    /// the fragments it writes.
    AsConfigured,
    /// Never, whatever `state.cenc` says. The low-latency path emits chunks
    /// through `build_segment_chunk`, which writes no senc/saiz/saio, so its
    /// media is in the clear regardless. An init declaring encryption would
    /// describe bytes this path does not send, which is worse than the
    /// existing gap: a player would fail on the first chunk instead of
    /// playing it. LL-CMAF ignoring `encryption` is a real defect, tracked
    /// separately — closing it means encrypting the chunks, not the init.
    Never,
}

/// Publish `init.mp4` when it is due, from either the whole-segment or the
/// low-latency path.
///
/// Returns whether an init has landed at least once, i.e. whether media may
/// usefully be published.
///
/// One function for both paths on purpose. They had separate copies, and the
/// two fixes this exists for — the periodic republish, and dropping the audio
/// track no fragment fills — were applied to one copy only, which is exactly
/// the browser-stalls-silently failure the second fix is about.
#[allow(clippy::too_many_arguments)]
async fn publish_init_if_due(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    init_url: &str,
    encryption: InitEncryption,
    audio: AudioPolicy,
    event_sender: &EventSender,
    flow_id: &str,
) -> bool {
    // Before the due check: a rotation clears the publish clock, and it has
    // to, because the manifest publish is gated on `init_uploaded` and a row
    // cut under the new generation must not be advertised before the init it
    // names is on the origin.
    adopt_generation(state, &config.id);
    if !init_publish_due(state) {
        return state.init_uploaded;
    }
    if state.video_seg.is_none() {
        return state.init_uploaded;
    }
    // Start the audio-detection clock the moment a video track exists.
    if state.video_ready_at.is_none() {
        state.video_ready_at = Some(std::time::Instant::now());
    }

    // The track list here and the tracks the fragments actually carry must
    // agree exactly, in both directions. Declaring an audio track no fragment
    // fills stalls MSE silently — it initialises the track, waits forever for
    // data that never comes, buffers nothing and reports no error, while the
    // manifest, the segments and the origin all look healthy (#130). Sending
    // audio the init never declared fails just as quietly. `audio_muxing` is
    // the single latched answer both sides read.
    let with_audio = match audio {
        AudioPolicy::Never => {
            // Latch it, so the segment path cannot decide otherwise later.
            state.audio_muxing = Some(false);
            false
        }
        AudioPolicy::MuxWhenPresent => match resolve_audio_muxing(state) {
            Some(decided) => decided,
            // Still inside the grace window with no audio yet: the answer is
            // genuinely unknown and latching it now would stick for the life
            // of the flow. Wait; nothing else depends on init existing yet.
            None => return state.init_uploaded,
        },
    };
    let first = !state.init_uploaded;

    // `first` is true again after a rotation, but the answer has not changed
    // and was given the first time.
    if first
        && !with_audio
        && !state.late_audio_warned
        && (state.audio_seg.is_some() || state.audio_ready)
    {
        // Audio exists but is not being carried, and the reason is structural
        // — it will not change for the life of the flow. (Audio that turns up
        // *later* is reported from the audio path, which is the only place
        // that can see it: this block runs once.)
        let why = match audio {
            AudioPolicy::Never => "low_latency chunks carry a single track",
            AudioPolicy::MuxWhenPresent => {
                "CENC encrypts video only, and shipping audio in the clear \
                 under an init that declares the output encrypted is worse"
            }
        };
        state.late_audio_warned = true;
        tracing::warn!(
            "CMAF output '{}': source has audio but the output is video-only — {}.",
            config.id,
            why,
        );
    }

    // Build before touching `state` again: the track borrow has to end before
    // the upload result can be recorded.
    let (init_bytes, width, height, track_codec) = {
        let v = state
            .video_seg
            .as_ref()
            .expect("video_seg checked immediately above");
        let audio_track = if with_audio {
            state.audio_seg.as_ref().map(|a| &a.track)
        } else {
            None
        };
        let cenc = match encryption {
            InitEncryption::AsConfigured => state.cenc.as_ref(),
            InitEncryption::Never => None,
        };
        let bytes = if let Some(c) = cenc {
            let params = fmp4::CencInitParams {
                scheme: c.scheme,
                key_id: &c.key_id,
                extra_pssh: c.extra_pssh.clone(),
            };
            fmp4::build_encrypted_init_segment(&v.track, audio_track, &params)
        } else {
            fmp4::build_init_segment(&v.track, audio_track)
        };
        (bytes, v.track.width, v.track.height, v.track.codec)
    };

    // Does the restored window still describe media this init can decode?
    //
    // The restored rows were published under the previous run's track list,
    // sample entries and parameter sets. Nothing used to compare them with
    // this run's, so any track or parameter change across a restart left the
    // whole restored history described by an init that did not match it: a
    // muxed fragment under a video-only init, or the reverse, which is the
    // failure MSE answers by initialising the declared track and then
    // waiting for ever with nothing wrong on the wire (#130). An
    // `h264`→`h265` edit makes `appendBuffer` throw outright, and a
    // resolution change puts a new SPS in the `avcC`.
    //
    // The triggers are ordinary operator edits that both restart the output
    // and change the tracks — toggling `low_latency`, adding or removing
    // `audio_encode`, changing the video codec or resolution, enabling
    // encryption — plus the audio-detection race, which can latch differently
    // on two runs of the same config.
    //
    // So the init's identity is published with the manifest and read back
    // with it. On a mismatch this run's init opens a new generation: it goes
    // up under a new name, the restored rows keep the one they decode
    // against, and the join is a discontinuity. The first version dropped
    // the restored rows instead, because `init.mp4` was one fixed object
    // about to be overwritten.
    let fingerprint = init_fingerprint(&init_bytes);
    if state.restored_init_differs(&fingerprint) == Some(true) {
        let bumped = state.video_seg.as_mut().map(|seg| {
            let from = seg.generation();
            seg.bump_generation();
            (from, seg.generation())
        });
        if let Some((from, to)) = bumped {
            state.relabel_own_rows(from, to);
            // The low-latency segment open now was opened under `from`
            // before this comparison could run, and has carried nothing yet
            // — chunks wait for the init this publish is about to put up.
            if let Some(ll) = state.ll_current.as_mut()
                && ll.generation == from
            {
                ll.generation = to;
            }
            adopt_generation(state, &config.id);
        }
        tracing::warn!(
            output = %config.id, now = %fingerprint, init = %state.init_uri,
            "CMAF output: this run's init does not describe the window the origin \
             was serving; publishing it as a new generation so the restored rows \
             keep the init they decode against"
        );
        event_sender.emit_flow(
            EventSeverity::Warning,
            category::CMAF,
            format!(
                "CMAF output '{}': the tracks changed across the restart — the DVR \
                 window is kept, and playback across the join is a discontinuity",
                config.id
            ),
            flow_id,
        );
    }
    state.init_fingerprint = Some(fingerprint);
    // The restored generation's bytes are needed only if this run will not
    // put that name up itself — after a mismatch, or after a rotation that
    // moved on before the first publish — and the restored rows still name
    // it. Keyed on the name, which both cases have settled by now.
    if let Some(kept) = state.restored_current_init.take()
        && kept.uri != state.init_uri
    {
        state.init_history.push(kept);
    }

    // A rotated init is a NEW object, not an overwrite. Overwriting would
    // strand every segment already in the window: their media decodes against
    // the old parameter sets, and the playlist still points them at that name.
    let init_base = init_url.rsplit_once('/').map(|(base, _)| base).unwrap_or("");
    let versioned_url = format!("{init_base}/{}", state.init_uri);
    let published = init_bytes.clone();

    match http_put(&versioned_url, init_bytes, "video/mp4", config.auth_token.as_deref()).await {
        Ok(_) => {
            state.init_uploaded = true;
            state.init_upload_failing = false;
            state.init_last_upload = Some(std::time::Instant::now());
            state.last_init_bytes = Some(published);
            if first {
                tracing::info!(
                    "CMAF output '{}': uploaded {} ({}x{}, {:?}{})",
                    config.id,
                    state.init_uri,
                    width,
                    height,
                    track_codec,
                    if with_audio { " + audio" } else { "" },
                );
            }
            // The older generations the window still names, on the same
            // cadence and for the same reason: an origin that lost its store
            // has lost them too, and a viewer seeking back into those rows
            // would 404 on their map while the live edge plays on.
            for held in &state.init_history {
                let url = format!("{init_base}/{}", held.uri);
                if let Err(e) =
                    http_put(&url, held.bytes.clone(), "video/mp4", config.auth_token.as_deref())
                        .await
                {
                    tracing::warn!(
                        "CMAF output '{}': republish of {} failed: {e}",
                        config.id, held.uri,
                    );
                }
            }
        }
        Err(e) => {
            // Stamp the attempt even though it failed. Without it
            // `init_publish_due` stays true and the next video frame retries
            // immediately — a PUT and a manager event per frame, for as long
            // as the origin is unreachable.
            state.init_last_upload = Some(std::time::Instant::now());
            tracing::warn!("CMAF output '{}': init.mp4 upload failed: {e}", config.id);
            if !state.init_upload_failing {
                state.init_upload_failing = true;
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::CMAF,
                    format!("CMAF output '{}': init upload failed: {e}", config.id),
                    flow_id,
                );
            }
        }
    }
    state.init_uploaded
}

/// Build the video track from the demuxer's cached parameter sets, or, once
/// it exists, notice that those sets have changed.
///
/// Returns false until the sets are available. The check for a change runs
/// only on an IDR: that is where new parameter sets take effect, a
/// non-IDR frame decodes against the sets its GOP started with whatever the
/// cache says, and comparing on every frame would rotate on a source that
/// re-sends a differing PPS between IDRs. The first IDR's sets used to be
/// taken as the stream's for life, which is true right up until an encoder
/// changes underneath — an operator pinning a codec, `h264_auto` falling
/// between NVENC and x264 — and then every sample after it decodes against
/// the wrong SPS. Browsers reject that outright: `bufferAppendError`, five
/// frames, then nothing, with the buffer filling normally the whole time.
#[allow(clippy::too_many_arguments)]
fn ensure_video_segmenter(
    resume_seq: u64,
    resume_generation: u32,
    slot: &mut Option<VideoSegmenter>,
    codec: VideoCodec,
    demuxer: &TsDemuxer,
    is_keyframe: bool,
    segment_duration_secs: f64,
    output_id: &str,
) -> bool {
    if let Some(seg) = slot.as_mut() {
        if !is_keyframe || seg.rotation_pending() {
            return true;
        }
        // Borrowed, not copied: this runs on every IDR for the life of the
        // stream, and the sets are equal on all but a handful of them.
        let changed = match codec {
            VideoCodec::H264 => match (demuxer.cached_sps(), demuxer.cached_pps()) {
                (Some(sps), Some(pps)) => sps != seg.track.sps || pps != seg.track.pps,
                _ => false,
            },
            VideoCodec::H265 => match (
                demuxer.cached_h265_vps(),
                demuxer.cached_h265_sps(),
                demuxer.cached_h265_pps(),
            ) {
                (Some(vps), Some(sps), Some(pps)) => {
                    vps != seg.track.vps || sps != seg.track.sps || pps != seg.track.pps
                }
                _ => false,
            },
        };
        if !changed {
            return true;
        }
        let track = match codec {
            VideoCodec::H264 => VideoTrack::from_h264(
                demuxer.cached_sps().map(<[u8]>::to_vec).unwrap_or_default(),
                demuxer.cached_pps().map(<[u8]>::to_vec).unwrap_or_default(),
            ),
            VideoCodec::H265 => VideoTrack::from_h265(
                demuxer.cached_h265_vps().map(<[u8]>::to_vec).unwrap_or_default(),
                demuxer.cached_h265_sps().map(<[u8]>::to_vec).unwrap_or_default(),
                demuxer.cached_h265_pps().map(<[u8]>::to_vec).unwrap_or_default(),
            ),
        };
        tracing::warn!(
            "CMAF output '{}': the source's parameter sets changed ({}x{} -> {}x{}); \
             cutting the open segment and publishing a new init so what is written \
             stays decodable",
            output_id,
            seg.track.width,
            seg.track.height,
            track.width,
            track.height,
        );
        // Applied at this IDR by `push`, which cuts the open segment first
        // so nothing already queued is described by the new sets.
        seg.rotate_track(track);
        return true;
    }
    let track = match codec {
        VideoCodec::H264 => {
            let sps = match demuxer.cached_sps() {
                Some(s) => s.to_vec(),
                None => return false,
            };
            let pps = match demuxer.cached_pps() {
                Some(p) => p.to_vec(),
                None => return false,
            };
            VideoTrack::from_h264(sps, pps)
        }
        VideoCodec::H265 => {
            let vps = match demuxer.cached_h265_vps() {
                Some(v) => v.to_vec(),
                None => return false,
            };
            let sps = match demuxer.cached_h265_sps() {
                Some(s) => s.to_vec(),
                None => return false,
            };
            let pps = match demuxer.cached_h265_pps() {
                Some(p) => p.to_vec(),
                None => return false,
            };
            VideoTrack::from_h265(vps, sps, pps)
        }
    };
    tracing::info!(
        "CMAF output '{}': video track detected {:?} {}x{}",
        output_id, track.codec, track.width, track.height,
    );
    let mut seg = VideoSegmenter::new_from_seq(track, segment_duration_secs, resume_seq);
    seg.set_generation(resume_generation);
    *slot = Some(seg);
    true
}

/// Bring the output's init bookkeeping up to the segmenter's generation.
///
/// The segmenter is the authority — it stamps every segment it cuts — and
/// this is read from it before each init publish, so the publish that
/// follows goes up under the new name and nothing is ever written over the
/// object the older rows name. Returns whether a generation was adopted.
///
/// The previous generation's bytes go into the republish set if they were
/// ever published. If they never were — the origin was unreachable from the
/// start and the encoder changed before it came back — then no manifest ever
/// listed that generation's rows, and they are dropped rather than
/// advertised under an init that will now never exist.
fn adopt_generation(state: &mut CmafState, output_id: &str) -> bool {
    let Some(generation) = state.video_seg.as_ref().map(VideoSegmenter::generation) else {
        return false;
    };
    if generation == state.init_generation {
        return false;
    }
    let previous = std::mem::replace(&mut state.init_uri, init_object_name(generation));
    if state.init_uploaded && let Some(bytes) = state.last_init_bytes.take() {
        state.init_history.push(PublishedInit { uri: previous, bytes });
    } else {
        // The segment closed at this very IDR is stamped with the retired
        // generation and is already in the window — both paths push the row
        // before the init publish runs — so this catches it too.
        let resume_seq = state.resume_seq;
        let before = state.playlist.len();
        state.playlist.retain(|r| {
            r.sequence_number < resume_seq || CmafState::row_init_name(r) != previous
        });
        let dropped = before - state.playlist.len();
        if dropped > 0 {
            tracing::warn!(
                "CMAF output '{output_id}': {dropped} segment(s) were cut under an init \
                 that never reached the origin and are dropped from the window"
            );
        }
    }
    state.init_generation = generation;
    // Force the publish rather than wait for the republish interval: until
    // the new init is up, nothing further can be advertised, and the
    // manifest publish is gated on `init_uploaded` for exactly that reason.
    state.init_uploaded = false;
    state.init_last_upload = None;
    state.last_init_bytes = None;
    tracing::info!(
        "CMAF output '{output_id}': init generation {generation}: publishing '{}'; the \
         window keeps naming the older init(s) for the rows that decode against them",
        state.init_uri,
    );
    true
}

/// The chunk that closes a low-latency segment: the samples the chunker had
/// not taken when the IDR cut it, as one more `moof`+`mdat` on the same PUT.
struct TailChunk {
    bytes: Vec<u8>,
    /// Where the tail starts on the media timeline — its `tfdt`.
    tfdt_90k: u64,
    /// How long it runs; with the chunks before it, the segment's length.
    duration_90k: u64,
    /// Whether it opens the object: true only when nothing was chunked
    /// before it, so the tail is the whole segment.
    includes_styp: bool,
}

/// Build the tail from what the cut carried. `seg.first_pending_dts_90k` is
/// the first sample still queued at the cut — the segment base when nothing
/// was chunked, the start of the tail otherwise — and the tail's durations
/// were computed against the next segment's start, so it tiles exactly to
/// the boundary.
fn ll_tail_chunk(seg: &CompletedSegment, tail: &[Sample], chunks_emitted: u32) -> TailChunk {
    let includes_styp = chunks_emitted == 0;
    TailChunk {
        bytes: fmp4::build_segment_chunk(
            fmp4::VIDEO_TRACK_ID,
            seg.sequence_number as u32,
            seg.first_pending_dts_90k,
            tail,
            includes_styp,
        ),
        tfdt_90k: seg.first_pending_dts_90k,
        duration_90k: tail.iter().map(|s| s.duration as u64).sum(),
        includes_styp,
    }
}

/// LL-CMAF chunk pump. Called once per video frame push; it opens a
/// chunked PUT at segment boundaries, flushes accumulated samples as
/// moof+mdat chunks every `chunk_duration_ms` worth of media, and
/// closes the PUT at the segment boundary.
#[allow(clippy::too_many_arguments)]
async fn handle_ll_cmaf(
    state: &mut CmafState,
    outcome: &PushOutcome,
    config: &CmafOutputConfig,
    base_url: &str,
    init_url: &str,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    stats: &OutputStatsAccumulator,
    event_sender: &EventSender,
    flow_id: &str,
    recv_time_us: u64,
) {
    // Before anything here can wait on the origin — the init republish just
    // below, and the closing PUT's `finish()` after it. `push()` cut the
    // segment a few microseconds ago in `handle_video`, and this is the instant
    // the row that closes below is dated from. See the same sample on the plain
    // path for what sampling after an upload does to the flow's epoch.
    let closed_at = chrono::Utc::now();

    // On a new segment boundary, finalise the previous LL PUT and open a new
    // one — before the init publish, whatever state the init is in. A
    // rotation whose init is still going up is a boundary like any other;
    // skipping it until the init landed left the previous segment's PUT open
    // with the new generation's samples chunked into it. And the init
    // publish wants the closing row in the window when it runs: see the
    // plain path for the three things it gets wrong otherwise.
    if outcome.new_segment_started {
        // Close previous LL segment if any.
        if let Some(mut ll) = state.ll_current.take() {
            let uri = ll.uri.clone();
            let seq = ll.sequence_number;
            let base_dts_90k = ll.base_dts_90k;
            // The tail: the samples the chunker had not taken when the IDR
            // cut the segment. `push()` snapshots them into the completed
            // segment and clears them, and this path never read that, so
            // every segment's object stopped up to one chunk short of the
            // duration its row advertised — a hole at the end of every
            // segment, 25 % of each at the default 2 s / 500 ms. With no
            // chunk emitted yet (the init landed late) the tail is the whole
            // segment, `styp` and all.
            let mut tail_stored = false;
            if let (Some(seg), Some((_, _, tail))) =
                (outcome.completed_video.as_ref(), outcome.completed_video_samples.as_ref())
                && !tail.is_empty()
            {
                let t = ll_tail_chunk(seg, tail, ll.chunks_emitted);
                tracing::trace!(
                    "CMAF output '{}': LL seg {} tail at {} for {} ticks ({} samples{})",
                    config.id,
                    seq,
                    t.tfdt_90k,
                    t.duration_90k,
                    tail.len(),
                    if t.includes_styp { ", whole segment" } else { "" },
                );
                let bytes = t.bytes;
                tail_stored = match ll.handle.send_chunk(bytes.clone()) {
                    Ok(()) => true,
                    // A PUT that died before carrying anything — opened into
                    // an origin that was down — is opened again for the
                    // same object: the tail is the whole segment, so nothing
                    // is missing from it.
                    Err(upload::ChunkSendError::Closed) if ll.chunks_emitted == 0 => {
                        ll.handle = chunked_put(
                            &format!("{base_url}/{uri}"),
                            "video/mp4",
                            config.auth_token.as_deref(),
                            8,
                        );
                        ll.handle.send_chunk(bytes).is_ok()
                    }
                    Err(_) => false,
                };
                if tail_stored {
                    ll.chunks_emitted += 1;
                }
            }
            let finish = ll.handle.finish().await;
            let stored = tail_stored && finish.is_ok();
            match finish {
                Ok(()) => {
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        "CMAF output '{}': finished LL seg {} ({})",
                        config.id, seq, uri,
                    );
                    stats.record_latency(recv_time_us);
                }
                Err(e) => {
                    tracing::warn!(
                        "CMAF output '{}': LL seg {} PUT final response error: {e}",
                        config.id, seq,
                    );
                    // Probe the origin with the 1 KB init on the next publish
                    // rather than trusting a success up to 30 s old: it lands
                    // and the reopen may proceed, or it fails and the reopen
                    // is refused, with the 1 s retry floor engaged.
                    state.init_last_upload = None;
                    // Once per episode: an origin that is down fails every
                    // close, one every segment, for as long as it is down.
                    if !state.ll_put_failing {
                        state.ll_put_failing = true;
                        event_sender.emit_flow(
                            EventSeverity::Warning,
                            category::CMAF,
                            format!("CMAF output '{}': LL PUT failed: {e}", config.id),
                            flow_id,
                        );
                    }
                }
            }
            if stored {
                state.ll_put_failing = false;
                // Where the segment ended, from the segmenter — see
                // `CmafState::closed_segment_end_dts_90k`, which is a named
                // function precisely so this derivation is reachable from a test.
                let next_base_dts_90k = state.closed_segment_end_dts_90k();
                let mut row = closed_ll_entry(
                    flow_id,
                    seq,
                    uri,
                    base_dts_90k,
                    next_base_dts_90k,
                    config.segment_duration_secs,
                    closed_at,
                    ll.generation,
                    ll.first_of_generation,
                );
                // The restore's own discontinuity, consumed here too.
                //
                // The plain path takes the flag when it builds its row; this path
                // returns above that point, so on a `low_latency` output the flag
                // was set at start and never consumed. After a *process* restart
                // the flow clock is fresh, so `segment_date_marking` has nothing to
                // compare against and answers `false` — and the restored rows are
                // parsed from a manifest whose tags this parser used to discard.
                // The republished playlist then joined the previous run's rows to a
                // timeline whose `base_dts` restarted at zero with no
                // `EXT-X-DISCONTINUITY` anywhere, which is the opposite of what
                // docs/cmaf.md promises.
                row.discontinuity |= std::mem::take(&mut state.restore_discontinuity)
                    || state.generation_break(ll.generation);
                state.playlist.push_back(row);
                state.trim_playlist(config.playlist_window_segments());
            } else {
                // A segment the origin does not hold is not advertised — the
                // plain path's rule. The restore flag stays armed for the
                // next row that is.
                tracing::warn!(
                    "CMAF output '{}': LL seg {} did not reach the origin and is not \
                     advertised",
                    config.id, seq,
                );
            }
        }
        // Open new segment.
        if let Some(vs) = state.video_seg.as_ref() {
            let seq = vs.next_segment_number();
            let uri = default_segment_uri(seq);
            let url = format!("{}/{}", base_url, uri);
            let handle =
                chunked_put(&url, "video/mp4", config.auth_token.as_deref(), 8);
            tracing::debug!(
                "CMAF output '{}': opened LL chunked PUT for seg={} ({})",
                config.id, seq, url
            );
            state.ll_current = Some(LlSegment {
                handle,
                sequence_number: seq,
                chunks_emitted: 0,
                parts: Vec::new(),
                base_dts_90k: vs.open_segment_base_dts_90k().unwrap_or(0),
                generation: vs.generation(),
                first_of_generation: vs.open_segment_first_of_generation(),
                uri,
            });
            if state.availability_start_unix == 0 {
                state.availability_start_unix = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
            }
        }
    }

    // Publish the init if it is due — first time, a periodic republish, or a
    // new generation. Chunks are meaningless to a player that cannot fetch
    // `#EXT-X-MAP`, so none is emitted until it has landed at least once; the
    // samples stay queued in the segmenter meanwhile, and the segment they
    // belong to is written whole at its close if none was chunked by then.
    let init_ready = publish_init_if_due(
        state,
        config,
        init_url,
        InitEncryption::Never,
        AudioPolicy::Never,
        event_sender,
        flow_id,
    )
    .await;
    if !init_ready {
        return;
    }

    // Try to emit one or more chunks from the segmenter's accumulated
    // samples.
    let chunk_duration_90k =
        (config.chunk_duration_ms as u64 * 90_000) / 1_000;
    let mut did_emit = false;

    if let (Some(vs), Some(ll)) = (state.video_seg.as_mut(), state.ll_current.as_mut()) {
        loop {
            let chunk_bytes = vs.take_pending_chunk(
                ll.sequence_number as u32,
                chunk_duration_90k,
                ll.chunks_emitted,
            );
            let Some(bytes) = chunk_bytes else {
                break;
            };
            let bytes_len = bytes.len();
            let sent = match ll.handle.send_chunk(bytes.clone()) {
                // The PUT died before it carried anything — opened into an
                // origin that was down, and now the init has landed and the
                // origin is back. A fresh PUT for the same object loses
                // nothing: this is the segment's first chunk. Only when the
                // init is landing, though: while it is not, the origin is
                // still down and a second PUT is a second failure.
                Err(upload::ChunkSendError::Closed)
                    if ll.chunks_emitted == 0 && !state.init_upload_failing =>
                {
                    ll.handle = chunked_put(
                        &format!("{}/{}", base_url, ll.uri),
                        "video/mp4",
                        config.auth_token.as_deref(),
                        8,
                    );
                    ll.handle.send_chunk(bytes)
                }
                other => other,
            };
            match sent {
                Ok(()) => {
                    ll.chunks_emitted += 1;
                    did_emit = true;
                    let part_dur =
                        config.chunk_duration_ms as f64 / 1000.0;
                    ll.parts.push(HlsPartEntry {
                        uri: format!("{}?part={}", ll.uri, ll.chunks_emitted - 1),
                        duration_secs: part_dur,
                        independent: ll.chunks_emitted == 1,
                    });
                    tracing::trace!(
                        "CMAF output '{}': emitted LL chunk {}#{} ({}B)",
                        config.id, ll.sequence_number, ll.chunks_emitted - 1, bytes_len
                    );
                }
                Err(why) => {
                    // Abort the PUT, discard accumulated samples for this
                    // segment, and wait for the next IDR to open a fresh
                    // segment. Named by what happened: a full queue is the
                    // ingest not keeping up, a closed request is the origin
                    // gone — and the second is one Warning per episode, the
                    // same episode the close path tracks, not one per
                    // segment for as long as the origin is away.
                    let seq = ll.sequence_number;
                    let chunks = ll.chunks_emitted;
                    match why {
                        upload::ChunkSendError::Full => {
                            tracing::warn!(
                                "CMAF output '{}': LL ingest stall, aborting seg {seq}",
                                config.id
                            );
                            event_sender.emit_flow(
                                EventSeverity::Warning,
                                category::CMAF,
                                format!(
                                    "CMAF output '{}': LL chunk enqueue full (seg {seq}) — aborting",
                                    config.id
                                ),
                                flow_id,
                            );
                        }
                        upload::ChunkSendError::Closed => {
                            tracing::warn!(
                                "CMAF output '{}': origin closed the PUT for seg {seq} after \
                                 {chunks} chunk(s), aborting",
                                config.id
                            );
                            // See the close path: the next publish probes.
                            state.init_last_upload = None;
                            if !state.ll_put_failing {
                                state.ll_put_failing = true;
                                event_sender.emit_flow(
                                    EventSeverity::Warning,
                                    category::CMAF,
                                    format!(
                                        "CMAF output '{}': the origin closed an LL PUT (seg {seq})",
                                        config.id
                                    ),
                                    flow_id,
                                );
                            }
                        }
                    }
                    let ll = state.ll_current.take().unwrap();
                    ll.handle.abort();
                    return;
                }
            }
        }
    }

    // Update the manifest on every chunk emission so players see the
    // new part advertised.
    if did_emit && publish_hls {
        publish_ll_hls(state, config, init_name, m3u8_url, event_sender, flow_id)
            .await;
    }
    if did_emit && publish_dash {
        publish_ll_dash(state, config, mpd_url, event_sender, flow_id).await;
    }
}

async fn publish_ll_dash(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    mpd_url: &str,
    event_sender: &EventSender,
    flow_id: &str,
) {
    let Some(v) = state.video_seg.as_ref() else { return };
    let latest_seq = state
        .ll_current
        .as_ref()
        .map(|l| l.sequence_number)
        .unwrap_or_else(|| {
            state
                .playlist
                .back()
                .map(|e| e.sequence_number)
                .unwrap_or(0)
        });
    let ato = (config.segment_duration_secs
        - (config.chunk_duration_ms as f64 / 1000.0))
        .max(0.0);
    let video_rep = DashVideoRep {
        codec: v.track.codec,
        sps: &v.track.sps,
        width: v.track.width,
        height: v.track.height,
        timescale: v.track.timescale,
        bandwidth_bps: state.video_bps_ewma.max(500_000),
    };
    let audio_rep = state
        .audio_seg
        .as_ref()
        .and_then(|a| a.track.aac_asc().map(|asc| (a, asc)))
        .map(|(a, asc)| DashAudioRep {
            asc,
            sample_rate: a.track.sample_rate,
            channels: a.track.channels,
            bandwidth_bps: state.audio_bps_ewma.max(64_000),
        });
    let mpd = build_dash_mpd(&DashInput {
        availability_start_unix_secs: state.availability_start_unix,
        target_segment_duration_secs: config.segment_duration_secs,
        video: Some(video_rep),
        audio: audio_rep,
        latest_segment_number: latest_seq,
        available_segments: state.playlist.len() as u64,
        availability_time_offset_secs: ato,
        init_uri: &state.init_uri,
    });
    if let Err(e) = http_put(
        mpd_url,
        mpd.into_bytes(),
        "application/dash+xml",
        config.auth_token.as_deref(),
    )
    .await
    {
        tracing::warn!("CMAF output '{}': LL mpd upload failed: {e}", config.id);
        event_sender.emit_flow(
            EventSeverity::Warning,
            category::CMAF,
            format!("CMAF output '{}': LL mpd upload failed: {e}", config.id),
            flow_id,
        );
    }
}

/// The segment a low-latency output is currently writing, as the playlist
/// needs to see it.
///
/// Deliberately not [`LlSegment`] itself: that owns a live chunked-PUT handle,
/// which cannot be built without a socket, so anything taking one is untestable
/// and the row-building was therefore never tested at all. That is how the
/// open-segment dating bug survived — every date test called the close path,
/// so reverting the one line that fixed it left the suite green. This struct is
/// the seam that closes it.
struct OpenSegmentRow<'a> {
    sequence_number: u64,
    uri: &'a str,
    parts: &'a [HlsPartEntry],
    base_dts_90k: u64,
    generation: u32,
    /// The open segment breaks the decode chain from the last closed row:
    /// it opened a generation, or names a different init from that row's,
    /// or is this run's first after a restore. The closed row it becomes
    /// carries the same tag, so a player does not see one appear on a row
    /// it already parsed.
    breaks_chain: bool,
}

/// The rows a low-latency playlist publishes: every closed segment in the
/// window, plus a synthetic row for the one still being written, so its parts
/// are advertised before it closes.
fn ll_playlist_entries(
    closed: &VecDeque<M3u8Entry>,
    open: Option<OpenSegmentRow<'_>>,
    nominal_segment_secs: f64,
    flow_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<M3u8Entry> {
    let mut entries: Vec<M3u8Entry> = closed.iter().cloned().collect();
    if let Some(open) = open {
        // Read the flow clock; never steer it. This row describes a segment
        // that has *opened*, and it is rebuilt on every chunk emission —
        // feeding those samples to the steering loop, which assumes a segment
        // that has closed, put every date this flow published
        // `segment_duration - chunk_duration` early, for the life of the flow
        // and for its sibling rendition too. See
        // `FlowClock::date_open_segment`.
        //
        // `None` until the flow's first segment closes, so a low-latency
        // output's opening playlist carries no `#EXT-X-PROGRAM-DATE-TIME` for
        // about one segment. The tag is optional under RFC 8216 §4.3.2.6; a
        // date wrong by most of a segment is not.
        // How much of this segment has actually been published: the parts
        // listed under the row, which are the chunks already on the origin.
        //
        // A closed row advertises the length it really ran. An open one cannot
        // — the length is settled by the IDR that ends it, which has not
        // arrived — so it needs a figure that is honest about that. RFC 8216's
        // own answer for a segment still being written is that it has no
        // `#EXTINF` at all until it is complete; it is advertised by its
        // `#EXT-X-PART` rows, and the `#EXTINF` appears when the segment does.
        // Emitting the row early is what makes this implementation's part rows
        // reachable (`build_hls_playlist` hangs them off the last entry), so
        // the row stays — but its duration must be a floor rather than a
        // guess, because an over-claim is a player seeking to media that does
        // not exist yet and, since the tag is derived from the longest row in
        // the window, an over-claim also widens `#EXT-X-TARGETDURATION` for
        // the rest of the session on the strength of a prediction.
        //
        // The nominal target alone was that guess in the other direction: with
        // a 5 s GOP against a 2 s target the row said `#EXTINF:2.000` while
        // twenty-five `#EXT-X-PART:DURATION=0.200` rows beneath it — in the
        // same playlist — accounted for 5 s. Taking the larger of the two ends
        // that contradiction without ever claiming media that has not been
        // written: the parts are already on the origin, and their advertised
        // durations are the chunk target, which the segmenter meets or exceeds
        // before it emits one.
        let published_secs: f64 = open.parts.iter().map(|p| p.duration_secs).sum();
        let open_secs = nominal_segment_secs.max(published_secs);
        // The same figure goes to the clock, where it is the upper end of the
        // band that decides whether `now` can plausibly sit inside this
        // segment: `EPOCH_REANCHOR_SECS + seg_secs`. Fixed at the nominal
        // target that ceiling does not grow with the segment, so a segment that
        // runs more than ten seconds past the target falls outside its own
        // band and the in-progress row silently loses its date for the tail of
        // every one. Measured on a 15 s segment against a 2 s target: undated
        // from t = 12.2 s to the close — the last fifteen manifest publishes at
        // 200 ms chunks — and dated throughout once the figure tracks the parts
        // already written.
        let dated = open_segment_date(flow_id, open.base_dts_90k, open_secs, now);
        entries.push(M3u8Entry {
            sequence_number: open.sequence_number,
            // At least the nominal target, and at least what the parts under
            // this row already carry. The row is rewritten with the true
            // figure when the segment closes and enters the window properly.
            duration_secs: open_secs,
            uri: Some(open.uri.to_string()),
            parts: open.parts.to_vec(),
            init_uri: (open.generation > 0).then(|| init_object_name(open.generation)),
            program_date_time: dated.map(|(pdt, _)| pdt),
            // This path takes no sample, so it discovers no discontinuity of
            // its own. What it can carry is one a *sibling* rendition already
            // found under this segment: that re-anchor has already moved the
            // date above, and a moved date published without the tag is the
            // contradiction the tag exists to close. And the break its own
            // row will carry when it closes — see `OpenSegmentRow`.
            discontinuity: dated.is_some_and(|(_, disc)| disc) || open.breaks_chain,
        });
    }
    entries
}

/// The playlist row a low-latency segment contributes when it closes.
///
/// `next_base_dts_90k` is where the *following* segment starts, which is how
/// long this one actually ran. The segmenter cuts on the first IDR at or past
/// the target, so that equals the target only when the GOP divides it; a 1.5 s
/// GOP against a 2 s target produces 3 s segments. Passing the nominal figure
/// instead — which is what this did — tells the flow clock a segment closed
/// (actual − nominal) earlier than it did, so the epoch it founds is late by
/// that much, permanently, from segment zero: a full second in that example,
/// three seconds for a 5 s GOP against a 2 s target. Both renditions of a flow
/// share the epoch and the per-segment cache makes the second reproduce the
/// first's answer, so the low-latency output — which normally dates first,
/// because it publishes on the final chunk PUT rather than waiting for the
/// whole-segment PUT — drags its correct sibling along with it. It is the
/// open-segment bug one magnitude down.
#[allow(clippy::too_many_arguments)]
fn closed_ll_entry(
    flow_id: &str,
    sequence_number: u64,
    uri: String,
    base_dts_90k: u64,
    next_base_dts_90k: Option<u64>,
    nominal_segment_secs: f64,
    now: chrono::DateTime<chrono::Utc>,
    generation: u32,
    first_of_generation: bool,
) -> M3u8Entry {
    let seg_secs = next_base_dts_90k
        .filter(|next| *next > base_dts_90k)
        .map(|next| (next - base_dts_90k) as f64 / 90_000.0)
        .unwrap_or(nominal_segment_secs);
    // From the media timeline, through the flow's shared epoch — see
    // `segment_date_marking`, and the plain path, which has always passed the
    // segment's real duration here.
    let (pdt, discontinuity) = segment_date_marking(flow_id, base_dts_90k, seg_secs, now);
    M3u8Entry {
        sequence_number,
        duration_secs: seg_secs,
        uri: Some(uri),
        parts: Vec::new(),
        init_uri: (generation > 0).then(|| init_object_name(generation)),
        program_date_time: Some(pdt),
        discontinuity: discontinuity || first_of_generation,
    }
}

async fn publish_ll_hls(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    init_name: &str,
    m3u8_url: &str,
    event_sender: &EventSender,
    flow_id: &str,
) {
    let entries = ll_playlist_entries(
        &state.playlist,
        state.ll_current.as_ref().map(|ll| OpenSegmentRow {
            sequence_number: ll.sequence_number,
            uri: &ll.uri,
            parts: &ll.parts,
            base_dts_90k: ll.base_dts_90k,
            generation: ll.generation,
            // Peeked, not taken: the close is what consumes the restore flag.
            breaks_chain: ll.first_of_generation
                || state.restore_discontinuity
                || state.generation_break(ll.generation),
        }),
        config.segment_duration_secs,
        flow_id,
        chrono::Utc::now(),
    );
    let hints = LowLatencyHints {
        part_target_secs: config.chunk_duration_ms as f64 / 1000.0,
        can_block_reload: true,
    };
    // Not the configured segment length: the segmenter cuts at or past it, so
    // the rows can be longer, and `HOLD-BACK` is three times whatever this
    // says. See `CmafState::target_duration_published`.
    let target_duration =
        state.advertised_target_duration(config.segment_duration_secs, &entries);
    let body = stamp_init_fingerprint(
        build_hls_playlist(
            target_duration,
            &entries,
            init_name,
            state.discontinuities_trimmed,
            Some(&hints),
        ),
        state.init_fingerprint.as_deref(),
        state.init_generation,
    );
    if let Err(e) = http_put(
        m3u8_url,
        body.into_bytes(),
        "application/vnd.apple.mpegurl",
        config.auth_token.as_deref(),
    )
    .await
    {
        tracing::warn!(
            "CMAF output '{}': LL m3u8 upload failed: {e}",
            config.id,
        );
        event_sender.emit_flow(
            EventSeverity::Warning,
            category::CMAF,
            format!("CMAF output '{}': LL m3u8 upload failed: {e}", config.id),
            flow_id,
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_manifests(
    state: &mut CmafState,
    config: &CmafOutputConfig,
    init_name: &str,
    m3u8_url: &str,
    mpd_url: &str,
    publish_hls: bool,
    publish_dash: bool,
    latest_seq: u64,
    event_sender: &EventSender,
    flow_id: &str,
) {
    if publish_hls {
        let entries: Vec<M3u8Entry> = state.playlist.iter().cloned().collect();
        let target_duration =
            state.advertised_target_duration(config.segment_duration_secs, &entries);
        let body = stamp_init_fingerprint(
            build_hls_playlist(
                target_duration,
                &entries,
                init_name,
                state.discontinuities_trimmed,
                None,
            ),
            state.init_fingerprint.as_deref(),
            state.init_generation,
        );
        if let Err(e) = http_put(
            m3u8_url,
            body.into_bytes(),
            "application/vnd.apple.mpegurl",
            config.auth_token.as_deref(),
        )
        .await
        {
            tracing::warn!(
                "CMAF output '{}': manifest.m3u8 upload failed: {e}",
                config.id,
            );
            event_sender.emit_flow(
                EventSeverity::Warning,
                category::CMAF,
                format!("CMAF output '{}': m3u8 upload failed: {e}", config.id),
                flow_id,
            );
        }
    }

    if publish_dash
        && let Some(v) = state.video_seg.as_ref() {
            let video_rep = DashVideoRep {
                codec: v.track.codec,
                sps: &v.track.sps,
                width: v.track.width,
                height: v.track.height,
                timescale: v.track.timescale,
                bandwidth_bps: state.video_bps_ewma.max(500_000),
            };
            let audio_rep = state
                .audio_seg
                .as_ref()
                .and_then(|a| a.track.aac_asc().map(|asc| (a, asc)))
                .map(|(a, asc)| DashAudioRep {
                    asc,
                    sample_rate: a.track.sample_rate,
                    channels: a.track.channels,
                    bandwidth_bps: state.audio_bps_ewma.max(64_000),
                });
            let mpd = build_dash_mpd(&DashInput {
                availability_start_unix_secs: state.availability_start_unix,
                target_segment_duration_secs: config.segment_duration_secs,
                video: Some(video_rep),
                audio: audio_rep,
                latest_segment_number: latest_seq,
                available_segments: state.playlist.len() as u64,
                availability_time_offset_secs: 0.0,
                init_uri: &state.init_uri,
            });
            if let Err(e) = http_put(
                mpd_url,
                mpd.into_bytes(),
                "application/dash+xml",
                config.auth_token.as_deref(),
            )
            .await
            {
                tracing::warn!(
                    "CMAF output '{}': manifest.mpd upload failed: {e}",
                    config.id,
                );
                event_sender.emit_flow(
                    EventSeverity::Warning,
                    category::CMAF,
                    format!("CMAF output '{}': mpd upload failed: {e}", config.id),
                    flow_id,
                );
            }
        }
}

#[cfg(test)]
mod date_tests {
    use super::*;

    /// A restart picks up the window the origin already holds.
    ///
    /// Without this the edge republishes a manifest covering only what it has
    /// produced since it started, while the origin still holds the previous
    /// hour — so every viewer's DVR history disappears and marks grey out,
    /// because the player reads reachability from the playlist.
    #[test]
    fn a_served_playlist_can_be_read_back_as_a_window() {
        let m3u8 = concat!(
            "#EXTM3U
#EXT-X-VERSION:9
#EXT-X-TARGETDURATION:2
",
            "#EXT-X-MAP:URI=\"init.mp4\"
",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z
#EXTINF:2.000,
seg-00040.m4s?token=abc
",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:02.000Z
#EXTINF:2.000,
seg-00041.m4s?token=abc
",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:04.000Z
#EXTINF:1.960,
seg-00042.m4s?token=abc
",
        );

        let restored = parse_published_window(m3u8, 100).expect("a window");
        let (rows, next) = (restored.rows, restored.next_seq);
        assert_eq!(rows.len(), 3);
        // Numbering continues past what the origin holds. Restarting at zero
        // is what let a new run overwrite the segments it had just restored.
        assert_eq!(next, 43, "the next segment would overwrite an existing one");
        assert_eq!(rows[0].sequence_number, 40);
        assert!((rows[2].duration_secs - 1.960).abs() < 1e-6);
        assert!(rows[0].program_date_time.is_some(), "the row lost its date");
        // The viewer token in the URI is the *player's*, not this edge's.
        assert_eq!(rows[0].uri.as_deref(), Some("seg-00040.m4s"));

        // Never restore more than the output will advertise.
        let trimmed = parse_published_window(m3u8, 2).expect("a window").rows;
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].sequence_number, 41, "the wrong end was trimmed");

        // A fresh stream has nothing to restore, and must not look like it does.
        assert!(parse_published_window("#EXTM3U
#EXT-X-VERSION:9
", 100).is_none());
        assert!(parse_published_window("", 100).is_none());
    }

    /// A discontinuity inside the restored window survives the restart.
    ///
    /// The tag really is on the wire — the relay origin passes through every
    /// line it does not itself rewrite — so discarding it was a loss in this
    /// parser. The republished playlist then asserted one continuous timeline
    /// across a real media-timeline re-anchor whose post-jump dates *were*
    /// restored: a viewer scrubbing back over the join decodes across it with
    /// no reset, and one who reloads sees a tag vanish from rows already
    /// parsed.
    #[test]
    fn a_discontinuity_inside_the_window_is_read_back_with_it() {
        let m3u8 = concat!(
            "#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z\n#EXTINF:2.000,\nseg-00040.m4s\n",
            "#EXT-X-DISCONTINUITY\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T11:00:00.000Z\n#EXTINF:2.000,\nseg-00041.m4s\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T11:00:02.000Z\n#EXTINF:2.000,\nseg-00042.m4s\n",
        );
        let restored = parse_published_window(m3u8, 100).expect("a window");
        assert!(!restored.rows[0].discontinuity, "the tag moved to the wrong row");
        assert!(restored.rows[1].discontinuity, "the join was lost in the read-back");
        assert!(!restored.rows[2].discontinuity, "the tag was not cleared after its row");

        // And the count of the ones that have already aged out.
        //
        // `CmafState::new` starts this at 0, so without restoring it a restart
        // republished a playlist whose discontinuity sequence went 3 → 0 while
        // its media sequence carried on normally. RFC 8216 §6.3.3 makes that an
        // incompatible playlist change, and a player answers by resynchronising
        // or resetting the media element.
        assert_eq!(restored.discontinuity_sequence, 3);
    }

    /// The init identity survives a round trip through a served playlist.
    ///
    /// This is what lets the next run tell "the same tracks as before" from
    /// "an init that no longer describes the restored rows". `init.mp4` is one
    /// fixed object the new run overwrites, so without it any track change
    /// across a restart left an hour of restored history described by an init
    /// that cannot decode it — and MSE answers that by initialising the
    /// declared track and waiting for ever, with nothing wrong on the wire.
    #[test]
    fn the_init_identity_survives_a_round_trip_through_the_manifest() {
        let plain = concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-MAP:URI=\"init.mp4\"\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z\n#EXTINF:2.000,\nseg-00040.m4s\n",
        );
        let fp = init_fingerprint(b"an init segment's bytes");
        let stamped = stamp_init_fingerprint(plain.to_string(), Some(&fp), 0);
        assert!(
            stamped.starts_with("#EXTM3U\n#EXT-X-BILBYCAST-INIT:"),
            "the tag has to be inside the playlist, after the marker: {stamped}"
        );

        let restored = parse_published_window(&stamped, 100).expect("a window");
        assert_eq!(restored.init_fingerprint.as_deref(), Some(fp.as_str()));
        assert_eq!(restored.rows.len(), 1, "the tag consumed a media row");
        assert_eq!(restored.init_generation, 0);

        // The stamp carries the generation its hash describes, and the
        // restore continues it even when no row has reached it yet: the
        // manifest published in the segment after a rotation names the old
        // generation on every row, and a run that continued from the rows
        // would publish under that name and over the object they decode
        // against.
        let ahead = stamp_init_fingerprint(plain.to_string(), Some(&fp), 3);
        assert!(ahead.contains(&format!("#EXT-X-BILBYCAST-INIT:{fp},gen=3\n")), "{ahead}");
        let restored = parse_published_window(&ahead, 100).expect("a window");
        assert_eq!(restored.init_fingerprint.as_deref(), Some(fp.as_str()));
        assert_eq!(restored.init_generation, 3, "the stamp's generation, past the rows'");

        // A number this edge could not have written does not steer it.
        let absurd = format!("#EXTM3U\n#EXT-X-BILBYCAST-INIT:{fp},gen=4294967295\n#EXTINF:2.000,\nseg-00001.m4s\n");
        assert_eq!(parse_published_window(&absurd, 100).expect("a window").init_generation, 0);

        // Different init bytes, different name.
        assert_ne!(fp, init_fingerprint(b"a different init segment's bytes"));

        // A manifest written before the tag existed says nothing, which is not
        // the same as saying "different" — refusing every such restore would
        // cost the window on no evidence.
        assert!(parse_published_window(plain, 100).expect("a window").init_fingerprint.is_none());

        // And a playlist is unchanged when there is nothing to stamp.
        assert_eq!(stamp_init_fingerprint(plain.to_string(), None, 0), plain);
    }

    /// A low-latency playlist's open segment is not restored as a closed one.
    ///
    /// The LL writer lists the segment still being uploaded exactly like a
    /// finished one and hangs its `#EXT-X-PART` rows beneath it. If the
    /// previous run died mid-PUT the origin never stored that segment, so
    /// restoring the row advertised a 404 in the middle of the window for as
    /// long as the window lasted — and the origin's head trim cannot reach it,
    /// because it stops at the first row that is backed. The number is still
    /// kept, so the new run does not reuse it either.
    #[test]
    fn a_low_latency_playlists_open_segment_is_not_restored() {
        let m3u8 = concat!(
            "#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z\n#EXTINF:2.000,\nseg-00040.m4s\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:02.000Z\n#EXTINF:2.000,\nseg-00041.m4s\n",
            "#EXT-X-PART:DURATION=0.500,URI=\"seg-00041.m4s?part=0\"\n",
            "#EXT-X-PART:DURATION=0.500,URI=\"seg-00041.m4s?part=1\"\n",
        );
        let restored = parse_published_window(m3u8, 100).expect("a window");
        assert_eq!(restored.rows.len(), 1, "the open segment was restored as closed");
        assert_eq!(restored.rows[0].sequence_number, 40);
        assert_eq!(restored.next_seq, 42, "the interrupted segment's number must not be reused");
    }

    /// Numbering is protected by every segment the origin advertises, usable
    /// row or not; and trimming the restored window to its limit counts the
    /// discontinuities it drops.
    ///
    /// A window whose every row was unusable used to restore nothing at all,
    /// which left `resume_seq` at zero — and the new run then renumbered from
    /// `seg-00000` over segments the origin still served, the destructive
    /// outcome the restore exists to prevent.
    #[test]
    fn an_unusable_window_still_protects_the_numbering() {
        let unusable = concat!(
            "#EXTM3U\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z\n#EXTINF:NaN,\nseg-00040.m4s\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:02.000Z\n#EXTINF:inf,\nseg-00041.m4s\n",
        );
        let restored = parse_published_window(unusable, 100).expect("a number to carry on from");
        assert!(restored.rows.is_empty(), "an unusable row became a row anyway");
        assert_eq!(restored.next_seq, 42);

        // A long-GOP source can legitimately close a segment well past the
        // configured target; a row like that is ours and is kept.
        let long_gop = concat!(
            "#EXTM3U\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z\n#EXTINF:75.000,\nseg-00040.m4s\n",
        );
        assert_eq!(parse_published_window(long_gop, 100).expect("a window").rows.len(), 1);

        // Trimming to the limit advances the discontinuity sequence by every
        // tagged row it drops, exactly as `trim_playlist` would.
        let tagged = concat!(
            "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n",
            "#EXTINF:2.000,\nseg-00040.m4s\n",
            "#EXT-X-DISCONTINUITY\n#EXTINF:2.000,\nseg-00041.m4s\n",
            "#EXTINF:2.000,\nseg-00042.m4s\n",
            "#EXTINF:2.000,\nseg-00043.m4s\n",
        );
        let restored = parse_published_window(tagged, 2).expect("a window");
        assert_eq!(restored.rows.len(), 2);
        assert_eq!(restored.rows[0].sequence_number, 42);
        assert_eq!(restored.discontinuity_sequence, 4, "the dropped tag was not counted");
    }

    /// An init mismatch across a restart opens a new generation and keeps
    /// every row — the restored ones under the init they name, this run's own
    /// under the new one.
    ///
    /// The first version dropped the restored rows, because `init.mp4` was
    /// one fixed object about to be overwritten with parameter sets that did
    /// not describe them. Per-generation inits made that unnecessary: nothing
    /// is overwritten, so nothing is stranded. This run's own first segment
    /// can close before the fingerprint is compared — the audio-detection
    /// grace is three seconds and a segment is two — and was stamped with
    /// the restored generation while its media decodes against the new one,
    /// so it is relabelled rather than dropped.
    #[test]
    fn an_init_mismatch_rotates_instead_of_dropping() {
        let row = |seq: u64, discontinuity: bool| M3u8Entry {
            sequence_number: seq,
            duration_secs: 2.0,
            uri: None,
            parts: Vec::new(),
            init_uri: Some("init-2.mp4".to_string()),
            program_date_time: None,
            discontinuity,
        };
        let mut state = CmafState::new();
        state.playlist = [row(40, false), row(41, true), row(42, false), row(43, true)]
            .into_iter()
            .collect();
        state.resume_seq = 43;
        state.init_generation = 2;
        state.init_uri = init_object_name(2);
        state.restored_init_fingerprint = Some("old".into());

        assert_eq!(state.restored_init_differs("new"), Some(true));
        // Compared once: the fingerprint is consumed.
        assert_eq!(state.restored_init_differs("new"), None);

        state.relabel_own_rows(2, 3);
        let names: Vec<&str> =
            state.playlist.iter().map(CmafState::row_init_name).collect();
        assert_eq!(
            names,
            vec!["init-2.mp4", "init-2.mp4", "init-2.mp4", "init-3.mp4"],
            "restored rows keep their init; this run's own row moves to the new one"
        );
        assert_eq!(state.playlist.len(), 4, "nothing is dropped");
        assert!(state.playlist[3].discontinuity, "and the join keeps its tag");

        // Only rows of the generation being bumped: an own row this run cut
        // under an earlier generation of its own decodes against that one.
        let mut two = CmafState::new();
        two.playlist = [row(43, true), row(44, false)].into_iter().collect();
        two.playlist[1].init_uri = Some("init-5.mp4".to_string());
        two.resume_seq = 43;
        two.relabel_own_rows(5, 6);
        let names: Vec<&str> = two.playlist.iter().map(CmafState::row_init_name).collect();
        assert_eq!(names, vec!["init-2.mp4", "init-6.mp4"]);

        // A window with no restored row has nothing the previous init
        // describes: a differing fingerprint opens no generation.
        let mut empty = CmafState::new();
        empty.playlist = [row(43, true)].into_iter().collect();
        empty.resume_seq = 43;
        empty.restored_init_fingerprint = Some("old".into());
        assert_eq!(empty.restored_init_differs("new"), Some(false));

        // A match says so, and is also consumed.
        let mut same = CmafState::new();
        same.restored_init_fingerprint = Some("fp".into());
        assert_eq!(same.restored_init_differs("fp"), Some(false));
        assert_eq!(same.restored_init_differs("fp"), None);

        // Nothing restored, nothing to say.
        assert_eq!(CmafState::new().restored_init_differs("fp"), None);
    }

    /// The restore parser carries each row's `#EXT-X-MAP` and the highest
    /// generation it names, so a restart after a rotation continues that
    /// generation rather than publishing `init.mp4` over the object the
    /// oldest rows decode against.
    #[test]
    fn a_restored_window_keeps_each_rows_init() {
        let served = "#EXTM3U\n\
            #EXT-X-VERSION:7\n\
            #EXT-X-TARGETDURATION:2\n\
            #EXT-X-MEDIA-SEQUENCE:40\n\
            #EXT-X-MAP:URI=\"init.mp4?token=abc\"\n\
            #EXTINF:2.000,\nseg-00040.m4s\n\
            #EXT-X-MAP:URI=\"init-1.mp4\"\n\
            #EXT-X-DISCONTINUITY\n\
            #EXTINF:2.000,\nseg-00041.m4s\n\
            #EXTINF:2.000,\nseg-00042.m4s\n";
        let restored = parse_published_window(served, 100).expect("a window");
        let names: Vec<&str> = restored.rows.iter().map(CmafState::row_init_name).collect();
        assert_eq!(names, vec!["init.mp4", "init-1.mp4", "init-1.mp4"]);
        assert_eq!(restored.rows[0].init_uri, None, "generation 0 is the stream's own init");
        assert_eq!(restored.init_generation, 1);
        assert!(restored.rows[1].discontinuity);

        // A manifest from before rotations existed: one map, generation 0.
        let plain = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2.000,\nseg-00001.m4s\n";
        let restored = parse_published_window(plain, 100).expect("a window");
        assert_eq!(restored.init_generation, 0);
        assert_eq!(restored.rows[0].init_uri, None);
    }

    /// Adopting a generation keeps the previous init for republishing while
    /// the window still names it, and forgets it once no row does.
    #[test]
    fn a_rotation_keeps_the_previous_init_while_the_window_names_it() {
        let row = |seq: u64, init: Option<&str>| M3u8Entry {
            sequence_number: seq,
            duration_secs: 2.0,
            uri: None,
            parts: Vec::new(),
            init_uri: init.map(str::to_string),
            program_date_time: None,
            discontinuity: false,
        };
        let mut state = CmafState::new();
        state.playlist = [row(0, None), row(1, None)].into_iter().collect();
        state.init_uploaded = true;
        state.last_init_bytes = Some(b"generation zero".to_vec());
        let track = VideoTrack::from_h264(vec![0x67, 0x42, 0x00, 0x1e], vec![0x68, 0xce]);
        let mut seg = VideoSegmenter::new(track, 2.0);
        seg.rotate_track(VideoTrack::from_h264(vec![0x67, 0x64, 0x00, 0x1f], vec![0x68, 0xee]));
        // Applied at the next IDR.
        seg.push(&[vec![0x65, 0x88]], 0, true);
        assert_eq!(seg.generation(), 1);
        state.video_seg = Some(seg);

        assert!(adopt_generation(&mut state, "out"));
        assert_eq!(state.init_generation, 1);
        assert_eq!(state.init_uri, "init-1.mp4");
        assert!(!state.init_uploaded, "the new init has to go up before anything is advertised");
        assert_eq!(state.init_history.len(), 1);
        assert_eq!(state.init_history[0].uri, "init.mp4");
        assert_eq!(state.init_history[0].bytes, b"generation zero");
        assert!(!adopt_generation(&mut state, "out"), "adopted once");

        // The held init is forgotten only when the window stops naming it —
        // which is a row leaving the window, never the rotation itself.
        state.playlist.push_back(row(2, Some("init-1.mp4")));
        state.trim_playlist(3);
        assert_eq!(state.init_history.len(), 1, "two rows still name it");
        state.trim_playlist(1);
        assert!(state.init_history.is_empty(), "none does");

        // A rotation whose retired generation has no row in the window yet
        // — its only segment is the one closing at this IDR — keeps the init
        // until that row has been pushed and has left.
        let mut fresh = CmafState::new();
        fresh.init_uploaded = true;
        fresh.last_init_bytes = Some(b"only one segment".to_vec());
        let mut seg = VideoSegmenter::new(
            VideoTrack::from_h264(vec![0x67, 0x42, 0x00, 0x1e], vec![0x68, 0xce]),
            2.0,
        );
        seg.rotate_track(VideoTrack::from_h264(vec![0x67, 0x64, 0x00, 0x1f], vec![0x68, 0xee]));
        seg.push(&[vec![0x65, 0x88]], 0, true);
        fresh.video_seg = Some(seg);
        assert!(adopt_generation(&mut fresh, "out"));
        assert_eq!(fresh.init_history.len(), 1, "kept for the row still to come");
    }

    /// Read a chunk's `tfdt` and its `trun` sample durations. The chunk's
    /// `trun` flags are fixed by `build_segment_chunk` (data offset, first
    /// sample flags, and per-sample duration / size / composition offset).
    fn chunk_timeline(bytes: &[u8]) -> (u64, Vec<u32>, bool) {
        fn find(bytes: &[u8], kind: &[u8; 4]) -> Option<usize> {
            (0..bytes.len().saturating_sub(4)).find(|&i| &bytes[i..i + 4] == kind)
        }
        fn be32(b: &[u8]) -> u32 {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
        let styp = find(bytes, b"styp").is_some();
        let tfdt = find(bytes, b"tfdt").expect("tfdt") + 4;
        let base = u64::from_be_bytes(bytes[tfdt + 4..tfdt + 12].try_into().unwrap());
        let trun = find(bytes, b"trun").expect("trun") + 4;
        let flags = be32(&bytes[trun..]) & 0x00FF_FFFF;
        assert_eq!(flags, 0x0001 | 0x0004 | 0x0100 | 0x0200 | 0x0800);
        let count = be32(&bytes[trun + 4..]) as usize;
        let mut p = trun + 8 + 4 + 4; // data offset + first sample flags
        let mut durations = Vec::with_capacity(count);
        for _ in 0..count {
            durations.push(be32(&bytes[p..]));
            p += 12;
        }
        (base, durations, styp)
    }

    /// The chunks a low-latency segment sends, plus the tail that closes it,
    /// tile the segment exactly — every sample pushed reaches the origin,
    /// with the tail's `tfdt` where the last chunk ended.
    ///
    /// The tail was dropped: `push()` snapshots the un-chunked samples into
    /// the completed segment and clears them, and the low-latency path
    /// never read that, so every segment's object stopped up to one chunk
    /// short of its row — 25 % of each at the default 2 s / 500 ms, a hole
    /// MSE gap-jumped or stalled on every segment. Driven in production
    /// order: push, then the chunk loop, then the tail at the cut.
    #[test]
    fn a_low_latency_segment_is_written_whole_including_its_tail() {
        for (fps, chunk_ms) in [(25u64, 500u64), (30, 500), (25, 2000)] {
            let track = VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]);
            let mut seg = VideoSegmenter::new(track, 2.0);
            let chunk_90k = chunk_ms * 90_000 / 1_000;
            let frame_90k = 90_000 / fps;
            let idr = vec![vec![0x65, 0xB8]];
            let p = vec![vec![0x41, 0x00]];
            let mut chunks_emitted = 0u32;
            // Samples the origin has for the open segment, and where the
            // next chunk must start.
            let mut written = 0usize;
            let mut next_start: Option<u64> = None;
            let mut segments = 0;
            // Frame n is an IDR every 2 s exactly.
            for n in 0..(3 * 2 * fps + 1) {
                let dts = n * frame_90k;
                let is_idr = n % (2 * fps) == 0;
                let out = seg.push(if is_idr { &idr } else { &p }, dts, is_idr);
                if let (Some(done), Some((_, _, tail))) =
                    (out.completed_video.as_ref(), out.completed_video_samples.as_ref())
                {
                    // The boundary: the tail closes the segment.
                    let t = ll_tail_chunk(done, tail, chunks_emitted);
                    let (base, durations, styp) = chunk_timeline(&t.bytes);
                    assert_eq!(base, t.tfdt_90k);
                    assert_eq!(styp, t.includes_styp);
                    assert_eq!(styp, chunks_emitted == 0, "styp opens the object, once");
                    assert_eq!(durations.iter().map(|d| *d as u64).sum::<u64>(), t.duration_90k);
                    if let Some(e) = next_start {
                        assert_eq!(base, e, "{fps} fps / {chunk_ms} ms: the tail starts where the last chunk ended");
                    } else {
                        assert_eq!(base, done.base_dts_90k, "nothing chunked: the tail is the segment");
                    }
                    assert_eq!(
                        t.tfdt_90k + t.duration_90k,
                        dts,
                        "{fps} fps / {chunk_ms} ms: the tail ends where the next segment starts"
                    );
                    written += durations.len();
                    assert_eq!(
                        written,
                        (2 * fps) as usize,
                        "{fps} fps / {chunk_ms} ms: every frame of the segment was written"
                    );
                    written = 0;
                    next_start = None;
                    chunks_emitted = 0;
                    segments += 1;
                    continue;
                }
                // The chunk loop, as handle_ll_cmaf runs it after every push.
                while let Some(bytes) = seg.take_pending_chunk(0, chunk_90k, chunks_emitted) {
                    let (base, durations, styp) = chunk_timeline(&bytes);
                    assert_eq!(styp, chunks_emitted == 0);
                    if let Some(e) = next_start {
                        assert_eq!(base, e, "chunks tile");
                    }
                    next_start = Some(base + durations.iter().map(|d| *d as u64).sum::<u64>());
                    chunks_emitted += 1;
                    written += durations.len();
                }
            }
            assert_eq!(segments, 3, "{fps} fps / {chunk_ms} ms");
        }
    }

    /// A row that names a different init from the last row in the window
    /// is a break in the decode chain, whatever became of the segment that
    /// opened the generation.
    #[test]
    fn a_change_of_map_between_rows_is_a_break() {
        let row = |seq: u64, init: Option<&str>| M3u8Entry {
            sequence_number: seq,
            duration_secs: 2.0,
            uri: None,
            parts: Vec::new(),
            init_uri: init.map(str::to_string),
            program_date_time: None,
            discontinuity: false,
        };
        let mut state = CmafState::new();
        assert!(!state.generation_break(0), "an empty window breaks from nothing");
        assert!(!state.generation_break(3));
        state.playlist.push_back(row(0, None));
        assert!(!state.generation_break(0));
        assert!(state.generation_break(1), "init.mp4 -> init-1.mp4");
        state.playlist.push_back(row(1, Some("init-1.mp4")));
        assert!(!state.generation_break(1));
        assert!(state.generation_break(0), "and back again, should a source flap");
    }

    /// A rotation before the previous init ever reached the origin drops the
    /// rows cut under it: no manifest ever listed them, and the init they
    /// would need is never going to exist.
    #[test]
    fn a_rotation_under_an_unpublished_init_drops_its_rows() {
        let row = |seq: u64, init: Option<&str>| M3u8Entry {
            sequence_number: seq,
            duration_secs: 2.0,
            uri: None,
            parts: Vec::new(),
            init_uri: init.map(str::to_string),
            program_date_time: None,
            discontinuity: false,
        };
        let mut state = CmafState::new();
        // Two restored rows under init.mp4, then two of this run's own.
        state.playlist = [row(0, None), row(1, None), row(2, None), row(3, None)]
            .into_iter()
            .collect();
        state.resume_seq = 2;
        state.init_uploaded = false;
        let track = VideoTrack::from_h264(vec![0x67, 0x42, 0x00, 0x1e], vec![0x68, 0xce]);
        let mut seg = VideoSegmenter::new(track, 2.0);
        seg.bump_generation();
        state.video_seg = Some(seg);

        assert!(adopt_generation(&mut state, "out"));
        let left: Vec<u64> = state.playlist.iter().map(|r| r.sequence_number).collect();
        assert_eq!(left, vec![0, 1], "the restored rows are not this run's to drop");
        assert!(state.init_history.is_empty());
    }

    /// A row the edge could not have written is not copied back out.
    ///
    /// Restored rows are republished verbatim, so `#EXTINF:NaN,` — which
    /// `parse::<f64>()` accepts — would be served back to every viewer for the
    /// whole window, and a `u64::MAX` sequence would wrap `next_seq` to 0 in a
    /// release build and renumber from `seg-00000` over the segments just
    /// restored. Neither is producible by this edge's own writer, so seeing one
    /// means the manifest is not ours to trust.
    #[test]
    fn a_manifest_the_edge_did_not_write_is_not_trusted_blindly() {
        let poisoned = concat!(
            "#EXTM3U\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z\n#EXTINF:NaN,\nseg-00040.m4s\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:02.000Z\n#EXTINF:inf,\nseg-00041.m4s\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:04.000Z\n#EXTINF:6000.0,\nseg-00042.m4s\n",
            "#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:06.000Z\n#EXTINF:2.000,\nseg-00043.m4s\n",
        );
        let restored = parse_published_window(poisoned, 100).expect("one good row");
        assert_eq!(restored.rows.len(), 1, "an unusable duration became a row anyway");
        assert_eq!(restored.rows[0].sequence_number, 43);

        // `u64::MAX + 1` has no answer: a number this edge could not have
        // written, so nothing of the manifest is trusted and the run starts
        // fresh rather than carrying on from a wrapped zero.
        let overflowing = concat!(
            "#EXTM3U\n#EXT-X-PROGRAM-DATE-TIME:2026-09-08T10:00:00.000Z\n#EXTINF:2.000,\n",
            "seg-18446744073709551615.m4s\n",
        );
        assert!(
            parse_published_window(overflowing, 100).is_none(),
            "a sequence number that cannot be continued was restored anyway"
        );
    }

    fn ts(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc)
    }

    /// Two renditions of one source publish the *same* date for the same
    /// content.
    ///
    /// This is the whole point of keying the epoch on the flow. The two are
    /// separate CMAF outputs with separate encoders and separate scheduling,
    /// so their `Utc::now()` calls land at different instants — 31-81 ms
    /// apart, measured, moving ~50 ms between samples. `PtsUnwrap` does not
    /// rebase, so `base_dts_90k` is the same number in both, and one shared
    /// epoch makes the published dates identical.
    ///
    /// The player relates the two renditions through these dates in order to
    /// put a full-resolution still over a low-resolution picture. Any
    /// disagreement here is a still that lands on a different frame.
    #[test]
    fn two_renditions_of_one_flow_date_the_same_content_identically() {
        let flow = "flow-identical";
        // The main rendition closes its segment first.
        let a = segment_date(flow, 900_000, 2.0, ts("2026-08-27T00:00:10.000Z"));
        // The proxy closes the same content 45 ms later, as measured.
        let b = segment_date(flow, 900_000, 2.0, ts("2026-08-27T00:00:10.045Z"));
        assert_eq!(a, b, "two renditions disagree about the same content");
        // Exactly, not nearly — and it has to stay exact while the epoch
        // slews to track the source clock, which is why the epoch in force for
        // each segment is remembered rather than recomputed.
        for later in [
            "2026-08-27T00:00:10.200Z",
            "2026-08-27T00:00:11.900Z",
        ] {
            assert_eq!(
                segment_date(flow, 900_000, 2.0, ts(later)),
                a,
                "the same content was dated differently once the epoch moved"
            );
        }

        // And a later segment is derived, not re-sampled: 2 s of media is 2 s
        // of clock, give or take the bounded slew that tracks the source.
        let later = segment_date(flow, 900_000 + 180_000, 2.0, ts("2026-08-27T00:00:12.400Z"));
        let step = (later - a).num_milliseconds();
        assert!(
            (2000 - step).abs() <= (EPOCH_SLEW_SECS * 1000.0) as i64,
            "the published clock does not advance with the media timeline: {step}ms"
        );
    }

    /// Publish jitter does not reach the tag.
    ///
    /// Before this, every segment sampled `Utc::now()`, so each rendition's
    /// head date wandered against a steady clock — 27 ms on main, 67 ms on
    /// the proxy — and because only one date was written per playlist, that
    /// wander moved the entire derived window every time it slid.
    #[test]
    fn a_late_publish_barely_moves_the_clock() {
        let flow = "flow-jitter";
        // The same segment, dated twice, is the *other* rendition arriving —
        // it must reproduce the first answer exactly. That is asserted
        // elsewhere; here the question is what publish jitter does to the
        // spacing of consecutive segments.
        let base = ts("2026-08-27T00:00:00.000Z");
        let mut prev = None;
        let mut worst = 0i64;
        // Publish on time, then 250 ms late, then early, then on time again.
        for (i, jitter_ms) in [0i64, 250, -120, 0, 300].into_iter().enumerate() {
            let wall = base
                + chrono::Duration::milliseconds((i as i64 + 1) * 2000 + jitter_ms);
            let got = segment_date(flow, i as u64 * 180_000, 2.0, wall);
            if let Some(p) = prev {
                let step: chrono::TimeDelta = got - p;
                worst = worst.max((step.num_milliseconds() - 2000).abs());
            }
            prev = Some(got);
        }
        // Taken at face value those samples would move the date by up to
        // 370 ms between segments — which is what put ~50 ms of noise into
        // every tag on the rig and left the renditions disagreeing.
        assert!(
            worst <= (EPOCH_SLEW_SECS * 1000.0) as i64,
            "publish jitter reached the published date: {worst}ms off a 2 s step"
        );
    }

    /// The epoch tracks a source clock that is not a wall clock.
    ///
    /// Measured on the demo rig: 480.00 s of media published in 480.22 s of
    /// real time, about 450 ppm slow. Pinned to its first sample the epoch
    /// would drift 4.1 s across a 2h30m session, and the operator's
    /// time-of-day readout with it.
    #[test]
    fn the_epoch_tracks_a_slow_source_clock() {
        let flow = "flow-slow";
        // 450 ppm: each 2 s segment is published 0.9 ms later than the last.
        let base = ts("2026-08-27T00:00:00.000Z");
        let mut last = None;
        for i in 0..600u64 {
            let media = i * 180_000; // 2 s in 90 kHz
            let wall = base
                + chrono::Duration::milliseconds((i as i64 + 1) * 2000)
                + chrono::Duration::microseconds(i as i64 * 900);
            last = Some(segment_date(flow, media, 2.0, wall));
        }
        // After twenty minutes of media the published date must still be close
        // to the wall clock the samples implied, not 0.54 s behind it.
        let want = base + chrono::Duration::milliseconds(599 * 2000 + 540);
        let drift = (last.unwrap() - want).num_milliseconds().abs();
        assert!(
            drift < 50,
            "the epoch did not track the source: {drift}ms adrift after 20 minutes"
        );
    }

    /// A source restart is re-anchored rather than absorbed.
    ///
    /// Jitter is tens of milliseconds; a restart or a PTS discontinuity moves
    /// the implied epoch by the whole elapsed time. Holding the old epoch
    /// through that would date every later segment hours out, silently.
    #[test]
    fn a_restarted_timeline_re_anchors() {
        let flow = "flow-restart";
        let before = segment_date(flow, 900_000, 2.0, ts("2026-08-27T00:00:12.000Z"));
        // The source restarts: PTS back near zero, wall clock much later.
        let after = segment_date(flow, 0, 2.0, ts("2026-08-27T01:00:00.000Z"));
        assert!(
            (after - before).num_minutes() >= 59,
            "a restarted source was dated from the old epoch: {before} -> {after}"
        );
        assert_eq!(
            after,
            ts("2026-08-27T00:59:58.000Z"),
            "re-anchoring did not use the new sample"
        );
    }

    /// The clock still tracks the source when the samples are noisy.
    ///
    /// This is the case the plain clamp could not handle. Publish jitter is
    /// larger than the 5 ms bound, so the clamp bound on nearly every segment
    /// and the loop moved 5 ms toward whichever side the noise fell — only the
    /// imbalance corrected the drift. Measured on the rig, that left 102 ppm
    /// of the original 407, or 0.9 s of walk across a 2h30m session.
    ///
    /// With the sample filtered first, the clamp bounds how fast the epoch may
    /// move rather than deciding how far, and the loop tracks the rate.
    #[test]
    fn the_epoch_tracks_a_slow_source_through_publish_jitter() {
        let flow = "flow-noisy";
        let base = ts("2026-08-27T00:00:00.000Z");
        // 450 ppm slow, plus ±30 ms of publish jitter — deterministic, so a
        // failure is reproducible rather than a bad afternoon.
        let mut rng: u64 = 0x9E3779B97F4A7C15;
        let mut last: Option<chrono::DateTime<chrono::Utc>> = None;
        // How far each published date sits from a clean 2 s step. This is what
        // separates a loop that tracks from one that chases noise: correcting
        // against the raw sample makes the clamp bind either way each segment,
        // so the dates jitter by the bound while still arriving in roughly the
        // right place. The endpoint alone cannot see that.
        let mut worst_step_err = 0i64;
        for i in 0..900u64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let jitter_ms = ((rng >> 33) % 61) as i64 - 30;
            let wall = base
                + chrono::Duration::milliseconds((i as i64 + 1) * 2000)
                + chrono::Duration::microseconds(i as i64 * 900)
                + chrono::Duration::milliseconds(jitter_ms);
            let got = segment_date(flow, i * 180_000, 2.0, wall);
            if let Some(prev) = last {
                // Ignore the first hundred, while the filter is still settling.
                if i > 100 {
                    let step = (got - prev).num_milliseconds();
                    worst_step_err = worst_step_err.max((step - 2000).abs());
                }
            }
            last = Some(got);
        }
        assert!(
            worst_step_err <= 2,
            "the published dates jitter with the samples: {worst_step_err}ms off a clean 2 s step"
        );
        // Thirty minutes of media. Pinned, the epoch would be 0.81 s adrift;
        // clamped against the raw sample it recovered only about three
        // quarters of that.
        let want = base + chrono::Duration::milliseconds(899 * 2000 + 899 * 900 / 1000);
        let drift = (last.unwrap() - want).num_milliseconds().abs();
        assert!(
            drift < 120,
            "the loop did not track the source through jitter: {drift}ms adrift"
        );
    }

    /// Flows do not share an epoch with each other.
    #[test]
    fn separate_flows_keep_separate_epochs() {
        let a = segment_date("flow-a", 0, 2.0, ts("2026-08-27T00:00:02.000Z"));
        let b = segment_date("flow-b", 0, 2.0, ts("2026-08-27T05:00:02.000Z"));
        assert_ne!(a, b, "two flows were given one epoch");
    }

    /// `secs_f` seconds after `base`.
    fn at(base: chrono::DateTime<chrono::Utc>, secs_f: f64) -> chrono::DateTime<chrono::Utc> {
        base + secs(secs_f)
    }

    /// The low-latency path reads the flow clock; it must never steer it.
    ///
    /// `publish_ll_hls` dates the segment that has just *opened*, and it runs
    /// on every chunk emission. Routed through the close-time arithmetic —
    /// which is what it did — the first of those calls founds the flow's epoch
    /// on a sample that is `segment_duration - chunk_duration` early, and the
    /// flow then publishes every date early by that much for as long as it
    /// runs: 1.8 s at 2 s segments and 200 ms chunks, from segment zero,
    /// never converging. The suite was green throughout, because every date
    /// test called the close path only.
    #[test]
    fn the_low_latency_path_publishes_open_segments_on_wall_time() {
        let flow = "flow-ll-open";
        let seg = 2.0;
        let chunk = 0.2;
        // Wall clock at media position 0. The pipeline delay is left at zero
        // so the assertion is about the content's own time rather than about
        // the constant the epoch's founding sample carries.
        let origin = ts("2026-08-27T00:00:00.000Z");

        let mut dated_open = 0usize;
        for n in 0..8u64 {
            let base = n * 180_000;
            let opened = at(origin, n as f64 * seg);
            // Every chunk emission republishes the manifest, and with it the
            // in-progress row for this segment.
            let mut chunks = 1u32;
            while chunks as f64 * chunk <= seg {
                let now = at(opened, chunks as f64 * chunk);
                if let Some((got, disc)) = open_segment_date(flow, base, seg, now) {
                    assert!(!disc, "segment {n} chunk {chunks}: the read-only path invented a discontinuity");
                    let err = (got - opened).num_milliseconds();
                    assert!(
                        err.abs() <= 5,
                        "segment {n} chunk {chunks}: the in-progress row is {err}ms off its own content"
                    );
                    dated_open += 1;
                }
                chunks += 1;
            }
            // Then it closes — the only sample allowed to steer the clock.
            let (closed, disc) = segment_date_marking(flow, base, seg, at(opened, seg));
            let err = (closed - opened).num_milliseconds();
            assert!(err.abs() <= 5, "segment {n} closed {err}ms off its own content");
            assert!(!disc, "segment {n} claimed a discontinuity");
        }
        // The first segment carries no date — nothing has closed on this flow
        // yet — and all ten publishes of every segment after it do.
        assert_eq!(
            dated_open, 70,
            "the in-progress row stopped carrying a date after the clock existed"
        );
    }

    /// Reading the clock must leave no trace in it.
    ///
    /// The old low-latency call also *remembered* the epoch it dated the open
    /// segment with, so when that segment genuinely closed two seconds later
    /// the honest sample hit the per-segment cache and was discarded. The bias
    /// could not correct itself even in principle.
    #[test]
    fn dating_an_open_segment_leaves_the_clock_untouched() {
        let origin = ts("2026-08-27T00:00:00.000Z");
        let mut clock = FlowClock::new(0, 2.0, at(origin, 2.0));
        let epoch_before = clock.epoch;
        for j in 1..=10 {
            let _ = clock.date_open_segment(180_000, 2.0, at(origin, 2.0 + j as f64 * 0.2));
        }
        assert_eq!(clock.epoch, epoch_before, "reading the clock steered it");
        assert!(clock.recent.is_empty(), "reading the clock filled the per-segment cache");
        // So the sample taken when the segment does close is the one that
        // decides, and it puts the date on the content.
        let (got, _) = clock.date_closed_segment(180_000, 2.0, at(origin, 4.0), "flow-readonly");
        assert_eq!(got, at(origin, 2.0), "the close-time sample was discarded");
    }

    /// A low-latency rendition must not drag its plain sibling off wall time.
    ///
    /// They share one epoch by design — that is what makes them agree about a
    /// frame — so a bias introduced by either is published by both. Asserting
    /// only that the two agree cannot see it: they agreed while both were
    /// 1.8 s early.
    #[test]
    fn a_low_latency_rendition_does_not_drag_its_plain_sibling() {
        let flow = "flow-ll-sibling";
        let seg = 2.0;
        let origin = ts("2026-08-27T00:00:00.000Z");
        let mut last_ll = None;
        let mut last_plain = None;
        let mut last_opened = origin;
        for n in 0..30u64 {
            let base = n * 180_000;
            let opened = at(origin, n as f64 * seg);
            // The low-latency output republishes on every 200 ms chunk.
            for j in 1..=10 {
                let _ = open_segment_date(flow, base, seg, at(opened, j as f64 * 0.2));
            }
            let (ll, _) = segment_date_marking(flow, base, seg, at(opened, seg));
            // The plain rendition closes the same content 45 ms later, as
            // measured on the rig.
            let (plain, _) = segment_date_marking(
                flow,
                base,
                seg,
                at(opened, seg) + chrono::Duration::milliseconds(45),
            );
            last_ll = Some(ll);
            last_plain = Some(plain);
            last_opened = opened;
        }
        assert_eq!(last_ll, last_plain, "the two renditions disagree about one segment");
        let err = (last_plain.unwrap() - last_opened).num_milliseconds();
        assert!(
            err.abs() <= 5,
            "both renditions agree on a date {err}ms off the content — the whole flow is dragged"
        );
    }

    /// A restart is re-anchored even when the segment it lands on is still
    /// remembered.
    ///
    /// The per-segment cache used to be consulted before the discontinuity
    /// test, so a source coming back at PTS 0 while `0` was one of the sixteen
    /// remembered positions was answered from the hour-old epoch: no
    /// re-anchor, no `recent.clear()`, no log line, nothing on the Events
    /// page. Sixteen entries is about 32 s of a 2 s-segment flow — exactly
    /// when a flapping source comes back.
    #[test]
    fn a_restart_onto_a_remembered_segment_still_re_anchors() {
        let flow = "flow-restart-cached";
        segment_date_marking(flow, 0, 2.0, ts("2026-08-27T00:00:02.000Z"));
        segment_date_marking(flow, 180_000, 2.0, ts("2026-08-27T00:00:04.000Z"));
        // An hour later the source restarts, back at a position the cache
        // still holds.
        let (after, disc) = segment_date_marking(flow, 0, 2.0, ts("2026-08-27T01:00:02.000Z"));
        assert!(disc, "a restart onto a remembered position was absorbed silently");
        assert_eq!(
            after,
            ts("2026-08-27T01:00:00.000Z"),
            "the restart was dated from the stale epoch"
        );
    }

    /// A rendition added after a 33-bit PTS wrap joins the flow's timeline
    /// rather than fighting it.
    ///
    /// `PtsUnwrap` is per output and counts wraps from whenever that output
    /// started, so a rendition added — or restarted by an `UpdateFlow` — after
    /// the 26 h 30 m wrap reports the same content 2^33 ticks below its
    /// sibling. Uncorrected, each sample re-anchored the other: `recent`
    /// permanently empty, the renditions unable to agree, the epoch collapsed
    /// back to `now - seg_secs`, and the node logging a source restart twice
    /// per segment forever.
    #[test]
    fn a_rendition_added_after_a_pts_wrap_joins_the_timeline() {
        let flow = "flow-wrap";
        let lap = PTS_LAP_90K;
        let t0 = ts("2026-08-28T03:00:10.000Z");
        // The established output has counted the wrap.
        let (a, disc_a) = segment_date_marking(flow, lap + 900_000, 2.0, t0);
        assert!(!disc_a);
        // The new one has not, and reports the same content a lap lower.
        let (b, disc_b) = segment_date_marking(
            flow,
            900_000,
            2.0,
            t0 + chrono::Duration::milliseconds(45),
        );
        assert!(!disc_b, "the second rendition re-anchored the flow's timeline");
        assert_eq!(a, b, "the two renditions dated one segment 26.5 hours apart");
        // And the flow's own clock is intact: the next segment is 2 s on, not
        // re-founded on a collapsed epoch.
        let (c, disc_c) = segment_date_marking(flow, lap + 1_080_000, 2.0, at(t0, 2.0));
        assert!(!disc_c);
        let step = (c - a).num_milliseconds();
        assert!(
            (step - 2000).abs() <= (EPOCH_SLEW_SECS * 1000.0) as i64,
            "the published clock lost the timeline: {step}ms for a 2 s segment"
        );
    }

    /// The lap correction moves whole laps and nothing else.
    ///
    /// It is arithmetic rather than a subtract-until-in-range loop, so the
    /// distances that actually occur are worth pinning: one lap for a
    /// rendition added just after a wrap, hundreds for an output restarted
    /// late in a long session, and zero for anything a restart produces.
    #[test]
    fn lap_alignment_moves_whole_laps_and_nothing_else() {
        let origin = ts("2026-08-27T00:00:00.000Z");
        let lap = PTS_LAP_90K;
        let clock = FlowClock::new(lap + 900_000, 2.0, at(origin, 2.0));
        // The flow's own position, and the ordinary step to the next segment.
        assert_eq!(clock.align(lap + 900_000), lap + 900_000);
        assert_eq!(clock.align(lap + 1_080_000), lap + 1_080_000);
        // A rendition that has counted one wrap fewer.
        assert_eq!(clock.align(900_000), lap + 900_000);
        // And one that has counted several hundred fewer — an output added a
        // year into the flow.
        let far = FlowClock::new(300 * lap + 900_000, 2.0, at(origin, 2.0));
        assert_eq!(far.align(900_000), 300 * lap + 900_000);
        // A restart moves by minutes or hours, nowhere near a lap, so it is
        // left alone for the re-anchor test to catch.
        assert_eq!(clock.align(lap), lap);
    }

    /// A source the slew cannot track snaps, and the snap is declared.
    ///
    /// The epoch corrects by at most 5 ms per segment, so a source further out
    /// than ~2500 ppm falls behind until the error crosses
    /// `EPOCH_REANCHOR_SECS` and the clock jumps ten seconds at once. That is
    /// not a bug to remove — the alternative is an unbounded lie — but it is a
    /// real break in the published timeline, and every row on either side of
    /// it still advertises a clean `EXTINF` step. RFC 8216 §6.2.1 puts the
    /// obligation to say so on the server.
    #[test]
    fn a_source_outside_the_slew_band_declares_its_snap() {
        let flow = "flow-snap";
        let seg = 2.0;
        let origin = ts("2026-08-27T00:00:00.000Z");
        // 10 000 ppm — ordinary for a poor contribution encoder, and four
        // times what the slew can absorb.
        let ppm = 10_000.0;
        let mut snapped = 0usize;
        let mut prev: Option<chrono::DateTime<chrono::Utc>> = None;
        for n in 0..900u64 {
            let wall = at(origin, (n as f64 + 1.0) * seg * (1.0 + ppm / 1e6));
            let (got, disc) = segment_date_marking(flow, n * 180_000, seg, wall);
            if disc {
                snapped += 1;
            } else if let Some(p) = prev {
                // Every row that does *not* carry the tag must be within a
                // slew step of a clean segment-length advance, which is what
                // makes the tag land on exactly the row that moved.
                let step = (got - p).num_milliseconds();
                assert!(
                    (step - 2000).abs() <= (EPOCH_SLEW_SECS * 1000.0) as i64,
                    "segment {n} moved {step}ms with no discontinuity declared"
                );
            }
            prev = Some(got);
        }
        assert_eq!(
            snapped, 1,
            "the epoch did not snap in 30 minutes of a 10 000 ppm source, so this test no longer covers the case it was written for"
        );
    }

    /// A low-latency segment is dated by the length it actually ran, not by
    /// the configured target.
    ///
    /// The segmenter cuts on the first IDR at or *past* the target, so the two
    /// are equal only when the GOP divides it — a 1.5 s GOP against a 2 s
    /// target gives 3 s segments, and a 5 s GOP against the same target gives
    /// 5 s ones. The low-latency close passed the nominal figure, which tells
    /// the clock the segment ended (actual - nominal) earlier than it did, so
    /// the epoch it founds is late by exactly that, from segment zero and
    /// permanently: the sample says the same wrong thing every time, so there
    /// is nothing for the slew to correct against. The plain path has always
    /// passed the real duration, which is why only low-latency outputs carried
    /// it — and why they then dragged their plain sibling, since the two share
    /// one epoch and the low-latency output normally dates first.
    #[test]
    fn a_low_latency_segment_is_dated_by_its_real_length() {
        let flow = "flow-ll-gop";
        // 2 s target, 1.5 s GOP: every segment closes at its second IDR, 3 s
        // in. Nominal minus actual is a full second.
        let nominal = 2.0;
        let real = 3.0;
        let ticks = (real * 90_000.0) as u64;
        let origin = ts("2026-08-27T00:00:00.000Z");
        for n in 0..20u64 {
            let base = n * ticks;
            let opened = at(origin, n as f64 * real);
            let entry = closed_ll_entry(
                flow,
                n,
                default_segment_uri(n),
                base,
                Some(base + ticks),
                nominal,
                at(opened, real),
                0,
                false,
            );
            assert!(
                (entry.duration_secs - real).abs() < 1e-9,
                "segment {n} advertises #EXTINF:{:.3} for a {real}s segment",
                entry.duration_secs
            );
            let pdt = entry.program_date_time.expect("a closed row is always dated");
            let err = (pdt - opened).num_milliseconds();
            assert!(
                err.abs() <= 5,
                "segment {n} is dated {err}ms off its own content — the nominal duration reached the flow clock"
            );
        }
    }

    /// A re-anchor reaches *both* renditions of a flow, not only the one that
    /// noticed it.
    ///
    /// The second rendition to close a segment lands within tens of
    /// milliseconds of the epoch the first has just re-anchored to, so it
    /// never trips the re-anchor test itself — it is answered from the
    /// per-segment cache, which used to hand back a hard-coded `false`. Both
    /// playlists then carried the identical hour-long jump, one tagged and one
    /// bare, and their `#EXT-X-DISCONTINUITY-SEQUENCE` counts diverged for the
    /// rest of the session. RFC 8216 §4.3.3.3 makes that count something a
    /// player carries across a rendition switch.
    #[test]
    fn a_re_anchor_reaches_both_renditions() {
        let flow = "flow-disc-both";
        let seg = 2.0;
        let origin = ts("2026-08-27T00:00:00.000Z");
        let mut main = CmafState::new();
        let mut proxy = CmafState::new();
        let mut tagged = 0usize;
        for n in 0..6u64 {
            // The source restarts at segment 3: PTS back to zero, an hour of
            // wall clock gone.
            let (base, closed_at) = if n < 3 {
                (n * 180_000, at(origin, (n + 1) as f64 * seg))
            } else {
                ((n - 3) * 180_000, at(origin, 3600.0 + (n - 2) as f64 * seg))
            };
            // The main rendition closes first; the proxy 45 ms later, as
            // measured on the rig.
            let (a_pdt, a_disc) = segment_date_marking(flow, base, seg, closed_at);
            let (b_pdt, b_disc) = segment_date_marking(
                flow,
                base,
                seg,
                closed_at + chrono::Duration::milliseconds(45),
            );
            assert_eq!(a_pdt, b_pdt, "segment {n}: the renditions disagree about the date");
            assert_eq!(
                a_disc, b_disc,
                "segment {n}: one rendition declared a discontinuity the other published in silence"
            );
            if a_disc {
                tagged += 1;
            }
            for (state, disc) in [(&mut main, a_disc), (&mut proxy, b_disc)] {
                state.playlist.push_back(M3u8Entry {
                    sequence_number: n,
                    duration_secs: seg,
                    uri: None,
                    parts: Vec::new(),
                    init_uri: None,
                    program_date_time: Some(a_pdt),
                    discontinuity: disc,
                });
            }
        }
        assert_eq!(tagged, 1, "the restart was not declared at all");

        // Both playlists carry the tag, on the same row.
        let rows_main: Vec<M3u8Entry> = main.playlist.iter().cloned().collect();
        let rows_proxy: Vec<M3u8Entry> = proxy.playlist.iter().cloned().collect();
        for (name, rows) in [("main", &rows_main), ("proxy", &rows_proxy)] {
            let p = build_hls_playlist(
                required_target_duration(seg, rows),
                rows,
                "init.mp4",
                0,
                None,
            );
            assert_eq!(
                p.matches("#EXT-X-DISCONTINUITY\n").count(),
                1,
                "the {name} rendition published the jump without a tag: {p}"
            );
        }

        // And once the tagged row ages out of the window, both reach the same
        // discontinuity sequence — which is the number a player carries when
        // it switches between them.
        main.trim_playlist(2);
        proxy.trim_playlist(2);
        assert_eq!(
            main.discontinuities_trimmed, proxy.discontinuities_trimmed,
            "the renditions disagree about #EXT-X-DISCONTINUITY-SEQUENCE"
        );
        assert_eq!(main.discontinuities_trimmed, 1);
    }

    /// Publishing a low-latency playlist must not found the flow's clock.
    ///
    /// This drives the real row-building path rather than the arithmetic under
    /// it, and that is the point: the fix for the open-segment bug is a call
    /// site, and both of the tests written for it called `open_segment_date`
    /// directly. Reverting the call site left every one of them green.
    #[test]
    fn a_low_latency_playlist_never_founds_the_flow_clock() {
        let flow = "flow-ll-founding";
        let entries = ll_playlist_entries(
            &VecDeque::new(),
            Some(OpenSegmentRow {
                sequence_number: 0,
                uri: "seg-00000.m4s",
                parts: &[],
                base_dts_90k: 0,
                    generation: 0,
                    breaks_chain: false,
            }),
            2.0,
            flow,
            ts("2026-08-27T00:00:00.200Z"),
        );
        assert_eq!(entries.len(), 1, "the in-progress row is missing");
        assert!(
            entries[0].program_date_time.is_none(),
            "an in-progress row was dated before the flow had closed anything"
        );
        assert!(!entries[0].discontinuity);
        assert!(
            lock_flow_clocks().get(flow).is_none(),
            "publishing an in-progress row founded the flow's clock — on a sample taken part-way into a segment"
        );
    }

    /// The whole low-latency publish sequence keeps the flow on wall time.
    ///
    /// Open, ten chunk publishes, close, thirty times over — through the same
    /// functions the output calls, so a revert of the read-only call site
    /// fails here rather than passing quietly. Routed through the close-time
    /// arithmetic the very first chunk publish founds the epoch 1.8 s early
    /// (`segment_duration - chunk_duration`), and every date the flow ever
    /// publishes carries it.
    #[test]
    fn publishing_a_low_latency_playlist_does_not_drag_the_flow_epoch() {
        let flow = "flow-ll-wired";
        let seg = 2.0;
        let chunk = 0.2;
        let origin = ts("2026-08-27T00:00:00.000Z");
        let mut closed: VecDeque<M3u8Entry> = VecDeque::new();
        let mut dated_open = 0usize;
        for n in 0..30u64 {
            let base = n * 180_000;
            let opened = at(origin, n as f64 * seg);
            let uri = default_segment_uri(n);
            // Every chunk emission rebuilds the manifest, and with it the
            // in-progress row. This is where the biased sample was taken.
            let mut chunks = 1u32;
            while chunks as f64 * chunk <= seg {
                let rows = ll_playlist_entries(
                    &closed,
                    Some(OpenSegmentRow {
                        sequence_number: n,
                        uri: &uri,
                        parts: &[],
                        base_dts_90k: base,
                    generation: 0,
                    breaks_chain: false,
                    }),
                    seg,
                    flow,
                    at(opened, chunks as f64 * chunk),
                );
                let row = rows.last().expect("the in-progress row");
                assert_eq!(row.sequence_number, n);
                if let Some(pdt) = row.program_date_time {
                    let err = (pdt - opened).num_milliseconds();
                    assert!(
                        err.abs() <= 5,
                        "segment {n} chunk {chunks}: the in-progress row is {err}ms off its own content"
                    );
                    dated_open += 1;
                }
                chunks += 1;
            }
            // Then it closes — the only sample allowed to steer the clock.
            let entry = closed_ll_entry(
                flow,
                n,
                uri,
                base,
                Some(base + 180_000),
                seg,
                at(opened, seg),
                0,
                false,
            );
            let pdt = entry.program_date_time.expect("a closed row is always dated");
            let err = (pdt - opened).num_milliseconds();
            assert!(err.abs() <= 5, "segment {n} closed {err}ms off its own content");
            assert!(!entry.discontinuity, "segment {n} claimed a discontinuity");
            closed.push_back(entry);
        }
        // The first segment carries no date — nothing has closed on this flow
        // yet — and all ten publishes of every segment after it do.
        assert_eq!(
            dated_open, 290,
            "the in-progress row stopped carrying a date after the clock existed"
        );
    }

    /// The length a closed low-latency row advertises comes from the
    /// segmenter, not from a number the caller happened to have.
    ///
    /// `closed_ll_entry` takes "where the next segment starts" as an argument,
    /// and both tests that drive it hand it a literal `Some(base + ticks)`.
    /// That leaves the derivation itself — `CmafState::closed_segment_end_dts_90k`,
    /// reading the segmenter that `push()` has already advanced — with no test
    /// caller at all: reverting it to `None` restores the whole bug (every row
    /// dated by the configured target, a second early for a 1.5 s GOP and
    /// three for a 5 s one, permanently and from segment zero) with the suite
    /// green. So this test drives a real [`VideoSegmenter`] and asks the state
    /// the same question the output asks it.
    #[test]
    fn the_closed_row_takes_its_length_from_the_segmenter() {
        let flow = "flow-ll-close-wired";
        // 2 s configured target, 1.5 s GOP: the segmenter cuts on the first
        // IDR at or past the target, so every segment runs 3 s.
        let nominal = 2.0;
        let gop_90k = 135_000u64;
        let origin = ts("2026-08-27T00:00:00.000Z");

        let mut state = CmafState::new();
        state.video_seg = Some(VideoSegmenter::new(
            VideoTrack::from_h264(vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE]),
            nominal,
        ));
        let idr = vec![vec![0x65, 0xB8]];

        let mut closed = 0usize;
        for n in 0..12u64 {
            let outcome = state
                .video_seg
                .as_mut()
                .expect("segmenter")
                .push(&idr, n * gop_90k, true);
            let Some(seg) = outcome.completed_video else {
                continue;
            };
            // Exactly the production route: the segmenter has already moved on,
            // and the state is asked where the segment that just closed ended.
            let next_base = state.closed_segment_end_dts_90k();
            let opened = at(origin, seg.base_dts_90k as f64 / 90_000.0);
            let real_secs = seg.duration_90k as f64 / 90_000.0;
            let row = closed_ll_entry(
                flow,
                seg.sequence_number,
                default_segment_uri(seg.sequence_number),
                seg.base_dts_90k,
                next_base,
                nominal,
                at(opened, real_secs),
                0,
                false,
            );
            assert!(
                (row.duration_secs - 3.0).abs() < 1e-9,
                "segment {} advertises #EXTINF:{:.3} for a 3 s segment",
                seg.sequence_number,
                row.duration_secs
            );
            let pdt = row.program_date_time.expect("a closed row is always dated");
            let err = (pdt - opened).num_milliseconds();
            assert!(
                err.abs() <= 5,
                "segment {} is dated {err}ms off its own content — the nominal \
                 duration reached the flow clock",
                seg.sequence_number
            );
            closed += 1;
        }
        assert_eq!(closed, 5, "the segmenter did not close the segments this test is about");
    }

    /// The in-progress row never advertises less media than the parts listed
    /// underneath it.
    ///
    /// Its length is genuinely unknown — the IDR that ends the segment has not
    /// arrived — so the row carries a floor rather than a guess: the greater of
    /// the configured target and what the parts already published account for.
    /// Pinned at the nominal target, a 5 s segment with 200 ms chunks
    /// published `#EXTINF:2.000` with twenty-five `#EXT-X-PART:DURATION=0.200`
    /// rows beneath it, in the same playlist, accounting for 5 s.
    #[test]
    fn the_in_progress_row_covers_the_parts_it_lists() {
        let flow = "flow-ll-open-parts";
        let seg = 2.0;
        let now = ts("2026-08-27T00:00:05.000Z");
        let part = |i: usize| HlsPartEntry {
            uri: format!("seg-00000.m4s?part={i}"),
            duration_secs: 0.2,
            independent: i == 0,
        };

        // Early in the segment the parts account for less than the target, and
        // the target is the better floor: the segment cannot close before it.
        let few: Vec<HlsPartEntry> = (0..3).map(part).collect();
        let rows = ll_playlist_entries(
            &VecDeque::new(),
            Some(OpenSegmentRow {
                sequence_number: 0,
                uri: "seg-00000.m4s",
                parts: &few,
                base_dts_90k: 0,
                    generation: 0,
                    breaks_chain: false,
            }),
            seg,
            flow,
            now,
        );
        assert!((rows[0].duration_secs - seg).abs() < 1e-9, "{}", rows[0].duration_secs);

        // A 5 s GOP against the same 2 s target: twenty-five parts are on the
        // origin and the row has to say so.
        let many: Vec<HlsPartEntry> = (0..25).map(part).collect();
        let rows = ll_playlist_entries(
            &VecDeque::new(),
            Some(OpenSegmentRow {
                sequence_number: 0,
                uri: "seg-00000.m4s",
                parts: &many,
                base_dts_90k: 0,
                    generation: 0,
                    breaks_chain: false,
            }),
            seg,
            flow,
            now,
        );
        let advertised = rows[0].duration_secs;
        let listed: f64 = rows[0].parts.iter().map(|p| p.duration_secs).sum();
        assert!(
            advertised + 1e-9 >= listed,
            "the row advertises {advertised:.3}s over parts accounting for {listed:.3}s"
        );
        assert!((advertised - 5.0).abs() < 1e-9, "{advertised}");

        // And the playlist that carries it advertises a target the row fits
        // inside, rather than the 2 it would have inherited from the config.
        let ll = LowLatencyHints {
            part_target_secs: 0.2,
            can_block_reload: true,
        };
        let p = build_hls_playlist(
            required_target_duration(seg, &rows),
            &rows,
            "init.mp4",
            0,
            Some(&ll),
        );
        assert!(p.contains("#EXT-X-TARGETDURATION:5"), "{p}");
        assert!(p.contains("#EXTINF:5.000,"), "{p}");
        assert_eq!(p.matches("#EXT-X-PART:").count(), 25, "{p}");
    }

    /// `#EXT-X-TARGETDURATION` rises with the window and never falls back.
    ///
    /// It has to rise the moment a row longer than the advertised target
    /// enters, or the playlist breaks RFC 8216 §4.3.3.1 against a row it is
    /// listing. It must not fall when that row is trimmed: a player reads the
    /// value once and sizes its reload cadence, its buffer and its hold-back
    /// from it, so a value that shrinks between reloads retracts a decision it
    /// has already acted on. A source alternating GOP lengths would otherwise
    /// move the tag on every trim.
    #[test]
    fn the_target_duration_rises_with_the_window_and_stays_risen() {
        let row = |seq: u64, dur: f64| M3u8Entry {
            sequence_number: seq,
            duration_secs: dur,
            uri: None,
            parts: Vec::new(),
            init_uri: None,
            program_date_time: None,
            discontinuity: false,
        };
        let mut state = CmafState::new();
        let steady = [row(0, 2.0), row(1, 2.0)];
        assert_eq!(state.advertised_target_duration(2.0, &steady), 2);
        // A long segment arrives — one IDR late, or a source that changed GOP.
        let with_long = [row(1, 2.0), row(2, 5.0)];
        assert_eq!(state.advertised_target_duration(2.0, &with_long), 5);
        // It rolls out of the window again, and the tag stays where it is.
        let after = [row(3, 2.0), row(4, 2.0)];
        assert_eq!(
            state.advertised_target_duration(2.0, &after),
            5,
            "the target dropped back under a player that had already read it"
        );
        // The high-water is what holds it, not the config: a fresh output with
        // the same config and the same rows advertises the configured target.
        assert_eq!(CmafState::new().advertised_target_duration(2.0, &after), 2);
    }

    /// Two renditions agree across a re-anchor while their skew is under one
    /// segment — and one segment is where that stops.
    ///
    /// The invariant RFC 8216 §4.3.3.3 needs (both renditions tag the same row,
    /// so a player carries one `#EXT-X-DISCONTINUITY-SEQUENCE` across a switch)
    /// is conditional on how far apart in wall time the two renditions close
    /// the same segment, and the condition is not stated anywhere the reader of
    /// `recent` would find it.
    ///
    /// The bound is one segment duration, measured against this model at a
    /// restart on segment 20: 0.045 s, 0.5 s, 1.5 s and 1.9 s of skew agree
    /// exactly; 2.5 s gives two date and two flag disagreements (the leader
    /// tags rows 20 and 21, the laggard tags 19 and 21) and 8 s gives six.
    /// `reanchor` clears `recent`, so a laggard that still has a *pre*-restart
    /// segment to close when the leader re-anchors finds an empty cache, takes
    /// a fresh sample implying the old epoch, and re-anchors back — after which
    /// the two take turns re-anchoring each other. It is reported, at least:
    /// three re-anchors inside a minute escalate to the WARN that names this
    /// exact cause.
    #[test]
    fn renditions_agree_across_a_re_anchor_within_a_segment_of_skew() {
        let seg = 2.0;
        let restart_at = 20u64;
        for skew in [0.045f64, 0.5, 1.9] {
            let flow = format!("flow-skew-{skew}");
            let origin = ts("2026-08-27T00:00:00.000Z");
            // Both renditions close every segment, the laggard `skew` later,
            // interleaved by wall clock exactly as the two outputs would run.
            let mut events: Vec<(f64, bool, u64)> = Vec::new();
            for n in 0..26u64 {
                let closed = (n + 1) as f64 * seg;
                events.push((closed, true, n));
                events.push((closed + skew, false, n));
            }
            events.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("finite"));

            let mut leader = std::collections::HashMap::new();
            let mut laggard = std::collections::HashMap::new();
            for (t, is_leader, n) in events {
                // The source restarts at segment 20: PTS back to zero while
                // the wall clock runs on.
                let base = if n < restart_at {
                    n * 180_000
                } else {
                    (n - restart_at) * 180_000
                };
                let got = segment_date_marking(&flow, base, seg, at(origin, t));
                if is_leader {
                    leader.insert(n, got);
                } else {
                    laggard.insert(n, got);
                }
            }
            let mut tagged = 0usize;
            for n in 0..26u64 {
                let (a_pdt, a_disc) = leader[&n];
                let (b_pdt, b_disc) = laggard[&n];
                assert_eq!(
                    a_pdt, b_pdt,
                    "skew {skew}s, segment {n}: the renditions disagree about the date"
                );
                assert_eq!(
                    a_disc, b_disc,
                    "skew {skew}s, segment {n}: one rendition declared a discontinuity the other published in silence"
                );
                if a_disc {
                    tagged += 1;
                }
            }
            assert_eq!(
                tagged, 1,
                "skew {skew}s: the restart was declared {tagged} times rather than once"
            );
        }
    }

    /// A segment that runs far past the target keeps its date to the close.
    ///
    /// `date_open_segment` accepts `now` only inside
    /// `EPOCH_REANCHOR_SECS + seg_secs` of the segment's own start — outside
    /// that it is a timeline the clock does not describe, and no date is
    /// better than an hour-stale one. Handing it the nominal target fixed that
    /// ceiling at 12 s whatever the segment was doing, so a 15 s segment
    /// dropped its `#EXT-X-PROGRAM-DATE-TIME` from t = 12.2 s to the close:
    /// the last fifteen manifest publishes of every segment, silently, with a
    /// dated row before it and a dated row after it.
    #[test]
    fn a_long_segments_in_progress_row_keeps_its_date() {
        let flow = "flow-ll-long-gop";
        let nominal = 2.0;
        let real = 15.0;
        let origin = ts("2026-08-27T00:00:00.000Z");
        // One closed segment, so the flow has a clock to read.
        segment_date_marking(flow, 0, real, at(origin, real));

        let base = (real * 90_000.0) as u64;
        let mut dated = 0usize;
        let mut chunks = 1u32;
        while chunks as f64 * 0.2 <= real {
            let parts: Vec<HlsPartEntry> = (0..chunks)
                .map(|i| HlsPartEntry {
                    uri: format!("seg-00001.m4s?part={i}"),
                    duration_secs: 0.2,
                    independent: i == 0,
                })
                .collect();
            let rows = ll_playlist_entries(
                &VecDeque::new(),
                Some(OpenSegmentRow {
                    sequence_number: 1,
                    uri: "seg-00001.m4s",
                    parts: &parts,
                    base_dts_90k: base,
                    generation: 0,
                    breaks_chain: false,
                }),
                nominal,
                flow,
                at(origin, real + chunks as f64 * 0.2),
            );
            let row = rows.last().expect("the in-progress row");
            assert!(
                row.program_date_time.is_some(),
                "the in-progress row lost its date {:.1}s into a {real}s segment",
                chunks as f64 * 0.2
            );
            dated += 1;
            chunks += 1;
        }
        assert_eq!(dated, 75, "the segment was not driven to its close");
    }

    /// The discontinuity sequence counts what has aged out of the window.
    #[test]
    fn trimming_a_discontinuous_row_advances_the_discontinuity_sequence() {
        let mut state = CmafState::new();
        for n in 0..4u64 {
            state.playlist.push_back(M3u8Entry {
                sequence_number: n,
                duration_secs: 2.0,
                uri: None,
                parts: Vec::new(),
                init_uri: None,
                program_date_time: None,
                discontinuity: n == 1,
            });
        }
        state.trim_playlist(4);
        assert_eq!(state.discontinuities_trimmed, 0, "nothing had left the window yet");
        state.trim_playlist(2);
        assert_eq!(
            state.discontinuities_trimmed, 1,
            "the tagged row left the window without being counted"
        );
        // It only ever grows, and a trim that drops nothing changes nothing.
        state.trim_playlist(2);
        assert_eq!(state.discontinuities_trimmed, 1);
    }
}
