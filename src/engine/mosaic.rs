// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! Mosaic compositor geometry — the multiviewer canvas, its tiles, and what a
//! tile shows when its source is not there (edge #107, phase 1).
//!
//! This module is deliberately **pure arithmetic and state**. It owns where
//! each tile goes, how a source of the wrong shape is fitted into it, and when
//! a tile stops showing pictures and starts showing a badge. It decodes
//! nothing, allocates no FFmpeg objects and touches no I/O, which is what lets
//! every rule here be tested without hardware, a feature flag or a live source.
//!
//! # Facts this is built on, measured rather than assumed
//!
//! The design document behind this feature asserted things about the
//! compositing primitive that turned out to be wrong. They were checked against
//! the code and with a real test
//! (`bilbycast-ffmpeg-video-rs/video-engine/tests/canvas_subrect_blit.rs`)
//! before a line of this was written:
//!
//! * **The canvas is packed BGRA8, not YUV.** `scale_raw_planes_into_packed`
//!   refuses every planar destination format, so a canvas pixel is 4 bytes. The
//!   design assumed YUV420 at 1.5 bytes, so a canvas costs **2.7x** what it
//!   budgeted, and a stream head must convert BGRA to YUV before it can encode.
//! * **Tile rects need no alignment.** BGRA has no chroma sub-sampling, so an
//!   odd x, y, width or height is exactly representable. Had the canvas been
//!   YUV420, every rect would have had to be even — a real constraint on a
//!   layout editor that does not exist.
//! * **A tile is blitted by handing the scaler a canvas-pitched sub-slice** at
//!   the tile's byte offset. That works, and touches nothing outside the rect.
//!   It did **not** work for the bottom row of tiles until the over-strict
//!   bounds check upstream was corrected — it demanded `pitch * height` when
//!   the true requirement is `(h-1)*pitch + w*4`, so it refused every tile whose
//!   last row was the canvas's last row. That is fixed; [`Canvas::byte_offset`]
//!   documents the arithmetic that relies on it.
//!
//! # What is deliberately not here
//!
//! Encoding. A composite reaching an SRT/RTP/UDP output means a full H.264 or
//! HEVC **encode plus TS mux**, because the flow bus carries MPEG-TS rather
//! than frames — the design's claim that outputs "come free" is only true of
//! the fan-out, not of getting onto the bus. A default `cargo build` has **no
//! video encoder at all**, so the stream head is necessarily gated behind the
//! same `video-encoder-*` features transcoding already uses. Keeping the
//! geometry here, unfeatured and always compiled, means the wall's layout rules
//! stay testable on any machine.

// Phase-1 geometry, complete and unit-tested, not yet wired to a producer.
//
// `bilbycast-edge` is a binary crate, so `pub` does not escape it and every
// item here reads as dead until the ingest and encode halves land. The same
// treatment `input_media_player::frame_rate` carries for the same reason.
//
// This is scaffolding, but it is not *unverified* scaffolding: the 24 tests
// below exercise every rule in the file, and two of them encode arithmetic that
// already corrected a real defect in the scaler upstream. The allow goes when
// the compositor input type calls into it.
#![allow(dead_code)]

use std::time::{Duration, Instant};

/// A tile's position and size on the canvas, in pixels.
///
/// No alignment requirement — see the module note on why packed BGRA makes odd
/// coordinates legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl TileRect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// Whether this rect lies entirely within a canvas of `width` x `height`.
    pub fn fits_within(&self, width: u32, height: u32) -> bool {
        self.x.saturating_add(self.width) <= width
            && self.y.saturating_add(self.height) <= height
    }

    /// Whether two rects share any pixel.
    ///
    /// Overlap is **legal** — z-order exists precisely so a PiP or a banner can
    /// sit over another tile — so this is reported, never refused. It is used
    /// to tell an operator what they have drawn, and to decide paint order.
    pub fn overlaps(&self, other: &TileRect) -> bool {
        let ax2 = self.x.saturating_add(self.width);
        let ay2 = self.y.saturating_add(self.height);
        let bx2 = other.x.saturating_add(other.width);
        let by2 = other.y.saturating_add(other.height);
        self.x < bx2 && other.x < ax2 && self.y < by2 && other.y < ay2
    }
}

