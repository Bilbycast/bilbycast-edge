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
) -> JoinHandle<()> {
    let mut rx = broadcast_tx.subscribe();
    let id = config.id.clone();
    let flow_id = flow_id.to_string();

    // Branch on transport_mode. The 302M path runs its own dedicated loop;
    // the default path delegates to the existing ST 2110 audio runtime
    // (which already supports the standard `transcode` block).
    if matches!(config.transport_mode.as_deref(), Some("audio_302m")) {
        return tokio::spawn(async move {
            if let Err(e) =
                run_rtp_audio_302m_output(config, &mut rx, output_stats, cancel, input_format)
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
) -> anyhow::Result<()> {
    let (socket, dest) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
    )
    .await?;

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
    let mut pipeline = S302mOutputPipeline::new(input, out_channels, 24, 4_000)
        .map_err(|e| anyhow::anyhow!("S302mOutputPipeline build failed: {e}"))?;

    let ssrc = config.ssrc.unwrap_or_else(rand::random);
    let mut seq: u16 = rand::random();
    // 90 kHz timestamp; advances by (1316 / 188) × 90000 / TS-rate-per-second.
    // For practical purposes we just increment by a fixed step that matches
    // the 4 ms PES cadence: 4 ms × 90 kHz = 360 ticks per emitted PES, so
    // each TS datagram (which carries roughly one PES worth) advances ~360.
    let mut rtp_ts: u32 = rand::random();

    tracing::info!(
        "rtp_audio output '{}' (audio_302m) -> {} ({}Hz/{}bit/{}ch in)",
        config.id,
        dest,
        input.sample_rate,
        input.bit_depth.as_u8(),
        input.channels,
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
                        pipeline.process(&packet);
                        for ts_chunk in pipeline.take_ready_datagrams() {
                            let rtp = rtp_mp2t_packet(&ts_chunk, seq, rtp_ts, ssrc);
                            seq = seq.wrapping_add(1);
                            // Each chunk represents ~one emitted PES at 4 ms.
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
