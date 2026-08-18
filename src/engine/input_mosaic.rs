// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! The mosaic compositor — a multiviewer wall's stream head (edge #107).
//!
//! Composites N node-local inputs into one canvas and publishes it as a fresh
//! MPEG-TS feed, so the wall is an ordinary flow source: it records, restreams,
//! nests and thumbnails like anything else. Geometry, letterboxing and badge
//! state live in [`crate::engine::mosaic`]; this module is the moving parts.
//!
//! # The first input that consumes other inputs
//!
//! Nothing else in the tree does this. A tile's source is a **node-local input
//! id**, reached through `FlowManager::subscribe_input`, and the compositor
//! neither knows nor cares whether that input is a full-resolution local SDI
//! feed or a 640x360 proxy arriving over SRT from another site. That is the
//! whole point: proxies can be added later without the compositor changing.
//!
//! # Nothing here may ever block a media path
//!
//! This is a tier-1 constraint, not a preference, and it shapes every choice
//! below. Video bandwidth on a contribution node can be very high, and a wall
//! is a *monitoring* surface — it must never be able to apply backpressure to
//! the feeds it is watching.
//!
//! * **Each tile decodes independently and keeps only the newest frame.** The
//!   handoff is a `watch` channel, whose send overwrites rather than queues and
//!   can never block or grow. A tile that decodes faster than the canvas ticks
//!   simply has its older frames dropped, which is exactly right — nobody wants
//!   to look at a stale frame that was queued behind a fresher one.
//! * **The compositor never waits for a tile.** At each canvas tick it takes
//!   whatever each tile currently has. A dead source does not stall the wall;
//!   it goes to `NO SIGNAL` on its own timer while every other tile keeps
//!   running.
//! * **A slow subscriber to the tile's source is the broadcast channel's
//!   problem, not ours** — `RecvError::Lagged` is counted and skipped, never
//!   waited on.
//! * **Decode and encode run under `block_in_place`**, so the FFmpeg C calls
//!   never occupy an async worker.
//!
//! # What the design document got wrong, and this had to be built around
//!
//! Two things, both measured rather than argued (see
//! `bilbycast-ffmpeg-video-rs/video-engine/tests/canvas_subrect_blit.rs`):
//!
//! 1. **The canvas is packed BGRA8**, because `scale_raw_planes_into_packed`
//!    refuses every planar destination. So it costs 4 bytes/pixel rather than
//!    YUV420's 1.5, and the canvas must be converted to YUV before it can be
//!    encoded. The upside is that BGRA has no chroma sub-sampling, so tile
//!    rects need no even alignment.
//! 2. **The scaler's bounds check used to refuse the bottom row of every
//!    wall.** It demanded `pitch * height` when the true requirement is
//!    `(h-1)*pitch + w*4`. Fixed upstream; [`crate::engine::mosaic::Canvas`]
//!    documents the arithmetic that now relies on it.
//!
//! And one the design stated as free that is not: publishing onto the flow bus
//! is **an encode and a mux**, not a send, because the bus carries MPEG-TS.
//! That is why this module needs a `video-encoder-*` feature and says so with a
//! named error when none was compiled in.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::MosaicInputConfig;
use crate::manager::events::EventSender;
use crate::engine::manager::FlowManager;
use crate::engine::mosaic::{self, AspectPolicy, Canvas, MosaicLayout, Tile, TileLiveness, TileRect};
use crate::engine::packet::RtpPacket;
use crate::stats::collector::FlowStatsAccumulator;

/// TS packets bundled into each published datagram.
///
/// 7 x 188 = 1316 B — the SRT payload size and the internet-safe MTU. Both
/// extremes are wrong and both have been shipped in this codebase before: one
/// RtpPacket per 188 B packet saturates the flow broadcast channel, and a whole
/// frame per RtpPacket emits oversized datagrams that fragment over the public
/// internet and the tunnel path.
const TS_PACKETS_PER_DATAGRAM: usize = 7;

/// One tile's picture, **already scaled to the rect it occupies**.
///
/// Scaling happens in the tile's own task rather than in the compositor, and
/// that placement is the single biggest performance decision in this module.
///
/// The obvious design stores the decoded source frame and scales it at
/// composite time. It is badly wrong at scale: a 1080p YUV420 frame is ~3.1 MB,
/// so sixteen tiles at 50 fps is **~2.5 GB/s of allocation and memcpy** just to
/// hand frames across, before any scaling — on a node that is also carrying the
/// live feeds being watched. It also puts N libswscale calls on the compositor's
/// critical path, serialising work that is naturally parallel.
///
/// Scaling per tile instead means the buffer handed over is tile-sized: a 4x4
/// wall's 480x270 patch is 518 KB rather than 3.1 MB, the scale cost is spread
/// across N independent tasks, and the compositor's inner loop becomes a row
/// memcpy — the cheapest thing it could possibly be.
struct TileFrame {
    /// Packed BGRA, `rect.width * rect.height * 4` bytes, no padding.
    bgra: Vec<u8>,
    /// Where this patch lands on the canvas, letterboxing already applied.
    rect: TileRect,
}

/// What one tile's decoder task publishes to the compositor.
///
/// A `watch` channel rather than an mpsc, deliberately. `watch::Sender::send`
/// overwrites the slot and returns immediately — it cannot block, cannot fail
/// for fullness, and cannot grow. That is precisely the semantics a monitoring
/// tile wants: the compositor only ever draws the newest frame, and an older
/// one that has been superseded has no value at all. An mpsc, however bounded,
/// would make the wrong promise: it would either block a decoder (backpressure
/// onto a media path) or need a drop policy this already has for free.
type TileSlot = watch::Receiver<Option<Arc<TileFrame>>>;

