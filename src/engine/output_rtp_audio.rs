// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Generic RFC 3551 PCM-over-RTP audio output.
//!
//! Wire-identical to ST 2110-30 but without ST 2110's PTP / RFC 7273 /
//! NMOS clock_domain advertising. Internally synthesizes an
//! [`St2110AudioOutputConfig`] (with `clock_domain = None`) and dispatches
//! to the existing `st2110_io::run_st2110_audio_output` runtime so the
//! transcode stage, IS-08 channel routing, SMPTE 2022-7 redundancy, and
//! per-output stats wiring all work without duplication.
//!
//! ## Transport modes
//!
//! - `"rtp"` (default): RFC 3551 PCM/RTP/UDP — what this module does today.
//! - `"audio_302m"`: SMPTE 302M LPCM in MPEG-TS over RTP/MP2T (RFC 2250).
//!   Wired in Chunks 5–7. Until then, `audio_302m` is accepted by the
//!   validator but the runtime falls back to plain RTP with a warning.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::models::{RtpAudioOutputConfig, St2110AudioOutputConfig};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::socket::create_udp_output;

use super::audio_302m::{rtp_mp2t_packet, S302mOutputPipeline};
use super::audio_transcode::InputFormat;
use super::packet::RtpPacket;
use super::st2110_io::run_st2110_audio_output;

pub fn spawn_rtp_audio_output(
    config: RtpAudioOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    input_format: Option<InputFormat>,
    flow_id: &str,
    compressed_audio_input: bool,
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    let id = config.id.clone();
    let flow_id = flow_id.to_string();

    // Branch on transport_mode. The 302M path runs its own dedicated loop;
    // the default path delegates to the existing ST 2110 audio runtime
    // (which already supports the standard `transcode` block).
    if matches!(config.transport_mode.as_deref(), Some("audio_302m")) {
        return tokio::spawn(async move {
            if let Err(e) = run_rtp_audio_302m_output(
                config,
                &mut rx,
                output_stats,
                cancel,
                input_format,
                compressed_audio_input,
            )
            .await
            {
                tracing::error!("rtp_audio output '{id}' (302M) exited with error: {e}");
            }
        });
    }

    let synthesized = St2110AudioOutputConfig {
        id: config.id.clone(),
        name: config.name.clone(),
        dest_addr: config.dest_addr.clone(),
        bind_addr: config.bind_addr.clone(),
        interface_addr: config.interface_addr.clone(),
        redundancy: config.redundancy.clone(),
        sample_rate: config.sample_rate,
        bit_depth: config.bit_depth,
        channels: config.channels,
        packet_time_us: config.packet_time_us,
        payload_type: config.payload_type,
        clock_domain: None, // Force PTP / NMOS clock advertising off.
        dscp: config.dscp,
        ssrc: config.ssrc,
        transcode: config.transcode.clone(),
    };

    tokio::spawn(async move {
        if let Err(e) = run_st2110_audio_output(
            synthesized,
            false,
            &mut rx,
            output_stats,
            cancel,
            input_format,
            &flow_id,
            compressed_audio_input,
        )
        .await
        {
            tracing::error!("rtp_audio output '{id}' exited with error: {e}");
        }
    })
}

