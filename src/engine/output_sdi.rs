// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native SDI playout output via Blackmagic DeckLink (`sdi-decklink` feature).
//!
//! ```text
//! broadcast_rx ─► TsDemuxer ─┬─ video AUs ─► spawn_blocking worker:
//!                            │                  VideoDecoder ─► pack_uyvy422
//!                            │                  ─► write_video (card-clock paced)
//!                            └─ audio AUs ─►     AAC/MP2/AC-3/E-AC-3 decode
//!                                                ─► interleave i32 ─► write_audio
//!                                                   (timestamped, A/V-synced)
//! ```
//!
//! One ordered channel carries both essences so the worker anchors audio to
//! the first video frame. The card's completion callbacks pace video
//! (`write_video` blocks on the in-flight window); the bounded mpsc absorbs
//! jitter and overflow drops at the feeder — the broadcast channel is never
//! blocked.
//!
//! **A/V sync**: the first displayed video frame's PTS is schedule-time 0.
//! Audio for source-PTS `p` is scheduled (timestamped) at `p - anchor` on the
//! shared 90 kHz playout clock, so the card lip-syncs it to video. `audio_pos`
//! then advances by exact sample duration (drift-free), re-anchoring only on a
//! source discontinuity. Audio is fixed at 48 kHz (the card's rate); other
//! rates drop with an alarm (resampling is a follow-up). Opus and AC-4 are not
//! handled here — those flows play out video-only.
//!
//! No scaling — the configured `mode`'s raster must match the decoded video,
//! and other sizes drop with a throttled alarm so a source switch can never
//! emit a garbled picture. Failure modes mirror the SDI input: an unsupported
//! mode/device is fatal (re-opening cannot fix a config problem); a device
//! that vanishes mid-run re-opens with backoff; decode trouble drops + alarms.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::models::SdiOutputConfig;
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::OutputStatsAccumulator;

#[cfg(feature = "media-codecs")]
use crate::engine::audio_decode::AacDecoder;
#[cfg(feature = "media-codecs")]
use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
#[cfg(feature = "media-codecs")]
use decklink_rs::{DecklinkPixelFormat, DecklinkPlayout, DecklinkPlayoutConfig};
#[cfg(feature = "media-codecs")]
use video_codec::{AudioDecoderCodec, VideoCodec};
#[cfg(feature = "media-codecs")]
use video_engine::AudioDecoder as FfAudioDecoder;

/// DeckLink scheduled audio output is fixed at 48 kHz. A decoded track at any
/// other rate is dropped (resampling is a follow-up).
#[cfg(feature = "media-codecs")]
const SDI_AUDIO_RATE: u32 = 48_000;

/// Re-anchor the audio schedule position if a block's PTS-derived time diverges
/// from the running (sample-counted) position by more than this — a source
/// discontinuity, not accumulated drift.
#[cfg(feature = "media-codecs")]
const AUDIO_RESYNC_THRESHOLD_90K: i64 = 45_000; // 0.5 s

/// Backoff between playout re-open attempts after the device vanishes.
#[cfg(feature = "media-codecs")]
const SDI_PLAYOUT_REOPEN_BACKOFF: Duration = Duration::from_millis(500);

/// Minimum interval between repeated raster-mismatch / decode warnings so a
/// wrong-raster source cannot flood the event bus at frame rate.
#[cfg(feature = "media-codecs")]
const THROTTLE: Duration = Duration::from_secs(5);

/// One demuxed video access unit, reassembled into Annex B for the decoder.
#[cfg(feature = "media-codecs")]
struct VideoAu {
    annexb: Vec<u8>,
    codec: VideoCodec,
    is_keyframe: bool,
    /// Source PTS (90 kHz). The first keyframe's PTS anchors the A/V timeline.
    pts: u64,
}

/// One demuxed audio access unit, still coded — decoded in the blocking worker
/// so no codec work touches the async reactor.
#[cfg(feature = "media-codecs")]
struct AudioAu {
    /// Source PTS (90 kHz).
    pts: u64,
    payload: AudioPayload,
}

/// Coded audio + enough to construct/select its decoder in the worker.
#[cfg(feature = "media-codecs")]
enum AudioPayload {
    /// AAC frame (ADTS stripped) + the ADTS config tuple
    /// `(profile, sample_rate_index, channel_config)` from the demuxer.
    Aac { data: Vec<u8>, config: (u8, u8, u8) },
    /// MP2 / AC-3 / E-AC-3 elementary stream for the FFmpeg audio decoder.
    Ff {
        codec: AudioDecoderCodec,
        data: Vec<u8>,
    },
}

