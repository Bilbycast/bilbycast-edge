// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

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
/// `VideoEncoderConfig`. Defaults fill in mandatory fields the operator may
/// omit.
#[cfg(feature = "video-thumbnail")]
fn build_encoder_config(
    enc: &VideoEncodeConfig,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
) -> Result<video_codec::VideoEncoderConfig> {
    use video_codec::{VideoEncoderCodec, VideoPreset, VideoProfile};
    let codec = match enc.codec.as_str() {
        "x264" => VideoEncoderCodec::X264,
        "x265" => VideoEncoderCodec::X265,
        "h264_nvenc" => VideoEncoderCodec::H264Nvenc,
        "hevc_nvenc" => VideoEncoderCodec::HevcNvenc,
        other => return Err(anyhow!("unknown encoder codec '{other}'")),
    };
    let preset = match enc.preset.as_deref() {
        Some("ultrafast") => VideoPreset::Ultrafast,
        Some("superfast") => VideoPreset::Superfast,
        Some("veryfast") => VideoPreset::Veryfast,
        Some("faster") => VideoPreset::Faster,
        Some("fast") => VideoPreset::Fast,
        Some("slow") => VideoPreset::Slow,
        Some("slower") => VideoPreset::Slower,
        Some("veryslow") => VideoPreset::Veryslow,
        _ => VideoPreset::Medium,
    };
    let profile = match enc.profile.as_deref() {
        Some("baseline") => VideoProfile::Baseline,
        Some("main") => VideoProfile::Main,
        Some("high") => VideoProfile::High,
        _ => VideoProfile::Auto,
    };
    Ok(video_codec::VideoEncoderConfig {
        codec,
        width: enc.width.unwrap_or(width),
        height: enc.height.unwrap_or(height),
        fps_num: enc.fps_num.unwrap_or(fps_num),
        fps_den: enc.fps_den.unwrap_or(fps_den),
        bitrate_kbps: enc.bitrate_kbps.unwrap_or(8_000),
        gop_size: enc.gop_size.unwrap_or(2 * (fps_num / fps_den.max(1)).max(1)),
        preset,
        profile,
        global_header: false,
    })
}

/// Run the ST 2110-20 input pipeline. Blocks forever until cancel.
pub async fn run_st2110_20_input(
    config: St2110VideoInputConfig,
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
    cancel: CancellationToken,
) {
    use video_engine::VideoEncoder;
    let cfg = match build_encoder_config(&enc, width, height, fps_num, fps_den) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "ST 2110-20 input: failed to build encoder config");
            return;
        }
    };
    let mut encoder = match VideoEncoder::open(&cfg) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "ST 2110-20 input: encoder open failed");
            return;
        }
    };
    let mut ts_mux = TsMuxer::new();
    let mut pts: i64 = 0;

    while !cancel.is_cancelled() {
        let frame = match frame_rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };
        // Convert pgroup bytes to planar YUV 4:2:0 8-bit (encoder input).
        let (y, u, v, y_w, y_h) = match fmt {
            PgroupFormat::Yuv422_8bit => {
                let (y, cb, cr) = unpack_yuv422_8bit(&frame.pixels, frame.width, frame.height);
                // 4:2:2 → 4:2:0 vertical subsample (drop every other chroma row).
                let cw = (frame.width / 2) as usize;
                let ch = (frame.height / 2) as usize;
                let mut cb420 = vec![0u8; cw * ch];
                let mut cr420 = vec![0u8; cw * ch];
                for row in 0..ch {
                    cb420[row * cw..(row + 1) * cw]
                        .copy_from_slice(&cb[row * 2 * cw..row * 2 * cw + cw]);
                    cr420[row * cw..(row + 1) * cw]
                        .copy_from_slice(&cr[row * 2 * cw..row * 2 * cw + cw]);
                }
                (y, cb420, cr420, frame.width as usize, frame.height as usize)
            }
            PgroupFormat::Yuv422_10bit => {
                let (y10, cb10, cr10) =
                    unpack_yuv422_10bit(&frame.pixels, frame.width, frame.height);
                // Simple 10→8 bit reduction: drop low 2 bits.
                let y8: Vec<u8> = y10.iter().map(|&s| (s >> 2) as u8).collect();
                let cw = (frame.width / 2) as usize;
                let h = frame.height as usize;
                let ch = h / 2;
                let mut cb8 = vec![0u8; cw * ch];
                let mut cr8 = vec![0u8; cw * ch];
                for row in 0..ch {
                    for x in 0..cw {
                        cb8[row * cw + x] = (cb10[row * 2 * cw + x] >> 2) as u8;
                        cr8[row * cw + x] = (cr10[row * 2 * cw + x] >> 2) as u8;
                    }
                }
                (y8, cb8, cr8, frame.width as usize, h)
            }
        };

        let y_stride = y_w;
        let c_stride = y_w / 2;
        let enc_out = encoder.encode_frame(&y, y_stride, &u, c_stride, &v, c_stride, Some(pts));
        pts += 1;
        let frames = match enc_out {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "ST 2110-20 input: encoder error");
                continue;
            }
        };
        for ef in frames {
            let ts_packets = ts_mux.mux_video(&ef.data, ef.pts as u64, ef.dts as u64, ef.keyframe);
            let _ = y_h;
            for ts in ts_packets {
                let pkt = RtpPacket {
                    data: ts.clone(),
                    sequence_number: 0,
                    rtp_timestamp: ef.pts as u32,
                    recv_time_us: crate::util::time::now_us(),
                    is_raw_ts: true,
                };
                stats
                    .input_packets
                    .fetch_add(1, Ordering::Relaxed);
                stats
                    .input_bytes
                    .fetch_add(ts.len() as u64, Ordering::Relaxed);
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
