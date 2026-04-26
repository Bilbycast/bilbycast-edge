// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared input/output runtime helpers for SMPTE ST 2110 essence flows.
//!
//! This module is the implementation backbone for the six thin spawn modules
//! ([`super::input_st2110_30`], [`super::input_st2110_31`],
//! [`super::input_st2110_40`], [`super::output_st2110_30`],
//! [`super::output_st2110_31`], [`super::output_st2110_40`]).
//!
//! ## Data flow
//!
//! Inputs receive RTP datagrams from the configured "Red" leg (and an optional
//! "Blue" leg for SMPTE 2022-7 hitless redundancy via [`RedBluePair`]). Each
//! datagram is parsed to validate the RTP header and the essence-specific
//! payload (PCM frame alignment for ST 2110-30/-31, RFC 8331 ANC framing for
//! ST 2110-40). Validated packets are wrapped in an [`RtpPacket`] (with
//! `is_raw_ts: false`) and published to the flow's broadcast channel exactly
//! like the existing RTP input.
//!
//! Outputs subscribe to the broadcast channel and forward each packet to the
//! configured destination on the Red leg, plus the Blue leg when redundancy
//! is set. DSCP marking follows RP 2129 C10 (default 46/EF). The output side
//! intentionally does not re-packetize: the broadcast channel already carries
//! whole RTP packets, so a paired ST 2110 input + output achieves
//! byte-identical loopback by passthrough. Re-packetization (changing SSRC,
//! payload type, or essence format) is reserved for the WAN-bridge work in
//! step 5.
//!
//! ## Validation
//!
//! Failures from the depacketizer / ANC parser are counted via
//! `FlowStatsAccumulator::input_filtered`. The packet is still forwarded
//! because the existing RTP path also forwards uncategorized payloads, and
//! ST 2110 receivers downstream perform their own framing checks. We surface
//! the count so operators can see "this stream is malformed" without dropping
//! traffic that downstream receivers may still consume.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::config::models::{
    St2110AncillaryInputConfig, St2110AncillaryOutputConfig, St2110AudioInputConfig,
    St2110AudioOutputConfig,
};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::stats::collector::{FlowStatsAccumulator, OutputStatsAccumulator};
use crate::util::rtp_parse::{is_likely_rtp, parse_rtp_sequence_number, parse_rtp_timestamp};
use crate::util::socket::{bind_udp_input, create_udp_output};
use crate::util::time::now_us;

use super::input_pcm_encode::PcmInputProcessor;
use super::packet::RtpPacket;
use super::st2110::ancillary::unpack_ancillary;
use super::st2110::audio::{PcmDepacketizer, PcmFormat};
use super::st2110::ptp::{PtpReporterConfig, PtpStateReporter};
use super::st2110::redblue::RedBluePair;
use super::st2110::scte104::{SpliceOpcode, is_scte104_packet, parse_scte104};

const MAX_DGRAM: usize = 1500 + 28; // jumbo headroom + IP/UDP overhead

/// Convert a configured `bit_depth` to a [`PcmFormat`]. Defaults to L24 when
/// the value is unrecognized; validation upstream rejects anything other than
/// 16 or 24 so this fallback should be unreachable in practice.
fn pcm_format_for_depth(bit_depth: u8) -> PcmFormat {
    match bit_depth {
        16 => PcmFormat::L16,
        _ => PcmFormat::L24,
    }
}

/// Build a per-input source-IP allow-list (`HashSet<IpAddr>`) from the optional
/// list in the config. Mirrors `input_rtp.rs::source_filter` exactly.
fn build_source_filter(allowed: &Option<Vec<String>>) -> Option<HashSet<IpAddr>> {
    allowed.as_ref().map(|sources| {
        let set: HashSet<IpAddr> = sources
            .iter()
            .filter_map(|s| s.parse::<IpAddr>().ok())
            .collect();
        tracing::info!(
            "ST 2110 source IP filter enabled: {} allowed addresses",
            set.len()
        );
        set
    })
}

/// If `clock_domain` is `Some(d)`, spawn a PTP state reporter on that domain
/// and store its handle on the flow stats accumulator. The reporter polls
/// `/var/run/ptp4l` and reports `Unavailable` cleanly when the daemon is
/// missing — see [`crate::engine::st2110::ptp`].
///
/// Idempotent: if a handle is already set on the accumulator, this is a no-op
/// (`OnceLock::set` returns `Err`, which we discard). This lets diff-based
/// flow updates re-call the input spawn helpers without leaking reporter
/// tasks.
fn maybe_spawn_ptp_reporter(
    clock_domain: Option<u8>,
    stats: &Arc<FlowStatsAccumulator>,
    cancel: &CancellationToken,
    event_sender: Option<&EventSender>,
    flow_id: Option<&str>,
) {
    let Some(domain) = clock_domain else {
        return;
    };
    if stats.ptp_state.get().is_some() {
        return;
    }
    let cfg = PtpReporterConfig {
        domain,
        ..PtpReporterConfig::default()
    };
    let handle = PtpStateReporter::spawn_with_events(
        cfg,
        cancel.child_token(),
        event_sender.cloned(),
        flow_id.map(|s| s.to_string()),
    );
    let _ = stats.ptp_state.set(handle);
}

// ─────────────────────────── Audio (-30 / -31) input ───────────────────────────