/// What the feeder hands the worker: video or audio, in TS arrival order on one
/// channel so the worker sees a single ordered stream (the first video keyframe
/// anchors audio scheduling).
#[cfg(feature = "media-codecs")]
enum PlayoutAu {
    Video(VideoAu),
    Audio(AudioAu),
    /// Upstream input switch (PMT version bump). Flush decoders so the next
    /// keyframe re-anchors cleanly instead of referencing the old stream.
    Discontinuity,
}

pub fn spawn_sdi_output(
    config: SdiOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) -> tokio::task::JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        #[cfg(feature = "media-codecs")]
        {
            run_sdi_output(config, rx, stats, cancel, event_sender, flow_id).await;
        }
        #[cfg(not(feature = "media-codecs"))]
        {
            let _ = (rx, stats, cancel, flow_id);
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!(
                    "SDI output '{}' requires the media-codecs feature \
                     (error_code: sdi_no_media_codecs)",
                    config.id
                ),
            );
        }
    })
}

#[cfg(feature = "media-codecs")]
async fn run_sdi_output(
    config: SdiOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
) {
    let ctx = format!(
        "SDI output '{}' (flow={} device='{}' mode={})",
        config.id, flow_id, config.device, config.mode
    );

    // Bounded hand-off to the blocking worker. Sized for ~0.3 s of media:
    // deep enough to ride out decoder hiccups, shallow enough that a stalled
    // card doesn't buffer seconds of latency. Carries video AND audio AUs in
    // one ordered stream so the worker anchors audio to the first video frame.
    let (au_tx, au_rx) = mpsc::channel::<PlayoutAu>(32);
    let want_audio = config.audio_channels > 0;

    let worker = {
        let ctx = ctx.clone();
        let config = config.clone();
        let stats = stats.clone();
        let cancel = cancel.clone();
        let event_sender = event_sender.clone();
        tokio::task::spawn_blocking(move || {
            playout_worker(&ctx, &config, au_rx, &stats, &cancel, &event_sender);
        })
    };

    // Feeder: broadcast → TS demux → access units. Never blocks — a full
    // hand-off channel drops the AU (the worker is behind the card's cadence,
    // and dropping a coded frame beats unbounded latency).
    let mut demuxer = TsDemuxer::new(config.program_number);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            rec = rx.recv() => match rec {
                Ok(packet) => {
                    let ts: &[u8] = if packet.is_raw_ts {
                        &packet.data
                    } else if packet.data.len() >= 12 {
                        &packet.data[12..]
                    } else {
                        continue;
                    };
                    for frame in demuxer.demux(ts) {
                        let au = match frame {
                            DemuxedFrame::H264 { nalus, is_keyframe, pts } => {
                                PlayoutAu::Video(VideoAu {
                                    annexb: annexb(&nalus),
                                    codec: VideoCodec::H264,
                                    is_keyframe,
                                    pts,
                                })
                            }
                            DemuxedFrame::H265 { nalus, is_keyframe, pts } => {
                                PlayoutAu::Video(VideoAu {
                                    annexb: annexb(&nalus),
                                    codec: VideoCodec::Hevc,
                                    is_keyframe,
                                    pts,
                                })
                            }
                            // MPEG-2 ES is fed to the decoder verbatim — no
                            // NALU framing exists in this codec.
                            DemuxedFrame::Mpeg2 { es, is_keyframe, pts } => {
                                PlayoutAu::Video(VideoAu {
                                    annexb: es,
                                    codec: VideoCodec::Mpeg2,
                                    is_keyframe,
                                    pts,
                                })
                            }
                            // Audio only when the output was configured for it.
                            // AAC is the SDI-native case (the SDI input muxes
                            // AAC-LC); MP2/AC-3/E-AC-3 cover DVB/ATSC flows.
                            // Opus (display-gated variant) and AC-4 (no decoder)
                            // are not handled — those flows play out video-only.
                            DemuxedFrame::Aac { data, pts } if want_audio => {
                                match demuxer.cached_aac_config() {
                                    Some(config) => PlayoutAu::Audio(AudioAu {
                                        pts,
                                        payload: AudioPayload::Aac { data, config },
                                    }),
                                    None => continue, // config not seen yet
                                }
                            }
                            DemuxedFrame::OtherAudio { stream_type, data, pts } if want_audio => {
                                match crate::engine::audio_decode::ff_codec_for_stream_type(
                                    stream_type,
                                ) {
                                    Some(codec) => PlayoutAu::Audio(AudioAu {
                                        pts,
                                        payload: AudioPayload::Ff { codec, data },
                                    }),
                                    None => continue, // AC-4 / unknown — no decoder
                                }
                            }
                            DemuxedFrame::Discontinuity => PlayoutAu::Discontinuity,
                            _ => continue,
                        };
                        if au_tx.try_send(au).is_err() {
                            stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                }
                Err(_) => break,
            },
        }
    }

    drop(au_tx);
    let _ = worker.await;
    info!(target: "sdi.out", "{ctx}: output stopped");
}

