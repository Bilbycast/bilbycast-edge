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
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::models::{
    St2110VideoInputConfig, St2110VideoOutputConfig, St2110VideoPixelFormat, St2110_23InputConfig,
    St2110_23OutputConfig, St2110_23PartitionModeConfig, VideoEncodeConfig,
};
use crate::engine::packet::RtpPacket;
use crate::engine::rtmp::ts_mux::TsMuxer;
use crate::engine::st2110::redblue::RedBluePair;
use crate::engine::st2110::video::{
    pack_yuv422_10bit, pack_yuv422_8bit, partition_frame, unpack_yuv422_10bit,
    unpack_yuv422_8bit, DepacketizeOutcome, PacketizerConfig, PgroupFormat, RawVideoFrame,
    Rfc4175Depacketizer, Rfc4175MultiStreamReassembler, Rfc4175Packetizer,
    St2110_23PartitionMode, VideoField,
};
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
/// `VideoEncoderConfig`. Delegates to
/// [`crate::engine::video_encode_util::build_encoder_config`] so advanced
/// knobs (chroma / bit_depth / rate_control / CRF / bframes / refs / level
/// / tune / colour metadata) propagate through to the ST 2110-20/-23
/// ingest encoder in lock-step with RTMP / WebRTC / TS video replacer.
#[cfg(feature = "video-thumbnail")]
fn build_encoder_config(
    enc: &VideoEncodeConfig,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
) -> Result<video_codec::VideoEncoderConfig> {
    use video_codec::VideoEncoderCodec;
    let codec = match enc.codec.as_str() {
        "x264" => VideoEncoderCodec::X264,
        "x265" => VideoEncoderCodec::X265,
        "h264_nvenc" => VideoEncoderCodec::H264Nvenc,
        "hevc_nvenc" => VideoEncoderCodec::HevcNvenc,
        other => return Err(anyhow!("unknown encoder codec '{other}'")),
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
        config.redundancy.as_ref(),
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
        );
    });

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

#[cfg(feature = "video-thumbnail")]
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
    let mut pts: i64 = 0;

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
                let (y10, cb10, cr10) =
                    unpack_yuv422_10bit(&frame.pixels, frame.width, frame.height);
                let cw = w / 2;
                let bps = if target_10bit { 2 } else { 1 };
                // Y plane (byte form).
                y_bytes_scratch.resize(w * h * bps, 0);
                if target_10bit {
                    // Little-endian u16 pairs (pix_fmt YUV422P10LE).
                    for (i, &s) in y10.iter().enumerate() {
                        let b = s.to_le_bytes();
                        y_bytes_scratch[i * 2] = b[0];
                        y_bytes_scratch[i * 2 + 1] = b[1];
                    }
                } else {
                    for (i, &s) in y10.iter().enumerate() {
                        y_bytes_scratch[i] = (s >> 2) as u8;
                    }
                }

                let c_rows_out = if downsample_rows { h / 2 } else { h };
                cb_scratch.resize(cw * c_rows_out * bps, 0);
                cr_scratch.resize(cw * c_rows_out * bps, 0);

                match (downsample_rows, target_10bit) {
                    (false, false) => {
                        for row in 0..h {
                            for x in 0..cw {
                                cb_scratch[row * cw + x] = (cb10[row * cw + x] >> 2) as u8;
                                cr_scratch[row * cw + x] = (cr10[row * cw + x] >> 2) as u8;
                            }
                        }
                    }
                    (false, true) => {
                        for (i, &s) in cb10.iter().enumerate() {
                            let b = s.to_le_bytes();
                            cb_scratch[i * 2] = b[0];
                            cb_scratch[i * 2 + 1] = b[1];
                        }
                        for (i, &s) in cr10.iter().enumerate() {
                            let b = s.to_le_bytes();
                            cr_scratch[i * 2] = b[0];
                            cr_scratch[i * 2 + 1] = b[1];
                        }
                    }
                    (true, false) => {
                        for row in 0..c_rows_out {
                            for x in 0..cw {
                                cb_scratch[row * cw + x] =
                                    (cb10[row * 2 * cw + x] >> 2) as u8;
                                cr_scratch[row * cw + x] =
                                    (cr10[row * 2 * cw + x] >> 2) as u8;
                            }
                        }
                    }
                    (true, true) => {
                        for row in 0..c_rows_out {
                            for x in 0..cw {
                                let s_cb = cb10[row * 2 * cw + x].to_le_bytes();
                                let s_cr = cr10[row * 2 * cw + x].to_le_bytes();
                                cb_scratch[(row * cw + x) * 2] = s_cb[0];
                                cb_scratch[(row * cw + x) * 2 + 1] = s_cb[1];
                                cr_scratch[(row * cw + x) * 2] = s_cr[0];
                                cr_scratch[(row * cw + x) * 2 + 1] = s_cr[1];
                            }
                        }
                    }
                }

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
        pts += 1;
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
                };
                stats.input_packets.fetch_add(1, Ordering::Relaxed);
                stats.input_bytes.fetch_add(ts_len, Ordering::Relaxed);
                let _ = broadcast_tx.send(pkt);
            }
        }
    }
}