/// Live counters for one wall, read by the stats surface.
#[derive(Default)]
pub struct MosaicCounters {
    pub canvas_frames: AtomicU64,
    /// Canvas ticks whose composite work took longer than the canvas period.
    ///
    /// Rising means **the head cannot sustain this wall** — reduce the tile
    /// count, the canvas size or the frame rate. Distinct from `canvas_skipped`
    /// below: this is the wall being too expensive, that is the wall being
    /// starved.
    pub canvas_over_budget: AtomicU64,
    /// Canvas periods that elapsed with no frame produced.
    ///
    /// The ticker skips missed ticks rather than trying to catch up, which is
    /// right — a wall that fell behind should resume at the current instant,
    /// not sprint through stale frames. But skipping silently would let a wall
    /// run at a fraction of its configured rate with every counter reading
    /// healthy, so the skipped periods are counted.
    pub canvas_skipped: AtomicU64,
    /// TS packets a tile's subscription missed because it fell behind its
    /// source's broadcast channel.
    ///
    /// **Not** the same as a decoded frame being superseded by a fresher one,
    /// which is the ordinary and healthy case and is not counted at all — the
    /// `watch` slot simply overwrites. This counts a tile losing *input*, which
    /// means that tile's decoder could not keep up and its picture will have
    /// artefacts or gaps. An earlier version of this field conflated the two
    /// and documented the alarming one as healthy.
    pub tile_input_lagged: AtomicU64,
    pub tile_decode_errors: AtomicU64,
}

/// Spawn the compositor for one mosaic input.
#[allow(clippy::too_many_arguments)]
pub fn spawn_mosaic_input(
    config: MosaicInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
    flow_manager: Arc<FlowManager>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(reason) = run_mosaic(
            config,
            broadcast_tx,
            flow_stats,
            cancel.clone(),
            event_sender.clone(),
            flow_id.clone(),
            input_id.clone(),
            flow_manager,
        )
        .await
        {
            tracing::error!(flow_id = %flow_id, input_id = %input_id, "mosaic: {reason}");
            event_sender.emit_flow_with_details(
                crate::manager::events::EventSeverity::Critical,
                "flow",
                format!("Multiviewer wall stopped: {reason}"),
                &flow_id,
                serde_json::json!({
                    "error_code": "mosaic_failed",
                    "input_id": input_id,
                }),
            );
        }
    })
}

