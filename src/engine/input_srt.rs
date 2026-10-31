// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// How long a 2022-7 listener leg may wait for its first caller before a
/// Warning says so. The leg keeps waiting. Caller legs report through the
/// connect-failure latch instead.
const LEG_CONNECT_WARN_AFTER: Duration = Duration::from_secs(15);

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use srt_transport::{SocketStatus, SrtListener, SrtSocket};
use tokio_util::sync::CancellationToken;

use crate::config::models::{SrtInputConfig, SrtMode, SrtRedundancyConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::redundancy::merger::{ActiveLeg, BufferedHitlessMerger, HitlessMerger};
use crate::srt::bond_supervisor::{
    BondEventScope, BondEvents, BondHealthTracker, BondLatch, BondSupervisor, degrade_after,
    final_member_report, member_sources_json, plan_bond_members,
};
use crate::srt::connection::{
    accept_srt_connection, acquire_redundant_leg, bind_srt_listener_for_bonded_input,
    bind_srt_listener_for_input, bind_srt_listener_for_redundancy, connect_srt_group_observed,
    connect_srt_with_retry_observed, leg_release_bound, release_leg_acquire,
    spawn_srt_group_stats_poller, spawn_srt_socket_group_stats_poller, spawn_srt_stats_poller,
    LegAcquire, LegAcquired, SrtConnectionParams, CONNECT_FAILED_THRESHOLD, LEG_REACQUIRE_DELAY,
};
use crate::stats::collector::{FlowStatsAccumulator, SrtInputSocket, SrtInputStats};
use crate::util::rtp_parse::{is_likely_rtp, parse_rtp_sequence_number, parse_rtp_timestamp};
use crate::util::time::now_us;

use super::input_post_process::{InputPostProcess, InputPostProcessConfig};
use super::input_transcode::InputTranscoder;
use super::packet::RtpPacket;
use super::ts_parse::is_null_ts_only;

// ── Constants ──────────────────────────────────────────────────────────────

/// TS sync byte — used to detect raw TS data (first byte = 0x47).
const TS_SYNC_BYTE: u8 = 0x47;

/// TS packet size.
const TS_PACKET_SIZE: usize = 188;

// ── SRT Payload Format Detection ───────────────────────────────────────────

/// Detected payload format inside SRT datagrams.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SrtPayloadFormat {
    /// Not yet detected — waiting for the first packet that can tell.
    Unknown,
    /// RTP/TS per SMPTE ST 2022-2: RTP header (12+ bytes) + TS packets.
    RtpTs,
    /// Raw MPEG-2 TS: one or more 188-byte packets with no RTP header.
    /// This is what OBS, Haivision, srt-live-transmit, and vMix send.
    RawTs,
}

/// What an SRT input's receive loop carries across reconnects: the payload
/// format detected on the first packet that can tell and the sequence it
/// numbers packets with, so a reconnect neither re-detects nor restarts the
/// gap check.
struct SrtRecvState {
    format: SrtPayloadFormat,
    last_seq: Option<u16>,
    raw_ts_seq_counter: u16,
    raw_ts_timestamp: u32,
}

impl SrtRecvState {
    fn new() -> Self {
        Self {
            format: SrtPayloadFormat::Unknown,
            last_seq: None,
            raw_ts_seq_counter: 0,
            raw_ts_timestamp: 0,
        }
    }
}

/// Detect the payload format of an SRT datagram.
///
/// Checks whether the first bytes look like an RTP header (version 2) or
/// a raw TS sync byte (0x47). If the data starts with 0x47 and is at least
/// one 188-byte packet long, it's almost certainly raw TS.
///
/// `None` for a datagram of NULL packets alone, which decides nothing: a
/// sender's idle keepalive — a bilbycast flow sends one at its start and
/// whenever its input falls silent — is raw TS whatever framing its media
/// takes, so an RTP stream settled by one would publish every later
/// datagram with its RTP header inside the TS. While the format is
/// undecided a receive loop drops such a datagram uncounted and waits for
/// one that can decide it.
fn detect_format(data: &[u8]) -> Option<SrtPayloadFormat> {
    if is_null_ts_only(data) {
        return None;
    }
    if data.len() < 12 {
        return Some(SrtPayloadFormat::Unknown);
    }

    // Check for RTP version 2 header
    if is_likely_rtp(data) {
        return Some(SrtPayloadFormat::RtpTs);
    }

    // Check for raw TS: starts with sync byte and holds a whole packet
    if data[0] == TS_SYNC_BYTE && data.len() >= TS_PACKET_SIZE {
        return Some(SrtPayloadFormat::RawTs);
    }

    Some(SrtPayloadFormat::Unknown)
}

/// Settle a single-socket receive loop's payload format from `data`, a
/// datagram that arrived while it was still unknown, and log it. Data that
/// is neither RTP nor raw TS is taken for raw TS. Returns `false`, leaving
/// the format unknown, for a datagram that cannot decide it
/// ([`detect_format`]), which the loop then drops.
fn settle_format(format: &mut SrtPayloadFormat, data: &[u8]) -> bool {
    let Some(detected) = detect_format(data) else {
        return false;
    };
    log_detected_format(detected);
    *format = match detected {
        SrtPayloadFormat::Unknown => SrtPayloadFormat::RawTs,
        settled => settled,
    };
    true
}

/// Scan a raw-TS or RTP-payload datagram for the first PCR-bearing TS
/// packet and return its 27 MHz PCR value. Used to synthesise the outer
/// RTP timestamp at 90 kHz from the inner media clock (RFC 2250 §3.5)
/// for raw-TS-in-SRT inputs that don't carry an RTP layer of their own.
///
/// Skips a leading RTP header if present (`is_likely_rtp` heuristic) so
/// the same helper works for both `RtpTs` and `RawTs` payloads. Returns
/// `None` when no PCR-bearing packet is found in this datagram — the
/// caller falls back to the previous timestamp value (most TS packets
/// don't carry PCR; PCR cadence is typically 25–40 ms in broadcast).
pub(crate) fn scan_first_pcr_in_datagram(data: &[u8]) -> Option<u64> {
    let payload = if is_likely_rtp(data) {
        // Best-effort RTP header skip: minimum 12 bytes; CSRC count +
        // extension may extend it. If the resulting offset doesn't land
        // on a TS sync byte we still try the scan from the start of the
        // datagram, since some non-spec wrappers vary.
        let cc = (data[0] & 0x0F) as usize;
        let ext_bit = data[0] & 0x10 != 0;
        let mut hdr = 12 + 4 * cc;
        if ext_bit && hdr + 4 <= data.len() {
            let ext_len_words =
                ((data[hdr + 2] as usize) << 8) | data[hdr + 3] as usize;
            hdr += 4 + 4 * ext_len_words;
        }
        if hdr <= data.len() {
            &data[hdr..]
        } else {
            data
        }
    } else {
        data
    };
    // Walk in 188-byte strides, looking for a PCR-bearing adaptation
    // field. Stop at the first match — the caller only needs one
    // anchor per datagram to keep the 90 kHz output monotonic.
    let mut i = 0;
    while i + TS_PACKET_SIZE <= payload.len() {
        let pkt = &payload[i..i + TS_PACKET_SIZE];
        if pkt[0] == TS_SYNC_BYTE {
            if let Some(pcr) = crate::engine::ts_parse::extract_pcr(pkt) {
                return Some(pcr);
            }
            i += TS_PACKET_SIZE;
        } else {
            // Resync — scan forward for the next sync byte.
            let p = payload[i + 1..].iter().position(|&b| b == TS_SYNC_BYTE)?;
            i = i + 1 + p;
        }
    }
    None
}

// ── Public Entry Point ─────────────────────────────────────────────────────

/// Spawn a task that receives packets from an SRT connection and publishes
/// them to a broadcast channel for fan-out to outputs.
///
/// Supports both RTP-wrapped TS (SMPTE ST 2022-2) and raw TS over SRT.
/// Format is auto-detected from the first received packet:
/// - **RTP/TS**: packets are parsed for RTP seq/timestamp, forwarded with
///   `is_raw_ts = false`.
/// - **Raw TS**: a synthetic incrementing sequence number is generated,
///   forwarded with `is_raw_ts = true` so outputs know not to strip an
///   RTP header.
///
/// If the config has redundancy enabled, two SRT legs are connected and
/// packets are merged using SMPTE 2022-7 hitless protection switching.
/// For raw TS, 2022-7 merge uses a synthetic sequence counter per leg.
pub fn spawn_srt_input(
    config: SrtInputConfig,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    stats: Arc<FlowStatsAccumulator>,
    cancel: CancellationToken,
    event_sender: EventSender,
    flow_id: String,
    input_id: String,
    force_idr: Arc<std::sync::atomic::AtomicBool>,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Apply interface_binding (loose only on SRT in Phase 1): when set
        // and `local_addr` is unset, resolve the NIC name to its primary
        // IPv4 and inject as the SRT source. Strict mode rejected at
        // validation time — see `validate_interface_binding`.
        let mut config = config;
        match crate::util::socket::srt_local_addr_from_binding(
            config.interface_binding.as_ref(),
            config.local_addr.as_deref(),
        ) {
            Ok(Some(addr)) => {
                tracing::info!(
                    "SRT input '{}': interface_binding {} → local_addr {}",
                    input_id,
                    config.interface_binding.as_ref().map(|b| b.name.as_str()).unwrap_or("?"),
                    addr,
                );
                config.local_addr = Some(addr);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("SRT input '{input_id}': interface_binding failed: {e}");
                event_sender.emit_flow_with_details(
                    EventSeverity::Critical,
                    crate::manager::events::category::SRT,
                    format!("SRT input '{input_id}': interface_binding rejected: {e}"),
                    &flow_id,
                    serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
                );
                return;
            }
        }
        // Apply same to the redundancy leg (independent NIC pinning for
        // 2022-7-style SRT dual-leg).
        if let Some(red) = config.redundancy.as_mut() {
            match crate::util::socket::srt_local_addr_from_binding(
                red.interface_binding.as_ref(),
                red.local_addr.as_deref(),
            ) {
                Ok(Some(addr)) => {
                    tracing::info!(
                        "SRT input '{}' leg2: interface_binding {} → local_addr {}",
                        input_id,
                        red.interface_binding.as_ref().map(|b| b.name.as_str()).unwrap_or("?"),
                        addr,
                    );
                    red.local_addr = Some(addr);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("SRT input '{input_id}' leg2: interface_binding failed: {e}");
                    event_sender.emit_flow_with_details(
                        EventSeverity::Critical,
                        crate::manager::events::category::SRT,
                        format!("SRT input '{input_id}' leg2: interface_binding rejected: {e}"),
                        &flow_id,
                        serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
                    );
                    return;
                }
            }
        }
        // Check per-endpoint binding for SRT bonding members. Each member
        // can pin a different NIC — the whole point of bonding for path
        // diversity. Checked only: the NIC's address is resolved again on
        // every group connect and member re-add (`srt::bond_supervisor`),
        // so a member follows a NIC whose address changes.
        if let Some(bonding) = config.bonding.as_ref()
            && let Err(e) = crate::srt::bond_supervisor::check_bond_endpoint_bindings(bonding)
        {
            tracing::error!("SRT input '{input_id}' bonding endpoint: {e}");
            event_sender.emit_flow_with_details(
                EventSeverity::Critical,
                crate::manager::events::category::SRT,
                format!("SRT input '{input_id}' bonding endpoint rejected: {e}"),
                &flow_id,
                serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
            );
            return;
        }
        // Build ingress transcoder once per connection cycle so PMT discovery
        // state and codec buffers persist across reconnects within a single
        // configured flow.
        let mut transcoder = match InputTranscoder::new(
            config.audio_encode.as_ref(),
            config.transcode.as_ref(),
            config.video_encode.as_ref(),
            Some(force_idr.clone()),
        ) {
            Ok(t) => {
                if let Some(ref t) = t {
                    tracing::info!("SRT input: ingress transcode active — {}", t.describe());
                }
                t
            }
            Err(e) => {
                tracing::error!("SRT input: transcode setup failed, falling back to passthrough: {e}");
                event_sender.emit_flow(
                    EventSeverity::Critical,
                    category::FLOW,
                    format!("SRT input transcode disabled: {e}"),
                    &flow_id,
                );
                None
            }
        };
        super::input_transcode::register_ingress_stats(
            stats.as_ref(),
            &input_id,
            transcoder.as_mut(),
            config.audio_encode.as_ref(),
            config.video_encode.as_ref(),
            &event_sender,
        );
        let passthrough_clock = config.passthrough_clock.unwrap_or(false);
        let av_skew_for_post = stats.as_ref().av_skew_reporter_for_input(&input_id);
        let mut post = InputPostProcess::from_config(&InputPostProcessConfig {
            program_number: config.program_number,
            pid_overrides: config.pid_overrides.as_ref(),
            pid_map: config.pid_map.as_ref(),
            passthrough_clock,
            av_sync_pacer: av_sync_pacer.as_ref(),
            av_skew: Some(&av_skew_for_post),
        });
        // The muxer-mode clock rewriter reports a source-clock fallback
        // (no PMT learned) as `clock_rewrite_pmt_not_learned`.
        if let Some(p) = post.as_mut() {
            p.set_event_sender(&event_sender, &input_id);
        }
        if let Some(ref _p) = post {
            tracing::info!(
                "SRT input '{input_id}': ingress post-process active (program_filter={} pid_overrides={} pid_map={} passthrough_clock={})",
                config.program_number.is_some(),
                config.pid_overrides.is_some(),
                config.pid_map.is_some(),
                passthrough_clock,
            );
        }

        // Per-input ingress publisher (fixed-delay + de-jitter modes). Warn when layered on
        // top of SRT's native `latency` jitter buffer (usually
        // redundant; operator either set both deliberately or by
        // mistake — surface either way).
        crate::engine::ingress_publisher::warn_if_double_buffer_srt(
            &input_id,
            &flow_id,
            config.ingress_delay_ms,
            config.recv_latency_ms.map(|v| v as u32),
            &event_sender,
        );
        let publisher = crate::engine::ingress_publisher::IngressPublisher::new(
            // SRT has libsrt TSBPD as its transport de-jitter — no
            // media-rate ingress de-jitter (would double-buffer).
            crate::engine::ingress_publisher::IngressBuffering {
                delay_ms: config.ingress_delay_ms,
                ..Default::default()
            },
            broadcast_tx,
            &input_id,
            cancel.clone(),
            stats.clone(),
        );

        // Every SRT input polls libsrt into its own stats entry.
        let srt = stats.register_srt_input_stats(&input_id);
        let result = if config.bonding.is_some() {
            srt_input_bonded_loop(config, publisher, stats, &srt, cancel, &event_sender, &flow_id, &input_id, &mut transcoder, &mut post).await
        } else if config.redundancy.is_some() {
            srt_input_redundant_loop(config, publisher, stats, &srt, cancel, &event_sender, &flow_id, &input_id, &mut transcoder, &mut post).await
        } else {
            srt_input_loop(config, publisher, stats, &srt, cancel, &event_sender, &flow_id, &input_id, &mut transcoder, &mut post).await
        };
        if let Err(e) = result {
            tracing::error!("SRT input task exited with error: {e}");
            event_sender.emit_flow(EventSeverity::Critical, category::FLOW, format!("Flow input lost: {e}"), &flow_id);
        }
    })
}

// ---------------------------------------------------------------------------
// Single-leg SRT input (no redundancy)
// ---------------------------------------------------------------------------

async fn srt_input_loop(
    config: SrtInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    srt: &Arc<SrtInputStats>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => srt_input_listener_loop(config, publisher, stats, srt, cancel, events, flow_id, input_id, transcoder, post).await,
        _ => srt_input_caller_loop(config, publisher, stats, srt, cancel, events, flow_id, input_id, transcoder, post).await,
    }
}

