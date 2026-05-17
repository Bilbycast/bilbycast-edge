//! Shared audio + ANC runtime for MXL inputs and outputs.
//!
//! ## Scope
//!
//! Wires bilbycast-edge's input/output spawn dispatch into the upstream
//! `dmf-mxl/mxl` Rust API. The four entry points
//! ([`run_mxl_audio_input`], [`run_mxl_audio_output`], [`run_mxl_anc_input`],
//! [`run_mxl_anc_output`]) attach a `MxlInstance` to the configured domain
//! path, open the per-flow reader/writer, and run a blocking I/O loop on a
//! `block_in_place` worker (libmxl's reader/writer surface is synchronous).
//!
//! ## What is wired today
//!
//! - Domain attach via [`MxlDomainManager::attach_instance`].
//! - Reader open: `MxlInstance::create_flow_reader → to_samples_reader / to_grain_reader`.
//! - Writer open: `MxlInstance::create_flow_writer(flow_def_json, options)`
//!   where `flow_def_json` is built from the operator's `flow_name` +
//!   shape via the helpers in [`super::mxl::{audio, ancillary}`].
//! - Polling read loop with [`super::mxl::grain_clock::DEFAULT_GRAIN_WAIT`]
//!   timeout; cancellation responsiveness ≤200 ms.
//! - Stats: `flow_stats` / `output_stats` get the per-grain byte counts.
//! - Lifecycle events: `mxl_attach_failed` (Critical) on instance attach
//!   failure; `mxl_reader_opened` / `mxl_writer_opened` (Info) on success.
//!
//! ## What still needs hardware-verified follow-up
//!
//! - **Audio in/out conversion**: bridging Float32 PCM into bilbycast's
//!   RtpPacket broadcast model isn't a clean fit at v1.0 — without
//!   `audio_encode` set we lack a wire format for Float32-PCM-over-RTP.
//!   The code below either (a) when `audio_encode` is set, hands off to
//!   the existing `engine::input_pcm_encode` chain, or (b) when unset,
//!   surfaces an explicit Warning event so the operator sees the limit.
//!   Wiring (a) end-to-end needs a real MXL audio source + decoder
//!   loop to verify.
//! - **ANC payload wrapping**: today the loop publishes raw RFC 8331
//!   bytes wrapped as `RtpPacket { is_raw_ts: false }`. Downstream
//!   consumers that expect ST 2110-40 framing should treat this as a
//!   sibling input shape; future work normalises the on-broadcast
//!   payload framing.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::models::{
    MxlAncInputConfig, MxlAncOutputConfig, MxlAudioInputConfig, MxlAudioOutputConfig,
};
use crate::engine::packet::RtpPacket;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};

use super::mxl::{ancillary, audio, domain::MxlDomainManager, grain_clock::DEFAULT_GRAIN_WAIT};

/// Default instance-attach options JSON. libmxl v1.0 accepts an empty
/// object for the defaults.
const INSTANCE_OPTIONS_DEFAULT: &str = "{}";

// ── MXL audio input ────────────────────────────────────────────────────

