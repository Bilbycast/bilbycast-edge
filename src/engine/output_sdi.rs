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
//! **A/V sync**: both essences ride one 90 kHz playout clock. The first
//! displayed video frame's PTS is schedule-time 0; everything is scheduled
//! (timestamped) at `pts - anchor`, so the card lip-syncs the two. Video is
//! placed from its own PTS — snapped to the mode's frame grid — and never from
//! a count of writes that succeeded: a frame this worker drops leaves a hole
//! the card fills by repeating the previous picture, and every frame after it
//! still lands where its timestamp says. `audio_pos` advances between blocks
//! by exact sample duration (drift-free), re-anchoring to the PTS-derived time
//! only on a source discontinuity. Source PTS are 33-bit and wrap every
//! ~26.5 h, so `PtsTimeline` lifts both essences onto one continuous
//! timeline before any of this arithmetic runs. Audio is fixed at 48 kHz (the
//! card's rate); other rates drop with an alarm (resampling is a follow-up).
//! Opus and AC-4 are not handled here — those flows play out video-only.
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
use decklink_rs::{CapturedAncillaryPacket, DecklinkPixelFormat, DecklinkPlayout, DecklinkPlayoutConfig};
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

/// Source PTS are 33-bit (MPEG-2 systems), so they wrap every
/// `2^33 / 90_000` ≈ 26.5 h.
#[cfg(feature = "media-codecs")]
const PTS_WRAP_90K: i64 = 1 << 33;

/// Consecutive video frames that must fail the frame-grid advance test before
/// the source epoch is re-mapped. One is never enough: the demuxer surfaces a
/// PES carrying no PTS as `0`, and re-anchoring on a single one of those would
/// throw the timeline forward by the whole elapsed runtime and freeze playout
/// until the card's clock caught up. A source running faster than the mode
/// also lands two frames in one slot, and that must drop the extra frame, not
/// re-map anything — a successful write resets the streak, so an alternating
/// pass/fail pattern never reaches this.
#[cfg(feature = "media-codecs")]
const REANCHOR_STREAK: u32 = 3;

/// Consecutive `Busy` writes before the card is called out as not draining.
/// `Busy` is legitimately transient (a decode burst, a scheduling hiccup);
/// only an unbroken run of them means the card has stopped presenting.
#[cfg(feature = "media-codecs")]
const BUSY_ALARM_STREAK: u32 = 25;

/// Frames a queued SCTE-104 cue is repeated across. VANC is unprotected on the
/// wire, so the cue rides several consecutive frames to survive a corrupted
/// one. Only frames actually written to the card count against this.
#[cfg(feature = "media-codecs")]
const SCTE104_VANC_REPEATS: u8 = 3;

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
    /// Source PTS (90 kHz) exactly as the demuxer read it — 33-bit, wrapping,
    /// and in decode order. [`PtsTimeline`] lifts it onto the playout timeline.
    pts: u64,
}

/// One demuxed audio access unit, still coded — decoded in the blocking worker
/// so no codec work touches the async reactor.
#[cfg(feature = "media-codecs")]
struct AudioAu {
    /// Source PTS (90 kHz), same 33-bit raw form as [`VideoAu::pts`].
    pts: u64,
    payload: AudioPayload,
}

/// The playout mode's frame grid. The card displays on it, and the SDK demands
/// strictly ascending display times, so a decoded frame's card time is snapped
/// to its nearest slot before it is scheduled.
#[cfg(feature = "media-codecs")]
struct FrameGrid {
    /// Mode frame rate as `num / den` frames per second.
    num: i128,
    den: i128,
}

#[cfg(feature = "media-codecs")]
impl FrameGrid {
    fn new(fr_num: u32, fr_den: u32) -> Self {
        // A mode the card declines to describe would divide by zero here.
        // 25p only mis-snaps frames on a device that cannot play anyway.
        let (num, den) = if fr_num == 0 || fr_den == 0 {
            (25, 1)
        } else {
            (fr_num, fr_den)
        };
        Self {
            num: num as i128,
            den: den as i128,
        }
    }

    /// Nearest slot index for a card time.
    fn slot_of(&self, t_90k: i64) -> i64 {
        div_round(t_90k as i128 * self.num, 90_000 * self.den) as i64
    }

