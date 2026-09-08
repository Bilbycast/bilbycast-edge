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
    build_dash_mpd, build_hls_playlist, default_segment_uri, required_target_duration,
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
/// Returns the restored rows and the sequence number to carry on from.
async fn restore_published_window(
    base: &str,
    auth: Option<&str>,
    limit: usize,
) -> Option<(VecDeque<M3u8Entry>, u64)> {
    let body = clips::fetch_manifest(base, auth).await.ok()?;
    let text = String::from_utf8_lossy(&body);
    parse_published_window(&text, limit)
}

/// The rows of a served media playlist, and the sequence to carry on from.
fn parse_published_window(text: &str, limit: usize) -> Option<(VecDeque<M3u8Entry>, u64)> {
    let mut rows: Vec<M3u8Entry> = Vec::new();
    let mut pdt: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut dur: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pdt = chrono::DateTime::parse_from_rfc3339(rest.trim())
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc));
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            dur = rest.trim_end_matches(',').trim().parse::<f64>().ok();
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
            if let (Some(seq), Some(d)) = (seq, dur) {
                rows.push(M3u8Entry {
                    sequence_number: seq,
                    duration_secs: d,
                    uri: Some(uri),
                    parts: Vec::new(),
                    program_date_time: pdt,
                    discontinuity: false,
                });
            }
            pdt = None;
            dur = None;
        }
    }
    if rows.is_empty() {
        return None;
    }
    // Never restore more than this output is configured to advertise, or the
    // first trim would drop most of it anyway and the manifest would briefly
    // claim a window the retention policy does not keep.
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
    let next_seq = rows.iter().map(|r| r.sequence_number).max().unwrap_or(0) + 1;
    Some((rows.into_iter().collect(), next_seq))
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
            init_upload_failing: false,
            init_last_upload: None,
            playlist: VecDeque::new(),
            restore_discontinuity: false,
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
    }
}