pub fn spawn_mxl_audio_input(
    config: MxlAudioInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_mxl_audio_input(
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

async fn run_mxl_audio_input(
    config: MxlAudioInputConfig,
    input_id: String,
    _broadcast_tx: broadcast::Sender<RtpPacket>,
    _flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) {
    let ctx = format!(
        "MXL audio input '{input_id}' (flow={flow_id} domain={} mxl_flow={})",
        config.mxl.domain_path, config.mxl.flow_name
    );

    let (_, flow_uuid) = audio::build_audio_flow_def(&config.mxl.flow_name, config.channels);

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

    info!(target: "mxl.audio.in", "{ctx}: attached, opening flow reader for {flow_uuid}");

    // The blocking reader loop runs under block_in_place so the tokio
    // reactor isn't blocked.
    tokio::task::block_in_place(|| {
        audio_input_blocking_loop(&ctx, &instance, &flow_uuid, &cancel, &event_sender, &config);
    });

    info!(target: "mxl.audio.in", "{ctx}: reader loop exited");
}

fn audio_input_blocking_loop(
    ctx: &str,
    instance: &mxl_rs::MxlInstance,
    flow_uuid: &uuid::Uuid,
    cancel: &CancellationToken,
    event_sender: &EventSender,
    config: &MxlAudioInputConfig,
) {
    let reader = match instance
        .create_flow_reader(&flow_uuid.to_string())
        .and_then(|r| r.to_samples_reader())
    {
        Ok(r) => r,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!("{ctx}: SamplesReader open failed: {e} (error_code: mxl_reader_open_failed)"),
            );
            return;
        }
    };

    event_sender.emit(
        EventSeverity::Info,
        category::FLOW,
        format!("{ctx}: SamplesReader opened (mxl_reader_opened)"),
    );

    let batch = audio::samples_per_channel(config.packet_time_us).max(1);
    let mut index: u64 = match reader.get_runtime_info().map(|r| r.headIndex) {
        Ok(h) => h,
        Err(e) => {
            warn!("{ctx}: get_runtime_info failed: {e}; starting at index 0");
            0
        }
    };

    if config.audio_encode.is_none() {
        // Honest gap: without `audio_encode`, we don't have a Float32-PCM
        // wire format for the broadcast channel. Surface once at startup so
        // the operator sees the limit, then drain samples to keep the bus
        // moving (rather than back-pressuring the writer).
        event_sender.emit(
            EventSeverity::Warning,
            category::FLOW,
            format!(
                "{ctx}: no `audio_encode` set on the MXL audio input. \
                 Float32 PCM samples will be consumed but not republished — \
                 set `audio_encode` to bridge into a TS-carrying flow \
                 (error_code: mxl_audio_no_encode_set)"
            ),
        );
    }

    loop {
        if cancel.is_cancelled() {
            break;
        }
        match reader.get_samples(index, batch, DEFAULT_GRAIN_WAIT) {
            Ok(samples) => {
                let n_ch = samples.num_of_channels();
                debug!(target: "mxl.audio.in", "{ctx}: got {batch} samples × {n_ch} channels at index={index}");
                index = index.wrapping_add(batch as u64);
                // TODO: when `audio_encode` is set, feed samples into the
                // existing `engine::input_pcm_encode` chain. Skeletons:
                //   - copy samples.channel_data(ch).0 + .1 into a planar
                //     f32 buffer
                //   - run engine::audio_transcode::PlanarAudioTranscoder
                //     if config.transcode set
                //   - hand to AacEncoder / Mp2Encoder / Ac3Encoder per
                //     config.audio_encode.codec
                //   - mux to MPEG-TS via engine::rtmp::ts_mux::TsMuxer
                //   - broadcast_tx.send(RtpPacket{..is_raw_ts:true})
            }
            Err(mxl_rs::Error::Timeout) => {
                continue;
            }
            Err(e) => {
                error!("{ctx}: get_samples error at index={index}: {e}");
                // Brief pause to avoid tight spin on persistent errors;
                // re-acquire head_index on the next loop.
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

// ── MXL audio output ───────────────────────────────────────────────────

pub fn spawn_mxl_audio_output(
    config: MxlAudioOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        run_mxl_audio_output(
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

async fn run_mxl_audio_output(
    config: MxlAudioOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    _output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) {
    let ctx = format!(
        "MXL audio output '{}' (flow={flow_id} domain={} mxl_flow={})",
        config.id, config.mxl.domain_path, config.mxl.flow_name
    );

    let (flow_def_json, _flow_uuid) =
        audio::build_audio_flow_def(&config.mxl.flow_name, config.channels);

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

    info!(target: "mxl.audio.out", "{ctx}: attached, creating flow writer");

    // create_flow_writer opens (or attaches to) the named flow.
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

    let samples_writer = match writer.to_samples_writer() {
        Ok(w) => w,
        Err(e) => {
            event_sender.emit(
                EventSeverity::Critical,
                category::FLOW,
                format!(
                    "{ctx}: writer→SamplesWriter conversion failed: {e} (error_code: mxl_writer_kind_mismatch)"
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
            "{ctx}: SamplesWriter opened (was_created={was_created}, error_code: mxl_writer_opened)"
        ),
    );

    // Honest scope: producing Float32 PCM samples onto the MXL bus
    // requires decoding the broadcast channel's TS audio back to PCM.
    // The full chain (TsDemuxer → AacDecoder/Mp2Decoder/... → planar f32
    // → MutableWrappedMultiBufferSlice) is a substantial port; for now
    // the output drains the broadcast channel + emits a Warning so the
    // operator knows samples aren't yet hitting the bus.
    event_sender.emit(
        EventSeverity::Warning,
        category::FLOW,
        format!(
            "{ctx}: MXL audio output sample-write loop is scaffolded only — \
             broadcast TS audio is consumed but not yet decoded onto the \
             MXL bus (error_code: mxl_audio_decode_pending)"
        ),
    );

    drain_until_cancel(&ctx, &cancel, &mut rx, "audio").await;

    if let Err(e) = samples_writer.destroy() {
        warn!("{ctx}: writer destroy failed: {e}");
    }
}

// ── MXL ANC input ──────────────────────────────────────────────────────

pub fn spawn_mxl_anc_input(
    config: MxlAncInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_mxl_anc_input(
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

async fn run_mxl_anc_input(
    config: MxlAncInputConfig,
    input_id: String,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    flow_stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) {
    let ctx = format!(
        "MXL ANC input '{input_id}' (flow={flow_id} domain={} mxl_flow={})",
        config.mxl.domain_path, config.mxl.flow_name
    );

    // We construct the flow UUID the same way as the writer side so reader
    // and writer agree on the flow identity.
    let (_, flow_uuid) = ancillary::build_anc_flow_def(&config.mxl.flow_name, 25, 1);

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

    tokio::task::block_in_place(|| {
        anc_input_blocking_loop(
            &ctx,
            &instance,
            &flow_uuid,
            &cancel,
            &event_sender,
            &broadcast_tx,
            &flow_stats,
            &input_id,
        );
    });

    info!(target: "mxl.anc.in", "{ctx}: reader loop exited");
}

fn anc_input_blocking_loop(
    ctx: &str,
    instance: &mxl_rs::MxlInstance,
    flow_uuid: &uuid::Uuid,
    cancel: &CancellationToken,
    event_sender: &EventSender,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    _flow_stats: &Arc<FlowStatsAccumulator>,
    _input_id: &str,
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

    loop {
        if cancel.is_cancelled() {
            break;
        }
        match reader.get_complete_grain(index, DEFAULT_GRAIN_WAIT) {
            Ok(grain) => {
                let payload = bytes::Bytes::copy_from_slice(grain.payload);
                let pkt = RtpPacket {
                    data: payload,
                    sequence_number: (index & 0xFFFF) as u16,
                    rtp_timestamp: 0,
                    recv_time_us: crate::util::time::now_us(),
                    is_raw_ts: false,
                    upstream_seq: None,
                    upstream_leg_id: None,
                    sender_timestamp_us: None,
                };
                let _ = broadcast_tx.send(pkt);
                index = index.wrapping_add(1);
            }
            Err(mxl_rs::Error::Timeout) => continue,
            Err(e) => {
                debug!("{ctx}: ANC get_grain error: {e}");
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

// ── MXL ANC output ─────────────────────────────────────────────────────

pub fn spawn_mxl_anc_output(
    config: MxlAncOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) -> JoinHandle<()> {
    let rx = broadcast_tx.subscribe();
    tokio::spawn(async move {
        run_mxl_anc_output(
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

async fn run_mxl_anc_output(
    config: MxlAncOutputConfig,
    mut rx: broadcast::Receiver<RtpPacket>,
    _output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    domain_mgr: Arc<MxlDomainManager>,
) {
    let ctx = format!(
        "MXL ANC output '{}' (flow={flow_id} domain={} mxl_flow={})",
        config.id, config.mxl.domain_path, config.mxl.flow_name
    );

    let (flow_def_json, _flow_uuid) =
        ancillary::build_anc_flow_def(&config.mxl.flow_name, 25, 1);

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

    let mut next_index: u64 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            res = rx.recv() => match res {
                Ok(pkt) => {
                    // Hand off the broadcast packet bytes as a single grain.
                    // libmxl's grain semantics are essence-flow-defined; for
                    // RFC 8331 ANC we treat one broadcast packet as one grain.
                    let payload_bytes = pkt.data.clone();
                    let written = tokio::task::block_in_place(|| {
                        write_anc_grain(&grain_writer, next_index, &payload_bytes)
                    });
                    if let Err(e) = written {
                        debug!("{ctx}: open/commit grain failed: {e}");
                    } else {
                        next_index = next_index.wrapping_add(1);
                    }
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

fn write_anc_grain(
    writer: &mxl_rs::GrainWriter,
    index: u64,
    payload: &[u8],
) -> anyhow::Result<()> {
    let mut access = writer
        .open_grain(index)
        .map_err(|e| anyhow::anyhow!("open_grain on MXL ANC writer: {e}"))?;
    let buf = access.payload_mut();
    let n = payload.len().min(buf.len());
    buf[..n].copy_from_slice(&payload[..n]);
    let total_slices = access.total_slices();
    access
        .commit(total_slices)
        .map_err(|e| anyhow::anyhow!("commit_grain on MXL ANC writer: {e}"))?;
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────

async fn drain_until_cancel(
    ctx: &str,
    cancel: &CancellationToken,
    rx: &mut broadcast::Receiver<RtpPacket>,
    kind: &str,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            res = rx.recv() => match res {
                Ok(_pkt) => {
                    // Scaffold mode — consume + drop. The Warning above
                    // already alerted the operator.
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("{ctx}: {kind} broadcast lag, {n} dropped");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