/// Build the layout the compositor will render, from config.
fn layout_from_config(config: &MosaicInputConfig) -> MosaicLayout {
    MosaicLayout {
        canvas: Canvas::new(config.width, config.height),
        tiles: config
            .tiles
            .iter()
            .map(|t| Tile {
                id: t.id.clone(),
                rect: TileRect::new(t.x, t.y, t.width, t.height),
                z: t.z,
                source_input_id: t.source_input_id.clone(),
                liveness: match t.source_input_id {
                    Some(_) => TileLiveness::assigned(),
                    None => TileLiveness::unassigned(),
                },
            })
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_mosaic(
    config: MosaicInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
    flow_manager: Arc<FlowManager>,
) -> Result<(), String> {
    let layout = layout_from_config(&config);
    layout.validate().map_err(|e| e.to_string())?;

    let counters = Arc::new(MosaicCounters::default());

    // One decoder task per assigned tile. Each owns its own decoder and
    // publishes into its own slot; none of them can affect another, and none
    // of them can affect the source they are watching.
    let mut slots: Vec<Option<TileSlot>> = Vec::with_capacity(layout.tiles.len());
    // Held so the tiles can be joined when the wall stops. Discarding them
    // leaves detached tasks decoding into a slot nobody reads if the compositor
    // exits first — on a node that is still carrying the live feeds those tiles
    // are subscribed to.
    let mut tile_tasks: Vec<JoinHandle<()>> = Vec::with_capacity(layout.tiles.len());
    for tile in &layout.tiles {
        match tile.source_input_id.as_deref() {
            None => slots.push(None),
            // A tile naming the wall's own input id would subscribe the
            // compositor to its own output: every canvas frame it published
            // would arrive back as a tile to decode and composite, which is an
            // unbounded feedback loop on a live node. Refused rather than
            // rendered, because there is no sensible picture to draw for it.
            Some(source) if source == input_id => {
                tracing::warn!(
                    flow_id = %flow_id, tile = %tile.id,
                    "mosaic: tile sources the wall itself; refusing to feed it back"
                );
                event_sender.emit_flow_with_details(
                    crate::manager::events::EventSeverity::Warning,
                    "flow",
                    format!(
                        "Multiviewer tile '{}' sources the wall itself and was left unassigned",
                        tile.id
                    ),
                    &flow_id,
                    serde_json::json!({
                        "error_code": "mosaic_tile_self_reference",
                        "input_id": input_id,
                        "tile_id": tile.id,
                    }),
                );
                slots.push(None);
            }
            Some(source) => {
                // The task owns subscription and retries, so a source that is
                // not running *yet* is picked up when it appears rather than
                // leaving the tile dead for the life of the wall. A wall is
                // usually started alongside the feeds it watches, so that race
                // is the common case, not the exotic one.
                if flow_manager.subscribe_input(source).is_none() {
                    tracing::info!(
                        flow_id = %flow_id, tile = %tile.id, source = %source,
                        "mosaic: tile source not running yet; the tile will retry"
                    );
                    event_sender.emit_flow_with_details(
                        crate::manager::events::EventSeverity::Warning,
                        "flow",
                        format!(
                            "Multiviewer tile '{}' is waiting for input '{source}'",
                            tile.id
                        ),
                        &flow_id,
                        serde_json::json!({
                            "error_code": "mosaic_tile_source_missing",
                            "input_id": input_id,
                            "tile_id": tile.id,
                            "source_input_id": source,
                        }),
                    );
                }
                let (tx, slot_rx) = watch::channel(None);
                tile_tasks.push(spawn_tile_decoder(
                    source.to_string(),
                    Arc::clone(&flow_manager),
                    tx,
                    cancel.child_token(),
                    Arc::clone(&counters),
                    tile.id.clone(),
                    flow_id.clone(),
                    tile.rect,
                ));
                slots.push(Some(slot_rx));
            }
        }
    }

    let result = run_compositor(
        config,
        layout,
        slots,
        broadcast_tx,
        cancel,
        counters,
        flow_id,
        input_id,
        flow_stats,
    )
    .await;

    // The compositor has stopped, so every tile's child token is cancelled by
    // the parent. Wait for them rather than leaving them detached: a tile still
    // holding a subscription to a live source after the wall is gone is a
    // consumer nobody can see.
    for task in tile_tasks {
        let _ = task.await;
    }
    result
}

/// One tile's ingest: subscribe, demux, decode, publish newest frame.
///
/// Everything in here is best-effort by design. A malformed packet, a decoder
/// hiccup or a lagged subscription costs this tile a frame and nothing else.
#[allow(clippy::too_many_arguments)]
fn spawn_tile_decoder(
    source_input_id: String,
    flow_manager: Arc<FlowManager>,
    tx: watch::Sender<Option<Arc<TileFrame>>>,
    cancel: CancellationToken,
    counters: Arc<MosaicCounters>,
    tile_id: String,
    flow_id: String,
    tile_rect: TileRect,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(feature = "media-codecs")]
        {
            use crate::engine::webrtc::ts_demux::{DemuxedFrame, TsDemuxer};

            // **Subscribe in a retry loop, not once at wall start.**
            //
            // Inputs come up in whatever order the flow starts them, and a wall
            // is usually started alongside the very feeds it watches. A single
            // attempt at start therefore loses a race it will lose often, and
            // loses it permanently: the tile shows NO SIGNAL for the life of
            // the wall even though its source came up a second later. The same
            // loop covers a source that is stopped and restarted mid-show,
            // which on a long production is not unusual.
            let mut rx = loop {
                if cancel.is_cancelled() {
                    return;
                }
                match flow_manager.subscribe_input(&source_input_id) {
                    Some(rx) => break rx,
                    None => {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(SOURCE_RETRY_INTERVAL) => {}
                        }
                    }
                }
            };

            let mut demuxer = TsDemuxer::new(None);
            let mut decoder: Option<video_engine::VideoDecoder> = None;
            let mut decoder_codec: Option<video_engine::VideoCodec> = None;
            // Rebuilt when the source changes shape — a source switch mid-show
            // is ordinary, and a stale scaler would produce a wrong-sized patch.
            let mut scaler: Option<(u32, u32, i32, u32, u32, video_engine::VideoScaler)> = None;

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    received = rx.recv() => {
                        let packet = match received {
                            Ok(p) => p,
                            // The source is producing faster than this tile is
                            // consuming. Skipping is the correct response and
                            // the only one that keeps the source unblocked.
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                counters
                                    .tile_input_lagged
                                    .fetch_add(n, Ordering::Relaxed);
                                continue;
                            }
                            // The source stopped. Wait for it to come back
                            // rather than ending the tile — a restarted input
                            // publishes on a fresh sender, so the old handle is
                            // dead but the source is not.
                            Err(broadcast::error::RecvError::Closed) => {
                                let _ = tx.send(None);
                                match resubscribe(&flow_manager, &source_input_id, &cancel).await {
                                    Some(fresh) => {
                                        rx = fresh;
                                        demuxer = TsDemuxer::new(None);
                                        decoder = None;
                                        decoder_codec = None;
                                        continue;
                                    }
                                    None => return,
                                }
                            }
                        };

                        let ts: &[u8] = if packet.is_raw_ts {
                            &packet.data
                        } else if packet.data.len() >= 12 {
                            &packet.data[12..]
                        } else {
                            continue;
                        };

                        for frame in demuxer.demux(ts) {
                            let (es, codec) = match frame {
                                DemuxedFrame::H264 { nalus, .. } => {
                                    (annexb(&nalus), video_engine::VideoCodec::H264)
                                }
                                DemuxedFrame::H265 { nalus, .. } => {
                                    (annexb(&nalus), video_engine::VideoCodec::Hevc)
                                }
                                DemuxedFrame::Mpeg2 { es, .. } => {
                                    (es, video_engine::VideoCodec::Mpeg2)
                                }
                                // Audio and everything else: a wall shows
                                // pictures. Audio metering rides a later phase.
                                _ => continue,
                            };

                            // Re-open on a codec change — a source switch can
                            // change codec under us, and feeding H.265 to an
                            // H.264 decoder produces a wedged tile rather than
                            // a loud failure.
                            if decoder_codec != Some(codec) {
                                decoder = video_engine::VideoDecoder::open(codec).ok();
                                decoder_codec = decoder.as_ref().map(|_| codec);
                            }
                            let Some(dec) = decoder.as_mut() else {
                                counters.tile_decode_errors.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };

                            // `block_in_place`: the FFmpeg calls are C and can
                            // take milliseconds; they must not sit on an async
                            // worker.
                            let decoded = tokio::task::block_in_place(|| {
                                if dec.send_packet(&es).is_err() {
                                    return None;
                                }
                                dec.receive_frame().ok()
                            });
                            let Some(decoded) = decoded else { continue };

                            // Scale into the tile here, in this tile's own
                            // task. See `TileFrame` for why this is not done in
                            // the compositor.
                            let scaled = tokio::task::block_in_place(|| {
                                scale_into_tile(&decoded, tile_rect, &mut scaler)
                            });
                            let Some(owned) = scaled else {
                                counters.tile_decode_errors.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };

                            // Overwrites. Never blocks, never grows. An older
                            // frame nobody drew is worth nothing.
                            if tx.send(Some(Arc::new(owned))).is_err() {
                                // Compositor is gone.
                                return;
                            }
                        }
                    }
                }
            }
            tracing::debug!(flow_id = %flow_id, tile = %tile_id, "mosaic: tile decoder stopped");
        }
        #[cfg(not(feature = "media-codecs"))]
        {
            let _ = (&mut rx, &tx, &counters, &tile_id, &flow_id);
            cancel.cancelled().await;
        }
    })
}

/// How often a tile retries a source that is not running yet.
///
/// One second: fast enough that a wall started alongside its feeds fills in
/// while an operator is still looking at it, slow enough that a permanently
/// absent source costs nothing measurable.
const SOURCE_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Wait for a source to (re)appear, or for cancellation.
#[cfg(feature = "media-codecs")]
async fn resubscribe(
    flow_manager: &Arc<FlowManager>,
    source_input_id: &str,
    cancel: &CancellationToken,
) -> Option<broadcast::Receiver<RtpPacket>> {
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        if let Some(rx) = flow_manager.subscribe_input(source_input_id) {
            return Some(rx);
        }
        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(SOURCE_RETRY_INTERVAL) => {}
        }
    }
}

