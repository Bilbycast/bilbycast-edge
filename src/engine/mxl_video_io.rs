//! MXL video I/O — V210 grain reader (input) and writer (output).
//!
//! Input: reads V210 grains from shared memory, unpacks to planar YUV422P10,
//! encodes to H.264/HEVC, muxes into TS, and publishes onto the flow's
//! broadcast channel. Mirrors the ST 2110-20 ingress encoder path.
//!
//! Output: subscribes to the flow's broadcast channel, demuxes TS to video
//! NALUs, decodes to planar YUV, packs to V210, and writes grains into
//! shared memory. Mirrors the ST 2110-20 egress decoder path.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::models::{MxlVideoInputConfig, MxlVideoOutputConfig};
use crate::engine::packet::RtpPacket;
use crate::engine::ts_demux::{DemuxedFrame, TsDemuxer};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};

use super::mxl::{domain::MxlDomainManager, grain_clock::DEFAULT_GRAIN_WAIT, video};

const INSTANCE_OPTIONS_DEFAULT: &str = "{}";
const NALU_QUEUE_CAPACITY: usize = 8;

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

// ── MXL video input ────────────────────────────────────────────────────

pub fn spawn_mxl_video_input(
    config: MxlVideoInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_mxl_video_input(
            config,
            input_id,
            broadcast_tx,
            flow_stats,
            cancel,
            event_sender,
            flow_id,
            domain_mgr,
        )
        .await;
    })
}

async fn run_mxl_video_input(
    config: MxlVideoInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) {
    let ctx = format!(
        "MXL video input '{input_id}' (flow={flow_id} domain={} mxl_flow={})",
        config.mxl.domain_path, config.mxl.flow_name
    );

    let (_, flow_uuid) = video::build_video_flow_def(
        &config.mxl.flow_name,
        config.width,
        config.height,
        config.frame_rate_num,
        config.frame_rate_den,
    );

    let instance = match domain_mgr.attach_instance(&config.mxl.domain_path, INSTANCE_OPTIONS_DEFAULT) {
        Ok(inst) => inst,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!("{ctx}: attach failed: {e} (error_code: mxl_attach_failed)"),
            );
            cancel.cancelled().await;
            return;
        }
    };

    info!(target: "mxl.video.in", "{ctx}: attached, opening flow reader for {flow_uuid}");

    #[cfg(feature = "media-codecs")]
    {
        tokio::task::block_in_place(|| {
            video_input_blocking_loop(
                &ctx,
                &config,
                &instance,
                &flow_uuid,
                &broadcast_tx,
                &flow_stats,
                &cancel,
                &event_sender,
            );
        });
    }
    #[cfg(not(feature = "media-codecs"))]
    {
        event_sender.emit(
            EventSeverity::Critical,
            category::FLOW,
            format!("{ctx}: MXL video input requires the media-codecs feature (error_code: mxl_video_no_media_codecs)"),
        );
        cancel.cancelled().await;
    }

    info!(target: "mxl.video.in", "{ctx}: reader loop exited");
}

