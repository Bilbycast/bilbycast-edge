// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-output PCM audio transcoding stage.
//!
//! Sits between an audio output's broadcast subscriber and its packetize/send
//! loop. Decodes incoming PCM RTP packets to planar f32 samples, applies an
//! optional channel routing matrix (mono→stereo dup, surround downmix, or an
//! arbitrary N×M map sourced from IS-08), runs sample rate conversion via
//! `rubato`, dithers and re-encodes to the target bit depth, and re-emits at
//! the requested packet time and payload type.
//!
//! ## Layering
//!
//! ```text
//! broadcast::Receiver<RtpPacket>
//!         │
//!         ▼
//! ┌──────────────────────┐
//! │  TranscodeStage      │
//! │  ─ PcmDepacketizer   │
//! │  ─ decode_pcm_be     │
//! │  ─ apply matrix      │
//! │  ─ rubato SRC        │
//! │  ─ frame accumulator │
//! │  ─ encode_pcm_be     │
//! │  ─ PcmPacketizer     │
//! └──────────┬───────────┘
//!            ▼
//!  Vec<RtpPacket> ready to send
//! ```
//!
//! The stage is **lock-free on the hot path**: counters are `AtomicU64`,
//! the channel-matrix snapshot is taken from a `tokio::sync::watch::Receiver`
//! via `borrow()` (single atomic load). Slow consumers drop packets via
//! [`TranscodeStats::dropped`] rather than blocking the producer.
//!
//! ## Pure Rust
//!
//! Uses `rubato` for sample rate conversion (pure Rust, no C deps). Bit-depth
//! conversion and TPDF dither are hand-rolled. The PCM wire format helpers are
//! shared with [`crate::engine::st2110::audio`].
//!
//! ## Format change handling
//!
//! If a packet arrives whose declared input format differs from the configured
//! input format (rare in 2110, but possible across format renegotiations), the
//! transcoder logs a warning, calls [`TranscodeStage::reset`], and continues
//! with the new format. The resampler is rebuilt lazily on the next packet.
//!
//! ## What this module does NOT do
//!
//! - It does NOT touch RTP timestamping for SMPTE 2110 PTP correlation. The
//!   re-emitted packets carry timestamps derived from the configured output
//!   sample rate, not from the input packet's PTP-derived timestamp.
//! - It does NOT do any acoustic processing beyond linear gain in the channel
//!   matrix. No EQ, compression, or noise gating.
//! - It does NOT support codecs (Opus, AAC, MP2, AC-3). PCM only.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::{Bytes, BytesMut};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::engine::packet::RtpPacket;
use crate::engine::st2110::audio::{PcmDepacketizer, PcmFormat, PcmPacketizer};

// ── IS-08 global watch sender ───────────────────────────────────────────────

/// Process-wide handle to the IS-08 active channel-map watch sender, set
/// once at startup by `Is08State::load_or_default`. Audio output spawn
/// modules call [`subscribe_global_is08`] to obtain a `watch::Receiver`
/// for live channel-routing updates without threading the Is08State
/// through FlowManager.
static IS08_GLOBAL: OnceLock<watch::Sender<Arc<crate::api::nmos_is08::ChannelMap>>> =
    OnceLock::new();

/// Register the IS-08 channel-map watch sender. First-call wins; subsequent
/// calls are no-ops. Idempotent so test harnesses that re-init `Is08State`
/// don't panic.
pub fn set_global_is08_sender(
    tx: watch::Sender<Arc<crate::api::nmos_is08::ChannelMap>>,
) {
    let _ = IS08_GLOBAL.set(tx);
}

/// Subscribe to the IS-08 channel-map watch. Returns `None` when no IS-08
/// state has been registered (e.g., test contexts that don't bring up
/// `Is08State`). Each subscriber sees every activation immediately on the
/// next packet.
pub fn subscribe_global_is08() -> Option<watch::Receiver<Arc<crate::api::nmos_is08::ChannelMap>>>
{
    IS08_GLOBAL.get().map(|tx| tx.subscribe())
}

// ── Public types ────────────────────────────────────────────────────────────

/// PCM bit depth supported by the transcoder.
///
/// L20 is encoded on the wire as L24 with the bottom 4 bits zeroed
/// (per RFC 3190 §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BitDepth {
    /// 16-bit big-endian signed PCM.
    L16,
    /// 20-bit big-endian signed PCM (carried in 24-bit slots, low 4 bits zero).
    L20,
    /// 24-bit big-endian signed PCM.
    L24,
}

impl BitDepth {
    /// Bytes occupied by one sample on the wire.
    pub fn wire_bytes(self) -> usize {
        match self {
            BitDepth::L16 => 2,
            BitDepth::L20 => 3,
            BitDepth::L24 => 3,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            16 => Some(BitDepth::L16),
            20 => Some(BitDepth::L20),
            24 => Some(BitDepth::L24),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            BitDepth::L16 => 16,
            BitDepth::L20 => 20,
            BitDepth::L24 => 24,
        }
    }

    /// Underlying wire format used by the existing PCM packetizer.
    fn pcm_format(self) -> PcmFormat {
        match self {
            BitDepth::L16 => PcmFormat::L16,
            BitDepth::L20 => PcmFormat::L24, // L20 rides in L24 slots
            BitDepth::L24 => PcmFormat::L24,
        }
    }
}

/// Sample rate conversion quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SrcQuality {
    /// High-quality windowed sinc resampler. Broadcast monitoring quality;
    /// CPU cost is several × `Fast`. Default.
    High,
    /// Lower-quality polynomial-interpolation resampler. Lower CPU + latency,
    /// suitable for talkback / IFB paths where pristine fidelity is not
    /// required.
    Fast,
}

impl Default for SrcQuality {
    fn default() -> Self {
        SrcQuality::High
    }
}

/// Dithering applied when down-converting bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dither {
    /// Triangular probability density function dither (recommended for
    /// PCM down-conversion to break quantization correlation).
    Tpdf,
    /// No dither: truncate. Faster, slightly worse quality. Use only when
    /// downstream is going to dither anyway.
    None,
}

impl Default for Dither {
    fn default() -> Self {
        Dither::Tpdf
    }
}

/// A channel routing matrix.
///
/// `matrix[out_ch] = Vec<(in_ch, gain)>` — the output channel is the sum
/// of the listed input channels each multiplied by `gain`. Gain is linear
/// (1.0 = unity, 0.5 = -6.02 dB, 0.707 ≈ -3.01 dB).
///
/// `matrix.len()` is the number of output channels. Every entry must
/// reference input channels less than the input channel count.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMatrix(pub Vec<Vec<(u8, f32)>>);

impl ChannelMatrix {
    pub fn out_channels(&self) -> usize {
        self.0.len()
    }

    /// Identity routing for `n` channels (each output channel is the same
    /// numbered input channel at unity gain).
    pub fn identity(n: u8) -> Self {
        ChannelMatrix((0..n).map(|i| vec![(i, 1.0_f32)]).collect())
    }

    /// Highest input channel index referenced by any matrix entry.
    pub fn max_input_channel(&self) -> Option<u8> {
        self.0
            .iter()
            .flat_map(|row| row.iter().map(|(ch, _)| *ch))
            .max()
    }

    /// True when the matrix is an identity (row N picks input channel N at
    /// unity gain). Used by fast-path planar transcoders to skip the mul.
    pub fn is_identity(&self) -> bool {
        self.0.iter().enumerate().all(|(row_idx, row)| {
            row.len() == 1 && row[0].0 as usize == row_idx && (row[0].1 - 1.0).abs() < 1e-6
        })
    }
}

// ── MatrixSource: static or IS-08-tracked routing ──────────────────────────

/// Source of the active channel routing matrix for a [`TranscodeStage`].
///
/// `Static` carries a fixed matrix from the operator's `transcode.channel_map`
/// or `transcode.channel_map_preset` config. `Is08Tracked` watches the IS-08
/// active map and derives the per-output matrix on demand, caching the result
/// until the next IS-08 activation.
///
/// The cache is invalidated by `watch::Receiver::has_changed()`, which is a
/// single atomic load on the hot path. Map activations are rare (operator
/// action), so the per-packet steady state cost is one atomic load + one Arc
/// clone.
pub enum MatrixSource {
    Static(Arc<ChannelMatrix>),
    Is08Tracked {
        rx: watch::Receiver<Arc<crate::api::nmos_is08::ChannelMap>>,
        /// IS-08 output id (`st2110_30:<flow>:<output>`) — the row this stage
        /// pulls from the map.
        output_id: String,
        /// Upstream input id reference (e.g. `st2110_30:<flow>`). Map entries
        /// referencing other inputs are ignored (cross-flow channel routing
        /// is not supported in this phase).
        input_id: String,
        /// Number of input channels — used to validate routing entries.
        in_channels: u8,
        /// Number of output channels in the resolved matrix.
        out_channels: u8,
        /// Static fallback used when the IS-08 entry is empty or absent.
        fallback: Arc<ChannelMatrix>,
        /// Cached resolved matrix from the most recent IS-08 snapshot.
        /// Refreshed whenever `rx.has_changed()` is true or this is the first
        /// call after construction.
        cached: Option<Arc<ChannelMatrix>>,
        /// Sticky flag forcing a refresh on first call.
        first_call: bool,
    },
}

impl MatrixSource {
    /// Convenience: build a static source from a fixed matrix.
    pub fn static_(matrix: ChannelMatrix) -> Self {
        MatrixSource::Static(Arc::new(matrix))
    }

    /// Build an IS-08-tracked source. Subscribes to the global IS-08 watch
    /// sender if one is registered; otherwise falls back to a static source.
    /// `fallback_matrix` is the operator's configured static matrix used when
    /// no IS-08 entry exists for `output_id`.
    pub fn is08_tracked(
        output_id: String,
        input_id: String,
        in_channels: u8,
        out_channels: u8,
        fallback_matrix: ChannelMatrix,
    ) -> Self {
        match subscribe_global_is08() {
            Some(rx) => MatrixSource::Is08Tracked {
                rx,
                output_id,
                input_id,
                in_channels,
                out_channels,
                fallback: Arc::new(fallback_matrix),
                cached: None,
                first_call: true,
            },
            None => MatrixSource::Static(Arc::new(fallback_matrix)),
        }
    }

    /// Snapshot the current matrix for one packet's worth of work. Cheap
    /// (`Arc` clone in steady state).
    pub fn current(&mut self) -> Arc<ChannelMatrix> {
        match self {
            MatrixSource::Static(m) => m.clone(),
            MatrixSource::Is08Tracked {
                rx,
                output_id,
                input_id,
                in_channels,
                out_channels,
                fallback,
                cached,
                first_call,
            } => {
                let changed = rx.has_changed().unwrap_or(false);
                if changed || *first_call {
                    *first_call = false;
                    let map = rx.borrow_and_update();
                    *cached = derive_matrix_from_is08(
                        &map,
                        output_id,
                        input_id,
                        *in_channels,
                        *out_channels,
                    )
                    .map(Arc::new);
                }
                cached.clone().unwrap_or_else(|| fallback.clone())
            }
        }
    }
}