/// How a source whose aspect ratio differs from its tile is fitted.
///
/// Phase 1 implements **letterbox only**, and implements it properly. The
/// design document is explicit that none of the three policies exists anywhere
/// in the tree today, so there is nothing to be consistent with and no reason
/// to ship two half-done ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AspectPolicy {
    /// Preserve the source aspect, centre it, and leave bars.
    #[default]
    Letterbox,
}

/// Where a source actually lands inside its tile once aspect is honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FittedRect {
    /// The picture rect, in canvas coordinates.
    pub rect: TileRect,
    /// Bar thickness in pixels: (left, top). Right/bottom are whatever remains,
    /// which may differ by one pixel when the leftover is odd.
    pub bars: (u32, u32),
}

/// Fit a `src_w x src_h` source into `tile` under `policy`.
///
/// Returns the centred picture rect. An odd leftover puts the extra pixel on
/// the right/bottom — stated rather than left to rounding, because a tile that
/// jitters by a pixel between frames is visible on a wall.
pub fn fit(src_w: u32, src_h: u32, tile: TileRect, policy: AspectPolicy) -> FittedRect {
    let AspectPolicy::Letterbox = policy;
    if src_w == 0 || src_h == 0 || tile.width == 0 || tile.height == 0 {
        return FittedRect { rect: TileRect::new(tile.x, tile.y, 0, 0), bars: (0, 0) };
    }

    // Compare aspect ratios with integer cross-multiplication rather than
    // floats: a float comparison makes a 16:9 source in a 16:9 tile sometimes
    // produce a one-pixel bar, which reads as a rendering fault.
    let src_wide = u64::from(src_w) * u64::from(tile.height);
    let tile_wide = u64::from(tile.width) * u64::from(src_h);

    let (w, h) = if src_wide == tile_wide {
        (tile.width, tile.height)
    } else if src_wide > tile_wide {
        // Source is wider: full tile width, shorter height, bars top and bottom.
        let h = (u64::from(tile.width) * u64::from(src_h) / u64::from(src_w)) as u32;
        (tile.width, h.max(1).min(tile.height))
    } else {
        // Source is taller: full tile height, narrower width, bars left/right.
        let w = (u64::from(tile.height) * u64::from(src_w) / u64::from(src_h)) as u32;
        (w.max(1).min(tile.width), tile.height)
    };

    let bar_x = (tile.width - w) / 2;
    let bar_y = (tile.height - h) / 2;
    FittedRect {
        rect: TileRect::new(tile.x + bar_x, tile.y + bar_y, w, h),
        bars: (bar_x, bar_y),
    }
}

/// The canvas a wall is composited into.
///
/// Packed BGRA8. `pitch` is bytes per row and is `width * 4` unless a caller
/// has a reason to over-allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

impl Canvas {
    /// Bytes per pixel of the packed destination the scaler requires.
    pub const BYTES_PER_PIXEL: usize = 4;

    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub fn pitch(&self) -> usize {
        self.width as usize * Self::BYTES_PER_PIXEL
    }

    /// Total buffer size for one canvas frame.
    pub fn buffer_len(&self) -> usize {
        self.pitch() * self.height as usize
    }

    /// The byte offset of a rect's top-left pixel.
    ///
    /// This is the whole trick: a tile is blitted by passing the scaler
    /// `&mut canvas[byte_offset(rect)..]` with `dst_pitch = canvas.pitch()`,
    /// and libswscale writes `rect.width` pixels per row stepping the canvas
    /// pitch — a rect-confined write with no intermediate buffer and no copy.
    ///
    /// This requires the upstream bounds check to demand
    /// `(h-1)*pitch + w*4` rather than `pitch*h`; with the old check every
    /// bottom-row tile was refused. Guarded by
    /// `a_bottom_row_tile_fits_an_exactly_sized_canvas` in the video-engine
    /// tests, which fails if that regresses.
    pub fn byte_offset(&self, rect: &TileRect) -> usize {
        rect.y as usize * self.pitch() + rect.x as usize * Self::BYTES_PER_PIXEL
    }

    /// The bytes the scaler will require in the slice starting at this rect's
    /// offset, i.e. the corrected requirement.
    pub fn required_tail(&self, rect: &TileRect) -> usize {
        match (rect.height as usize).checked_sub(1) {
            None | Some(0) if rect.height == 0 => 0,
            None => 0,
            Some(full_rows) => {
                full_rows * self.pitch() + rect.width as usize * Self::BYTES_PER_PIXEL
            }
        }
    }
}

