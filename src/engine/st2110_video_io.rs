// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared runtime helpers for SMPTE ST 2110-20 and ST 2110-23 (uncompressed
//! video) input and output tasks.
//!
//! ## Ingress path (ST 2110-20 / -23 input)
//!
//! ```text
//!  UDP socket(s) ─► RedBluePair ─► Rfc4175Depacketizer ─► mpsc(raw frames)
//!                                                            │
//!                                       spawn_blocking: VideoEncoder
//!                                                            │
//!                                       NALUs ─► TsMuxer ─► RtpPacket
//!                                                            │
//!                                                   broadcast_tx
//! ```
//!
//! ## Egress path (ST 2110-20 / -23 output)
//!
//! ```text
//!  broadcast_rx ─► TsDemuxer ─► H264/HEVC access units ─► mpsc(NALUs)
//!                                                            │
//!                              spawn_blocking: VideoDecoder + VideoScaler
//!                                                            │
//!                              ScaledFrame ─► pack to pgroup ─► mpsc(frames)
//!                                                            │
//!                              Rfc4175Packetizer ─► UDP sendto Red + Blue
//! ```
//!
//! Heavy codec work (encode / decode / scale) always runs inside
//! `tokio::task::spawn_blocking`. Bounded mpsc channels provide back-pressure
//! with drop-on-lag semantics — the tokio reactor is never blocked and
//! upstream inputs are never held up by slow outputs.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::models::{
    EgressPacingMode, St2110VideoInputConfig, St2110VideoOutputConfig, St2110VideoPixelFormat,
    St2110_21ProfileConfig, St2110_23InputConfig, St2110_23OutputConfig,
    St2110_23PartitionModeConfig, VideoEncodeConfig, WirePacingConfig,
};
use crate::engine::packet::RtpPacket;
use crate::engine::rtmp::ts_mux::TsMuxer;
use crate::engine::st2110::pacer::{St2110_21Pacer, St2110_21Profile};
use crate::engine::st2110::redblue::RedBluePair;
use crate::engine::st2110::video::{
    pack_yuv422_8bit, partition_frame, unpack_yuv422_10bit,
    unpack_yuv422_8bit, DepacketizeOutcome, PacketizerConfig, PgroupFormat, RawVideoFrame,
    Rfc4175Depacketizer, Rfc4175MultiStreamReassembler, Rfc4175Packetizer,
    St2110_23PartitionMode, VideoField,
};
use crate::engine::wire_emit::{AnchorSource, WireDatagram, WirePacingClass, spawn_wire_emitter};
use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};
use crate::util::socket::{create_udp_output};

const INGRESS_FRAME_QUEUE: usize = 4;
const EGRESS_NALUS_QUEUE: usize = 16;
const EGRESS_FRAME_QUEUE: usize = 4;
const MAX_DGRAM: usize = 10_000; // jumbo-safe: 10 Gbps-class NICs often use 9000 MTU

fn pgroup_for(fmt: St2110VideoPixelFormat) -> PgroupFormat {
    match fmt {
        St2110VideoPixelFormat::Yuv422_8bit => PgroupFormat::Yuv422_8bit,
        St2110VideoPixelFormat::Yuv422_10bit => PgroupFormat::Yuv422_10bit,
    }
}

/// Compute the ST 2110-21 pacer parameters for a -20 output and
/// return a fresh pacer. Frame period derives from
/// `frame_rate_num / frame_rate_den`; packets-per-frame is
/// `ceil(frame_bytes / payload_budget)` where `frame_bytes` is the
/// pgroup-packed video bytes for the given format and resolution
/// (RFC 4175 triplet header overhead is negligible at the per-frame
/// scale and ignored — the pacer's even-pacing math already absorbs
/// a few percent of slack).
fn build_pacer(config: &St2110VideoOutputConfig, profile: St2110_21ProfileConfig) -> St2110_21Pacer {
    let pgroup_fmt = pgroup_for(config.pixel_format);
    let pgroup_bytes = pgroup_fmt.pgroup_bytes() as u64;
    let pgroup_pixels = pgroup_fmt.pgroup_pixels() as u64;
    let frame_pixels = (config.width as u64).saturating_mul(config.height as u64);
    let frame_bytes = frame_pixels.saturating_mul(pgroup_bytes) / pgroup_pixels.max(1);
    let payload = (config.payload_budget as u64).max(1);
    let packets_per_frame =
        (frame_bytes.saturating_add(payload - 1) / payload).max(1) as u32;
    let frame_period_ns = 1_000_000_000u64
        .saturating_mul(config.frame_rate_den as u64)
        / (config.frame_rate_num as u64).max(1);
    let pacer_profile = match profile {
        St2110_21ProfileConfig::Narrow => St2110_21Profile::Narrow,
        St2110_21ProfileConfig::NarrowLinear => St2110_21Profile::NarrowLinear,
        St2110_21ProfileConfig::Wide => St2110_21Profile::Wide,
    };
    St2110_21Pacer::new(frame_period_ns, packets_per_frame, pacer_profile)
}

/// Blocking-thread packetiser for ST 2110-20 outputs. Always paced.
///
/// Drains `frame_rx`, runs the RFC 4175 packetiser, computes a per-packet
/// `target_tx_time_ns` against the active raster via [`St2110_21Pacer`],
/// and dispatches each datagram to the supplied wire-emit channel(s).
/// Each wire-emit instance owns one std blocking UDP socket and selects
/// SO_TXTIME or `clock_nanosleep` based on its capability probe.
///
/// At ST 2110-20/-23 packet rates (~250k+ pps), only the SO_TXTIME tier
/// can keep up — the clock_nanosleep fallback is in tree for parity but
/// will saturate a core well before the frame rate hits 1080p50.
/// Operators see the active tier in the per-output stats
/// (`wire_pacing_tier`) and the startup log.
fn run_paced_sender(
    id: String,
    pacer: Arc<St2110_21Pacer>,
    packetizer: &mut Rfc4175Packetizer,
    mut frame_rx: mpsc::Receiver<RawVideoFrame>,
    cancel: CancellationToken,
    wire_red: crate::engine::wire_emit::WireTxHandle,
    wire_blue: Option<crate::engine::wire_emit::WireTxHandle>,
    stats: Arc<OutputStatsAccumulator>,
) {
    let _ = id;
    let mut frame_index: u64 = 0;
    while !cancel.is_cancelled() {
        let frame = match frame_rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };
        let mut out_pkts: Vec<Bytes> = Vec::with_capacity(16);
        packetizer.packetize(&frame, |pkt| out_pkts.push(pkt));
        let total_pkts = out_pkts.len() as u64;
        let mut total_bytes = 0u64;
        let pkts_in_frame = pacer.packets_per_frame();
        let recv_us = crate::util::time::now_us();
        for (idx, pkt) in out_pkts.iter().enumerate() {
            total_bytes += pkt.len() as u64;
            let pkt_idx = (idx as u32).min(pkts_in_frame.saturating_sub(1));
            let target_ns = pacer.target_for_packet(frame_index, pkt_idx);
            // Both legs receive the same Bytes (refcount bump only).
            let dg_red = WireDatagram {
                bytes: pkt.clone(),
                recv_time_us: recv_us,
                enqueue_us: 0,
                target_tx_time_ns: Some(target_ns),
                // ST 2110-20/-23 paths run under the St2110Raster
                // anchor, which ignores ts_offset.
                ts_offset: 0,
            };
            if wire_red.try_send(dg_red).is_err() {
                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(ref blue) = wire_blue {
                let dg_blue = WireDatagram {
                    bytes: pkt.clone(),
                    recv_time_us: recv_us,
                    enqueue_us: 0,
                    target_tx_time_ns: Some(target_ns),
                    ts_offset: 0,
                };
                let _ = blue.try_send(dg_blue);
            }
        }
        // packets_sent / bytes_sent are tracked inside wire_emit on
        // successful send. We track frame-level totals locally only
        // for diagnostics — but to avoid double-counting against the
        // wire_emit's own counters, we DON'T fetch_add stats here.
        let _ = total_pkts;
        let _ = total_bytes;
        frame_index = frame_index.wrapping_add(1);
    }
}