async fn run(
    config: &CmafOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: &EventSender,
    flow_id: &str,
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
            // The flow is how the exporter finds the local recording: a
            // recorder's `storage_id` defaults to the flow id, which is what
            // an exact cut is read from.
            flow_id.to_string(),
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
    match restore_published_window(
        &base_url,
        config.auth_token.as_deref(),
        config.playlist_window_segments(),
    )
    .await
    {
        Some((rows, next_seq)) => {
            tracing::info!(
                output = %config.id, segments = rows.len(), next_seq,
                "CMAF output: resumed the window the origin already holds"
            );
            state.playlist = rows;
            state.resume_seq = next_seq;
            state.restore_discontinuity = true;
        }
        // A fresh stream, or an origin that cannot be reached. Neither is a
        // reason to refuse to start: the cost is a shorter window.
        None => {}
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
                    match crate::timed_block_in_place!(
                        "cmaf.audio_silence_encode",
                        crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
                        { reenc.encode_silence_if_needed() }
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
    let Some(reenc) = state.audio_reencoder.as_mut() else {
        return;
    };
    reenc.mark_real_audio(pts);

    let mut frames_to_buffer: Vec<Vec<u8>> = Vec::new();
    for (planar, sr, ch) in decoded {
        match crate::timed_block_in_place!(
            "cmaf.audio_reencoder",
            crate::engine::perf::TRANSCODE_BLOCK_WARN_MS,
            { reenc.encode_planar(&planar, pts, sr, ch) }
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
    buffer_audio_frames(state, config, frames_to_buffer.into_iter().map(|f| (f, pts)));
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
    if state.video_reencoder.is_none()
        && !ensure_video_segmenter(
            state.resume_seq,
            &mut state.video_seg,
            codec,
            demuxer,
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

    // Publish init.mp4 the first time a video track is materialised, and
    // republish it periodically thereafter.
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

    if let Some(seg) = outcome.completed_video {
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
        state.playlist.push_back(M3u8Entry {
            sequence_number: seg.sequence_number,
            duration_secs: seg_secs,
            uri: Some(uri),
            parts: Vec::new(),
            program_date_time: Some(pdt),
            // The first row after a restored window always breaks the
            // timeline, whatever the flow clock believes: it was built fresh
            // with this process and has nothing to compare against.
            discontinuity: discontinuity
                || std::mem::take(&mut state.restore_discontinuity),
        });
        state.trim_playlist(config.playlist_window_segments());

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
    // Lazily construct the audio track on first AAC frame (we need the
    // demuxer-cached AAC config to derive the AudioSpecificConfig).
    if state.audio_seg.is_none() {
        let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() else {
            return;
        };
        let asc = aac_audio_specific_config(profile, sr_idx, ch_cfg);
        let sample_rate = codecs::sample_rate_from_index(sr_idx);
        let track = AudioTrack::aac(
            asc,
            sample_rate,
            ch_cfg as u16,
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
            "CMAF output '{}': audio track detected AAC sr={} ch={}",
            config.id, sample_rate, ch_cfg,
        );
    }

    // Phase 3: audio_encode hook — pump the source AAC frame through
    // the AudioReencoder and substitute the re-encoded frame(s).
    let frames_to_buffer: Vec<Vec<u8>> = if let Some(reenc) = state.audio_reencoder.as_mut() {
        // Reset the silent-fallback drop watchdog — real audio is flowing.
        reenc.mark_real_audio(pts);
        // Propagate the ADTS triplet from the demuxer so lazy decoder
        // construction inside AudioReencoder can succeed.
        if let Some((profile, sr_idx, ch_cfg)) = demuxer.cached_aac_config() {
            reenc.set_adts_config(profile, sr_idx, ch_cfg);
        }
        match crate::timed_block_in_place!(
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
        }
    } else {
        vec![data.to_vec()]
    };

    buffer_audio_frames(state, config, frames_to_buffer.into_iter().map(|f| (f, pts)));
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
    let v_track_codec = state.video_seg.as_ref()?.track.codec;
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
    *slot = Some(VideoSegmenter::new_from_seq(track, segment_duration_secs, resume_seq));
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

    if first && !with_audio && (state.audio_seg.is_some() || state.audio_ready) {
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

    match http_put(init_url, init_bytes, "video/mp4", config.auth_token.as_deref()).await {
        Ok(_) => {
            state.init_uploaded = true;
            state.init_upload_failing = false;
            state.init_last_upload = Some(std::time::Instant::now());
            if first {
                tracing::info!(
                    "CMAF output '{}': uploaded init.mp4 ({}x{}, {:?}{})",
                    config.id,
                    width,
                    height,
                    track_codec,
                    if with_audio { " + audio" } else { "" },
                );
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

fn ensure_video_segmenter(
    resume_seq: u64,
    slot: &mut Option<VideoSegmenter>,
    codec: VideoCodec,
    demuxer: &TsDemuxer,
    segment_duration_secs: f64,
    output_id: &str,
) -> bool {
    if slot.is_some() {
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
    *slot = Some(VideoSegmenter::new_from_seq(track, segment_duration_secs, resume_seq));
    true
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

    // Publish init.mp4 if it is due — first time, or a periodic republish.
    // Chunks are meaningless to a player that cannot fetch `#EXT-X-MAP`, so
    // nothing is emitted until it has landed at least once.
    if !publish_init_if_due(
        state,
        config,
        init_url,
        InitEncryption::Never,
        AudioPolicy::Never,
        event_sender,
        flow_id,
    )
    .await
    {
        return;
    }

    // On a new segment boundary, finalise the previous LL PUT and open
    // a new one.
    if outcome.new_segment_started {
        // Close previous LL segment if any.
        if let Some(ll) = state.ll_current.take() {
            let uri = ll.uri.clone();
            let seq = ll.sequence_number;
            let base_dts_90k = ll.base_dts_90k;
            let finish = ll.handle.finish().await;
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
                    event_sender.emit_flow(
                        EventSeverity::Warning,
                        category::CMAF,
                        format!("CMAF output '{}': LL PUT failed: {e}", config.id),
                        flow_id,
                    );
                }
            }
            // Where the segment ended, from the segmenter — see
            // `CmafState::closed_segment_end_dts_90k`, which is a named
            // function precisely so this derivation is reachable from a test.
            let next_base_dts_90k = state.closed_segment_end_dts_90k();
            state.playlist.push_back(closed_ll_entry(
                flow_id,
                seq,
                uri,
                base_dts_90k,
                next_base_dts_90k,
                config.segment_duration_secs,
                closed_at,
            ));
            state.trim_playlist(config.playlist_window_segments());
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
            match ll.handle.send_chunk(bytes) {
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
                Err(()) => {
                    // Backpressure: ingest is too slow. Abort the PUT,
                    // discard accumulated samples for this segment, and
                    // wait for the next IDR to open a fresh segment.
                    tracing::warn!(
                        "CMAF output '{}': LL ingest stall, aborting seg {}",
                        config.id, ll.sequence_number
                    );
                    event_sender.emit_flow(
                        EventSeverity::Warning,
                        category::CMAF,
                        format!(
                            "CMAF output '{}': LL chunk enqueue full (seg {}) — aborting",
                            config.id, ll.sequence_number
                        ),
                        flow_id,
                    );
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
            program_date_time: dated.map(|(pdt, _)| pdt),
            // This path takes no sample, so it discovers no discontinuity of
            // its own. What it can carry is one a *sibling* rendition already
            // found under this segment: that re-anchor has already moved the
            // date above, and a moved date published without the tag is the
            // contradiction the tag exists to close.
            discontinuity: dated.is_some_and(|(_, disc)| disc),
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
fn closed_ll_entry(
    flow_id: &str,
    sequence_number: u64,
    uri: String,
    base_dts_90k: u64,
    next_base_dts_90k: Option<u64>,
    nominal_segment_secs: f64,
    now: chrono::DateTime<chrono::Utc>,
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
        program_date_time: Some(pdt),
        discontinuity,
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
    let body = build_hls_playlist(
        target_duration,
        &entries,
        init_name,
        state.discontinuities_trimmed,
        Some(&hints),
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
        let body = build_hls_playlist(
            target_duration,
            &entries,
            init_name,
            state.discontinuities_trimmed,
            None,
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

        let (rows, next) = parse_published_window(m3u8, 100).expect("a window");
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
        let (trimmed, _) = parse_published_window(m3u8, 2).expect("a window");
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].sequence_number, 41, "the wrong end was trimmed");

        // A fresh stream has nothing to restore, and must not look like it does.
        assert!(parse_published_window("#EXTM3U
#EXT-X-VERSION:9
", 100).is_none());
        assert!(parse_published_window("", 100).is_none());
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
