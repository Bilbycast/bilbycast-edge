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

// ─────────────────────── head advertisement ───────────────────────
//
// `MULTIVIEWER_PLAN.md` §"The minimum separation that must land in phase 1",
// item 2: *a Head is a capability advertised by a node —
// `{head_id, node_id, kind, max_canvas, encoder_backends}`, discovered from
// `HealthPayload`, exactly the shape `DisplayDevice` already uses.* This is
// that shape. The manager mirrors it into `mv_heads`, which is why every
// derived column over there is documented as health-tick owned.

/// The node-local id of the phase-1 stream head.
///
/// A **constant**, and that is load-bearing rather than lazy. `mv_heads` keys
/// on `(node_id, head_id)` and its schema requires the id be "stable across
/// reboots on the node's side" — so anything derived from runtime state (a flow
/// id, an input id, a uuid minted at boot) would mint a *second* head row on
/// every restart and strand the wall pointing at the retired one. A constant is
/// stable by construction.
///
/// One head, because phase 1 is scoped "one wall, one head, one node" and the
/// manager enforces exactly that in `refuse_double_booking`. Panel and SDI
/// heads are enumerable — a KMS connector each, a DeckLink port each — and
/// arrive with their own ids in phase 2.
pub const STREAM_HEAD_ID: &str = "stream0";

/// What this node says about one of its heads, on the health tick.
///
/// Field names are the manager's `HeadAdvertisement` (`manager-core`,
/// `db::multiviewer`), which deserialises this verbatim. Renaming a field here
/// silently stops that column being refreshed — the value simply stops arriving
/// and `last_seen_at` keeps advancing, which looks like a healthy head with
/// stale capabilities rather than like a bug.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HeadAdvertisement {
    pub head_id: String,
    /// `"stream"`, `"panel"` or `"sdi"` — a CHECK constraint on the manager
    /// side rejects anything else, so this is not free-form.
    pub kind: &'static str,
    /// A KMS connector or DeckLink port name for the phase-2 kinds; `None` for
    /// a stream head, which occupies no physical port.
    pub connector: Option<String>,
    pub max_canvas_width: u32,
    pub max_canvas_height: u32,
    /// Free-form JSON, stored as JSONB. A column per capability would be a
    /// migration per firmware.
    pub capabilities: serde_json::Value,
}