/// Derive a per-output [`ChannelMatrix`] from an IS-08 [`ChannelMap`] entry.
///
/// Returns `None` if the map has no entry for `output_id`, if the entry has no
/// channels, or if every channel references a different input than `input_id`
/// (cross-flow routing is not supported here — the caller falls back to the
/// static matrix in that case).
///
/// Routing rules:
/// - Each IS-08 channel entry becomes one output-channel row.
/// - Entries with `input == Some(input_id)` and an in-range `channel_index`
///   contribute `(channel_index, 1.0)` to that row.
/// - Entries with `input == None` (muted) produce an empty row (silent
///   output channel).
/// - Entries with `input == Some(other)` (cross-input) are skipped — the row
///   is left empty (silent) for that channel rather than dropped, so the
///   output keeps its expected channel count.
/// - If the entry has fewer channels than `out_channels`, missing rows are
///   filled with empty (muted) rows.
pub fn derive_matrix_from_is08(
    map: &crate::api::nmos_is08::ChannelMap,
    output_id: &str,
    input_id: &str,
    in_channels: u8,
    out_channels: u8,
) -> Option<ChannelMatrix> {
    let entry = map.outputs.get(output_id)?;
    if entry.channels.is_empty() {
        return None;
    }
    let mut rows: Vec<Vec<(u8, f32)>> = Vec::with_capacity(out_channels as usize);
    for ch_entry in entry.channels.iter().take(out_channels as usize) {
        let row = match (&ch_entry.input, ch_entry.channel_index) {
            (Some(src), Some(idx))
                if src == input_id && (idx as u8) < in_channels =>
            {
                let gain = ch_entry.gain.unwrap_or(1.0_f32).max(0.0);
                vec![(idx as u8, gain)]
            }
            _ => Vec::new(),
        };
        rows.push(row);
    }
    while rows.len() < out_channels as usize {
        rows.push(Vec::new());
    }
    Some(ChannelMatrix(rows))
}

/// JSON-serialized transcode block on a config object.
///
/// All fields are optional so an operator can override only what they need;
/// unset fields fall back to validated defaults that match the upstream
/// input format. The validator (`crate::config::validation::validate_transcode`)
/// resolves this into a [`TranscodeConfig`] with concrete values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TranscodeJson {
    /// Output sample rate in Hz. One of 32000, 44100, 48000, 88200, 96000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Output bit depth. One of 16, 20, 24.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    /// Output channel count. 1..=16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
    /// Explicit channel routing matrix: `channel_map[out_ch] = [in_ch_1, in_ch_2, ...]`.
    /// All gains are unity. Use [`channel_map_with_gain`] for per-channel gain
    /// control, or [`channel_map_preset`] for named surround downmixes.
    ///
    /// Mutually exclusive with `channel_map_with_gain` and `channel_map_preset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_map: Option<Vec<Vec<u8>>>,
    /// Explicit channel routing matrix with per-input-channel gain control:
    /// `channel_map_with_gain[out_ch] = [[in_ch, gain], ...]`.
    ///
    /// Each entry is `[input_channel_index, linear_gain]` where gain is a
    /// linear multiplier (1.0 = unity, 0.5 ≈ -6 dB, 0.707 ≈ -3 dB, 0.0 = mute).
    ///
    /// Example — swap L/R with the right channel attenuated by 3 dB:
    /// ```json
    /// "channel_map_with_gain": [
    ///   [[1, 1.0]],
    ///   [[0, 0.707]]
    /// ]
    /// ```
    ///
    /// Example — mix channels 0 and 2 at different levels into output channel 0:
    /// ```json
    /// "channel_map_with_gain": [
    ///   [[0, 0.8], [2, 0.3]]
    /// ]
    /// ```
    ///
    /// Mutually exclusive with `channel_map` and `channel_map_preset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_map_with_gain: Option<Vec<Vec<[f64; 2]>>>,
    /// Named downmix preset. Mutually exclusive with `channel_map` and
    /// `channel_map_with_gain`.
    ///
    /// Supported: `mono_to_stereo`, `stereo_to_mono_3db`, `stereo_to_mono_6db`,
    /// `5_1_to_stereo_bs775`, `7_1_to_stereo_bs775`, `4ch_to_stereo_lt_rt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_map_preset: Option<String>,
    /// Output packet time in microseconds. One of 125, 250, 333, 500, 1000, 4000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_time_us: Option<u32>,
    /// Output dynamic RTP payload type, 96..=127.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_type: Option<u8>,
    /// SRC quality (default: high).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_quality: Option<SrcQuality>,
    /// Dither mode for bit-depth down-conversion (default: tpdf).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dither: Option<Dither>,
}

/// Concrete, validated transcode configuration ready for the runtime.
#[derive(Debug, Clone)]
pub struct TranscodeConfig {
    pub out_sample_rate: u32,
    pub out_bit_depth: BitDepth,
    pub out_channels: u8,
    pub channel_matrix: ChannelMatrix,
    pub out_packet_time_us: u32,
    pub out_payload_type: u8,
    pub src_quality: SrcQuality,
    pub dither: Dither,
}

/// Description of the upstream input format the transcoder receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputFormat {
    pub sample_rate: u32,
    pub bit_depth: BitDepth,
    pub channels: u8,
}

impl InputFormat {
    /// Resolve an [`InputFormat`] from the upstream flow input config, if the
    /// input is an audio essence (ST 2110-30 / -31). Returns `None` for non-audio
    /// inputs (RTP video, SRT MPEG-TS, etc.). Used by output spawn modules to
    /// gate transcode stage construction.
    pub fn from_input_config(input: &crate::config::models::InputConfig) -> Option<Self> {
        use crate::config::models::InputConfig;
        match input {
            InputConfig::St2110_30(c) | InputConfig::St2110_31(c) => Some(InputFormat {
                sample_rate: c.sample_rate,
                bit_depth: BitDepth::from_u8(c.bit_depth)?,
                channels: c.channels,
            }),
            InputConfig::RtpAudio(c) => Some(InputFormat {
                sample_rate: c.sample_rate,
                bit_depth: BitDepth::from_u8(c.bit_depth)?,
                channels: c.channels,
            }),
            _ => None,
        }
    }
}

// ── Stats ───────────────────────────────────────────────────────────────────

/// Lock-free counters for the transcode stage. Composed alongside the per-output
/// `OutputStatsAccumulator` by the spawn module.
#[derive(Debug, Default)]
pub struct TranscodeStats {
    pub input_packets: AtomicU64,
    pub output_packets: AtomicU64,
    pub dropped: AtomicU64,
    pub format_resets: AtomicU64,
    /// Last measured end-to-end latency through the transcoder, in microseconds.
    pub last_latency_us: AtomicU64,
}