/// Listener-mode input: binds the listener once and re-accepts new callers
/// on disconnect.
async fn srt_input_listener_loop(
    config: SrtInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    srt: &Arc<SrtInputStats>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let mut listener = match bind_srt_listener_for_input(&config).await {
        Ok(l) => l,
        Err(e) => {
            use crate::manager::events::{BindProto, BindScope};
            let addr = config.local_addr.as_deref().unwrap_or("auto");
            let scope = BindScope::flow_input(flow_id, input_id);
            if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                events.emit_port_conflict("SRT input listener", addr, BindProto::Udp, scope, &e);
            } else {
                events.emit_bind_failed("SRT input listener", addr, BindProto::Udp, scope, &e);
            }
            return Err(e);
        }
    };
    let mut recv_state = SrtRecvState::new();

    loop {
        let socket = match accept_srt_connection(&mut listener, &cancel).await {
            Ok(s) => s,
            Err(_) if cancel.is_cancelled() => {
                tracing::info!("SRT input stopping (cancelled during accept)");
                let _ = listener.close().await;
                return Ok(());
            }
            Err(e) => {
                tracing::error!("SRT input accept failed: {e}");
                events.emit_flow_with_details(
                    EventSeverity::Critical, category::SRT,
                    format!("SRT input connection failed: {e}"), flow_id,
                    serde_json::json!({
                        "mode": "listener",
                        "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                        "stream_id": config.stream_id.as_deref().unwrap_or(""),
                        "error": e.to_string(),
                    }),
                );
                let _ = listener.close().await;
                return Err(e);
            }
        };

        tracing::info!(
            "SRT input connected: mode=listener local={}",
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_flow_with_details(
            EventSeverity::Info, category::SRT,
            "SRT input connected (mode=listener)", flow_id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(
            socket.clone(),
            srt.leg.clone(),
            Some((srt.clone(), SrtInputSocket::Only)),
            poller_cancel.clone(),
        );

        let disconnected = srt_input_recv_loop(
            &socket, &publisher, &stats, srt, &cancel, &mut recv_state, transcoder, post,
        ).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            let _ = listener.close().await;
            return Ok(());
        }

        events.emit_flow_with_details(
            EventSeverity::Warning, category::SRT,
            "SRT input disconnected, reconnecting", flow_id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input stopping during reconnect delay");
                let _ = listener.close().await;
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT input waiting for caller to reconnect...");
    }
}

/// Store `attempts`, the current connect's count of consecutive failed
/// attempts, on `counter`, and report whether this attempt is the one that
/// reaches [`CONNECT_FAILED_THRESHOLD`]: whether the latched event fires (a
/// Critical, or a Warning on a 2022-7 leg whose other leg is connected).
/// Every new connect counts `attempts` from 1 again, so the latch fires once
/// per connect. `counter` is only what the stats read, and nothing re-arms
/// the latch through it: it is the input's own counter, reset on a connect,
/// or for a 2022-7 leg 2 a scratch one nothing reads.
fn record_connect_failure(attempts: u32, counter: &AtomicU32) -> bool {
    counter.store(attempts, Ordering::Relaxed);
    attempts == CONNECT_FAILED_THRESHOLD
}

/// Build the per-attempt connect-failure observer shared by the SRT
/// caller-mode connect paths of a 2022-7 input's legs and a bonded group:
/// bumps `connect_failures` (→ state `connect_failed` past the threshold,
/// for the counter the stats read) and emits the latched event once per
/// acquisition. `static_details` carries the path-specific fields (mode /
/// remote_addr / leg / endpoints).
///
/// `other_leg_up` is the other leg of a 2022-7 pair (`None` elsewhere). The
/// event is a Warning while that leg is connected, since the input is then
/// on air, and a Critical otherwise; with a pair, `details.other_leg` says
/// which (`connected` / `down`).
fn connect_failure_observer<'a>(
    connect_failures: &'a Arc<AtomicU32>,
    events: &'a EventSender,
    flow_id: &'a str,
    input_id: &'a str,
    static_details: serde_json::Value,
    other_leg_up: Option<&'a AtomicBool>,
) -> impl FnMut(u32, &anyhow::Error) + Send + 'a {
    move |attempts: u32, e: &anyhow::Error| {
        if !record_connect_failure(attempts, connect_failures) {
            return;
        }
        tracing::error!(
            "SRT input '{input_id}' connection failed {attempts} consecutive times: {e}"
        );
        let other_up = other_leg_up.map(|up| up.load(Ordering::Relaxed));
        let mut details = static_details.clone();
        if let Some(map) = details.as_object_mut() {
            map.insert("error".into(), serde_json::json!(e.to_string()));
            map.insert("input_id".into(), serde_json::json!(input_id));
            map.insert("consecutive_failures".into(), serde_json::json!(attempts));
            if let Some(up) = other_up {
                map.insert(
                    "other_leg".into(),
                    serde_json::json!(if up { "connected" } else { "down" }),
                );
            }
        }
        events.emit_flow_with_details(
            if other_up == Some(true) { EventSeverity::Warning } else { EventSeverity::Critical },
            category::SRT,
            format!("SRT input connection failed: {e}"),
            flow_id,
            details,
        );
    }
}

/// Caller-mode input: reconnects with exponential back-off on disconnect.
async fn srt_input_caller_loop(
    config: SrtInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    srt: &Arc<SrtInputStats>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let mut recv_state = SrtRecvState::new();

    // Per-input consecutive-connect-failure counter, registered by
    // `spawn_input_runtime` before this task starts. The snapshot path maps
    // it to state "connect_failed"; the detached fallback only exists for
    // callers that never registered counters (tests).
    let connect_failures = stats
        .per_input_counters
        .get(input_id)
        .map(|c| c.connect_failures.clone())
        .unwrap_or_default();

    loop {
        // Surface a never-connecting peer while the retry loop is still
        // running (it only ever returns on success or cancellation): bump
        // the shared counter per attempt and emit the Critical exactly once
        // per connect cycle when the threshold is crossed. A successful
        // connect resets the counter, re-arming the latch.
        let mut on_attempt_failed = |attempts: u32, e: &anyhow::Error| {
            if !record_connect_failure(attempts, &connect_failures) {
                return;
            }
            tracing::error!(
                "SRT input '{input_id}' connection failed {attempts} consecutive times: {e}"
            );
            events.emit_flow_with_details(
                EventSeverity::Critical, category::SRT,
                format!("SRT input connection failed: {e}"), flow_id,
                serde_json::json!({
                    "mode": format!("{:?}", config.mode),
                    "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
                    "stream_id": config.stream_id.as_deref().unwrap_or(""),
                    "error": e.to_string(),
                    "input_id": input_id,
                    "consecutive_failures": attempts,
                }),
            );
        };
        let params = SrtConnectionParams::from(&config);
        let socket =
            match connect_srt_with_retry_observed(&params, &cancel, &mut on_attempt_failed).await {
                Ok(s) => s,
                Err(e) => {
                    if cancel.is_cancelled() {
                        tracing::info!("SRT input stopping (cancelled during connect)");
                        return Ok(());
                    }
                    // Unreachable today — the retry loop errors only on
                    // cancellation; never-connecting peers surface through
                    // the threshold observer above.
                    tracing::error!("SRT input connection failed: {e}");
                    return Err(e);
                }
            };
        connect_failures.store(0, Ordering::Relaxed);

        tracing::info!(
            "SRT input connected: mode={:?} local={}",
            config.mode,
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_flow_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT input connected (mode={:?})", config.mode), flow_id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(
            socket.clone(),
            srt.leg.clone(),
            Some((srt.clone(), SrtInputSocket::Only)),
            poller_cancel.clone(),
        );

        let disconnected = srt_input_recv_loop(
            &socket, &publisher, &stats, srt, &cancel, &mut recv_state, transcoder, post,
        ).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            break;
        }

        events.emit_flow_with_details(
            EventSeverity::Warning, category::SRT,
            "SRT input disconnected, reconnecting", flow_id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
            }),
        );

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input stopping during reconnect delay");
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        tracing::info!("SRT input attempting reconnection...");
    }

    Ok(())
}