/// The heads this node can actually drive, for `HealthPayload`.
///
/// Empty — so the manager mirrors nothing — unless an encoder backend resolves.
/// That is the same condition that gates the `mv-compositor` capability bit,
/// and deliberately so: the flow bus carries MPEG-TS, so a composite reaches an
/// output only by being encoded, and a build with the feature but no encoder
/// refuses at flow start. Advertising a head such a node cannot drive would put
/// it in the operator's picker and fail at the moment it went to air.
///
/// `encoder_backends` carries the backends a wall on this node **would
/// actually use**, head first — the resolved `h264_auto` chain for the default
/// canvas codec, filtered by what this host can open.
///
/// It used to name `select_video_backend`'s answer, which is x264 on every
/// published artefact whatever the host has, so the manager was told a wall on a
/// 32-session QSV box would encode on the CPU. It now would not (#129).
///
/// **The gate stays compile-time**, deliberately. Whether a head exists at all
/// is `select_video_backend().is_some()` — "is any encoder in this binary" —
/// which is the question that function is right for, is the same one behind the
/// `mv-compositor` capability, and is answerable cold. The release workflow
/// asserts that capability against a freshly built binary with no probe run, so
/// making head *existence* depend on the runtime probe would fail every release.
/// Only the reported backend list consults the probe, and it falls back to the
/// compile-time answer before the probe has run.
pub fn advertised_heads() -> Vec<HeadAdvertisement> {
    let Some(codec) = crate::engine::input_test_pattern::select_video_backend() else {
        return Vec::new();
    };
    #[cfg(feature = "media-codecs")]
    let backends: Vec<String> = {
        let probe = crate::engine::hardware_probe::static_capabilities();
        match probe.as_deref().and_then(|caps| {
            crate::engine::hardware_probe::resolve_video_encoder_chain(
                &crate::config::models::default_mosaic_codec(),
                None,
                None,
                Some(caps),
            )
            .ok()
        }) {
            Some(chain) if !chain.is_empty() => {
                // Through `as_video_encoder_codec` so both branches spell a
                // backend the same way. `ResolvedVideoEncoder::ffmpeg_name`
                // says "x264" where `VideoEncoderCodec::ffmpeg_name` says
                // "libx264", and this field is documented as a verbatim wire
                // contract the manager deserialises.
                chain
                    .iter()
                    .map(|r| r.as_video_encoder_codec().ffmpeg_name().to_string())
                    .collect()
            }
            _ => vec![codec.ffmpeg_name().to_string()],
        }
    };
    #[cfg(not(feature = "media-codecs"))]
    let backends: Vec<String> = vec![codec.ffmpeg_name().to_string()];
    vec![HeadAdvertisement {
        head_id: STREAM_HEAD_ID.to_string(),
        kind: "stream",
        connector: None,
        max_canvas_width: mosaic::MAX_CANVAS_W,
        max_canvas_height: mosaic::MAX_CANVAS_H,
        capabilities: serde_json::json!({
            "encoder_backends": backends,
        }),
    }]
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

        let (mut encoder, backend_chain) = build_encoder(&config, &flow_id)?;

        // **The PMT must describe what the encoder actually produces.**
        //
        // `TsMuxer::new()` defaults to `stream_type` 0x1B (H.264). If the
        // resolved backend is an HEVC one — x265, hevc_nvenc, hevc_qsv,
        // hevc_vaapi, or `hevc_auto` resolving to any of them — the wall would
        // emit HEVC announced as H.264, and no receiver could decode it. The
        // stream would look healthy at every layer that does not parse the
        // elementary stream, which is most of them.
        let mut muxer = crate::engine::rtmp::ts_mux::TsMuxer::new();
        // **From the chain that was actually resolved, not from a second
        // opinion.** This asked `select_video_backend()` again, which answers a
        // different question from the one `build_encoder` had just answered — so
        // a wall encoding HEVC could be announced in the PMT as H.264 (0x1B).
        // Nothing downstream of the mux parses the elementary stream to notice.
        // The chain is family-pure, so its head settles this even though the
        // encoder has not opened yet.
        if chain_is_hevc(&backend_chain) {
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
                )
                .map_err(EncodeStep::Scale)?;
                let (y, ys) = yuv.plane(0).ok_or(EncodeStep::MissingPlane("Y"))?;
                let (u, us) = yuv.plane(1).ok_or(EncodeStep::MissingPlane("U"))?;
                let (v, vs) = yuv.plane(2).ok_or(EncodeStep::MissingPlane("V"))?;
                let pts_90k = frame_index * 90_000 / i64::from(config.fps.max(1));
                // **The encode result, not `unwrap_or_default()`.**
                //
                // The encoder lazy-opens on this call, so the whole backend
                // chain refusing to open arrives here as an `Err`. Discarding it
                // left the wall ticking forever with `canvas_frames` climbing,
                // zero packets published, and nothing in a log or an event.
                //
                // Unreachable in practice while the chain was always `[X264]`,
                // which essentially never fails `avcodec_open2` for a valid
                // canvas. Honouring `config.codec` (#129) makes it reachable: an
                // explicit backend resolves to a **one-element** chain with
                // nothing to fall through to, and a hardware open fails for
                // reasons the boot probe cannot see — sessions exhausted, a
                // driver replaced, a render node re-permissioned.
                let frames = encoder
                    .encode_raw_planes(
                        canvas.width,
                        canvas.height,
                        video_engine::av_pix_fmt_for_yuv(video_codec::VideoChroma::Yuv420, 8)
                            .unwrap_or(0),
                        y, ys, u, us, v, vs,
                        Some(pts_90k),
                    )
                    .map_err(EncodeStep::Encode)?;
                Ok::<_, EncodeStep>((frames, pts_90k))
            });

            let (frames, pts_90k) = match encoded {
                Ok(value) => value,
                Err(EncodeStep::Encode(reason)) if !encoder.is_open() => {
                    // **An encoder that never opened is a dead input, not a bad
                    // frame.** `is_open()` is what separates the chain refusing
                    // every candidate from an open encoder rejecting one canvas.
                    // Returning routes through the Critical `mosaic_failed` event
                    // `spawn_mosaic_input` already emits, which is the honest
                    // operator-facing outcome: a wall that cannot encode is not a
                    // wall, and it must not sit there looking alive.
                    return Err(format!(
                        "multiviewer wall on flow '{flow_id}': the canvas encoder never opened: {reason}"
                    ));
                }
                Err(reason) => {
                    // An open encoder that refused one canvas, or a colour
                    // conversion that failed. Costs a frame, not the wall.
                    tracing::warn!(
                        flow_id = %flow_id,
                        "mosaic: dropping a canvas frame: {reason}",
                    );
                    frame_index += 1;
                    continue;
                }
            };
            {
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

/// The backends this canvas may be encoded with, head first.
///
/// **Through the probe, never through `select_video_backend`.** That function
/// answers a *compile-time* question — "is any encoder in this binary?" — by
/// returning the first `cfg!` match in a fixed order, x264 first. It has no
/// VAAPI or RKMPP arm at all, and its NVENC and QSV arms are gated
/// `not(video-encoder-x264)`, which every published artefact defines. So it
/// returns X264 unconditionally on every shipped build: a wall on an Intel host
/// with 32 idle QSV sessions encoded on the CPU, and on `-rockchip` the VPU that
/// artefact exists for sat idle. `MULTIVIEWER_PLAN.md` §6 forbids copying it
/// here, by name, for exactly this reason; edge #129 is it having been copied.
///
/// `resolve_chain_for_video_encode_config` asks the *runtime* question instead,
/// against probed `StaticCapabilities`, and honours the operator's `codec` —
/// which `MosaicInputConfig::codec` has always documented as accepting
/// `h264_auto` / `x264` / `h264_nvenc` / … and defaulting to `h264_auto`.
///
/// The chain is family-pure by construction (`auto_priority_chain` returns all
/// H.264 backends or all HEVC ones), so a demote at `avcodec_open2` changes the
/// backend and never the wire codec. That is what lets the caller settle the
/// PMT stream type from this without waiting for the encoder to open.
///
/// Falls back to `select_video_backend` only where there is no probe snapshot —
/// in-process tests and early startup — which is the presence question it is
/// actually right for.
#[cfg(feature = "media-codecs")]
fn canvas_backend_chain(
    video_cfg: &crate::config::models::VideoEncodeConfig,
    flow_id: &str,
    caps: Option<&crate::engine::hardware_probe::StaticCapabilities>,
) -> Result<Vec<video_codec::VideoEncoderCodec>, String> {
    let no_encoder = || {
        format!(
            "no video encoder is compiled into this build, so a multiviewer wall has \
             nothing to publish its canvas with. Rebuild with an encoder alongside the \
             feature, e.g. --features \"multiviewer,video-encoder-x264\" (GPL v2+) or \
             --features \"multiviewer,video-encoder-nvenc\". Requested codec was '{}'.",
            video_cfg.codec
        )
    };
    // **Nothing compiled in is a compile-time fact, answered before the
    // resolver.** Mapping a resolver error onto it instead would over-fire —
    // `hevc_auto` on an x264-only build fails to resolve and is not a
    // no-encoder build — and would lose `FeatureDisabled`'s more precise text.
    // This is also the message that names the rebuild, which was the whole point
    // of having it and which became unreachable when the resolver moved in front.
    if !crate::engine::hardware_probe::any_video_encoder_compiled() {
        return Err(no_encoder());
    }
    // Caps arrive as an argument rather than out of the global, so this is a
    // pure function of `(cfg, caps)` — which is what lets a test drive the
    // Rockchip, NVIDIA and bare-CPU hosts on an x86 runner, as
    // `MULTIVIEWER_PLAN.md` §6 requires. `install_static_capabilities` is a
    // set-once `OnceLock`, so faking the global would leak into every other test
    // in the binary.
    if let Some(caps) = caps {
        let chain = crate::engine::hardware_probe::resolve_video_encoder_chain(
            &video_cfg.codec,
            video_cfg.chroma.as_deref(),
            video_cfg.bit_depth,
            Some(caps),
        )
        .map_err(|e| {
                format!(
                    "multiviewer wall on flow '{flow_id}': no encoder can carry codec '{}': {}",
                    video_cfg.codec,
                    e.message()
                )
            })?;
        if chain.is_empty() {
            return Err(no_encoder());
        }
        tracing::info!(
            "multiviewer wall on flow '{flow_id}': codec '{}' resolved to {:?}",
            video_cfg.codec,
            chain.iter().map(|r| r.ffmpeg_name()).collect::<Vec<_>>(),
        );
        return Ok(chain.iter().map(|r| r.as_video_encoder_codec()).collect());
    }
    // No probed snapshot — the compile-time answer is the only one available,
    // and it is the question `select_video_backend` genuinely answers.
    crate::engine::input_test_pattern::select_video_backend()
        .map(|backend| vec![backend])
        .ok_or_else(no_encoder)
}

/// The wire codec family a chain will produce.
///
/// Safe to read off the head: the chain is family-pure, so a fall-through at
/// open time cannot change the answer. The PMT depends on this.
#[cfg(feature = "media-codecs")]
fn chain_is_hevc(chain: &[video_codec::VideoEncoderCodec]) -> bool {
    // `family()` rather than a local `matches!` over the HEVC variants: that
    // duplicate would compile happily after a new backend is added upstream and
    // put stream_type 0x1B on an HEVC elementary stream, which nothing
    // downstream of the mux parses the ES to notice.
    chain.first().map(|codec| codec.family()) == Some(video_codec::VideoCodec::Hevc)
}

/// Why one canvas frame did not reach the muxer.
///
/// Typed rather than a bare string because the caller has to tell **the encoder
/// never opened** — a dead wall — from an open encoder refusing one frame, which
/// costs a frame. `ScaledVideoEncoder::is_open()` makes that call; this carries
/// the reason so whichever way it goes, it is named.
#[cfg(feature = "media-codecs")]
#[derive(Debug)]
enum EncodeStep {
    Scale(video_codec::VideoError),
    MissingPlane(&'static str),
    Encode(String),
}

#[cfg(feature = "media-codecs")]
impl std::fmt::Display for EncodeStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeStep::Scale(e) => write!(f, "canvas colour conversion failed: {e}"),
            EncodeStep::MissingPlane(p) => write!(f, "canvas frame missing {p} plane"),
            EncodeStep::Encode(e) => write!(f, "{e}"),
        }
    }
}