/// What a tile is showing.
///
/// Ordered deliberately: a wall's health is the worst state on it, so this
/// derives `Ord` and the worst state is the greatest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TileState {
    /// Pictures are arriving and being painted.
    Live,
    /// The tile has a source assigned, but nothing has arrived recently.
    NoSignal,
    /// No source is assigned to this tile at all.
    Unassigned,
}

impl TileState {
    /// The badge text drawn into the tile, or `None` while live.
    ///
    /// Two states, not one. "Nobody has routed anything here" and "what was
    /// routed here has stopped" are different operator problems with different
    /// responses, and a wall that shows the same black rectangle for both makes
    /// an operator check the wrong thing first.
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Self::Live => None,
            Self::NoSignal => Some("NO SIGNAL"),
            Self::Unassigned => Some("UNASSIGNED"),
        }
    }
}

/// How long a tile keeps painting its last picture before it is declared dead.
///
/// Two seconds. Long enough to ride out a GOP boundary, a bonded-path
/// reordering window or a decoder hiccup without flickering a badge across a
/// wall; short enough that an operator watching a failure sees it within a
/// breath.
pub const NO_SIGNAL_AFTER: Duration = Duration::from_secs(2);

/// Per-tile liveness, driven by frame arrivals.
///
/// **A stale frame is never presented as live.** The design fixes this as a
/// locked ruling and it is the reason this is a timer rather than a flag: a
/// source that stops delivering leaves its last frame in the canvas, and
/// without a timer that frozen picture is indistinguishable from a working one.
/// The badge is what makes the difference visible.
#[derive(Debug, Clone)]
pub struct TileLiveness {
    assigned: bool,
    last_frame: Option<Instant>,
    no_signal_after: Duration,
}

impl TileLiveness {
    pub fn unassigned() -> Self {
        Self { assigned: false, last_frame: None, no_signal_after: NO_SIGNAL_AFTER }
    }

    pub fn assigned() -> Self {
        Self { assigned: true, last_frame: None, no_signal_after: NO_SIGNAL_AFTER }
    }

    /// Override the staleness window — used by tests and, later, by config.
    pub fn with_window(mut self, window: Duration) -> Self {
        self.no_signal_after = window;
        self
    }

    /// Assign or clear this tile's source.
    ///
    /// Clearing also forgets the last frame time: an unassigned tile that later
    /// gains a source must not inherit liveness from the previous one, which
    /// would show a fresh route as live before a single frame had arrived.
    pub fn set_assigned(&mut self, assigned: bool) {
        if self.assigned != assigned {
            self.last_frame = None;
        }
        self.assigned = assigned;
    }

    /// Record that a frame was painted into this tile.
    pub fn frame_arrived(&mut self, at: Instant) {
        if self.assigned {
            self.last_frame = Some(at);
        }
    }

    /// The tile's state as of `now`.
    pub fn state_at(&self, now: Instant) -> TileState {
        if !self.assigned {
            return TileState::Unassigned;
        }
        match self.last_frame {
            None => TileState::NoSignal,
            Some(seen) => {
                // `saturating_duration_since` because a monotonic clock can
                // still hand back an earlier instant across some suspend paths,
                // and a negative elapsed would wrap into "very stale" and blink
                // the whole wall.
                if now.saturating_duration_since(seen) >= self.no_signal_after {
                    TileState::NoSignal
                } else {
                    TileState::Live
                }
            }
        }
    }
}

/// One tile of a wall: where it is, what feeds it, and how it is doing.
#[derive(Debug, Clone)]
pub struct Tile {
    /// Stable identity minted by the layout, never a renameable label.
    ///
    /// Routing keys on this, so renaming a tile in the editor cannot silently
    /// re-point a signal — the house precedent from the visual editor's layout
    /// table.
    pub id: String,
    pub rect: TileRect,
    /// Paint order. Higher is painted later, therefore on top.
    pub z: i32,
    /// The node-local input id feeding this tile, if any.
    pub source_input_id: Option<String>,
    pub liveness: TileLiveness,
}

/// A complete wall: a canvas and its tiles.
#[derive(Debug, Clone)]
pub struct MosaicLayout {
    pub canvas: Canvas,
    pub tiles: Vec<Tile>,
}