fn partition_mode_for(m: St2110_23PartitionModeConfig) -> St2110_23PartitionMode {
    match m {
        St2110_23PartitionModeConfig::TwoSampleInterleave => {
            St2110_23PartitionMode::TwoSampleInterleave
        }
        St2110_23PartitionModeConfig::SampleRow => St2110_23PartitionMode::SampleRow,
    }
}

// ── INGRESS ────────────────────────────────────────────────────────────────

/// Convert a configured `VideoEncodeConfig` to the codec crate's
/// `VideoEncoderConfig`. The codec field passes through the runtime
/// host-capability resolver so:
///
/// - `*_auto` strings (`h264_auto` / `hevc_auto`) resolve to the best
///   backend the host actually supports for the requested
///   `(chroma, bit_depth)`. Matches RTMP / WebRTC / TS replacer behaviour.
/// - Operator-pinned HW backends that the host probe finds incompatible
///   with the requested `(chroma, bit_depth)` (e.g. `hevc_vaapi` +
///   4:2:2 10-bit on an Intel iGPU whose iHD driver lacks
///   `VAProfileHEVCMain422_10`) fall back to the matching SW backend
///   (`x265` for HEVC, `x264` for H.264) and log a single
///   operator-facing `error_code = "encoder_chroma_not_supported"`
///   message. Previously this surfaced as an opaque FFmpeg `-22` at
///   first frame; now the operator gets the precise reason and the
///   flow keeps running on the SW path.
/// - Operator-pinned SW backends and HW backends that ARE supported
///   pass straight through, no behaviour change.
///
/// Advanced knobs (chroma / bit_depth / rate_control / CRF / bframes /
/// refs / level / tune / colour metadata) propagate through to the
/// ST 2110-20/-23 ingest encoder in lock-step with RTMP / WebRTC /
/// TS video replacer.
#[cfg(feature = "media-codecs")]
pub(crate) fn build_encoder_config(
    enc: &VideoEncodeConfig,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
) -> Result<video_codec::VideoEncoderConfig> {
    use crate::engine::hardware_probe::{
        resolve_for_video_encode_config, EncoderResolutionError, ResolvedVideoEncoder,
    };
    use video_codec::VideoEncoderCodec;
    let resolved_to_codec = |r: ResolvedVideoEncoder| match r {
        ResolvedVideoEncoder::X264 => VideoEncoderCodec::X264,
        ResolvedVideoEncoder::X265 => VideoEncoderCodec::X265,
        ResolvedVideoEncoder::H264Nvenc => VideoEncoderCodec::H264Nvenc,
        ResolvedVideoEncoder::HevcNvenc => VideoEncoderCodec::HevcNvenc,
        ResolvedVideoEncoder::H264Qsv => VideoEncoderCodec::H264Qsv,
        ResolvedVideoEncoder::HevcQsv => VideoEncoderCodec::HevcQsv,
        ResolvedVideoEncoder::H264Vaapi => VideoEncoderCodec::H264Vaapi,
        ResolvedVideoEncoder::HevcVaapi => VideoEncoderCodec::HevcVaapi,
    };
    // Try the host-capability resolver first. This unwraps `*_auto`
    // strings AND validates fixed backends against the runtime
    // (chroma, bit-depth) chroma matrix.
    let codec = match resolve_for_video_encode_config(enc) {
        Ok(resolved) => resolved_to_codec(resolved),
        Err(EncoderResolutionError::ChromaUnsupported {
            backend, chroma, bit_depth,
        }) => {
            // Fall back to the matching SW backend. SW is always
            // compiled into the bilbycast `video-encoders-full`
            // release artefact and accepts the broadcast contribution
            // matrix (4:2:0 / 4:2:2 / 4:4:4 × 8 / 10) on every host.
            // Pick the family from the original requested backend so
            // an HEVC ask doesn't silently demote to H.264.
            let fallback = match enc.codec.as_str() {
                "h264_vaapi" | "h264_qsv" | "h264_nvenc" => VideoEncoderCodec::X264,
                "hevc_vaapi" | "hevc_qsv" | "hevc_nvenc" => VideoEncoderCodec::X265,
                "h264_auto" => VideoEncoderCodec::X264,
                "hevc_auto" => VideoEncoderCodec::X265,
                _ => VideoEncoderCodec::X265,
            };
            tracing::warn!(
                error_code = "encoder_chroma_not_supported",
                requested = %enc.codec,
                backend = %backend,
                chroma = %chroma,
                bit_depth = bit_depth,
                fallback = ?fallback,
                "ST 2110-20 input: host's {backend} doesn't support \
                 ({chroma}, {bit_depth}-bit) — falling back to {fallback:?}. \
                 Configure `video_encode.codec` explicitly to suppress \
                 this warning, or use `hevc_auto`/`h264_auto` to let the \
                 resolver pick the cheapest supported backend per host."
            );
            fallback
        }
        Err(e) => return Err(anyhow!("ST 2110-20 input: {}", e.message())),
    };
    Ok(crate::engine::video_encode_util::build_encoder_config(
        enc, codec, width, height, fps_num, fps_den, false,
    ))
}