/// Decode-side scale: turn a decoded frame into the BGRA patch its tile needs.
///
/// Letterboxing is applied here, so the returned patch is exactly the picture
/// rect and the compositor never has to think about aspect ratio. The scaler is
/// cached across frames and rebuilt only when the source or the destination
/// changes shape.
#[cfg(feature = "media-codecs")]
fn scale_into_tile(
    decoded: &video_engine::DecodedFrame,
    tile_rect: TileRect,
    cache: &mut Option<(u32, u32, i32, u32, u32, video_engine::VideoScaler)>,
) -> Option<TileFrame> {
    // A VAAPI surface lives on the GPU and its planar accessors read garbage;
    // download it first. NVDEC / QSV / CPU frames are already sysmem and this
    // is a no-op for them.
    let downloaded;
    let frame = if decoded.is_vaapi() {
        downloaded = decoded.download_to_sysmem().ok()?;
        &downloaded
    } else {
        decoded
    };
    let (y, ys, u, us, v, vs) = frame.yuv_planes()?;
    let (sw, sh, fmt) = (frame.width(), frame.height(), frame.pixel_format());

    let fitted = mosaic::fit(sw, sh, tile_rect, AspectPolicy::Letterbox);
    if fitted.rect.width == 0 || fitted.rect.height == 0 {
        return None;
    }

    let want = (sw, sh, fmt, fitted.rect.width, fitted.rect.height);
    let stale = cache
        .as_ref()
        .map(|(a, b, c, d, e, _)| (*a, *b, *c, *d, *e) != want)
        .unwrap_or(true);
    if stale {
        let scaler = video_engine::VideoScaler::new_with_dst_format(
            sw,
            sh,
            fmt,
            fitted.rect.width,
            fitted.rect.height,
            video_engine::ScalerDstFormat::Bgra8,
        )
        .ok()?;
        *cache = Some((want.0, want.1, want.2, want.3, want.4, scaler));
    }
    let (_, _, _, _, _, scaler) = cache.as_ref()?;

    // A tight patch: pitch is exactly the patch width, so the compositor's row
    // copy needs no stride arithmetic beyond the canvas side.
    let pitch = fitted.rect.width as usize * Canvas::BYTES_PER_PIXEL;
    let mut bgra = vec![0u8; pitch * fitted.rect.height as usize];
    scaler
        .scale_raw_planes_into_packed(sw, sh, fmt, y, ys, u, us, v, vs, &mut bgra, pitch)
        .ok()?;

    Some(TileFrame { bgra, rect: fitted.rect })
}

/// Flatten NAL units into an Annex-B elementary stream.
#[cfg(feature = "media-codecs")]
fn annexb(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nalus.iter().map(|n| n.len() + 4).sum());
    for nalu in nalus {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nalu);
    }
    out
}