#[cfg(feature = "media-codecs")]
fn video_input_blocking_loop(
    ctx: &str,
    config: &MxlVideoInputConfig,
    instance: &mxl_rs::MxlInstance,
    flow_uuid: &uuid::Uuid,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    flow_stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    event_sender: &EventSender,
) {
    use crate::engine::video_encode_util::ScaledVideoEncoder;

    let reader = loop {
        if cancel.is_cancelled() {
            return;
        }
        match instance
            .create_flow_reader(&flow_uuid.to_string())
            .and_then(|r| r.to_grain_reader())
        {
            Ok(r) => break r,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };

    event_sender.emit(
        EventSeverity::Info,
        category::FLOW,
        format!("{ctx}: GrainReader opened (mxl_reader_opened)"),
    );

    let enc = &config.video_encode;

    let width = config.width;
    let height = config.height;
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;

    let enc_cfg = match crate::engine::st2110_video_io::build_encoder_config(enc, width, height, config.frame_rate_num, config.frame_rate_den) {
        Ok(c) => c,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!("{ctx}: encoder config failed: {e} (error_code: mxl_video_encode_config_failed)"),
            );
            return;
        }
    };

    let mut pipeline = ScaledVideoEncoder::new(
        enc.clone(),
        enc_cfg.codec,
        config.frame_rate_num,
        config.frame_rate_den,
        false,
        format!("MXL video input:{ctx}"),
    );

    let mut ts_mux = crate::engine::rtmp::ts_mux::TsMuxer::new();
    let pts_step = 90_000u64 * config.frame_rate_den as u64 / config.frame_rate_num as u64;
    let mut pts: i64 = 0;

    let target_10bit = enc_cfg.bit_depth == 10;
    let bps: usize = if target_10bit { 2 } else { 1 };
    let src_pix_fmt = video_engine::av_pix_fmt_for_yuv(enc_cfg.chroma, if target_10bit { 10 } else { 8 });

    let mut y_bytes_scratch: Vec<u8> = vec![0u8; w * h * bps];
    let mut cb_scratch: Vec<u8> = vec![0u8; cw * h * bps];
    let mut cr_scratch: Vec<u8> = vec![0u8; cw * h * bps];

    let mut index: u64 = match reader.get_runtime_info().map(|r| r.headIndex) {
        Ok(hd) => hd,
        Err(_) => 0,
    };

    event_sender.emit(
        EventSeverity::Info,
        category::FLOW,
        format!("{ctx}: V210→encode pipeline started (codec={}, starting grain={index})", enc.codec),
    );

    loop {
        if cancel.is_cancelled() {
            break;
        }
        match reader.get_complete_grain(index, DEFAULT_GRAIN_WAIT) {
            Ok(grain) => {
                let (y16, cb16, cr16) = video::unpack_v210(grain.payload, width, height);

                if target_10bit {
                    for (i, &s) in y16.iter().enumerate() {
                        let b = s.to_le_bytes();
                        y_bytes_scratch[i * 2] = b[0];
                        y_bytes_scratch[i * 2 + 1] = b[1];
                    }
                    for (i, &s) in cb16.iter().enumerate() {
                        let b = s.to_le_bytes();
                        cb_scratch[i * 2] = b[0];
                        cb_scratch[i * 2 + 1] = b[1];
                    }
                    for (i, &s) in cr16.iter().enumerate() {
                        let b = s.to_le_bytes();
                        cr_scratch[i * 2] = b[0];
                        cr_scratch[i * 2 + 1] = b[1];
                    }
                } else {
                    for (i, &s) in y16.iter().enumerate() {
                        y_bytes_scratch[i] = (s >> 2) as u8;
                    }
                    for (i, &s) in cb16.iter().enumerate() {
                        cb_scratch[i] = (s >> 2) as u8;
                    }
                    for (i, &s) in cr16.iter().enumerate() {
                        cr_scratch[i] = (s >> 2) as u8;
                    }
                }

                let y_stride = w * bps;
                let c_stride = cw * bps;

                let fmt = match src_pix_fmt {
                    Some(f) => f,
                    None => {
                        debug!("{ctx}: unsupported pixel format for encoder");
                        index = index.wrapping_add(1);
                        continue;
                    }
                };

                let enc_out = pipeline.encode_raw_planes(
                    width, height, fmt,
                    &y_bytes_scratch, y_stride,
                    &cb_scratch, c_stride,
                    &cr_scratch, c_stride,
                    Some(pts),
                );

                match enc_out {
                    Ok(frames) => {
                        for ef in frames {
                            let ts_packets = ts_mux.mux_video(&ef.data, ef.pts as u64, ef.dts as u64, ef.keyframe);
                            for ts in ts_packets {
                                let ts_len = ts.len() as u64;
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
                                flow_stats.input_packets.fetch_add(1, Ordering::Relaxed);
                                flow_stats.input_bytes.fetch_add(ts_len, Ordering::Relaxed);
                                let _ = broadcast_tx.send(pkt);
                            }
                        }
                    }
                    Err(e) => {
                        if !pipeline.is_open() {
                            event_sender.emit(
                                EventSeverity::Critical,
                                category::FLOW,
                                format!("{ctx}: encoder open failed: {e} (error_code: mxl_video_encode_failed)"),
                            );
                            break;
                        }
                        debug!("{ctx}: encode error: {e}");
                    }
                }

                pts += pts_step as i64;
                index = index.wrapping_add(1);
            }
            Err(mxl_rs::Error::Timeout) => continue,
            Err(e) => {
                debug!("{ctx}: video get_grain error: {e}");
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(hd) = reader.get_runtime_info().map(|r| r.headIndex) {
                    index = hd;
                }
            }
        }
    }

    if let Err(e) = reader.destroy() {
        warn!("{ctx}: reader destroy failed: {e}");
    }
}