/// The `video_encode` config a wall's canvas is encoded with.
///
/// Lifted out of [`build_encoder`] so the **resource-cost model resolves the
/// same backend the compositor will actually open**. `derive_cost_plan` has no
/// other way in — a mosaic carries no `video_encode` block of its own — and
/// without this it falls through to `HwEncoderFamily::classify`, a substring
/// match that returns `None` for `h264_auto` and bills every wall as a software
/// encode. The manager emits `h264_auto` for every wall it deploys and offers no
/// dropdown, so that is *every* wall.
///
/// Harmless while the canvas always ran x264 — software billing was accidentally
/// right. Once the canvas started resolving hardware (#129) it became an
/// undercount that hides a real QSV/NVENC session from the
/// `hw_encoder_oversubscribed` watchdog and from the manager's Resources card.
pub(crate) fn mosaic_video_encode_config(
    config: &MosaicInputConfig,
) -> crate::config::models::VideoEncodeConfig {
    use crate::config::models::VideoEncodeConfig;
    let fps = u32::from(config.fps.max(1));
    VideoEncodeConfig {
        // **The operator's codec, carried through.** This used to be
        // overwritten with `backend_codec_string(select_video_backend())`, so
        // `config.codec` reached the encoder only as text in an error message:
        // a wall asked for HEVC was given H.264, silently (#129).
        codec: config.codec.clone(),
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
    }
}