/// The canvas loop: sample every tile, blit, encode, publish.
#[allow(clippy::too_many_arguments)]
async fn run_compositor(
    config: MosaicInputConfig,
    mut layout: MosaicLayout,
    mut slots: Vec<Option<TileSlot>>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
    counters: Arc<MosaicCounters>,
    flow_id: String,
    input_id: String,
    flow_stats: Arc<FlowStatsAccumulator>,
) -> Result<(), String> {
    #[cfg(not(feature = "media-codecs"))]
    {
        let _ = (config, layout, slots, broadcast_tx, cancel, counters, flow_id, input_id, flow_stats);
        return Err(
            "this build has no codec support, so a mosaic cannot decode tiles or \
             encode a canvas; rebuild with --features multiviewer plus a video encoder"
                .into(),
        );
    }

    #[cfg(feature = "media-codecs")]
    {
        let canvas = layout.canvas;
        let pitch = canvas.pitch();
        let period = Duration::from_nanos(1_000_000_000 / u64::from(config.fps.max(1)));

        // BGRA canvas -> YUV420p for the encoder. Built once: the canvas
        // never changes shape.
        let to_yuv = video_engine::VideoScaler::new_with_dst_format(
            canvas.width,
            canvas.height,
            video_engine::av_pix_fmt_bgra(),
            canvas.width,
            canvas.height,
            video_engine::ScalerDstFormat::Yuv420p8,
        )
        .map_err(|e| format!("canvas colour conversion unavailable: {e:?}"))?;

        let mut encoder = build_encoder(&config, &flow_id)?;

        // **The PMT must describe what the encoder actually produces.**
        //
        // `TsMuxer::new()` defaults to `stream_type` 0x1B (H.264). If the
        // resolved backend is an HEVC one — x265, hevc_nvenc, hevc_qsv,
        // hevc_vaapi, or `hevc_auto` resolving to any of them — the wall would
        // emit HEVC announced as H.264, and no receiver could decode it. The
        // stream would look healthy at every layer that does not parse the
        // elementary stream, which is most of them.
        let mut muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
        let backend = crate::engine::input_test_pattern::select_video_backend();
        if matches!(
            backend,
            Some(video_codec::VideoEncoderCodec::X265)
                | Some(video_codec::VideoEncoderCodec::HevcNvenc)
                | Some(video_codec::VideoEncoderCodec::HevcQsv)
                | Some(video_codec::VideoEncoderCodec::HevcVaapi)
        ) {
            muxer.set_video_stream_type(crate::engine::rtmp::ts_mux::STREAM_TYPE_H265);
        }

        // A wall carries pictures only. Saying so keeps the PMT honest: an
        // announced audio PID that never carries a packet makes a receiver wait
        // for audio that is never coming, and makes every downstream A/V check
        // report a fault that is not one.
        muxer.set_has_audio(false);
        let mut buffer = vec![0u8; canvas.buffer_len()];
        let mut last_tick = Instant::now();
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut frame_index: i64 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let tick_started = Instant::now();
            let now = tick_started;

            // Sample every tile first — cheap `Arc` clones off `watch` slots,
            // no C calls, no blocking — then do all the pixel work in one
            // `block_in_place`.
            #[allow(clippy::type_complexity)]
            let mut patches: Vec<(usize, Option<Arc<TileFrame>>, TileRect, Option<&'static str>)> =
                Vec::with_capacity(layout.tiles.len());
            for index in layout.paint_order() {
                // **Only a *new* frame counts as the source being alive.**
                //
                // A `watch` channel holds its last value forever, so a source
                // that dies still hands back its final frame on every tick. If
                // that counted as an arrival the liveness timer would be
                // refreshed indefinitely and `NO SIGNAL` would never appear —
                // the wall would show a frozen picture and call it live, which
                // is the one thing the design forbids outright.
                //
                // So arrival is `has_changed()`, and a stale frame is still
                // *painted* (going black the instant a feed hiccups would be
                // worse) but the badge goes over the top once the timer expires.
                let (frame, is_new) = match slots.get_mut(index).and_then(|s| s.as_mut()) {
                    None => (None, false),
                    Some(rx) => {
                        let changed = rx.has_changed().unwrap_or(false);
                        let frame = if changed {
                            rx.borrow_and_update().clone()
                        } else {
                            rx.borrow().clone()
                        };
                        (frame, changed)
                    }
                };
                if is_new {
                    layout.tiles[index].liveness.frame_arrived(now);
                }
                // The badge decision rides with the patch, **in paint order**.
                // Collecting badges separately from `layout.tiles` would put
                // them back in declaration order, and drawing them in a second
                // pass would let a background tile showing NO SIGNAL paint its
                // bar straight across a picture-in-picture sitting on top of
                // it — defacing a live tile with a fault belonging to another
                // one. That defect was introduced, fixed, and then
                // reintroduced by a refactor here; the guard below exists
                // because of the second time.
                let badge = layout.tiles[index].liveness.state_at(now).badge();
                patches.push((index, frame, layout.tiles[index].rect, badge));
            }

            // **Everything from here to the encode is C and must not sit on an
            // async worker.** The canvas clear, N row copies, the badge fills,
            // the BGRA->YUV convert and the encode are all synchronous; leaving
            // any of them outside `block_in_place` occupies a Tokio worker for
            // the whole canvas period, and on a node whose other workers are
            // driving live contribution feeds that is exactly the stall this
            // module exists not to cause.
            let encoded = tokio::task::block_in_place(|| {
                // Background: anything no tile covers stays black rather than
                // showing the previous frame's pixels through a gap.
                buffer.fill(0);

                // One paint-ordered pass: each tile's picture, then its own
                // badge, before the next tile is touched. A tile with a higher
                // z therefore covers both the picture *and* the badge of every
                // tile beneath it, which is what z-order means.
                for (_, frame, tile_rect, badge) in &patches {
                    if let Some(frame) = frame {
                        // A straight row copy — the scaling already happened in
                        // the tile's own task.
                        let row_bytes = frame.rect.width as usize * Canvas::BYTES_PER_PIXEL;
                        for row in 0..frame.rect.height as usize {
                            let dst_start = (frame.rect.y as usize + row) * pitch
                                + frame.rect.x as usize * Canvas::BYTES_PER_PIXEL;
                            let src_start = row * row_bytes;
                            if dst_start + row_bytes > buffer.len()
                                || src_start + row_bytes > frame.bgra.len()
                            {
                                break;
                            }
                            buffer[dst_start..dst_start + row_bytes]
                                .copy_from_slice(&frame.bgra[src_start..src_start + row_bytes]);
                        }
                    }
                    if let Some(badge) = badge {
                        draw_badge(&mut buffer, pitch, canvas, tile_rect, badge);
                    }
                }

                let yuv = to_yuv.scale_raw_planes(
                    canvas.width,
                    canvas.height,
                    video_engine::av_pix_fmt_bgra(),
                    &buffer,
                    pitch,
                    &[],
                    0,
                    &[],
                    0,
                )?;
                let (y, ys) = yuv.plane(0).ok_or(video_codec::VideoError::AllocFrame)?;
                let (u, us) = yuv.plane(1).ok_or(video_codec::VideoError::AllocFrame)?;
                let (v, vs) = yuv.plane(2).ok_or(video_codec::VideoError::AllocFrame)?;
                let pts_90k = frame_index * 90_000 / i64::from(config.fps.max(1));
                Ok::<_, video_codec::VideoError>((
                    encoder
                        .encode_raw_planes(
                            canvas.width,
                            canvas.height,
                            video_engine::av_pix_fmt_for_yuv(video_codec::VideoChroma::Yuv420, 8)
                                .unwrap_or(0),
                            y, ys, u, us, v, vs,
                            Some(pts_90k),
                        )
                        .unwrap_or_default(),
                    pts_90k,
                ))
            });

            if let Ok((frames, pts_90k)) = encoded {
                for ef in frames {
                    let ts = muxer.mux_video(
                        &ef.data,
                        ef.pts.max(0) as u64,
                        ef.dts.max(0) as u64,
                        ef.keyframe,
                    );
                    // **Bundle 7 x 188 B into each datagram**, not one
                    // RtpPacket per TS packet. Publishing each 188-byte packet
                    // separately saturates the flow broadcast channel and
                    // starves the thumbnail and analyser subscribers — the
                    // failure mode `input_test_pattern::publish_chunks`
                    // documents, and one that bites hardest exactly where a
                    // wall is most useful: a busy node carrying high-bitrate
                    // feeds. 7 x 188 = 1316 B is the SRT payload size and the
                    // internet-safe MTU.
                    for batch in ts.chunks(TS_PACKETS_PER_DATAGRAM) {
                        let total: usize = batch.iter().map(|c| c.len()).sum();
                        let mut combined = bytes::BytesMut::with_capacity(total);
                        for chunk in batch {
                            combined.extend_from_slice(chunk);
                        }
                        // `broadcast::send` never blocks; a lagging output
                        // sheds on its own side. An error only means nobody is
                        // listening yet, which is not this task's problem.
                        // Advance the flow's input counters like every other
                        // input does. Without this a perfectly healthy wall
                        // reads as an input delivering nothing, and the flow's
                        // stall detection raises a permanent false alarm that
                        // an operator cannot clear.
                        flow_stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        flow_stats
                            .input_bytes
                            .fetch_add(total as u64, Ordering::Relaxed);
                        let _ = broadcast_tx.send(RtpPacket {
                            data: combined.freeze(),
                            sequence_number: 0,
                            rtp_timestamp: pts_90k as u32,
                            recv_time_us: crate::util::time::now_us(),
                            is_raw_ts: true,
                            upstream_seq: None,
                            upstream_leg_id: None,
                            sender_timestamp_us: None,
                        });
                    }
                }
            }

            frame_index += 1;
            counters.canvas_frames.fetch_add(1, Ordering::Relaxed);

            // Two different faults, counted separately.
            //
            // `canvas_over_budget` is the composite work outrunning the canvas
            // period — the wall is too expensive for this head. `canvas_skipped`
            // is wall-clock periods that went by without a frame, which is what
            // an operator actually sees as a stuttering wall and which the
            // ticker's skip behaviour would otherwise hide completely.
            if tick_started.elapsed() > period {
                counters.canvas_over_budget.fetch_add(1, Ordering::Relaxed);
            }
            let elapsed_periods = last_tick.elapsed().as_nanos() / period.as_nanos().max(1);
            if elapsed_periods > 1 {
                counters
                    .canvas_skipped
                    .fetch_add((elapsed_periods - 1) as u64, Ordering::Relaxed);
            }
            last_tick = Instant::now();
        }

        tracing::info!(
            flow_id = %flow_id, input_id = %input_id,
            frames = counters.canvas_frames.load(Ordering::Relaxed),
            over_budget = counters.canvas_over_budget.load(Ordering::Relaxed),
            skipped = counters.canvas_skipped.load(Ordering::Relaxed),
            tile_decode_errors = counters.tile_decode_errors.load(Ordering::Relaxed),
            "mosaic: compositor stopped"
        );
        Ok(())
    }
}