/// Inner receive loop shared by listener and caller input modes.
///
/// Returns `Ok(true)` if the connection was lost (caller should reconnect),
/// `Ok(false)` if cancelled (flow is shutting down).
async fn srt_input_recv_loop(
    socket: &Arc<srt_transport::SrtSocket>,
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    stats: &Arc<FlowStatsAccumulator>,
    srt: &SrtInputStats,
    cancel: &CancellationToken,
    state: &mut SrtRecvState,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<bool> {
    let SrtRecvState { format, last_seq, raw_ts_seq_counter, raw_ts_timestamp } = state;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input stopping (cancelled)");
                return Ok(false);
            }
            result = socket.recv() => {
                match result {
                    Ok(pkt) => {
                        // Destructure the libsrt-surfaced packet: `data` is the
                        // payload, `srctime_us` is the sender-set µs timestamp
                        // (None when the sender's srt_sendmsg2 msgctrl was NULL
                        // or carried srctime=0). The latter feeds
                        // SenderTimestampMaster downstream; we attach it to the
                        // emitted RtpPacket below.
                        let srctime_us = pkt.sender_timestamp_us;
                        let data = pkt.data;
                        // Auto-detect format on the first packet that can
                        // tell: NULL packets alone are dropped until one does.
                        if *format == SrtPayloadFormat::Unknown {
                            if !settle_format(format, &data) {
                                continue;
                            }
                            // libsrt's too-late drops then also leave RTP
                            // sequence gaps, which the stats must not count
                            // a second time.
                            srt.rtp_framed.store(*format == SrtPayloadFormat::RtpTs, Ordering::Relaxed);
                        }

                        let (seq, ts, is_raw) = match *format {
                            SrtPayloadFormat::RtpTs => {
                                if !is_likely_rtp(&data) {
                                    continue;
                                }
                                let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
                                let ts = parse_rtp_timestamp(&data).unwrap_or(0);
                                (seq, ts, false)
                            }
                            SrtPayloadFormat::RawTs | SrtPayloadFormat::Unknown => {
                                let seq = *raw_ts_seq_counter;
                                *raw_ts_seq_counter = raw_ts_seq_counter.wrapping_add(1);
                                // RFC 2250 §3.5: the RTP timestamp for MP2T
                                // payload is a 90 kHz media clock anchored to
                                // PCR. Scan this datagram for the first PCR-
                                // bearing TS packet and use `pcr_27mhz / 300`
                                // as the 90 kHz timestamp. Falls through to
                                // the last-seen value when no PCR is in this
                                // datagram (most TS packets don't carry PCR;
                                // PCR cadence is typically 25–40 ms in
                                // broadcast). Replaces the old `bits_so_far`
                                // counter (which advanced ~278× too fast for
                                // a 90 kHz clock at 25 Mbps and tripped
                                // receivers that consult the RTP timestamp
                                // for de-jitter sizing).
                                if let Some(pcr_27mhz) =
                                    scan_first_pcr_in_datagram(&data)
                                {
                                    *raw_ts_timestamp = (pcr_27mhz / 300) as u32;
                                }
                                (seq, *raw_ts_timestamp, true)
                            }
                        };

                        // Detect sequence gaps for loss counting
                        if let Some(prev) = *last_seq {
                            let expected = prev.wrapping_add(1);
                            if seq != expected {
                                let gap = seq.wrapping_sub(expected) as u64;
                                if gap > 0 && gap < 1000 {
                                    stats.input_loss.fetch_add(gap, Ordering::Relaxed);
                                }
                            }
                        }
                        *last_seq = Some(seq);

                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

                        // Bandwidth limit enforcement: drop packet if flow is blocked
                        if stats.bandwidth_blocked.load(Ordering::Relaxed) {
                            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }

                        let packet = RtpPacket {
                            data,
                            sequence_number: seq,
                            rtp_timestamp: ts,
                            recv_time_us: now_us(),
                            is_raw_ts: is_raw,
                            upstream_seq: None,
                            upstream_leg_id: None,
                            sender_timestamp_us: srctime_us,
                        };

                        crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, publisher, packet);
                    }
                    Err(_) => {
                        tracing::warn!("SRT input connection lost, will reconnect");
                        return Ok(true);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dual-leg SRT input with SMPTE 2022-7 hitless protection switching
// ---------------------------------------------------------------------------

/// How far the later of a 2022-7 pair's legs lags the other by
/// configuration: the difference between the receive latencies the legs ask
/// for. TSBPD delivers each packet its leg's latency after it was sent, so
/// the later leg lags the other by that difference whatever the networks do
/// — 1.38 s for legs at 120 ms and 1 500 ms. A sender asking for more
/// latency than a leg does moves that leg later still.
fn pair_leg_lag(leg1: &SrtInputConfig, leg2: &SrtRedundancyConfig) -> Duration {
    let latency1 = leg1.recv_latency_ms.unwrap_or(leg1.latency_ms);
    let latency2 = leg2.recv_latency_ms.unwrap_or(leg2.latency_ms);
    Duration::from_millis(latency1.abs_diff(latency2))
}

/// The stale time of a 2022-7 pair's unbuffered merger
/// ([`HitlessMerger::with_stale_after_us`]): a second past
/// [`pair_leg_lag`], since a stream that moves is followed cleanly only while
/// the stale time exceeds the legs' lag. A buffered merger adds its hold
/// ([`BufferedHitlessMerger::with_leg_lag`]).
fn pair_stale_after_us(leg1: &SrtInputConfig, leg2: &SrtRedundancyConfig) -> u64 {
    crate::redundancy::merger::stale_after_lag_us(pair_leg_lag(leg1, leg2))
}

/// Dual-leg SRT input with SMPTE 2022-7 hitless merge.
///
/// For **RTP/TS** streams: merge uses the RTP sequence number (standard 2022-7).
/// For **raw TS** streams there is no sequence number the legs share, so the
/// input fails over between them ([`RawTsFailover`]), forwarding one leg at a
/// time, rather than merging. The format is detected once per pair, from the
/// first packet either leg delivers that is not NULL packets alone
/// ([`detect_format`]), and both legs must send it.
///
/// Each leg is accepted or dialled on its own ([`acquire_redundant_leg`]),
/// both from the start, so either leg alone carries the input. Listener-mode
/// legs bind once and re-accept on that listener; caller-mode legs re-dial
/// with exponential back-off. A lost leg is re-acquired
/// [`LEG_REACQUIRE_DELAY`] after it is lost while the other leg carries on in
/// the same merger. Only a loss that leaves neither leg connected raises
/// "Both redundant legs lost" and starts the pair afresh — a new merger,
/// failover and format detection — as each connect cycle used to. A listener
/// whose accept channel has closed can never accept again: its leg raises a
/// Critical and is not re-armed, and the task ends only once both legs have
/// failed so.
///
/// Without `path_differential_ms` the merger forwards each seq as it first
/// arrives, so a leg running *ahead* of the leg that carries with a hole in
/// it — returning on a shorter path or a lower receive latency, or just
/// re-attached and skipping packets its first retransmissions could not
/// save in time — is not hitless: the merger forwards past the hole at once
/// and the trailing leg's fills follow newer packets, so for about the lead
/// the output interleaves two stream positions; a lead of a whole merger
/// window or more is skipped instead. Neither reaches `packets_lost`: the
/// interleaved seqs are each forwarded once, and the pair's stats count a
/// skip only as far as the legs' drops confirm it, which a leg just back has
/// none of. With `path_differential_ms` the pair's merger holds each packet
/// that long from its first arrival and releases in seq order
/// ([`PairMerger::Buffered`]), which closes both while the lead and the
/// fills' lateness stay inside the hold and the merger's window.
///
/// Each leg's socket has its own stats poller, into `srt.leg` and
/// `srt.leg2`, from each attach until that leg is lost. What reaches `srt`'s
/// drops is what the pair lost (see [`crate::stats::collector::SrtPair`]):
/// with RTP, the packets neither leg delivered, as the merger counts them;
/// with raw TS, the drops of the leg the failover forwards.
async fn srt_input_redundant_loop(
    config: SrtInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    srt: &Arc<SrtInputStats>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Leg 1's consecutive-connect-failure counter (contract C3 /
    // `connect_failed` state), stored 0 whenever leg 1 attaches. Leg 1 only,
    // as before the legs were acquired on their own: leg 2's caller attempts
    // latch their own event through a scratch counter.
    let connect_failures = stats
        .per_input_counters
        .get(input_id)
        .map(|c| c.connect_failures.clone())
        .unwrap_or_default();

    // Bind persistent listeners for any legs in listener mode. Each is handed
    // to its leg's acquisition and back on every attach, so it stays bound
    // for the life of the input.
    let listener_leg1 = if config.mode == SrtMode::Listener {
        match bind_srt_listener_for_input(&config).await {
            Ok(l) => Some(l),
            Err(e) => {
                use crate::manager::events::{BindProto, BindScope};
                let addr = config.local_addr.as_deref().unwrap_or("auto");
                let scope = BindScope::flow_input(flow_id, input_id);
                if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                    events.emit_port_conflict("SRT input listener leg 1", addr, BindProto::Udp, scope, &e);
                } else {
                    events.emit_bind_failed("SRT input listener leg 1", addr, BindProto::Udp, scope, &e);
                }
                return Err(e);
            }
        }
    } else {
        None
    };
    let listener_leg2 = if redundancy.mode == SrtMode::Listener {
        match bind_srt_listener_for_redundancy(redundancy).await {
            Ok(l) => Some(l),
            Err(e) => {
                use crate::manager::events::{BindProto, BindScope};
                let addr = redundancy.local_addr.as_deref().unwrap_or("auto");
                let scope = BindScope::flow_input(flow_id, input_id);
                if crate::util::port_error::anyhow_is_addr_in_use(&e) {
                    events.emit_port_conflict("SRT input listener leg 2", addr, BindProto::Udp, scope, &e);
                } else {
                    events.emit_bind_failed("SRT input listener leg 2", addr, BindProto::Udp, scope, &e);
                }
                return Err(e);
            }
        }
    } else {
        None
    };

    // Whether each leg is attached. Written on attach and loss only; read by
    // the connect-failure observers and the slow-connect Warnings, which run
    // inside the acquisitions and cannot see the sockets.
    let leg_up = [AtomicBool::new(false), AtomicBool::new(false)];
    // Leg 2's caller attempts, for the latch alone: nothing reads the count.
    let leg2_attempts = Arc::new(AtomicU32::new(0));
    // Cancelled at teardown whether or not `cancel` was, so a pending
    // acquisition can be let finish (see the end of this function).
    let legs_cancel = cancel.child_token();
    let ctx = RedundantInputCtx {
        config: &config,
        redundancy,
        cancel: &legs_cancel,
        events,
        flow_id,
        input_id,
        leg_up: &leg_up,
        connect_failures: &connect_failures,
        leg2_attempts: &leg2_attempts,
    };

    // Each leg's state in locals of its own, so the select! below borrows one
    // leg's acquisition mutably beside the other leg's socket.
    let mut acquire1 = Some(arm_input_leg(&ctx, 1, listener_leg1, Duration::ZERO, true));
    let mut acquire2 = Some(arm_input_leg(&ctx, 2, listener_leg2, Duration::ZERO, true));
    // A leg's listener, held from its attach until its next re-arm.
    let (mut listener1, mut listener2): (Option<SrtListener>, Option<SrtListener>) = (None, None);
    let (mut sock1, mut sock2): (Option<Arc<SrtSocket>>, Option<Arc<SrtSocket>>) = (None, None);
    let (mut poller1, mut poller2): (Option<CancellationToken>, Option<CancellationToken>) =
        (None, None);
    // A leg whose listener has failed for good.
    let (mut dead1, mut dead2) = (false, false);

    let mut pair = RedundantPair::new(PairMerger::new(&config, redundancy));
    // A buffered merger's stats, on the flow's stats as this input's for as
    // long as the loop runs. The pair keeps its merger across both legs
    // being lost, so they carry on with it.
    let _buffered_stats = pair.merger.publish_stats(&stats, input_id);

    let outcome = loop {
        // When a buffered merger next has a packet to release; never for an
        // unbuffered one.
        let drain_at = pair.merger.next_deadline().map(tokio::time::Instant::from_std);
        // A leg's recv future polls only while the leg has a socket.
        // `std::future::pending()` never resolves, so the branch stays
        // dormant until the leg attaches.
        let leg1_recv = async {
            match &sock1 {
                Some(s) => s.recv().await,
                None => std::future::pending().await,
            }
        };
        let leg2_recv = async {
            match &sock2 {
                Some(s) => s.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT input (redundant) stopping (cancelled)");
                break Ok(());
            }
            _ = tokio::time::sleep_until(drain_at.unwrap_or_else(tokio::time::Instant::now)), if drain_at.is_some() => {
                drain_redundant_pair(&mut pair, &stats, &publisher, transcoder, post);
            }
            acquired = async { acquire1.as_mut().expect("armed").await }, if acquire1.is_some() => {
                acquire1 = None;
                match acquired {
                    LegAcquired::Connected(socket, listener) => {
                        listener1 = listener;
                        connect_failures.store(0, Ordering::Relaxed);
                        poller1 = Some(attach_input_leg(&ctx, 1, &socket, srt));
                        sock1 = Some(socket);
                    }
                    // Produced only once `cancel` has fired; the cancel arm
                    // returns.
                    LegAcquired::Cancelled => {}
                    LegAcquired::Failed(e) => {
                        dead1 = true;
                        report_failed_input_leg(&ctx, 1, &e, &sock2);
                        if dead2 {
                            break Err(e);
                        }
                    }
                }
            }
            acquired = async { acquire2.as_mut().expect("armed").await }, if acquire2.is_some() => {
                acquire2 = None;
                match acquired {
                    LegAcquired::Connected(socket, listener) => {
                        listener2 = listener;
                        poller2 = Some(attach_input_leg(&ctx, 2, &socket, srt));
                        sock2 = Some(socket);
                    }
                    LegAcquired::Cancelled => {}
                    LegAcquired::Failed(e) => {
                        dead2 = true;
                        report_failed_input_leg(&ctx, 2, &e, &sock1);
                        if dead1 {
                            break Err(e);
                        }
                    }
                }
            }
            result = leg1_recv, if sock1.is_some() => {
                match result {
                    Ok(pkt) => {
                        let srctime_us = pkt.sender_timestamp_us;
                        let data = pkt.data;
                        if pair.format == SrtPayloadFormat::Unknown {
                            // Settled once (`settle_pair_format`); NULL
                            // packets alone decide nothing.
                            if !settle_pair_format(&mut pair.format, &data, events, flow_id, redundancy.path_differential_ms) {
                                continue;
                            }
                            srt.pair.set_merged(pair.format == SrtPayloadFormat::RtpTs);
                        }
                        process_redundant_packet(data, srctime_us, ActiveLeg::Leg1, pair.format, RedundantPacketState { raw_ts_seq: &mut pair.raw_ts_seq_leg1, raw_ts_timestamp: &mut pair.raw_ts_timestamp, merger: &mut pair.merger, merger_handoff: &mut pair.merger_handoff, failover: &mut pair.failover, prev_active_leg: &mut pair.prev_active_leg, stats: &stats, srt, publisher: &publisher, transcoder, post });
                    }
                    Err(_) => {
                        lose_input_leg(&ctx, 1, &mut sock1, &mut poller1, &sock2).await;
                        acquire1 = Some(arm_input_leg(&ctx, 1, listener1.take(), LEG_REACQUIRE_DELAY, false));
                        if sock2.is_none() {
                            both_input_legs_lost(events, flow_id, &mut pair);
                        }
                    }
                }
            }
            result = leg2_recv, if sock2.is_some() => {
                match result {
                    Ok(pkt) => {
                        let srctime_us = pkt.sender_timestamp_us;
                        let data = pkt.data;
                        if pair.format == SrtPayloadFormat::Unknown {
                            // Settled once (`settle_pair_format`); NULL
                            // packets alone decide nothing.
                            if !settle_pair_format(&mut pair.format, &data, events, flow_id, redundancy.path_differential_ms) {
                                continue;
                            }
                            srt.pair.set_merged(pair.format == SrtPayloadFormat::RtpTs);
                        }
                        process_redundant_packet(data, srctime_us, ActiveLeg::Leg2, pair.format, RedundantPacketState { raw_ts_seq: &mut pair.raw_ts_seq_leg2, raw_ts_timestamp: &mut pair.raw_ts_timestamp, merger: &mut pair.merger, merger_handoff: &mut pair.merger_handoff, failover: &mut pair.failover, prev_active_leg: &mut pair.prev_active_leg, stats: &stats, srt, publisher: &publisher, transcoder, post });
                    }
                    Err(_) => {
                        lose_input_leg(&ctx, 2, &mut sock2, &mut poller2, &sock1).await;
                        acquire2 = Some(arm_input_leg(&ctx, 2, listener2.take(), LEG_REACQUIRE_DELAY, false));
                        if sock1.is_none() {
                            both_input_legs_lost(events, flow_id, &mut pair);
                        }
                    }
                }
            }
        }
    };

    // Stop the pending acquisitions and let each finish beside the rest of
    // the teardown: dropped part-way through a caller's connect, one would
    // leave a connection nothing closes (`release_leg_acquire`). A pending
    // accept returns at once and closes the listener it holds. The pollers,
    // children of `legs_cancel`, stop with it; then the attached legs and
    // the held listeners close.
    legs_cancel.cancel();
    let attached = async move {
        for socket in [&sock1, &sock2].into_iter().flatten() {
            let _ = socket.close().await;
        }
        for listener in [&listener1, &listener2].into_iter().flatten() {
            let _ = listener.close().await;
        }
    };
    tokio::join!(
        release_leg_acquire(acquire1, leg_release_bound(config.connect_timeout_secs)),
        release_leg_acquire(acquire2, leg_release_bound(redundancy.connect_timeout_secs)),
        attached
    );
    outcome
}

/// What a 2022-7 input's two legs share while either is up: the merger and
/// the raw-TS failover, what of the merger's counts the pair's stats have
/// been told, the payload format and the raw-TS counters. Started afresh
/// only when a loss leaves neither leg connected ([`Self::restart`]).
struct RedundantPair {
    merger: PairMerger,
    merger_handoff: crate::stats::collector::MergerHandoff,
    failover: RawTsFailover,
    prev_active_leg: ActiveLeg,
    format: SrtPayloadFormat,
    raw_ts_seq_leg1: u16,
    raw_ts_seq_leg2: u16,
    raw_ts_timestamp: u32,
}

impl RedundantPair {
    fn new(merger: PairMerger) -> Self {
        Self {
            merger,
            merger_handoff: crate::stats::collector::MergerHandoff::default(),
            failover: RawTsFailover::new(),
            prev_active_leg: ActiveLeg::None,
            format: SrtPayloadFormat::Unknown,
            raw_ts_seq_leg1: 0,
            raw_ts_seq_leg2: 0,
            raw_ts_timestamp: 0,
        }
    }

    /// Start afresh, as each connect cycle used to: the merger on whatever
    /// stream the legs bring back ([`PairMerger::restart`]), the failover and
    /// the format detection. The merger's counts carry on, and so does what
    /// of them `srt.pair` has been handed.
    fn restart(&mut self) {
        self.merger.restart();
        self.failover = RawTsFailover::new();
        self.prev_active_leg = ActiveLeg::None;
        self.format = SrtPayloadFormat::Unknown;
        (self.raw_ts_seq_leg1, self.raw_ts_seq_leg2, self.raw_ts_timestamp) = (0, 0, 0);
    }
}

/// The merger of a 2022-7 pair whose legs carry RTP, chosen once from the
/// input's config.
enum PairMerger {
    /// No `path_differential_ms`: each seq is forwarded as it first arrives.
    Bare(HitlessMerger),
    /// Each seq is held `path_differential_ms` from its first arrival and
    /// released in seq order, so a leg that runs ahead with a hole leaves the
    /// hole to the other leg rather than putting its fills out after newer
    /// packets. Boxed: it is several times the size of the bare merger.
    Buffered(Box<BufferedHitlessMerger<RtpPacket>>),
}

impl PairMerger {
    /// Its stale time takes in the legs' latency difference
    /// ([`pair_leg_lag`]), the buffered merger's its hold as well.
    fn new(config: &SrtInputConfig, redundancy: &SrtRedundancyConfig) -> Self {
        match redundancy.path_differential_ms {
            Some(ms) => {
                tracing::info!(
                    "SRT input (2022-7): buffered merger enabled (path_differential = {ms} ms)"
                );
                Self::Buffered(Box::new(BufferedHitlessMerger::with_leg_lag(
                    Duration::from_millis(ms.into()),
                    crate::redundancy::merger::REORDER_WINDOW,
                    pair_leg_lag(config, redundancy),
                )))
            }
            None => Self::Bare(
                HitlessMerger::new().with_stale_after_us(pair_stale_after_us(config, redundancy)),
            ),
        }
    }

    /// Neither leg is connected: forget the stream, keeping the counts. A
    /// buffered merger releases what it holds at its own release times —
    /// packets the legs delivered before the loss, which dropping would lose
    /// and releasing at once would burst onto the flow, and which go out
    /// ahead of anything a leg brings back, as that arrives later and is held
    /// as long.
    fn restart(&mut self) {
        match self {
            Self::Bare(merger) => merger.restart(),
            Self::Buffered(merger) => merger.restart(),
        }
    }

    /// When the merger next has a packet to release: never for the bare one.
    fn next_deadline(&self) -> Option<std::time::Instant> {
        match self {
            Self::Bare(_) => None,
            Self::Buffered(merger) => merger.next_deadline(),
        }
    }

    /// Publish a buffered merger's stats on the flow's as `input_id`'s,
    /// until what this returns drops; the bare merger has none.
    fn publish_stats(
        &self,
        flow: &Arc<FlowStatsAccumulator>,
        input_id: &str,
    ) -> Option<crate::stats::collector::BufferedHitlessPublication> {
        match self {
            Self::Bare(_) => None,
            Self::Buffered(merger) => Some(crate::stats::collector::BufferedHitlessPublication::new(
                flow,
                input_id,
                merger.stats_handle(),
            )),
        }
    }
}

/// What arming and attaching a 2022-7 input's legs needs, borrowed for the
/// life of the input.
#[derive(Clone, Copy)]
struct RedundantInputCtx<'a> {
    config: &'a SrtInputConfig,
    redundancy: &'a SrtRedundancyConfig,
    /// The legs' own token, a child of the input's: it stops the
    /// acquisitions and the pollers, and is cancelled at teardown however
    /// the loop ended.
    cancel: &'a CancellationToken,
    events: &'a EventSender,
    flow_id: &'a str,
    input_id: &'a str,
    leg_up: &'a [AtomicBool; 2],
    connect_failures: &'a Arc<AtomicU32>,
    leg2_attempts: &'a Arc<AtomicU32>,
}

impl<'a> RedundantInputCtx<'a> {
    /// Leg `leg`'s connection parameters: leg 1 from the input's own config,
    /// leg 2 from its `redundancy` block.
    fn params(&self, leg: u8) -> SrtConnectionParams<'a> {
        if leg == 1 {
            SrtConnectionParams::from(self.config)
        } else {
            SrtConnectionParams::from(self.redundancy)
        }
    }

    fn up(&self, leg: u8) -> &'a AtomicBool {
        &self.leg_up[if leg == 1 { 0 } else { 1 }]
    }

    fn other_up(&self, leg: u8) -> &'a AtomicBool {
        &self.leg_up[if leg == 1 { 1 } else { 0 }]
    }
}

/// `details.other_leg` of a 2022-7 input's leg events, read off the other
/// leg's socket: `connected` only while its status says so, so a socket that
/// broke a moment ago reads `down` before its own loss is handled.
fn input_leg_state(socket: &Option<Arc<SrtSocket>>) -> &'static str {
    match socket {
        Some(s) if s.status() == SocketStatus::Connected => "connected",
        _ => "down",
    }
}