impl TranscodeStats {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn inc_input(&self) {
        self.input_packets.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_output(&self, n: u64) {
        self.output_packets.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_reset(&self) {
        self.format_resets.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_latency(&self, us: u64) {
        self.last_latency_us.store(us, Ordering::Relaxed);
    }
}

// ── Channel-matrix preset expansion ─────────────────────────────────────────

/// Resolve a named channel-map preset against an input channel count, returning
/// the matrix or an error if the preset doesn't apply to the given input.
///
/// Presets follow standard broadcast layouts:
///
/// | Preset                   | In  | Out | Notes                                       |
/// |--------------------------|-----|-----|---------------------------------------------|
/// | `mono_to_stereo`         |  1  |  2  | Duplicate channel 0 to L and R              |
/// | `stereo_to_mono_3db`     |  2  |  1  | (L + R) × 0.7071 (~ -3.01 dB)               |
/// | `stereo_to_mono_6db`     |  2  |  1  | (L + R) × 0.5    (~ -6.02 dB)               |
/// | `5_1_to_stereo_bs775`    |  6  |  2  | ITU-R BS.775 5.1→stereo                     |
/// | `7_1_to_stereo_bs775`    |  8  |  2  | ITU-R BS.775 7.1→stereo                     |
/// | `4ch_to_stereo_lt_rt`    |  4  |  2  | Lt/Rt matrix encode (L,R,Ls,Rs → Lt,Rt)     |
pub fn expand_preset(preset: &str, in_channels: u8) -> Result<ChannelMatrix, String> {
    match preset {
        "mono_to_stereo" => {
            require_in_channels(preset, in_channels, 1)?;
            Ok(ChannelMatrix(vec![
                vec![(0, 1.0)],
                vec![(0, 1.0)],
            ]))
        }
        "stereo_to_mono_3db" => {
            require_in_channels(preset, in_channels, 2)?;
            const G: f32 = 0.707_106_77; // 1/sqrt(2)
            Ok(ChannelMatrix(vec![vec![(0, G), (1, G)]]))
        }
        "stereo_to_mono_6db" => {
            require_in_channels(preset, in_channels, 2)?;
            Ok(ChannelMatrix(vec![vec![(0, 0.5), (1, 0.5)]]))
        }
        "5_1_to_stereo_bs775" => {
            // 5.1 channel order: L, R, C, LFE, Ls, Rs
            // BS.775: Lt = L + (-3 dB)·C + (-3 dB)·Ls
            //         Rt = R + (-3 dB)·C + (-3 dB)·Rs
            // LFE is dropped (not present in stereo monitoring).
            require_in_channels(preset, in_channels, 6)?;
            const G3: f32 = 0.707_106_77;
            Ok(ChannelMatrix(vec![
                vec![(0, 1.0), (2, G3), (4, G3)],
                vec![(1, 1.0), (2, G3), (5, G3)],
            ]))
        }
        "7_1_to_stereo_bs775" => {
            // 7.1 channel order: L, R, C, LFE, Lss, Rss, Lrs, Rrs
            // BS.775 extension: Lt = L + (-3 dB)·C + (-3 dB)·Lss + (-3 dB)·Lrs
            //                   Rt = R + (-3 dB)·C + (-3 dB)·Rss + (-3 dB)·Rrs
            require_in_channels(preset, in_channels, 8)?;
            const G3: f32 = 0.707_106_77;
            Ok(ChannelMatrix(vec![
                vec![(0, 1.0), (2, G3), (4, G3), (6, G3)],
                vec![(1, 1.0), (2, G3), (5, G3), (7, G3)],
            ]))
        }
        "4ch_to_stereo_lt_rt" => {
            // Quad order: L, R, Ls, Rs
            // Lt = L + (-3 dB)·Ls
            // Rt = R + (-3 dB)·Rs
            require_in_channels(preset, in_channels, 4)?;
            const G3: f32 = 0.707_106_77;
            Ok(ChannelMatrix(vec![
                vec![(0, 1.0), (2, G3)],
                vec![(1, 1.0), (3, G3)],
            ]))
        }
        other => Err(format!("unknown channel_map_preset '{other}'")),
    }
}

fn require_in_channels(preset: &str, got: u8, want: u8) -> Result<(), String> {
    if got != want {
        Err(format!(
            "channel_map_preset '{preset}' requires {want} input channels, input has {got}"
        ))
    } else {
        Ok(())
    }
}

/// Resolve a [`TranscodeJson`] block against the upstream input format,
/// producing a concrete [`TranscodeConfig`]. Used by both the validator and
/// (in the runtime) the spawn module before constructing a [`TranscodeStage`].
pub fn resolve_transcode(
    json: &TranscodeJson,
    input: InputFormat,
) -> Result<TranscodeConfig, String> {
    let out_sample_rate = json.sample_rate.unwrap_or(input.sample_rate);
    let out_bit_depth = match json.bit_depth {
        Some(v) => BitDepth::from_u8(v).ok_or_else(|| {
            format!("transcode.bit_depth must be 16, 20, or 24, got {v}")
        })?,
        None => input.bit_depth,
    };
    let out_channels = json.channels.unwrap_or(input.channels);
    if out_channels == 0 || out_channels > 16 {
        return Err(format!(
            "transcode.channels must be 1..=16, got {out_channels}"
        ));
    }

    // Exactly one of channel_map, channel_map_with_gain, channel_map_preset
    // may be set. Count the number specified and reject if more than one.
    let map_sources = [
        json.channel_map.is_some(),
        json.channel_map_with_gain.is_some(),
        json.channel_map_preset.is_some(),
    ];
    if map_sources.iter().filter(|&&v| v).count() > 1 {
        return Err(
            "transcode: channel_map, channel_map_with_gain, and channel_map_preset \
             are mutually exclusive — specify at most one"
                .to_string(),
        );
    }

    let channel_matrix = if let Some(map) = json.channel_map.as_ref() {
        // Unity-gain index-only matrix.
        if map.len() != out_channels as usize {
            return Err(format!(
                "transcode.channel_map has {} rows but transcode.channels is {}",
                map.len(),
                out_channels
            ));
        }
        let mat = ChannelMatrix(
            map.iter()
                .map(|row| row.iter().map(|&ch| (ch, 1.0_f32)).collect())
                .collect(),
        );
        if let Some(max) = mat.max_input_channel() {
            if max >= input.channels {
                return Err(format!(
                    "transcode.channel_map references input channel {max} but input has only {} channels",
                    input.channels
                ));
            }
        }
        mat
    } else if let Some(map) = json.channel_map_with_gain.as_ref() {
        // Per-channel gain matrix: each entry is [channel_index, gain].
        if map.len() != out_channels as usize {
            return Err(format!(
                "transcode.channel_map_with_gain has {} rows but transcode.channels is {}",
                map.len(),
                out_channels
            ));
        }
        let mat = ChannelMatrix(
            map.iter()
                .enumerate()
                .map(|(row_idx, row)| {
                    row.iter()
                        .enumerate()
                        .map(|(col_idx, pair)| {
                            let ch = pair[0] as u8;
                            let gain = pair[1] as f32;
                            if gain < 0.0 {
                                tracing::warn!(
                                    "transcode.channel_map_with_gain[{}][{}]: negative gain {:.3} \
                                     clamped to 0.0 (use positive values only)",
                                    row_idx, col_idx, gain
                                );
                            }
                            (ch, gain.max(0.0))
                        })
                        .collect()
                })
                .collect(),
        );
        if let Some(max) = mat.max_input_channel() {
            if max >= input.channels {
                return Err(format!(
                    "transcode.channel_map_with_gain references input channel {max} but input has only {} channels",
                    input.channels
                ));
            }
        }
        mat
    } else if let Some(preset) = json.channel_map_preset.as_deref() {
        let mat = expand_preset(preset, input.channels)?;
        if mat.out_channels() as u8 != out_channels {
            return Err(format!(
                "channel_map_preset '{preset}' produces {} channels but transcode.channels is {}",
                mat.out_channels(),
                out_channels
            ));
        }
        mat
    } else {
        // Default: identity if channel counts match, mono→stereo dup if
        // expanding 1→2, sum-to-mono otherwise. We pick the conservative
        // identity here and let the validator reject mismatched defaults
        // so the operator must be explicit.
        if out_channels == input.channels {
            ChannelMatrix::identity(input.channels)
        } else if input.channels == 1 && out_channels == 2 {
            expand_preset("mono_to_stereo", 1)?
        } else if input.channels == 2 && out_channels == 1 {
            expand_preset("stereo_to_mono_3db", 2)?
        } else {
            return Err(format!(
                "transcode: input has {} channels and output has {} channels — \
                 specify channel_map, channel_map_with_gain, or channel_map_preset",
                input.channels, out_channels
            ));
        }
    };

    let out_packet_time_us = json.packet_time_us.unwrap_or({
        // Default to a sane packet time for the output sample rate. 1 ms is
        // standard for ST 2110-30 PM profile and works for everything else.
        1_000
    });
    let out_payload_type = json.payload_type.unwrap_or(97);
    let src_quality = json.src_quality.unwrap_or_default();
    let dither = json.dither.unwrap_or_default();

    Ok(TranscodeConfig {
        out_sample_rate,
        out_bit_depth,
        out_channels,
        channel_matrix,
        out_packet_time_us,
        out_payload_type,
        src_quality,
        dither,
    })
}

// ── PCM ⇄ f32 helpers ───────────────────────────────────────────────────────

/// Decode big-endian PCM into planar f32 samples in `[-1.0, 1.0]`.
///
/// `payload` must be `samples_per_channel * channels * bit_depth.wire_bytes()`
/// bytes long. Output is one `Vec<f32>` per channel; each is overwritten in
/// place (caller pre-sizes them).
pub fn decode_pcm_be(
    payload: &[u8],
    bit_depth: BitDepth,
    channels: u8,
    out: &mut [Vec<f32>],
) -> Result<usize, String> {
    let bps = bit_depth.wire_bytes();
    let frame_size = channels as usize * bps;
    if payload.len() % frame_size != 0 {
        return Err(format!(
            "decode_pcm_be: payload {} not aligned to frame size {}",
            payload.len(),
            frame_size
        ));
    }
    let n_frames = payload.len() / frame_size;
    if out.len() != channels as usize {
        return Err(format!(
            "decode_pcm_be: out has {} channels, expected {}",
            out.len(),
            channels
        ));
    }
    for ch in out.iter_mut() {
        ch.clear();
        ch.reserve(n_frames);
    }
    let mut off = 0;
    for _ in 0..n_frames {
        for ch in 0..channels as usize {
            let s = match bit_depth {
                BitDepth::L16 => {
                    let v = i16::from_be_bytes([payload[off], payload[off + 1]]);
                    (v as f32) / (i16::MAX as f32 + 1.0)
                }
                BitDepth::L20 | BitDepth::L24 => {
                    // Sign-extend 24-bit big-endian into i32
                    let b0 = payload[off] as i32;
                    let b1 = payload[off + 1] as i32;
                    let b2 = payload[off + 2] as i32;
                    let mut v = (b0 << 16) | (b1 << 8) | b2;
                    if v & 0x0080_0000 != 0 {
                        v |= !0x00FF_FFFF;
                    }
                    if matches!(bit_depth, BitDepth::L20) {
                        // L20 leaves the bottom 4 bits zero on the wire; shift
                        // them out for normalisation. Result is the 20-bit
                        // sample range.
                        let v20 = v >> 4;
                        (v20 as f32) / ((1 << 19) as f32)
                    } else {
                        (v as f32) / ((1 << 23) as f32)
                    }
                }
            };
            out[ch].push(s.clamp(-1.0, 1.0));
            off += bps;
        }
    }
    Ok(n_frames)
}

/// Encode planar f32 samples (`[-1.0, 1.0]`) into big-endian PCM bytes,
/// applying TPDF dither when down-converting from f32 to a smaller integer
/// width.
///
/// `samples_per_channel = planar[0].len()` (all channels must match).
pub fn encode_pcm_be(
    planar: &[Vec<f32>],
    bit_depth: BitDepth,
    dither: Dither,
    out: &mut BytesMut,
    rng: &mut TpdfRng,
) {
    let channels = planar.len();
    if channels == 0 {
        return;
    }
    let n = planar[0].len();
    let bps = bit_depth.wire_bytes();
    out.reserve(n * channels * bps);
    for i in 0..n {
        for ch in 0..channels {
            let s = planar[ch][i].clamp(-1.0, 1.0);
            match bit_depth {
                BitDepth::L16 => {
                    let scale = i16::MAX as f32 + 1.0;
                    let mut v = s * scale;
                    if matches!(dither, Dither::Tpdf) {
                        v += rng.tpdf_one_lsb_f32();
                    }
                    let q = (v.round() as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    out.extend_from_slice(&q.to_be_bytes());
                }
                BitDepth::L20 => {
                    let scale = (1 << 19) as f32;
                    let mut v = s * scale;
                    if matches!(dither, Dither::Tpdf) {
                        v += rng.tpdf_one_lsb_f32();
                    }
                    let q20 = (v.round() as i32).clamp(-(1 << 19), (1 << 19) - 1);
                    let q24 = q20 << 4; // shift back into the L24 wire slot
                    let bytes = [
                        ((q24 >> 16) & 0xFF) as u8,
                        ((q24 >> 8) & 0xFF) as u8,
                        (q24 & 0xFF) as u8,
                    ];
                    out.extend_from_slice(&bytes);
                }
                BitDepth::L24 => {
                    let scale = (1 << 23) as f32;
                    let mut v = s * scale;
                    // Only dither if the source is f32 with effective resolution
                    // greater than 24-bit. We always dither here for symmetry.
                    if matches!(dither, Dither::Tpdf) {
                        v += rng.tpdf_one_lsb_f32();
                    }
                    let q = (v.round() as i32).clamp(-(1 << 23), (1 << 23) - 1);
                    let bytes = [
                        ((q >> 16) & 0xFF) as u8,
                        ((q >> 8) & 0xFF) as u8,
                        (q & 0xFF) as u8,
                    ];
                    out.extend_from_slice(&bytes);
                }
            }
        }
    }
}

/// Apply a channel routing matrix to planar input samples, producing planar
/// output samples. `out` is pre-sized to `(matrix.out_channels(), n_frames)`.
pub fn apply_channel_matrix(
    input: &[Vec<f32>],
    matrix: &ChannelMatrix,
    out: &mut [Vec<f32>],
) -> Result<(), String> {
    if input.is_empty() {
        return Ok(());
    }
    let n_frames = input[0].len();
    if out.len() != matrix.out_channels() {
        return Err(format!(
            "apply_channel_matrix: out has {} channels, matrix expects {}",
            out.len(),
            matrix.out_channels()
        ));
    }
    for (out_ch, row) in matrix.0.iter().enumerate() {
        let dst = &mut out[out_ch];
        dst.clear();
        dst.resize(n_frames, 0.0);
        for &(in_ch, gain) in row {
            let src = input
                .get(in_ch as usize)
                .ok_or_else(|| format!("matrix references input channel {in_ch} which is missing"))?;
            if src.len() != n_frames {
                return Err(format!(
                    "apply_channel_matrix: input channel {in_ch} has {} frames, expected {}",
                    src.len(),
                    n_frames
                ));
            }
            for (d, &s) in dst.iter_mut().zip(src.iter()) {
                *d += s * gain;
            }
        }
    }
    Ok(())
}

// ── TPDF dither RNG ─────────────────────────────────────────────────────────

/// Lightweight xorshift PRNG used for TPDF dither. Avoids pulling `rand` into
/// the hot path; deterministic seed makes round-trip tests reproducible.
#[derive(Debug)]
pub struct TpdfRng {
    state: u64,
}

impl TpdfRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    #[inline]
    fn next_u32(&mut self) -> u32 {
        // xorshift64
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        (x >> 32) as u32
    }

    /// Sample a TPDF random value with peak-to-peak amplitude of 2 LSB at the
    /// f32 representation of one quantization step. Returned value is in
    /// `(-1.0, 1.0)` and the caller multiplies by the quantizer scale.
    #[inline]
    pub fn tpdf_one_lsb_f32(&mut self) -> f32 {
        // Two independent uniform samples in [0, 1) summed to a triangular
        // distribution in (-1, 1).
        let a = (self.next_u32() as f32) / (u32::MAX as f32);
        let b = (self.next_u32() as f32) / (u32::MAX as f32);
        a - b
    }
}

// ── TranscodeStage ──────────────────────────────────────────────────────────

/// Per-output PCM transcoding pipeline.
///
/// Construction is cheap: the rubato resampler is built lazily on the first
/// packet so the input format can be auto-detected. The stage is `!Sync` and
/// is owned by a single output task; cloning is not supported.
pub struct TranscodeStage {
    input: InputFormat,
    cfg: TranscodeConfig,
    matrix: MatrixSource,
    stats: Arc<TranscodeStats>,
    depacketizer: PcmDepacketizer,
    packetizer: PcmPacketizer,

    // Decode scratch (planar input samples). Lazily sized on first packet.
    in_scratch: Vec<Vec<f32>>,
    // Routed scratch (after channel matrix; planar at input sample rate).
    routed_scratch: Vec<Vec<f32>>,
    // SRC output accumulator (planar at output sample rate). Drained in
    // `out_samples_per_packet`-sized blocks into the packetizer.
    out_accum: Vec<Vec<f32>>,
    // Lazy resampler instance — created when we know the chunk size.
    resampler: Option<Async<f32>>,
    // SRC output scratch (planar). Sized to resampler.output_frames_max() at
    // construction; reused on every process call to keep the hot path
    // allocation-free.
    resample_out_scratch: Vec<Vec<f32>>,
    // Resampler input chunk size (frames). Determined by the first input packet.
    resampler_chunk_in: usize,
    // Number of output samples per emitted packet at the target rate.
    out_samples_per_packet: usize,
    // Re-encode scratch.
    encode_buf: BytesMut,
    // TPDF dither RNG.
    rng: TpdfRng,
}

impl TranscodeStage {
    /// Build a new transcode stage.
    ///
    /// `matrix` is a [`MatrixSource`] — either a fixed `Static` matrix or an
    /// `Is08Tracked` source that watches the IS-08 active channel map. The
    /// stage snapshots the current matrix once per packet via a single atomic
    /// load (`watch::Receiver::has_changed`) so IS-08 activations propagate
    /// without restarting the flow.
    pub fn new(
        input: InputFormat,
        cfg: TranscodeConfig,
        matrix: MatrixSource,
        stats: Arc<TranscodeStats>,
        ssrc: u32,
        initial_seq: u16,
        initial_timestamp: u32,
    ) -> Self {
        let depacketizer = PcmDepacketizer::new(
            input.bit_depth.pcm_format(),
            // The input PT is enforced upstream by the input task; we accept
            // anything here. The depacketizer requires `expected_pt < 128` so
            // pass 0 (RFC 3551 PCMU) which always passes — but we then need
            // to bypass the PT check. Simpler: build it with the configured
            // input PT taken from the cfg's payload_type. We don't actually
            // know the upstream PT here, so use a placeholder and call
            // `depacketize_relaxed` (custom helper) — for now we trust the
            // upstream input task to filter PT.
            0,
            input.channels,
        );
        let packetizer = PcmPacketizer::new(
            cfg.out_bit_depth.pcm_format(),
            cfg.out_sample_rate,
            cfg.out_channels,
            cfg.out_packet_time_us,
            cfg.out_payload_type,
            ssrc,
            initial_seq,
            initial_timestamp,
        );
        let out_samples_per_packet =
            ((cfg.out_packet_time_us as u64 * cfg.out_sample_rate as u64) / 1_000_000) as usize;
        let in_scratch = vec![Vec::new(); input.channels as usize];
        let routed_scratch = vec![Vec::new(); cfg.out_channels as usize];
        let out_accum = vec![Vec::new(); cfg.out_channels as usize];
        Self {
            input,
            cfg,
            matrix,
            stats,
            depacketizer,
            packetizer,
            in_scratch,
            routed_scratch,
            out_accum,
            resampler: None,
            resample_out_scratch: Vec::new(),
            resampler_chunk_in: 0,
            out_samples_per_packet,
            encode_buf: BytesMut::with_capacity(4096),
            rng: TpdfRng::new(0xCAFE_BABE),
        }
    }

    /// Reset internal state. Called on detected format changes; drops any
    /// in-flight resampler buffers and clears the output accumulator. The
    /// resampler will be rebuilt on the next packet.
    pub fn reset(&mut self) {
        self.resampler = None;
        self.resampler_chunk_in = 0;
        for ch in self.out_accum.iter_mut() {
            ch.clear();
        }
        self.stats.inc_reset();
    }

    /// Snapshot the current channel matrix from the [`MatrixSource`].
    fn current_matrix(&mut self) -> Arc<ChannelMatrix> {
        self.matrix.current()
    }

    /// Process one input RTP packet and return zero or more output RTP packets.
    ///
    /// The number of output packets per input packet depends on the relative
    /// packet times and sample rates. The transcoder accumulates samples
    /// internally; an early packet may yield zero outputs while a later one
    /// drains a backlog.
    pub fn process(&mut self, packet: &RtpPacket) -> Vec<Bytes> {
        self.stats.inc_input();
        let now_us = current_micros();

        // Parse the RTP header and extract the audio payload bytes. We
        // construct a fresh depacketizer that doesn't enforce a PT (the input
        // task already filtered) by reading the header inline.
        let payload = match parse_rtp_payload(&packet.data) {
            Some(p) => p,
            None => {
                self.stats.inc_dropped();
                return Vec::new();
            }
        };

        // Decode big-endian PCM into planar f32 at the input sample rate.
        let n_frames = match decode_pcm_be(
            payload,
            self.input.bit_depth,
            self.input.channels,
            &mut self.in_scratch,
        ) {
            Ok(n) => n,
            Err(_) => {
                self.stats.inc_dropped();
                return Vec::new();
            }
        };
        if n_frames == 0 {
            return Vec::new();
        }

        // Apply the live channel matrix.
        let matrix = self.current_matrix();
        if matrix.out_channels() != self.cfg.out_channels as usize {
            // Live matrix shape mismatch — fall back to the configured matrix.
            if let Err(_) = apply_channel_matrix(
                &self.in_scratch,
                &self.cfg.channel_matrix,
                &mut self.routed_scratch,
            ) {
                self.stats.inc_dropped();
                return Vec::new();
            }
        } else if let Err(_) = apply_channel_matrix(&self.in_scratch, &matrix, &mut self.routed_scratch)
        {
            self.stats.inc_dropped();
            return Vec::new();
        }

        // Run the resampler. Build it lazily so we can fix the chunk size to
        // the first packet's frame count.
        if self.input.sample_rate == self.cfg.out_sample_rate {
            // Pass-through: no SRC needed.
            for (ch_idx, ch) in self.routed_scratch.iter().enumerate() {
                if ch_idx < self.out_accum.len() {
                    self.out_accum[ch_idx].extend_from_slice(ch);
                }
            }
        } else {
            if self.resampler.is_none() || self.resampler_chunk_in != n_frames {
                let ratio = self.cfg.out_sample_rate as f64 / self.input.sample_rate as f64;
                let params = sinc_params_for(self.cfg.src_quality);
                match Async::<f32>::new_sinc(
                    ratio,
                    2.0,
                    &params,
                    n_frames,
                    self.cfg.out_channels as usize,
                    FixedAsync::Input,
                ) {
                    Ok(r) => {
                        let max_out = r.output_frames_max();
                        self.resample_out_scratch = (0..self.cfg.out_channels as usize)
                            .map(|_| vec![0.0f32; max_out])
                            .collect();
                        self.resampler = Some(r);
                        self.resampler_chunk_in = n_frames;
                    }
                    Err(_) => {
                        self.stats.inc_dropped();
                        return Vec::new();
                    }
                }
            }
            let r = self.resampler.as_mut().unwrap();
            let channels = self.cfg.out_channels as usize;
            let in_frames = n_frames;
            let out_frames_max = self.resample_out_scratch
                .first()
                .map(|v| v.len())
                .unwrap_or(0);
            let in_adapter = match SequentialSliceOfVecs::new(
                self.routed_scratch.as_slice(),
                channels,
                in_frames,
            ) {
                Ok(a) => a,
                Err(_) => {
                    self.stats.inc_dropped();
                    return Vec::new();
                }
            };
            let mut out_adapter = match SequentialSliceOfVecs::new_mut(
                self.resample_out_scratch.as_mut_slice(),
                channels,
                out_frames_max,
            ) {
                Ok(a) => a,
                Err(_) => {
                    self.stats.inc_dropped();
                    return Vec::new();
                }
            };
            let written = match r.process_into_buffer(
                &in_adapter,
                &mut out_adapter,
                None,
            ) {
                Ok((_in_used, out_written)) => out_written,
                Err(_) => {
                    self.stats.inc_dropped();
                    return Vec::new();
                }
            };
            for (ch_idx, ch) in self.resample_out_scratch.iter().enumerate() {
                if ch_idx < self.out_accum.len() {
                    self.out_accum[ch_idx].extend_from_slice(&ch[..written]);
                }
            }
        }

        let mut emitted = Vec::new();
        while self
            .out_accum
            .iter()
            .all(|c| c.len() >= self.out_samples_per_packet)
        {
            // Take one packet's worth from each channel.
            let mut block: Vec<Vec<f32>> = self
                .out_accum
                .iter_mut()
                .map(|c| c.drain(..self.out_samples_per_packet).collect())
                .collect();

            self.encode_buf.clear();
            encode_pcm_be(
                &block,
                self.cfg.out_bit_depth,
                self.cfg.dither,
                &mut self.encode_buf,
                &mut self.rng,
            );

            // Hand to the existing PcmPacketizer to build the RTP frame.
            let mut packets: Vec<Bytes> = Vec::with_capacity(1);
            let _leftover = self
                .packetizer
                .packetize(&self.encode_buf[..], &mut packets);
            // The packetizer is configured so each block produces exactly one
            // packet (block size == bytes_per_packet). leftover should be empty.
            for p in packets {
                emitted.push(p);
            }

            // Avoid moving the temporary out of the loop.
            let _ = block.drain(..);
        }

        self.stats.inc_output(emitted.len() as u64);
        let latency = current_micros().saturating_sub(now_us).max(packet.recv_time_us.saturating_sub(0).saturating_sub(now_us));
        // Latency relative to packet receive time, not relative to "now" entry,
        // so we recompute against recv_time_us:
        let lat = current_micros().saturating_sub(packet.recv_time_us);
        self.stats.record_latency(lat.max(latency.min(lat)));
        emitted
    }

    /// Process pre-decoded planar f32 PCM samples instead of an RTP-wrapped
    /// PCM packet.
    ///
    /// This is the entry point used by the AAC-decode bridge in
    /// [`crate::engine::audio_decode`]: the AAC decoder produces planar
    /// `Vec<Vec<f32>>` from each ADTS frame, and that is fed directly here,
    /// skipping the RTP header parse and `decode_pcm_be` stage that the
    /// regular [`process`](Self::process) method runs first.
    ///
    /// `planar` must have exactly `self.input.channels` rows and equal-length
    /// columns. `recv_time_us` is the upstream packet receive timestamp,
    /// used solely for latency stats.
    ///
    /// Output is the same as [`process`](Self::process): zero or more
    /// fully-formed RTP-PCM frames at the configured target rate / depth /
    /// channel count, ready to send to the wire.
    pub fn process_planar(&mut self, planar: &[Vec<f32>], recv_time_us: u64) -> Vec<Bytes> {
        self.stats.inc_input();

        if planar.len() != self.input.channels as usize {
            self.stats.inc_dropped();
            return Vec::new();
        }
        let n_frames = planar.first().map(|c| c.len()).unwrap_or(0);
        if n_frames == 0 {
            return Vec::new();
        }
        if planar.iter().any(|c| c.len() != n_frames) {
            self.stats.inc_dropped();
            return Vec::new();
        }

        // Mirror the existing process() pipeline starting at "apply channel
        // matrix". We copy planar into in_scratch so the rest of the code
        // path is byte-identical.
        for (dst, src) in self.in_scratch.iter_mut().zip(planar.iter()) {
            dst.clear();
            dst.extend_from_slice(src);
        }

        // Apply the live channel matrix.
        let matrix = self.current_matrix();
        if matrix.out_channels() != self.cfg.out_channels as usize {
            if let Err(_) = apply_channel_matrix(
                &self.in_scratch,
                &self.cfg.channel_matrix,
                &mut self.routed_scratch,
            ) {
                self.stats.inc_dropped();
                return Vec::new();
            }
        } else if let Err(_) =
            apply_channel_matrix(&self.in_scratch, &matrix, &mut self.routed_scratch)
        {
            self.stats.inc_dropped();
            return Vec::new();
        }

        // Resample (rebuilt lazily, identical to process()).
        if self.input.sample_rate == self.cfg.out_sample_rate {
            for (ch_idx, ch) in self.routed_scratch.iter().enumerate() {
                if ch_idx < self.out_accum.len() {
                    self.out_accum[ch_idx].extend_from_slice(ch);
                }
            }
        } else {
            if self.resampler.is_none() || self.resampler_chunk_in != n_frames {
                let ratio =
                    self.cfg.out_sample_rate as f64 / self.input.sample_rate as f64;
                let params = sinc_params_for(self.cfg.src_quality);
                match Async::<f32>::new_sinc(
                    ratio,
                    2.0,
                    &params,
                    n_frames,
                    self.cfg.out_channels as usize,
                    FixedAsync::Input,
                ) {
                    Ok(r) => {
                        let max_out = r.output_frames_max();
                        self.resample_out_scratch = (0..self.cfg.out_channels as usize)
                            .map(|_| vec![0.0f32; max_out])
                            .collect();
                        self.resampler = Some(r);
                        self.resampler_chunk_in = n_frames;
                    }
                    Err(_) => {
                        self.stats.inc_dropped();
                        return Vec::new();
                    }
                }
            }
            let r = self.resampler.as_mut().unwrap();
            let channels = self.cfg.out_channels as usize;
            let in_frames = n_frames;
            let out_frames_max =
                self.resample_out_scratch.first().map(|v| v.len()).unwrap_or(0);
            let in_adapter = match SequentialSliceOfVecs::new(
                self.routed_scratch.as_slice(),
                channels,
                in_frames,
            ) {
                Ok(a) => a,
                Err(_) => {
                    self.stats.inc_dropped();
                    return Vec::new();
                }
            };
            let mut out_adapter = match SequentialSliceOfVecs::new_mut(
                self.resample_out_scratch.as_mut_slice(),
                channels,
                out_frames_max,
            ) {
                Ok(a) => a,
                Err(_) => {
                    self.stats.inc_dropped();
                    return Vec::new();
                }
            };
            let written = match r.process_into_buffer(&in_adapter, &mut out_adapter, None) {
                Ok((_in_used, out_written)) => out_written,
                Err(_) => {
                    self.stats.inc_dropped();
                    return Vec::new();
                }
            };
            for (ch_idx, ch) in self.resample_out_scratch.iter().enumerate() {
                if ch_idx < self.out_accum.len() {
                    self.out_accum[ch_idx].extend_from_slice(&ch[..written]);
                }
            }
        }

        // Drain accumulated samples into output packets, identical to process().
        let mut emitted = Vec::new();
        while self
            .out_accum
            .iter()
            .all(|c| c.len() >= self.out_samples_per_packet)
        {
            let mut block: Vec<Vec<f32>> = self
                .out_accum
                .iter_mut()
                .map(|c| c.drain(..self.out_samples_per_packet).collect())
                .collect();

            self.encode_buf.clear();
            encode_pcm_be(
                &block,
                self.cfg.out_bit_depth,
                self.cfg.dither,
                &mut self.encode_buf,
                &mut self.rng,
            );

            let mut packets: Vec<Bytes> = Vec::with_capacity(1);
            let _leftover = self
                .packetizer
                .packetize(&self.encode_buf[..], &mut packets);
            for p in packets {
                emitted.push(p);
            }

            let _ = block.drain(..);
        }

        self.stats.inc_output(emitted.len() as u64);
        let lat = current_micros().saturating_sub(recv_time_us);
        self.stats.record_latency(lat);
        emitted
    }
}

// ── Planar PCM → PCM transcoder (for encoded-audio pipelines) ──────────────

/// Planar-f32-in / planar-f32-out transcode stage used by the AAC decode →
/// audio encode bridge. This is the "option 3" shuffle stage: it sits between
/// [`crate::engine::audio_decode::AacDecoder`] and
/// [`crate::engine::audio_encode::AudioEncoder`] on every video-carrying
/// output (SRT/UDP/RTP/RIST TS, RTMP, HLS, WebRTC), applying a channel
/// routing matrix and an optional sample-rate conversion before the encoder
/// runs.
///
/// Unlike [`TranscodeStage`] (which carries PCM RTP in and out), there is no
/// bit-depth stage here — encoders accept f32 directly — and no packetizer.
///
/// Fast paths (all determined at construction time):
/// - All fields unset and channel count matches source → full passthrough
///   (input is cloned; no matrix mul, no SRC).
/// - Channels + matrix form an identity, output rate == input rate →
///   passthrough without matrix mul.
/// - Output rate == input rate → matrix mul only, no rubato.
/// - Otherwise: matrix then rubato.
pub struct PlanarAudioTranscoder {
    in_rate: u32,
    in_channels: u8,
    out_rate: u32,
    out_channels: u8,
    matrix: ChannelMatrix,
    matrix_is_identity: bool,
    rate_unchanged: bool,
    routed_scratch: Vec<Vec<f32>>,
    resampler: Option<Async<f32>>,
    resample_out_scratch: Vec<Vec<f32>>,
    resampler_chunk_in: usize,
    src_quality: SrcQuality,
}

impl PlanarAudioTranscoder {
    /// Build a new planar transcoder against a discovered input format.
    ///
    /// `in_rate` and `in_channels` come from the upstream codec (typically the
    /// AAC decoder's first decoded frame). The concrete target is resolved
    /// from `json` against that input — unset fields fall back to the
    /// upstream values, so an empty `TranscodeJson` yields a pure passthrough.
    pub fn new(
        in_rate: u32,
        in_channels: u8,
        json: &TranscodeJson,
    ) -> Result<Self, String> {
        if in_channels == 0 {
            return Err("PlanarAudioTranscoder: in_channels must be > 0".to_string());
        }
        // Bit depth is irrelevant for the planar path, but `resolve_transcode`
        // requires it. Use L24 as a neutral placeholder — the resolver won't
        // inspect it beyond storing it on the TranscodeConfig, and the field
        // is ignored by this struct.
        let cfg = resolve_transcode(
            json,
            InputFormat {
                sample_rate: in_rate,
                bit_depth: BitDepth::L24,
                channels: in_channels,
            },
        )?;
        let matrix_is_identity =
            cfg.out_channels == in_channels && cfg.channel_matrix.is_identity();
        let rate_unchanged = cfg.out_sample_rate == in_rate;
        Ok(Self {
            in_rate,
            in_channels,
            out_rate: cfg.out_sample_rate,
            out_channels: cfg.out_channels,
            matrix: cfg.channel_matrix,
            matrix_is_identity,
            rate_unchanged,
            routed_scratch: vec![Vec::new(); cfg.out_channels as usize],
            resampler: None,
            resample_out_scratch: Vec::new(),
            resampler_chunk_in: 0,
            src_quality: cfg.src_quality,
        })
    }

    pub fn out_sample_rate(&self) -> u32 {
        self.out_rate
    }

    pub fn out_channels(&self) -> u8 {
        self.out_channels
    }

    pub fn in_sample_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn in_channels(&self) -> u8 {
        self.in_channels
    }

    /// True when this transcoder is a pure passthrough (same rate, identity
    /// matrix, same channel count). Callers MAY use this to skip `process`
    /// entirely, but it's safe to call `process` either way — the full-pass
    /// path just clones the input vectors.
    pub fn is_passthrough(&self) -> bool {
        self.matrix_is_identity && self.rate_unchanged
    }

    /// Transcode one chunk of planar f32 samples. `planar_in` must have
    /// exactly `in_channels` rows; each row must be the same length.
    /// Returns planar f32 at the target rate and channel count.
    pub fn process(&mut self, planar_in: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, String> {
        if planar_in.len() != self.in_channels as usize {
            return Err(format!(
                "PlanarAudioTranscoder: expected {} input channels, got {}",
                self.in_channels,
                planar_in.len()
            ));
        }
        let n_in = planar_in.first().map(|c| c.len()).unwrap_or(0);
        if n_in == 0 {
            return Ok((0..self.out_channels as usize).map(|_| Vec::new()).collect());
        }
        if planar_in.iter().any(|c| c.len() != n_in) {
            return Err("PlanarAudioTranscoder: channels have mismatched frame counts".to_string());
        }

        // Full passthrough fast-path.
        if self.matrix_is_identity && self.rate_unchanged {
            return Ok(planar_in.iter().map(|c| c.clone()).collect());
        }

        // Channel matrix stage.
        if self.matrix_is_identity {
            for (dst, src) in self.routed_scratch.iter_mut().zip(planar_in.iter()) {
                dst.clear();
                dst.extend_from_slice(src);
            }
        } else {
            apply_channel_matrix(planar_in, &self.matrix, &mut self.routed_scratch)?;
        }

        // SRC fast-path: same rate, matrix already applied.
        if self.rate_unchanged {
            return Ok(self.routed_scratch.iter().map(|c| c.clone()).collect());
        }

        // Rubato SRC: lazy construct (input chunk size comes from the first
        // call; rebuilt on any chunk-size change).
        if self.resampler.is_none() || self.resampler_chunk_in != n_in {
            let ratio = self.out_rate as f64 / self.in_rate as f64;
            let params = sinc_params_for(self.src_quality);
            let r = Async::<f32>::new_sinc(
                ratio,
                2.0,
                &params,
                n_in,
                self.out_channels as usize,
                FixedAsync::Input,
            )
            .map_err(|e| format!("PlanarAudioTranscoder: rubato init failed: {e}"))?;
            let max_out = r.output_frames_max();
            self.resample_out_scratch = (0..self.out_channels as usize)
                .map(|_| vec![0.0f32; max_out])
                .collect();
            self.resampler = Some(r);
            self.resampler_chunk_in = n_in;
        }
        let r = self.resampler.as_mut().unwrap();
        let channels = self.out_channels as usize;
        let out_frames_max = self
            .resample_out_scratch
            .first()
            .map(|v| v.len())
            .unwrap_or(0);
        let in_adapter =
            SequentialSliceOfVecs::new(self.routed_scratch.as_slice(), channels, n_in)
                .map_err(|e| format!("PlanarAudioTranscoder: in adapter: {e}"))?;
        let mut out_adapter = SequentialSliceOfVecs::new_mut(
            self.resample_out_scratch.as_mut_slice(),
            channels,
            out_frames_max,
        )
        .map_err(|e| format!("PlanarAudioTranscoder: out adapter: {e}"))?;
        let written = r
            .process_into_buffer(&in_adapter, &mut out_adapter, None)
            .map(|(_used, w)| w)
            .map_err(|e| format!("PlanarAudioTranscoder: rubato process: {e}"))?;
        Ok(self
            .resample_out_scratch
            .iter()
            .map(|c| c[..written].to_vec())
            .collect())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn sinc_params_for(quality: SrcQuality) -> SincInterpolationParameters {
    match quality {
        SrcQuality::High => SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        },
        SrcQuality::Fast => SincInterpolationParameters {
            sinc_len: 64,
            f_cutoff: 0.92,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::Hann,
        },
    }
}

/// Parse the RTP header and return a slice into the payload (samples).
/// Returns None if the buffer is malformed.
fn parse_rtp_payload(buf: &[u8]) -> Option<&[u8]> {
    if buf.len() < 12 {
        return None;
    }
    if (buf[0] >> 6) != 2 {
        return None;
    }
    let cc = (buf[0] & 0x0F) as usize;
    let header_len = 12 + cc * 4;
    if buf.len() < header_len {
        return None;
    }
    Some(&buf[header_len..])
}

#[inline]
fn current_micros() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn static_source(matrix: ChannelMatrix) -> MatrixSource {
        MatrixSource::static_(matrix)
    }

    fn build_rtp_packet_l24_stereo(samples: &[(i32, i32)], pt: u8, seq: u16, ts: u32) -> Bytes {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0x80, pt & 0x7F]);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&ts.to_be_bytes());
        buf.extend_from_slice(&0xdead_beefu32.to_be_bytes());
        for &(l, r) in samples {
            for v in [l, r] {
                let v24 = v & 0x00FF_FFFF;
                buf.extend_from_slice(&[
                    ((v24 >> 16) & 0xFF) as u8,
                    ((v24 >> 8) & 0xFF) as u8,
                    (v24 & 0xFF) as u8,
                ]);
            }
        }
        buf.freeze()
    }