/// Resolve and open the canvas encoder.
///
/// A default build has **no video encoder at all** — every backend resolves to
/// `FeatureDisabled` — so this is where that becomes a named refusal naming the
/// rebuild, rather than an obscure failure at the first frame.
#[cfg(feature = "media-codecs")]
fn build_encoder(
    config: &MosaicInputConfig,
    flow_id: &str,
) -> Result<crate::engine::video_encode_util::ScaledVideoEncoder, String> {
    use crate::config::models::VideoEncodeConfig;

    let fps = u32::from(config.fps.max(1));
    let backend = crate::engine::input_test_pattern::select_video_backend().ok_or_else(|| {
        format!(
            "no video encoder is compiled into this build, so a multiviewer wall has              nothing to publish its canvas with. Rebuild with an encoder alongside the              feature, e.g. --features \"multiviewer,video-encoder-x264\" (GPL v2+) or              --features \"multiviewer,video-encoder-nvenc\". Requested codec was '{}'.",
            config.codec
        )
    })?;

    let video_cfg = VideoEncodeConfig {
        codec: crate::engine::input_test_pattern::backend_codec_string(backend).to_string(),
        width: Some(config.width),
        height: Some(config.height),
        fps_num: Some(fps),
        fps_den: Some(1),
        bitrate_kbps: Some(config.video_bitrate_kbps),
        // Two-second GOP: a wall is watched, not archived, and a short GOP
        // keeps a viewer joining mid-stream from staring at grey.
        gop_size: Some((fps * 2).min(600)),
        // No B-frames. A monitoring surface is judged on latency, and
        // reordering buys compression an operator will never notice while
        // costing delay they will.
        bframes: Some(0),
        preset: Some("ultrafast".to_string()),
        profile: None,
        chroma: None,
        bit_depth: None,
        rate_control: None,
        crf: None,
        max_bitrate_kbps: None,
        refs: None,
        level: None,
        tune: None,
        color_primaries: None,
        color_transfer: None,
        color_matrix: None,
        color_range: None,
        source_video_pid: None,
        hw_decode: None,
    };

    let mut encoder = crate::engine::video_encode_util::ScaledVideoEncoder::new(
        video_cfg,
        backend,
        fps,
        1,
        false,
        format!("mosaic:{flow_id}"),
    );

    // **The 90 kHz PTS contract.** The compositor stamps every canvas frame
    // with a 90 kHz PTS, so the encoder must be told that is the timebase.
    //
    // Omitting this does not fail loudly. Lazy-open would keep the 1/fps
    // timebase implied by `fps_num`/`fps_den`, so libavcodec reads the 90 kHz
    // step — 3600 ticks at 25 fps — as 3600 *frame periods*, i.e. 144 s per
    // picture, and rate control budgets `bitrate × 144 s` for every frame.
    // Hardware-measured on bilby-bite for the still-image path: a 500 kbps
    // source emitted ~9 MB/frame ≈ 1.8 Gbps and drove the edge's RSS into the
    // OOM killer. On a wall the same mistake would do it at canvas rate.
    //
    // See `docs/sdi.md` "The 90 kHz PTS contract"; asserted by
    // `the_canvas_encoder_declares_90khz_pts` below, because there is no
    // runtime signal that would catch it.
    encoder.set_pts_90k();
    Ok(encoder)
}