    /// Card time of a slot, from exact rational arithmetic — never
    /// `slot * frame_duration`, because the card's frame duration is a whole
    /// number of ticks and 59.94 needs 1501.5: half a tick lost per frame
    /// walks video 1.2 s away from audio over an hour on the shared timeline.
    fn time_of(&self, slot: i64) -> i64 {
        div_round(slot as i128 * 90_000 * self.den, self.num) as i64
    }
}

/// `n / d` rounded to nearest, ties away from zero. `d` must be positive.
#[cfg(feature = "media-codecs")]
fn div_round(n: i128, d: i128) -> i128 {
    if n >= 0 { (n + d / 2) / d } else { (n - d / 2) / d }
}

/// Lifts 33-bit source PTS onto the continuous timeline both essences share.
///
/// Each PTS is measured against the one before it — the only reference close
/// enough for the modular reduction to carry a sign. A fixed anchor cannot
/// serve: past half the modulus (~13 h) the distance to it is ambiguous, so at
/// the 26.5 h wrap a stream anchored near 0 reads as having returned to the
/// start rather than run a full lap, and everything placed against it is
/// scheduled 26.5 h into the card's past.
#[cfg(feature = "media-codecs")]
#[derive(Default)]
struct PtsTimeline {
    last: Option<i64>,
    epoch: i64,
}