/// Rebuild Annex B from start-code-stripped NALUs.
#[cfg(feature = "media-codecs")]
fn annexb(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nalus.iter().map(|n| n.len() + 4).sum());
    for n in nalus {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

/// Open the DeckLink output, retrying until it succeeds, the flow is
/// cancelled, or the failure is one a retry can never fix (unsupported
/// mode/pixel-format ⇒ `None` with a Critical event — config problem).
#[cfg(feature = "media-codecs")]
fn open_playout(
    ctx: &str,
    config: &SdiOutputConfig,
    cancel: &CancellationToken,
    event_sender: &EventSender,
) -> Option<DecklinkPlayout> {
    let cfg = DecklinkPlayoutConfig {
        device: config.device.clone(),
        format: config.mode.clone(),
        width: 0,
        height: 0,
        frame_rate_num: 0,
        frame_rate_den: 0,
        pixel_format: DecklinkPixelFormat::Uyvy422,
        audio_channels: config.audio_channels,
        audio_sample_rate: SDI_AUDIO_RATE,
    };
    let mut announced = false;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        match DecklinkPlayout::open(cfg.clone()) {
            Ok(po) => {
                let (w, h) = po.video_dimensions();
                let (n, d) = po.video_frame_rate();
                info!(target: "sdi.out", "{ctx}: playout opened {w}x{h} @ {n}/{d}");
                event_sender.emit(
                    EventSeverity::Info,
                    category::FLOW,
                    format!(
                        "{ctx}: playout opened {w}x{h} @ {n}/{d} \
                         (error_code: sdi_playout_opened)"
                    ),
                );
                return Some(po);
            }
            // A mode/device combination the card refuses is a config
            // problem — retrying forever would just spin.
            Err(e @ decklink_rs::Error::Unsupported(_)) => {
                let msg = format!(
                    "{ctx}: playout refused: {e} (error_code: sdi_playout_mode_unsupported)"
                );
                tracing::error!(target: "sdi.out", "{msg}");
                event_sender.emit(EventSeverity::Critical, category::FLOW, msg);
                return None;
            }
            Err(e) => {
                if !announced {
                    announced = true;
                    warn!(target: "sdi.out", "{ctx}: playout open failed: {e}; retrying");
                    event_sender.emit(
                        EventSeverity::Warning,
                        category::FLOW,
                        format!(
                            "{ctx}: playout open failed: {e} — retrying until the \
                             device returns (error_code: sdi_playout_open_failed)"
                        ),
                    );
                }
                std::thread::sleep(SDI_PLAYOUT_REOPEN_BACKOFF);
            }
        }
    }
}