#[cfg(not(feature = "video-thumbnail"))]
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
) {
    tracing::error!(
        "ST 2110-20 input requires the video-thumbnail and a video-encoder-* feature \
         to be compiled in; this build has video-thumbnail disabled"
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
    )
    .await?;
    let blue_sock = if let Some(r) = &config.redundancy {
        let (s, _) = create_udp_output(
            &r.addr,
            config.bind_addr.as_deref(),
            r.interface_addr.as_deref(),
            config.dscp,
        )
        .await?;
        Some(s)
    } else {
        None
    };

    let (frame_tx, mut frame_rx) = mpsc::channel::<RawVideoFrame>(EGRESS_FRAME_QUEUE);
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

    let ssrc = cfg.ssrc;

    let red_arc = Arc::new(red_sock);
    let blue_arc = blue_sock.map(Arc::new);

    let send_red = red_arc.clone();
    let send_blue = blue_arc.clone();
    let send_cancel = cancel.clone();
    let sender_stats = stats.clone();
    let sender_handle = tokio::spawn(async move {
        while !send_cancel.is_cancelled() {
            let frame = tokio::select! {
                _ = send_cancel.cancelled() => break,
                f = frame_rx.recv() => match f { Some(f) => f, None => break },
            };
            // Packetize synchronously into a buffer, then send async.
            let mut out_pkts: Vec<Bytes> = Vec::with_capacity(16);
            packetizer.packetize(&frame, |pkt| out_pkts.push(pkt));
            let mut total_bytes = 0u64;
            let total_pkts = out_pkts.len() as u64;
            for pkt in out_pkts {
                total_bytes += pkt.len() as u64;
                let _ = send_red.send_to(&pkt, dest_red).await;
                if let (Some(b), Some(addr)) = (send_blue.as_ref(), dest_blue) {
                    let _ = b.send_to(&pkt, addr).await;
                }
            }
            sender_stats.packets_sent.fetch_add(total_pkts, Ordering::Relaxed);
            sender_stats.bytes_sent.fetch_add(total_bytes, Ordering::Relaxed);
        }
    });

    // Feeder task: drain broadcast_rx, demux TS, forward H.264/HEVC frames.
    let mut ts_demuxer = TsDemuxer::new(None);
    let ssrc_used = ssrc;
    let _ = ssrc_used;
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

#[cfg(feature = "video-thumbnail")]
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

    while !cancel.is_cancelled() {
        let frame = match nalu_rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };
        let (nalus, is_h264, pts) = match frame {
            DemuxedFrame::H264 { nalus, pts, .. } => (nalus, true, pts),
            _ => continue,
        };
        let codec = if is_h264 { VideoCodec::H264 } else { VideoCodec::Hevc };
        if current_codec != Some(codec) {
            current_codec = Some(codec);
            decoder = Some(match VideoDecoder::open(codec) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(error = %e, "ST 2110-20 output decoder open failed");
                    continue;
                }
            });
            scaler = None;
        }
        let dec = decoder.as_mut().unwrap();

        // Feed NALUs as annex-B to decoder.
        let mut nalu_bytes = Vec::new();
        for nalu in nalus {
            nalu_bytes.extend_from_slice(&[0, 0, 0, 1]);
            nalu_bytes.extend_from_slice(&nalu);
        }
        if let Err(e) = dec.send_packet(&nalu_bytes) {
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
            // Pack planes to pgroup bytes.
            let pixels = match fmt {
                PgroupFormat::Yuv422_8bit => {
                    let (y, ys) = scaled.plane(0).unwrap();
                    let (cb, cs) = scaled.plane(1).unwrap();
                    let (cr, _) = scaled.plane(2).unwrap();
                    pack_yuv422_8bit(y, ys, cb, cs, cr, cs, width, height)
                }
                PgroupFormat::Yuv422_10bit => {
                    // Planes are 16-bit little-endian; convert to u16.
                    let (y_b, ys) = scaled.plane(0).unwrap();
                    let (cb_b, cs) = scaled.plane(1).unwrap();
                    let (cr_b, _) = scaled.plane(2).unwrap();
                    let y16 = bytes_le_to_u16(y_b, ys, width as usize, height as usize);
                    let cb16 = bytes_le_to_u16(cb_b, cs, (width / 2) as usize, height as usize);
                    let cr16 = bytes_le_to_u16(cr_b, cs, (width / 2) as usize, height as usize);
                    pack_yuv422_10bit(
                        &y16,
                        width as usize,
                        &cb16,
                        (width / 2) as usize,
                        &cr16,
                        (width / 2) as usize,
                        width,
                        height,
                    )
                }
            };
            let raw = RawVideoFrame {
                pixels,
                width,
                height,
                format: fmt,
                pts_90k: pts as u32,
                field: VideoField::Progressive,
            };
            let _ = frame_tx.try_send(raw);
        }
    }
}