/// Start one acquisition of leg `leg` ([`acquire_redundant_leg`]).
///
/// A caller leg reports every failed attempt to the connect-failure observer:
/// leg 1 through the input's counter, leg 2 through a scratch one, each
/// latching its own event, a Warning while the other leg is connected. A
/// listener leg's `first` acquisition raises a Warning once it has waited
/// [`LEG_CONNECT_WARN_AFTER`] and keeps waiting; a re-arm after a loss does
/// not, as the leg-lost event has already said the leg is down.
fn arm_input_leg<'a>(
    ctx: &RedundantInputCtx<'a>,
    leg: u8,
    listener: Option<SrtListener>,
    delay: Duration,
    first: bool,
) -> LegAcquire<'a> {
    let ctx = *ctx;
    let params = ctx.params(leg);
    let other_up = ctx.other_up(leg);
    let counter = if leg == 1 { ctx.connect_failures } else { ctx.leg2_attempts };
    let observer = connect_failure_observer(
        counter,
        ctx.events,
        ctx.flow_id,
        ctx.input_id,
        serde_json::json!({
            "mode": format!("{:?}", params.mode),
            "remote_addr": params.remote_addr.unwrap_or(""),
            "stream_id": params.stream_id.unwrap_or(""),
            "leg": leg,
        }),
        Some(other_up),
    );
    let slow = (first && *params.mode == SrtMode::Listener).then(|| {
        let (mode, local_addr) = (*params.mode, params.local_addr.unwrap_or("auto"));
        let warn: Box<dyn FnOnce() + Send + 'a> = Box::new(move || {
            tracing::warn!(
                "SRT input 2022-7 leg {leg} did not connect within {:?}; still trying",
                LEG_CONNECT_WARN_AFTER
            );
            ctx.events.emit_flow_with_details(
                EventSeverity::Warning,
                category::SRT,
                format!(
                    "SRT input 2022-7 leg {leg} did not connect within {:?}; still trying",
                    LEG_CONNECT_WARN_AFTER
                ),
                ctx.flow_id,
                serde_json::json!({
                    "mode": format!("{mode:?}"),
                    "leg": leg,
                    "local_addr": local_addr,
                    "after_ms": LEG_CONNECT_WARN_AFTER.as_millis() as u64,
                    "other_leg": if other_up.load(Ordering::Relaxed) { "connected" } else { "down" },
                }),
            );
        });
        (LEG_CONNECT_WARN_AFTER, warn)
    });
    acquire_redundant_leg(params, listener, delay, ctx.cancel, observer, slow)
}

/// Attach leg `leg`'s new socket: mark it up, start its stats poller (the
/// returned token stops it) and raise its connected Info, on every attach.
fn attach_input_leg(
    ctx: &RedundantInputCtx<'_>,
    leg: u8,
    socket: &Arc<SrtSocket>,
    srt: &Arc<SrtInputStats>,
) -> CancellationToken {
    let params = ctx.params(leg);
    ctx.up(leg).store(true, Ordering::Relaxed);
    let poller = ctx.cancel.child_token();
    let (cache, which) = if leg == 1 {
        (srt.leg.clone(), SrtInputSocket::Leg1)
    } else {
        (srt.leg2.clone(), SrtInputSocket::Leg2)
    };
    spawn_srt_stats_poller(socket.clone(), cache, Some((srt.clone(), which)), poller.clone());
    let remote_addr = socket
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|| params.remote_addr.unwrap_or("").to_string());
    tracing::info!(
        "SRT input leg{leg} connected: mode={:?} local={} remote={remote_addr}",
        params.mode,
        params.local_addr.unwrap_or("auto"),
    );
    ctx.events.emit_flow_with_details(
        EventSeverity::Info,
        category::SRT,
        format!("SRT input connected (mode={:?}, redundant leg {leg})", params.mode),
        ctx.flow_id,
        serde_json::json!({
            "mode": format!("{:?}", params.mode),
            "leg": leg,
            "local_addr": params.local_addr.unwrap_or("auto"),
            "remote_addr": remote_addr,
            "stream_id": params.stream_id.unwrap_or(""),
        }),
    );
    poller
}

/// Detach lost leg `leg` and raise its Warning; the caller re-arms it.
/// `other` is the other leg's socket, read before this leg's state changes.
async fn lose_input_leg(
    ctx: &RedundantInputCtx<'_>,
    leg: u8,
    socket: &mut Option<Arc<SrtSocket>>,
    poller: &mut Option<CancellationToken>,
    other: &Option<Arc<SrtSocket>>,
) {
    tracing::warn!("SRT input leg{leg} connection lost");
    let other_leg = input_leg_state(other);
    ctx.up(leg).store(false, Ordering::Relaxed);
    if let Some(poller) = poller.take() {
        poller.cancel();
    }
    if let Some(socket) = socket.take() {
        let _ = socket.close().await;
    }
    ctx.events.emit_flow_with_details(
        EventSeverity::Warning,
        category::REDUNDANCY,
        format!("Redundant leg {leg} lost"),
        ctx.flow_id,
        serde_json::json!({
            "leg": leg,
            "mode": format!("{:?}", ctx.params(leg).mode),
            "other_leg": other_leg,
        }),
    );
}

/// A loss has left neither leg connected: raise the Critical and start the
/// pair afresh ([`RedundantPair::restart`]). Both legs are being
/// re-acquired.
fn both_input_legs_lost(events: &EventSender, flow_id: &str, pair: &mut RedundantPair) {
    tracing::warn!("SRT input (redundant) both legs lost, re-acquiring each");
    events.emit_flow(EventSeverity::Critical, category::REDUNDANCY, "Both redundant legs lost", flow_id);
    pair.restart();
}

/// Leg `leg`'s listener can never accept again (see
/// [`LegAcquired::Failed`]): raise the single-leg listener's Critical. The
/// leg is not re-armed; the other carries on.
fn report_failed_input_leg(
    ctx: &RedundantInputCtx<'_>,
    leg: u8,
    e: &anyhow::Error,
    other: &Option<Arc<SrtSocket>>,
) {
    let params = ctx.params(leg);
    tracing::error!("SRT input 2022-7 leg {leg} accept failed for good: {e}");
    ctx.events.emit_flow_with_details(
        EventSeverity::Critical,
        category::SRT,
        format!("SRT input connection failed: {e}"),
        ctx.flow_id,
        serde_json::json!({
            "mode": format!("{:?}", params.mode),
            "leg": leg,
            "local_addr": params.local_addr.unwrap_or("auto"),
            "stream_id": params.stream_id.unwrap_or(""),
            "error": e.to_string(),
            "other_leg": input_leg_state(other),
        }),
    );
}

fn log_detected_format(format: SrtPayloadFormat) {
    match format {
        SrtPayloadFormat::RtpTs => {
            tracing::info!("SRT input: detected RTP/TS format (SMPTE ST 2022-2)");
        }
        SrtPayloadFormat::RawTs => {
            tracing::info!("SRT input: detected raw TS format (OBS/Haivision/srt-live-transmit compatible)");
        }
        SrtPayloadFormat::Unknown => {
            tracing::warn!("SRT input: unrecognized payload format, treating as raw data");
        }
    }
}

/// Settle a 2022-7 pair's payload format from `data`, a datagram either leg
/// delivered while it was still unknown, as [`settle_format`] settles a
/// single socket's: data that is neither RTP nor raw TS is taken for raw TS,
/// so the format is settled once. When raw TS lands on a 2022-7-configured
/// input, real hitless dedup is impossible (no shared transport-layer seq
/// across legs) — the runtime falls back to active-leg protection-switching.
/// Emit a Warning event, once per settle, so operators understand what mode
/// they're actually getting. Returns `false`, leaving the format unknown,
/// for a datagram that cannot decide it, which the loop then drops. A
/// `path_differential_ms` the input sets has nothing to hold on raw TS
/// either, which the event says too: the message stays the same, so filters
/// on it keep matching.
fn settle_pair_format(
    format: &mut SrtPayloadFormat,
    data: &[u8],
    events: &EventSender,
    flow_id: &str,
    path_differential_ms: Option<u32>,
) -> bool {
    if !settle_format(format, data) {
        return false;
    }
    if *format == SrtPayloadFormat::RawTs {
        tracing::warn!(
            "SRT redundant input: raw TS has no shared cross-leg seq — running active-leg failover, NOT hitless dedup. \
             Configure the publisher to send RTP/TS (SMPTE ST 2022-2) for true 2022-7 hitless behaviour."
        );
        let mut details = serde_json::json!({
            "error_code": "redundancy_failover_mode",
            "format": "raw_ts",
            "fix_hint": "Configure the publisher to send RTP/TS for true SMPTE 2022-7 hitless behaviour",
        });
        if let Some(ms) = path_differential_ms {
            tracing::warn!(
                "SRT redundant input: path_differential_ms ({ms} ms) does not apply to raw TS, \
                 which has no sequence number to hold and order on"
            );
            details["path_differential_ms"] = ms.into();
            details["fix_hint"] = "Configure the publisher to send RTP/TS for true SMPTE 2022-7 \
                hitless behaviour; path_differential_ms holds and orders RTP/TS only, so it does \
                nothing on raw TS"
                .into();
        }
        events.emit_flow_with_details(
            EventSeverity::Warning,
            category::REDUNDANCY,
            "SRT redundancy on raw TS: running active-leg failover, not hitless dedup",
            flow_id,
            details,
        );
    }
    true
}

/// Stall threshold for the raw-TS active-leg failover path. If the active
/// output leg is silent for longer than this, the next packet from the
/// standby leg promotes it. Sized large enough to absorb normal jitter
/// (well above the SRT TSBPD release cadence) while still failing over
/// inside an operator-noticeable window.
const RAW_TS_FAILOVER_STALL: Duration = Duration::from_millis(50);

/// State for the raw-TS-over-SRT redundancy active-leg failover path.
///
/// Raw TS over SRT does not carry a transport-layer sequence number that
/// is shared across legs (each SRT connection numbers its packets
/// independently from its own ISN, and the TS payload itself only has a
/// 4-bit per-PID continuity counter). The synthetic per-leg counters that
/// the legacy code fed into `HitlessMerger` violated the merger's
/// precondition (shared seq across legs) and produced random dedup
/// collisions plus continuous active-leg flapping. The merger is the
/// right primitive when the legs share a real seq (RTP/TS); for raw TS
/// the honest answer is protection-switching, not hitless dedup.
///
/// Behaviour: forward packets only from `active`; switch to the other leg
/// when `active`'s last arrival is older than [`RAW_TS_FAILOVER_STALL`].
/// First packet ever seen sets `active`. The unused leg's bytes are
/// silently dropped — they would be near-duplicates of the active leg's
/// bytes (the camera/encoder pushes the same TS to both ports), and we
/// have no transport-layer key to dedup them.
struct RawTsFailover {
    active: ActiveLeg,
    last_leg1: Option<std::time::Instant>,
    last_leg2: Option<std::time::Instant>,
}

impl RawTsFailover {
    fn new() -> Self {
        Self {
            active: ActiveLeg::None,
            last_leg1: None,
            last_leg2: None,
        }
    }

    /// Returns `true` if this packet should be forwarded; updates state.
    fn admit(&mut self, leg: ActiveLeg) -> bool {
        let now = std::time::Instant::now();
        match leg {
            ActiveLeg::Leg1 => self.last_leg1 = Some(now),
            ActiveLeg::Leg2 => self.last_leg2 = Some(now),
            ActiveLeg::None => return false,
        }

        if self.active == ActiveLeg::None {
            self.active = leg;
            return true;
        }

        if self.active == leg {
            return true;
        }

        // Standby-leg packet — only promote on stall of the active leg.
        let active_last = match self.active {
            ActiveLeg::Leg1 => self.last_leg1,
            ActiveLeg::Leg2 => self.last_leg2,
            ActiveLeg::None => None,
        };
        let stalled = active_last
            .map(|t| now.duration_since(t) > RAW_TS_FAILOVER_STALL)
            .unwrap_or(true);
        if stalled {
            self.active = leg;
            true
        } else {
            false
        }
    }
}

/// The dual-leg receive path's mutable state: sequence/timestamp counters,
/// the hitless merger and failover, the pair's stats and the downstream
/// publish chain.
struct RedundantPacketState<'a> {
    raw_ts_seq: &'a mut u16,
    raw_ts_timestamp: &'a mut u32,
    merger: &'a mut PairMerger,
    /// What of the merger's counts has been handed to `srt.pair`.
    merger_handoff: &'a mut crate::stats::collector::MergerHandoff,
    failover: &'a mut RawTsFailover,
    prev_active_leg: &'a mut ActiveLeg,
    stats: &'a Arc<FlowStatsAccumulator>,
    srt: &'a SrtInputStats,
    publisher: &'a crate::engine::ingress_publisher::IngressPublisher,
    transcoder: &'a mut Option<InputTranscoder>,
    post: &'a mut Option<InputPostProcess>,
}

/// Process a single packet from a redundancy leg.
///
/// Dispatches by detected payload format:
/// - **RTP/TS**: feeds the real RTP sequence number through the pair's
///   merger ([`PairMerger`]) for true SMPTE 2022-7 dedup + gap-fill:
///   forwarded at once, or held for `path_differential_ms` and released in
///   seq order.
/// - **Raw TS / Unknown**: routes through [`RawTsFailover`] for
///   active-leg protection-switching. Real dedup is impossible without a
///   shared seq across legs.
fn process_redundant_packet(
    data: Bytes,
    srctime_us: Option<i64>,
    leg: ActiveLeg,
    format: SrtPayloadFormat,
    params: RedundantPacketState<'_>,
) {
    // Destructured back into the original bindings so the body is unchanged.
    let RedundantPacketState {
        raw_ts_seq,
        raw_ts_timestamp,
        merger,
        merger_handoff,
        failover,
        prev_active_leg,
        stats,
        srt,
        publisher,
        transcoder,
        post,
    } = params;

    // The arrival time stamped on the packet, read once: it is also the
    // merger's clock for telling a moved stream from a lagging leg.
    let recv_time_us = now_us();
    match format {
        SrtPayloadFormat::RtpTs => {
            if !is_likely_rtp(&data) {
                return;
            }
            let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
            let ts = parse_rtp_timestamp(&data).unwrap_or(0);
            let packet = RtpPacket {
                data,
                sequence_number: seq,
                rtp_timestamp: ts,
                recv_time_us,
                is_raw_ts: false,
                upstream_seq: None,
                upstream_leg_id: None,
                sender_timestamp_us: srctime_us,
            };
            match merger {
                PairMerger::Bare(merger) => {
                    let chosen = merger.try_merge(seq, leg, recv_time_us);
                    // A seq leaving the window unfilled, or a re-anchor, is
                    // rare: two compares per packet, an atomic add only when
                    // one happened.
                    srt.pair.hand_off(merger, merger_handoff);
                    if let Some(chosen_leg) = chosen {
                        publish_redundant_packet(packet, chosen_leg, prev_active_leg, stats, publisher, transcoder, post);
                    }
                }
                PairMerger::Buffered(merger) => {
                    let now = std::time::Instant::now();
                    for (chosen_leg, packet) in merger.ingest(seq, leg, packet, now) {
                        publish_redundant_packet(packet, chosen_leg, prev_active_leg, stats, publisher, transcoder, post);
                    }
                    srt.pair.hand_off(&**merger, merger_handoff);
                }
            }
        }
        SrtPayloadFormat::RawTs | SrtPayloadFormat::Unknown => {
            // What a buffered merger still holds of RTP from before both legs
            // were lost is older than anything the legs bring back: out first.
            if let PairMerger::Buffered(merger) = merger {
                for (held_leg, packet) in merger.release_all() {
                    publish_redundant_packet(packet, held_leg, prev_active_leg, stats, publisher, transcoder, post);
                }
            }
            if !failover.admit(leg) {
                return;
            }
            // The failover's leg is the one on air; it changes only on a
            // failover.
            if leg != *prev_active_leg {
                srt.pair.set_forwarding_leg2(leg == ActiveLeg::Leg2);
            }
            // Synthetic seq advances independently per leg; useful only as a
            // monotonic stamp on the published `RtpPacket` for downstream
            // bookkeeping. It is no longer used for dedup.
            let seq = *raw_ts_seq;
            *raw_ts_seq = raw_ts_seq.wrapping_add(1);
            // RFC 2250 §3.5 — 90 kHz media-clock RTP timestamp from
            // PCR; falls back to last-seen value when this datagram
            // doesn't carry a PCR-bearing TS packet.
            if let Some(pcr_27mhz) = scan_first_pcr_in_datagram(&data) {
                *raw_ts_timestamp = (pcr_27mhz / 300) as u32;
            }
            let packet = RtpPacket {
                data,
                sequence_number: seq,
                rtp_timestamp: *raw_ts_timestamp,
                recv_time_us,
                is_raw_ts: true,
                upstream_seq: None,
                upstream_leg_id: None,
                sender_timestamp_us: srctime_us,
            };
            publish_redundant_packet(packet, leg, prev_active_leg, stats, publisher, transcoder, post);
        }
    }
}