/// Why a layout cannot be composited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    NoTiles,
    DuplicateTileId(String),
    TileOutsideCanvas { id: String },
    ZeroSizedTile { id: String },
    CanvasTooLarge { width: u32, height: u32 },
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTiles => write!(f, "a wall needs at least one tile"),
            Self::DuplicateTileId(id) => {
                write!(f, "tile id '{id}' is used more than once; routing keys on it")
            }
            Self::TileOutsideCanvas { id } => {
                write!(f, "tile '{id}' does not fit inside the canvas")
            }
            Self::ZeroSizedTile { id } => write!(f, "tile '{id}' has zero width or height"),
            Self::CanvasTooLarge { width, height } => write!(
                f,
                "a {width}x{height} canvas exceeds what a CPU blit sustains; \
                 the measured ceiling is 1920x1080"
            ),
        }
    }
}

/// The canvas ceiling for phase 1.
///
/// Mirrors `SW_BLIT_MAX_W/H` in the display output, which exists because a 4K
/// libswscale convert into a write-combining dumb buffer measured ~7 s/frame. A
/// stream head composites into ordinary cached sysmem and is not bound by that
/// number — but nobody has measured the stream-head shape, and shipping an
/// unmeasured UHD path is how the display output earned its own ceiling.
/// Raising this is gated on that measurement, not on an argument.
pub const MAX_CANVAS_W: u32 = 1920;
pub const MAX_CANVAS_H: u32 = 1080;