/// 302M loop for `rtp_audio` outputs: builds an `S302mOutputPipeline` and
/// wraps each emitted 1316-byte TS chunk in a single RTP/MP2T packet
/// (RFC 2250) before sending over UDP. This makes Bilbycast a clean source
/// for hardware decoders that consume RTP-encapsulated MPEG-TS audio
/// streams.
async fn run_rtp_audio_302m_output(
    config: RtpAudioOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
) -> anyhow::Result<()> {
    let (socket, dest) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
    )
    .await?;

    // PCM-input path: the input format is known up-front and the pipeline is
    // built eagerly. Compressed-input path: the input format is discovered
    // from the first AAC frame, so the pipeline (and the AAC decoder) are
    // built lazily inside the recv loop.
    let mut pipeline: Option<S302mOutputPipeline> = if !compressed_audio_input {
        let input = match input_format {
            Some(i) => i,
            None => {
                tracing::warn!(
                    "rtp_audio output '{}' transport_mode=audio_302m but upstream input is not audio",
                    config.id
                );
                return Ok(());
            }
        };
        let out_channels = match input.channels {
            1 => 2,
            n if n % 2 == 0 && n <= 8 => n,
            n if n > 8 => 8,
            _ => input.channels + 1,
        };
        Some(
            S302mOutputPipeline::new(input, out_channels, 24, 4_000)
                .map_err(|e| anyhow::anyhow!("S302mOutputPipeline build failed: {e}"))?,
        )
    } else {
        None
    };

    // Compressed-audio bridge state: only built if compressed_audio_input.
    let mut ts_demuxer: Option<crate::engine::ts_demux::TsDemuxer> = if compressed_audio_input {
        Some(crate::engine::ts_demux::TsDemuxer::new(None))
    } else {
        None
    };
    let mut aac_decoder: Option<crate::engine::audio_decode::AacDecoder> = None;
    let mut logged_init_failure = false;
    let mut logged_decode_error: Option<String> = None;

    let ssrc = config.ssrc.unwrap_or_else(rand::random);
    let mut seq: u16 = rand::random();
    let mut rtp_ts: u32 = rand::random();

    tracing::info!(
        "rtp_audio output '{}' (audio_302m) -> {} compressed_input={}",
        config.id,
        dest,
        compressed_audio_input,
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("rtp_audio output '{}' (302M) stopping (cancelled)", config.id);
                return Ok(());
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Drive whichever path is active.
                        if let Some(demuxer) = ts_demuxer.as_mut() {
                            run_compressed_302m_step(
                                &packet,
                                demuxer,
                                &mut aac_decoder,
                                &mut pipeline,
                                &config,
                                &mut logged_init_failure,
                                &mut logged_decode_error,
                            );
                        } else if let Some(p) = pipeline.as_mut() {
                            p.process(&packet);
                        }

                        if let Some(p) = pipeline.as_mut() {
                            for ts_chunk in p.take_ready_datagrams() {
                                let rtp = rtp_mp2t_packet(&ts_chunk, seq, rtp_ts, ssrc);
                                seq = seq.wrapping_add(1);
                                rtp_ts = rtp_ts.wrapping_add(360);
                                match socket.send_to(&rtp, dest).await {
                                    Ok(sent) => {
                                        stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                        stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "rtp_audio output '{}' (302M) send error: {e}",
                                            config.id
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            "rtp_audio output '{}' (302M) broadcast closed",
                            config.id
                        );
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// One step of the compressed-audio bridge for the 302M output path.
///
/// Demuxes any TS packets in `packet`, decodes ADTS AAC frames into planar
/// f32 PCM, and feeds them through `S302mOutputPipeline::process_planar`.
/// The pipeline (and the AAC decoder) are built lazily on the first AAC
/// frame, once the AAC config (sample rate / channel count) is known.
fn run_compressed_302m_step(
    packet: &RtpPacket,
    demuxer: &mut crate::engine::ts_demux::TsDemuxer,
    decoder: &mut Option<crate::engine::audio_decode::AacDecoder>,
    pipeline: &mut Option<S302mOutputPipeline>,
    config: &RtpAudioOutputConfig,
    logged_init_failure: &mut bool,
    logged_decode_error: &mut Option<String>,
) {
    use crate::engine::audio_decode::AacDecoder;
    use crate::engine::ts_demux::DemuxedFrame;
    use crate::engine::ts_parse::strip_rtp_header;

    let ts_payload = strip_rtp_header(packet);
    if ts_payload.is_empty() {
        return;
    }
    let frames = demuxer.demux(ts_payload);
    if frames.is_empty() {
        return;
    }

    for frame in frames {
        let DemuxedFrame::Aac { data, pts: _ } = frame else {
            continue;
        };

        if decoder.is_none() {
            let Some((profile, sri, cc)) = demuxer.cached_aac_config() else {
                if !*logged_init_failure {
                    tracing::warn!(
                        "rtp_audio output '{}' (302M) AAC frame without cached config",
                        config.id
                    );
                    *logged_init_failure = true;
                }
                continue;
            };
            match AacDecoder::from_adts_config(profile, sri, cc) {
                Ok(d) => {
                    let sample_rate = d.sample_rate();
                    let channels = d.channels();
                    let input = InputFormat {
                        sample_rate,
                        bit_depth: crate::engine::audio_transcode::BitDepth::L24,
                        channels,
                    };
                    let out_channels = if channels == 1 { 2 } else { channels };
                    match S302mOutputPipeline::new(input, out_channels, 24, 4_000) {
                        Ok(p) => {
                            tracing::info!(
                                "rtp_audio output '{}' (302M) compressed bridge ready: \
                                 AAC-LC {sample_rate} Hz, {channels} ch -> 48000 Hz {out_channels} ch L24 302M",
                                config.id
                            );
                            *decoder = Some(d);
                            *pipeline = Some(p);
                        }
                        Err(e) => {
                            if !*logged_init_failure {
                                tracing::error!(
                                    "rtp_audio output '{}' (302M) S302mOutputPipeline build failed: {e}",
                                    config.id
                                );
                                *logged_init_failure = true;
                            }
                            return;
                        }
                    }
                }
                Err(e) => {
                    if !*logged_init_failure {
                        tracing::error!(
                            "rtp_audio output '{}' (302M) AAC decoder init failed: {e}",
                            config.id
                        );
                        *logged_init_failure = true;
                    }
                    return;
                }
            }
        }

        let (Some(dec), Some(p)) = (decoder.as_mut(), pipeline.as_mut()) else {
            continue;
        };

        match dec.decode_frame(&data) {
            Ok(planar) => {
                p.process_planar(&planar, packet.recv_time_us);
            }
            Err(e) => {
                let kind = format!("{e}");
                if logged_decode_error.as_deref() != Some(kind.as_str()) {
                    tracing::warn!(
                        "rtp_audio output '{}' (302M) AAC decode error: {e}",
                        config.id
                    );
                    *logged_decode_error = Some(kind);
                }
            }
        }
    }
}