/// Publish a packet the pair forwards from `leg`: count a switch of leg, the
/// input's packets and bytes, then the bandwidth gate and the publish chain.
fn publish_redundant_packet(
    packet: RtpPacket,
    leg: ActiveLeg,
    prev_active_leg: &mut ActiveLeg,
    stats: &FlowStatsAccumulator,
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    if *prev_active_leg != ActiveLeg::None && leg != *prev_active_leg {
        stats
            .redundancy_switches
            .fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            "SRT input redundancy switch: {:?} -> {:?} at seq {}",
            prev_active_leg,
            leg,
            packet.sequence_number
        );
    }
    *prev_active_leg = leg;

    stats.input_packets.fetch_add(1, Ordering::Relaxed);
    stats
        .input_bytes
        .fetch_add(packet.data.len() as u64, Ordering::Relaxed);

    // Bandwidth limit enforcement: drop packet if flow is blocked
    if stats.bandwidth_blocked.load(Ordering::Relaxed) {
        stats.input_filtered.fetch_add(1, Ordering::Relaxed);
        return;
    }

    crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, publisher, packet);
}

/// Release what a buffered pair merger holds past its release time. Nothing
/// for an unbuffered merger, which holds nothing. The flow's stats read the
/// merger's counters where it writes them ([`PairMerger::publish_stats`]), so
/// a drain takes no lock to refresh a copy.
fn drain_redundant_pair(
    pair: &mut RedundantPair,
    stats: &FlowStatsAccumulator,
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) {
    let PairMerger::Buffered(merger) = &mut pair.merger else {
        return;
    };
    for (leg, packet) in merger.drain_expired(std::time::Instant::now()) {
        publish_redundant_packet(packet, leg, &mut pair.prev_active_leg, stats, publisher, transcoder, post);
    }
}

// ---------------------------------------------------------------------------
// Native libsrt SRT bonding (socket groups)
// ---------------------------------------------------------------------------

/// Entry point for an SRT input whose config has a `bonding` block.
///
/// Dispatches between caller-side bonding (the edge initiates a group
/// connection to N peer endpoints) and listener-side bonding (the edge
/// listens on one bind address with `SRTO_GROUPCONNECT=true` and accepts
/// bonded callers).
async fn srt_input_bonded_loop(
    config: SrtInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    srt: &Arc<SrtInputStats>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => {
            srt_input_bonded_listener_loop(
                config, publisher, stats, srt, cancel, events, flow_id, input_id, transcoder, post,
            )
            .await
        }
        _ => {
            srt_input_bonded_caller_loop(
                config, publisher, stats, srt, cancel, events, flow_id, input_id, transcoder, post,
            )
            .await
        }
    }
}