// ── MXL video output ───────────────────────────────────────────────────

pub fn spawn_mxl_video_output(
    config: MxlVideoOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        run_mxl_video_output(
            config,
            rx,
            output_stats,
            cancel,
            event_sender,
            flow_id,
            domain_mgr,
        )
        .await;
    })
}

async fn run_mxl_video_output(
    config: MxlVideoOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) {
    let ctx = format!(
        "MXL video output '{}' (flow={flow_id} domain={} mxl_flow={})",
        config.id, config.mxl.domain_path, config.mxl.flow_name
    );

    let (flow_def_json, _flow_uuid) = video::build_video_flow_def(
        &config.mxl.flow_name,
        config.width,
        config.height,
        config.frame_rate_num,
        config.frame_rate_den,
    );

    let instance = match domain_mgr.attach_instance(&config.mxl.domain_path, INSTANCE_OPTIONS_DEFAULT) {
        Ok(inst) => inst,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!("{ctx}: attach failed: {e} (error_code: mxl_attach_failed)"),
            );
            cancel.cancelled().await;
            return;
        }
    };

    let (writer, _info, was_created) = match instance.create_flow_writer(&flow_def_json, None) {
        Ok(triple) => triple,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!(
                    "{ctx}: create_flow_writer failed: {e} (error_code: mxl_writer_open_failed)"
                ),
            );
            cancel.cancelled().await;
            return;
        }
    };

    let grain_writer = match writer.to_grain_writer() {
        Ok(w) => w,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!(
                    "{ctx}: writer→GrainWriter conversion failed: {e} (error_code: mxl_writer_kind_mismatch)"
                ),
            );
            cancel.cancelled().await;
            return;
        }
    };

    event_sender.emit(
        EventSeverity::Info,
        category::FLOW,
        format!(
            "{ctx}: GrainWriter opened (was_created={was_created}, error_code: mxl_writer_opened)"
        ),
    );

    #[cfg(feature = "media-codecs")]
    {
        let (nalu_tx, nalu_rx) = mpsc::channel::<DemuxedFrame>(NALU_QUEUE_CAPACITY);

        let width = config.width;
        let height = config.height;
        let worker_cancel = cancel.clone();
        let worker_stats = output_stats.clone();
        let worker_ctx = ctx.clone();
        let _decode_handle = tokio::task::spawn_blocking(move || {
            decode_v210_worker(
                &worker_ctx,
                width,
                height,
                nalu_rx,
                grain_writer,
                worker_stats,
                worker_cancel,
            );
        });

        let mut ts_demuxer = TsDemuxer::new(None);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                res = rx.recv() => match res {
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
                        output_stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        debug!("{ctx}: broadcast lag, dropped {n} packets");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        drop(nalu_tx);
    }

    #[cfg(not(feature = "media-codecs"))]
    {
        event_sender.emit(
            EventSeverity::Critical,
            category::FLOW,
            format!("{ctx}: MXL video output requires the media-codecs feature (error_code: mxl_video_no_media_codecs)"),
        );
        let _ = grain_writer;
        cancel.cancelled().await;
    }
}