/// Run the ST 2110-30 / -31 audio input loop.
///
/// `is_aes3` selects between -30 (linear PCM) and -31 (AES3 transparent). The
/// wire framing is identical; only the depacketizer's logging label differs.
pub async fn run_st2110_audio_input(
    config: St2110AudioInputConfig,
    is_aes3: bool,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: Option<EventSender>,
    flow_id: Option<String>,
) -> Result<()> {
    let label = if is_aes3 { "ST2110-31" } else { "ST2110-30" };

    // Optional PTP reporter, gated on `clock_domain`. Idempotent across
    // diff-based flow restarts.
    maybe_spawn_ptp_reporter(
        config.clock_domain,
        &stats,
        &cancel,
        event_sender.as_ref(),
        flow_id.as_deref(),
    );

    // Bind Red + optional Blue.
    let mut pair = RedBluePair::bind_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
        config.redundancy.as_ref(),
    )
    .await?;
    if let (Some(tx), Some(fid)) = (event_sender.clone(), flow_id.clone()) {
        pair = pair.with_events(tx, fid);
    }
    // Publish the leg counters Arc into the flow stats accumulator so
    // `snapshot()` can read it. Only present when `redundancy` is set;
    // single-leg ("Red only") flows still record into `pair.stats.red.*`
    // but the manager UI uses presence of NetworkLegsStats as the gate
    // for the dual-leg display, so we only publish when both legs exist.
    if config.redundancy.is_some() {
        let _ = stats.red_blue_stats.set(pair.stats.clone());
    }

    tracing::info!(
        "{label} input started: red={} blue={:?} pt={} ch={} sr={} bd={} packet_time_us={}",
        config.bind_addr,
        config.redundancy.as_ref().map(|b| b.addr.clone()),
        config.payload_type,
        config.channels,
        config.sample_rate,
        config.bit_depth,
        config.packet_time_us
    );

    let source_filter = build_source_filter(&config.allowed_sources);
    let depacketizer =
        PcmDepacketizer::new(pcm_format_for_depth(config.bit_depth), config.payload_type, config.channels);

    // Build the optional ingress processor (`transcode` / `audio_encode`).
    // Phase 6.5 enables PCM → audio-only MPEG-TS synthesis via
    // `audio_encode` (aac_lc / he_aac_v1 / he_aac_v2 / s302m). Validation
    // restricts -31 to `audio_encode.codec = s302m` only.
    //
    // Failure policy: if `audio_encode` was requested but construction
    // failed (e.g. fdk-aac missing, unsupported codec on the runtime),
    // we return Err so the flow fails loudly. The assembler-side
    // classifier also filters unsupported codecs up-front via
    // `pid_bus_audio_encode_codec_not_supported_on_input`, but this is
    // the backstop for the transcode-only-failure cases which should
    // still be loud rather than silently passthrough.
    let audio_encode_requested = config.audio_encode.is_some();
    let mut processor: Option<PcmInputProcessor> =
        match PcmInputProcessor::new_with_labels(
            config.sample_rate,
            config.bit_depth,
            config.channels,
            config.transcode.as_ref(),
            config.audio_encode.as_ref(),
            /* ssrc */ 0,
            /* initial_seq */ 0,
            /* initial_timestamp */ 0,
            flow_id.as_deref().unwrap_or("flow"),
            label,
        ) {
            Ok(p) => {
                if p.is_some() {
                    tracing::info!("{label} input: ingress PCM processor active");
                }
                p
            }
            Err(e) if audio_encode_requested => {
                // audio_encode was asked for and we could not honour it.
                // Do not fall back — that would silently produce PCM when
                // the operator (and the downstream flow) expected TS.
                anyhow::bail!("{label} input: audio_encode init failed: {e}");
            }
            Err(e) => {
                tracing::warn!("{label} input: PCM processor disabled, passthrough: {e}");
                None
            }
        };

    // The recv loop in RedBluePair owns the sockets, so we cannot easily reach
    // out for source-IP filtering at this layer (it ignores src). Apply source
    // filtering inline by reading via a single-leg path when no Blue is set.
    if pair.blue.is_none() && source_filter.is_some() {
        // Single-leg path with source IP filter — fall through to the manual loop
        // below so we can see `src` per packet.
        run_audio_input_single_with_filter(
            config,
            label,
            depacketizer,
            source_filter.unwrap(),
            broadcast_tx,
            stats,
            cancel,
            processor,
        )
        .await
    } else {
        // Standard path: dual-leg or single-leg without source filter.
        let stats_clone = stats.clone();
        let depacketizer = depacketizer; // owned by closure
        let label_owned = label.to_string();
        let tx = broadcast_tx;
        pair.recv_loop(cancel, move |payload, _leg, _seq| {
            forward_audio_packet(&payload, &depacketizer, &tx, &stats_clone, &label_owned, &mut processor);
            true
        })
        .await;
        Ok(())
    }
}

/// Single-leg input variant with manual `recv_from` so we can apply a source-IP
/// allow-list. Used when the user configures `allowed_sources` and there is no
/// Blue leg (the merger path doesn't expose `src`). Dual-leg + source-filter
/// is rejected by validation today.
async fn run_audio_input_single_with_filter(
    config: St2110AudioInputConfig,
    label: &'static str,
    depacketizer: PcmDepacketizer,
    source_filter: HashSet<IpAddr>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    mut processor: Option<PcmInputProcessor>,
) -> Result<()> {
    let socket = bind_udp_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
    )
    .await?;
    let mut buf = vec![0u8; MAX_DGRAM];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("{label} input on {} stopping (cancelled)", config.bind_addr);
                return Ok(());
            }
            res = socket.recv_from(&mut buf) => {
                match res {
                    Ok((n, src)) => {
                        if !source_filter.contains(&src.ip()) {
                            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        forward_audio_packet(
                            &buf[..n],
                            &depacketizer,
                            &broadcast_tx,
                            &stats,
                            label,
                            &mut processor,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("{label} input recv error: {e}");
                    }
                }
            }
        }
    }
}

/// Validate one inbound RTP datagram, update stats, and publish to the
/// broadcast channel. Shared by both the dual-leg and single-leg audio paths.
fn forward_audio_packet(
    data: &[u8],
    depacketizer: &PcmDepacketizer,
    tx: &broadcast::Sender<RtpPacket>,
    stats: &FlowStatsAccumulator,
    label: &str,
    processor: &mut Option<PcmInputProcessor>,
) {
    if !is_likely_rtp(data) {
        return;
    }
    if let Err(err) = depacketizer.depacketize(data) {
        // Validation failure: count it but still forward — downstream receivers
        // may consume packets we don't perfectly understand (e.g. CSRC lists,
        // header extensions). The counter surfaces "stream is malformed".
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        tracing::trace!("{label} depacketize error: {err}");
    }
    let seq = parse_rtp_sequence_number(data).unwrap_or(0);
    let ts = parse_rtp_timestamp(data).unwrap_or(0);
    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let packet = RtpPacket {
        data: Bytes::copy_from_slice(data),
        sequence_number: seq,
        rtp_timestamp: ts,
        recv_time_us: now_us(),
        is_raw_ts: false,
    };

    // Route through the optional PCM processor. When absent, passthrough.
    // Codec-class work runs on a blocking worker so the reactor is never
    // stalled — matches the output-side rule.
    match processor.as_mut() {
        Some(p) => {
            let outs = tokio::task::block_in_place(|| p.process(&packet));
            let shape_change = p.shape_change();
            for data_out in outs {
                let out_pkt = RtpPacket {
                    data: data_out,
                    sequence_number: packet.sequence_number,
                    rtp_timestamp: packet.rtp_timestamp,
                    recv_time_us: packet.recv_time_us,
                    is_raw_ts: shape_change,
                };
                let _ = tx.send(out_pkt);
            }
        }
        None => {
            let _ = tx.send(packet);
        }
    }
}