/// Blocking side: decode access units, repack video to UYVY422 and interleave
/// audio, schedule both on the card's clock. `write_video` blocking on the
/// in-flight window is what paces the whole pipeline to the SDI cadence; audio
/// is scheduled timestamped on the same 90 kHz timeline for hardware A/V sync.
#[cfg(feature = "media-codecs")]
fn playout_worker(
    ctx: &str,
    config: &SdiOutputConfig,
    mut au_rx: mpsc::Receiver<PlayoutAu>,
    stats: &Arc<OutputStatsAccumulator>,
    cancel: &CancellationToken,
    event_sender: &EventSender,
) {
    use video_engine::VideoDecoder;

    let Some(mut playout) = open_playout(ctx, config, cancel, event_sender) else {
        return; // cancelled, or fatally unsupported (already alarmed)
    };
    let (out_w, out_h) = playout.video_dimensions();
    let row_bytes = playout.row_bytes();
    let mut frame_buf = vec![0u8; row_bytes * out_h as usize];

    let mut decoder: Option<VideoDecoder> = None;
    let mut current_codec: Option<VideoCodec> = None;
    // Decoders need a parameter set before anything decodes; feeding P-frames
    // first just produces reference errors, so gate on the first keyframe.
    let mut seen_keyframe = false;
    // Per-output SDI playout telemetry, surfaced on `OutputStats.sdi_stats`.
    // The card's late/dropped counters are cumulative per playout session, so
    // both bases reset to 0 on a device re-open (below).
    let sdi = std::sync::Arc::new(crate::stats::collector::SdiPlayoutStats::default());
    stats.set_sdi_playout_stats(sdi.clone());
    let mut card_drops_base: u64 = 0;
    let mut card_late_base: u64 = 0;
    let mut last_mismatch_warn: Option<Instant> = None;
    let mut last_decode_warn: Option<Instant> = None;

    // ── audio state ──────────────────────────────────────────────────────
    // The first video keyframe's PTS is schedule-time 0; audio for source-PTS
    // `p` is placed at `p - anchor`. Audio arriving before the anchor (i.e.
    // before the first displayed frame) has no video to pair with and is
    // dropped. `audio_pos` then advances by exact sample duration (drift-free),
    // re-anchoring to the PTS-derived time only on a discontinuity.
    let audio_out_ch = config.audio_channels as usize; // 0 = video-only
    // Operator A/V trim on the 90 kHz card timeline (+ delays audio, - advances).
    let audio_offset_90k = config.audio_offset_ms as i64 * 90;
    let mut anchor_pts: Option<u64> = None;
    let mut aac_dec: Option<AacDecoder> = None;
    let mut ff_dec: Option<(AudioDecoderCodec, FfAudioDecoder)> = None;
    let mut audio_pos: Option<i64> = None;
    let mut audio_ibuf: Vec<i32> = Vec::new();
    let mut last_audio_warn: Option<Instant> = None;
    // Until the first audio block schedules successfully we're in startup: the
    // early blocks whose schedule time already passed (video preroll started
    // playback) are dropped by design — dropping past audio keeps lip-sync,
    // so those failures are expected and must not alarm.
    let mut audio_started = false;

    while let Some(item) = au_rx.blocking_recv() {
        if cancel.is_cancelled() {
            break;
        }

        let au = match item {
            PlayoutAu::Video(v) => v,
            PlayoutAu::Discontinuity => {
                // Input switch. Flush the video decoder and wait for a fresh
                // keyframe so it doesn't reference the old stream. Reset the
                // audio decoders (the new input may carry a different codec /
                // AAC config) and let audio re-anchor. The card's schedule
                // clock keeps running, so `anchor_pts` stays — output PTS is
                // regenerated continuous across switches by the edge's
                // clocking, so the shared timeline is unbroken.
                if let Some(d) = decoder.as_mut() {
                    d.flush();
                }
                seen_keyframe = false;
                aac_dec = None;
                ff_dec = None;
                audio_pos = None;
                continue;
            }
            PlayoutAu::Audio(a) => {
                // Can't place audio until the video timeline is anchored.
                let Some(anchor) = anchor_pts else { continue };
                decode_and_schedule_audio(
                    ctx,
                    &mut playout,
                    a,
                    anchor,
                    audio_out_ch,
                    audio_offset_90k,
                    &mut aac_dec,
                    &mut ff_dec,
                    &mut audio_pos,
                    &mut audio_started,
                    &mut audio_ibuf,
                    stats,
                    &mut last_audio_warn,
                    event_sender,
                );
                continue;
            }
        };

        // (Re)open the decoder on first use and on codec change.
        if current_codec != Some(au.codec) {
            match VideoDecoder::open_threaded(au.codec) {
                Ok(d) => {
                    decoder = Some(d);
                    current_codec = Some(au.codec);
                    seen_keyframe = false;
                }
                Err(e) => {
                    throttled(&mut last_decode_warn, || {
                        warn!(target: "sdi.out", "{ctx}: decoder open failed: {e}");
                        event_sender.emit(
                            EventSeverity::Warning,
                            category::FLOW,
                            format!(
                                "{ctx}: video decoder open failed: {e} \
                                 (error_code: sdi_playout_decode_failed)"
                            ),
                        );
                    });
                    continue;
                }
            }
        }
        if !seen_keyframe {
            if !au.is_keyframe {
                continue;
            }
            seen_keyframe = true;
            // First displayed frame → anchor the A/V timeline (set once).
            anchor_pts.get_or_insert(au.pts);
        }
        let dec = decoder.as_mut().expect("decoder opened above");

        if let Err(e) = dec.send_packet(&au.annexb) {
            throttled(&mut last_decode_warn, || {
                warn!(target: "sdi.out", "{ctx}: decode error: {e}");
            });
            continue;
        }
        while let Ok(frame) = dec.receive_frame() {
            let (w, h) = (frame.width(), frame.height());
            if (w, h) != (out_w, out_h) {
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                throttled(&mut last_mismatch_warn, || {
                    warn!(
                        target: "sdi.out",
                        "{ctx}: decoded {w}x{h} does not match the {out_w}x{out_h} \
                         playout mode; dropping frames"
                    );
                    event_sender.emit(
                        EventSeverity::Warning,
                        category::FLOW,
                        format!(
                            "{ctx}: decoded video is {w}x{h} but the configured mode \
                             is {out_w}x{out_h} — frames dropped; fix `mode` or the \
                             source (error_code: sdi_playout_raster_mismatch)"
                        ),
                    );
                });
                continue;
            }
            let Some((y, ys, u, us, v, vs)) = frame.yuv_planes() else {
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                throttled(&mut last_decode_warn, || {
                    warn!(
                        target: "sdi.out",
                        "{ctx}: decoded frame is not 8-bit planar YUV (pix_fmt {}); \
                         dropping frames",
                        frame.pixel_format(),
                    );
                });
                continue;
            };
            // Chroma layout from evidence, not enum-matching: yuv_planes()
            // returns exact-height slices, so rows = len / stride tells us
            // 4:2:0 (h/2) vs 4:2:2 (h) without new FFI surface.
            //
            // But yuv_planes() also returns Some(..) for 4:4:4 (chroma rows ==
            // h, indistinguishable from 4:2:2 by height alone) and for 10-bit
            // planar (chroma rows look 8-bit-shaped). pack_uyvy422 reads w/2
            // 8-bit chroma samples per row, so feeding it either would silently
            // corrupt colour rather than drop. Guard on chroma being both
            // 8-bit and horizontally subsampled: a 4:2:x 8-bit chroma plane has
            // byte-stride ~w/2 (always < w for a DeckLink raster ≥720 wide),
            // while 4:4:4-8bit and 10-bit-anything have byte-stride ≥ w.
            let chroma_rows = u.len().checked_div(us).unwrap_or(0);
            let chroma_half_width_8bit = us < w as usize;
            let chroma_420 = chroma_rows == (h as usize).div_ceil(2);
            let chroma_422 = chroma_rows == h as usize;
            if !chroma_half_width_8bit || !(chroma_420 || chroma_422) {
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                throttled(&mut last_decode_warn, || {
                    warn!(
                        target: "sdi.out",
                        "{ctx}: decoded chroma is not 8-bit 4:2:0/4:2:2 (pix_fmt {}); \
                         dropping frames — only 8-bit 4:2:0/4:2:2 playout is supported",
                        frame.pixel_format(),
                    );
                    event_sender.emit(
                        EventSeverity::Warning,
                        category::FLOW,
                        format!(
                            "{ctx}: decoded video chroma is unsupported for SDI playout \
                             (only 8-bit 4:2:0/4:2:2) — frames dropped \
                             (error_code: sdi_playout_chroma_unsupported)"
                        ),
                    );
                });
                continue;
            }

            pack_uyvy422(
                y,
                ys,
                u,
                us,
                v,
                vs,
                w,
                h,
                chroma_420,
                row_bytes,
                &mut frame_buf,
            );

            match playout.write_video(&frame_buf) {
                Ok(()) => {
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    stats
                        .bytes_sent
                        .fetch_add(frame_buf.len() as u64, Ordering::Relaxed);
                    sdi.frames_sent.fetch_add(1, Ordering::Relaxed);
                    // Late frames were displayed behind their slot but still
                    // shown — a scheduling-pressure signal, not lost picture.
                    // Track them separately; do NOT fold into packets_dropped.
                    let late = playout.late_frames();
                    if late > card_late_base {
                        sdi.frames_late
                            .fetch_add(late - card_late_base, Ordering::Relaxed);
                        card_late_base = late;
                    }
                    // Hard drops (card fell behind the SDI cadence) are real
                    // lost picture — surface in the SDI breakdown AND fold into
                    // the generic packets_dropped so every output view sees them.
                    let hard = playout.dropped_frames();
                    if hard > card_drops_base {
                        let d = hard - card_drops_base;
                        stats.packets_dropped.fetch_add(d, Ordering::Relaxed);
                        sdi.frames_dropped.fetch_add(d, Ordering::Relaxed);
                        card_drops_base = hard;
                    }
                }
                // Card is behind the SDI cadence or wedged (playback running
                // but not draining). Not a device failure — skip the frame and
                // re-check cancellation so a flow stop is honoured promptly
                // rather than parking this blocking thread on a dead card.
                Err(decklink_rs::Error::Busy) => {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    sdi.frames_dropped.fetch_add(1, Ordering::Relaxed);
                    if cancel.is_cancelled() {
                        return;
                    }
                }
                Err(decklink_rs::Error::Eof) => return,
                Err(e) => {
                    warn!(target: "sdi.out", "{ctx}: playout write failed: {e}; reopening");
                    event_sender.emit(
                        EventSeverity::Warning,
                        category::FLOW,
                        format!(
                            "{ctx}: playout write failed: {e} — reopening device \
                             (error_code: sdi_playout_lost)"
                        ),
                    );
                    drop(playout);
                    match open_playout(ctx, config, cancel, event_sender) {
                        Some(po) => {
                            playout = po;
                            // New session — the card's cumulative counters restart.
                            card_drops_base = 0;
                            card_late_base = 0;
                        }
                        None => return,
                    }
                }
            }
        }
    }
}

