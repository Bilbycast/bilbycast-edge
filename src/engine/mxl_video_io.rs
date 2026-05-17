//! Shared video runtime for MXL inputs and outputs.
//!
//! Mirrors `engine::mxl_io` but for V210 video grains. The input path
//! reads grains, surfaces grain availability stats, and surfaces an
//! explicit honest scaffold-mode warning for the V210→planar→encode chain
//! (the heaviest single piece of M3, needing libswscale + a working
//! encoder backend selected via the existing video-encode resolver).
//! The output path opens a GrainWriter on the configured domain + flow
//! and surfaces a parallel decode→V210 scaffold warning.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::models::{MxlVideoInputConfig, MxlVideoOutputConfig};
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};

use super::mxl::{domain::MxlDomainManager, grain_clock::DEFAULT_GRAIN_WAIT, video};

const INSTANCE_OPTIONS_DEFAULT: &str = "{}";

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
    _broadcast_tx: broadcast::Sender<RtpPacket>,
    _flow_stats: Arc<FlowStatsAccumulator>,
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

    event_sender.emit(
        EventSeverity::Warning,
        category::FLOW,
        format!(
            "{ctx}: MXL video input is in scaffold mode — V210 grains will \
             be read off the bus, but V210→planar→encoder bridging is \
             pending hardware verification (error_code: mxl_video_encode_pending)"
        ),
    );

    tokio::task::block_in_place(|| {
        video_input_blocking_loop(&ctx, &instance, &flow_uuid, &cancel, &event_sender);
    });

    info!(target: "mxl.video.in", "{ctx}: reader loop exited");
}

fn video_input_blocking_loop(
    ctx: &str,
    instance: &mxl_rs::MxlInstance,
    flow_uuid: &uuid::Uuid,
    cancel: &CancellationToken,
    event_sender: &EventSender,
) {
    let reader = match instance
        .create_flow_reader(&flow_uuid.to_string())
        .and_then(|r| r.to_grain_reader())
    {
        Ok(r) => r,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!("{ctx}: GrainReader open failed: {e} (error_code: mxl_reader_open_failed)"),
            );
            return;
        }
    };

    event_sender.emit(
        EventSeverity::Info,
        category::FLOW,
        format!("{ctx}: GrainReader opened (mxl_reader_opened)"),
    );

    let mut index: u64 = match reader.get_runtime_info().map(|r| r.headIndex) {
        Ok(h) => h,
        Err(_) => 0,
    };
    let mut grains_seen: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            break;
        }
        match reader.get_complete_grain(index, DEFAULT_GRAIN_WAIT) {
            Ok(grain) => {
                grains_seen = grains_seen.wrapping_add(1);
                if grains_seen.is_multiple_of(60) {
                    debug!(
                        target: "mxl.video.in",
                        "{ctx}: read {} grains so far (last grain size={} bytes)",
                        grains_seen,
                        grain.payload.len()
                    );
                }
                index = index.wrapping_add(1);
                // TODO: V210 unpack → planar YUV 10-bit → VideoEncoder
                // (per config.video_encode) → TsMuxer → broadcast_tx.
                // Mirrors engine::st2110_video_io::run_st2110_20_input —
                // reuse the same encode + mux primitives. Pending the
                // libswscale call signature audit on V210 input pixel
                // format (`AV_PIX_FMT_V210` in newer FFmpeg).
            }
            Err(mxl_rs::Error::Timeout) => continue,
            Err(e) => {
                debug!("{ctx}: video get_grain error: {e}");
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(h) = reader.get_runtime_info().map(|r| r.headIndex) {
                    index = h;
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
    _output_stats: Arc<OutputStatsAccumulator>,
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

    event_sender.emit(
        EventSeverity::Warning,
        category::FLOW,
        format!(
            "{ctx}: MXL video output is in scaffold mode — broadcast TS \
             video is consumed but TS→decode→V210 bridging is pending \
             hardware verification (error_code: mxl_video_decode_pending)"
        ),
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            res = rx.recv() => match res {
                Ok(_pkt) => {
                    // Scaffold mode — consume + drop. Real pipeline:
                    //   TsDemuxer → VideoDecoder → planar 10-bit YUV →
                    //   V210 pack → grain_writer.open_grain(idx) →
                    //   commit(total_slices).
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("{ctx}: broadcast lag, dropped {n} packets");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    if let Err(e) = grain_writer.destroy() {
        warn!("{ctx}: writer destroy failed: {e}");
    }
}