#[cfg(not(feature = "video-thumbnail"))]
fn decode_worker(
    _width: u32,
    _height: u32,
    _fmt: PgroupFormat,
    _nalu_rx: mpsc::Receiver<DemuxedFrame>,
    _frame_tx: mpsc::Sender<RawVideoFrame>,
    _cancel: CancellationToken,
) {
    tracing::error!(
        "ST 2110-20 output requires the video-thumbnail feature to be compiled in; \
         this build has video-thumbnail disabled"
    );
}

#[cfg(feature = "video-thumbnail")]
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
    for (idx, sub) in config.sub_streams.into_iter().enumerate() {
        let pair = RedBluePair::bind_input(
            &sub.bind_addr,
            sub.interface_addr.as_deref(),
            sub.redundancy.as_ref(),
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

    // Build per-sub-stream packetizers + sockets.
    struct SubSink {
        red: Arc<UdpSocket>,
        red_addr: SocketAddr,
        blue: Option<Arc<UdpSocket>>,
        blue_addr: Option<SocketAddr>,
        pkt: Rfc4175Packetizer,
    }
    let mut sinks: Vec<SubSink> = Vec::with_capacity(n as usize);
    for sub in &config.sub_streams {
        let (red_sock, red_addr) = create_udp_output(
            &sub.dest_addr,
            sub.bind_addr.as_deref(),
            sub.interface_addr.as_deref(),
            config.dscp,
        )
        .await?;
        let (blue_sock, blue_addr) = match &sub.redundancy {
            Some(r) => {
                let (sock, addr) = create_udp_output(
                    &r.addr,
                    sub.bind_addr.as_deref(),
                    r.interface_addr.as_deref(),
                    config.dscp,
                )
                .await?;
                (Some(Arc::new(sock)), Some(addr))
            }
            None => (None, None),
        };
        let pkt_cfg = PacketizerConfig {
            payload_budget: config.payload_budget,
            payload_type: sub.payload_type,
            ssrc: sub.ssrc.unwrap_or_else(rand::random),
        };
        sinks.push(SubSink {
            red: Arc::new(red_sock),
            red_addr,
            blue: blue_sock,
            blue_addr,
            pkt: Rfc4175Packetizer::new(pkt_cfg),
        });
    }

    let (frame_tx, mut frame_rx) = mpsc::channel::<RawVideoFrame>(EGRESS_FRAME_QUEUE);
    let (nalu_tx, nalu_rx) = mpsc::channel::<DemuxedFrame>(EGRESS_NALUS_QUEUE);
    let width = config.width;
    let height = config.height;
    let worker_cancel = cancel.clone();
    let _decode_handle = tokio::task::spawn_blocking(move || {
        decode_worker(width, height, fmt, nalu_rx, frame_tx, worker_cancel);
    });

    let sender_cancel = cancel.clone();
    let sender_stats = stats.clone();
    let sender_handle = tokio::spawn(async move {
        while !sender_cancel.is_cancelled() {
            let frame = tokio::select! {
                _ = sender_cancel.cancelled() => break,
                f = frame_rx.recv() => match f { Some(f) => f, None => break },
            };
            let subs = partition_frame(&frame, mode, n);
            for (i, sub_frame) in subs.into_iter().enumerate() {
                let (red, blue, red_addr, blue_addr, out_pkts) = {
                    let sink = &mut sinks[i];
                    let mut out_pkts: Vec<Bytes> = Vec::with_capacity(16);
                    sink.pkt.packetize(&sub_frame, |pkt| out_pkts.push(pkt));
                    (
                        sink.red.clone(),
                        sink.blue.clone(),
                        sink.red_addr,
                        sink.blue_addr,
                        out_pkts,
                    )
                };
                let mut total_bytes = 0u64;
                let total_pkts = out_pkts.len() as u64;
                for pkt in out_pkts {
                    total_bytes += pkt.len() as u64;
                    let _ = red.send_to(&pkt, red_addr).await;
                    if let (Some(b), Some(addr)) = (blue.as_ref(), blue_addr) {
                        let _ = b.send_to(&pkt, addr).await;
                    }
                }
                sender_stats.packets_sent.fetch_add(total_pkts, Ordering::Relaxed);
                sender_stats.bytes_sent.fetch_add(total_bytes, Ordering::Relaxed);
            }
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