// ─────────────────────────── Audio (-30 / -31) output ──────────────────────────

/// Run the ST 2110-30 / -31 audio output loop. Forwards every received
/// [`RtpPacket`] to the configured Red destination, plus the optional Blue
/// destination, with the configured DSCP.
///
/// Three modes:
///
/// 1. **PCM passthrough** (default for ST 2110 → ST 2110): no transcode,
///    no decode. Each `RtpPacket` is sent unchanged.
/// 2. **PCM transcode** (`config.transcode` is `Some` AND `input_format` is
///    `Some`): an in-line [`crate::engine::audio_transcode::TranscodeStage`]
///    runs between the broadcast receiver and the send call: each input
///    packet is decoded, optionally resampled / rerouted / re-quantized, and
///    re-emitted as zero or more output RTP packets at the target format.
/// 3. **Compressed-audio bridge** (`compressed_audio_input` is `true`): the
///    upstream input is a TS-bearing source (RTMP, RTSP, SRT/UDP/RTP TS).
///    The loop runs an in-line [`crate::engine::ts_demux::TsDemuxer`] +
///    [`crate::engine::audio_decode::AacDecoder`], decodes AAC-LC frames to
///    planar f32 PCM, and feeds them to a lazily-built `TranscodeStage`
///    (built once the first AAC frame reveals the actual sample rate /
///    channel count via `cached_aac_config`). When `config.transcode` is
///    not set, the stage is auto-configured to target the output's own
///    `sample_rate` / `bit_depth` / `channels`.
pub async fn run_st2110_audio_output(
    config: St2110AudioOutputConfig,
    is_aes3: bool,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    input_format: Option<crate::engine::audio_transcode::InputFormat>,
    flow_id: &str,
    compressed_audio_input: bool,
) -> Result<()> {
    let label = if is_aes3 { "ST2110-31" } else { "ST2110-30" };
    let (red_sock, red_dest) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
    )
    .await?;

    let blue = if let Some(b) = &config.redundancy {
        let (sock, dest) =
            create_udp_output(&b.addr, config.bind_addr.as_deref(), b.interface_addr.as_deref(), config.dscp)
                .await?;
        Some((sock, dest))
    } else {
        None
    };

    // Build the optional transcode stage. Requires both `transcode` config and
    // a known input format from the upstream input. The channel routing matrix
    // is sourced from a [`MatrixSource::Is08Tracked`] source so live IS-08
    // activations propagate without restarting the flow; if no IS-08 state
    // has been registered (test contexts) it falls back to a static source.
    let mut transcode = match (config.transcode.as_ref(), input_format) {
        (Some(tj), Some(input)) => build_audio_transcode_stage(
            tj.clone(),
            input,
            &config,
            &stats,
            flow_id,
            label,
        ),
        (Some(_), None) => {
            if compressed_audio_input {
                // Defer: input format is unknown until the first decoded AAC
                // frame. The transcode block will be applied lazily by the
                // compressed-audio path below.
                None
            } else {
                tracing::warn!(
                    "{label} output '{}' has transcode config but upstream input is not audio; \
                     falling back to passthrough",
                    config.id
                );
                None
            }
        }
        _ => None,
    };

    // Compressed-audio bridge state. Built only when the upstream input may
    // carry MPEG-TS / AAC. The decoder + transcode stage are both built
    // lazily — we have to wait for the first ADTS frame to reveal the AAC
    // sample rate and channel count.
    let mut ts_demuxer: Option<crate::engine::ts_demux::TsDemuxer> =
        if compressed_audio_input {
            Some(crate::engine::ts_demux::TsDemuxer::with_audio_track(
                None,
                config.audio_track_index,
            ))
        } else {
            None
        };
    let mut aac_decoder: Option<crate::engine::audio_decode::AacDecoder> = None;
    let mut compressed_transcode: Option<crate::engine::audio_transcode::TranscodeStage> = None;
    // Logged-once flag to prevent log spam when AAC config / decode errors
    // repeat on every frame.
    let mut logged_decoder_init_failure = false;
    let mut logged_decode_error_kind: Option<String> = None;
    // Lock-free decode counters shared with the per-output stats accumulator.
    // Registered lazily the first time we successfully construct the AAC
    // decoder (because we need the decoder's sample rate / channel count).
    let compressed_decode_stats =
        std::sync::Arc::new(crate::engine::audio_decode::DecodeStats::new());
    let mut compressed_decode_stats_registered = false;

    tracing::info!(
        "{label} output '{}' started -> red={} blue={:?} transcode={} compressed_input={}",
        config.id,
        red_dest,
        blue.as_ref().map(|(_, d)| *d),
        transcode.is_some(),
        compressed_audio_input,
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("{label} output '{}' stopping (cancelled)", config.id);
                return Ok(());
            }
            res = rx.recv() => {
                match res {
                    Ok(packet) => {
                        // Build the list of output buffers to send. Three modes:
                        //   (a) Compressed-audio bridge (TS→AAC→PCM)
                        //   (b) PCM transcode
                        //   (c) Byte-identical passthrough
                        let outputs: Vec<Bytes> = if let Some(demuxer) = ts_demuxer.as_mut() {
                            let out = run_compressed_audio_step(
                                &packet,
                                demuxer,
                                &mut aac_decoder,
                                &mut compressed_transcode,
                                &config,
                                &stats,
                                flow_id,
                                label,
                                &mut logged_decoder_init_failure,
                                &mut logged_decode_error_kind,
                                &compressed_decode_stats,
                            );
                            // Bind the decode counter handle to the output
                            // stats accumulator once, lazily — we need the
                            // decoder's sample rate / channel count to
                            // label the stage for the UI.
                            if !compressed_decode_stats_registered {
                                if let Some(dec) = aac_decoder.as_ref() {
                                    stats.set_decode_stats(
                                        compressed_decode_stats.clone(),
                                        dec.codec_name(),
                                        dec.sample_rate(),
                                        dec.channels(),
                                    );
                                    compressed_decode_stats_registered = true;
                                }
                            }
                            out
                        } else if let Some(stage) = transcode.as_mut() {
                            stage.process(&packet)
                        } else {
                            vec![packet.data.clone()]
                        };
                        for buf in &outputs {
                            match red_sock.send_to(buf, red_dest).await {
                                Ok(sent) => {
                                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                    stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                    stats.record_latency(packet.recv_time_us);
                                }
                                Err(e) => {
                                    tracing::warn!("{label} output '{}' red send error: {e}", config.id);
                                }
                            }
                            if let Some((sock, dest)) = &blue {
                                if let Err(e) = sock.send_to(buf, *dest).await {
                                    tracing::warn!(
                                        "{label} output '{}' blue send error: {e}",
                                        config.id
                                    );
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!("{label} output '{}' lagged, dropped {n} packets", config.id);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("{label} output '{}' broadcast closed", config.id);
                        return Ok(());
                    }
                }
            }
        }
    }
}