/// Register an ST 2110-20/-23 ingress encoder's stats handle and static
/// ingress summary descriptor on the flow accumulator, keyed by `input_id`,
/// so the manager UI can surface this input's live encode target and
/// per-frame counters (and stop reporting them after a switch to another
/// input).
fn register_ingress_video_encode_stats(
    stats: &Arc<FlowStatsAccumulator>,
    input_id: &str,
    enc: &VideoEncodeConfig,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
) -> Arc<crate::engine::ts_video_replace::VideoEncodeStats> {
    let handle = Arc::new(crate::engine::ts_video_replace::VideoEncodeStats::default());
    let backend = match enc.codec.as_str() {
        "x264" | "x265" => enc.codec.clone(),
        "h264_nvenc" | "hevc_nvenc" => "nvenc".to_string(),
        other => other.to_string(),
    };
    let target_codec = match enc.codec.as_str() {
        "x264" | "h264_nvenc" => "h264",
        "x265" | "hevc_nvenc" => "hevc",
        other => other,
    };
    let fps = if fps_den > 0 {
        fps_num as f32 / fps_den as f32
    } else {
        0.0
    };
    stats.set_input_video_encode_stats(
        input_id,
        handle.clone(),
        "raw".to_string(),
        target_codec.to_string(),
        width,
        height,
        fps,
        enc.bitrate_kbps.unwrap_or(0),
        backend,
    );
    stats.set_ingress_static(
        input_id,
        crate::stats::collector::EgressMediaSummaryStatic {
            transport_mode: Some("ts".to_string()),
            video_passthrough: false,
            audio_passthrough: true,
            audio_only: false,
            ..Default::default()
        },
    );
    handle
}

/// Run the ST 2110-20 input pipeline. Blocks forever until cancel.
pub async fn run_st2110_20_input(
    config: St2110VideoInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> Result<()> {
    let fmt = pgroup_for(config.pixel_format);
    let pair = RedBluePair::bind_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
        config.redundancy.as_ref(),
        config.interface_binding.as_ref(),
    )
    .await?;

    if pair.blue.is_some() {
        let _ = stats.red_blue_stats.set(pair.stats.clone());
    }

    // Channel from depacketizer → encode worker.
    let (frame_tx, frame_rx) = mpsc::channel::<RawVideoFrame>(INGRESS_FRAME_QUEUE);

    // Spawn blocking encode worker.
    let enc_cfg = config.video_encode.clone();
    let width = config.width;
    let height = config.height;
    let fps_num = config.frame_rate_num;
    let fps_den = config.frame_rate_den;
    let encode_stats = register_ingress_video_encode_stats(&stats, &input_id, &enc_cfg, width, height, fps_num, fps_den);
    let tx_for_worker = broadcast_tx.clone();
    let stats_for_worker = stats.clone();
    let worker_cancel = cancel.clone();
    let pid_overrides = config.pid_overrides.clone();
    let _enc_handle = tokio::task::spawn_blocking(move || {
        encode_worker(
            enc_cfg,
            width,
            height,
            fps_num,
            fps_den,
            fmt,
            frame_rx,
            tx_for_worker,
            stats_for_worker,
            encode_stats,
            worker_cancel,
            pid_overrides,
        );
    });

    // Single-leg Linux ingest runs on a dedicated recvmmsg thread —
    // ST 2110-20 packet rates (146 kpps at 1080p50, 580 kpps at 2160p50)
    // exceed what the per-packet tokio recv loop drains, and the excess
    // is dropped in-kernel at the socket buffer. Dual-leg (2022-7)
    // keeps the select-based recv_loop below.
    #[cfg(target_os = "linux")]
    if pair.blue.is_none() {
        let mut depkr =
            Rfc4175Depacketizer::new(config.width, config.height, fmt, config.payload_type);
        let stats_rl = stats.clone();
        let handle = pair.spawn_dedicated_single_leg_loop(
            cancel.clone(),
            move |payload: &[u8], _leg, _seq| -> bool {
                match depkr.feed(payload) {
                    Ok(DepacketizeOutcome::Frame(frame)) => {
                        let _ = frame_tx.try_send(frame);
                    }
                    Ok(DepacketizeOutcome::Continue) => {}
                    Ok(DepacketizeOutcome::Dropped { reason, dropped_bytes }) => {
                        tracing::debug!(reason, dropped_bytes, "ST 2110-20 input dropped partial frame");
                        stats_rl.input_filtered.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::debug!(error=?e, "ST 2110-20 input RFC 4175 parse error");
                        stats_rl.input_filtered.fetch_add(1, Ordering::Relaxed);
                    }
                }
                true
            },
        )?;
        let _ = handle.await;
        return Ok(());
    }

    // Depacketizer runs on the tokio reactor.
    let mut depkr = Rfc4175Depacketizer::new(config.width, config.height, fmt, config.payload_type);
    let cancel_loop = cancel.clone();
    let stats_rl = stats.clone();
    pair.recv_loop(cancel_loop, move |payload: Bytes, _leg, _seq| -> bool {
        match depkr.feed(&payload) {
            Ok(DepacketizeOutcome::Frame(frame)) => {
                let _ = frame_tx.try_send(frame);
            }
            Ok(DepacketizeOutcome::Continue) => {}
            Ok(DepacketizeOutcome::Dropped { reason, dropped_bytes }) => {
                tracing::debug!(reason, dropped_bytes, "ST 2110-20 input dropped partial frame");
                stats_rl.input_filtered.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::debug!(error=?e, "ST 2110-20 input RFC 4175 parse error");
                stats_rl.input_filtered.fetch_add(1, Ordering::Relaxed);
            }
        }
        true
    })
    .await;

    Ok(())
}