/// Run `f` at most once per [`THROTTLE`].
#[cfg(feature = "media-codecs")]
fn throttled(last: &mut Option<Instant>, f: impl FnOnce()) {
    if last.map(|t| t.elapsed() >= THROTTLE).unwrap_or(true) {
        *last = Some(Instant::now());
        f();
    }
}

/// Convert a planar f32 sample in `[-1.0, 1.0]` to DeckLink's 32-bit signed.
#[cfg(feature = "media-codecs")]
#[inline]
fn f32_to_i32(s: f32) -> i32 {
    (s.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32
}

/// Decode one audio AU and schedule the resulting sample block(s) on the card,
/// lip-synced to video via the shared 90 kHz timeline.
///
/// AAC yields one block per AU; the FFmpeg codecs may yield several — each is
/// scheduled contiguously by the advancing `audio_pos`.
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn decode_and_schedule_audio(
    ctx: &str,
    playout: &mut DecklinkPlayout,
    au: AudioAu,
    anchor_pts: u64,
    out_ch: usize,
    audio_offset_90k: i64,
    aac_dec: &mut Option<AacDecoder>,
    ff_dec: &mut Option<(AudioDecoderCodec, FfAudioDecoder)>,
    audio_pos: &mut Option<i64>,
    audio_started: &mut bool,
    ibuf: &mut Vec<i32>,
    stats: &Arc<OutputStatsAccumulator>,
    last_warn: &mut Option<Instant>,
    event_sender: &EventSender,
) {
    let pts = au.pts;
    match au.payload {
        AudioPayload::Aac { data, config } => {
            if aac_dec.is_none() {
                match AacDecoder::from_adts_config(config.0, config.1, config.2) {
                    Ok(d) => *aac_dec = Some(d),
                    Err(e) => {
                        throttled(last_warn, || {
                            warn!(target: "sdi.out", "{ctx}: AAC decoder init failed: {e}");
                        });
                        return;
                    }
                }
            }
            if let Some(d) = aac_dec.as_mut() {
                match d.decode_frame(&data) {
                    Ok(planar) => {
                        let (sr, ch) = (d.sample_rate(), d.channels() as usize);
                        schedule_audio_block(
                            ctx,
                            playout,
                            &planar,
                            sr,
                            ch,
                            out_ch,
                            audio_offset_90k,
                            pts,
                            anchor_pts,
                            audio_pos,
                            audio_started,
                            ibuf,
                            stats,
                            last_warn,
                        );
                    }
                    Err(_) => throttled(last_warn, || {
                        warn!(target: "sdi.out", "{ctx}: AAC decode error; audio block dropped");
                    }),
                }
            }
        }
        AudioPayload::Ff { codec, data } => {
            if ff_dec.as_ref().map(|(c, _)| *c) != Some(codec) {
                *ff_dec = FfAudioDecoder::open(codec).ok().map(|d| (codec, d));
            }
            let Some((_, d)) = ff_dec.as_mut() else {
                throttled(last_warn, || {
                    warn!(target: "sdi.out", "{ctx}: audio decoder open failed for {codec:?}");
                });
                return;
            };
            for frame_bytes in crate::engine::audio_decode::split_audio_codec_frames(&data, codec) {
                if d.send_packet(frame_bytes, pts as i64).is_err() {
                    continue;
                }
                while let Ok(decoded) = d.receive_frame() {
                    schedule_audio_block(
                        ctx,
                        playout,
                        &decoded.planar,
                        decoded.sample_rate,
                        decoded.channels as usize,
                        out_ch,
                        audio_offset_90k,
                        pts,
                        anchor_pts,
                        audio_pos,
                        audio_started,
                        ibuf,
                        stats,
                        last_warn,
                    );
                }
            }
        }
    }
    let _ = event_sender; // reserved for a future rate-unsupported alarm path
}

/// Place one decoded planar-f32 block on the card at its lip-synced schedule
/// time. `audio_pos` advances by the block's exact duration (drift-free);
/// a > 0.5 s divergence from the PTS-derived time re-anchors it (discontinuity).
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn schedule_audio_block(
    ctx: &str,
    playout: &mut DecklinkPlayout,
    planar: &[Vec<f32>],
    sample_rate: u32,
    dec_ch: usize,
    out_ch: usize,
    audio_offset_90k: i64,
    src_pts: u64,
    anchor_pts: u64,
    audio_pos: &mut Option<i64>,
    audio_started: &mut bool,
    ibuf: &mut Vec<i32>,
    stats: &Arc<OutputStatsAccumulator>,
    last_warn: &mut Option<Instant>,
) {
    // The card runs at a fixed 48 kHz; other rates would need SRC (a follow-up).
    if sample_rate != SDI_AUDIO_RATE {
        throttled(last_warn, || {
            warn!(
                target: "sdi.out",
                "{ctx}: decoded audio is {sample_rate} Hz, playout is fixed at \
                 {SDI_AUDIO_RATE} Hz; audio block dropped"
            );
        });
        return;
    }
    let frames = planar.first().map(|c| c.len()).unwrap_or(0);
    if frames == 0 || dec_ch == 0 {
        return;
    }

    let target = src_pts as i64 - anchor_pts as i64;
    let pos = match *audio_pos {
        None => {
            if target < 0 {
                return; // audio precedes the first video frame — no pair
            }
            target
        }
        Some(cur) => {
            if (target - cur).abs() > AUDIO_RESYNC_THRESHOLD_90K {
                target // discontinuity — re-anchor to source timing
            } else {
                cur
            }
        }
    };
    if pos < 0 {
        return;
    }
    // Apply the operator A/V trim to the scheduled card time only. `audio_pos`
    // keeps tracking the untrimmed `pos`, so the drift-free advance and the
    // discontinuity re-anchor are independent of the trim — a constant shift
    // that never accumulates.
    let sched = pos + audio_offset_90k;
    if sched < 0 {
        return; // a negative (advance) trim placed this block before stream start
    }

    // Interleave into the card's channel layout: copy the decoded channels,
    // zero-fill any extra, truncate any excess.
    ibuf.clear();
    ibuf.reserve(frames * out_ch);
    for f in 0..frames {
        for c in 0..out_ch {
            let s = planar
                .get(c)
                .and_then(|ch| ch.get(f))
                .copied()
                .unwrap_or(0.0);
            ibuf.push(f32_to_i32(s));
        }
    }

    match playout.write_audio(ibuf, sched) {
        Ok(()) => {
            *audio_started = true;
            stats
                .bytes_sent
                .fetch_add((ibuf.len() * 4) as u64, Ordering::Relaxed);
            *audio_pos = Some(pos + frames as i64 * 90_000 / sample_rate as i64);
        }
        Err(decklink_rs::Error::Eof) => {}
        // Before the first success we're in startup: blocks whose schedule time
        // already passed (video preroll began playback) are dropped by design,
        // which keeps lip-sync — don't alarm. After audio has started, a
        // failure is a real regression.
        Err(e) => {
            if *audio_started {
                throttled(last_warn, || {
                    warn!(target: "sdi.out", "{ctx}: audio schedule failed: {e}");
                });
            }
        }
    }
}

/// Interleave planar 8-bit YUV into UYVY422 (`U Y V Y` per two pixels), the
/// exact inverse of `sdi_io::unpack_uyvy422`.
///
/// * 4:2:2 sources map 1:1 — chroma row `r` feeds output row `r`.
/// * 4:2:0 sources upsample vertically by row duplication — chroma row `r/2`
///   feeds output rows `2r` and `2r+1`. Nearest-neighbour is correct here:
///   the card wants 4:2:2 and inventing intermediate chroma would soften
///   colour edges the source never had.
///
/// `out` must be `out_row_bytes * height` bytes; rows are written at
/// `out_row_bytes` pitch (the card's row stride, which may exceed `width*2`).
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn pack_uyvy422(
    y: &[u8],
    y_stride: usize,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    width: u32,
    height: u32,
    chroma_420: bool,
    out_row_bytes: usize,
    out: &mut [u8],
) {
    let w = width as usize;
    let cw = w / 2;
    for row in 0..height as usize {
        let c_row = if chroma_420 { row / 2 } else { row };
        let y_line = &y[row * y_stride..row * y_stride + w];
        let u_line = &u[c_row * u_stride..c_row * u_stride + cw];
        let v_line = &v[c_row * v_stride..c_row * v_stride + cw];
        let o_line = &mut out[row * out_row_bytes..row * out_row_bytes + w * 2];
        for (((dst, y2), cb), cr) in o_line
            .chunks_exact_mut(4)
            .zip(y_line.chunks_exact(2))
            .zip(u_line.iter())
            .zip(v_line.iter())
        {
            dst[0] = *cb;
            dst[1] = y2[0];
            dst[2] = *cr;
            dst[3] = y2[1];
        }
    }
}

#[cfg(all(test, feature = "media-codecs"))]
mod tests {
    use super::pack_uyvy422;

    // 2x2 planar fixtures.
    const Y: [u8; 4] = [100, 101, 102, 103];

    #[test]
    fn packs_422_one_chroma_row_per_line() {
        let (u, v) = ([10u8, 30], [20u8, 40]); // one sample per row (cw=1, h=2)
        let mut out = [0u8; 8];
        pack_uyvy422(&Y, 2, &u, 1, &v, 1, 2, 2, false, 4, &mut out);
        assert_eq!(out, [10, 100, 20, 101, 30, 102, 40, 103]);
    }

    #[test]
    fn packs_420_duplicating_chroma_rows() {
        let (u, v) = ([10u8], [20u8]); // single chroma row (cw=1, ch=1)
        let mut out = [0u8; 8];
        pack_uyvy422(&Y, 2, &u, 1, &v, 1, 2, 2, true, 4, &mut out);
        // Both output rows carry the same chroma — vertical duplication.
        assert_eq!(out, [10, 100, 20, 101, 10, 102, 20, 103]);
    }

    #[test]
    fn respects_output_row_pitch() {
        // Card stride wider than w*2: padding bytes must be untouched.
        let (u, v) = ([10u8], [20u8]);
        let mut out = [0xAAu8; 12]; // pitch 6, 2 rows
        pack_uyvy422(&Y, 2, &u, 1, &v, 1, 2, 2, true, 6, &mut out);
        assert_eq!(&out[0..4], &[10, 100, 20, 101]);
        assert_eq!(&out[4..6], &[0xAA, 0xAA], "padding untouched");
        assert_eq!(&out[6..10], &[10, 102, 20, 103]);
    }

    /// Round-trip with the input side's unpacker: pack(unpack(x)) == x for
    /// 4:2:2, which pins the two functions as true inverses.
    #[test]
    fn round_trips_with_the_input_unpacker() {
        let src: Vec<u8> = (0u8..16).collect(); // 2 rows of 2px UYVY (stride 4... 2px = 4 bytes)
        let src = &src[..8];
        let (mut y, mut cb, mut cr) = ([0u8; 4], [0u8; 2], [0u8; 2]);
        crate::engine::sdi_io::unpack_uyvy422_for_test(
            src, 4, 2, 2, false, &mut y, &mut cb, &mut cr,
        );
        let mut back = [0u8; 8];
        pack_uyvy422(&y, 2, &cb, 1, &cr, 1, 2, 2, false, 4, &mut back);
        assert_eq!(&back, src);
    }
}