    #[test]
    fn bit_depth_round_trip_l24() {
        // L24 → f32 → L24 should be sample-exact.
        let mut planar = vec![Vec::<f32>::new(); 2];
        let mut payload = BytesMut::new();
        for i in 0..32 {
            let l = (i * 1000) - 16000;
            let r = -((i * 1000) - 16000);
            for v in [l, r] {
                let v24 = v & 0x00FF_FFFF;
                payload.extend_from_slice(&[
                    ((v24 >> 16) & 0xFF) as u8,
                    ((v24 >> 8) & 0xFF) as u8,
                    (v24 & 0xFF) as u8,
                ]);
            }
        }
        let n = decode_pcm_be(&payload, BitDepth::L24, 2, &mut planar).unwrap();
        assert_eq!(n, 32);
        let mut out = BytesMut::new();
        let mut rng = TpdfRng::new(1);
        encode_pcm_be(&planar, BitDepth::L24, Dither::None, &mut out, &mut rng);
        assert_eq!(out.len(), payload.len());
        // Within ±1 LSB after f32 round-trip.
        for (a, b) in payload.iter().zip(out.iter()) {
            let diff = (*a as i32 - *b as i32).abs();
            assert!(diff <= 1, "L24 round-trip diff {diff} > 1");
        }
    }