#[cfg(feature = "media-codecs")]
fn encode_worker(
    enc: VideoEncodeConfig,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    fmt: PgroupFormat,
    mut frame_rx: mpsc::Receiver<RawVideoFrame>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    encode_stats: Arc<crate::engine::ts_video_replace::VideoEncodeStats>,
    cancel: CancellationToken,
    pid_overrides: Option<crate::config::models::TsPidOverridesMap>,
) {
    // Build an encoder config once up-front — used for chroma/bit-depth
    // decisions below and to spot backend-not-compiled-in errors
    // synchronously. The shared `ScaledVideoEncoder` pipeline built
    // right after it owns the real, lazy-opened encoder and — when the
    // operator's `video_encode.width` / `.height` differ from the raw
    // RFC 4175 frame dims — transparently wires a `VideoScaler` between
    // plane scratch buffers and the encoder.
    let cfg = match build_encoder_config(&enc, width, height, fps_num, fps_den) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "ST 2110-20 input: failed to build encoder config");
            return;
        }
    };
    let mut pipeline = crate::engine::video_encode_util::ScaledVideoEncoder::new(
        enc.clone(),
        cfg.codec,
        fps_num,
        fps_den,
        false,
        "ST 2110-20 input".to_string(),
    );
    let mut ts_mux = TsMuxer::new();
    if let Some(po) = pid_overrides.as_ref() {
        if let Some(entry) = po.get(&1) {
                ts_mux.set_pids(entry.pmt_pid, entry.video_pid, entry.audio_pid, entry.pcr_pid);
            }
    }
    let mut pts: i64 = 0;
    let mut last_rtp_ts: Option<u32> = None;

    // Target chroma / bit depth chosen by the operator via video_encode
    // (defaults: 4:2:0 8-bit for parity with pre-Phase-4 behaviour). When
    // the target matches the source (4:2:2 + 10-bit), we skip the
    // subsample / depth conversion entirely — the encoder handles native
    // 4:2:2 10-bit planes.
    let target_chroma = cfg.chroma;
    let target_10bit = cfg.bit_depth == 10;
    let target_bit_depth: u8 = if target_10bit { 10 } else { 8 };

    // AVPixelFormat describing the packed scratch planes we hand to the
    // pipeline. The chroma-subsample / bit-depth conversion below
    // normalises every source combination into one of these four
    // layouts. `None` (e.g. 4:4:4) falls back to no-scale encode below.
    let src_pix_fmt = video_engine::av_pix_fmt_for_yuv(target_chroma, target_bit_depth);

    // Scratch buffers for chroma-subsample / bit-depth conversion.
    // Long-lived — resized in-place each frame via `resize()` (only
    // reallocates when capacity is insufficient) so steady-state runs
    // produce zero allocations on the encode path for these. Replaces the
    // per-frame `vec![0u8; ...]` allocations that used to appear on every
    // 10-bit frame. The 8-bit Y plane still comes from
    // `unpack_yuv422_8bit`, which allocates its own Vec; we move that in.
    let mut cb_scratch: Vec<u8> = Vec::new();
    let mut cr_scratch: Vec<u8> = Vec::new();
    let mut y_bytes_scratch: Vec<u8> = Vec::new();
    // Holds the Y plane for the 8-bit path (owned by unpack result, moved).
    #[allow(unused_assignments)]
    let mut y_scratch: Vec<u8> = Vec::new();

    while !cancel.is_cancelled() {
        let frame = match frame_rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        // Downsample source chroma rows from 4:2:2 to 4:2:0. No-op when
        // the operator asked for 4:2:2 output — we keep the full chroma
        // resolution.
        let downsample_rows = matches!(target_chroma, video_codec::VideoChroma::Yuv420);

        // Compute encoded-frame plane dimensions up-front based on target
        // chroma + bit depth.
        let w = frame.width as usize;
        let h = frame.height as usize;
        let enc_y_stride: usize;
        let enc_c_stride: usize;
        let enc_c_rows: usize;
        let y_bytes: &[u8];
        let u_bytes: &[u8];
        let v_bytes: &[u8];

        match fmt {
            PgroupFormat::Yuv422_8bit => {
                // 8-bit source. Regardless of target bit depth we feed 8
                // bits (no cheap up-conversion; 10-bit target with an
                // 8-bit source is effectively lossless-passthrough from
                // the encoder's perspective).
                let (src_y, src_cb, src_cr) =
                    unpack_yuv422_8bit(&frame.pixels, frame.width, frame.height);
                let cw = w / 2;
                if downsample_rows {
                    let ch = h / 2;
                    cb_scratch.resize(cw * ch, 0);
                    cr_scratch.resize(cw * ch, 0);
                    for row in 0..ch {
                        let dst = row * cw;
                        let src = row * 2 * cw;
                        cb_scratch[dst..dst + cw].copy_from_slice(&src_cb[src..src + cw]);
                        cr_scratch[dst..dst + cw].copy_from_slice(&src_cr[src..src + cw]);
                    }
                    y_scratch = src_y;
                    enc_c_rows = ch;
                } else {
                    cb_scratch = src_cb;
                    cr_scratch = src_cr;
                    y_scratch = src_y;
                    enc_c_rows = h;
                }
                enc_y_stride = w;
                enc_c_stride = cw;
                y_bytes = &y_scratch;
                u_bytes = &cb_scratch;
                v_bytes = &cr_scratch;
            }
            PgroupFormat::Yuv422_10bit => {
                // Fused unpack + convert — one pass from pgroup bytes
                // straight into the encoder scratch planes. See
                // unpack_yuv422_10bit_into_planes for why the previous
                // two-step path was too slow for 2160p50.
                let cw = w / 2;
                let bps = if target_10bit { 2 } else { 1 };
                let c_rows_out = if downsample_rows { h / 2 } else { h };
                crate::engine::st2110::video::unpack_yuv422_10bit_into_planes(
                    &frame.pixels,
                    frame.width,
                    frame.height,
                    target_10bit,
                    downsample_rows,
                    &mut y_bytes_scratch,
                    &mut cb_scratch,
                    &mut cr_scratch,
                );
                enc_y_stride = w * bps;
                enc_c_stride = cw * bps;
                enc_c_rows = c_rows_out;
                y_bytes = &y_bytes_scratch;
                u_bytes = &cb_scratch;
                v_bytes = &cr_scratch;
            }
        }

        let _ = enc_c_rows; // documented — the encoder computes rows itself from pix_fmt
        encode_stats.input_frames.fetch_add(1, Ordering::Relaxed);

        // Route packed planes through the shared pipeline. When
        // `src_pix_fmt` is `None` (e.g. 4:4:4 target) the pipeline
        // can't plumb a scaler for this pixel format, so we force
        // matching src=dst and skip scaling entirely — giving the
        // pre-Phase-4 behaviour of passing planes verbatim.
        let (src_w_hint, src_h_hint, src_fmt_hint) = match src_pix_fmt {
            Some(fmt) => (frame.width, frame.height, fmt),
            None => (
                enc.width.unwrap_or(frame.width),
                enc.height.unwrap_or(frame.height),
                0i32,
            ),
        };
        // Advance the PES timeline by the RTP-timestamp delta (90 kHz,
        // RFC 4175 §5.1) so encoded PES PTS reflects real sender time —
        // frame-index stepping (`pts += 1`) emits PES PTS 1 tick apart,
        // which no downstream decoder can pace. Wrapping diff keeps the
        // timeline monotonic across the u32 RTP wrap (~13.2 h); a
        // non-positive delta (sender restart) falls back to one nominal
        // frame duration.
        if let Some(prev) = last_rtp_ts {
            let delta = frame.pts_90k.wrapping_sub(prev) as i32;
            if delta > 0 {
                pts += delta as i64;
            } else {
                pts += (90_000i64 * fps_den.max(1) as i64) / fps_num.max(1) as i64;
            }
        }
        last_rtp_ts = Some(frame.pts_90k);
        let enc_out = pipeline.encode_raw_planes(
            src_w_hint,
            src_h_hint,
            src_fmt_hint,
            y_bytes,
            enc_y_stride,
            u_bytes,
            enc_c_stride,
            v_bytes,
            enc_c_stride,
            Some(pts),
        );
        let frames = match enc_out {
            Ok(f) => f,
            Err(e) => {
                if !pipeline.is_open() {
                    tracing::error!(error = %e, "ST 2110-20 input: encoder open failed");
                    return;
                }
                tracing::warn!(error = %e, "ST 2110-20 input: encoder error");
                encode_stats.dropped_frames.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        for ef in frames {
            encode_stats.output_frames.fetch_add(1, Ordering::Relaxed);
            let ts_packets = ts_mux.mux_video(&ef.data, ef.pts as u64, ef.dts as u64, ef.keyframe);
            for ts in ts_packets {
                let ts_len = ts.len() as u64;
                // `ts` is `Bytes` (from TsMuxer) — `Bytes::clone()` is a
                // refcount bump, not a memcpy. Keep that semantics
                // explicit so future refactors don't accidentally
                // reintroduce a deep clone here.
                let pkt = RtpPacket {
                    data: ts,
                    sequence_number: 0,
                    rtp_timestamp: ef.pts as u32,
                    recv_time_us: crate::util::time::now_us(),
                    is_raw_ts: true,
                    upstream_seq: None,
                    upstream_leg_id: None,
                    sender_timestamp_us: None,
                };
                stats.input_packets.fetch_add(1, Ordering::Relaxed);
                stats.input_bytes.fetch_add(ts_len, Ordering::Relaxed);
                let _ = broadcast_tx.send(pkt);
            }
        }
    }
}

#[cfg(not(feature = "media-codecs"))]
fn encode_worker(
    _enc: VideoEncodeConfig,
    _width: u32,
    _height: u32,
    _fps_num: u32,
    _fps_den: u32,
    _fmt: PgroupFormat,
    _frame_rx: mpsc::Receiver<RawVideoFrame>,
    _broadcast_tx: broadcast::Sender<RtpPacket>,
    _stats: Arc<FlowStatsAccumulator>,
    _encode_stats: Arc<crate::engine::ts_video_replace::VideoEncodeStats>,
    _cancel: CancellationToken,
    _pid_overrides: Option<crate::config::models::TsPidOverridesMap>,
) {
    tracing::error!(
        "ST 2110-20 input requires the media-codecs and a video-encoder-* feature \
         to be compiled in; this build has media-codecs disabled"
    );
}

// ── EGRESS ─────────────────────────────────────────────────────────────────

pub async fn run_st2110_20_output(
    config: St2110VideoOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> Result<()> {
    let fmt = pgroup_for(config.pixel_format);
    let dest_red: SocketAddr = config
        .dest_addr
        .parse()
        .map_err(|e| anyhow!("ST 2110-20 output dest_addr parse: {e}"))?;
    let dest_blue: Option<SocketAddr> = match &config.redundancy {
        Some(r) => Some(
            r.addr
                .parse()
                .map_err(|e| anyhow!("ST 2110-20 output redundancy addr parse: {e}"))?,
        ),
        None => None,
    };
    let (red_sock, _red_dest_from_fn) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
        config.interface_binding.as_ref(),
    )
    .await?;
    let blue_sock = if let Some(r) = &config.redundancy {
        let (s, _) = create_udp_output(
            &r.addr,
            config.bind_addr.as_deref(),
            r.interface_addr.as_deref(),
            config.dscp,
            config.interface_binding.as_ref(),
        )
        .await?;
        Some(s)
    } else {
        None
    };

    let (frame_tx, frame_rx) = mpsc::channel::<RawVideoFrame>(EGRESS_FRAME_QUEUE);
    let (nalu_tx, nalu_rx) = mpsc::channel::<DemuxedFrame>(EGRESS_NALUS_QUEUE);

    let width = config.width;
    let height = config.height;
    let worker_cancel = cancel.clone();
    let _decode_handle = tokio::task::spawn_blocking(move || {
        decode_worker(width, height, fmt, nalu_rx, frame_tx, worker_cancel);
    });

    let cfg = PacketizerConfig {
        payload_budget: config.payload_budget,
        payload_type: config.payload_type,
        ssrc: config.ssrc.unwrap_or_else(rand::random),
    };
    let mut packetizer = Rfc4175Packetizer::new(cfg);

    // ── Wire egress: PCR/raster-paced via wire_emit ──────────────────
    //
    // Every ST 2110-20 output is paced unconditionally. The
    // `St2110_21Pacer` computes `target_tx_time_ns` per RFC 4175
    // packet against the active video frame raster (anchored on
    // `CLOCK_TAI` when `ptp4l` is running). `wire_emit` selects
    // SO_TXTIME on Linux ≥ 4.19 with permitted setsockopt or
    // `clock_nanosleep` fallback otherwise; tier is logged at
    // startup and surfaced in `OutputStats.wire_pacing_tier`.
    //
    // The deprecated `wire_pacing` config field is ignored at runtime
    // — kept in the schema for backward compatibility with stored
    // configs.
    #[allow(deprecated)]
    let wire_pacing_was_set = config.wire_pacing.is_some();
    if wire_pacing_was_set {
        tracing::warn!(
            "ST 2110-20 output '{}': wire_pacing config field is deprecated and ignored \
             (pacing is now automatic — see docs/wire-pacing.md)",
            config.id
        );
    }
    #[allow(deprecated)]
    let profile = match config.wire_pacing.as_ref() {
        Some(WirePacingConfig::TxTime { profile }) => *profile,
        None => St2110_21ProfileConfig::NarrowLinear,
    };
    let pacer = Arc::new(build_pacer(&config, profile));

    let red_std = red_sock
        .into_std()
        .map_err(|e| anyhow!("ST 2110-20 output '{}' red into_std: {e}", config.id))?;
    red_std.set_nonblocking(false).ok();
    let wire_red = spawn_wire_emitter(
        format!("{}-red", config.id),
        red_std,
        dest_red,
        AnchorSource::St2110Raster,
        WirePacingClass::EtfEligible,
        EgressPacingMode::Forward,
        None,
        stats.clone(),
        cancel.clone(),
    );
    let wire_blue = match (blue_sock, dest_blue) {
        (Some(s), Some(addr)) => {
            let blue_std = s
                .into_std()
                .map_err(|e| anyhow!("ST 2110-20 output '{}' blue into_std: {e}", config.id))?;
            blue_std.set_nonblocking(false).ok();
            // Leg 2 stats are private — never surfaced — to keep the
            // observable counters single-leg.
            let leg2_stats = Arc::new(OutputStatsAccumulator::new(
                format!("{}-blue", config.id),
                format!("{}-blue", config.id),
                "st2110_20_blue".to_string(),
            ));
            Some(spawn_wire_emitter(
                format!("{}-blue", config.id),
                blue_std,
                addr,
                AnchorSource::St2110Raster,
                WirePacingClass::EtfEligible,
                EgressPacingMode::Forward,
                None,
                leg2_stats,
                cancel.clone(),
            ))
        }
        _ => None,
    };

    let sender_id = config.id.clone();
    let sender_cancel = cancel.clone();
    let sender_stats = stats.clone();
    let sender_handle = tokio::task::spawn_blocking(move || {
        run_paced_sender(
            sender_id,
            pacer,
            &mut packetizer,
            frame_rx,
            sender_cancel,
            wire_red,
            wire_blue,
            sender_stats,
        );
    });

    // Feeder task: drain broadcast_rx, demux TS, forward H.264/HEVC frames.
    let mut ts_demuxer = TsDemuxer::new(None);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            rec = rx.recv() => {
                match rec {
                    Ok(packet) => {
                        let ts_payload: &[u8] = if packet.is_raw_ts {
                            &packet.data
                        } else if packet.data.len() >= 12 {
                            &packet.data[12..]
                        } else {
                            continue;
                        };
                        for frame in ts_demuxer.demux(ts_payload) {
                            let _ = nalu_tx.try_send(frame);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    drop(nalu_tx);
    let _ = sender_handle.await;
    Ok(())
}

#[cfg(feature = "media-codecs")]
fn decode_worker(
    width: u32,
    height: u32,
    fmt: PgroupFormat,
    mut nalu_rx: mpsc::Receiver<DemuxedFrame>,
    frame_tx: mpsc::Sender<RawVideoFrame>,
    cancel: CancellationToken,
) {
    use video_codec::{ScalerDstFormat, VideoCodec};
    use video_engine::{VideoDecoder, VideoScaler};

    let dst_fmt = match fmt {
        PgroupFormat::Yuv422_8bit => ScalerDstFormat::Yuv422p8,
        PgroupFormat::Yuv422_10bit => ScalerDstFormat::Yuv422p10le,
    };

    let mut current_codec: Option<VideoCodec> = None;
    let mut decoder: Option<VideoDecoder> = None;
    let mut scaler: Option<VideoScaler> = None;
    // Monotonic fallback when the source omits PTS (very rare on TS
    // ingress — `media-codecs` decoder always supplies one). Bumped
    // per emitted frame so receivers never see a backwards step.
    let mut synth_pts: i64 = 0;

    // Dedicated pack stage. pgroup packing costs 6–10 ms per 2160p
    // frame, and decode-pull + scale alone consume most of the 20 ms
    // frame budget at 2160p50 — serialized they cap egress below the
    // raster rate. `ScaledFrame` owns its AVFrame and is `Send`, so the
    // handoff is zero-copy; a single pack thread preserves frame order.
    // `sync_channel(3)` bounds memory (each entry holds one full
    // uncompressed frame); when packing falls behind, the frame is
    // dropped at `try_send` exactly as the old in-line path dropped at
    // the `frame_tx` queue.
    let (pack_tx, pack_rx) =
        std::sync::mpsc::sync_channel::<(video_engine::ScaledFrame, i64)>(3);
    let pack_fmt = fmt;
    let pack_handle = std::thread::Builder::new()
        .name("st2110-pack".into())
        .spawn(move || {
            while let Ok((scaled, frame_pts)) = pack_rx.recv() {
                let pixels = match pack_fmt {
                    PgroupFormat::Yuv422_8bit => {
                        let (y, ys) = scaled.plane(0).unwrap();
                        let (cb, cs) = scaled.plane(1).unwrap();
                        let (cr, _) = scaled.plane(2).unwrap();
                        pack_yuv422_8bit(y, ys, cb, cs, cr, cs, width, height)
                    }
                    PgroupFormat::Yuv422_10bit => {
                        // Planes are 16-bit little-endian; pack pgroups
                        // straight from the byte planes (no intermediate
                        // u16 temporaries — see pack_yuv422_10bit_le_bytes).
                        let (y_b, ys) = scaled.plane(0).unwrap();
                        let (cb_b, cs) = scaled.plane(1).unwrap();
                        let (cr_b, _) = scaled.plane(2).unwrap();
                        crate::engine::st2110::video::pack_yuv422_10bit_le_bytes(
                            y_b, ys, cb_b, cs, cr_b, cs, width, height,
                        )
                    }
                };
                let raw = RawVideoFrame {
                    pixels,
                    width,
                    height,
                    format: pack_fmt,
                    pts_90k: frame_pts as u32,
                    field: VideoField::Progressive,
                };
                let _ = frame_tx.try_send(raw);
            }
        })
        .expect("spawn st2110-pack thread");

    while !cancel.is_cancelled() {
        let frame = match nalu_rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };
        let (nalus, is_h264, pts) = match frame {
            DemuxedFrame::H264 { nalus, pts, .. } => (nalus, true, pts),
            DemuxedFrame::H265 { nalus, pts, .. } => (nalus, false, pts),
            _ => continue,
        };
        let codec = if is_h264 { VideoCodec::H264 } else { VideoCodec::Hevc };
        if current_codec != Some(codec) {
            current_codec = Some(codec);
            // Auto-threaded decode: UHD HEVC at 50 fps exceeds what a
            // single libavcodec thread sustains. The frames-deep
            // pipeline delay is a constant offset, absorbed by the
            // receiver's VRX buffer — throughput is what matters here.
            decoder = Some(match VideoDecoder::open_threaded(codec) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(error = %e, "ST 2110-20 output decoder open failed");
                    continue;
                }
            });
            scaler = None;
        }
        let dec = decoder.as_mut().unwrap();

        // Feed NALUs as annex-B to decoder. Attach the source PES PTS
        // so the decoder propagates it (in presentation order, after
        // B-frame reorder) onto each output frame's `pts()`. Without
        // this, the RFC 4175 wire timestamp would inherit the
        // most-recently-sent packet's decode-order PTS — non-monotonic
        // on streams with B-frames, which RFC 4175 § 4.1 forbids.
        let mut nalu_bytes = Vec::new();
        for nalu in nalus {
            nalu_bytes.extend_from_slice(&[0, 0, 0, 1]);
            nalu_bytes.extend_from_slice(&nalu);
        }
        // `DemuxedFrame::H264::pts` is u64 (TS PES PTS in 90 kHz, 33-bit).
        // libavcodec wants i64; the high bit is always 0 for a 33-bit
        // value, so a plain cast is safe.
        if let Err(e) = dec.send_packet_with_pts(&nalu_bytes, pts as i64) {
            tracing::debug!(error = %e, "ST 2110-20 output decoder send_packet error");
            continue;
        }

        loop {
            let dec_frame = match dec.receive_frame() {
                Ok(f) => f,
                Err(_) => break,
            };
            let src_w = dec_frame.width();
            let src_h = dec_frame.height();
            let src_pix_fmt = dec_frame.pixel_format();
            // Pull the PRESENTATION-order PTS that libavcodec attached
            // after the decoder's B-frame reorder. Falls back to a
            // monotonic synthetic anchor only if the source genuinely
            // omitted PTS (very rare — `media-codecs` always supplies
            // it in our pipeline). The fallback uses a per-decoder
            // counter so receivers still see monotonic timestamps.
            let frame_pts = dec_frame.pts().unwrap_or_else(|| {
                synth_pts += 1;
                synth_pts
            });
            if scaler.is_none() {
                scaler = Some(
                    match VideoScaler::new_with_dst_format(
                        src_w, src_h, src_pix_fmt, width, height, dst_fmt,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(error = %e, "ST 2110-20 output scaler init failed");
                            break;
                        }
                    },
                );
            }
            let s = scaler.as_ref().unwrap();
            let scaled = match s.scale(&dec_frame) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "ST 2110-20 output scale error");
                    continue;
                }
            };
            // Hand off to the pack thread (zero-copy — ScaledFrame owns
            // its AVFrame). Drop the frame when packing is behind.
            let _ = pack_tx.try_send((scaled, frame_pts));
        }
    }

    drop(pack_tx);
    let _ = pack_handle.join();
}