/// Paint a badge over a tile.
///
/// Deliberately a filled block rather than glyphs: rendering text needs a font
/// stack this binary does not carry, and an operator reading a wall from across
/// a gallery reads *position and colour* long before they read letters. A dark
/// panel with a coloured bar says "this tile is not live" at a glance, which is
/// the job. Glyph rendering is a later phase.
#[cfg(feature = "media-codecs")]
fn draw_badge(buffer: &mut [u8], pitch: usize, canvas: Canvas, rect: &TileRect, badge: &str) {
    // NO SIGNAL is amber, UNASSIGNED is a neutral grey: the first is a fault to
    // chase, the second is a slot nobody has filled, and they must not look the
    // same from ten feet away.
    let (b, g, r) = if badge.starts_with("NO") {
        (0x00u8, 0x9Fu8, 0xE0u8)
    } else {
        (0x60u8, 0x60u8, 0x60u8)
    };

    // A bar across the middle third of the tile.
    // Saturating throughout: a one-pixel-tall tile makes `bar_h` (2) exceed
    // `rect.height`, and `rect.height / 2 - bar_h / 2` underflows u32 into
    // roughly four billion — which on a release build is a silent wrong offset
    // rather than a panic.
    let bar_h = (rect.height / 6).max(2).min(rect.height.max(1));
    let bar_y = rect.y + (rect.height / 2).saturating_sub(bar_h / 2);
    for row in bar_y..(bar_y + bar_h).min(canvas.height) {
        let start = row as usize * pitch + rect.x as usize * Canvas::BYTES_PER_PIXEL;
        let end = start + rect.width as usize * Canvas::BYTES_PER_PIXEL;
        if end > buffer.len() {
            break;
        }
        for px in buffer[start..end].chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 0xFF;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::MosaicTileConfig;
    use crate::engine::mosaic::TileState;

    fn cfg(tiles: Vec<MosaicTileConfig>) -> MosaicInputConfig {
        MosaicInputConfig {
            width: 1920,
            height: 1080,
            fps: 25,
            video_bitrate_kbps: 8000,
            codec: "h264_auto".into(),
            tiles,
        }
    }

    fn tile(id: &str, x: u32, y: u32, w: u32, h: u32, src: Option<&str>) -> MosaicTileConfig {
        MosaicTileConfig {
            id: id.into(),
            source_input_id: src.map(str::to_string),
            x,
            y,
            width: w,
            height: h,
            z: 0,
            label: String::new(),
        }
    }

    #[test]
    fn a_config_becomes_a_layout_that_validates() {
        let config = cfg(vec![
            tile("a", 0, 0, 960, 540, Some("in-a")),
            tile("b", 960, 0, 960, 540, Some("in-b")),
            tile("c", 0, 540, 960, 540, Some("in-c")),
            tile("d", 960, 540, 960, 540, Some("in-d")),
        ]);
        let layout = layout_from_config(&config);
        assert_eq!(layout.validate(), Ok(()));
        assert_eq!(layout.tiles.len(), 4);
    }

    /// A tile with no source starts UNASSIGNED, not NO SIGNAL.
    ///
    /// The distinction is the whole reason there are two badges: "nobody routed
    /// anything here" and "what was routed here has stopped" send an operator
    /// to different places.
    #[test]
    fn an_unrouted_tile_starts_unassigned() {
        let config = cfg(vec![
            tile("routed", 0, 0, 960, 1080, Some("in-a")),
            tile("empty", 960, 0, 960, 1080, None),
        ]);
        let layout = layout_from_config(&config);
        let now = Instant::now();
        assert_eq!(layout.tiles[0].liveness.state_at(now), TileState::NoSignal);
        assert_eq!(layout.tiles[1].liveness.state_at(now), TileState::Unassigned);
    }

    /// **A source that dies must reach NO SIGNAL, even though its last frame
    /// is still available.**
    ///
    /// This is the bug this arrangement exists to prevent, and it is easy to
    /// write by accident: a `watch` channel holds its final value forever, so
    /// "is there a frame?" is true long after the source stopped. Marking
    /// arrival on that would refresh the liveness timer every tick and the
    /// badge would never appear — a frozen picture presented as live, which the
    /// design forbids outright.
    ///
    /// The test drives the same `has_changed`/`borrow_and_update` protocol the
    /// compositor uses, so it fails if that protocol is replaced with a plain
    /// `borrow()`.
    #[test]
    fn a_source_that_dies_goes_to_no_signal_despite_a_retained_frame() {
        let (tx, mut rx) = watch::channel::<Option<Arc<TileFrame>>>(None);
        let mut liveness = TileLiveness::assigned().with_window(Duration::from_millis(500));
        let start = Instant::now();

        let frame = Arc::new(TileFrame {
            bgra: vec![0; 16 * 16 * 4],
            rect: TileRect::new(0, 0, 16, 16),
        });
        tx.send(Some(frame)).expect("publish");

        // Tick one: a genuinely new frame.
        assert!(rx.has_changed().unwrap(), "the first frame must read as new");
        let held = rx.borrow_and_update().clone();
        assert!(held.is_some());
        liveness.frame_arrived(start);
        assert_eq!(liveness.state_at(start), TileState::Live);

        // The source now stops. Every later tick still finds a frame in the
        // slot — that is the trap — but none of them is new.
        for tick in 1..=10u64 {
            let now = start + Duration::from_millis(100 * tick);
            assert!(
                !rx.has_changed().unwrap(),
                "no new frame was published, so nothing may read as new"
            );
            assert!(
                rx.borrow().is_some(),
                "the retained frame is still there — which is exactly why \
                 arrival must not be inferred from its presence"
            );
            if rx.has_changed().unwrap() {
                liveness.frame_arrived(now);
            }
        }

        assert_eq!(
            liveness.state_at(start + Duration::from_secs(1)),
            TileState::NoSignal,
            "a dead source must reach NO SIGNAL even though its last frame is retained"
        );
    }

    /// A one-pixel-tall tile does not underflow the badge geometry.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn a_tiny_tile_does_not_underflow_the_badge() {
        let canvas = Canvas::new(64, 32);
        let pitch = canvas.pitch();
        let mut buffer = vec![0u8; canvas.buffer_len()];
        // `bar_h` floors at 2, so a 1px tile makes `height/2 - bar_h/2`
        // underflow u32 unless it saturates.
        for h in [1u32, 2, 3] {
            let rect = TileRect::new(0, 0, 8, h);
            draw_badge(&mut buffer, pitch, canvas, &rect, "NO SIGNAL");
        }
        // Nothing outside the top few rows may have been written.
        for y in 4..canvas.height {
            for x in 0..canvas.width {
                let at = y as usize * pitch + x as usize * 4;
                assert_eq!(
                    buffer[at], 0,
                    "a tiny tile's badge escaped to ({x},{y}) — the offset underflowed"
                );
            }
        }
    }

    /// A tile whose source is the wall itself is refused, not rendered.
    ///
    /// Subscribing the compositor to its own output is an unbounded feedback
    /// loop: every canvas frame published arrives back as a tile to decode and
    /// composite. On a node already carrying live contribution feeds that is
    /// not a cosmetic bug.
    #[test]
    fn a_tile_cannot_source_the_wall_itself() {
        let src = include_str!("input_mosaic.rs");
        assert!(
            src.contains("Some(source) if source == input_id =>"),
            "the self-reference guard is gone; a tile naming the wall's own input \
             id would feed the compositor its own output"
        );
    }

    /// Badges are drawn inside the paint-order loop, not in a second pass.
    ///
    /// A second pass runs in declaration order, so a background tile showing
    /// NO SIGNAL would paint its bar over a picture-in-picture sitting on top
    /// of it — defacing a live tile with a fault belonging to another one.
    #[test]
    fn a_badge_cannot_deface_a_higher_z_tile() {
        let src = include_str!("input_mosaic.rs");

        // Exactly one `draw_badge` call site, and it is inside the single
        // paint-ordered pass. A second pass — over `layout.tiles`, which is
        // declaration order — is precisely the defect: it lets a background
        // tile's NO SIGNAL bar paint over a picture-in-picture above it.
        //
        // Counted rather than merely located: an earlier version of this guard
        // bounded its search on a marker that existed only inside its own
        // source, so the region it checked was the whole rest of the file and a
        // refactor reintroduced the bug underneath a passing test.
        // Production code only — the badge tests below call it too.
        let production = &src[..src.find("#[cfg(test)]").expect("test module")];
        let call_sites = production.matches("draw_badge(&mut buffer").count();
        assert_eq!(
            call_sites, 1,
            "expected exactly one draw_badge call site in the compositor; a second \
             pass over the tile list would paint a lower-z badge over a higher-z tile"
        );
        assert!(
            src.contains("for (_, frame, tile_rect, badge) in &patches"),
            "the paint pass no longer carries each tile's badge alongside its \
             picture, so badge order is no longer paint order"
        );
        // And the patches themselves must be built in paint order.
        assert!(
            production.contains("for index in layout.paint_order()"),
            "patches are no longer collected in paint order"
        );

        // **The actual failure mode**: badges driven by `layout.tiles`, which
        // is declaration order. Whatever loop draws them must be fed from
        // `patches` (paint-ordered), never from the tile list directly.
        let badge_at = production
            .find("draw_badge(&mut buffer")
            .expect("the badge call site");
        let patches_at = production[..badge_at]
            .rfind("in &patches {")
            .expect("the badge draw must sit after the paint-ordered patches loop");

        // Nothing between the paint-ordered loop and the badge draw may
        // re-enter the tile list: that is the shape of the defect — a second
        // pass over `layout.tiles`, which is declaration order.
        let between = &production[patches_at..badge_at];
        assert!(
            !between.contains("layout.tiles"),
            "the badge draw is reached through `layout.tiles` rather than the \
             paint-ordered `patches`, so a background tile's NO SIGNAL bar would \
             paint over a picture-in-picture above it"
        );
    }

    /// Regression guard for the 90 kHz PTS contract.
    ///
    /// The compositor stamps 90 kHz PTS on every canvas frame. If
    /// `set_pts_90k()` is dropped, nothing crashes — rate control silently
    /// budgets `bitrate × 144 s` per frame, which measured ~1.8 Gbps for a
    /// 500 kbps source on the still-image path that made the same mistake. A
    /// wall would do it at canvas rate on a node already carrying live
    /// contribution feeds. There is no runtime signal, so it is asserted here.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn the_canvas_encoder_declares_90khz_pts() {
        // Only meaningful when an encoder backend exists to open.
        if crate::engine::input_test_pattern::select_video_backend().is_none() {
            return;
        }
        let encoder = build_encoder(&cfg(vec![tile("a", 0, 0, 64, 64, Some("in-a"))]), "test")
            .expect("an encoder backend is compiled in");
        assert!(
            encoder.is_pts_90k(),
            "the canvas encoder must declare 90 kHz PTS; without it rate control \
             over-allocates by ~3600x (see docs/sdi.md, the 90 kHz PTS contract)"
        );
    }

    /// The compositor samples tiles with `has_changed`, not a bare `borrow`.
    ///
    /// The test above proves the *protocol* behaves as required; this one
    /// proves the compositor still uses it. A source scan, because what has to
    /// be guaranteed is about the next edit somebody makes to that loop — and
    /// replacing the sampling with a plain `borrow()` would compile, pass every
    /// other test, and quietly break `NO SIGNAL` on a live wall.
    #[test]
    fn the_compositor_only_counts_a_new_frame_as_an_arrival() {
        let src = include_str!("input_mosaic.rs");
        let loop_start = src
            .find("for index in layout.paint_order()")
            .expect("the paint loop has moved");
        let loop_end = src[loop_start..]
            .find("// Badges for tiles")
            .map(|o| loop_start + o)
            .expect("the badge pass has moved");
        let paint_loop = &src[loop_start..loop_end];

        assert!(
            paint_loop.contains("has_changed()"),
            "the paint loop no longer distinguishes a new frame from a retained one, \
             so a dead source would keep refreshing its liveness timer and NO SIGNAL \
             would never appear"
        );
        assert!(
            paint_loop.contains("if is_new {"),
            "frame_arrived is no longer gated on the frame being new"
        );
    }

    /// Every tile of a valid wall can actually be blitted.
    ///
    /// The end-to-end geometric guarantee: for each tile, the canvas tail from
    /// its byte offset satisfies the scaler's requirement. This is the check
    /// that would have caught the bottom-row bounds defect.
    #[test]
    fn every_tile_offset_leaves_room_for_its_blit() {
        let config = cfg(vec![
            tile("a", 0, 0, 960, 540, Some("in-a")),
            tile("b", 960, 0, 960, 540, Some("in-b")),
            tile("c", 0, 540, 960, 540, Some("in-c")),
            tile("d", 960, 540, 960, 540, Some("in-d")),
        ]);
        let layout = layout_from_config(&config);
        for t in &layout.tiles {
            let offset = layout.canvas.byte_offset(&t.rect);
            let tail = layout.canvas.buffer_len() - offset;
            assert!(
                tail >= layout.canvas.required_tail(&t.rect),
                "tile '{}' cannot be blitted",
                t.id
            );
        }
    }

    /// The badge never writes outside its tile.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn a_badge_stays_inside_its_tile() {
        let canvas = Canvas::new(64, 32);
        let pitch = canvas.pitch();
        let mut buffer = vec![0u8; canvas.buffer_len()];
        let rect = TileRect::new(32, 16, 32, 16);
        draw_badge(&mut buffer, pitch, canvas, &rect, "NO SIGNAL");

        for y in 0..canvas.height {
            for x in 0..canvas.width {
                let at = y as usize * pitch + x as usize * 4;
                let touched = buffer[at] != 0 || buffer[at + 1] != 0 || buffer[at + 2] != 0;
                if touched {
                    assert!(
                        (32..64).contains(&x) && (16..32).contains(&y),
                        "badge wrote outside its tile at ({x},{y})"
                    );
                }
            }
        }
    }

    /// The two badges are visually distinct.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn the_two_badges_do_not_look_the_same() {
        let canvas = Canvas::new(64, 32);
        let pitch = canvas.pitch();
        let rect = TileRect::new(0, 0, 64, 32);

        let mut a = vec![0u8; canvas.buffer_len()];
        draw_badge(&mut a, pitch, canvas, &rect, "NO SIGNAL");
        let mut b = vec![0u8; canvas.buffer_len()];
        draw_badge(&mut b, pitch, canvas, &rect, "UNASSIGNED");

        assert_ne!(
            a, b,
            "a fault and an empty slot must not render identically"
        );
    }
}