/// Resolve and open the canvas encoder.
///
/// A default build has **no video encoder at all** — every backend resolves to
/// `FeatureDisabled` — so this is where that becomes a named refusal naming the
/// rebuild, rather than an obscure failure at the first frame.
///
/// Returns the resolved chain alongside the encoder: the caller settles the
/// PMT's `stream_type` from it, and asking a second, separate question there is
/// how a wall could encode HEVC and be announced as H.264.
#[cfg(feature = "media-codecs")]
fn build_encoder(
    config: &MosaicInputConfig,
    flow_id: &str,
) -> Result<(crate::engine::video_encode_util::ScaledVideoEncoder, Vec<video_codec::VideoEncoderCodec>), String> {
    let fps = u32::from(config.fps.max(1));
    let video_cfg = mosaic_video_encode_config(config);

    let chain = canvas_backend_chain(
        &video_cfg,
        flow_id,
        crate::engine::hardware_probe::static_capabilities().as_deref(),
    )?;
    let mut encoder = crate::engine::video_encode_util::ScaledVideoEncoder::with_backend_chain(
        video_cfg,
        chain.clone(),
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
    Ok((encoder, chain))
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
        let (encoder, _chain) = build_encoder(&cfg(vec![tile("a", 0, 0, 64, 64, Some("in-a"))]), "test")
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
    // ───────────────────── head advertisement ─────────────────────

    /// The advertisement serialises to exactly the field names the manager's
    /// `HeadAdvertisement` deserialises.
    ///
    /// This is a **cross-repo contract test with only one end in this repo**,
    /// which is the most it can be: the two sides are separate crates in
    /// separate git repositories, so no compiler checks that
    /// `bilbycast-manager`'s `HeadAdvertisement` still names these fields.
    /// Every field there is `#[serde(default)]`, so a rename on either side
    /// does not fail — the value silently stops arriving while `last_seen_at`
    /// keeps advancing, which reads as a healthy head with stale capabilities.
    /// Pinning the wire keys here at least makes the edge half deliberate.
    #[test]
    fn the_advertisement_uses_the_wire_names_the_manager_reads() {
        let head = HeadAdvertisement {
            head_id: STREAM_HEAD_ID.to_string(),
            kind: "stream",
            connector: None,
            max_canvas_width: 1920,
            max_canvas_height: 1080,
            capabilities: serde_json::json!({ "encoder_backends": ["libx264"] }),
        };
        let v = serde_json::to_value(&head).expect("serialise");
        let obj = v.as_object().expect("an object");

        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "capabilities",
                "connector",
                "head_id",
                "kind",
                "max_canvas_height",
                "max_canvas_width",
            ],
            "these are mv_heads' column names; a rename stops that column \
             refreshing without failing anything"
        );

        // `kind` is checked by a CHECK constraint on the manager side, so an
        // unrecognised value is a failed INSERT rather than a soft default.
        assert_eq!(obj["kind"], "stream");
        // A stream head occupies no physical port; the column is documented
        // NULL for exactly this kind.
        assert!(obj["connector"].is_null());
    }

    /// A node with no encoder backend advertises no head at all.
    ///
    /// The compositor cannot reach an output without one — the flow bus carries
    /// MPEG-TS — so a head here would appear in the operator's picker and fail
    /// at flow start. The condition is the same one gating the `mv-compositor`
    /// The canvas honours the operator's codec, and the PMT follows it.
    ///
    /// Both halves of #129. `build_encoder` used to overwrite `config.codec`
    /// with `backend_codec_string(select_video_backend())` — always `x264` on a
    /// shipped build — so a wall asked for HEVC silently got H.264, and the
    /// muxer asked `select_video_backend()` a second time rather than reading
    /// what had just been resolved.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn the_pmt_family_read_matches_the_encoders_own() {
        // Deliberately needs no encoder feature and no probe: CI runs
        // `--features multiviewer` over the default set, which compiles in no
        // `video-encoder-*` at all, so anything gated on a backend existing
        // asserts nothing on the only machine that runs this automatically.
        //
        // Checked against `VideoEncoderCodec::family()` rather than a literal
        // table, because `chain_is_hevc` hand-rolls what upstream already
        // decides. A backend added to `auto_priority_chain` and missed in that
        // `matches!` would put stream_type 0x1B on an HEVC elementary stream,
        // with nothing downstream parsing the ES to notice.
        use video_codec::VideoEncoderCodec as C;
        for backend in [
            C::X264, C::X265,
            C::H264Nvenc, C::HevcNvenc,
            C::H264Qsv, C::HevcQsv,
            C::H264Vaapi, C::HevcVaapi,
            C::H264Rkmpp, C::HevcRkmpp,
        ] {
            assert_eq!(
                chain_is_hevc(&[backend]),
                backend.family() == video_codec::VideoCodec::Hevc,
                "chain_is_hevc disagrees with the codec's own family for {backend:?}",
            );
        }
        assert!(!chain_is_hevc(&[]), "no chain is not an HEVC chain");
    }

    /// The resolver picks the host's hardware, on hosts this runner is not.
    ///
    /// `MULTIVIEWER_PLAN.md` §6 requires exactly this — "that resolution must
    /// itself be unit-tested against synthetic `StaticCapabilities` so the
    /// rk3588 case is covered on an x86 runner". Possible because
    /// `canvas_backend_chain` takes its caps as an argument instead of reading
    /// the set-once global.
    ///
    /// **A backend needs both halves**: `host_supports_encoder` intersects the
    /// compiled-in Cargo feature with the probed host capability, so synthetic
    /// caps alone cannot conjure RKMPP into a binary that lacks the feature —
    /// and *that* is the honest assertion. Each host below is checked against
    /// its `cfg!`: where the feature is in, the hardware must lead; where it is
    /// not, the wall must still resolve and fall to the CPU tail rather than
    /// refusing. Writing it as a bare "rk3588 leads with its VPU" made the test
    /// pass only on a Rockchip build and fail everywhere else.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn the_canvas_resolves_the_hosts_hardware_not_the_binarys_first_arm() {
        use crate::engine::hardware_probe::{HwCodecCapability, tests::make_caps};
        use video_codec::VideoEncoderCodec as C;

        let chain_on = |codec: &str, caps: &_| {
            let mut c = cfg(vec![tile("a", 0, 0, 64, 64, Some("in-a"))]);
            c.codec = codec.to_string();
            canvas_backend_chain(&mosaic_video_encode_config(&c), "test", Some(caps))
        };

        // (host capability, the feature that must also be compiled in, the
        // backend that should then lead) — the whole point of #129 is that
        // `select_video_backend` could never reach the last two at all.
        let hosts: [(HwCodecCapability, bool, C); 4] = [
            (HwCodecCapability { h264_rkmpp: true, ..Default::default() },
             cfg!(feature = "video-encoder-rkmpp"), C::H264Rkmpp),
            (HwCodecCapability { h264_nvenc: true, ..Default::default() },
             cfg!(feature = "video-encoder-nvenc"), C::H264Nvenc),
            (HwCodecCapability { h264_qsv: true, ..Default::default() },
             cfg!(feature = "video-encoder-qsv"), C::H264Qsv),
            (HwCodecCapability { h264_vaapi: true, ..Default::default() },
             cfg!(feature = "video-encoder-vaapi"), C::H264Vaapi),
        ];

        for (caps, compiled_in, expected) in hosts {
            let caps = make_caps(caps);
            let Ok(chain) = chain_on("h264_auto", &caps) else {
                // No encoder in this build at all — nothing to assert about
                // which one leads. `any_video_encoder_compiled` covers that.
                continue;
            };
            assert!(!chain.is_empty(), "a resolved chain is never empty");
            if compiled_in {
                assert_eq!(
                    chain.first(),
                    Some(&expected),
                    "{expected:?} is compiled in and the host has it, so it must lead: {chain:?}",
                );
            } else {
                assert_ne!(
                    chain.first(),
                    Some(&expected),
                    "{expected:?} is not compiled into this build and must not be selected",
                );
            }
            // Whatever leads, an H.264 request stays an H.264 chain — the PMT
            // is settled from its head before the encoder opens.
            assert!(
                chain.iter().all(|b| b.family() != video_codec::VideoCodec::Hevc),
                "an H.264 request resolved a mixed-family chain: {chain:?}",
            );
        }

        // A host with no hardware at all still gets a wall: the CPU tail is the
        // floor, not a failure.
        let bare = make_caps(HwCodecCapability::default());
        if let Ok(chain) = chain_on("h264_auto", &bare) {
            assert!(!chain.is_empty(), "a bare host still resolves the CPU tail: {chain:?}");
            assert!(chain.iter().all(|b| b.family() != video_codec::VideoCodec::Hevc));
        }
    }
    /// A resolved chain never mixes families.
    ///
    /// Load-bearing rather than incidental: the PMT stream type is settled from
    /// the chain's head before the encoder opens, so a chain that could demote
    /// from HEVC to H.264 at `avcodec_open2` would put the wrong `stream_type`
    /// on the wire with nothing downstream parsing the ES to notice.
    #[cfg(feature = "media-codecs")]
    #[test]
    fn a_resolved_chain_is_family_pure() {
        if crate::engine::hardware_probe::static_capabilities().is_none() {
            return;
        }
        for (codec, want_hevc) in [("h264_auto", false), ("hevc_auto", true)] {
            let mut config = cfg(vec![tile("a", 0, 0, 64, 64, Some("in-a"))]);
            config.codec = codec.to_string();
            let Ok((_, chain)) = build_encoder(&config, "test") else {
                continue;
            };
            for backend in &chain {
                assert_eq!(
                    chain_is_hevc(std::slice::from_ref(backend)),
                    want_hevc,
                    "{codec} resolved a mixed-family chain: {chain:?}",
                );
            }
        }
    }

    /// A node with no encoder backend advertises no head at all.
    ///
    /// The manager must not offer a wall on a node that would refuse it at flow
    /// start. The condition is the same one gating the `mv-compositor`
    /// capability bit, and the two must not drift apart.
    #[test]
    fn a_head_is_advertised_only_when_an_encoder_backend_resolves() {
        let heads = advertised_heads();
        let has_backend = crate::engine::input_test_pattern::select_video_backend().is_some();
        assert_eq!(
            heads.is_empty(),
            !has_backend,
            "advertised_heads() must agree with select_video_backend(), which \
             is what gates the mv-compositor capability"
        );
        if has_backend {
            assert_eq!(heads.len(), 1, "phase 1 is one wall, one head, one node");
            assert_eq!(heads[0].head_id, STREAM_HEAD_ID);
            assert_eq!(heads[0].max_canvas_width, mosaic::MAX_CANVAS_W);
            assert_eq!(heads[0].max_canvas_height, mosaic::MAX_CANVAS_H);
        }
    }
}