#[cfg(not(feature = "media-codecs"))]
fn decode_worker(
    _width: u32,
    _height: u32,
    _fmt: PgroupFormat,
    _nalu_rx: mpsc::Receiver<DemuxedFrame>,
    _frame_tx: mpsc::Sender<RawVideoFrame>,
    _cancel: CancellationToken,
) {
    tracing::error!(
        "ST 2110-20 output requires the media-codecs feature to be compiled in; \
         this build has media-codecs disabled"
    );
}

#[cfg(feature = "media-codecs")]
fn bytes_le_to_u16(buf: &[u8], stride: usize, width: usize, height: usize) -> Vec<u16> {
    let mut out = vec![0u16; width * height];
    for row in 0..height {
        let src = &buf[row * stride..row * stride + width * 2];
        for x in 0..width {
            out[row * width + x] = u16::from_le_bytes([src[x * 2], src[x * 2 + 1]]);
        }
    }
    out
}

// ── ST 2110-23: thin wrappers that reuse -20 encode/decode ────────────────

pub async fn run_st2110_23_input(
    config: St2110_23InputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
) -> Result<()> {
    let fmt = pgroup_for(config.pixel_format);
    let mode = partition_mode_for(config.partition_mode);
    let n = config.sub_streams.len() as u32;

    let (frame_tx, frame_rx) = mpsc::channel::<RawVideoFrame>(INGRESS_FRAME_QUEUE);

    let enc_cfg = config.video_encode.clone();
    let width = config.width;
    let height = config.height;
    let fps_num = config.frame_rate_num;
    let fps_den = config.frame_rate_den;
    let encode_stats = register_ingress_video_encode_stats(&stats, &input_id, &enc_cfg, width, height, fps_num, fps_den);
    let tx_for_worker = broadcast_tx.clone();
    let stats_for_worker = stats.clone();
    let worker_cancel = cancel.clone();
    let pid_overrides = config.pid_overrides.clone();
    let _enc_handle = tokio::task::spawn_blocking(move || {
        encode_worker(
            enc_cfg,
            width,
            height,
            fps_num,
            fps_den,
            fmt,
            frame_rx,
            tx_for_worker,
            stats_for_worker,
            encode_stats,
            worker_cancel,
            pid_overrides,
        );
    });

    // One per-sub-stream (sub_index, RawVideoFrame) mpsc into a single reassembler task.
    let (sub_frame_tx, mut sub_frame_rx) = mpsc::channel::<(u32, RawVideoFrame)>(INGRESS_FRAME_QUEUE * n as usize);

    // Reassembler task.
    let reassembler_tx = frame_tx.clone();
    let reassembler_cancel = cancel.clone();
    let _reassembler_handle = tokio::spawn(async move {
        let mut reassembler = Rfc4175MultiStreamReassembler::new(n, mode);
        while !reassembler_cancel.is_cancelled() {
            let (idx, f) = tokio::select! {
                _ = reassembler_cancel.cancelled() => break,
                v = sub_frame_rx.recv() => match v { Some(v) => v, None => break },
            };
            if let Some(full) = reassembler.feed(idx, f) {
                let _ = reassembler_tx.try_send(full);
            }
        }
    });

    // Spawn one receiver task per sub-stream.
    let mut handles = Vec::new();
    let parent_binding = config.interface_binding.clone();
    for (idx, sub) in config.sub_streams.into_iter().enumerate() {
        let pair = RedBluePair::bind_input(
            &sub.bind_addr,
            sub.interface_addr.as_deref(),
            sub.source_addr.as_deref(),
            sub.redundancy.as_ref(),
            parent_binding.as_ref(),
        )
        .await?;
        let sub_frame_tx = sub_frame_tx.clone();
        let stats_leg = stats.clone();
        let sub_cancel = cancel.clone();
        let pt = sub.payload_type;
        let w = config.width;
        let h = config.height;
        let fmt2 = fmt;
        let idx_u = idx as u32;
        let handle = tokio::spawn(async move {
            let mut depkr = Rfc4175Depacketizer::new(w, h, fmt2, pt);
            pair.recv_loop(sub_cancel, move |payload: Bytes, _leg, _seq| -> bool {
                match depkr.feed(&payload) {
                    Ok(DepacketizeOutcome::Frame(frame)) => {
                        let _ = sub_frame_tx.try_send((idx_u, frame));
                    }
                    Ok(DepacketizeOutcome::Dropped { reason, dropped_bytes }) => {
                        tracing::debug!(sub_idx = idx_u, reason, dropped_bytes, "ST 2110-23 input partial drop");
                        stats_leg.input_filtered.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(DepacketizeOutcome::Continue) => {}
                    Err(_) => {
                        stats_leg.input_filtered.fetch_add(1, Ordering::Relaxed);
                    }
                }
                true
            })
            .await;
        });
        handles.push(handle);
    }
    drop(sub_frame_tx);

    cancel.cancelled().await;
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

pub async fn run_st2110_23_output(
    config: St2110_23OutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> Result<()> {
    let fmt = pgroup_for(config.pixel_format);
    let mode = partition_mode_for(config.partition_mode);
    let n = config.sub_streams.len() as u32;

    #[allow(deprecated)]
    let wire_pacing_was_set = config.wire_pacing.is_some();
    if wire_pacing_was_set {
        tracing::warn!(
            "ST 2110-23 output '{}': wire_pacing config field is deprecated and ignored \
             (pacing is now automatic — see docs/wire-pacing.md)",
            config.id
        );
    }
    #[allow(deprecated)]
    let profile = match config.wire_pacing.as_ref() {
        Some(WirePacingConfig::TxTime { profile }) => *profile,
        None => St2110_21ProfileConfig::NarrowLinear,
    };

    // Per-sub-stream pacer. Each sub-stream carries 1/N of the packets
    // of the full frame at the same frame rate; the pacer evenly
    // spreads its packet budget across the frame period.
    let pgroup_fmt = pgroup_for(config.pixel_format);
    let pgroup_bytes = pgroup_fmt.pgroup_bytes() as u64;
    let pgroup_pixels = pgroup_fmt.pgroup_pixels() as u64;
    let frame_pixels = (config.width as u64).saturating_mul(config.height as u64);
    let frame_bytes = frame_pixels.saturating_mul(pgroup_bytes) / pgroup_pixels.max(1);
    let payload = (config.payload_budget as u64).max(1);
    let packets_per_frame_total = (frame_bytes.saturating_add(payload - 1) / payload).max(1) as u32;
    let packets_per_substream = ((packets_per_frame_total as u64 + n as u64 - 1) / n as u64) as u32;
    let frame_period_ns = 1_000_000_000u64
        .saturating_mul(config.frame_rate_den as u64)
        / (config.frame_rate_num as u64).max(1);
    let pacer_profile = match profile {
        St2110_21ProfileConfig::Narrow => St2110_21Profile::Narrow,
        St2110_21ProfileConfig::NarrowLinear => St2110_21Profile::NarrowLinear,
        St2110_21ProfileConfig::Wide => St2110_21Profile::Wide,
    };
    let shared_pacer = Arc::new(St2110_21Pacer::new(frame_period_ns, packets_per_substream, pacer_profile));

    // Build per-sub-stream packetizers + wire-emit pairs.
    struct SubSink {
        wire_red: crate::engine::wire_emit::WireTxHandle,
        wire_blue: Option<crate::engine::wire_emit::WireTxHandle>,
        pkt: Rfc4175Packetizer,
    }
    let mut sinks: Vec<SubSink> = Vec::with_capacity(n as usize);
    for (idx, sub) in config.sub_streams.iter().enumerate() {
        let (red_sock, red_addr) = create_udp_output(
            &sub.dest_addr,
            sub.bind_addr.as_deref(),
            sub.interface_addr.as_deref(),
            config.dscp,
            config.interface_binding.as_ref(),
        )
        .await?;
        let red_std = red_sock
            .into_std()
            .map_err(|e| anyhow!("ST 2110-23 output '{}' sub {idx} red into_std: {e}", config.id))?;
        red_std.set_nonblocking(false).ok();
        let wire_red = spawn_wire_emitter(
            format!("{}-sub{}-red", config.id, idx),
            red_std,
            red_addr,
            AnchorSource::St2110Raster,
            WirePacingClass::EtfEligible,
            EgressPacingMode::Forward,
            None,
            stats.clone(),
            cancel.clone(),
        );

        let wire_blue = match &sub.redundancy {
            Some(r) => {
                let (blue_sock, blue_addr) = create_udp_output(
                    &r.addr,
                    sub.bind_addr.as_deref(),
                    r.interface_addr.as_deref(),
                    config.dscp,
                    config.interface_binding.as_ref(),
                )
                .await?;
                let blue_std = blue_sock.into_std().map_err(|e| {
                    anyhow!("ST 2110-23 output '{}' sub {idx} blue into_std: {e}", config.id)
                })?;
                blue_std.set_nonblocking(false).ok();
                let leg2_stats = Arc::new(OutputStatsAccumulator::new(
                    format!("{}-sub{}-blue", config.id, idx),
                    format!("{}-sub{}-blue", config.id, idx),
                    "st2110_23_blue".to_string(),
                ));
                Some(spawn_wire_emitter(
                    format!("{}-sub{}-blue", config.id, idx),
                    blue_std,
                    blue_addr,
                    AnchorSource::St2110Raster,
                    WirePacingClass::EtfEligible,
                    EgressPacingMode::Forward,
                    None,
                    leg2_stats,
                    cancel.clone(),
                ))
            }
            None => None,
        };

        let pkt_cfg = PacketizerConfig {
            payload_budget: config.payload_budget,
            payload_type: sub.payload_type,
            ssrc: sub.ssrc.unwrap_or_else(rand::random),
        };
        sinks.push(SubSink {
            wire_red,
            wire_blue,
            pkt: Rfc4175Packetizer::new(pkt_cfg),
        });
    }

    let (frame_tx, frame_rx) = mpsc::channel::<RawVideoFrame>(EGRESS_FRAME_QUEUE);
    let (nalu_tx, nalu_rx) = mpsc::channel::<DemuxedFrame>(EGRESS_NALUS_QUEUE);
    let width = config.width;
    let height = config.height;
    let worker_cancel = cancel.clone();
    let _decode_handle = tokio::task::spawn_blocking(move || {
        decode_worker(width, height, fmt, nalu_rx, frame_tx, worker_cancel);
    });

    let sender_cancel = cancel.clone();
    let sender_stats = stats.clone();
    let sender_pacer = shared_pacer.clone();
    let sender_handle = tokio::task::spawn_blocking(move || {
        let mut frame_rx = frame_rx;
        let mut frame_index: u64 = 0;
        while !sender_cancel.is_cancelled() {
            let frame = match frame_rx.blocking_recv() {
                Some(f) => f,
                None => break,
            };
            let subs = partition_frame(&frame, mode, n);
            let recv_us = crate::util::time::now_us();
            for (i, sub_frame) in subs.into_iter().enumerate() {
                let mut out_pkts: Vec<Bytes> = Vec::with_capacity(16);
                sinks[i].pkt.packetize(&sub_frame, |pkt| out_pkts.push(pkt));
                let pkts_per_frame = sender_pacer.packets_per_frame();
                for (idx, pkt) in out_pkts.iter().enumerate() {
                    let pkt_idx = (idx as u32).min(pkts_per_frame.saturating_sub(1));
                    let target_ns = sender_pacer.target_for_packet(frame_index, pkt_idx);
                    let dg_red = WireDatagram {
                        bytes: pkt.clone(),
                        recv_time_us: recv_us,
                        enqueue_us: 0,
                        target_tx_time_ns: Some(target_ns),
                        ts_offset: 0,
                    };
                    if sinks[i].wire_red.try_send(dg_red).is_err() {
                        sender_stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(ref blue) = sinks[i].wire_blue {
                        let dg_blue = WireDatagram {
                            bytes: pkt.clone(),
                            recv_time_us: recv_us,
                            enqueue_us: 0,
                            target_tx_time_ns: Some(target_ns),
                            ts_offset: 0,
                        };
                        let _ = blue.try_send(dg_blue);
                    }
                }
            }
            frame_index = frame_index.wrapping_add(1);
        }
    });

    let mut ts_demuxer = TsDemuxer::new(None);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            rec = rx.recv() => {
                match rec {
                    Ok(packet) => {
                        let ts_payload: &[u8] = if packet.is_raw_ts {
                            &packet.data
                        } else if packet.data.len() >= 12 {
                            &packet.data[12..]
                        } else {
                            continue;
                        };
                        for frame in ts_demuxer.demux(ts_payload) {
                            let _ = nalu_tx.try_send(frame);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
        }
    }
    drop(nalu_tx);
    let _ = sender_handle.await;
    Ok(())
}

// ── Stats extensions ───────────────────────────────────────────────────────

/// Lock-free counters for the ST 2110 video ingress / egress pipelines.
pub struct VideoPipelineStats {
    pub frames_in: AtomicU64,
    pub frames_out: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub last_encode_latency_us: AtomicU64,
    pub last_decode_latency_us: AtomicU64,
}

impl VideoPipelineStats {
    pub fn new() -> Self {
        Self {
            frames_in: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            last_encode_latency_us: AtomicU64::new(0),
            last_decode_latency_us: AtomicU64::new(0),
        }
    }
}