#[cfg(feature = "media-codecs")]
fn decode_v210_worker(
    ctx: &str,
    width: u32,
    height: u32,
    mut nalu_rx: mpsc::Receiver<DemuxedFrame>,
    grain_writer: mxl_rs::GrainWriter,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) {
    use video_codec::{ScalerDstFormat, VideoCodec};
    use video_engine::{VideoDecoder, VideoScaler};

    let mut current_codec: Option<VideoCodec> = None;
    let mut decoder: Option<VideoDecoder> = None;
    let mut scaler: Option<VideoScaler> = None;
    let mut synth_pts: i64 = 0;
    let mut grain_index: u64 = 0;

    let expected_size = video::v210_frame_bytes(width, height);

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
            decoder = Some(match VideoDecoder::open(codec) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(target: "mxl.video.out", "{ctx}: decoder open failed: {e}");
                    continue;
                }
            });
            scaler = None;
        }
        let dec = decoder.as_mut().unwrap();

        let mut nalu_bytes = Vec::new();
        for nalu in nalus {
            nalu_bytes.extend_from_slice(&[0, 0, 0, 1]);
            nalu_bytes.extend_from_slice(&nalu);
        }

        if let Err(e) = dec.send_packet_with_pts(&nalu_bytes, pts as i64) {
            debug!(target: "mxl.video.out", "{ctx}: decoder send_packet error: {e}");
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
            let _frame_pts = dec_frame.pts().unwrap_or_else(|| {
                synth_pts += 1;
                synth_pts
            });

            if scaler.is_none() {
                scaler = Some(
                    match VideoScaler::new_with_dst_format(
                        src_w, src_h, src_pix_fmt,
                        width, height,
                        ScalerDstFormat::Yuv422p10le,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(target: "mxl.video.out", "{ctx}: scaler init failed: {e}");
                            break;
                        }
                    },
                );
            }
            let s = scaler.as_ref().unwrap();
            let scaled = match s.scale(&dec_frame) {
                Ok(s) => s,
                Err(e) => {
                    warn!(target: "mxl.video.out", "{ctx}: scale error: {e}");
                    continue;
                }
            };

            let (y_b, ys) = scaled.plane(0).unwrap();
            let (cb_b, cs) = scaled.plane(1).unwrap();
            let (cr_b, _) = scaled.plane(2).unwrap();
            let y16 = bytes_le_to_u16(y_b, ys, width as usize, height as usize);
            let cb16 = bytes_le_to_u16(cb_b, cs, (width / 2) as usize, height as usize);
            let cr16 = bytes_le_to_u16(cr_b, cs, (width / 2) as usize, height as usize);

            let v210_bytes = video::pack_v210(&y16, &cb16, &cr16, width, height);

            match grain_writer.open_grain(grain_index) {
                Ok(mut access) => {
                    let payload = access.payload_mut();
                    let copy_len = v210_bytes.len().min(payload.len());
                    payload[..copy_len].copy_from_slice(&v210_bytes[..copy_len]);
                    let slices = access.total_slices();
                    if let Err(e) = access.commit(slices) {
                        debug!(target: "mxl.video.out", "{ctx}: grain commit failed: {e}");
                    } else {
                        output_stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                        output_stats.bytes_sent.fetch_add(expected_size as u64, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    debug!(target: "mxl.video.out", "{ctx}: open_grain({grain_index}) failed: {e}");
                }
            }
            grain_index = grain_index.wrapping_add(1);
        }
    }

    if let Err(e) = grain_writer.destroy() {
        warn!("{ctx}: writer destroy failed: {e}");
    }
}