impl MosaicLayout {
    /// Check a layout is composable, before anything is allocated.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.tiles.is_empty() {
            return Err(LayoutError::NoTiles);
        }
        if self.canvas.width > MAX_CANVAS_W || self.canvas.height > MAX_CANVAS_H {
            return Err(LayoutError::CanvasTooLarge {
                width: self.canvas.width,
                height: self.canvas.height,
            });
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.tiles.len());
        for tile in &self.tiles {
            if tile.rect.width == 0 || tile.rect.height == 0 {
                return Err(LayoutError::ZeroSizedTile { id: tile.id.clone() });
            }
            if !tile.rect.fits_within(self.canvas.width, self.canvas.height) {
                return Err(LayoutError::TileOutsideCanvas { id: tile.id.clone() });
            }
            if seen.contains(&tile.id.as_str()) {
                return Err(LayoutError::DuplicateTileId(tile.id.clone()));
            }
            seen.push(&tile.id);
        }
        Ok(())
    }

    /// Tile indices in paint order: lowest z first, ties by declaration order.
    ///
    /// A stable sort on declaration order matters — an unstable one would let
    /// two tiles at the same z swap places between frames, which on a wall
    /// looks like flicker and has no cause an operator could find.
    pub fn paint_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.tiles.len()).collect();
        order.sort_by_key(|&i| self.tiles[i].z);
        order
    }

    /// Every pair of tile ids that overlap, in paint order.
    ///
    /// Reported for the operator, never refused: overlap is how a PiP is built.
    pub fn overlapping_pairs(&self) -> Vec<(String, String)> {
        let order = self.paint_order();
        let mut pairs = Vec::new();
        for (a_pos, &a) in order.iter().enumerate() {
            for &b in order.iter().skip(a_pos + 1) {
                if self.tiles[a].rect.overlaps(&self.tiles[b].rect) {
                    pairs.push((self.tiles[a].id.clone(), self.tiles[b].id.clone()));
                }
            }
        }
        pairs
    }

    /// The wall's overall state: the worst state on it.
    pub fn worst_state_at(&self, now: Instant) -> TileState {
        self.tiles
            .iter()
            .map(|t| t.liveness.state_at(now))
            .max()
            .unwrap_or(TileState::Unassigned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(id: &str, x: u32, y: u32, w: u32, h: u32, z: i32) -> Tile {
        Tile {
            id: id.into(),
            rect: TileRect::new(x, y, w, h),
            z,
            source_input_id: Some(format!("in-{id}")),
            liveness: TileLiveness::assigned(),
        }
    }

    fn wall_2x2() -> MosaicLayout {
        MosaicLayout {
            canvas: Canvas::new(1920, 1080),
            tiles: vec![
                tile("a", 0, 0, 960, 540, 0),
                tile("b", 960, 0, 960, 540, 0),
                tile("c", 0, 540, 960, 540, 0),
                tile("d", 960, 540, 960, 540, 0),
            ],
        }
    }

    // ── geometry ────────────────────────────────────────────────────────────

    #[test]
    fn the_canvas_is_four_bytes_per_pixel_because_the_scaler_demands_packed() {
        let canvas = Canvas::new(1920, 1080);
        assert_eq!(canvas.pitch(), 1920 * 4);
        assert_eq!(canvas.buffer_len(), 1920 * 1080 * 4);
        // 2.7x what a YUV420 canvas would have cost — the number the design
        // budgeted with.
        assert_eq!(canvas.buffer_len(), 8_294_400);
    }

    /// The exact case the corrected upstream bounds check unblocked.
    #[test]
    fn the_bottom_right_tile_needs_less_than_a_whole_pitch_of_tail() {
        let canvas = Canvas::new(1920, 1080);
        let bottom_right = TileRect::new(960, 540, 960, 540);
        let offset = canvas.byte_offset(&bottom_right);
        let tail = canvas.buffer_len() - offset;
        let required = canvas.required_tail(&bottom_right);

        assert!(
            tail >= required,
            "the corrected requirement ({required}) must fit the tail ({tail})"
        );
        // And the old requirement did not fit — by exactly x0 * 4 bytes, which
        // is the defect this arithmetic exists to record.
        let old_requirement = canvas.pitch() * bottom_right.height as usize;
        assert!(old_requirement > tail, "premise: the old check over-demanded");
        assert_eq!(
            old_requirement - tail,
            bottom_right.x as usize * 4,
            "the shortfall is exactly the tile's x origin in bytes"
        );
    }

    #[test]
    fn a_tile_rect_needs_no_even_alignment() {
        // Legal because the canvas is packed BGRA. Under YUV420 none of these
        // would be representable.
        let odd = TileRect::new(7, 3, 15, 9);
        assert!(odd.fits_within(1920, 1080));
        let canvas = Canvas::new(64, 40);
        assert_eq!(canvas.byte_offset(&odd), 3 * 64 * 4 + 7 * 4);
    }

    // ── letterbox ───────────────────────────────────────────────────────────

    #[test]
    fn a_matching_aspect_fills_the_tile_with_no_bars() {
        // 16:9 into 16:9 must be exact — a one-pixel bar here reads as a fault.
        let fitted = fit(1920, 1080, TileRect::new(0, 0, 960, 540), AspectPolicy::Letterbox);
        assert_eq!(fitted.rect, TileRect::new(0, 0, 960, 540));
        assert_eq!(fitted.bars, (0, 0));

        // And for a tile that is not a neat divisor of the source.
        let fitted = fit(1280, 720, TileRect::new(10, 20, 640, 360), AspectPolicy::Letterbox);
        assert_eq!(fitted.rect, TileRect::new(10, 20, 640, 360));
        assert_eq!(fitted.bars, (0, 0));
    }

    #[test]
    fn a_wider_source_gets_bars_top_and_bottom() {
        // 2:1 source into a 16:9 tile.
        let fitted = fit(1000, 500, TileRect::new(0, 0, 800, 450), AspectPolicy::Letterbox);
        assert_eq!(fitted.rect.width, 800, "full tile width");
        assert_eq!(fitted.rect.height, 400, "800 * 500 / 1000");
        assert_eq!(fitted.bars, (0, 25));
        assert_eq!(fitted.rect.y, 25, "centred vertically");
    }

    #[test]
    fn a_taller_source_gets_bars_left_and_right() {
        // 4:3 source into a 16:9 tile.
        let fitted = fit(640, 480, TileRect::new(0, 0, 960, 540), AspectPolicy::Letterbox);
        assert_eq!(fitted.rect.height, 540, "full tile height");
        assert_eq!(fitted.rect.width, 720, "540 * 640 / 480");
        assert_eq!(fitted.bars, (120, 0));
        assert_eq!(fitted.rect.x, 120, "centred horizontally");
    }

    #[test]
    fn an_odd_leftover_does_not_jitter() {
        // 3 pixels of bar cannot be split evenly; the rule must be fixed, not
        // rounded, or a tile moves by a pixel between frames.
        let fitted = fit(100, 100, TileRect::new(0, 0, 103, 100), AspectPolicy::Letterbox);
        assert_eq!(fitted.rect.width, 100);
        assert_eq!(fitted.bars.0, 1, "floor: the extra pixel goes right");
        assert_eq!(fitted.rect.x, 1);
        // Deterministic across repeated calls.
        let again = fit(100, 100, TileRect::new(0, 0, 103, 100), AspectPolicy::Letterbox);
        assert_eq!(fitted.rect, again.rect);
    }

    #[test]
    fn a_degenerate_source_or_tile_produces_an_empty_rect_rather_than_panicking() {
        for (sw, sh) in [(0, 100), (100, 0), (0, 0)] {
            let fitted = fit(sw, sh, TileRect::new(5, 6, 100, 100), AspectPolicy::Letterbox);
            assert_eq!(fitted.rect.width, 0);
            assert_eq!(fitted.rect.height, 0);
        }
        let fitted = fit(100, 100, TileRect::new(0, 0, 0, 50), AspectPolicy::Letterbox);
        assert_eq!(fitted.rect.width, 0);
    }

    #[test]
    fn a_fitted_rect_never_escapes_its_tile() {
        // Property-ish sweep over awkward shapes.
        for (sw, sh) in [(1920, 1080), (720, 576), (1, 1000), (1000, 1), (101, 97)] {
            for (tw, th) in [(960, 540), (317, 211), (1, 1), (640, 360)] {
                let t = TileRect::new(13, 17, tw, th);
                let f = fit(sw, sh, t, AspectPolicy::Letterbox);
                assert!(
                    f.rect.x >= t.x && f.rect.y >= t.y,
                    "{sw}x{sh} into {tw}x{th} escaped top-left"
                );
                assert!(
                    f.rect.x + f.rect.width <= t.x + t.width
                        && f.rect.y + f.rect.height <= t.y + t.height,
                    "{sw}x{sh} into {tw}x{th} escaped bottom-right: {f:?}"
                );
            }
        }
    }

    // ── badges and liveness ─────────────────────────────────────────────────

    #[test]
    fn an_unrouted_tile_and_a_dead_one_are_different_badges() {
        assert_eq!(TileState::Unassigned.badge(), Some("UNASSIGNED"));
        assert_eq!(TileState::NoSignal.badge(), Some("NO SIGNAL"));
        assert_eq!(TileState::Live.badge(), None);
    }

    #[test]
    fn a_source_that_stops_stops_being_live() {
        let start = Instant::now();
        let mut live = TileLiveness::assigned().with_window(Duration::from_secs(2));
        assert_eq!(live.state_at(start), TileState::NoSignal, "nothing has arrived yet");

        live.frame_arrived(start);
        assert_eq!(live.state_at(start), TileState::Live);
        assert_eq!(
            live.state_at(start + Duration::from_millis(1999)),
            TileState::Live,
            "inside the window the last picture still counts"
        );
        assert_eq!(
            live.state_at(start + Duration::from_secs(2)),
            TileState::NoSignal,
            "a frozen frame must not be presented as live"
        );
    }

    #[test]
    fn a_freshly_routed_tile_does_not_inherit_the_previous_sources_liveness() {
        // The defect this prevents: re-routing a tile from a working source to
        // a dead one would show the new route as Live until the window expired,
        // because the timestamp belonged to the old source.
        let start = Instant::now();
        let mut live = TileLiveness::assigned();
        live.frame_arrived(start);
        assert_eq!(live.state_at(start), TileState::Live);

        live.set_assigned(false);
        assert_eq!(live.state_at(start), TileState::Unassigned);

        live.set_assigned(true);
        assert_eq!(
            live.state_at(start),
            TileState::NoSignal,
            "a new route starts with no signal, not with the old one's"
        );
    }

    #[test]
    fn an_unassigned_tile_ignores_frames() {
        let start = Instant::now();
        let mut live = TileLiveness::unassigned();
        live.frame_arrived(start);
        assert_eq!(live.state_at(start), TileState::Unassigned);
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_blink_the_wall() {
        let start = Instant::now() + Duration::from_secs(10);
        let mut live = TileLiveness::assigned();
        live.frame_arrived(start);
        // `now` earlier than the last frame: saturating, so elapsed is zero.
        assert_eq!(live.state_at(start - Duration::from_secs(5)), TileState::Live);
    }

    #[test]
    fn a_walls_state_is_the_worst_tile_on_it() {
        let now = Instant::now();
        let mut wall = wall_2x2();
        for t in &mut wall.tiles {
            t.liveness.frame_arrived(now);
        }
        assert_eq!(wall.worst_state_at(now), TileState::Live);

        wall.tiles[2].liveness = TileLiveness::assigned(); // never delivered
        assert_eq!(wall.worst_state_at(now), TileState::NoSignal);

        wall.tiles[3].liveness = TileLiveness::unassigned();
        assert_eq!(
            wall.worst_state_at(now),
            TileState::Unassigned,
            "unassigned is the worst, so it wins the rollup"
        );
    }

    // ── layout validation ───────────────────────────────────────────────────

    #[test]
    fn a_2x2_wall_validates_and_tiles_do_not_overlap() {
        let wall = wall_2x2();
        assert_eq!(wall.validate(), Ok(()));
        assert!(wall.overlapping_pairs().is_empty());
    }

    #[test]
    fn a_duplicate_tile_id_is_refused_because_routing_keys_on_it() {
        let mut wall = wall_2x2();
        wall.tiles[1].id = "a".into();
        assert_eq!(wall.validate(), Err(LayoutError::DuplicateTileId("a".into())));
    }

    #[test]
    fn a_tile_that_does_not_fit_is_refused() {
        let mut wall = wall_2x2();
        wall.tiles[3].rect = TileRect::new(1000, 540, 960, 540); // runs off the right
        assert_eq!(
            wall.validate(),
            Err(LayoutError::TileOutsideCanvas { id: "d".into() })
        );
    }

    #[test]
    fn a_zero_sized_tile_is_refused() {
        let mut wall = wall_2x2();
        wall.tiles[0].rect = TileRect::new(0, 0, 0, 540);
        assert_eq!(wall.validate(), Err(LayoutError::ZeroSizedTile { id: "a".into() }));
    }

    #[test]
    fn an_empty_wall_is_refused() {
        let wall = MosaicLayout { canvas: Canvas::new(1920, 1080), tiles: vec![] };
        assert_eq!(wall.validate(), Err(LayoutError::NoTiles));
    }

    #[test]
    fn a_uhd_canvas_is_refused_until_the_stream_head_shape_is_measured() {
        let wall = MosaicLayout {
            canvas: Canvas::new(3840, 2160),
            tiles: vec![tile("a", 0, 0, 100, 100, 0)],
        };
        assert_eq!(
            wall.validate(),
            Err(LayoutError::CanvasTooLarge { width: 3840, height: 2160 })
        );
    }

    #[test]
    fn overlap_is_reported_not_refused_because_that_is_how_a_pip_is_built() {
        let wall = MosaicLayout {
            canvas: Canvas::new(1920, 1080),
            tiles: vec![
                tile("bg", 0, 0, 1920, 1080, 0),
                tile("pip", 1400, 700, 480, 270, 10),
            ],
        };
        assert_eq!(wall.validate(), Ok(()), "an overlapping layout is legal");
        assert_eq!(
            wall.overlapping_pairs(),
            vec![("bg".to_string(), "pip".to_string())]
        );
    }

    #[test]
    fn paint_order_is_by_z_then_stable_by_declaration() {
        let wall = MosaicLayout {
            canvas: Canvas::new(1920, 1080),
            tiles: vec![
                tile("top", 0, 0, 10, 10, 5),
                tile("first-at-zero", 0, 0, 10, 10, 0),
                tile("second-at-zero", 0, 0, 10, 10, 0),
            ],
        };
        let order = wall.paint_order();
        let ids: Vec<&str> = order.iter().map(|&i| wall.tiles[i].id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["first-at-zero", "second-at-zero", "top"],
            "ties must keep declaration order, or the wall flickers with no cause"
        );
    }

    #[test]
    fn every_tile_of_a_validated_wall_can_actually_be_blitted() {
        // The end-to-end geometric guarantee: for every tile of a valid layout,
        // the tail of the canvas from that tile's offset satisfies the scaler's
        // corrected requirement. If this fails, some tile cannot be painted.
        let wall = wall_2x2();
        assert_eq!(wall.validate(), Ok(()));
        for t in &wall.tiles {
            let offset = wall.canvas.byte_offset(&t.rect);
            let tail = wall.canvas.buffer_len() - offset;
            assert!(
                tail >= wall.canvas.required_tail(&t.rect),
                "tile '{}' cannot be blitted: tail {tail} < required {}",
                t.id,
                wall.canvas.required_tail(&t.rect)
            );
        }
    }
}