// ────────────────────────────── ANC (-40) input ────────────────────────────────

/// Run the ST 2110-40 ancillary data input loop.
pub async fn run_st2110_anc_input(
    config: St2110AncillaryInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: Option<EventSender>,
    flow_id: Option<String>,
) -> Result<()> {
    let pt = config.payload_type;

    maybe_spawn_ptp_reporter(
        config.clock_domain,
        &stats,
        &cancel,
        event_sender.as_ref(),
        flow_id.as_deref(),
    );

    let mut pair = RedBluePair::bind_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
        config.redundancy.as_ref(),
    )
    .await?;
    if let (Some(tx), Some(fid)) = (event_sender.clone(), flow_id.clone()) {
        pair = pair.with_events(tx, fid);
    }
    if config.redundancy.is_some() {
        let _ = stats.red_blue_stats.set(pair.stats.clone());
    }

    tracing::info!(
        "ST2110-40 input started: red={} blue={:?} pt={pt}",
        config.bind_addr,
        config.redundancy.as_ref().map(|b| b.addr.clone()),
    );

    let source_filter = build_source_filter(&config.allowed_sources);

    if pair.blue.is_none() && source_filter.is_some() {
        run_anc_input_single_with_filter(
            config,
            source_filter.unwrap(),
            broadcast_tx,
            stats,
            cancel,
            event_sender,
            flow_id,
        )
        .await
    } else {
        let tx = broadcast_tx;
        let stats_clone = stats.clone();
        let event_sender_inner = event_sender.clone();
        let flow_id_inner = flow_id.clone();
        pair.recv_loop(cancel, move |payload, _leg, _seq| {
            forward_anc_packet(
                &payload,
                pt,
                &tx,
                &stats_clone,
                event_sender_inner.as_ref(),
                flow_id_inner.as_deref(),
            );
            true
        })
        .await;
        Ok(())
    }
}

async fn run_anc_input_single_with_filter(
    config: St2110AncillaryInputConfig,
    source_filter: HashSet<IpAddr>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: Option<EventSender>,
    flow_id: Option<String>,
) -> Result<()> {
    let socket = bind_udp_input(
        &config.bind_addr,
        config.interface_addr.as_deref(),
        config.source_addr.as_deref(),
    )
    .await?;
    let mut buf = vec![0u8; MAX_DGRAM];
    let pt = config.payload_type;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("ST2110-40 input on {} stopping (cancelled)", config.bind_addr);
                return Ok(());
            }
            res = socket.recv_from(&mut buf) => {
                match res {
                    Ok((n, src)) => {
                        if !source_filter.contains(&src.ip()) {
                            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        forward_anc_packet(
                            &buf[..n],
                            pt,
                            &broadcast_tx,
                            &stats,
                            event_sender.as_ref(),
                            flow_id.as_deref(),
                        );
                    }
                    Err(e) => {
                        tracing::warn!("ST2110-40 input recv error: {e}");
                    }
                }
            }
        }
    }
}