    #[test]
    fn bit_depth_round_trip_l16() {
        let mut planar = vec![Vec::<f32>::new(); 1];
        let mut payload = BytesMut::new();
        for i in 0..64i32 {
            let v = (i * 100) - 3200;
            payload.extend_from_slice(&(v as i16).to_be_bytes());
        }
        let n = decode_pcm_be(&payload, BitDepth::L16, 1, &mut planar).unwrap();
        assert_eq!(n, 64);
        let mut out = BytesMut::new();
        let mut rng = TpdfRng::new(2);
        encode_pcm_be(&planar, BitDepth::L16, Dither::None, &mut out, &mut rng);
        assert_eq!(out.len(), payload.len());
        for (a, b) in payload.iter().zip(out.iter()) {
            let diff = (*a as i32 - *b as i32).abs();
            assert!(diff <= 1, "L16 round-trip diff {diff} > 1");
        }
    }

    #[test]
    fn channel_matrix_identity() {
        let m = ChannelMatrix::identity(2);
        let input = vec![vec![0.5_f32, 0.25, -0.5], vec![-0.25, 0.0, 0.75]];
        let mut out = vec![Vec::new(); 2];
        apply_channel_matrix(&input, &m, &mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn channel_matrix_mono_to_stereo() {
        let m = expand_preset("mono_to_stereo", 1).unwrap();
        let input = vec![vec![0.5_f32, -0.25, 0.0]];
        let mut out = vec![Vec::new(); 2];
        apply_channel_matrix(&input, &m, &mut out).unwrap();
        assert_eq!(out[0], input[0]);
        assert_eq!(out[1], input[0]);
    }

    #[test]
    fn channel_matrix_stereo_to_mono_3db() {
        let m = expand_preset("stereo_to_mono_3db", 2).unwrap();
        let input = vec![vec![1.0_f32], vec![1.0_f32]];
        let mut out = vec![Vec::new(); 1];
        apply_channel_matrix(&input, &m, &mut out).unwrap();
        // -3 dB sum: 1.0 * 0.7071 + 1.0 * 0.7071 = sqrt(2)
        assert!((out[0][0] - std::f32::consts::SQRT_2).abs() < 1e-4);
    }

    #[test]
    fn channel_matrix_5_1_to_stereo() {
        let m = expand_preset("5_1_to_stereo_bs775", 6).unwrap();
        // L=1, R=1, C=1, LFE=1, Ls=1, Rs=1
        let input: Vec<Vec<f32>> = (0..6).map(|_| vec![1.0_f32]).collect();
        let mut out = vec![Vec::new(); 2];
        apply_channel_matrix(&input, &m, &mut out).unwrap();
        // Lt = L + 0.7071*C + 0.7071*Ls = 1 + 0.7071 + 0.7071 ≈ 2.4142
        assert!((out[0][0] - 2.414_213_5).abs() < 1e-4);
        assert!((out[1][0] - 2.414_213_5).abs() < 1e-4);
    }

    #[test]
    fn preset_rejects_wrong_input_channels() {
        assert!(expand_preset("mono_to_stereo", 2).is_err());
        assert!(expand_preset("5_1_to_stereo_bs775", 4).is_err());
        assert!(expand_preset("nonexistent", 2).is_err());
    }

    #[test]
    fn resolve_transcode_default_identity() {
        let json = TranscodeJson::default();
        let cfg = resolve_transcode(
            &json,
            InputFormat {
                sample_rate: 48_000,
                bit_depth: BitDepth::L24,
                channels: 2,
            },
        )
        .unwrap();
        assert_eq!(cfg.out_sample_rate, 48_000);
        assert_eq!(cfg.out_bit_depth, BitDepth::L24);
        assert_eq!(cfg.out_channels, 2);
        assert_eq!(cfg.channel_matrix.0.len(), 2);
    }

    #[test]
    fn resolve_transcode_explicit_overrides() {
        let json = TranscodeJson {
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            channel_map: None,
            channel_map_with_gain: None,
            channel_map_preset: Some("5_1_to_stereo_bs775".to_string()),
            packet_time_us: Some(4_000),
            payload_type: Some(96),
            src_quality: Some(SrcQuality::Fast),
            dither: Some(Dither::Tpdf),
        };
        let cfg = resolve_transcode(
            &json,
            InputFormat {
                sample_rate: 48_000,
                bit_depth: BitDepth::L24,
                channels: 6,
            },
        )
        .unwrap();
        assert_eq!(cfg.out_sample_rate, 44_100);
        assert_eq!(cfg.out_bit_depth, BitDepth::L16);
        assert_eq!(cfg.out_channels, 2);
        assert_eq!(cfg.channel_matrix.0.len(), 2);
        assert_eq!(cfg.out_packet_time_us, 4_000);
        assert_eq!(cfg.out_payload_type, 96);
        assert!(matches!(cfg.src_quality, SrcQuality::Fast));
    }

    #[test]
    fn resolve_transcode_rejects_both_map_and_preset() {
        let json = TranscodeJson {
            channel_map: Some(vec![vec![0], vec![1]]),
            channel_map_preset: Some("mono_to_stereo".to_string()),
            ..Default::default()
        };
        let err = resolve_transcode(
            &json,
            InputFormat {
                sample_rate: 48_000,
                bit_depth: BitDepth::L24,
                channels: 2,
            },
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn resolve_transcode_rejects_oob_channel_map() {
        let json = TranscodeJson {
            channel_map: Some(vec![vec![0], vec![5]]),
            channels: Some(2),
            ..Default::default()
        };
        let err = resolve_transcode(
            &json,
            InputFormat {
                sample_rate: 48_000,
                bit_depth: BitDepth::L24,
                channels: 2,
            },
        )
        .unwrap_err();
        assert!(err.contains("input channel"));
    }

    #[test]
    fn resolve_transcode_rejects_invalid_bit_depth() {
        let json = TranscodeJson {
            bit_depth: Some(8),
            ..Default::default()
        };
        let err = resolve_transcode(
            &json,
            InputFormat {
                sample_rate: 48_000,
                bit_depth: BitDepth::L24,
                channels: 2,
            },
        )
        .unwrap_err();
        assert!(err.contains("16, 20, or 24"));
    }

    #[test]
    fn transcode_passthrough_48k_l24_stereo() {
        // No SRC, no bit depth change, no matrix change. Should produce one
        // output packet per one input packet at the same payload size.
        let input = InputFormat {
            sample_rate: 48_000,
            bit_depth: BitDepth::L24,
            channels: 2,
        };
        let cfg = resolve_transcode(&TranscodeJson::default(), input).unwrap();
        let stats = Arc::new(TranscodeStats::new());
        let matrix = static_source(cfg.channel_matrix.clone());
        let mut stage = TranscodeStage::new(input, cfg, matrix, stats.clone(), 0xdead_beef, 0, 0);

        // Build one 1ms packet of stereo L24 at 48kHz: 48 frames.
        let samples: Vec<(i32, i32)> = (0..48).map(|i| (i * 100, -i * 100)).collect();
        let data = build_rtp_packet_l24_stereo(&samples, 97, 0, 0);
        let pkt = RtpPacket {
            data,
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: current_micros(),
            is_raw_ts: false,
            upstream_seq: None,
            upstream_leg_id: None,
        };

        let out = stage.process(&pkt);
        assert_eq!(out.len(), 1, "expected 1 output packet for 1 input packet");
        assert_eq!(stats.input_packets.load(Ordering::Relaxed), 1);
        assert_eq!(stats.output_packets.load(Ordering::Relaxed), 1);
        // Output payload size: 48 frames × 2 ch × 3 bytes = 288 bytes after RTP header.
        let payload_len = parse_rtp_payload(&out[0]).unwrap().len();
        assert_eq!(payload_len, 48 * 2 * 3);
    }

    #[test]
    fn transcode_resamples_48k_to_44_1k() {
        // 48 kHz → 44.1 kHz, L24 stereo, 1ms packets.
        // 1 ms at 44.1 kHz = 44.1 samples; the resampler internally accumulates
        // and drains in 1 ms blocks (44 samples on average) so we should get
        // approximately one output packet per input packet after a small
        // priming period.
        let input = InputFormat {
            sample_rate: 48_000,
            bit_depth: BitDepth::L24,
            channels: 2,
        };
        let cfg = resolve_transcode(
            &TranscodeJson {
                sample_rate: Some(44_100),
                ..Default::default()
            },
            input,
        )
        .unwrap();
        assert_eq!(cfg.out_sample_rate, 44_100);
        let stats = Arc::new(TranscodeStats::new());
        let matrix = static_source(cfg.channel_matrix.clone());
        let mut stage = TranscodeStage::new(input, cfg, matrix, stats.clone(), 0xdead_beef, 0, 0);

        let mut total_out = 0;
        // Push 50 input packets (50 ms of audio).
        for i in 0..50 {
            let samples: Vec<(i32, i32)> =
                (0..48).map(|n| ((n + i * 48) * 10, -(n + i * 48) * 10)).collect();
            let data = build_rtp_packet_l24_stereo(&samples, 97, i as u16, (i * 48) as u32);
            let pkt = RtpPacket {
                data,
                sequence_number: i as u16,
                rtp_timestamp: (i * 48) as u32,
                recv_time_us: current_micros(),
                is_raw_ts: false,
                upstream_seq: None,
                upstream_leg_id: None,
            };
            total_out += stage.process(&pkt).len();
        }
        // After 50ms of input we should have ~44 output packets (1ms each
        // at 44.1kHz). Allow a generous priming buffer.
        assert!(
            (35..=50).contains(&total_out),
            "got {total_out} output packets, expected ~44"
        );
    }

    #[test]
    fn transcode_drops_malformed_payload() {
        let input = InputFormat {
            sample_rate: 48_000,
            bit_depth: BitDepth::L24,
            channels: 2,
        };
        let cfg = resolve_transcode(&TranscodeJson::default(), input).unwrap();
        let stats = Arc::new(TranscodeStats::new());
        let matrix = static_source(cfg.channel_matrix.clone());
        let mut stage = TranscodeStage::new(input, cfg, matrix, stats.clone(), 0, 0, 0);

        // Too short (no RTP header)
        let pkt = RtpPacket {
            data: Bytes::from(vec![0u8; 5]),
            sequence_number: 0,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: false,
            upstream_seq: None,
            upstream_leg_id: None,
        };
        let out = stage.process(&pkt);
        assert!(out.is_empty());
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn is08_derivation_passthrough_stereo() {
        use crate::api::nmos_is08::{ChannelEntry, ChannelMap, OutputMapping};
        let mut map = ChannelMap::default();
        let mut out = OutputMapping::default();
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(0),
            gain: None,
        });
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(1),
            gain: None,
        });
        map.outputs.insert("st2110_30:flow1:out1".into(), out);

        let m = derive_matrix_from_is08(
            &map,
            "st2110_30:flow1:out1",
            "st2110_30:flow1",
            2,
            2,
        )
        .expect("matrix");
        assert_eq!(m.0.len(), 2);
        assert_eq!(m.0[0], vec![(0, 1.0)]);
        assert_eq!(m.0[1], vec![(1, 1.0)]);
    }

    #[test]
    fn is08_derivation_swap_channels() {
        use crate::api::nmos_is08::{ChannelEntry, ChannelMap, OutputMapping};
        let mut map = ChannelMap::default();
        let mut out = OutputMapping::default();
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(1),
            gain: None,
        });
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(0),
            gain: None,
        });
        map.outputs.insert("st2110_30:flow1:out1".into(), out);
        let m = derive_matrix_from_is08(
            &map,
            "st2110_30:flow1:out1",
            "st2110_30:flow1",
            2,
            2,
        )
        .unwrap();
        assert_eq!(m.0[0], vec![(1, 1.0)]);
        assert_eq!(m.0[1], vec![(0, 1.0)]);
    }

    #[test]
    fn is08_derivation_muted_and_cross_input_become_silent() {
        use crate::api::nmos_is08::{ChannelEntry, ChannelMap, OutputMapping};
        let mut map = ChannelMap::default();
        let mut out = OutputMapping::default();
        out.channels.push(ChannelEntry { input: None, channel_index: None, gain: None });
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:other-flow".into()),
            channel_index: Some(0),
            gain: None,
        });
        map.outputs.insert("st2110_30:flow1:out1".into(), out);
        let m = derive_matrix_from_is08(
            &map,
            "st2110_30:flow1:out1",
            "st2110_30:flow1",
            2,
            2,
        )
        .unwrap();
        assert!(m.0[0].is_empty(), "muted channel should produce empty row");
        assert!(m.0[1].is_empty(), "cross-input channel should produce empty row");
    }

    #[test]
    fn is08_derivation_missing_entry_returns_none() {
        use crate::api::nmos_is08::ChannelMap;
        let map = ChannelMap::default();
        assert!(derive_matrix_from_is08(&map, "missing", "in", 2, 2).is_none());
    }

    #[test]
    fn matrix_source_static_returns_fixed_matrix() {
        let mut src = MatrixSource::static_(ChannelMatrix::identity(2));
        let m = src.current();
        assert_eq!(m.0.len(), 2);
    }

    #[tokio::test]
    async fn matrix_source_is08_tracked_picks_up_activation() {
        use crate::api::nmos_is08::{ChannelEntry, ChannelMap, OutputMapping};
        let (tx, rx) = watch::channel(Arc::new(ChannelMap::default()));
        let fallback = ChannelMatrix::identity(2);
        let mut src = MatrixSource::Is08Tracked {
            rx,
            output_id: "st2110_30:flow1:out1".into(),
            input_id: "st2110_30:flow1".into(),
            in_channels: 2,
            out_channels: 2,
            fallback: Arc::new(fallback),
            cached: None,
            first_call: true,
        };
        // Initial: empty IS-08 map → fallback (identity).
        let m1 = src.current();
        assert_eq!(m1.0[0], vec![(0, 1.0)]);
        assert_eq!(m1.0[1], vec![(1, 1.0)]);

        // Activate a swap map.
        let mut new_map = ChannelMap::default();
        let mut out = OutputMapping::default();
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(1),
            gain: None,
        });
        out.channels.push(ChannelEntry {
            input: Some("st2110_30:flow1".into()),
            channel_index: Some(0),
            gain: None,
        });
        new_map.outputs.insert("st2110_30:flow1:out1".into(), out);
        tx.send(Arc::new(new_map)).unwrap();

        // Next call should observe the new map (swap).
        let m2 = src.current();
        assert_eq!(m2.0[0], vec![(1, 1.0)]);
        assert_eq!(m2.0[1], vec![(0, 1.0)]);
    }

    #[test]
    fn tpdf_rng_distribution_centered() {
        let mut rng = TpdfRng::new(42);
        let mut sum = 0.0_f64;
        for _ in 0..10_000 {
            sum += rng.tpdf_one_lsb_f32() as f64;
        }
        let mean = sum / 10_000.0;
        // Mean of TPDF distribution is 0; allow noise.
        assert!(mean.abs() < 0.05, "mean {mean} not centered");
    }

    // ── PlanarAudioTranscoder tests ──────────────────────────────────────

    /// Empty transcode block on a planar transcoder must pass samples
    /// through unchanged — this is the option-3 contract.
    #[test]
    fn planar_transcoder_empty_is_passthrough() {
        let planar_in: Vec<Vec<f32>> = vec![
            vec![0.1, 0.2, 0.3, 0.4],
            vec![-0.1, -0.2, -0.3, -0.4],
        ];
        let tj = TranscodeJson::default();
        let mut tc = PlanarAudioTranscoder::new(48_000, 2, &tj).unwrap();
        assert!(tc.is_passthrough());
        assert_eq!(tc.out_sample_rate(), 48_000);
        assert_eq!(tc.out_channels(), 2);
        let out = tc.process(&planar_in).unwrap();
        assert_eq!(out, planar_in);
    }

    /// `channel_map_preset = stereo_to_mono_3db` collapses 2 → 1 with a
    /// -3 dB mix of L and R. No SRC should run — same input/output rate.
    #[test]
    fn planar_transcoder_stereo_to_mono_3db_preset() {
        let planar_in: Vec<Vec<f32>> = vec![
            vec![0.5, 0.3, -0.2],
            vec![0.5, 0.3, -0.2],
        ];
        let tj = TranscodeJson {
            channels: Some(1),
            channel_map_preset: Some("stereo_to_mono_3db".to_string()),
            ..TranscodeJson::default()
        };
        let mut tc = PlanarAudioTranscoder::new(48_000, 2, &tj).unwrap();
        assert_eq!(tc.out_channels(), 1);
        assert_eq!(tc.out_sample_rate(), 48_000);
        assert!(!tc.is_passthrough());
        let out = tc.process(&planar_in).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 3);
        // Expected: each sample * 0.707 + same * 0.707 ≈ 1.414 * sample.
        let gain = 2.0_f32 * 0.707_106_77;
        for i in 0..3 {
            let expected = planar_in[0][i] * gain;
            assert!(
                (out[0][i] - expected).abs() < 1e-5,
                "sample {i}: got {}, want {expected}",
                out[0][i]
            );
        }
    }

    /// Setting only `transcode.sample_rate` with the same value as the
    /// source must still be a passthrough (no rubato invocation).
    #[test]
    fn planar_transcoder_same_rate_is_passthrough_on_rate() {
        let planar_in: Vec<Vec<f32>> = vec![vec![0.25; 16]];
        let tj = TranscodeJson {
            sample_rate: Some(48_000),
            ..TranscodeJson::default()
        };
        let mut tc = PlanarAudioTranscoder::new(48_000, 1, &tj).unwrap();
        assert!(tc.is_passthrough());
        let out = tc.process(&planar_in).unwrap();
        assert_eq!(out, planar_in);
    }

    /// Channel swap via explicit `channel_map`: out[0] = in[1], out[1] = in[0].
    #[test]
    fn planar_transcoder_lr_swap_via_channel_map() {
        let planar_in: Vec<Vec<f32>> = vec![
            vec![0.1, 0.2, 0.3],
            vec![-0.1, -0.2, -0.3],
        ];
        let tj = TranscodeJson {
            channels: Some(2),
            channel_map: Some(vec![vec![1], vec![0]]),
            ..TranscodeJson::default()
        };
        let mut tc = PlanarAudioTranscoder::new(48_000, 2, &tj).unwrap();
        let out = tc.process(&planar_in).unwrap();
        assert_eq!(out[0], planar_in[1]);
        assert_eq!(out[1], planar_in[0]);
    }

    /// Rejects mismatched input channel count so callers get a clear error
    /// rather than silently zero-padding the wrong plane.
    #[test]
    fn planar_transcoder_rejects_channel_count_mismatch() {
        let tj = TranscodeJson::default();
        let mut tc = PlanarAudioTranscoder::new(48_000, 2, &tj).unwrap();
        let wrong: Vec<Vec<f32>> = vec![vec![0.0; 4]]; // only 1 channel
        assert!(tc.process(&wrong).is_err());
    }

    #[test]
    fn channel_matrix_is_identity_helper() {
        assert!(ChannelMatrix::identity(2).is_identity());
        assert!(ChannelMatrix::identity(5).is_identity());
        // Single non-identity entry breaks it.
        let m = ChannelMatrix(vec![vec![(1, 1.0)], vec![(0, 1.0)]]);
        assert!(!m.is_identity());
        // Two sources on the same row breaks it.
        let m = ChannelMatrix(vec![vec![(0, 1.0), (1, 0.5)]]);
        assert!(!m.is_identity());
    }

    // ── Additional coverage (gaps identified by test audit) ─────────────────

    /// L20 samples live in the upper 20 bits of the L24 wire slot with the
    /// bottom 4 bits zeroed. A round-trip through f32 must preserve the
    /// payload within ±1 LSB of the 20-bit range (i.e. ±16 in L24 units).
    #[test]
    fn bit_depth_round_trip_l20() {
        let mut planar = vec![Vec::<f32>::new(); 2];
        let mut payload = BytesMut::new();
        // Emit values that fit in 20 bits (range ±524288) with bottom 4 bits zero.
        for i in 0..32i32 {
            let l = ((i * 1000) - 16000) << 4; // already 20-bit << 4 = L24 slot
            let r = -(((i * 1000) - 16000) << 4);
            for v in [l, r] {
                let v24 = v & 0x00FF_FFFF;
                payload.extend_from_slice(&[
                    ((v24 >> 16) & 0xFF) as u8,
                    ((v24 >> 8) & 0xFF) as u8,
                    (v24 & 0xFF) as u8,
                ]);
            }
        }
        let n = decode_pcm_be(&payload, BitDepth::L20, 2, &mut planar).unwrap();
        assert_eq!(n, 32);
        let mut out = BytesMut::new();
        let mut rng = TpdfRng::new(3);
        encode_pcm_be(&planar, BitDepth::L20, Dither::None, &mut out, &mut rng);
        assert_eq!(out.len(), payload.len());
        // L20 precision is 16x coarser than L24, so compare as 24-bit values
        // and allow one L20 LSB of drift from the f32 round-trip.
        for frame in 0..(payload.len() / 3) {
            let off = frame * 3;
            let a = (((payload[off] as i32) << 16)
                | ((payload[off + 1] as i32) << 8)
                | payload[off + 2] as i32) as i32;
            let b = (((out[off] as i32) << 16)
                | ((out[off + 1] as i32) << 8)
                | out[off + 2] as i32) as i32;
            let a_s = if a & 0x0080_0000 != 0 { a | !0x00FF_FFFF } else { a };
            let b_s = if b & 0x0080_0000 != 0 { b | !0x00FF_FFFF } else { b };
            let diff = (a_s - b_s).abs();
            assert!(diff <= 16, "L20 round-trip diff {diff} > 16 (one L20 LSB)");
        }
    }

    /// **Audit gap**: the existing SRC test only checks packet count, so a bug
    /// that emitted all-zero samples would still pass. Drive a 1 kHz sine
    /// through `PlanarAudioTranscoder` at 48 kHz → 44.1 kHz and verify the
    /// output RMS matches the input RMS (~0.3536 for amplitude 0.5). A
    /// silence bug drops RMS to ~0; a scale bug moves it outside tolerance.
    #[test]
    fn planar_transcoder_src_preserves_sine_rms() {
        const N_IN: usize = 4800; // 100 ms at 48 kHz
        const FREQ: f32 = 1000.0;
        const AMP: f32 = 0.5;
        let mut ch_l = Vec::with_capacity(N_IN);
        let mut ch_r = Vec::with_capacity(N_IN);
        for i in 0..N_IN {
            let t = i as f32 / 48_000.0;
            let s = AMP * (2.0 * std::f32::consts::PI * FREQ * t).sin();
            ch_l.push(s);
            ch_r.push(s);
        }
        let planar_in: Vec<Vec<f32>> = vec![ch_l, ch_r];

        let tj = TranscodeJson {
            sample_rate: Some(44_100),
            ..TranscodeJson::default()
        };
        let mut tc = PlanarAudioTranscoder::new(48_000, 2, &tj).unwrap();
        assert!(!tc.is_passthrough());
        assert_eq!(tc.out_sample_rate(), 44_100);
        let out = tc.process(&planar_in).unwrap();
        assert_eq!(out.len(), 2);

        // Expected output length ≈ 4800 * 44100/48000 = 4410. rubato's FixedAsync::Input
        // mode processes the full input chunk; the first-call output size is close to
        // that. Allow ±10% for filter warmup.
        let out_len = out[0].len();
        assert!(
            (3900..=4500).contains(&out_len),
            "output sample count {out_len} outside expected range ~4410"
        );
        assert_eq!(out[0].len(), out[1].len(), "channels desynced");

        // Compute RMS, skipping the leading filter-warmup transient (first 10 ms).
        let skip = (44_100 * 10 / 1000) as usize;
        assert!(out_len > skip + 100, "output too short to measure");
        let body = &out[0][skip..];
        let sum_sq: f64 = body.iter().map(|s| (*s as f64).powi(2)).sum();
        let rms = (sum_sq / body.len() as f64).sqrt();
        let expected = (AMP as f64) / std::f64::consts::SQRT_2; // 0.3536
        // Accept ±10% of expected RMS. A silence bug gives ~0; a 2x scale bug
        // gives ~0.707 — both fail this check.
        let lo = expected * 0.9;
        let hi = expected * 1.1;
        assert!(
            rms >= lo && rms <= hi,
            "SRC output RMS {rms:.4} outside [{lo:.4}, {hi:.4}] — signal not preserved"
        );
    }

    /// `channel_map_with_gain` is an entirely separate code path from
    /// `channel_map` and presets and has zero coverage. Build an L/R swap
    /// with -6 dB (0.5 linear) on the swapped right channel and verify the
    /// gain is applied to each sample.
    #[test]
    fn planar_transcoder_channel_map_with_gain_attenuates() {
        let planar_in: Vec<Vec<f32>> = vec![
            vec![1.0, 0.5, -0.25],
            vec![-1.0, -0.5, 0.25],
        ];
        let tj = TranscodeJson {
            channels: Some(2),
            // Row 0: out_L = in_R at unity. Row 1: out_R = in_L at 0.5 (-6 dB).
            channel_map_with_gain: Some(vec![
                vec![[1.0, 1.0]],
                vec![[0.0, 0.5]],
            ]),
            ..TranscodeJson::default()
        };
        let mut tc = PlanarAudioTranscoder::new(48_000, 2, &tj).unwrap();
        assert!(!tc.is_passthrough());
        let out = tc.process(&planar_in).unwrap();
        assert_eq!(out.len(), 2);
        // Left output = original right, unchanged.
        assert_eq!(out[0], planar_in[1]);
        // Right output = original left * 0.5.
        for i in 0..planar_in[0].len() {
            let expected = planar_in[0][i] * 0.5;
            assert!(
                (out[1][i] - expected).abs() < 1e-6,
                "sample {i}: got {}, want {expected}",
                out[1][i]
            );
        }
    }

    /// Per the docstring on `channel_map_with_gain`, negative gains are
    /// clamped to 0 (silent) so an operator typo cannot invert a channel.
    #[test]
    fn resolve_transcode_clamps_negative_gain() {
        let json = TranscodeJson {
            channels: Some(1),
            channel_map_with_gain: Some(vec![vec![[0.0, -1.5]]]),
            ..TranscodeJson::default()
        };
        let cfg = resolve_transcode(
            &json,
            InputFormat {
                sample_rate: 48_000,
                bit_depth: BitDepth::L24,
                channels: 2,
            },
        )
        .unwrap();
        assert_eq!(cfg.channel_matrix.0.len(), 1);
        assert_eq!(cfg.channel_matrix.0[0].len(), 1);
        let (_ch, gain) = cfg.channel_matrix.0[0][0];
        assert_eq!(gain, 0.0, "negative gain must clamp to 0.0, got {gain}");
    }

    /// Combined path: matrix downmix AND sample-rate conversion in the same
    /// `process()` call. 6ch@48k → 2ch@44.1k via the 5.1→stereo BS.775 preset.
    /// A DC-like input (constant positive value on L/C/Ls, zero elsewhere)
    /// must produce a non-zero stereo output at the downsampled rate, with
    /// the left output dominant (L + -3dB·C + -3dB·Ls).
    #[test]
    fn planar_transcoder_5_1_downmix_with_src() {
        const N_IN: usize = 4800;
        // 5.1 channel order: L, R, C, LFE, Ls, Rs.
        // Feed L, C, and Ls constant +0.5; leave R, LFE, Rs at 0.
        let planar_in: Vec<Vec<f32>> = vec![
            vec![0.5; N_IN], // L
            vec![0.0; N_IN], // R
            vec![0.5; N_IN], // C
            vec![0.0; N_IN], // LFE
            vec![0.5; N_IN], // Ls
            vec![0.0; N_IN], // Rs
        ];
        let tj = TranscodeJson {
            sample_rate: Some(44_100),
            channels: Some(2),
            channel_map_preset: Some("5_1_to_stereo_bs775".to_string()),
            ..TranscodeJson::default()
        };
        let mut tc = PlanarAudioTranscoder::new(48_000, 6, &tj).unwrap();
        assert_eq!(tc.out_channels(), 2);
        assert_eq!(tc.out_sample_rate(), 44_100);
        let out = tc.process(&planar_in).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].len() > 3900 && out[0].len() < 4500);
        assert_eq!(out[0].len(), out[1].len());

        // BS.775: Lt = L + (-3 dB)·C + (-3 dB)·Ls = 0.5 + 0.707·0.5 + 0.707·0.5 ≈ 1.207
        //         Rt = R + (-3 dB)·C + (-3 dB)·Rs = 0 + 0.707·0.5 + 0 = 0.354
        // Check the steady-state mean after the filter warmup (skip 10 ms).
        let skip = 441;
        let body_l = &out[0][skip..];
        let body_r = &out[1][skip..];
        let mean_l: f64 = body_l.iter().map(|s| *s as f64).sum::<f64>() / body_l.len() as f64;
        let mean_r: f64 = body_r.iter().map(|s| *s as f64).sum::<f64>() / body_r.len() as f64;
        assert!(
            (mean_l - 1.207).abs() < 0.05,
            "Lt mean {mean_l:.4} not near 1.207 (downmix broken)"
        );
        assert!(
            (mean_r - 0.354).abs() < 0.05,
            "Rt mean {mean_r:.4} not near 0.354 (downmix broken)"
        );
        // Left must dominate — a channel-swap bug would fail this.
        assert!(mean_l > mean_r * 2.0, "expected Lt >> Rt for asymmetric input");
    }

    /// f32 samples outside `[-1.0, 1.0]` must clamp to the integer range,
    /// not wrap (which would flip sign at full scale). Regression guard for
    /// loud peak handling through `encode_pcm_be`.
    #[test]
    fn encode_pcm_be_clips_at_fullscale_without_wrap() {
        let planar: Vec<Vec<f32>> = vec![vec![2.0, -2.0, 1.5, -1.5]];
        let mut out = BytesMut::new();
        let mut rng = TpdfRng::new(7);
        encode_pcm_be(&planar, BitDepth::L16, Dither::None, &mut out, &mut rng);
        assert_eq!(out.len(), 4 * 2);
        let s0 = i16::from_be_bytes([out[0], out[1]]);
        let s1 = i16::from_be_bytes([out[2], out[3]]);
        let s2 = i16::from_be_bytes([out[4], out[5]]);
        let s3 = i16::from_be_bytes([out[6], out[7]]);
        assert_eq!(s0, i16::MAX, "positive clip must saturate to i16::MAX");
        assert_eq!(s1, i16::MIN, "negative clip must saturate to i16::MIN");
        assert_eq!(s2, i16::MAX, "1.5 must clamp to i16::MAX, not wrap");
        assert_eq!(s3, i16::MIN, "-1.5 must clamp to i16::MIN, not wrap");
    }

    /// Payload length not a multiple of `channels * wire_bytes` is a corrupt
    /// RTP packet and must fail cleanly rather than silently truncating.
    #[test]
    fn decode_pcm_be_rejects_unaligned_payload() {
        // 2 channels × L24 = 6 bytes per frame; 7 bytes is unaligned.
        let payload = vec![0u8; 7];
        let mut planar = vec![Vec::<f32>::new(); 2];
        let err = decode_pcm_be(&payload, BitDepth::L24, 2, &mut planar).unwrap_err();
        assert!(err.contains("not aligned"), "wrong error message: {err}");
    }
}