async fn srt_input_bonded_caller_loop(
    config: SrtInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    srt: &Arc<SrtInputStats>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let bond = config.bonding.as_ref().expect("bonding must be set").clone();
    let mut recv_state = SrtRecvState::new();
    // Validation guarantees the plan; if it cannot be made, the connect
    // below fails with the same error and reports it.
    let plans = plan_bond_members(&bond).unwrap_or_default();
    // Outlives each connected group, so a whole-group reconnect neither
    // repeats a warning nor loses a pending "restored".
    let latch = Arc::new(BondLatch::default());
    let bond_events = BondEvents::new(
        BondEventScope::Input { flow_id: flow_id.to_string(), input_id: input_id.to_string() },
        events.clone(),
        bond.mode,
        "caller-bonded",
    );

    // Group-connect consecutive-failure counter (contract C3 /
    // `connect_failed` state) — one counter per input, bumped per
    // group-connect attempt.
    let connect_failures = stats
        .per_input_counters
        .get(input_id)
        .map(|c| c.connect_failures.clone())
        .unwrap_or_default();

    loop {
        let params = SrtConnectionParams::from(&config);
        let mut on_attempt_failed = connect_failure_observer(
            &connect_failures,
            events,
            flow_id,
            input_id,
            serde_json::json!({
                "mode": "caller-bonded",
                "bonding_mode": format!("{:?}", bond.mode),
                "endpoints": bond.endpoints.iter().map(|e| &e.addr).collect::<Vec<_>>(),
                // Each member's source as this connect cycle resolves it: a
                // source with no route to the peer is the usual reason a
                // NIC-pinned bond never connects.
                "members": member_sources_json(&plans),
            }),
            None,
        );
        let group = match connect_srt_group_observed(&params, &bond, &cancel, &mut on_attempt_failed).await {
            Ok(g) => g,
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                tracing::error!("SRT bonded input connect failed: {e}");
                events.emit_flow_with_details(
                    EventSeverity::Critical,
                    category::SRT,
                    format!("SRT bonded input connection failed: {e}"),
                    flow_id,
                    serde_json::json!({
                        "mode": "caller-bonded",
                        "bonding_mode": format!("{:?}", bond.mode),
                        "endpoints": bond.endpoints.iter().map(|e| &e.addr).collect::<Vec<_>>(),
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
        };
        connect_failures.store(0, Ordering::Relaxed);

        tracing::info!(
            "SRT bonded input connected: mode=caller bonding={:?} endpoints={}",
            bond.mode,
            bond.endpoints.len(),
        );
        events.emit_flow_with_details(
            EventSeverity::Info,
            category::SRT,
            "SRT bonded input connected (caller)",
            flow_id,
            serde_json::json!({
                "mode": "caller-bonded",
                "bonding_mode": format!("{:?}", bond.mode),
                "endpoints": bond.endpoints.iter().map(|e| &e.addr).collect::<Vec<_>>(),
            }),
        );

        let poller_cancel = cancel.child_token();
        let supervisor = (!plans.is_empty()).then(|| {
            BondSupervisor::new(
                plans.clone(),
                bond_events.clone(),
                latch.clone(),
                degrade_after(params.connect_timeout_secs),
                std::time::Instant::now(),
            )
        });
        let poller = spawn_srt_group_stats_poller(
            group.clone(),
            bond.mode,
            srt.bonding.clone(),
            Some(srt.clone()),
            poller_cancel.clone(),
            supervisor,
        );

        let disconnected = srt_bonded_group_recv_loop(
            &group,
            &publisher,
            &stats,
            srt,
            &cancel,
            &mut recv_state,
            transcoder,
            post,
        )
        .await?;
        poller_cancel.cancel();
        // Join the poller before the close: it hands back the supervisor
        // whose member tracks the report is built from.
        let members = if disconnected {
            final_member_report(poller).await
        } else {
            serde_json::Value::Null
        };
        let _ = group.close().await;

        if !disconnected {
            break;
        }

        // Each member's source and, for one never live, why: a bond whose
        // handshakes never complete connects and fails whole, and this is
        // the only event that reports it.
        events.emit_flow_with_details(
            EventSeverity::Warning,
            category::SRT,
            "SRT bonded input disconnected, reconnecting",
            flow_id,
            serde_json::json!({ "mode": "caller-bonded", "members": members }),
        );
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }

    Ok(())
}

async fn srt_input_bonded_listener_loop(
    config: SrtInputConfig,
    publisher: crate::engine::ingress_publisher::IngressPublisher,
    stats: Arc<FlowStatsAccumulator>,
    srt: &Arc<SrtInputStats>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_id: &str,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<()> {
    let bond = config.bonding.as_ref().expect("bonding must be set").clone();
    let mut listener = bind_srt_listener_for_bonded_input(&config).await?;
    let mut recv_state = SrtRecvState::new();

    loop {
        let socket = match accept_srt_connection(&mut listener, &cancel).await {
            Ok(s) => s,
            Err(_) if cancel.is_cancelled() => {
                let _ = listener.close().await;
                return Ok(());
            }
            Err(e) => {
                tracing::error!("SRT bonded input accept failed: {e}");
                events.emit_flow_with_details(
                    EventSeverity::Critical,
                    category::SRT,
                    format!("SRT bonded input accept failed: {e}"),
                    flow_id,
                    serde_json::json!({
                        "mode": "listener-bonded",
                        "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                        "error": e.to_string(),
                    }),
                );
                let _ = listener.close().await;
                return Err(e);
            }
        };

        tracing::info!(
            "SRT bonded input connected: mode=listener local={}",
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_flow_with_details(
            EventSeverity::Info,
            category::SRT,
            "SRT bonded input connected (listener)",
            flow_id,
            serde_json::json!({
                "mode": "listener-bonded",
                "bonding_mode": format!("{:?}", bond.mode),
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );

        let poller_cancel = cancel.child_token();
        spawn_srt_socket_group_stats_poller(
            socket.clone(),
            bond.mode,
            srt.bonding.clone(),
            Some(srt.clone()),
            poller_cancel.clone(),
            Some(BondHealthTracker::new(
                BondEvents::new(
                    BondEventScope::Input {
                        flow_id: flow_id.to_string(),
                        input_id: input_id.to_string(),
                    },
                    events.clone(),
                    bond.mode,
                    "listener-bonded",
                ),
                degrade_after(config.connect_timeout_secs),
            )),
        );

        let disconnected = srt_input_recv_loop(
            &socket,
            &publisher,
            &stats,
            srt,
            &cancel,
            &mut recv_state,
            transcoder,
            post,
        )
        .await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            let _ = listener.close().await;
            return Ok(());
        }

        events.emit_flow_with_details(
            EventSeverity::Warning,
            category::SRT,
            "SRT bonded input disconnected, reconnecting",
            flow_id,
            serde_json::json!({ "mode": "listener-bonded" }),
        );
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = listener.close().await;
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

/// Receive loop for a bonded caller-side SRT group. Mirrors
/// [`srt_input_recv_loop`] but reads from [`srt_transport::SrtGroup`]
/// instead of an individual socket.
async fn srt_bonded_group_recv_loop(
    group: &Arc<srt_transport::SrtGroup>,
    publisher: &crate::engine::ingress_publisher::IngressPublisher,
    stats: &Arc<FlowStatsAccumulator>,
    srt: &SrtInputStats,
    cancel: &CancellationToken,
    state: &mut SrtRecvState,
    transcoder: &mut Option<InputTranscoder>,
    post: &mut Option<InputPostProcess>,
) -> anyhow::Result<bool> {
    let SrtRecvState { format, last_seq, raw_ts_seq_counter, raw_ts_timestamp } = state;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(false),
            result = group.recv() => {
                match result {
                    Ok(pkt) => {
                        let srctime_us = pkt.sender_timestamp_us;
                        let data = pkt.data;
                        if *format == SrtPayloadFormat::Unknown {
                            if !settle_format(format, &data) {
                                continue;
                            }
                            // As in `srt_input_recv_loop`: an RTP payload's
                            // drops are sequence gaps too.
                            srt.rtp_framed.store(*format == SrtPayloadFormat::RtpTs, Ordering::Relaxed);
                        }
                        let (seq, ts, is_raw) = match *format {
                            SrtPayloadFormat::RtpTs => {
                                if !is_likely_rtp(&data) {
                                    continue;
                                }
                                let seq = parse_rtp_sequence_number(&data).unwrap_or(0);
                                let ts = parse_rtp_timestamp(&data).unwrap_or(0);
                                (seq, ts, false)
                            }
                            SrtPayloadFormat::RawTs | SrtPayloadFormat::Unknown => {
                                let seq = *raw_ts_seq_counter;
                                *raw_ts_seq_counter = raw_ts_seq_counter.wrapping_add(1);
                                // RFC 2250 §3.5 — 90 kHz media-clock RTP
                                // timestamp from PCR; falls back to
                                // last-seen value when this datagram
                                // doesn't carry a PCR-bearing TS packet.
                                if let Some(pcr_27mhz) =
                                    scan_first_pcr_in_datagram(&data)
                                {
                                    *raw_ts_timestamp = (pcr_27mhz / 300) as u32;
                                }
                                (seq, *raw_ts_timestamp, true)
                            }
                        };

                        if let Some(prev) = *last_seq {
                            let expected = prev.wrapping_add(1);
                            if seq != expected {
                                let gap = seq.wrapping_sub(expected) as u64;
                                if gap > 0 && gap < 1000 {
                                    stats.input_loss.fetch_add(gap, Ordering::Relaxed);
                                }
                            }
                        }
                        *last_seq = Some(seq);

                        stats.input_packets.fetch_add(1, Ordering::Relaxed);
                        stats.input_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

                        if stats.bandwidth_blocked.load(Ordering::Relaxed) {
                            stats.input_filtered.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }

                        let packet = RtpPacket {
                            data,
                            sequence_number: seq,
                            rtp_timestamp: ts,
                            recv_time_us: now_us(),
                            is_raw_ts: is_raw,
                            upstream_seq: None,
                            upstream_leg_id: None,
                            sender_timestamp_us: srctime_us,
                        };
                        crate::engine::input_transcode::publish_input_packet_smoothed(transcoder, post, publisher, packet);
                    }
                    Err(_) => {
                        tracing::warn!("SRT bonded input connection lost, will reconnect");
                        return Ok(true);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SMPTE 2022-7 input over real SRT sockets: each leg has a stats
    /// poller, and the flow loses only the packets neither leg delivered —
    /// one leg's gaps are filled from the other. The senders never sent the
    /// one both legs lack, so libsrt dropped nothing: the flow counts it as a
    /// sequence gap, not an SRT drop.
    #[tokio::test]
    async fn a_2022_7_input_reports_both_legs_and_only_the_pairs_loss() {
        let free_port = || {
            std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
        };
        let (port1, port2) = (free_port(), free_port());
        let srt_config = |value: serde_json::Value| -> SrtInputConfig {
            serde_json::from_value(value).expect("SRT config fixture")
        };
        // A latency well above loopback scheduling jitter on a loaded test
        // host, so libsrt drops nothing of its own.
        let config = srt_config(serde_json::json!({
            "mode": "listener", "local_addr": format!("127.0.0.1:{port1}"), "latency_ms": 500,
            "redundancy": {
                "mode": "listener", "local_addr": format!("127.0.0.1:{port2}"), "latency_ms": 500
            }
        }));
        let flow = Arc::new(FlowStatsAccumulator::new("f1".into(), "flow-1".into(), "srt".into()));
        flow.set_active_input_id("pair");
        let (tx, _rx) = broadcast::channel(4096);
        let (events, _events_rx) = crate::manager::events::event_channel();
        let cancel = CancellationToken::new();
        let _stop = cancel.clone().drop_guard();
        let _input = spawn_srt_input(
            config,
            tx,
            flow.clone(),
            cancel.child_token(),
            events,
            "f1".into(),
            "pair".into(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        );
        let caller = |port: u16| {
            srt_config(serde_json::json!({
                "mode": "caller", "remote_addr": format!("127.0.0.1:{port}"), "latency_ms": 500
            }))
        };
        let leg1 = crate::srt::connection::connect_srt_input(&caller(port1), &cancel)
            .await
            .expect("leg 1 connects");
        let leg2 = crate::srt::connection::connect_srt_input(&caller(port2), &cancel)
            .await
            .expect("leg 2 connects");

        // RTP/TS, six null packets a datagram: seven and the RTP header would
        // overflow SRT's 1316-byte live payload. Leg 1 misses 100 and 200,
        // leg 2 misses 200 and 300, upstream of SRT: only 200 is lost, once
        // it leaves the merger's 1024-seq window.
        let datagram = |seq: u16| {
            let mut d = vec![0x80, 33];
            d.extend_from_slice(&seq.to_be_bytes());
            d.extend_from_slice(&(u32::from(seq) * 90).to_be_bytes());
            d.extend_from_slice(&[0, 0, 0, 1]);
            for _ in 0..6 {
                d.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
                d.extend_from_slice(&[0xFF; 184]);
            }
            d
        };
        for seq in 0..1400u16 {
            if seq != 100 && seq != 200 {
                leg1.send(&datagram(seq)).await.expect("leg 1 sends");
            }
            if seq != 200 && seq != 300 {
                leg2.send(&datagram(seq)).await.expect("leg 2 sends");
            }
            if seq % 20 == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        let pair = flow.srt_input_stats.get("pair").expect("registered").value().clone();
        let mut snap = flow.snapshot();
        for _ in 0..500 {
            snap = flow.snapshot();
            if snap.input.srt_stats.is_some()
                && snap.input.srt_leg2_stats.is_some()
                && snap.input.packets_lost > 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(snap.input.srt_stats.is_some(), "leg 1 publishes its stats");
        assert!(snap.input.srt_leg2_stats.is_some(), "leg 2 publishes its stats");
        // Past another poll of each leg, nothing more is counted.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(flow.input_packets.load(Ordering::Relaxed), 1399, "each seq forwarded once");
        let snap = flow.snapshot();
        assert_eq!(snap.input.packets_lost, 1, "only 200 is the pair's loss");
        assert_eq!(pair.recv_dropped(), 0, "and SRT dropped none of it");
        let codes: Vec<&str> = snap.health_reasons.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"packet_loss") && !codes.contains(&"srt_recv_dropped"), "{codes:?}");
    }

    /// One RTP/TS datagram numbered `seq`: six null packets, since seven and
    /// the RTP header would overflow SRT's 1316-byte live payload.
    fn rtp_datagram(seq: u16) -> Vec<u8> {
        let mut d = vec![0x80, 33];
        d.extend_from_slice(&seq.to_be_bytes());
        d.extend_from_slice(&(u32::from(seq) * 90).to_be_bytes());
        d.extend_from_slice(&[0, 0, 0, 1]);
        for _ in 0..6 {
            d.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
            d.extend_from_slice(&[0xFF; 184]);
        }
        d
    }

    fn free_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// Seven raw TS packets on PID 0x100: a raw-TS sender's datagram.
    fn raw_ts_datagram() -> Vec<u8> {
        let mut d = Vec::with_capacity(7 * TS_PACKET_SIZE);
        for _ in 0..7 {
            d.extend_from_slice(&[0x47, 0x01, 0x00, 0x10]);
            d.extend_from_slice(&[0xFF; 184]);
        }
        d
    }

    /// The flow's idle keepalive, as an SRT output sends it.
    fn keepalive() -> Vec<u8> {
        crate::engine::flow::null_ts_packet().data.to_vec()
    }

    /// NULL packets alone settle no format: the datagram after them does,
    /// RTP or raw TS, and data that is neither still falls back to raw TS.
    #[test]
    fn null_packets_alone_settle_no_format() {
        assert_eq!(detect_format(&keepalive()), None);
        assert_eq!(detect_format(&keepalive()[..TS_PACKET_SIZE]), None, "one NULL packet");
        assert_eq!(detect_format(&rtp_datagram(0)), Some(SrtPayloadFormat::RtpTs));
        assert_eq!(detect_format(&raw_ts_datagram()), Some(SrtPayloadFormat::RawTs));
        assert_eq!(detect_format(&[0u8; 64]), Some(SrtPayloadFormat::Unknown));

        for (then, settles) in [
            (rtp_datagram(0), SrtPayloadFormat::RtpTs),
            (raw_ts_datagram(), SrtPayloadFormat::RawTs),
            (vec![0u8; 64], SrtPayloadFormat::RawTs),
        ] {
            let mut format = SrtPayloadFormat::Unknown;
            assert!(!settle_format(&mut format, &keepalive()), "dropped");
            assert!(!settle_format(&mut format, &keepalive()));
            assert_eq!(format, SrtPayloadFormat::Unknown);
            assert!(settle_format(&mut format, &then));
            assert_eq!(format, settles);
        }
    }

    /// A single-socket SRT listener input fed `datagrams` in order by a
    /// caller, and what it published once `expect` packets were (or 5 s
    /// passed), plus half a second more to show nothing else comes.
    async fn published_by_single_input(
        datagrams: &[Vec<u8>],
        expect: usize,
    ) -> (Vec<RtpPacket>, Arc<FlowStatsAccumulator>) {
        let port = free_port();
        let flow = Arc::new(FlowStatsAccumulator::new("f1".into(), "flow-1".into(), "srt".into()));
        flow.set_active_input_id("in");
        let (tx, mut rx) = broadcast::channel(4096);
        let (events, _events_rx) = crate::manager::events::event_channel();
        let cancel = CancellationToken::new();
        let _stop = cancel.clone().drop_guard();
        spawn_srt_input(
            srt_config(serde_json::json!({
                "mode": "listener", "local_addr": format!("127.0.0.1:{port}"), "latency_ms": 200
            })),
            tx,
            flow.clone(),
            cancel.child_token(),
            events,
            "f1".into(),
            "in".into(),
            Arc::new(AtomicBool::new(false)),
            None,
        );
        let caller = srt_config(serde_json::json!({
            "mode": "caller", "remote_addr": format!("127.0.0.1:{port}"), "latency_ms": 200
        }));
        let sender = tokio::time::timeout(
            Duration::from_secs(10),
            crate::srt::connection::connect_srt_input(&caller, &cancel),
        )
        .await
        .expect("caller connects in time")
        .expect("caller connects");
        for d in datagrams {
            sender.send(d).await.expect("caller sends");
        }
        let mut published = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while published.len() < expect {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(packet)) => published.push(packet),
                _ => break,
            }
        }
        while let Ok(Ok(packet)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            published.push(packet);
        }
        (published, flow)
    }

    /// The flow's keepalive reaching an input ahead of an RTP sender's first
    /// datagram settles nothing: the RTP after it is published as RTP, with
    /// its header, and no keepalive is published or counted. A keepalive
    /// once the format is RTP is dropped as before. The input used to settle
    /// raw TS on it and publish every RTP header inside the TS.
    #[tokio::test]
    async fn a_keepalive_ahead_of_rtp_leaves_the_rtp_to_settle_the_format() {
        let mut datagrams = vec![keepalive(), keepalive()];
        datagrams.extend((0..5).map(rtp_datagram));
        datagrams.push(keepalive());
        datagrams.extend((5..10).map(rtp_datagram));
        let (published, flow) = published_by_single_input(&datagrams, 10).await;
        assert_eq!(published.len(), 10, "each RTP datagram, nothing else");
        for (seq, packet) in published.iter().enumerate() {
            assert!(!packet.is_raw_ts, "published as RTP");
            assert_eq!(packet.sequence_number, seq as u16);
            assert_eq!(&packet.data[..], &rtp_datagram(seq as u16)[..]);
        }
        assert_eq!(flow.input_packets.load(Ordering::Relaxed), 10, "keepalives uncounted");
        assert_eq!(flow.input_loss.load(Ordering::Relaxed), 0);
    }

    /// The same keepalive ahead of a raw-TS sender's first datagram: the raw
    /// TS settles the format, and once it has a keepalive is published like
    /// any raw TS.
    #[tokio::test]
    async fn a_keepalive_ahead_of_raw_ts_leaves_the_raw_ts_to_settle_the_format() {
        let datagrams = [keepalive(), raw_ts_datagram(), keepalive(), raw_ts_datagram()];
        let (published, flow) = published_by_single_input(&datagrams, 3).await;
        let got: Vec<&[u8]> = published.iter().map(|p| &p.data[..]).collect();
        let raw = raw_ts_datagram();
        let ka = keepalive();
        assert_eq!(got, [&raw[..], &ka[..], &raw[..]]);
        assert!(published.iter().all(|p| p.is_raw_ts));
        assert_eq!(flow.input_packets.load(Ordering::Relaxed), 3);
    }

    /// A 2022-7 SRT listener input on loopback with a caller on each leg.
    struct PairInput {
        published: broadcast::Receiver<RtpPacket>,
        events: tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        leg1: Arc<SrtSocket>,
        leg2: Arc<SrtSocket>,
        _stop: tokio_util::sync::DropGuard,
    }

    impl PairInput {
        async fn start() -> Self {
            let (port1, port2) = (free_port(), free_port());
            let flow =
                Arc::new(FlowStatsAccumulator::new("f1".into(), "flow-1".into(), "srt".into()));
            flow.set_active_input_id("pair");
            let (tx, published) = broadcast::channel(4096);
            let (events, events_rx) = crate::manager::events::event_channel();
            let cancel = CancellationToken::new();
            spawn_srt_input(
                srt_config(serde_json::json!({
                    "mode": "listener", "local_addr": format!("127.0.0.1:{port1}"),
                    "latency_ms": 200,
                    "redundancy": {
                        "mode": "listener", "local_addr": format!("127.0.0.1:{port2}"),
                        "latency_ms": 200
                    }
                })),
                tx,
                flow,
                cancel.child_token(),
                events,
                "f1".into(),
                "pair".into(),
                Arc::new(AtomicBool::new(false)),
                None,
            );
            let caller = |port: u16| {
                srt_config(serde_json::json!({
                    "mode": "caller", "remote_addr": format!("127.0.0.1:{port}"), "latency_ms": 200
                }))
            };
            let leg1 = crate::srt::connection::connect_srt_input(&caller(port1), &cancel)
                .await
                .expect("leg 1 connects");
            let leg2 = crate::srt::connection::connect_srt_input(&caller(port2), &cancel)
                .await
                .expect("leg 2 connects");
            Self { published, events: events_rx, leg1, leg2, _stop: cancel.drop_guard() }
        }

        /// What the pair published once `expect` packets were (or 5 s
        /// passed), plus half a second more to show nothing else comes.
        async fn published(&mut self, expect: usize) -> Vec<RtpPacket> {
            let mut published = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while published.len() < expect {
                match tokio::time::timeout_at(deadline, self.published.recv()).await {
                    Ok(Ok(packet)) => published.push(packet),
                    _ => break,
                }
            }
            while let Ok(Ok(packet)) =
                tokio::time::timeout(Duration::from_millis(500), self.published.recv()).await
            {
                published.push(packet);
            }
            published
        }

        /// The `error_code` of every event raised so far.
        fn error_codes(&mut self) -> Vec<String> {
            let mut codes = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                if let Some(code) = event.details.as_ref().and_then(|d| d["error_code"].as_str()) {
                    codes.push(code.to_string());
                }
            }
            codes
        }
    }

    /// A 2022-7 pair whose first datagram is the flow's keepalive, on leg 1,
    /// settles on the RTP both legs send after it: each seq is published
    /// once as RTP and no raw-TS failover Warning is raised. It used to
    /// settle raw TS there, run active-leg failover and publish every RTP
    /// header inside the TS until both legs dropped.
    #[tokio::test]
    async fn a_keepalive_first_on_a_2022_7_pair_leaves_the_rtp_to_settle_the_format() {
        let mut pair = PairInput::start().await;
        pair.leg1.send(&keepalive()).await.expect("leg 1 sends");
        // Delivered a latency after it is sent, well ahead of the RTP.
        tokio::time::sleep(Duration::from_millis(100)).await;
        for seq in 0..20u16 {
            pair.leg1.send(&rtp_datagram(seq)).await.expect("leg 1 sends");
            pair.leg2.send(&rtp_datagram(seq)).await.expect("leg 2 sends");
        }

        let published = pair.published(20).await;
        assert_eq!(published.len(), 20, "each seq once, nothing else");
        for (seq, packet) in published.iter().enumerate() {
            assert!(!packet.is_raw_ts, "published as RTP");
            assert_eq!(&packet.data[..], &rtp_datagram(seq as u16)[..]);
        }
        let codes = pair.error_codes();
        assert!(!codes.iter().any(|c| c == "redundancy_failover_mode"), "{codes:?}");
    }

    /// A 2022-7 pair fed data that is neither RTP nor TS takes it for raw TS
    /// once, as a single-socket input does: one `redundancy_failover_mode`
    /// Warning for the lot, and each datagram published through the raw-TS
    /// failover. The pair used to leave its format unknown, detect it again
    /// on every datagram and raise the Warning on each, at packet rate.
    #[tokio::test]
    async fn unrecognised_data_settles_a_2022_7_pair_once() {
        let mut pair = PairInput::start().await;
        for _ in 0..20 {
            pair.leg1.send(&[0u8; 64]).await.expect("leg 1 sends");
        }

        let published = pair.published(20).await;
        assert_eq!(published.len(), 20, "each datagram once");
        assert!(published.iter().all(|p| p.is_raw_ts && p.data[..] == [0u8; 64]));
        let codes = pair.error_codes();
        let warnings = codes.iter().filter(|c| *c == "redundancy_failover_mode").count();
        assert_eq!(warnings, 1, "{codes:?}");
    }

    fn srt_config(value: serde_json::Value) -> SrtInputConfig {
        serde_json::from_value(value).expect("SRT config fixture")
    }

    /// A 2022-7 SRT input, a pump sending one rising RTP seq every 2 ms to
    /// whichever test-side peers are present, and the events the input
    /// raised.
    struct InputPairHarness {
        flow: Arc<FlowStatsAccumulator>,
        events_rx: tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        events: Vec<crate::manager::events::Event>,
        ports: (u16, u16),
        /// The test-side peer of each leg the pump sends to.
        legs: Arc<std::sync::Mutex<[Option<Arc<SrtSocket>>; 2]>>,
        /// How many seqs the pump sent to at least one leg. A seq is used
        /// only once it is, so the seqs sent are exactly `0..sent`.
        sent: Arc<std::sync::atomic::AtomicU64>,
        pump: CancellationToken,
        cancel: CancellationToken,
        _rx: broadcast::Receiver<RtpPacket>,
    }

    impl InputPairHarness {
        /// Both legs listening on loopback.
        fn listeners() -> Self {
            let ports = (free_port(), free_port());
            Self::start(
                serde_json::json!({
                    "mode": "listener", "local_addr": format!("127.0.0.1:{}", ports.0),
                    "latency_ms": 200,
                    "redundancy": {
                        "mode": "listener", "local_addr": format!("127.0.0.1:{}", ports.1),
                        "latency_ms": 200
                    }
                }),
                ports,
            )
        }

        fn start(config: serde_json::Value, ports: (u16, u16)) -> Self {
            let flow =
                Arc::new(FlowStatsAccumulator::new("f1".into(), "flow-1".into(), "srt".into()));
            flow.set_active_input_id("pair");
            let (tx, rx) = broadcast::channel(4096);
            let (events, events_rx) = crate::manager::events::event_channel();
            let cancel = CancellationToken::new();
            spawn_srt_input(
                srt_config(config),
                tx,
                flow.clone(),
                cancel.child_token(),
                events,
                "f1".into(),
                "pair".into(),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                None,
            );
            let legs: Arc<std::sync::Mutex<[Option<Arc<SrtSocket>>; 2]>> = Arc::default();
            let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let pump = cancel.child_token();
            let (to, count, stop) = (legs.clone(), sent.clone(), pump.clone());
            tokio::spawn(async move {
                let mut seq: u16 = 0;
                let mut tick = tokio::time::interval(Duration::from_millis(2));
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tick.tick() => {
                            let peers: Vec<Arc<SrtSocket>> =
                                to.lock().unwrap().iter().flatten().cloned().collect();
                            let datagram = rtp_datagram(seq);
                            let mut sent_to_one = false;
                            for peer in &peers {
                                sent_to_one |= peer.send(&datagram).await.is_ok();
                            }
                            if sent_to_one {
                                count.fetch_add(1, Ordering::Relaxed);
                                seq = seq.wrapping_add(1);
                            }
                        }
                    }
                }
            });
            Self {
                flow,
                events_rx,
                events: Vec::new(),
                ports,
                legs,
                sent,
                pump,
                cancel,
                _rx: rx,
            }
        }

        /// A caller to leg `leg`'s listener, added to the pump.
        async fn connect(&self, leg: usize) -> Arc<SrtSocket> {
            let port = if leg == 1 { self.ports.0 } else { self.ports.1 };
            let config = srt_config(serde_json::json!({
                "mode": "caller", "remote_addr": format!("127.0.0.1:{port}"), "latency_ms": 200
            }));
            let socket = tokio::time::timeout(
                Duration::from_secs(10),
                crate::srt::connection::connect_srt_input(&config, &self.cancel),
            )
            .await
            .expect("the caller connects in time")
            .expect("the caller connects");
            self.legs.lock().unwrap()[leg - 1] = Some(socket.clone());
            socket
        }

        /// Take leg `leg`'s peer out of the pump, then close it, so every
        /// seq the pump sends from then on goes to the other leg alone.
        async fn drop_leg(&self, leg: usize) {
            let peer = self.legs.lock().unwrap()[leg - 1].take();
            if let Some(peer) = peer {
                let _ = peer.close().await;
            }
        }

        fn sent(&self) -> u64 {
            self.sent.load(Ordering::Relaxed)
        }

        /// Wait until the input has forwarded every seq sent so far. A seq
        /// sent while only one leg's peer existed is held by that leg's
        /// TSBPD for its latency, and closing the peer before then discards
        /// it — an SRT property, not the pair's loss — so a test settles
        /// before it drops the leg that carried those seqs alone.
        async fn settle(&mut self) {
            let sent = self.sent();
            self.wait_until("the input to catch up", |h| h.forwarded() >= sent).await;
        }

        fn forwarded(&self) -> u64 {
            self.flow.input_packets.load(Ordering::Relaxed)
        }

        /// Stop the pump and give the input time to deliver what it sent
        /// (the 200 ms latency) and to poll each leg once more.
        async fn stop_pump(&self) {
            self.pump.cancel();
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }

        fn pump_events(&mut self) {
            while let Ok(event) = self.events_rx.try_recv() {
                self.events.push(event);
            }
        }

        async fn wait_until(&mut self, what: &str, cond: impl Fn(&Self) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                self.pump_events();
                if cond(self) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        fn leg_stats(&self, leg: usize) -> bool {
            let snap = self.flow.snapshot();
            if leg == 1 { snap.input.srt_stats.is_some() } else { snap.input.srt_leg2_stats.is_some() }
        }

        /// "Redundant leg N lost" events, in the order they were raised.
        fn leg_lost(&self, leg: u64) -> Vec<&crate::manager::events::Event> {
            self.events
                .iter()
                .filter(|e| e.message == format!("Redundant leg {leg} lost"))
                .collect()
        }

        fn both_lost(&self) -> usize {
            self.events.iter().filter(|e| e.message == "Both redundant legs lost").count()
        }

        /// Leg `leg`'s connected Infos: one per attach.
        fn connected(&self, leg: u64) -> Vec<&crate::manager::events::Event> {
            self.events
                .iter()
                .filter(|e| {
                    e.severity == EventSeverity::Info
                        && e.message.starts_with("SRT input connected")
                        && e.details.as_ref().and_then(|d| d["leg"].as_u64()) == Some(leg)
                })
                .collect()
        }

        /// Every seq the pump sent was forwarded exactly once, and the pair
        /// lost none of them.
        fn assert_each_seq_forwarded_once(&self) {
            assert_eq!(self.forwarded(), self.sent(), "each seq the pump sent forwarded once");
            assert_eq!(self.flow.snapshot().input.packets_lost, 0, "and none of them lost");
        }
    }

    impl Drop for InputPairHarness {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    fn other_leg(event: &crate::manager::events::Event) -> &str {
        event.details.as_ref().and_then(|d| d["other_leg"].as_str()).expect("other_leg")
    }

    /// Leg 1's caller never comes: leg 2 alone carries the input from the
    /// start. Before edge#182 every connect cycle waited on leg 1's accept,
    /// with no timeout, before leg 2 was even tried, so the input stayed off
    /// air on a healthy leg 2.
    #[tokio::test]
    async fn a_2022_7_input_carries_on_leg_2_while_leg_1_never_connects() {
        let mut h = InputPairHarness::listeners();
        h.connect(2).await;
        h.wait_until("leg 2 to carry", |h| h.forwarded() > 100).await;
        h.wait_until("leg 2's stats", |h| h.leg_stats(2)).await;
        assert!(!h.leg_stats(1), "leg 1 never connected");
        let up = h.connected(2)[0];
        assert_eq!(up.message, "SRT input connected (mode=Listener, redundant leg 2)");
        let details = up.details.as_ref().unwrap();
        assert_eq!(details["mode"], "Listener");
        assert_eq!(details["local_addr"], format!("127.0.0.1:{}", h.ports.1));
        assert!(h.connected(1).is_empty());
        assert_eq!(h.both_lost(), 0);
    }

    /// Leg 2 blips and is re-accepted while leg 1 carries; then leg 1 is
    /// lost and leg 2 carries alone — in the same merger, so every seq goes
    /// out exactly once and none is lost. Before edge#182 leg 2 had one
    /// attempt per connect cycle: it never came back, and the later loss of
    /// leg 1 took the input off air with a false "Both redundant legs lost".
    #[tokio::test]
    async fn a_2022_7_input_reaccepts_a_lost_leg_2_and_survives_leg_1_loss() {
        let mut h = InputPairHarness::listeners();
        h.connect(1).await;
        h.connect(2).await;
        h.wait_until("both legs' stats", |h| h.leg_stats(1) && h.leg_stats(2)).await;
        h.settle().await;

        h.drop_leg(2).await;
        h.wait_until("leg 2's loss", |h| !h.leg_lost(2).is_empty()).await;
        let lost = h.leg_lost(2)[0];
        assert_eq!(lost.severity, EventSeverity::Warning);
        assert_eq!(lost.category, category::REDUNDANCY);
        assert_eq!(other_leg(lost), "connected");
        assert_eq!(lost.details.as_ref().unwrap()["mode"], "Listener");
        h.wait_until("leg 2's stats to clear", |h| !h.leg_stats(2)).await;

        h.connect(2).await;
        h.wait_until("leg 2 to be re-accepted", |h| h.leg_stats(2)).await;
        h.wait_until("leg 2's second connected event", |h| h.connected(2).len() == 2).await;

        h.drop_leg(1).await;
        h.wait_until("leg 1's loss", |h| !h.leg_lost(1).is_empty()).await;
        assert_eq!(other_leg(h.leg_lost(1)[0]), "connected", "leg 2 carries");
        let target = h.sent() + 500;
        h.wait_until("500 more seqs on leg 2", |h| h.sent() >= target).await;
        h.stop_pump().await;

        h.assert_each_seq_forwarded_once();
        h.pump_events();
        assert_eq!(h.both_lost(), 0, "a leg always carried");
        assert!(h.leg_stats(2) && !h.leg_stats(1));
    }

    /// The mirror: leg 1 is lost and re-accepted while leg 2 carries, then
    /// leg 2 is lost and leg 1 carries alone, every seq still out once.
    /// Before edge#182 a lost leg 1 was not re-accepted while leg 2 was up.
    #[tokio::test]
    async fn a_2022_7_input_reaccepts_a_lost_leg_1_while_leg_2_carries() {
        let mut h = InputPairHarness::listeners();
        h.connect(1).await;
        h.connect(2).await;
        h.wait_until("both legs' stats", |h| h.leg_stats(1) && h.leg_stats(2)).await;
        h.settle().await;

        h.drop_leg(1).await;
        h.wait_until("leg 1's loss", |h| !h.leg_lost(1).is_empty()).await;
        assert_eq!(other_leg(h.leg_lost(1)[0]), "connected");
        h.wait_until("leg 1's stats to clear", |h| !h.leg_stats(1)).await;

        h.connect(1).await;
        h.wait_until("leg 1 to be re-accepted", |h| h.leg_stats(1)).await;
        h.wait_until("leg 1's second connected event", |h| h.connected(1).len() == 2).await;
        assert_eq!(
            h.connected(1)[1].message,
            "SRT input connected (mode=Listener, redundant leg 1)"
        );

        h.drop_leg(2).await;
        h.wait_until("leg 2's loss", |h| !h.leg_lost(2).is_empty()).await;
        assert_eq!(other_leg(h.leg_lost(2)[0]), "connected", "leg 1 carries");
        let target = h.sent() + 500;
        h.wait_until("500 more seqs on leg 1", |h| h.sent() >= target).await;
        h.stop_pump().await;

        h.assert_each_seq_forwarded_once();
        h.pump_events();
        assert_eq!(h.both_lost(), 0, "a leg always carried");
    }

    /// Both legs lost together: one Critical, raised by the second loss,
    /// whose event says the other leg is down. Then leg 2 alone comes back
    /// and carries. Before edge#182 the next connect cycle waited on leg 1.
    #[tokio::test]
    async fn a_2022_7_input_raises_both_lost_once_and_resumes_on_either_leg() {
        let mut h = InputPairHarness::listeners();
        h.connect(1).await;
        h.connect(2).await;
        h.wait_until("both legs' stats", |h| h.leg_stats(1) && h.leg_stats(2)).await;

        let peers: Vec<Arc<SrtSocket>> =
            h.legs.lock().unwrap().iter_mut().filter_map(Option::take).collect();
        for peer in peers {
            let _ = peer.close().await;
        }
        h.wait_until("both legs' losses", |h| {
            !h.leg_lost(1).is_empty() && !h.leg_lost(2).is_empty()
        })
        .await;
        h.wait_until("the Critical", |h| h.both_lost() > 0).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.pump_events();
        assert_eq!(h.both_lost(), 1, "one Critical for the pair");
        let critical = h.events.iter().find(|e| e.message == "Both redundant legs lost").unwrap();
        assert_eq!(critical.severity, EventSeverity::Critical);
        let later = h
            .events
            .iter()
            .filter(|e| e.message.starts_with("Redundant leg "))
            .nth(1)
            .expect("two losses");
        assert_eq!(other_leg(later), "down", "the second loss finds no leg up");

        h.wait_until("both legs' stats to clear", |h| !h.leg_stats(1) && !h.leg_stats(2)).await;
        let before = h.forwarded();
        h.connect(2).await;
        h.wait_until("leg 2 to attach again", |h| h.connected(2).len() == 2).await;
        h.wait_until("leg 2 to carry", |h| h.forwarded() > before + 100).await;
        h.pump_events();
        assert_eq!(h.both_lost(), 1);
    }

    /// Both legs calling: leg 1's peer is absent and leg 2's is a listener
    /// that sends. Leg 2 is dialled on its own and carries. Before edge#182
    /// leg 1's connect retried forever before leg 2 was dialled at all.
    #[tokio::test]
    async fn a_2022_7_caller_input_reaches_leg_2_while_leg_1_peer_is_absent() {
        let ports = (free_port(), free_port());
        let peer2 = srt_config(serde_json::json!({
            "mode": "listener", "local_addr": format!("127.0.0.1:{}", ports.1), "latency_ms": 200
        }));
        let mut listener = crate::srt::connection::bind_srt_listener_for_input(&peer2)
            .await
            .expect("leg 2's peer binds");
        let mut h = InputPairHarness::start(
            serde_json::json!({
                "mode": "caller", "remote_addr": format!("127.0.0.1:{}", ports.0),
                "latency_ms": 200,
                "redundancy": {
                    "mode": "caller", "remote_addr": format!("127.0.0.1:{}", ports.1),
                    "latency_ms": 200
                }
            }),
            ports,
        );
        let socket = tokio::time::timeout(
            Duration::from_secs(10),
            crate::srt::connection::accept_srt_connection(&mut listener, &h.cancel),
        )
        .await
        .expect("leg 2 dials while leg 1's peer is absent")
        .expect("leg 2 is accepted");
        h.legs.lock().unwrap()[1] = Some(socket);
        h.wait_until("leg 2 to carry", |h| h.forwarded() > 100).await;
        let up = h.connected(2)[0];
        assert_eq!(up.message, "SRT input connected (mode=Caller, redundant leg 2)");
        assert_eq!(up.details.as_ref().unwrap()["remote_addr"], format!("127.0.0.1:{}", ports.1));
        assert!(h.connected(1).is_empty());
    }

    /// A stop while leg 1 is still dialling lets that connect finish and
    /// closes what it made, so the far end sees the connection go. The
    /// pending acquisition used to be dropped part-way, and a handshake
    /// completing after that left a connection that sent keepalives and no
    /// data until the process exited, held by a far end that never saw it
    /// lost.
    #[tokio::test]
    async fn a_2022_7_input_stopping_closes_a_leg_it_was_still_dialling() {
        let ports = (free_port(), free_port());
        let flow = Arc::new(FlowStatsAccumulator::new("f1".into(), "flow-1".into(), "srt".into()));
        let (tx, _rx) = broadcast::channel(16);
        let (events, _events_rx) = crate::manager::events::event_channel();
        let cancel = CancellationToken::new();
        let input = spawn_srt_input(
            srt_config(serde_json::json!({
                "mode": "caller", "remote_addr": format!("127.0.0.1:{}", ports.0),
                "latency_ms": 200,
                "redundancy": {
                    "mode": "listener", "local_addr": format!("127.0.0.1:{}", ports.1),
                    "latency_ms": 200
                }
            })),
            tx,
            flow,
            cancel.clone(),
            events,
            "f1".into(),
            "pair".into(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        );
        // Leg 1's first connect is now in flight, with no peer to answer it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();

        let peer_config = srt_config(serde_json::json!({
            "mode": "listener", "local_addr": format!("127.0.0.1:{}", ports.0), "latency_ms": 200
        }));
        let mut listener = crate::srt::connection::bind_srt_listener_for_input(&peer_config)
            .await
            .expect("the far end binds");
        let peer_cancel = CancellationToken::new();
        let peer = tokio::time::timeout(
            Duration::from_secs(5),
            crate::srt::connection::accept_srt_connection(&mut listener, &peer_cancel),
        )
        .await
        .expect("the connect sent before the stop completes")
        .expect("the far end accepts it");
        tokio::time::timeout(Duration::from_secs(10), input)
            .await
            .expect("the input stops")
            .expect("the input task does not panic");
        let closed = tokio::time::timeout(Duration::from_secs(3), peer.recv()).await;
        assert!(
            matches!(closed, Ok(Err(_))),
            "the far end sees the connection closed, not a peer that never sends"
        );
    }

    /// The latched connect-failure event of a 2022-7 leg is a Warning while
    /// the other leg is connected, as the input is on air, and a Critical
    /// otherwise; a bonded group, with no other leg, keeps the Critical.
    #[test]
    fn connect_failure_observer_warns_while_the_other_leg_carries() {
        let latched = |other_leg_up: Option<&AtomicBool>| {
            let (events, mut events_rx) = crate::manager::events::event_channel();
            let counter = Arc::new(AtomicU32::new(0));
            let mut observer = connect_failure_observer(
                &counter,
                &events,
                "f1",
                "pair",
                serde_json::json!({ "leg": 1 }),
                other_leg_up,
            );
            let e = anyhow::anyhow!("connection refused");
            for attempts in 1..=CONNECT_FAILED_THRESHOLD {
                observer(attempts, &e);
            }
            let mut raised = Vec::new();
            while let Ok(event) = events_rx.try_recv() {
                raised.push(event);
            }
            assert_eq!(raised.len(), 1, "latched once");
            raised.pop().unwrap()
        };

        let connected = AtomicBool::new(true);
        let warned = latched(Some(&connected));
        assert_eq!(warned.severity, EventSeverity::Warning);
        let details = warned.details.as_ref().unwrap();
        assert_eq!(details["other_leg"], "connected");
        assert_eq!(details["leg"], 1);
        assert_eq!(details["consecutive_failures"], CONNECT_FAILED_THRESHOLD);

        let down = AtomicBool::new(false);
        let critical = latched(Some(&down));
        assert_eq!(critical.severity, EventSeverity::Critical);
        assert_eq!(critical.details.as_ref().unwrap()["other_leg"], "down");

        let bonded = latched(None);
        assert_eq!(bonded.severity, EventSeverity::Critical);
        assert!(bonded.details.as_ref().unwrap().get("other_leg").is_none());
    }

    /// A pair's merger waits out its legs' latency difference: legs at
    /// 120 ms and 1 500 ms (a receive override over a 4 s `latency_ms`), and
    /// a source restarting on a sequence number behind the old while the
    /// later leg still plays the old stream, 1.38 s behind. Above that lag
    /// the stale time lets the later leg reach the restart before the
    /// merger follows it, so nothing of either stream goes out twice. At the
    /// 1 s default the merger followed the earlier leg's restart first, and
    /// the later leg's last of the old stream, ahead of it, moved it forward
    /// again and went out a second time.
    #[test]
    fn a_pairs_merger_waits_out_its_latency_difference() {
        let config: SrtInputConfig = serde_json::from_value(serde_json::json!({
            "mode": "listener", "local_addr": "127.0.0.1:9000", "latency_ms": 120,
            "redundancy": {
                "mode": "listener", "local_addr": "127.0.0.1:9002",
                "latency_ms": 4_000, "recv_latency_ms": 1_500
            }
        }))
        .expect("SRT config fixture");
        let stale = pair_stale_after_us(&config, config.redundancy.as_ref().unwrap());
        assert_eq!(stale, 2_380_000);
        let mut merger = HitlessMerger::new().with_stale_after_us(stale);
        // 4 750 packets a second; the restart at packet 20 000, onto seqs
        // 25 000 behind.
        let (period, lag) = (210, 6_555u64);
        let mut arrivals = Vec::new();
        for k in 0..60_000u64 {
            arrivals.push((k * period, k, ActiveLeg::Leg1));
            arrivals.push(((k + lag) * period, k, ActiveLeg::Leg2));
        }
        arrivals.sort_by_key(|&(t, k, _)| (t, k));
        let mut sent = Vec::new();
        for (t, k, leg) in arrivals {
            let seq = (if k < 20_000 { 30_000 + k } else { 5_000 + k }) as u16;
            if merger.try_merge(seq, leg, t).is_some() {
                sent.push(k);
            }
        }
        assert_eq!(sent.last(), Some(&59_999), "the restarted stream is followed");
        let mut once = sent.clone();
        once.sort_unstable();
        once.dedup();
        assert_eq!(once.len(), sent.len(), "nothing is forwarded twice");
    }

    /// A 2022-7 pair's receive path without sockets: each leg's packets
    /// handed to it as its recv arm would, published into a broadcast the
    /// test reads, and the drain arm run when the merger's next release is
    /// due.
    struct PairBench {
        flow: Arc<FlowStatsAccumulator>,
        srt: Arc<SrtInputStats>,
        publisher: crate::engine::ingress_publisher::IngressPublisher,
        rx: broadcast::Receiver<RtpPacket>,
        pair: RedundantPair,
        transcoder: Option<InputTranscoder>,
        post: Option<InputPostProcess>,
        /// The buffered merger's stats on the flow, as the loop holds them.
        _published: Option<crate::stats::collector::BufferedHitlessPublication>,
    }

    impl PairBench {
        /// Legs 1 and 2 listening, leg 2's block extended by `extra`.
        fn new(extra: serde_json::Value) -> Self {
            let mut redundancy =
                serde_json::json!({ "mode": "listener", "local_addr": "127.0.0.1:9002" });
            for (key, value) in extra.as_object().expect("an object") {
                redundancy[key] = value.clone();
            }
            let config = srt_config(serde_json::json!({
                "mode": "listener", "local_addr": "127.0.0.1:9000", "redundancy": redundancy
            }));
            let flow =
                Arc::new(FlowStatsAccumulator::new("f1".into(), "flow-1".into(), "srt".into()));
            flow.set_active_input_id("pair");
            let srt = flow.register_srt_input_stats("pair");
            let (tx, rx) = broadcast::channel(4096);
            let publisher = crate::engine::ingress_publisher::IngressPublisher::new(
                Default::default(),
                tx,
                "pair",
                CancellationToken::new(),
                flow.clone(),
            );
            let merger = PairMerger::new(&config, config.redundancy.as_ref().unwrap());
            let _published = merger.publish_stats(&flow, "pair");
            Self {
                flow,
                srt,
                publisher,
                rx,
                pair: RedundantPair::new(merger),
                transcoder: None,
                post: None,
                _published,
            }
        }

        /// Leg `leg`'s recv delivered RTP seq `seq`.
        fn deliver(&mut self, leg: ActiveLeg, seq: u16) {
            let data = Bytes::from(rtp_datagram(seq));
            let pair = &mut self.pair;
            if pair.format == SrtPayloadFormat::Unknown
                && let Some(detected) = detect_format(&data)
            {
                pair.format = detected;
                self.srt.pair.set_merged(pair.format == SrtPayloadFormat::RtpTs);
            }
            let raw_ts_seq =
                if leg == ActiveLeg::Leg1 { &mut pair.raw_ts_seq_leg1 } else { &mut pair.raw_ts_seq_leg2 };
            process_redundant_packet(data, None, leg, pair.format, RedundantPacketState {
                raw_ts_seq,
                raw_ts_timestamp: &mut pair.raw_ts_timestamp,
                merger: &mut pair.merger,
                merger_handoff: &mut pair.merger_handoff,
                failover: &mut pair.failover,
                prev_active_leg: &mut pair.prev_active_leg,
                stats: &self.flow,
                srt: &self.srt,
                publisher: &self.publisher,
                transcoder: &mut self.transcoder,
                post: &mut self.post,
            });
        }

        /// Run the drain arm at each release the merger holds, until it holds
        /// nothing.
        fn drain_when_due(&mut self) {
            while let Some(due) = self.pair.merger.next_deadline() {
                std::thread::sleep(due.saturating_duration_since(std::time::Instant::now()));
                drain_redundant_pair(&mut self.pair, &self.flow, &self.publisher, &mut self.transcoder, &mut self.post);
            }
        }

        /// The seqs published since the last call, in order.
        fn published(&mut self) -> Vec<u16> {
            let mut seqs = Vec::new();
            while let Ok(packet) = self.rx.try_recv() {
                seqs.push(packet.sequence_number);
            }
            seqs
        }

        /// What the merger has counted of the seqs it never forwarded.
        fn lost(&self) -> u64 {
            use crate::redundancy::merger::LossCounts;
            match &self.pair.merger {
                PairMerger::Bare(merger) => LossCounts::lost(merger),
                PairMerger::Buffered(merger) => LossCounts::lost(&**merger),
            }
        }
    }

    /// The bench's re-attach case, in the order the legs' recv arms hand it
    /// over, a seq a tick: leg 2 carries, five ticks behind, and leg 1 comes
    /// back ahead of it at seq 100 and skips 150..=160 (too late on its
    /// fresh connection), so leg 2's copies of what leg 1 is ahead of arrive
    /// after leg 1's newer packets.
    fn back_ahead_with_a_hole() -> Vec<(ActiveLeg, u16)> {
        let mut arrivals = Vec::new();
        for tick in 0..305u16 {
            if tick >= 5 {
                arrivals.push((ActiveLeg::Leg2, tick - 5));
            }
            if (100..300).contains(&tick) && !(150..=160).contains(&tick) {
                arrivals.push((ActiveLeg::Leg1, tick));
            }
        }
        arrivals
    }

    /// With `path_differential_ms` the pair releases a leg back ahead of the
    /// carrying leg, with a hole, as one stream: nothing before its hold
    /// expires, then every seq once and in order, nothing counted lost, and
    /// the buffered merger's stats on the flow for the manager's card.
    #[test]
    fn a_buffered_pair_releases_a_leg_back_ahead_with_a_hole_in_order() {
        let mut bench = PairBench::new(serde_json::json!({ "path_differential_ms": 1000 }));
        for (leg, seq) in back_ahead_with_a_hole() {
            bench.deliver(leg, seq);
        }
        assert!(bench.published().is_empty(), "held for path_differential_ms");
        bench.drain_when_due();
        assert_eq!(bench.published(), (0..300).collect::<Vec<_>>());
        assert_eq!(bench.lost(), 0);
        assert_eq!(bench.flow.input_packets.load(Ordering::Relaxed), 300);
        let snap = bench.flow.snapshot();
        let held = snap.input.buffered_hitless.expect("the buffered merger's stats");
        assert_eq!((held.max_path_diff_ms, held.packets_emitted, held.gap_lost), (1000, 300, 0));
    }

    /// Without `path_differential_ms` the pair forwards as it always did:
    /// each seq at once and once, leg 2's fills after leg 1's newer packets,
    /// and no buffered stats.
    #[test]
    fn an_unbuffered_pair_forwards_at_once_as_before() {
        let mut bench = PairBench::new(serde_json::json!({}));
        let mut forwarded = Vec::new();
        for (leg, seq) in back_ahead_with_a_hole() {
            bench.deliver(leg, seq);
            forwarded.extend(bench.published());
        }
        assert!(bench.pair.merger.next_deadline().is_none(), "nothing held");
        let mut once = forwarded.clone();
        once.sort_unstable();
        assert_eq!(once, (0..300).collect::<Vec<_>>(), "each seq once");
        let at = |seq: u16| forwarded.iter().position(|&s| s == seq).unwrap();
        assert!(at(100) < at(96) && at(161) < at(157), "interleaved: {forwarded:?}");
        assert!(bench.flow.snapshot().input.buffered_hitless.is_none());
    }

    /// A buffered pair's stats are its own input's: a passive pair's merger
    /// shows nothing under the flow's active input (a UDP one, say), and
    /// what the pair counted shows once it is active, with no drain having
    /// run. They go when the pair's loop does.
    #[test]
    fn a_passive_buffered_pairs_stats_stay_its_own() {
        let mut bench = PairBench::new(serde_json::json!({ "path_differential_ms": 200 }));
        bench.flow.set_active_input_id("udp");
        for seq in 0..10u16 {
            bench.deliver(ActiveLeg::Leg1, seq);
        }
        assert!(bench.flow.snapshot().input.buffered_hitless.is_none(), "not under the UDP input");
        bench.flow.set_active_input_id("pair");
        let held = bench.flow.snapshot().input.buffered_hitless.expect("its own, once active");
        assert_eq!((held.leg1_packets_received, held.current_buffer_depth), (10, 10));
        bench._published = None;
        assert!(bench.flow.snapshot().input.buffered_hitless.is_none(), "gone with the loop");
    }

    /// Both legs lost with packets held: the Critical, a fresh format
    /// detection, and what was held released at its own time, ahead of the
    /// stream the legs bring back on a new ISN far behind the old — a packet
    /// behind that stream's first included — with no gap counted between.
    #[test]
    fn a_buffered_pair_restarting_releases_what_it_held_first() {
        let mut bench = PairBench::new(serde_json::json!({ "path_differential_ms": 300 }));
        for seq in 40_000..40_100u16 {
            bench.deliver(ActiveLeg::Leg1, seq);
            bench.deliver(ActiveLeg::Leg2, seq);
        }
        let (events, mut events_rx) = crate::manager::events::event_channel();
        both_input_legs_lost(&events, "f1", &mut bench.pair);
        let critical = events_rx.try_recv().expect("the Critical");
        assert_eq!(critical.message, "Both redundant legs lost");
        assert_eq!(critical.severity, EventSeverity::Critical);
        assert_eq!(bench.pair.format, SrtPayloadFormat::Unknown, "detected afresh");
        assert!(bench.published().is_empty(), "what is held waits for its release time");

        for seq in 7..60u16 {
            bench.deliver(ActiveLeg::Leg2, seq);
        }
        for seq in 5..60u16 {
            bench.deliver(ActiveLeg::Leg1, seq);
        }
        bench.drain_when_due();
        let expected: Vec<u16> = (40_000..40_100).chain(5..60).collect();
        assert_eq!(bench.published(), expected);
        assert_eq!(bench.lost(), 0);
        let held = bench.flow.snapshot().input.buffered_hitless.expect("the buffered stats");
        assert_eq!((held.packets_emitted, held.gap_lost), (155, 0));
    }

    /// A raw-TS payload on a pair with `path_differential_ms`: the failover
    /// Warning keeps its message and says the hold does not apply.
    #[test]
    fn raw_ts_failover_warning_says_the_hold_does_not_apply() {
        let (events, mut events_rx) = crate::manager::events::event_channel();
        let raw_ts = vec![0x47u8; 188];
        let mut format = SrtPayloadFormat::Unknown;
        assert!(settle_pair_format(&mut format, &raw_ts, &events, "f1", Some(200)));
        let mut format = SrtPayloadFormat::Unknown;
        assert!(settle_pair_format(&mut format, &raw_ts, &events, "f1", None));
        assert_eq!(format, SrtPayloadFormat::RawTs);
        let held = events_rx.try_recv().expect("the Warning");
        let plain = events_rx.try_recv().expect("the Warning");
        assert_eq!(held.message, plain.message);
        let details = held.details.as_ref().unwrap();
        assert_eq!(details["error_code"], "redundancy_failover_mode");
        assert_eq!(details["path_differential_ms"], 200);
        assert!(details["fix_hint"].as_str().unwrap().contains("path_differential_ms"));
        assert!(plain.details.as_ref().unwrap().get("path_differential_ms").is_none());
    }

    #[test]
    fn connect_failure_threshold_fires_once_per_cycle() {
        let counter = AtomicU32::new(0);
        let fired = (1..=20)
            .filter(|&attempts| record_connect_failure(attempts, &counter))
            .count();
        assert_eq!(fired, 1, "Critical must fire exactly once per cycle");
        assert_eq!(counter.load(Ordering::Relaxed), 20);

        // Successful connect resets the counter (the caller loop stores 0)…
        counter.store(0, Ordering::Relaxed);
        // …so the next never-connecting cycle fires exactly once again.
        let fired = (1..=7)
            .filter(|&attempts| record_connect_failure(attempts, &counter))
            .count();
        assert_eq!(fired, 1, "latch must re-arm after a successful connect");
    }

    #[test]
    fn connect_failure_below_threshold_never_fires() {
        let counter = AtomicU32::new(0);
        for attempts in 1..CONNECT_FAILED_THRESHOLD {
            assert!(!record_connect_failure(attempts, &counter));
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            CONNECT_FAILED_THRESHOLD - 1,
            "counter must track every failed attempt for the stats snapshot"
        );
    }

    #[test]
    fn failover_first_packet_sets_active() {
        let mut f = RawTsFailover::new();
        assert!(f.admit(ActiveLeg::Leg1));
        assert_eq!(f.active, ActiveLeg::Leg1);
    }

    #[test]
    fn failover_active_leg_admitted_standby_dropped() {
        let mut f = RawTsFailover::new();
        f.admit(ActiveLeg::Leg1);
        // Active leg keeps flowing.
        assert!(f.admit(ActiveLeg::Leg1));
        // Standby leg dropped while active is healthy.
        assert!(!f.admit(ActiveLeg::Leg2));
        // Active stays leg1.
        assert_eq!(f.active, ActiveLeg::Leg1);
    }

    #[test]
    fn failover_promotes_standby_after_stall() {
        let mut f = RawTsFailover::new();
        f.admit(ActiveLeg::Leg1);
        // Backdate leg1's last arrival past the stall threshold.
        f.last_leg1 = Some(
            std::time::Instant::now() - (RAW_TS_FAILOVER_STALL + Duration::from_millis(10)),
        );
        assert!(f.admit(ActiveLeg::Leg2));
        assert_eq!(f.active, ActiveLeg::Leg2);
    }

    #[test]
    fn failover_does_not_dedup_within_active_leg() {
        // Even a "duplicate" stream from the active leg passes — the
        // primitive is failover, not dedup. If the publisher itself
        // double-sends, that's not the failover's problem.
        let mut f = RawTsFailover::new();
        f.admit(ActiveLeg::Leg1);
        for _ in 0..1000 {
            assert!(f.admit(ActiveLeg::Leg1));
        }
    }
}