#[cfg(feature = "media-codecs")]
impl PtsTimeline {
    /// Audio and video AUs interleave, and video AUs arrive in decode order,
    /// so successive PTS step either way by a few frames; only a jump past
    /// half the modulus reads as a wrap. A source that steps its PTS by more
    /// than 13 h is genuinely indistinguishable from one that wrapped — the
    /// worker's frame-grid guard is what catches that.
    fn unwrap_pts(&mut self, pts: u64) -> i64 {
        let pts = (pts & (PTS_WRAP_90K as u64 - 1)) as i64;
        if let Some(last) = self.last {
            let delta = pts - last;
            if delta < -(PTS_WRAP_90K / 2) {
                self.epoch += PTS_WRAP_90K;
            } else if delta > PTS_WRAP_90K / 2 {
                self.epoch -= PTS_WRAP_90K;
            }
        }
        self.last = Some(pts);
        self.epoch + pts
    }
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
    Scte35(Vec<u8>),
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
            event_sender.emit_output_with_details(
                EventSeverity::Critical,
                category::SDI,
                format!(
                    "SDI output '{}' requires the media-codecs feature",
                    config.id
                ),
                &config.id,
                serde_json::json!({ "error_code": "sdi_no_media_codecs" }),
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
                            DemuxedFrame::Scte35(section) if config.scte35_injection => {
                                PlayoutAu::Scte35(section)
                            }
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
                event_sender.emit_output_with_details(
                    EventSeverity::Info,
                    category::SDI,
                    format!("{ctx}: playout opened {w}x{h} @ {n}/{d}"),
                    &config.id,
                    serde_json::json!({
                        "error_code": "sdi_playout_opened",
                        "device": config.device,
                        "mode": config.mode,
                        "width": w,
                        "height": h,
                        "frame_rate_num": n,
                        "frame_rate_den": d,
                    }),
                );
                return Some(po);
            }
            // A mode/device combination the card refuses is a config
            // problem — retrying forever would just spin.
            Err(e @ decklink_rs::Error::Unsupported(_)) => {
                let msg = format!("{ctx}: playout refused: {e}");
                tracing::error!(target: "sdi.out", "{msg}");
                event_sender.emit_output_with_details(
                    EventSeverity::Critical,
                    category::SDI,
                    msg,
                    &config.id,
                    serde_json::json!({
                        "error_code": "sdi_playout_mode_unsupported",
                        "device": config.device,
                        "mode": config.mode,
                        "error": e.to_string(),
                    }),
                );
                return None;
            }
            Err(e) => {
                if !announced {
                    announced = true;
                    warn!(target: "sdi.out", "{ctx}: playout open failed: {e}; retrying");
                    event_sender.emit_output_with_details(
                        EventSeverity::Warning,
                        category::SDI,
                        format!(
                            "{ctx}: playout open failed: {e} — retrying until the \
                             device returns"
                        ),
                        &config.id,
                        serde_json::json!({
                            "error_code": "sdi_playout_open_failed",
                            "device": config.device,
                            "mode": config.mode,
                            "error": e.to_string(),
                        }),
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
    let (mut out_w, mut out_h) = playout.video_dimensions();
    let mut row_bytes = playout.row_bytes();
    let mut frame_buf = vec![0u8; row_bytes * out_h as usize];
    let (fr_num, fr_den) = playout.video_frame_rate();
    let mut grid = FrameGrid::new(fr_num, fr_den);

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
    let mut last_busy_warn: Option<Instant> = None;
    let mut last_reanchor_warn: Option<Instant> = None;

    // ── shared playout timeline ──────────────────────────────────────────
    // Both essences are placed against one anchor on one continuous source
    // timeline, so anything that shifts the mapping shifts them together and
    // lip-sync survives it.
    let mut timeline = PtsTimeline::default();
    // The source PTS that maps to card time 0 — the first frame actually
    // written, because the shim starts scheduled playback at 0.
    let mut anchor_pts: Option<i64> = None;
    // Last frame-grid slot handed to the card. The SDK refuses a display time
    // that does not advance, so this guards the write rather than discovering
    // it from an error.
    let mut last_slot: Option<i64> = None;
    let mut backwards_streak: u32 = 0;
    let mut busy_streak: u32 = 0;

    // ── audio state ──────────────────────────────────────────────────────
    // Audio for source-PTS `p` is placed at `p - anchor`. Audio arriving before
    // the anchor (i.e. before the first displayed frame) has no video to pair
    // with and is dropped. `audio_pos` then advances by exact sample duration
    // (drift-free), re-anchoring to the PTS-derived time only on a
    // discontinuity.
    let audio_out_ch = config.audio_channels as usize; // 0 = video-only
    // Operator A/V trim on the 90 kHz card timeline (+ delays audio, - advances).
    let audio_offset_90k = config.audio_offset_ms as i64 * 90;
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
    let mut last_video_pts = 0u64;
    let mut pending_ancillary: Option<(CapturedAncillaryPacket, u8)> = None;

    while let Some(item) = au_rx.blocking_recv() {
        if cancel.is_cancelled() {
            break;
        }

        let au = match item {
            PlayoutAu::Video(v) => {
                last_video_pts = v.pts;
                v
            }
            PlayoutAu::Scte35(section) => {
                if let Some(msg) = crate::engine::scte35_encode::decode_splice_insert_to_scte104(
                    &section,
                    last_video_pts,
                ) {
                    let event_id = msg.splice_event_id.unwrap_or(0);
                    let opcode = msg.opcode;
                    pending_ancillary = Some((
                        CapturedAncillaryPacket {
                            did: 0x41,
                            sdid: 0x07,
                            line_number: 0,
                            data: crate::engine::st2110::scte104::build_scte104_message(&msg),
                        },
                        SCTE104_VANC_REPEATS,
                    ));
                    event_sender.emit_output_with_details(
                        EventSeverity::Info,
                        category::SDI,
                        format!(
                            "{ctx}: queued SCTE-104 VANC cue event_id={event_id} \
                             opcode={opcode:?}"
                        ),
                        &config.id,
                        serde_json::json!({
                            "error_code": "sdi_scte104_queued",
                            "device": config.device,
                            "splice_event_id": event_id,
                            "opcode": format!("{opcode:?}"),
                        }),
                    );
                }
                continue;
            }
            PlayoutAu::Discontinuity => {
                // Input switch. Flush the video decoder and wait for a fresh
                // keyframe so it doesn't reference the old stream. Reset the
                // audio decoders (the new input may carry a different codec /
                // AAC config) and let audio re-anchor. `anchor_pts` stays: the
                // new input's PTS are continuous with the old one whenever the
                // edge's clocking regenerates them, and when they are not, the
                // frame-grid guard re-maps the epoch onto the next free slot.
                // Either way both essences keep reading the same anchor, so a
                // switch cannot pull them apart.
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
                // Unwrap before the anchor test, so the timeline keeps tracking
                // the source across a wrap even while audio has no video to
                // pair with.
                let src_pts = timeline.unwrap_pts(a.pts);
                // Can't place audio until the video timeline is anchored.
                let Some(anchor) = anchor_pts else { continue };
                decode_and_schedule_audio(
                    ctx,
                    &config.id,
                    &mut playout,
                    a.payload,
                    src_pts,
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
        // Every video AU advances the shared timeline, including the ones this
        // worker will not decode — skipping one would let a wrap slip past
        // unseen and strand every later PTS a full lap out.
        let au_pts = timeline.unwrap_pts(au.pts);

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
                        event_sender.emit_output_with_details(
                            EventSeverity::Warning,
                            category::SDI,
                            format!("{ctx}: video decoder open failed: {e}"),
                            &config.id,
                            serde_json::json!({
                                "error_code": "sdi_playout_decode_failed",
                                "codec": format!("{:?}", au.codec),
                                "error": e.to_string(),
                            }),
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
        }
        let dec = decoder.as_mut().expect("decoder opened above");

        // The AU's PTS rides the packet so the decoder hands it back on the
        // frame it belongs to: the reorder queue emits display order, which is
        // the order the card displays in, and B-frames make the two differ.
        if let Err(e) = dec.send_packet_with_pts(&au.annexb, au_pts) {
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
                    event_sender.emit_output_with_details(
                        EventSeverity::Warning,
                        category::SDI,
                        format!(
                            "{ctx}: decoded video is {w}x{h} but the configured mode \
                             is {out_w}x{out_h} — frames dropped; fix `mode` or the \
                             source"
                        ),
                        &config.id,
                        serde_json::json!({
                            "error_code": "sdi_playout_raster_mismatch",
                            "decoded_width": w,
                            "decoded_height": h,
                            "mode_width": out_w,
                            "mode_height": out_h,
                            "mode": config.mode,
                        }),
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
                    event_sender.emit_output_with_details(
                        EventSeverity::Warning,
                        category::SDI,
                        format!(
                            "{ctx}: decoded video chroma is unsupported for SDI playout \
                             (only 8-bit 4:2:0/4:2:2) — frames dropped"
                        ),
                        &config.id,
                        serde_json::json!({
                            "error_code": "sdi_playout_chroma_unsupported",
                            "pixel_format": frame.pixel_format(),
                        }),
                    );
                });
                continue;
            }
            // `pack_uyvy422` slices rows directly, so a plane shorter or more
            // narrowly strided than its geometry implies would panic inside
            // this blocking worker and take the flow down with it. A dropped
            // frame is always preferable to a dead flow.
            let cw = w as usize / 2;
            let rows = h as usize;
            if ys < w as usize
                || us < cw
                || vs < cw
                || row_bytes < w as usize * 2
                || y.len() < ys * rows.saturating_sub(1) + w as usize
                || u.len() < us * chroma_rows.saturating_sub(1) + cw
                || v.len() < vs * chroma_rows.saturating_sub(1) + cw
            {
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                throttled(&mut last_decode_warn, || {
                    warn!(
                        target: "sdi.out",
                        "{ctx}: skipping malformed {w}x{h} frame (strides y={ys} u={us} \
                         v={vs}, lens y={} u={} v={}, card pitch {row_bytes})",
                        y.len(), u.len(), v.len(),
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

            // Place the frame from its own PTS. FFmpeg carries the packet PTS
            // through the reorder queue, so this is the frame's display-order
            // source time.
            let frame_pts = frame.pts().unwrap_or_else(|| {
                // A decoder that doesn't propagate a PTS leaves nothing to
                // place against: take the next free slot so a picture still
                // reaches the card. Video then rides AU order and drifts
                // against audio exactly as a write counter would — but a
                // drifting picture beats none at all.
                anchor_pts.unwrap_or(0) + grid.time_of(last_slot.map_or(0, |s| s + 1))
            });
            let anchor = *anchor_pts.get_or_insert(frame_pts);
            let mut slot = grid.slot_of(frame_pts - anchor);
            // The SDK requires ascending display times. A slot already used
            // means either an extra frame for it (a source running faster than
            // the mode — the grid is the truth, drop it) or a source that
            // stepped its PTS backwards, which only a sustained run proves.
            if let Some(last) = last_slot
                && slot <= last
            {
                backwards_streak += 1;
                if backwards_streak < REANCHOR_STREAK {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                // Re-map the source epoch so this frame takes the next free
                // slot. Audio reads the same anchor, so it follows the step
                // and lip-sync survives it.
                slot = last + 1;
                anchor_pts = Some(frame_pts - grid.time_of(slot));
                backwards_streak = 0;
                throttled(&mut last_reanchor_warn, || {
                    warn!(
                        target: "sdi.out",
                        "{ctx}: source PTS stepped backwards; re-anchored playout at slot {slot}"
                    );
                });
            }

            // A cue rides the frames that reach the card, so the repeat count
            // is only ever consumed by a successful write below — a frame
            // dropped against the grid must not spend one of the three.
            let ancillary = pending_ancillary
                .as_ref()
                .map(|(packet, _)| std::slice::from_ref(packet))
                .unwrap_or(&[]);
            match playout.write_video_with_ancillary(&frame_buf, grid.time_of(slot), ancillary) {
                Ok(()) => {
                    last_slot = Some(slot);
                    backwards_streak = 0;
                    busy_streak = 0;
                    if let Some((_, remaining)) = pending_ancillary.as_mut() {
                        if *remaining == SCTE104_VANC_REPEATS {
                            sdi.scte35_cues_injected.fetch_add(1, Ordering::Relaxed);
                        }
                        *remaining -= 1;
                        if *remaining == 0 {
                            pending_ancillary = None;
                        }
                    }
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
                // `last_slot` is deliberately not advanced: the frame was never
                // scheduled, so its slot stays free and the shim's own guard
                // stays in step with this one.
                Err(decklink_rs::Error::Busy) => {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    sdi.frames_dropped.fetch_add(1, Ordering::Relaxed);
                    busy_streak += 1;
                    // An unbroken run means the card has stopped presenting —
                    // reference/genlock lost is the usual cause. Every frame is
                    // being dropped at this point, so it cannot stay a counter.
                    if busy_streak >= BUSY_ALARM_STREAK {
                        throttled(&mut last_busy_warn, || {
                            warn!(
                                target: "sdi.out",
                                "{ctx}: card not draining scheduled frames \
                                 ({busy_streak} consecutive); dropping every frame"
                            );
                            event_sender.emit_output_with_details(
                                EventSeverity::Warning,
                                category::SDI,
                                format!(
                                    "{ctx}: the card has stopped draining scheduled \
                                     frames ({busy_streak} consecutive) — every frame is \
                                     being dropped; check the card's reference/genlock"
                                ),
                                &config.id,
                                serde_json::json!({
                                    "error_code": "sdi_playout_card_not_draining",
                                    "device": config.device,
                                    "consecutive_busy": busy_streak,
                                }),
                            );
                        });
                    }
                    if cancel.is_cancelled() {
                        return;
                    }
                }
                Err(decklink_rs::Error::Eof) => return,
                Err(e) => {
                    warn!(target: "sdi.out", "{ctx}: playout write failed: {e}; reopening");
                    event_sender.emit_output_with_details(
                        EventSeverity::Warning,
                        category::SDI,
                        format!("{ctx}: playout write failed: {e} — reopening device"),
                        &config.id,
                        serde_json::json!({
                            "error_code": "sdi_playout_lost",
                            "device": config.device,
                            "error": e.to_string(),
                        }),
                    );
                    drop(playout);
                    match open_playout(ctx, config, cancel, event_sender) {
                        Some(po) => {
                            playout = po;
                            // A fresh session re-resolves the mode, so the
                            // raster, pitch and frame grid are re-read rather
                            // than carried over.
                            (out_w, out_h) = playout.video_dimensions();
                            row_bytes = playout.row_bytes();
                            frame_buf = vec![0u8; row_bytes * out_h as usize];
                            let (n, d) = playout.video_frame_rate();
                            grid = FrameGrid::new(n, d);
                            // The card's cumulative counters restart, and so
                            // does its schedule clock — the new session starts
                            // playback at 0. Drop the anchor so the next frame
                            // written re-establishes the epoch against it;
                            // carrying the old one would schedule every essence
                            // a whole session into the new clock's future.
                            // Audio re-anchors off the same drop.
                            card_drops_base = 0;
                            card_late_base = 0;
                            anchor_pts = None;
                            last_slot = None;
                            backwards_streak = 0;
                            busy_streak = 0;
                            audio_pos = None;
                            audio_started = false;
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

/// Decode one audio AU's payload and schedule the resulting sample block(s) on
/// the card, lip-synced to video via the shared 90 kHz timeline. `src_pts` is
/// the AU's source PTS already lifted onto that timeline.
///
/// AAC yields one block per AU; the FFmpeg codecs may yield several — each is
/// scheduled contiguously by the advancing `audio_pos`.
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn decode_and_schedule_audio(
    ctx: &str,
    output_id: &str,
    playout: &mut DecklinkPlayout,
    payload: AudioPayload,
    src_pts: i64,
    anchor_pts: i64,
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
    match payload {
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
                            output_id,
                            playout,
                            &planar,
                            sr,
                            ch,
                            out_ch,
                            audio_offset_90k,
                            src_pts,
                            anchor_pts,
                            audio_pos,
                            audio_started,
                            ibuf,
                            stats,
                            last_warn,
                            event_sender,
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
                if d.send_packet(frame_bytes, src_pts).is_err() {
                    continue;
                }
                while let Ok(decoded) = d.receive_frame() {
                    schedule_audio_block(
                        ctx,
                        output_id,
                        playout,
                        &decoded.planar,
                        decoded.sample_rate,
                        decoded.channels as usize,
                        out_ch,
                        audio_offset_90k,
                        src_pts,
                        anchor_pts,
                        audio_pos,
                        audio_started,
                        ibuf,
                        stats,
                        last_warn,
                        event_sender,
                    );
                }
            }
        }
    }
}

/// Place one decoded planar-f32 block on the card at its lip-synced schedule
/// time. `audio_pos` advances by the block's exact duration (drift-free);
/// a > 0.5 s divergence from the PTS-derived time re-anchors it (discontinuity).
#[cfg(feature = "media-codecs")]
#[allow(clippy::too_many_arguments)]
fn schedule_audio_block(
    ctx: &str,
    output_id: &str,
    playout: &mut DecklinkPlayout,
    planar: &[Vec<f32>],
    sample_rate: u32,
    dec_ch: usize,
    out_ch: usize,
    audio_offset_90k: i64,
    src_pts: i64,
    anchor_pts: i64,
    audio_pos: &mut Option<i64>,
    audio_started: &mut bool,
    ibuf: &mut Vec<i32>,
    stats: &Arc<OutputStatsAccumulator>,
    last_warn: &mut Option<Instant>,
    event_sender: &EventSender,
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

    // Both are already on the continuous timeline, so this subtraction needs no
    // wrap correction of its own — `PtsTimeline` did it against a reference
    // close enough to keep the sign.
    let target = src_pts - anchor_pts;
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
    // Audio placed before the epoch cannot be scheduled at all, so every block
    // from here on is silently discarded until something re-anchors. That is a
    // total loss of audio and must be audible on the event bus, not just in a
    // counter nobody reads.
    if pos < 0 {
        throttled(last_warn, || {
            warn!(
                target: "sdi.out",
                "{ctx}: audio schedule position {pos} precedes the playout epoch; \
                 audio stopped scheduling"
            );
            event_sender.emit_output_with_details(
                EventSeverity::Warning,
                category::SDI,
                format!(
                    "{ctx}: audio is being placed before the start of the playout \
                     timeline — audio has stopped scheduling and the output is \
                     silent; the source stepped its PTS behind the first video frame"
                ),
                output_id,
                serde_json::json!({
                    "error_code": "sdi_playout_audio_stalled",
                    "position_90k": pos,
                    "target_90k": target,
                }),
            );
        });
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
    use super::{FrameGrid, PTS_WRAP_90K, PtsTimeline, pack_uyvy422};

    #[test]
    fn grid_snaps_to_the_nearest_slot() {
        let g = FrameGrid::new(25_000, 1_000); // 25p → 3600 ticks/frame
        assert_eq!(g.slot_of(0), 0);
        assert_eq!(g.slot_of(3600), 1);
        assert_eq!(g.slot_of(3599), 1, "just under lands on its own slot");
        assert_eq!(g.slot_of(1799), 0, "under half a frame stays put");
        assert_eq!(g.slot_of(1801), 1, "over half a frame rounds up");
        assert_eq!(g.time_of(0), 0);
        assert_eq!(g.time_of(10), 36_000);
    }

    /// 59.94 needs 1501.5 ticks/frame. Deriving slot times from a whole-tick
    /// duration loses half a tick each frame and walks video away from audio;
    /// the exact rational cannot.
    #[test]
    fn grid_does_not_accumulate_error_at_59_94() {
        let g = FrameGrid::new(60_000, 1_001);
        // One hour of frames: exact time is slot * 90000 * 1001 / 60000.
        for slot in [1i64, 2, 3, 1000, 215_784] {
            let exact = (slot as i128 * 90_000 * 1_001) as f64 / 60_000.0;
            let got = g.time_of(slot) as f64;
            assert!(
                (got - exact).abs() <= 0.5,
                "slot {slot}: {got} drifted from {exact}"
            );
        }
        // A whole-tick duration would be 1.2 s out by the end of an hour.
        let naive = 215_784i64 * 1501;
        assert!(g.time_of(215_784) - naive > 100_000);
    }

    /// Slot times must strictly ascend — the SDK refuses anything else.
    #[test]
    fn grid_slot_times_strictly_ascend_at_59_94() {
        let g = FrameGrid::new(60_000, 1_001);
        let mut prev = g.time_of(0);
        for slot in 1..2_000 {
            let t = g.time_of(slot);
            assert!(t > prev, "slot {slot}: {t} did not advance past {prev}");
            prev = t;
        }
    }

    #[test]
    fn timeline_passes_through_without_a_wrap() {
        let mut t = PtsTimeline::default();
        assert_eq!(t.unwrap_pts(0), 0);
        assert_eq!(t.unwrap_pts(3600), 3600);
        assert_eq!(t.unwrap_pts(7200), 7200);
    }

    /// The 26.5 h wrap with the anchor near 0 — the case a fixed-anchor
    /// subtraction gets wrong, scheduling audio a full lap into the past.
    ///
    /// The boundary has to be walked, not jumped: every PTS is read against
    /// the one before it, so the test feeds the sequence a stream feeds.
    #[test]
    fn timeline_continues_across_a_wrap_anchored_near_zero() {
        const HOUR: u64 = 324_000_000;
        let mut t = PtsTimeline::default();
        assert_eq!(t.unwrap_pts(0), 0);
        let mut raw = 0u64;
        while raw + HOUR < PTS_WRAP_90K as u64 {
            raw += HOUR;
            assert_eq!(t.unwrap_pts(raw), raw as i64, "no wrap yet at {raw}");
        }
        // The step that crosses: the source counts back to near zero, but the
        // elapsed time is a full lap forward.
        let wrapped = (raw + HOUR) % PTS_WRAP_90K as u64;
        assert!(wrapped < raw, "this step must cross the boundary");
        assert_eq!(t.unwrap_pts(wrapped), raw as i64 + HOUR as i64);
        assert_eq!(t.unwrap_pts(wrapped + 3600), raw as i64 + HOUR as i64 + 3600);
    }

    /// Audio and video interleave and video arrives in decode order, so PTS
    /// straddle the boundary in both directions before settling.
    #[test]
    fn timeline_handles_essences_straddling_the_wrap() {
        let mut t = PtsTimeline::default();
        let pre = (PTS_WRAP_90K - 100) as u64;
        assert_eq!(t.unwrap_pts(pre), PTS_WRAP_90K - 100);
        // Video wrapped, audio hasn't yet.
        assert_eq!(t.unwrap_pts(0), PTS_WRAP_90K);
        assert_eq!(t.unwrap_pts(pre - 8), PTS_WRAP_90K - 108);
        assert_eq!(t.unwrap_pts(3600), PTS_WRAP_90K + 3600);
    }

    /// A source restart is a backwards step, not a wrap: the timeline must
    /// report it as such so the worker's grid guard can re-anchor.
    #[test]
    fn timeline_reports_a_source_restart_as_a_step_back() {
        let mut t = PtsTimeline::default();
        assert_eq!(t.unwrap_pts(54_000_000), 54_000_000);
        assert_eq!(t.unwrap_pts(0), 0, "10-min restart is not half a modulus");
    }

    /// The failure this whole timeline exists to prevent: a stream anchored
    /// near 0 that runs past the wrap must keep producing forward-advancing
    /// card times, never a negative one.
    #[test]
    fn audio_target_stays_forward_across_a_wrap() {
        let mut t = PtsTimeline::default();
        let anchor = t.unwrap_pts(900); // first video frame, PTS near 0
        let mut prev = 0;
        // Walk a whole lap in ~1 h steps, then past it.
        for step in 0..30 {
            let raw = (900 + step * 324_000_000u64) % PTS_WRAP_90K as u64;
            let target = t.unwrap_pts(raw) - anchor;
            assert!(target >= prev, "step {step}: {target} went backwards");
            prev = target;
        }
        assert!(prev > PTS_WRAP_90K, "the lap did not complete");
    }

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