/// Validate one inbound RTP/ANC datagram, update stats, publish.
///
/// When `event_sender` is `Some`, every ANC packet carrying a SCTE-104
/// message (DID `0x41`, SDID `0x07`) is decoded and surfaced to the manager
/// as a `scte104` event. Decode failures are silently counted via
/// `input_filtered`; the original RTP packet is still forwarded so downstream
/// receivers can act on payloads bilbycast does not understand.
fn forward_anc_packet(
    data: &[u8],
    expected_pt: u8,
    tx: &broadcast::Sender<RtpPacket>,
    stats: &FlowStatsAccumulator,
    event_sender: Option<&EventSender>,
    flow_id: Option<&str>,
) {
    if !is_likely_rtp(data) {
        return;
    }
    // Strip RTP header before unpacking ANC.
    if data.len() < 12 {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let pt = data[1] & 0x7F;
    if pt != expected_pt {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let cc = (data[0] & 0x0F) as usize;
    let header_len = 12 + cc * 4;
    if data.len() < header_len {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match unpack_ancillary(&data[header_len..]) {
        Ok(packets) => {
            if let (Some(es), Some(fid)) = (event_sender, flow_id) {
                for anc in &packets {
                    if is_scte104_packet(anc) {
                        emit_scte104_event(es, fid, anc);
                    }
                }
            }
        }
        Err(err) => {
            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
            tracing::trace!("ST2110-40 unpack error: {err}");
        }
    }
    let seq = parse_rtp_sequence_number(data).unwrap_or(0);
    let ts = parse_rtp_timestamp(data).unwrap_or(0);
    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let packet = RtpPacket {
        data: Bytes::copy_from_slice(data),
        sequence_number: seq,
        rtp_timestamp: ts,
        recv_time_us: now_us(),
        is_raw_ts: false,
    };
    let _ = tx.send(packet);
}

/// Decode a single ANC packet known to carry SCTE-104 and dispatch a
/// `scte104` event. Decode failures are logged at trace level and dropped:
/// the packet is still forwarded by the caller, so a malformed message can
/// never break a live ad-marker stream.
fn emit_scte104_event(tx: &EventSender, flow_id: &str, anc: &super::st2110::ancillary::AncPacket) {
    match parse_scte104(&anc.user_data) {
        Ok(msg) => {
            let (severity, label) = match msg.opcode {
                SpliceOpcode::SpliceNull => (EventSeverity::Info, "splice_null"),
                SpliceOpcode::SpliceStartNormal => (EventSeverity::Info, "splice_start_normal"),
                SpliceOpcode::SpliceEndNormal => (EventSeverity::Info, "splice_end_normal"),
                SpliceOpcode::SpliceCancel => (EventSeverity::Warning, "splice_cancel"),
                SpliceOpcode::Other(_) => (EventSeverity::Info, "splice_other"),
            };
            let opcode_value = match msg.opcode {
                SpliceOpcode::SpliceNull => 0x0000u16,
                SpliceOpcode::SpliceStartNormal
                | SpliceOpcode::SpliceEndNormal
                | SpliceOpcode::SpliceCancel => 0x0101u16,
                SpliceOpcode::Other(v) => v,
            };
            let details = serde_json::json!({
                "opcode": label,
                "op_id": opcode_value,
                "splice_event_id": msg.splice_event_id,
                "pre_roll_ms": msg.pre_roll_ms,
                "protocol_version": msg.protocol_version,
                "did": anc.did,
                "sdid": anc.sdid,
            });
            let message = format!("SCTE-104 {label}");
            tx.emit_flow_with_details(severity, category::SCTE104, message, flow_id, details);
        }
        Err(err) => {
            tracing::trace!("SCTE-104 decode error: {err}");
        }
    }
}

// ────────────────────────────── ANC (-40) output ───────────────────────────────

/// Run the ST 2110-40 ancillary data output loop.
pub async fn run_st2110_anc_output(
    config: St2110AncillaryOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
) -> Result<()> {
    let (red_sock, red_dest) = create_udp_output(
        &config.dest_addr,
        config.bind_addr.as_deref(),
        config.interface_addr.as_deref(),
        config.dscp,
    )
    .await?;

    let blue = if let Some(b) = &config.redundancy {
        let (sock, dest) =
            create_udp_output(&b.addr, config.bind_addr.as_deref(), b.interface_addr.as_deref(), config.dscp)
                .await?;
        Some((sock, dest))
    } else {
        None
    };

    tracing::info!(
        "ST2110-40 output '{}' started -> red={} blue={:?}",
        config.id,
        red_dest,
        blue.as_ref().map(|(_, d)| *d)
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("ST2110-40 output '{}' stopping (cancelled)", config.id);
                return Ok(());
            }
            res = rx.recv() => {
                match res {
                    Ok(packet) => {
                        match red_sock.send_to(&packet.data, red_dest).await {
                            Ok(sent) => {
                                stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                stats.record_latency(packet.recv_time_us);
                            }
                            Err(e) => {
                                tracing::warn!("ST2110-40 output '{}' red send error: {e}", config.id);
                            }
                        }
                        if let Some((sock, dest)) = &blue {
                            if let Err(e) = sock.send_to(&packet.data, *dest).await {
                                tracing::warn!(
                                    "ST2110-40 output '{}' blue send error: {e}",
                                    config.id
                                );
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!("ST2110-40 output '{}' lagged, dropped {n}", config.id);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!("ST2110-40 output '{}' broadcast closed", config.id);
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// One step of the compressed-audio bridge. Called from the audio output
/// recv loop when `compressed_audio_input` is true.
///
/// Demuxes any TS packets in the broadcast `RtpPacket`, decodes ADTS AAC
/// frames into planar f32 PCM, lazily builds the [`TranscodeStage`] from
/// the AAC config the first time it's known, and runs each decoded frame
/// through `process_planar`. Returns the resulting RTP-PCM frames ready
/// for the wire.
///
/// All errors are logged once (per kind) to avoid spamming the log when
/// a malformed input loops on the same frame.
#[allow(clippy::too_many_arguments)]
fn run_compressed_audio_step(
    packet: &RtpPacket,
    demuxer: &mut crate::engine::ts_demux::TsDemuxer,
    decoder: &mut Option<crate::engine::audio_decode::AacDecoder>,
    transcode: &mut Option<crate::engine::audio_transcode::TranscodeStage>,
    config: &St2110AudioOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    label: &str,
    logged_init_failure: &mut bool,
    logged_decode_error: &mut Option<String>,
    decode_stats: &std::sync::Arc<crate::engine::audio_decode::DecodeStats>,
) -> Vec<Bytes> {
    use crate::engine::audio_decode::AacDecoder;
    use crate::engine::ts_demux::DemuxedFrame;
    use crate::engine::ts_parse::strip_rtp_header;

    let ts_payload = strip_rtp_header(packet);
    if ts_payload.is_empty() {
        return Vec::new();
    }
    let frames = demuxer.demux(ts_payload);
    if frames.is_empty() {
        return Vec::new();
    }

    let mut emitted: Vec<Bytes> = Vec::new();
    for frame in frames {
        let DemuxedFrame::Aac { data, pts: _ } = frame else {
            // H.264 / Opus / other elementary streams: ignored. Audio
            // outputs are AAC-only in Phase A.
            continue;
        };

        // Lazy decoder + transcode-stage construction. Both are built only
        // once we have the AAC config (sample_rate_index + channel_config),
        // which lands in `cached_aac_config()` after the demuxer parses
        // the first ADTS header. The data we just received IS the first
        // ADTS frame's body, so the cache is populated by now.
        if decoder.is_none() {
            let Some((profile, sri, cc)) = demuxer.cached_aac_config() else {
                // Should never happen — we just got an Aac frame from the
                // demuxer, which means it parsed an ADTS header for us.
                // Log defensively and skip.
                if !*logged_init_failure {
                    tracing::warn!(
                        "{label} output '{}' received AAC frame but demuxer has no cached config",
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
                    tracing::info!(
                        "{label} output '{}' AAC-LC decoder ready: {sample_rate} Hz, {channels} ch",
                        config.id
                    );

                    // Discovered input format. Bit depth is nominal — the
                    // f32 path bypasses decode_pcm_be entirely. We pick L24
                    // because it round-trips f32 with the highest fidelity
                    // and matches the most common ST 2110-30 deployment.
                    let input = crate::engine::audio_transcode::InputFormat {
                        sample_rate,
                        bit_depth: crate::engine::audio_transcode::BitDepth::L24,
                        channels,
                    };

                    // Build a TranscodeJson: use the configured one if
                    // present, otherwise synthesize one that targets the
                    // output's own format. This is what makes "AAC →
                    // ST 2110-30 with no transcode block" work out of the
                    // box: we auto-bridge to the output's nominal format.
                    let tj = config.transcode.clone().unwrap_or_else(|| {
                        crate::engine::audio_transcode::TranscodeJson {
                            sample_rate: Some(config.sample_rate),
                            bit_depth: Some(config.bit_depth),
                            channels: Some(config.channels),
                            ..Default::default()
                        }
                    });

                    *decoder = Some(d);
                    *transcode = build_audio_transcode_stage(
                        tj,
                        input,
                        config,
                        stats,
                        flow_id,
                        label,
                    );
                    if transcode.is_none() {
                        if !*logged_init_failure {
                            tracing::error!(
                                "{label} output '{}' compressed-audio transcode stage build failed",
                                config.id
                            );
                            *logged_init_failure = true;
                        }
                    }
                }
                Err(e) => {
                    if !*logged_init_failure {
                        tracing::error!(
                            "{label} output '{}' AAC decoder init failed: {e}; AAC frames will be dropped",
                            config.id
                        );
                        *logged_init_failure = true;
                    }
                    continue;
                }
            }
        }

        let (Some(dec), Some(stage)) = (decoder.as_mut(), transcode.as_mut()) else {
            continue;
        };

        decode_stats.inc_input();
        match dec.decode_frame(&data) {
            Ok(planar) => {
                decode_stats.inc_output();
                let pkts = stage.process_planar(&planar, packet.recv_time_us);
                emitted.extend(pkts);
            }
            Err(e) => {
                decode_stats.inc_error();
                let kind = format!("{e}");
                if logged_decode_error.as_deref() != Some(kind.as_str()) {
                    tracing::warn!(
                        "{label} output '{}' AAC decode error: {e}",
                        config.id
                    );
                    *logged_decode_error = Some(kind);
                }
            }
        }
    }

    emitted
}

/// Build a [`crate::engine::audio_transcode::TranscodeStage`] for an audio
/// output, given a known input format.
///
/// Shared between two call sites:
///
/// 1. The eager construction at the top of [`run_st2110_audio_output`] when
///    the input is PCM and `config.transcode` is set.
/// 2. The lazy construction in the compressed-audio bridge below, fired
///    once the AAC decoder reveals the actual sample rate / channel count
///    from the first ADTS frame.
fn build_audio_transcode_stage(
    tj: crate::engine::audio_transcode::TranscodeJson,
    input: crate::engine::audio_transcode::InputFormat,
    config: &St2110AudioOutputConfig,
    stats: &Arc<OutputStatsAccumulator>,
    flow_id: &str,
    label: &str,
) -> Option<crate::engine::audio_transcode::TranscodeStage> {
    match crate::engine::audio_transcode::resolve_transcode(&tj, input) {
        Ok(cfg) => {
            let is08_input_id = format!("st2110_30:{}", flow_id);
            let is08_output_id = format!("st2110_30:{}:{}", flow_id, config.id);
            let matrix_source = crate::engine::audio_transcode::MatrixSource::is08_tracked(
                is08_output_id,
                is08_input_id,
                input.channels,
                cfg.out_channels,
                cfg.channel_matrix.clone(),
            );
            let stats_arc =
                std::sync::Arc::new(crate::engine::audio_transcode::TranscodeStats::new());
            stats.set_transcode_stats(stats_arc.clone());
            let ssrc = config.ssrc.unwrap_or_else(rand::random);
            let stage = crate::engine::audio_transcode::TranscodeStage::new(
                input,
                cfg,
                matrix_source,
                stats_arc,
                ssrc,
                rand::random::<u16>(),
                rand::random::<u32>(),
            );
            tracing::info!(
                "{label} output '{}' transcode enabled: {}Hz/{}bit/{}ch -> {}Hz/{}bit/{}ch",
                config.id,
                input.sample_rate,
                input.bit_depth.as_u8(),
                input.channels,
                tj.sample_rate.unwrap_or(input.sample_rate),
                tj.bit_depth.unwrap_or(input.bit_depth.as_u8()),
                tj.channels.unwrap_or(input.channels),
            );
            Some(stage)
        }
        Err(e) => {
            tracing::error!(
                "{label} output '{}' transcode resolve failed: {e}; falling back to passthrough",
                config.id
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::collector::FlowStatsAccumulator;
    use std::time::Duration;
    use tokio::sync::broadcast;

    /// Build a minimal RTP/PCM packet with a 4-frame stereo L24 payload.
    fn rtp_pcm_packet(seq: u16, ts: u32, pt: u8) -> Vec<u8> {
        let mut p = Vec::new();
        p.push(0x80); // V=2, P=0, X=0, CC=0
        p.push(pt & 0x7F); // M=0, PT
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        // 4 stereo frames × 2ch × 3 bytes/sample = 24 bytes payload
        for i in 0..(4 * 2 * 3) {
            p.push(i as u8);
        }
        p
    }

    #[tokio::test]
    async fn test_audio_input_output_loopback_red_only() {
        // Pick two free ports up front so we can address each independently.
        let recv_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv_sock.local_addr().unwrap();
        drop(recv_sock); // free the port; output will rebind it

        let in_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let in_addr = in_sock.local_addr().unwrap();
        drop(in_sock);

        // Spawn the ST 2110-30 input on `in_addr`.
        let input_config = St2110AudioInputConfig {
            bind_addr: in_addr.to_string(),
            interface_addr: None,
            source_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: None,
            allowed_sources: None,
            max_bitrate_mbps: None,
            audio_encode: None,
            transcode: None,
        };
        let (tx, _rx0) = broadcast::channel::<RtpPacket>(64);
        let flow_stats = Arc::new(FlowStatsAccumulator::new(
            "test-flow".to_string(),
            "test".to_string(),
            "st2110_30".to_string(),
        ));
        let cancel = CancellationToken::new();
        let in_cancel = cancel.child_token();
        let in_handle = tokio::spawn({
            let tx = tx.clone();
            let stats = flow_stats.clone();
            let cfg = input_config.clone();
            async move {
                let _ = run_st2110_audio_input(cfg, false, tx, stats, in_cancel, None, None).await;
            }
        });

        // Spawn the output on `recv_addr` (the loopback receiver below).
        let recv_back = tokio::net::UdpSocket::bind(recv_addr).await.unwrap();
        let output_config = St2110AudioOutputConfig {
            active: true,
            group: None,
            id: "out1".into(),
            name: "out1".into(),
            dest_addr: recv_addr.to_string(),
            bind_addr: None,
            interface_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: None,
            dscp: 46,
            ssrc: None,
            transcode: None,
            audio_track_index: None,
        };
        let out_stats = Arc::new(OutputStatsAccumulator::new(
            "out1".into(),
            "out1".into(),
            "st2110_30".into(),
        ));
        let out_cancel = cancel.child_token();
        let out_handle = tokio::spawn({
            let mut rx = tx.subscribe();
            let stats = out_stats.clone();
            async move {
                let _ = run_st2110_audio_output(output_config, false, &mut rx, stats, out_cancel, None, "test-flow", false).await;
            }
        });

        // Give the input/output a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send 3 packets into the input via UDP.
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packets: Vec<Vec<u8>> = (0..3)
            .map(|i| rtp_pcm_packet(100 + i, 1000 + i as u32 * 48, 97))
            .collect();
        for p in &packets {
            sender.send_to(p, in_addr).await.unwrap();
        }

        // Receive 3 packets out the other side and assert byte-identical match.
        let mut received: Vec<Vec<u8>> = Vec::new();
        let mut buf = vec![0u8; 2048];
        for _ in 0..3 {
            let r = tokio::time::timeout(Duration::from_secs(2), recv_back.recv_from(&mut buf))
                .await
                .expect("recv timeout")
                .expect("recv error");
            received.push(buf[..r.0].to_vec());
        }
        assert_eq!(received, packets);
        // Stats checks.
        assert_eq!(flow_stats.input_packets.load(Ordering::Relaxed), 3);
        assert_eq!(out_stats.packets_sent.load(Ordering::Relaxed), 3);

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), in_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), out_handle).await;
    }

    /// Loopback test that exercises the in-line `TranscodeStage` inside
    /// `run_st2110_audio_output`. Uses identity transcoding (same input and
    /// output format) so we can deterministically check that the output
    /// payload length matches one packet's worth of L24 stereo samples after
    /// the decode → re-encode round trip.
    #[tokio::test]
    async fn test_audio_output_with_identity_transcode_emits_packets() {
        let recv_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv_sock.local_addr().unwrap();
        drop(recv_sock);

        let in_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let in_addr = in_sock.local_addr().unwrap();
        drop(in_sock);

        let input_config = St2110AudioInputConfig {
            bind_addr: in_addr.to_string(),
            interface_addr: None,
            source_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: None,
            allowed_sources: None,
            max_bitrate_mbps: None,
            audio_encode: None,
            transcode: None,
        };
        let (tx, _rx0) = broadcast::channel::<RtpPacket>(64);
        let flow_stats = Arc::new(FlowStatsAccumulator::new(
            "tx-flow".to_string(),
            "tx".to_string(),
            "st2110_30".to_string(),
        ));
        let cancel = CancellationToken::new();
        let in_handle = tokio::spawn({
            let tx = tx.clone();
            let stats = flow_stats.clone();
            let cfg = input_config.clone();
            let c = cancel.child_token();
            async move {
                let _ = run_st2110_audio_input(cfg, false, tx, stats, c, None, None).await;
            }
        });

        let recv_back = tokio::net::UdpSocket::bind(recv_addr).await.unwrap();
        let output_config = St2110AudioOutputConfig {
            active: true,
            group: None,
            id: "tx-out".into(),
            name: "tx-out".into(),
            dest_addr: recv_addr.to_string(),
            bind_addr: None,
            interface_addr: None,
            redundancy: None,
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            clock_domain: None,
            dscp: 46,
            ssrc: Some(0xDEADBEEF),
            transcode: Some(crate::engine::audio_transcode::TranscodeJson::default()),
            audio_track_index: None,
        };
        let out_stats = Arc::new(OutputStatsAccumulator::new(
            "tx-out".into(),
            "tx-out".into(),
            "st2110_30".into(),
        ));
        let out_cancel = cancel.child_token();
        let input_format = Some(crate::engine::audio_transcode::InputFormat {
            sample_rate: 48_000,
            bit_depth: crate::engine::audio_transcode::BitDepth::L24,
            channels: 2,
        });
        let out_handle = tokio::spawn({
            let mut rx = tx.subscribe();
            let stats = out_stats.clone();
            async move {
                let _ = run_st2110_audio_output(
                    output_config,
                    false,
                    &mut rx,
                    stats,
                    out_cancel,
                    input_format,
                    "tx-flow",
                    false,
                )
                .await;
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Build a full 1ms (48 frames) stereo L24 packet so the transcoder
        // emits exactly one output packet per input.
        fn full_packet(seq: u16, ts: u32) -> Vec<u8> {
            let mut p = Vec::new();
            p.push(0x80);
            p.push(97);
            p.extend_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(&ts.to_be_bytes());
            p.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
            // 48 frames × 2ch × 3 bytes = 288 bytes
            for i in 0..(48 * 2 * 3) {
                p.push(i as u8);
            }
            p
        }

        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for i in 0..3 {
            sender
                .send_to(&full_packet(100 + i, 1000 + i as u32 * 48), in_addr)
                .await
                .unwrap();
        }

        // Receive 3 packets out the other side; assert payload length matches
        // a 1ms L24 stereo block (288 bytes payload + 12 byte RTP header = 300).
        let mut buf = vec![0u8; 2048];
        for _ in 0..3 {
            let r = tokio::time::timeout(Duration::from_secs(2), recv_back.recv_from(&mut buf))
                .await
                .expect("recv timeout")
                .expect("recv error");
            assert_eq!(r.0, 12 + 48 * 2 * 3, "transcoded packet has wrong size");
        }
        assert_eq!(out_stats.packets_sent.load(Ordering::Relaxed), 3);

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), in_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), out_handle).await;
    }

    fn rtp_anc_packet(seq: u16, pt: u8) -> Vec<u8> {
        // Use the real ANC packetizer to build a valid payload.
        use crate::engine::st2110::ancillary::{AncField, AncPacket, pack_ancillary};
        let anc = vec![AncPacket::simple(0x41, 0x07, 9, vec![0x10, 0x20, 0x30])];
        let payload = pack_ancillary(&anc, 0, AncField::Progressive);
        let mut p = Vec::new();
        p.push(0x80);
        p.push(pt & 0x7F);
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&payload);
        p
    }

    #[tokio::test]
    async fn test_anc_input_output_loopback_red_only() {
        let recv_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv_sock.local_addr().unwrap();
        drop(recv_sock);

        let in_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let in_addr = in_sock.local_addr().unwrap();
        drop(in_sock);

        let input_config = St2110AncillaryInputConfig {
            bind_addr: in_addr.to_string(),
            interface_addr: None,
            source_addr: None,
            redundancy: None,
            payload_type: 100,
            clock_domain: None,
            allowed_sources: None,
        };
        let (tx, _rx0) = broadcast::channel::<RtpPacket>(64);
        let flow_stats = Arc::new(FlowStatsAccumulator::new(
            "anc-flow".into(),
            "anc".into(),
            "st2110_40".into(),
        ));
        let cancel = CancellationToken::new();
        let in_handle = tokio::spawn({
            let tx = tx.clone();
            let stats = flow_stats.clone();
            let cfg = input_config.clone();
            let c = cancel.child_token();
            async move {
                let _ = run_st2110_anc_input(cfg, tx, stats, c, None, None).await;
            }
        });

        let recv_back = tokio::net::UdpSocket::bind(recv_addr).await.unwrap();
        let output_config = St2110AncillaryOutputConfig {
            active: true,
            group: None,
            id: "out1".into(),
            name: "out1".into(),
            dest_addr: recv_addr.to_string(),
            bind_addr: None,
            interface_addr: None,
            redundancy: None,
            payload_type: 100,
            clock_domain: None,
            dscp: 46,
            ssrc: None,
        };
        let out_stats = Arc::new(OutputStatsAccumulator::new(
            "out1".into(),
            "out1".into(),
            "st2110_40".into(),
        ));
        let out_cancel = cancel.child_token();
        let out_handle = tokio::spawn({
            let mut rx = tx.subscribe();
            let stats = out_stats.clone();
            async move {
                let _ = run_st2110_anc_output(output_config, &mut rx, stats, out_cancel).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packets: Vec<Vec<u8>> = (0..2).map(|i| rtp_anc_packet(200 + i, 100)).collect();
        for p in &packets {
            sender.send_to(p, in_addr).await.unwrap();
        }

        let mut received = Vec::new();
        let mut buf = vec![0u8; 2048];
        for _ in 0..2 {
            let r = tokio::time::timeout(Duration::from_secs(2), recv_back.recv_from(&mut buf))
                .await
                .expect("recv timeout")
                .expect("recv error");
            received.push(buf[..r.0].to_vec());
        }
        assert_eq!(received, packets);
        assert_eq!(flow_stats.input_packets.load(Ordering::Relaxed), 2);
        assert_eq!(out_stats.packets_sent.load(Ordering::Relaxed), 2);

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), in_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), out_handle).await;
    }

    /// Step 5 finishing test: a ST 2110-30 input with `clock_domain = Some(0)`
    /// and a Red/Blue redundancy config must publish both a `PtpStateHandle`
    /// and a `RedBlueStats` Arc onto its `FlowStatsAccumulator`, and a
    /// `snapshot()` taken afterwards must carry non-`None` `ptp_state` and
    /// `network_legs` fields. We don't assert specific values: ptp4l is not
    /// running in CI, so the lock_state is `unavailable`. The contract is
    /// "the wiring is plumbed end-to-end".
    #[tokio::test]
    async fn test_snapshot_carries_ptp_and_network_legs_when_configured() {
        use crate::config::models::RedBlueBindConfig;

        let red = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let red_addr = red.local_addr().unwrap();
        drop(red);
        let blue = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let blue_addr = blue.local_addr().unwrap();
        drop(blue);

        let cfg = St2110AudioInputConfig {
            bind_addr: red_addr.to_string(),
            interface_addr: None,
            source_addr: None,
            redundancy: Some(RedBlueBindConfig {
                addr: blue_addr.to_string(),
                interface_addr: None,
                source_addr: None,
            }),
            sample_rate: 48_000,
            bit_depth: 24,
            channels: 2,
            packet_time_us: 1_000,
            payload_type: 97,
            // ptp4l is not running in CI; the reporter will sit at
            // `Unavailable` but the handle must still be published.
            clock_domain: Some(0),
            allowed_sources: None,
            max_bitrate_mbps: None,
            audio_encode: None,
            transcode: None,
        };

        let (tx, _rx0) = broadcast::channel::<RtpPacket>(64);
        let flow_stats = Arc::new(FlowStatsAccumulator::new(
            "snap-flow".into(),
            "snap".into(),
            "st2110_30".into(),
        ));
        let cancel = CancellationToken::new();
        let stats_clone = flow_stats.clone();
        let cancel_inner = cancel.child_token();
        let handle = tokio::spawn(async move {
            let _ = run_st2110_audio_input(cfg, false, tx, stats_clone, cancel_inner, None, None).await;
        });

        // Give the spawn helper time to bind sockets and publish handles.
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            flow_stats.ptp_state.get().is_some(),
            "PtpStateHandle was not published onto FlowStatsAccumulator"
        );
        assert!(
            flow_stats.red_blue_stats.get().is_some(),
            "RedBlueStats Arc was not published onto FlowStatsAccumulator"
        );

        let snap = flow_stats.snapshot();
        assert!(
            snap.ptp_state.is_some(),
            "snapshot() must populate ptp_state when handle is set"
        );
        let ptp = snap.ptp_state.unwrap();
        assert_eq!(ptp.domain, Some(0));
        // ptp4l is unreachable in CI; lock state must be one of the well-known
        // strings (we accept either "unavailable" or "unknown" depending on
        // whether the reporter has had a chance to attempt a poll yet).
        assert!(
            matches!(
                ptp.lock_state.as_str(),
                "unavailable" | "unknown" | "acquiring"
            ),
            "unexpected lock_state {:?}",
            ptp.lock_state
        );

        assert!(
            snap.network_legs.is_some(),
            "snapshot() must populate network_legs when redundancy is set"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
