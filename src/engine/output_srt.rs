// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use srt_protocol::error::SrtError;
use srt_transport::SrtSocket;
use srt_transport::SocketStatus;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::models::{SrtMode, SrtOutputConfig};
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::srt::bond_supervisor::{
    BondEventScope, BondEvents, BondHealthTracker, BondLatch, BondSupervisor, degrade_after,
    final_member_report, plan_bond_members,
};
use crate::srt::connection::{
    accept_srt_connection, bind_srt_listener_for_bonded_output, bind_srt_listener_for_output,
    bind_srt_listener_for_redundancy, connect_srt_group, connect_srt_output,
    connect_srt_output_observed, connect_srt_redundancy_leg, spawn_srt_group_stats_poller,
    spawn_srt_socket_group_stats_poller, spawn_srt_stats_poller, SrtConnectionParams,
};
use crate::stats::collector::OutputStatsAccumulator;
use crate::util::time::now_us;

use super::audio_302m::{S302mOutputPipeline, SRT_TS_DATAGRAM_BYTES};
use super::audio_transcode::InputFormat;
use super::delay_buffer::resolve_output_delay;
use super::packet::RtpPacket;
use super::ts_pid_overrides_rewriter::TsPidOverridesRewriter;
use super::ts_pid_remapper::TsPidRemapper;
use super::ts_program_filter::TsProgramFilter;

/// Maximum SRT live-mode payload bytes per `srt_sendmsg` call.
///
/// SRT live mode delivers each `srt_sendmsg` as one logical message that the
/// receiver reassembles before returning from `srt_recvmsg`. The libsrt
/// receiver's default read buffer is 1456 bytes (one MSS minus headers); any
/// reassembled message larger than that is silently truncated. To stay
/// interoperable with `srt-live-transmit`, ffmpeg's libsrt input, hardware
/// decoders, and any other libsrt-based consumer, every outbound buffer is
/// chunked to ≤1316 bytes (= 7 × 188-byte MPEG-TS packets — the libsrt
/// recommended live-mode default).
///
/// UDP/RTP-sourced broadcast packets are typically already 1316 bytes and
/// pass through unchanged. RTMP/RTSP-sourced packets can be much larger (a
/// TsMuxer-bundled I-frame can be tens of KB) and need to be split before
/// they hit the SRT socket. Nothing here joins *small* items: a source that
/// publishes fewer than seven TS packets per item leaves as that many small
/// SRT messages, so every source that muxes its own TS bundles before it
/// publishes (`packet::bundle_ts_packets`; SDI published one packet per item
/// until edge#166, 5,800 SRT datagrams a second at 8.5 Mbit/s).
const SRT_LIVE_MAX_MSG_BYTES: usize = SRT_TS_DATAGRAM_BYTES;

/// Maximum time we wait for the SMPTE 2022-7 leg-2 socket to complete its
/// initial handshake before giving up and running the output in single-leg
/// mode. Prevents a misconfigured or unreachable leg-2 peer from gating
/// media flow on the healthy leg-1.
const INITIAL_LEG2_CONNECT_BUDGET: Duration = Duration::from_secs(15);

/// Split `data` into ≤[`SRT_LIVE_MAX_MSG_BYTES`] chunks for SRT live-mode
/// transmission. Pure synchronous helper used by [`srt_send_chunked`] and
/// the unit tests so chunking behaviour can be verified without a real SRT
/// socket.
fn srt_chunk_for_live(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.chunks(SRT_LIVE_MAX_MSG_BYTES)
}

/// Send `data` over an SRT live-mode socket, splitting into ≤[`SRT_LIVE_MAX_MSG_BYTES`]
/// chunks if needed. Returns the total number of bytes accepted by the socket
/// or the first send error encountered.
///
/// Returns the raw [`SrtError`] so callers can distinguish transient
/// backpressure ([`SrtError::AsyncSend`], now rare given the libsrt wrapper's
/// awaiting send) from fatal connection errors that warrant a reconnect.
async fn srt_send_chunked(socket: &Arc<SrtSocket>, data: &[u8]) -> Result<usize, SrtError> {
    if data.is_empty() {
        return Ok(0);
    }
    let mut total = 0usize;
    for chunk in srt_chunk_for_live(data) {
        let n = socket.send(chunk).await?;
        total += n;
    }
    Ok(total)
}

/// Capacity of the bounded channel between an output's broadcast-subscriber
/// loop and its `srt-io` send task.
///
/// **This is a burst buffer, not a latency budget.** Compressed video is not
/// delivered at a constant rate: an H.264 IDR access unit routinely runs 3×
/// the median AU size (measured 123–144 KB against a ~39 KB baseline on live
/// 1080i50 SDI at 8 Mbps CBR). Sizing the queue against the *mean* rate
/// therefore guarantees periodic overflow on completely ordinary content.
///
/// **The bound is in items, not bytes, and item size is set by the publisher**
/// — so quoting one duration or one byte figure for this queue is wrong. The
/// item is a whole upstream `RtpPacket`, and the three publishing shapes in
/// the tree differ by ~800×:
///
/// - **UDP / RTP / SRT passthrough** publish ~1316 B (7 × 188), and so, since
///   edge#166, do the sources that mux their own TS — **SDI**
///   (`sdi_io::publish_ts`), **WebRTC**, **MXL** and **ST 2110-20/-23** video —
///   seven packets to an item (`packet::bundle_ts_packets`; a call with a
///   single packet, a lone audio frame or PCR, is one 188 B item). ~5.4 MB.
///   Until edge#166 SDI and WebRTC published **one 188-byte TS packet per
///   item**, when 4096 slots was ~770 KB / ~0.77 s at 8 Mbps.
/// - **RTMP** (`input_rtmp.rs`) and **RTSP** bundle a whole access unit into
///   one item, "tens of KB" as the chunking note above warns. 4096 slots is
///   then ~164 MB / ~164 s at the same bitrate. Those sources never overflowed
///   at 256 either, so they gain nothing here and pay the depth — bounding
///   this queue in bytes rather than slots is tracked in #98.
///
/// That granularity is what made the measured failure inevitable rather than
/// merely unlucky: on the SDI path as it then published, a single 123–144 KB
/// IDR was **654–766 items** (94–110 since edge#166), so it overran the old 256-slot queue roughly 3× *on its own*,
/// once per GOP. Hardware-measured on bilby-z440 feeding four SRT outputs:
/// 2149 discards in 60 s with **zero** broadcast-`Lagged` warnings, i.e. every
/// one came from `try_send` returning `Full` here. Halving the source's IDR
/// count (`gop_size` 50 → 100) halved the discards and restoring it restored
/// them, confirming the IDR burst as the driver rather than a coincidence.
///
/// 4096 left ~5 IDRs of headroom on that path at 188 B an item (~37 since
/// edge#166) while staying bounded. It
/// mirrors the fix already applied one layer down in the libsrt wrapper, where
/// the caller → `srt-io` channel was raised 256 → 8192 for this same class of
/// stall (`srt-transport::socket::SEND_CHANNEL_CAPACITY`, commit `7cce767`);
/// that layer awaits, so it backpressures, whereas this one drops — which is
/// why it needed the headroom more, not less.
const SRT_SEND_QUEUE_CAPACITY: usize = 4096;

/// Byte ceiling for the same queue, applied alongside the slot count.
///
/// The slot cap alone is not a memory bound, because **an item is a whole
/// upstream `RtpPacket` and its size is set by the publisher**, varying by
/// ~800× across source classes (#98):
///
/// | publisher | bytes per item |
/// |---|---|
/// | UDP / RTP / SRT passthrough; SDI, WebRTC, MXL / ST 2110-20 video (since edge#166) | ~1316 B (7 × 188); 188 B for a single-packet mux call |
/// | RTMP, RTSP | a **whole access unit** — measured IDRs 123–144 KB |
///
/// So 4096 slots is ~5.4 MB of TS-class traffic but up to ~164 MB of
/// RTMP/RTSP access units. Those sources never overflowed at the old 256
/// slots either, so they gain nothing from the depth and pay for all of it.
///
/// 8 MiB is chosen so the slot cap still binds first for the two item shapes
/// #95 actually measured (the second was SDI's before edge#166): 4096 ×
/// 1316 B ≈ 5.4 MB and 4096 × 188 B ≈ 0.8 MB
/// are both under it, so the TS-class behaviour measured there (0 drops
/// across 10.6 M packets) is preserved exactly. For whole-AU items the byte
/// cap binds first — at the 123–144 KB IDRs actually measured that is 56–66
/// access units (pinned by `the_byte_ceiling_is_reached_at_the_measured_idr_sizes`),
/// still an order of magnitude more burst absorption than the ~2 an IDR
/// needs, at 5 % of the memory.
///
/// **The crossover is 2048 B/item** (`8 MiB / 4096`), not "every class the
/// depth was raised for" — above that the byte cap binds first. That is
/// reachable from config, not just from RTMP/RTSP: `ts_packets_per_datagram`
/// on test-pattern and media-player inputs validates up to 348 (65 424 B),
/// where the effective depth is 128 items rather than 4096. Still ~8 MiB of
/// burst either way, which is the quantity that matters — but the *slot*
/// figure no longer describes those flows, so read the byte bound, not the
/// slot count, when reasoning about them.
///
/// Deliberately a fixed budget rather than one derived from
/// `BandwidthProfile`: the profile describes the *flow's* bitrate class,
/// while this queue's exposure is set by the publisher's item shape, and
/// the two are independent — an `Uncompressed` flow publishing 188 B items
/// needs less headroom here than a `Standard` flow publishing whole AUs.
const SRT_SEND_QUEUE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Release `n` reserved bytes without ever underflowing.
///
/// A plain `fetch_sub` would wrap on any double-release race (a producer
/// unwinding its reservation while [`SendQueueRx::drop`] drains the same
/// item), and a wrapped counter reads as permanently over the ceiling —
/// which is exactly the wedge this queue must not create.
fn release_queued_bytes(counter: &AtomicUsize, n: usize) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_sub(n))
    });
}

/// Sender half of the SRT egress queue, bounded in **both** slots and bytes.
///
/// Wraps `mpsc::Sender` rather than replacing it so the item shape and the
/// redundant loop's both-legs-`Full` reconnect semantics are untouched —
/// `try_send` keeps returning `TrySendError` with the item handed back, and
/// every existing call site reads the same.
///
/// Deliberately **not** `Clone`: every queue in this file has exactly one
/// producer (the forward loop, or one redundancy leg). The single-producer
/// property is what makes the empty-queue admission rule in [`Self::try_send`]
/// a bound of `max(ceiling, one item)` rather than `slots × one item`.
struct SendQueueTx {
    tx: tokio::sync::mpsc::Sender<(Bytes, u64)>,
    queued_bytes: Arc<AtomicUsize>,
    /// Rejections attributable to the **byte** ceiling. Everything else in
    /// `packets_dropped` for this queue is the slot cap, and the two want
    /// opposite remedies — see the throttled warn in the forward loop.
    byte_bound_rejections: Arc<AtomicU64>,
}

impl SendQueueTx {
    /// Non-blocking enqueue. `Full` when either bound would be exceeded.
    ///
    /// Reserves the bytes before offering the item and releases them on any
    /// rejection, so the counter can transiently over-read but never
    /// under-read — an under-read would let the queue past its ceiling,
    /// which is the failure this bound exists to prevent.
    ///
    /// **An empty queue always admits one item, whatever its size.** Without
    /// that a publisher whose item exceeds the whole ceiling could never send
    /// anything at all: the reservation fails against an empty queue, so it is
    /// not transient fullness but permanent, total egress loss. That is not
    /// hypothetical — the media-player still-image path has produced ~9 MB
    /// frames (see `docs/sdi.md`, the 90 kHz PTS contract). Worse, in 2022-7
    /// mode both legs reject the same oversized item in lockstep, and the
    /// redundant loop reads "both legs `Full`" as *all legs lost*: it would
    /// close both healthy SRT sockets and reconnect, forever. The real bound
    /// is therefore `max(SRT_SEND_QUEUE_MAX_BYTES, largest single item)`.
    fn try_send(
        &self,
        item: (Bytes, u64),
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<(Bytes, u64)>> {
        let n = item.0.len();
        let prev = self.queued_bytes.fetch_add(n, Ordering::Relaxed);
        if prev > 0 && prev.saturating_add(n) > SRT_SEND_QUEUE_MAX_BYTES {
            release_queued_bytes(&self.queued_bytes, n);
            // Never let the byte ceiling mask a dead consumer. This check
            // runs *ahead* of `tx.try_send`, so on a closed channel it would
            // otherwise report `Full` where tokio would have said `Closed` —
            // and `Closed` is the only thing that triggers a reconnect on a
            // bonded output, whose `peer_is_gone()` is hardwired `false` for
            // an `SrtGroup`. Reporting fullness there wedges the output into
            // dropping every packet forever with no reconnect.
            return Err(if self.tx.is_closed() {
                tokio::sync::mpsc::error::TrySendError::Closed(item)
            } else {
                self.byte_bound_rejections.fetch_add(1, Ordering::Relaxed);
                tokio::sync::mpsc::error::TrySendError::Full(item)
            });
        }
        match self.tx.try_send(item) {
            Ok(()) => Ok(()),
            Err(e) => {
                release_queued_bytes(&self.queued_bytes, n);
                Err(e)
            }
        }
    }

    /// Live depth in bytes, for logging. Advisory — it can be read between a
    /// producer's reservation and its release.
    fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Relaxed)
    }

    /// How many rejections so far were the byte ceiling rather than the slots.
    fn byte_bound_rejections(&self) -> u64 {
        self.byte_bound_rejections.load(Ordering::Relaxed)
    }
}

/// Receiver half. Releases each item's bytes as it is dequeued.
struct SendQueueRx {
    rx: tokio::sync::mpsc::Receiver<(Bytes, u64)>,
    queued_bytes: Arc<AtomicUsize>,
}

impl SendQueueRx {
    async fn recv(&mut self) -> Option<(Bytes, u64)> {
        let item = self.rx.recv().await?;
        release_queued_bytes(&self.queued_bytes, item.0.len());
        Some(item)
    }
}

/// Releasing the reservation of everything still queued is **load-bearing**,
/// not tidiness: the send task `break`s out of its `while let` on a
/// non-transient send error, which drops this half with items still in the
/// channel. Their bytes would otherwise stay reserved for the life of the
/// connection, and once that residue reaches the ceiling every later
/// `try_send` short-circuits to `Full` — see [`SendQueueTx::try_send`] for
/// why a masked `Closed` is unrecoverable on a bonded output.
impl Drop for SendQueueRx {
    fn drop(&mut self) {
        // Close first so any concurrent producer sees `Closed` immediately
        // and unwinds its own reservation through the normal error path.
        self.rx.close();
        while let Ok(item) = self.rx.try_recv() {
            release_queued_bytes(&self.queued_bytes, item.0.len());
        }
    }
}

/// Build a queue bounded by [`SRT_SEND_QUEUE_CAPACITY`] slots **and**
/// [`SRT_SEND_QUEUE_MAX_BYTES`] bytes, whichever binds first.
fn send_queue() -> (SendQueueTx, SendQueueRx) {
    let (tx, rx) = tokio::sync::mpsc::channel::<(Bytes, u64)>(SRT_SEND_QUEUE_CAPACITY);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    (
        SendQueueTx {
            tx,
            queued_bytes: Arc::clone(&queued_bytes),
            byte_bound_rejections: Arc::new(AtomicU64::new(0)),
        },
        SendQueueRx { rx, queued_bytes },
    )
}

/// How often a saturated send queue may log. Discards arrive in bursts of
/// hundreds, so an unthrottled warning would itself become a load source.
const SEND_QUEUE_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// `true` when `err` represents a transient Rust-side backpressure condition
/// on the Tokio → `srt-io` send channel rather than a genuine connection
/// failure. Callers should **not** tear the connection down on these — drop
/// the current packet, bump `packets_dropped`, and keep serving.
///
/// **Dead on both backends.** On libsrt, `SrtSocket::send` awaits its bounded
/// channel and can only fail with `ConnectionLost`; `AsyncSend` is constructed
/// but consumed entirely inside the wrapper's `epoll_bridge` (a would-block is
/// pushed onto its send backlog), so it has not reached a caller since the
/// 2026-04-20 fix. The pure-Rust `bilbycast-srt` backend — swappable in via
/// `Cargo.toml` — never constructs the variant at all: its `SrtSocket::send`
/// likewise awaits and maps every failure to `NoConnection`. Retained as
/// forward-compatibility scaffolding should either backend reintroduce a
/// non-awaiting send, which is the 2026-04-20 regression described below.
///
/// Do not read a zero `packets_dropped` at these call sites as evidence the
/// send path is healthy — the real discard sites are the five `try_send`
/// calls below (three in the forward loop, two in the redundant loop).
///
/// Historically `SrtSocket::send` used `try_send` onto a 256-slot mpsc and
/// surfaced this error under bursty input (especially SRT + FEC on the send
/// side). The 2026-04-20 interop run traced the "SRT flapping" failure mode
/// to the forward loop treating this as fatal; the helper is retained as
/// belt-and-suspenders even after the libsrt wrapper was switched to an
/// awaiting send. See `testbed/test_reports/srt_full_interop_2026-04-20/`.
#[inline]
fn is_transient_backpressure(err: &SrtError) -> bool {
    matches!(err, SrtError::AsyncSend)
}

/// Send sink for an SRT output — either a single socket (plain or accepted
/// bonded-listener) or a native libsrt socket group (bonded caller).
///
/// Callers of [`srt_output_forward_loop`] hand one of these in and the
/// forward loop's send task fans chunks to whichever underlying handle
/// is present. Group send in live mode has the same live-message cap as
/// socket send, so we use the same chunking helper.
#[derive(Clone)]
enum SrtSendSink {
    Socket(Arc<SrtSocket>),
    Group(Arc<srt_transport::SrtGroup>),
}

impl SrtSendSink {
    async fn send_chunked(&self, data: &[u8]) -> Result<usize, SrtError> {
        if data.is_empty() {
            return Ok(0);
        }
        match self {
            SrtSendSink::Socket(s) => srt_send_chunked(s, data).await,
            SrtSendSink::Group(g) => {
                let mut total = 0usize;
                for chunk in srt_chunk_for_live(data) {
                    let n = g.send(chunk).await?;
                    total += n;
                }
                Ok(total)
            }
        }
    }

    /// `true` when the peer is gone and this sink will never carry data again.
    ///
    /// **Independent of the send path on purpose.** Until #100 the forward
    /// loop's only liveness signal was `try_send` returning `Closed`, which
    /// requires the `srt-io` send task to have errored first — so the whole
    /// output was staked on the contract "`send()` eventually errors when the
    /// peer is gone". It does now, but a single missed state transition in the
    /// backend was enough to park an output at `idle` indefinitely while its
    /// peer's listener sat waiting, recoverable only by recreating the output.
    /// A cheap poll of the socket's own status costs nothing per packet and
    /// does not depend on that contract holding.
    ///
    /// Groups are exempt: a member breaking is ordinary bonding failover, and
    /// the group survives it. Only libsrt's own group status could answer this
    /// and it means something different, so the bonded path keeps relying on
    /// the send error.
    fn peer_is_gone(&self) -> bool {
        match self {
            SrtSendSink::Socket(s) => matches!(
                s.status(),
                SocketStatus::Broken
                    | SocketStatus::Closing
                    | SocketStatus::Closed
                    | SocketStatus::NonExist
            ),
            SrtSendSink::Group(_) => false,
        }
    }
}

/// Resolve `interface_binding` → `local_addr` for the primary leg and the
/// 2022-7 redundancy leg, mutating `config` in place, and check every
/// bonding endpoint's binding. Bonding endpoints are checked only: their
/// NIC address is resolved again on every group connect and member re-add
/// (`srt::bond_supervisor`), so a member follows a NIC whose address
/// changes. Phase 1: loose only — strict requests are returned as Err.
fn apply_srt_output_interface_bindings(config: &mut SrtOutputConfig) -> anyhow::Result<()> {
    if let Some(addr) = crate::util::socket::srt_local_addr_from_binding(
        config.interface_binding.as_ref(),
        config.local_addr.as_deref(),
    )? {
        config.local_addr = Some(addr);
    }
    if let Some(red) = config.redundancy.as_mut()
        && let Some(addr) = crate::util::socket::srt_local_addr_from_binding(
            red.interface_binding.as_ref(),
            red.local_addr.as_deref(),
        )? {
            red.local_addr = Some(addr);
        }
    if let Some(bonding) = config.bonding.as_ref() {
        crate::srt::bond_supervisor::check_bond_endpoint_bindings(bonding)?;
    }
    Ok(())
}

/// Spawn a task that subscribes to the broadcast channel and sends
/// RTP packets over an SRT connection. If redundancy is configured,
/// packets are duplicated to both SRT legs via SrtDuplicator.
/// Per-flow context an SRT output needs beyond its own config: identity,
/// event sink, and the negotiated audio/clock state shared with the flow.
pub struct SrtOutputCtx {
    pub event_sender: EventSender,
    pub flow_id: String,
    pub input_format: Option<InputFormat>,
    pub compressed_audio_input: bool,
    pub frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    pub active_input_rx: tokio::sync::watch::Receiver<String>,
}

pub fn spawn_srt_output(
    config: SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    output_stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    params: SrtOutputCtx,
) -> JoinHandle<()> {
    // Destructured back into the original bindings so the body is unchanged.
    let SrtOutputCtx {
        event_sender,
        flow_id,
        input_format,
        compressed_audio_input,
        frame_rate_rx,
        active_input_rx,
    } = params;

    let broadcast_tx = broadcast_tx.clone();
    // Apply interface_binding (loose only on SRT in Phase 1) for the
    // primary leg, the 2022-7 redundancy leg, and every bonding endpoint.
    // Strict mode rejected at validation; spawn-time errors fire a
    // `srt_strict_binding_unsupported` event and abort start.
    let mut config = config;
    if let Err(e) = apply_srt_output_interface_bindings(&mut config) {
        tracing::error!("SRT output '{}': interface_binding failed: {e}", config.id);
        event_sender.emit_output_with_details(
            EventSeverity::Critical,
            crate::manager::events::category::SRT,
            format!("SRT output '{}': interface_binding rejected: {e}", config.id),
            &config.id,
            serde_json::json!({"error_code": "srt_strict_binding_unsupported"}),
        );
        return tokio::spawn(async {});
    }

    // Register the static portion of this output's egress media summary so
    // the manager UI can render the pipeline / format badges from the very
    // first stats snapshot. Audio-passthrough is `true` only when no
    // `audio_encode` block is present; video-passthrough mirrors `video_encode`.
    let mut egress_static = crate::stats::collector::EgressMediaSummaryStatic {
        transport_mode: Some(
            if matches!(config.transport_mode.as_deref(), Some("audio_302m")) {
                "audio_302m".to_string()
            } else {
                "ts".to_string()
            },
        ),
        video_passthrough: config.video_encode.is_none(),
        audio_passthrough: config.audio_encode.is_none()
            && !matches!(config.transport_mode.as_deref(), Some("audio_302m")),
        audio_only: matches!(config.transport_mode.as_deref(), Some("audio_302m")),
        ..Default::default()
    };
    if let Some(ve) = config.video_encode.as_ref() {
        egress_static = egress_static.with_video_encode_target(ve);
    }
    if let Some(ae) = config.audio_encode.as_ref() {
        egress_static = egress_static.with_audio_encode_target(ae);
    }
    output_stats.set_egress_static(egress_static);

    tokio::spawn(async move {
        let result = if config.bonding.is_some() {
            srt_output_bonded_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id, input_format, compressed_audio_input, frame_rate_rx, active_input_rx).await
        } else if config.redundancy.is_some() {
            srt_output_redundant_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id, frame_rate_rx, active_input_rx, INITIAL_LEG2_CONNECT_BUDGET).await
        } else {
            srt_output_loop(&config, &broadcast_tx, output_stats, cancel, &event_sender, &flow_id, input_format, compressed_audio_input, frame_rate_rx, active_input_rx).await
        };
        if let Err(e) = result {
            tracing::error!("SRT output '{}' exited with error: {e}", config.id);
        }
    })
}

// ---------------------------------------------------------------------------
// Single-leg SRT output (no redundancy)
// ---------------------------------------------------------------------------

/// Core send loop for a single-leg SRT output (no redundancy).
///
/// Dispatches to listener-specific or caller-specific loops based on the
/// SRT mode. Listener mode binds once and re-accepts on disconnect; caller
/// mode reconnects with exponential back-off.
#[allow(clippy::too_many_arguments)]
async fn srt_output_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    match config.mode {
        SrtMode::Listener => srt_output_listener_loop(config, broadcast_tx, stats, cancel, events, flow_id, input_format, compressed_audio_input, frame_rate_rx, active_input_rx).await,
        _ => srt_output_caller_loop(config, broadcast_tx, stats, cancel, events, flow_id, input_format, compressed_audio_input, frame_rate_rx, active_input_rx).await,
    }
}

/// Listener-mode output: binds the listener once and re-accepts new callers
/// on disconnect, avoiding the port-rebind problem.
#[allow(clippy::too_many_arguments)]
async fn srt_output_listener_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    _flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    let mut listener = bind_srt_listener_for_output(config).await?;
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (listener): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut pid_remapper = build_pid_remapper(config);
    let mut pid_overrides_rewriter = build_pid_overrides_rewriter(config);
    let mut transcode_chain =
        build_transcode_chain_for_srt(config, events, &stats);
    let mut null_padder = config
        .cbr_pad_to_kbps
        .map(crate::engine::ts_null_padder::TsNullPadder::new);
    spawn_replacer_switch_watcher(
        &config.id,
        transcode_chain.as_ref(),
        active_input_rx,
        cancel.clone(),
    );

    loop {
        let socket = match accept_srt_connection(&mut listener, &cancel).await {
            Ok(s) => s,
            Err(_) if cancel.is_cancelled() => {
                tracing::info!(
                    "SRT output '{}' stopping (cancelled during accept)",
                    config.id
                );
                let _ = listener.close().await;
                return Ok(());
            }
            Err(e) => {
                tracing::error!("SRT output '{}' accept failed: {e}", config.id);
                let _ = listener.close().await;
                return Err(e);
            }
        };

        tracing::info!(
            "SRT output '{}' connected: mode=listener local={}",
            config.id,
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_output_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT output '{}' connected", config.id), &config.id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

        // Validate the connection is alive before committing to it.
        // Under high delay/loss, the listener may accept a stale connection
        // where the caller's handshake timed out and retried with a new socket ID.
        // Send a few packets and check for ACK responses within 5 seconds.
        let is_alive = {
            let mut test_rx = broadcast_tx.subscribe();
            let mut sent = 0u32;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break false,
                    _ = tokio::time::sleep_until(deadline) => {
                        // Check if we received any ACKs
                        let srt_stats = socket.stats().await;
                        if srt_stats.pkt_recv_ack_total > 0 {
                            break true;
                        }
                        tracing::warn!(
                            "SRT output '{}' no ACK received after 3s, connection likely stale",
                            config.id
                        );
                        break false;
                    }
                    result = test_rx.recv() => {
                        if let Ok(packet) = result {
                            match srt_send_chunked(&socket, &packet.data).await {
                                Ok(_) => {
                                    sent += 1;
                                    // After sending some packets, check for ACKs periodically
                                    if sent.is_multiple_of(100) {
                                        let srt_stats = socket.stats().await;
                                        if srt_stats.pkt_recv_ack_total > 0 {
                                            break true;
                                        }
                                    }
                                }
                                Err(_) => {
                                    // Send channel may be full if the I/O thread hasn't
                                    // started draining yet. Yield briefly rather than
                                    // declaring the connection dead — the ACK deadline
                                    // is the real health check.
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                }
                            }
                        }
                    }
                }
            }
        };

        if !is_alive {
            tracing::info!(
                "SRT output '{}' stale connection detected, re-accepting...",
                config.id
            );
            events.emit_output(EventSeverity::Warning, category::SRT, format!("SRT output '{}' stale connection detected", config.id), &config.id);
            let _ = socket.close().await;
            continue;
        }

        // Subscribe AFTER the peer connects so no stale packets accumulate
        // during the accept() wait. The receiver is dropped and re-created
        // on each reconnect cycle.
        let mut rx = broadcast_tx.subscribe();

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.srt_stats_cache.clone(), None, poller_cancel.clone());

        let sink = SrtSendSink::Socket(socket.clone());
        let disconnected = srt_output_forward_loop(config, &mut rx, &stats, &cancel, &sink, &mut program_filter, &mut pid_remapper, &mut pid_overrides_rewriter, &mut transcode_chain, &mut null_padder, input_format, compressed_audio_input, frame_rate_rx.clone()).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            let _ = listener.close().await;
            return Ok(());
        }

        // Brief delay before re-accepting
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping during reconnect delay", config.id);
                let _ = listener.close().await;
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        events.emit_output_with_details(
            EventSeverity::Warning, category::SRT,
            format!("SRT output '{}' disconnected", config.id), &config.id,
            serde_json::json!({
                "mode": "listener",
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );
        tracing::info!("SRT output '{}' waiting for caller to reconnect...", config.id);
    }
}

/// Caller-mode output: reconnects with exponential back-off on disconnect.
#[allow(clippy::too_many_arguments)]
async fn srt_output_caller_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    _flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (caller): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut pid_remapper = build_pid_remapper(config);
    let mut pid_overrides_rewriter = build_pid_overrides_rewriter(config);
    let mut transcode_chain =
        build_transcode_chain_for_srt(config, events, &stats);
    let mut null_padder = config
        .cbr_pad_to_kbps
        .map(crate::engine::ts_null_padder::TsNullPadder::new);
    spawn_replacer_switch_watcher(
        &config.id,
        transcode_chain.as_ref(),
        active_input_rx,
        cancel.clone(),
    );
    loop {
        // Feed the retry loop's per-attempt failures into the output's
        // `connect_failures` counter and a throttled event. `connect_srt_*`
        // retries forever, so without this a caller whose peer is gone goes
        // silent after its first event and reports `idle` — the same state the
        // manager shows for a stopped flow (#100).
        let attempt_stats = Arc::clone(&stats);
        let attempt_events = events.clone();
        let attempt_output_id = config.id.clone();
        let attempt_remote = config.remote_addr.clone().unwrap_or_default();
        let mut on_attempt_failed = move |attempt: u32, err: &anyhow::Error| {
            attempt_stats
                .connect_failures
                .store(attempt, std::sync::atomic::Ordering::Relaxed);
            // One event as the state flips to `connect_failed`, then every
            // 20th attempt (~10 min at the 30 s cap) so a long outage leaves
            // a trail without flooding the events feed.
            let threshold = crate::srt::connection::CONNECT_FAILED_THRESHOLD;
            if attempt == threshold || (attempt > threshold && attempt.is_multiple_of(20)) {
                attempt_events.emit_output_with_details(
                    EventSeverity::Critical,
                    category::SRT,
                    format!(
                        "SRT output '{attempt_output_id}' cannot reach its peer —                          {attempt} consecutive connection attempts failed, still retrying"
                    ),
                    &attempt_output_id,
                    serde_json::json!({
                        "error_code": "srt_output_peer_unreachable",
                        "output_id": attempt_output_id,
                        "remote_addr": attempt_remote,
                        "attempts": attempt,
                        "error": err.to_string(),
                    }),
                );
            }
        };
        let socket = match connect_srt_output_observed(config, &cancel, &mut on_attempt_failed).await
        {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    tracing::info!(
                        "SRT output '{}' stopping (cancelled during connect)",
                        config.id
                    );
                    return Ok(());
                }
                tracing::error!("SRT output '{}' connection failed: {e}", config.id);
                events.emit_output_with_details(
                    EventSeverity::Critical, category::SRT,
                    format!("SRT output '{}' connection failed: {e}", config.id), &config.id,
                    serde_json::json!({
                        "mode": format!("{:?}", config.mode),
                        "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
                        "stream_id": config.stream_id.as_deref().unwrap_or(""),
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
        };

        stats
            .connect_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(
            "SRT output '{}' connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr.as_deref().unwrap_or("auto"),
        );
        events.emit_output_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT output '{}' connected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
                "stream_id": config.stream_id.as_deref().unwrap_or(""),
            }),
        );

        // Subscribe AFTER the peer connects so no stale packets accumulate
        let mut rx = broadcast_tx.subscribe();

        let poller_cancel = cancel.child_token();
        spawn_srt_stats_poller(socket.clone(), stats.srt_stats_cache.clone(), None, poller_cancel.clone());

        let sink = SrtSendSink::Socket(socket.clone());
        let disconnected = srt_output_forward_loop(config, &mut rx, &stats, &cancel, &sink, &mut program_filter, &mut pid_remapper, &mut pid_overrides_rewriter, &mut transcode_chain, &mut null_padder, input_format, compressed_audio_input, frame_rate_rx.clone()).await?;
        poller_cancel.cancel();
        let _ = socket.close().await;

        if !disconnected {
            break;
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping during reconnect delay", config.id);
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        events.emit_output_with_details(
            EventSeverity::Warning, category::SRT,
            format!("SRT output '{}' disconnected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "remote_addr": config.remote_addr.as_deref().unwrap_or(""),
            }),
        );
        tracing::info!("SRT output '{}' attempting reconnection...", config.id);
    }

    Ok(())
}

/// Inner packet forwarding loop shared by listener and caller output modes.
///
/// Bridges the broadcast receiver to the SRT socket via a bounded mpsc channel
/// and a dedicated Tokio send task. Returns `Ok(true)` if the connection was
/// lost (caller should reconnect), `Ok(false)` if the broadcast channel closed
/// (flow is shutting down).
///
/// `program_filter` is owned by the caller so its PAT/PMT state survives
/// reconnects.
/// Build a [`TsPidRemapper`] from `config.pid_map`. Returns `None` when
/// the map is absent or empty (no-op).
fn build_pid_remapper(config: &SrtOutputConfig) -> Option<TsPidRemapper> {
    let map = config.pid_map.as_ref()?;
    let r = TsPidRemapper::new(map);
    if r.is_active() {
        tracing::info!(
            "SRT output '{}': pid_map active ({} entries)",
            config.id,
            map.len()
        );
        Some(r)
    } else {
        None
    }
}

/// Build a [`TsPidOverridesRewriter`] from `config.pid_overrides`.
///
/// Single owner of PID remapping on both passthrough and transcoded paths —
/// the transcode replacers re-encode on the source audio / video PID and
/// let this rewriter rename PIDs afterwards, so the same stage works for
/// every program in `pid_overrides` regardless of whether
/// `audio_encode` / `video_encode` is set.
fn build_pid_overrides_rewriter(config: &SrtOutputConfig) -> Option<TsPidOverridesRewriter> {
    let map = config.pid_overrides.as_ref()?;
    let r = TsPidOverridesRewriter::new(map);
    if r.is_active() {
        tracing::info!(
            "SRT output '{}': pid_overrides rewriter active ({} programs)",
            config.id,
            map.len()
        );
        Some(r)
    } else {
        None
    }
}

/// Spawn the input-switch watcher for this SRT output's transcode
/// chain. See `engine::input_switch_watcher` for the rationale.
/// Skips spawning when there is no chain (passthrough output).
fn spawn_replacer_switch_watcher(
    output_id: &str,
    chain: Option<&crate::engine::transcode_chain::TranscodeChain>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
    cancel: CancellationToken,
) {
    let mut handles: Vec<Arc<std::sync::atomic::AtomicBool>> = Vec::new();
    if let Some(c) = chain {
        if let Some(h) = c.audio_external_reset_handle() {
            handles.push(h);
        }
        if let Some(h) = c.video_external_reset_handle() {
            handles.push(h);
        }
    }
    crate::engine::input_switch_watcher::spawn(
        output_id.to_string(),
        active_input_rx,
        handles,
        cancel,
    );
}

/// Resolve `audio_encode` + `video_encode` configs into a
/// [`crate::engine::transcode_chain::TranscodeChain`]. Returns `None`
/// when neither is configured. Logs + emits manager events on partial
/// failure (one replacer rejected) so the SRT output behaviour matches
/// the pre-Stage-1 build_audio_replacer / build_video_replacer pair.
fn build_transcode_chain_for_srt(
    config: &SrtOutputConfig,
    events: &EventSender,
    stats: &Arc<OutputStatsAccumulator>,
) -> Option<crate::engine::transcode_chain::TranscodeChain> {
    match crate::engine::transcode_chain::build_for_output(
        &config.id,
        config.audio_encode.as_ref(),
        config.video_encode.as_ref(),
        config.transcode.clone(),
        stats,
        None, // SRT has its own protocol-layer pacing, no wire_emit
        Some(events),
    ) {
        Ok(chain) => {
            if let Some(ref c) = chain {
                tracing::info!(
                    "SRT output '{}': transcode chain active ({})",
                    config.id,
                    c.target_description()
                );
                if config.audio_encode.is_some() {
                    events.emit_output_with_details(
                        EventSeverity::Info,
                        category::AUDIO_ENCODE,
                        format!("TS audio encoder started: output '{}'", config.id),
                        &config.id,
                        serde_json::json!({
                            "codec": config.audio_encode.as_ref().map(|e| &e.codec)
                        }),
                    );
                }
                if config.video_encode.is_some() {
                    events.emit_output_with_details(
                        EventSeverity::Info,
                        category::VIDEO_ENCODE,
                        format!("Video encoder started: output '{}'", config.id),
                        &config.id,
                        serde_json::json!({
                            "codec": config.video_encode.as_ref().map(|e| &e.codec)
                        }),
                    );
                }
            }
            chain
        }
        Err(e) => {
            tracing::error!(
                "SRT output '{}': transcode chain rejected: {e}; transcoding disabled",
                config.id
            );
            let (cat, label) = match e {
                crate::engine::transcode_chain::TranscodeChainError::Audio(_) => {
                    (category::AUDIO_ENCODE, "TS audio encoder")
                }
                crate::engine::transcode_chain::TranscodeChainError::Video(_) => {
                    (category::VIDEO_ENCODE, "Video encoder")
                }
            };
            events.emit_output_with_details(
                EventSeverity::Critical,
                cat,
                format!("{label} failed: output '{}': {e}", config.id),
                &config.id,
                serde_json::json!({ "error": e.to_string() }),
            );
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn srt_output_forward_loop(
    config: &SrtOutputConfig,
    rx: &mut broadcast::Receiver<RtpPacket>,
    stats: &Arc<OutputStatsAccumulator>,
    cancel: &CancellationToken,
    sink: &SrtSendSink,
    program_filter: &mut Option<TsProgramFilter>,
    pid_remapper: &mut Option<TsPidRemapper>,
    pid_overrides_rewriter: &mut Option<TsPidOverridesRewriter>,
    transcode_chain: &mut Option<crate::engine::transcode_chain::TranscodeChain>,
    null_padder: &mut Option<crate::engine::ts_null_padder::TsNullPadder>,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
) -> anyhow::Result<bool> {
    let (send_tx, mut send_rx) = send_queue();
    let send_sink = sink.clone();
    let send_stats = stats.clone();
    let output_id = config.id.clone();
    let send_handle = tokio::spawn(async move {
        while let Some((data, recv_time_us)) = send_rx.recv().await {
            // Chunk every outbound buffer into ≤SRT_LIVE_MAX_MSG_BYTES messages
            // before handing them to the SRT sink. Protects every upstream
            // code path (passthrough TS, program-filtered TS, 302M datagram,
            // and the RTMP/RTSP/WebRTC TS-mux paths that bundle whole frames)
            // from emitting oversized SRT live-mode messages that would be
            // silently truncated by libsrt receivers. The sink transparently
            // routes to either a single socket or a bonded SrtGroup.
            match send_sink.send_chunked(&data).await {
                Ok(sent) => {
                    // Count one logical packet per upstream send call (the
                    // chunking is invisible to upstream stats consumers).
                    send_stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    send_stats.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                    send_stats.record_latency(recv_time_us);
                    // A/V interleave: feed every TS packet.
                    let mut off = 0;
                    while off + 188 <= data.len() {
                        send_stats.observe_av_interleave_packet(&data[off..off + 188]);
                        off += 188;
                    }
                }
                Err(e) if is_transient_backpressure(&e) => {
                    // Rust-side send-channel was briefly full. Drop this
                    // packet and continue — the connection is still healthy.
                    send_stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!("SRT output '{}' send error: {e}", output_id);
                    break;
                }
            }
        }
    });

    // Build the optional SMPTE 302M output pipeline. Active when:
    //   - `transport_mode == "audio_302m"`
    //   - upstream input is an audio essence (`input_format` is Some) OR
    //     the input is a TS-bearing source carrying AAC
    //     (`compressed_audio_input == true`).
    //
    // When active, the program filter is bypassed (the brief explicitly
    // rejects `program_number` for the 302M audio mode) and incoming RTP
    // audio packets are routed through Transcode → 302M → TsMuxer →
    // 1316-byte SRT datagrams. For the compressed-audio-input case, an
    // in-line TsDemuxer + AacDecoder runs ahead of the pipeline, which
    // is itself built lazily on the first decoded AAC frame (once the
    // sample rate / channel count is known).
    let is_302m_mode = matches!(config.transport_mode.as_deref(), Some("audio_302m"));
    let mut audio_302m: Option<S302mOutputPipeline> = None;
    if is_302m_mode && !compressed_audio_input {
        match input_format {
            Some(input) => {
                // Default to L24 / preserve channel count (rounded up to even)
                // / 4 ms packet time. Hardcoded for chunk 6; chunks 9 will let
                // the operator override via UI.
                let out_channels = match input.channels {
                    1 => 2,
                    n if n % 2 == 0 && n <= 8 => n,
                    n if n > 8 => 8,
                    _ => input.channels + 1,
                };
                match S302mOutputPipeline::new(input, out_channels, 24, 4_000, config.pid_overrides.as_ref()) {
                    Ok(p) => {
                        tracing::info!(
                            "SRT output '{}' transport_mode=audio_302m: \
                             {}Hz/{}bit/{}ch -> 48000/24/{} 302M-in-MPEG-TS",
                            config.id,
                            input.sample_rate,
                            input.bit_depth.as_u8(),
                            input.channels,
                            out_channels,
                        );
                        audio_302m = Some(p);
                    }
                    Err(e) => {
                        tracing::error!(
                            "SRT output '{}' could not build 302M pipeline: {e}; \
                             falling back to passthrough TS",
                            config.id
                        );
                    }
                }
            }
            None => {
                tracing::warn!(
                    "SRT output '{}' transport_mode=audio_302m but upstream input \
                     is not audio; falling back to passthrough TS",
                    config.id
                );
            }
        }
    }

    // Compressed-audio bridge state. Only initialised when the 302M mode
    // is selected AND the upstream input is a TS-bearing source. All three
    // pieces are built lazily: the demuxer is created eagerly so it can
    // start parsing the PAT/PMT/ADTS headers; the decoder and pipeline
    // wait for the first AAC frame so they can pick up the sample rate
    // and channel count from `cached_aac_config()`.
    let mut ts_demuxer_302m: Option<crate::engine::ts_demux::TsDemuxer> =
        if is_302m_mode && compressed_audio_input {
            Some(crate::engine::ts_demux::TsDemuxer::new(None))
        } else {
            None
        };
    let mut aac_decoder_302m: Option<crate::engine::audio_decode::AacDecoder> = None;
    let mut logged_302m_init_failure = false;
    let mut logged_302m_decode_error: Option<String> = None;
    // Lock-free decode counters shared with the per-output stats
    // accumulator. Registered lazily the first time the decoder is built.
    let decode_stats_302m =
        std::sync::Arc::new(crate::engine::audio_decode::DecodeStats::new());
    let mut decode_stats_302m_registered = false;

    let mut filter_scratch: Vec<u8> = Vec::new();
    let mut remap_scratch: Vec<u8> = Vec::new();
    let mut overrides_scratch: Vec<u8> = Vec::new();

    // Optional output delay buffer for stream synchronization.
    // Only active for the normal TS path (not 302M, which is blocked
    // by validation). The delay buffer queues filtered packets and
    // releases them after the configured delay.
    let resolved = if let Some(ref delay) = config.delay {
        resolve_output_delay(delay, &config.id, frame_rate_rx, cancel).await
    } else {
        None
    };
    let (mut delay_buf, _frame_rate_watch) = match resolved {
        Some((buf, watch)) => (Some(buf), watch),
        None => (None, None),
    };

    let delay_sleep = tokio::time::sleep(Duration::from_secs(86400));
    tokio::pin!(delay_sleep);

    // Saturation of the send queue below is a silent packet discard — it bumps
    // a counter and nothing else. That cost real diagnostic time: an operator
    // seeing macroblocking has no signal distinguishing "this node is dropping
    // egress packets" from a network or receiver fault, and the broadcast
    // `Lagged` path (which does log) stays quiet throughout. Warn, throttled.
    let mut send_queue_dropped: u64 = 0;
    let mut send_queue_warned_at: Option<Instant> = None;

    let connection_lost = loop {
        if let Some(ref db) = delay_buf
            && let Some(release_us) = db.next_release_time() {
                let now = now_us();
                let wait = release_us.saturating_sub(now);
                delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
            }

        // Payloads to send via the SRT mpsc channel after the select.
        // Each entry carries (data, recv_time_us) for end-to-end latency tracking.
        let mut payloads_to_send: Vec<(Bytes, u64)> = Vec::new();
        let mut got_connection_lost = false;

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' stopping (cancelled)", config.id);
                drop(send_tx);
                let _ = send_handle.await;
                return Ok(false);
            }
            result = rx.recv() => {
                match result {
                    Ok(packet) => {
                        // Compressed-audio bridge for the 302M mode takes
                        // precedence over both the PCM 302M path and the
                        // TS program filter. Drives a TsDemuxer + lazy
                        // AacDecoder + lazy S302mOutputPipeline and then
                        // drains the pipeline's ready datagrams into the
                        // same send queue as the PCM path.
                        if let Some(demuxer) = ts_demuxer_302m.as_mut() {
                            srt_run_compressed_302m_step(
                                &packet,
                                demuxer,
                                &mut aac_decoder_302m,
                                &mut audio_302m,
                                config,
                                &mut logged_302m_init_failure,
                                &mut logged_302m_decode_error,
                                &decode_stats_302m,
                            );
                            if !decode_stats_302m_registered
                                && let Some(dec) = aac_decoder_302m.as_ref() {
                                    stats.set_decode_stats(
                                        decode_stats_302m.clone(),
                                        dec.codec_name(),
                                        dec.sample_rate(),
                                        dec.channels(),
                                    );
                                    decode_stats_302m_registered = true;
                                }
                            if let Some(pipeline) = audio_302m.as_mut() {
                                for datagram in pipeline.take_ready_datagrams() {
                                    match send_tx.try_send((datagram, packet.recv_time_us)) {
                                        Ok(()) => {}
                                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                            stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                            tracing::warn!(
                                                "SRT output '{}' connection lost (302M compressed), will reconnect",
                                                config.id
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        // 302M path takes precedence over the TS program filter
                        // (the modes are mutually exclusive — see validator).
                        if let Some(pipeline) = audio_302m.as_mut() {
                            pipeline.process(&packet);
                            for datagram in pipeline.take_ready_datagrams() {
                                match send_tx.try_send((datagram, packet.recv_time_us)) {
                                    Ok(()) => {}
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                        tracing::warn!(
                                            "SRT output '{}' connection lost (302M), will reconnect",
                                            config.id
                                        );
                                        break;
                                    }
                                }
                            }
                            continue;
                        }
                        // Apply program filter if configured. Skip the packet
                        // entirely when the filter eats it (no target-program
                        // bytes in this datagram).
                        let payload = if let Some(filter) = program_filter.as_mut() {
                            match filter.filter_packet(&packet, &mut filter_scratch) {
                                Some(b) => b,
                                None => continue,
                            }
                        } else {
                            packet.data
                        };

                        // Hand to the transcode chain (codec thread) if
                        // configured, otherwise pass through to the
                        // downstream pipeline (CBR padder, PID rewrites,
                        // sink). The chain runs encoders off the Tokio
                        // runtime so heavy encode bursts don't leak
                        // scheduling jitter to other tasks; output
                        // arrives asynchronously and we drain it inline
                        // here.
                        let need_strip = transcode_chain.is_some();
                        let payload = if let Some(chain) = transcode_chain.as_ref() {
                            const RTP_HEADER_MIN: usize = 12;
                            let ts_in: &[u8] = if packet.is_raw_ts {
                                &payload[..]
                            } else if payload.len() > RTP_HEADER_MIN {
                                &payload[RTP_HEADER_MIN..]
                            } else {
                                continue;
                            };
                            if chain.try_submit(Bytes::copy_from_slice(ts_in)).is_err() {
                                stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            // Drain whatever the codec thread has produced
                            // since last poll. If nothing's ready yet,
                            // skip this iteration — encoded output will
                            // come on a later iteration.
                            let Some(transcoded) =
                                transcode_chain.as_mut().and_then(|c| c.try_recv())
                            else {
                                continue;
                            };
                            // Apply CBR null padding to the transcoded
                            // bytes, then forward.
                            let mut pad_out: Vec<u8> = Vec::new();
                            let final_bytes: &[u8] =
                                if let Some(padder) = null_padder.as_mut() {
                                    padder.process(&transcoded, &mut pad_out);
                                    &pad_out
                                } else {
                                    &transcoded
                                };
                            if final_bytes.is_empty() {
                                continue;
                            }
                            Bytes::copy_from_slice(final_bytes)
                        } else {
                            payload
                        };

                        // Per-program role-keyed PID rewrite (passthrough only).
                        let payload = if let Some(rw) = pid_overrides_rewriter.as_mut() {
                            let temp = RtpPacket {
                                data: payload,
                                sequence_number: packet.sequence_number,
                                rtp_timestamp: packet.rtp_timestamp,
                                recv_time_us: packet.recv_time_us,
                                is_raw_ts: packet.is_raw_ts || need_strip,
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            };
                            match rw.process_packet(&temp, &mut overrides_scratch) {
                                Some(b) => b,
                                None => continue,
                            }
                        } else {
                            payload
                        };

                        // Apply PID remap last so audio/video replacers saw the
                        // original PIDs in the source PMT. need_strip implies
                        // the bytes are now raw TS (RTP header was discarded).
                        let payload = if let Some(remapper) = pid_remapper.as_mut() {
                            let temp = RtpPacket {
                                data: payload,
                                sequence_number: packet.sequence_number,
                                rtp_timestamp: packet.rtp_timestamp,
                                recv_time_us: packet.recv_time_us,
                                is_raw_ts: packet.is_raw_ts || need_strip,
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            };
                            match remapper.process_packet(&temp, &mut remap_scratch) {
                                Some(b) => b,
                                None => continue,
                            }
                        } else {
                            payload
                        };

                        if let Some(ref mut db) = delay_buf {
                            db.push(RtpPacket {
                                data: payload,
                                sequence_number: packet.sequence_number,
                                rtp_timestamp: packet.rtp_timestamp,
                                recv_time_us: packet.recv_time_us,
                                is_raw_ts: packet.is_raw_ts,
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            });
                        } else {
                            payloads_to_send.push((payload, packet.recv_time_us));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "SRT output '{}' lagged, dropped {n} packets",
                            config.id
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            "SRT output '{}' broadcast channel closed",
                            config.id
                        );
                        drop(send_tx);
                        let _ = send_handle.await;
                        return Ok(false);
                    }
                }
            }
            _ = &mut delay_sleep, if delay_buf.as_ref().is_some_and(|db| db.len() > 0) => {
                let db = delay_buf.as_mut().unwrap();
                let now = now_us();
                for packet in db.drain_ready(now) {
                    payloads_to_send.push((packet.data, packet.recv_time_us));
                }
            }
        }

        // Send all collected payloads to the SRT send task.
        for (payload, recv_time_us) in payloads_to_send {
            match send_tx.try_send((payload, recv_time_us)) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    send_queue_dropped += 1;
                    let now = Instant::now();
                    let due = send_queue_warned_at
                        .is_none_or(|t| now.duration_since(t) >= SEND_QUEUE_WARN_INTERVAL);
                    if due {
                        send_queue_warned_at = Some(now);
                        // Attribute the drop to the bound that actually
                        // rejected it. The two have opposite remedies —
                        // slots means the srt-io task is not draining fast
                        // enough, bytes means this publisher's items are
                        // large enough that 8 MiB is the binding constraint
                        // (crossover is 2048 B/item) — and the old wording
                        // asserted the first unconditionally, which a
                        // byte-bound rejection can contradict outright:
                        // that one fires with a nearly empty queue and a
                        // perfectly healthy io task.
                        let by_bytes = send_tx.byte_bound_rejections();
                        tracing::warn!(
                            output_id = %config.id,
                            dropped_total = send_queue_dropped,
                            dropped_by_byte_bound = by_bytes,
                            dropped_by_slot_bound =
                                send_queue_dropped.saturating_sub(by_bytes),
                            queued_bytes = send_tx.queued_bytes(),
                            capacity = SRT_SEND_QUEUE_CAPACITY,
                            max_bytes = SRT_SEND_QUEUE_MAX_BYTES,
                            "SRT output send queue full — discarding egress packets; \
                             compare dropped_by_slot_bound (the srt-io task is not \
                             draining fast enough to absorb the source's burst, a large \
                             IDR access unit being the usual cause) against \
                             dropped_by_byte_bound (this publisher's items are large \
                             enough that the 8 MiB ceiling binds before the slot count)"
                        );
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!(
                        "SRT output '{}' connection lost, will reconnect",
                        config.id
                    );
                    got_connection_lost = true;
                    break;
                }
            }
        }
        if got_connection_lost {
            break true;
        }
        // Second, independent liveness check. The `Closed` arm above only
        // fires once the `srt-io` send task has itself errored; if the send
        // path ever stops surfacing a dead peer, this notices anyway. It is a
        // status read on a watch channel, so it costs nothing to do per batch.
        if sink.peer_is_gone() {
            tracing::warn!(
                "SRT output '{}' peer gone (socket status {:?}), will reconnect",
                config.id,
                match &sink {
                    SrtSendSink::Socket(s) => Some(s.status()),
                    SrtSendSink::Group(_) => None,
                }
            );
            break true;
        }
    };

    drop(send_tx);
    let _ = send_handle.await;
    Ok(connection_lost)
}

// ---------------------------------------------------------------------------
// Dual-leg SRT output with SMPTE 2022-7 duplication
// ---------------------------------------------------------------------------

/// What the other leg of a 2022-7 SRT output pair is doing when one leg is
/// lost: `details.other_leg` on the leg-lost event.
///
/// This is deliberately **not** the forward loop's teardown verdict
/// ([`redundant_leg2_alive`]). That verdict counts a leg 2 whose connect is
/// still pending as able to carry, so the pair is not torn down (dropping
/// the leg-2 listener held inside the pending connect future) before leg 2
/// has had its budget. But such a leg carries nothing yet, and an event
/// saying the output runs on it would be false at exactly the moment the
/// output is dark.
///
/// `queued` is the outcome of handing the leg the current payload:
/// `Some(true)` queued, or shed on backpressure (a full queue is a live leg);
/// `Some(false)` its send task is gone; `None` no socket attached.
/// `connect_done` is whether the leg's connect has resolved this cycle.
/// Leg 1 is connected before a cycle starts, so it always passes `true`.
fn redundant_other_leg_state(queued: Option<bool>, connect_done: bool) -> &'static str {
    match queued {
        Some(true) => "carrying",
        Some(false) => "down",
        None if !connect_done => "connecting",
        None => "down",
    }
}

/// The forward loop's teardown verdict on leg 2 of a 2022-7 SRT output:
/// whether it can still carry this cycle. The pair is torn down and
/// reconnected once neither leg can.
///
/// Same inputs as [`redundant_other_leg_state`]. A leg 2 with no socket is
/// alive while its connect is pending: it may yet connect and take over,
/// and tearing down then would drop the leg-2 listener held inside the
/// pending connect future. Once that connect has given up, leg 2 cannot
/// attach this cycle, so it is gone. It used to count as alive then as
/// well, and a lost leg 1 left the output dark until it was restarted,
/// whether leg 1 went after leg 2 gave up or while leg 2 was connecting.
///
/// Alive exactly when the leg-lost event would not call it `down`, so the
/// event and the teardown never disagree.
fn redundant_leg2_alive(queued: Option<bool>, connect_done: bool) -> bool {
    queued.unwrap_or(!connect_done)
}

/// Raise the Warning for a lost leg of a 2022-7 SRT output.
///
/// The message is fixed per leg. The manager folds an unacknowledged event
/// into an existing row with the same message, and `details` are not part of
/// that key, so a message that varied with the other leg's state would split
/// one leg's losses across rows. The other leg's state rides in `details`.
///
/// The error code is distinct from the RTP input's flow-scoped
/// `redundancy_leg_dropped`, so a manager Routine filtered on that code does
/// not start firing on an output's legs as well.
fn emit_redundant_output_leg_lost(
    events: &EventSender,
    output_id: &str,
    leg: u8,
    mode: SrtMode,
    other_leg: &'static str,
) {
    events.emit_output_with_details(
        EventSeverity::Warning,
        category::REDUNDANCY,
        format!("SRT output '{output_id}' 2022-7 leg {leg} lost; down until the pair reconnects"),
        output_id,
        serde_json::json!({
            "error_code": "redundancy_output_leg_lost",
            "leg": leg,
            "mode": format!("{mode:?}"),
            "other_leg": other_leg,
        }),
    );
}

/// Core send loop for dual-leg SRT output with SMPTE 2022-7 packet duplication.
///
/// Connects two independent SRT legs (leg 1 from the main config, leg 2 from
/// the redundancy config). Each leg gets its own dedicated Tokio send task
/// and mpsc channel, following the same pattern as the single-leg output.
///
/// Listener-mode legs bind once and re-accept on disconnect; caller-mode legs
/// reconnect with exponential back-off.
///
/// Each leg runs its own SRT stats poller: leg 1 publishes `srt_stats`, leg 2
/// `srt_leg2_stats`. A leg's poller is cancelled, which clears its stats,
/// when the leg is lost and at the end of every connect cycle.
///
/// A leg whose send task dies raises a `redundancy` Warning
/// ([`emit_redundant_output_leg_lost`]) once per cycle. The lost leg stays
/// down until the pair reconnects, which happens only once both legs are
/// gone. The other leg carries on alone meanwhile. A leg 2 whose connect
/// gave up within `leg2_connect_budget` counts as gone
/// ([`redundant_leg2_alive`]), so losing leg 1 then reconnects the pair and
/// leg 2 gets a fresh budget.
///
/// Stats counting: while leg 1 is up, only leg 1 increments `packets_sent`,
/// `bytes_sent` and the latency, to avoid double-counting, since both legs
/// carry identical data. Once leg 1's send fails, leg 2's send task counts
/// instead, so a pair carried by leg 2 alone does not read `idle`. Around the
/// handoff a packet can be counted twice or not at all, bounded by how far
/// the two legs' queues were apart. That is stats only.
async fn srt_output_redundant_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    _flow_id: &str,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    // Redundant 2022-7 path is passthrough-only (no replacers); kept
    // for signature parity with the single-leg loop.
    _active_input_rx: tokio::sync::watch::Receiver<String>,
    // How long leg 2 is given to connect in each cycle:
    // `INITIAL_LEG2_CONNECT_BUDGET`, shorter only in tests.
    leg2_connect_budget: Duration,
) -> anyhow::Result<()> {
    let redundancy = config
        .redundancy
        .as_ref()
        .expect("redundancy config must be present");

    // Persistent listeners for any legs in listener mode (kept across
    // reconnect cycles so ports stay bound).
    let mut listener_leg1 = if config.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_output(config).await?)
    } else {
        None
    };
    let mut listener_leg2 = if redundancy.mode == SrtMode::Listener {
        Some(bind_srt_listener_for_redundancy(redundancy).await?)
    } else {
        None
    };

    // Optional MPTS → SPTS program filter (shared across both legs and across reconnects).
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (2022-7): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut filter_scratch: Vec<u8> = Vec::new();

    // Optional PID remapper. Both legs must emit identical payloads for the
    // receiver merger to dedupe by seq; a single remapper fed by both legs
    // guarantees that.
    let mut pid_remapper = build_pid_remapper(config);
    let mut pid_overrides_rewriter = build_pid_overrides_rewriter(config);
    let mut remap_scratch: Vec<u8> = Vec::new();
    let mut overrides_scratch: Vec<u8> = Vec::new();

    // Outer reconnection loop — restarts both legs on total connection loss
    loop {
        // Connect/accept leg 1
        let socket_leg1 = if let Some(ref mut listener) = listener_leg1 {
            match accept_srt_connection(listener, &cancel).await {
                Ok(s) => s,
                Err(_) if cancel.is_cancelled() => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            match connect_srt_output(config, &cancel).await {
                Ok(s) => s,
                Err(e) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    return Err(e);
                }
            }
        };
        tracing::info!(
            "SRT output '{}' leg1 connected: mode={:?} local={}",
            config.id,
            config.mode,
            config.local_addr.as_deref().unwrap_or("auto")
        );
        events.emit_output_with_details(
            EventSeverity::Info, category::SRT,
            format!("SRT output '{}' connected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "leg": 1,
                "local_addr": config.local_addr.as_deref().unwrap_or("auto"),
            }),
        );

        // Subscribe to broadcast and spawn leg 1 send task IMMEDIATELY.
        // Leg 2 is brought up in parallel (below) with `leg2_connect_budget` —
        // while that handshake is in progress we keep leg 1 hot so the receiver
        // keeps ACK'ing and the sender's libsrt send buffer never stalls
        // on an idle socket.
        let mut rx = broadcast_tx.subscribe();

        // Leg 1's stats until it is lost or the cycle ends; leg 2's from
        // when it attaches.
        let leg1_poller = cancel.child_token();
        spawn_srt_stats_poller(socket_leg1.clone(), stats.srt_stats_cache.clone(), None, leg1_poller.clone());
        let leg2_poller = cancel.child_token();
        // Set by leg 1's send task when a send fails. From then on leg 2's
        // task counts what the output sends, so a pair carried by leg 2
        // alone never reads idle. It is set only on a send error, never when
        // the queue closes at teardown, so a normal stop does not make leg 2
        // count what it drains on top of what leg 1 drained. An AtomicBool,
        // not a CancellationToken: leg 2's task reads it per item, and
        // `is_cancelled` takes a lock.
        let leg1_lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Each leg raises its leg-lost event at most once per cycle.
        let (mut leg1_up, mut leg2_up) = (true, false);

        let (send_tx_leg1, mut send_rx_leg1) = send_queue();
        let send_socket1 = socket_leg1.clone();
        let send_stats1 = stats.clone();
        let output_id1 = config.id.clone();
        let leg1_lost_tx = leg1_lost.clone();
        let send_handle1 = tokio::spawn(async move {
            while let Some((data, recv_time_us)) = send_rx_leg1.recv().await {
                match srt_send_chunked(&send_socket1, &data).await {
                    Ok(sent) => {
                        send_stats1.packets_sent.fetch_add(1, Ordering::Relaxed);
                        send_stats1.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                        send_stats1.record_latency(recv_time_us);
                    }
                    Err(e) if is_transient_backpressure(&e) => {
                        send_stats1.packets_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!("SRT output '{}' leg1 send error: {e}", output_id1);
                        leg1_lost_tx.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });

        // Leg-2 connect/accept future, pinned so we can poll it inside the
        // main packet select! and complete it exactly once per reconnect
        // cycle. Bounded by `leg2_connect_budget` so a dead leg-2 peer
        // never gates leg-1. While this is pending, leg-1 is already pumping.
        let leg2_connect_fut = {
            let events = events.clone();
            let output_id = config.id.clone();
            let redundancy = redundancy.clone();
            let cancel = cancel.clone();
            let mut listener_leg2_mut = listener_leg2.take();
            async move {
                let result: (Option<Arc<SrtSocket>>, Option<srt_transport::SrtListener>) = if let Some(ref mut listener) = listener_leg2_mut {
                    let r = tokio::time::timeout(
                        leg2_connect_budget,
                        accept_srt_connection(listener, &cancel),
                    ).await;
                    match r {
                        Ok(Ok(s)) => (Some(s), listener_leg2_mut),
                        Ok(Err(_)) if cancel.is_cancelled() => (None, listener_leg2_mut),
                        Ok(Err(e)) => {
                            tracing::warn!("SRT output '{}' leg2 accept failed: {e}, running single-leg", output_id);
                            (None, listener_leg2_mut)
                        }
                        Err(_) => {
                            tracing::warn!(
                                "SRT output '{}' leg2 did not come up within {:?} (accept timed out), running single-leg",
                                output_id, leg2_connect_budget,
                            );
                            events.emit_output_with_details(
                                EventSeverity::Warning, category::SRT,
                                format!("SRT output '{}' 2022-7 leg 2 did not connect within {:?}; running single-leg", output_id, leg2_connect_budget),
                                &output_id,
                                serde_json::json!({
                                    "mode": format!("{:?}", redundancy.mode),
                                    "leg": 2, "reason": "accept timed out",
                                    "budget_ms": leg2_connect_budget.as_millis(),
                                }),
                            );
                            (None, listener_leg2_mut)
                        }
                    }
                } else {
                    match tokio::time::timeout(
                        leg2_connect_budget,
                        connect_srt_redundancy_leg(&redundancy, &cancel),
                    ).await {
                        Ok(Ok(s)) => (Some(s), None),
                        Ok(Err(_)) if cancel.is_cancelled() => (None, None),
                        Ok(Err(e)) => {
                            tracing::warn!("SRT output '{}' leg2 connection failed: {e}, running single-leg", output_id);
                            (None, None)
                        }
                        Err(_) => {
                            tracing::warn!(
                                "SRT output '{}' leg2 did not come up within {:?} (caller connect retry budget exhausted), running single-leg",
                                output_id, leg2_connect_budget,
                            );
                            events.emit_output_with_details(
                                EventSeverity::Warning, category::SRT,
                                format!("SRT output '{}' 2022-7 leg 2 did not connect within {:?}; running single-leg", output_id, leg2_connect_budget),
                                &output_id,
                                serde_json::json!({
                                    "mode": format!("{:?}", redundancy.mode),
                                    "leg": 2, "reason": "caller connect retry budget exhausted",
                                    "budget_ms": leg2_connect_budget.as_millis(),
                                }),
                            );
                            (None, None)
                        }
                    }
                };
                if result.0.is_some() {
                    tracing::info!(
                        "SRT output '{}' leg2 connected: mode={:?} local={}",
                        output_id,
                        redundancy.mode,
                        redundancy.local_addr.as_deref().unwrap_or("auto")
                    );
                }
                result
            }
        };
        tokio::pin!(leg2_connect_fut);
        let mut leg2_done = false;

        // leg 2 send task is spawned lazily once the leg-2 future resolves.
        let mut socket_leg2: Option<Arc<SrtSocket>> = None;
        let mut send_tx_leg2_opt: Option<SendQueueTx> = None;

        // Optional output delay buffer for stream synchronization.
        let resolved = if let Some(ref delay) = config.delay {
            resolve_output_delay(delay, &config.id, frame_rate_rx.clone(), &cancel).await
        } else {
            None
        };
        let (mut delay_buf, _frame_rate_watch) = match resolved {
            Some((buf, watch)) => (Some(buf), watch),
            None => (None, None),
        };

        let delay_sleep = tokio::time::sleep(Duration::from_secs(86400));
        tokio::pin!(delay_sleep);

        // Packet forwarding loop — runs until both legs die or cancel
        let mut broadcast_closed = false;
        loop {
            if let Some(ref db) = delay_buf
                && let Some(release_us) = db.next_release_time() {
                    let now = now_us();
                    let wait = release_us.saturating_sub(now);
                    delay_sleep.as_mut().reset(Instant::now() + Duration::from_micros(wait));
                }

            let mut payloads_to_send: Vec<(Bytes, u64)> = Vec::new();

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("SRT output '{}' (redundant) stopping (cancelled)", config.id);
                    drop(send_tx_leg1);
                    drop(send_tx_leg2_opt);
                    let _ = send_handle1.await;
                    let _ = socket_leg1.close().await;
                    if let Some(ref s) = socket_leg2 {
                        let _ = s.close().await;
                    }
                    if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                    return Ok(());
                }
                // Leg 2 connector finished — attach send task (or stay single-leg).
                // Guarded by `!leg2_done` so select! won't re-poll a complete future.
                (sock, returned_listener) = &mut leg2_connect_fut, if !leg2_done => {
                    leg2_done = true;
                    if returned_listener.is_some() {
                        listener_leg2 = returned_listener;
                    }
                    if let Some(sock2) = sock {
                        spawn_srt_stats_poller(sock2.clone(), stats.srt_leg2_stats_cache.clone(), None, leg2_poller.clone());
                        let (tx2, mut rx2) = send_queue();
                        let send_socket2 = sock2.clone();
                        let send_stats2 = stats.clone();
                        let leg1_lost2 = leg1_lost.clone();
                        let output_id2 = config.id.clone();
                        tokio::spawn(async move {
                            while let Some((data, recv_time_us)) = rx2.recv().await {
                                // While leg 1 is up it counts what the pair
                                // sends, so leg 2 counts nothing. Once leg
                                // 1's send has failed, leg 2 counts.
                                let counts = leg1_lost2.load(Ordering::Relaxed);
                                match srt_send_chunked(&send_socket2, &data).await {
                                    Ok(sent) => {
                                        if counts {
                                            send_stats2.packets_sent.fetch_add(1, Ordering::Relaxed);
                                            send_stats2.bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                                            send_stats2.record_latency(recv_time_us);
                                        }
                                    }
                                    Err(e) if is_transient_backpressure(&e) => {
                                        // Transient Rust-side channel pressure
                                        // on leg 2 only. Drop this packet and
                                        // continue. While leg 1 is up it
                                        // carries the dupe under 2022-7, so
                                        // the receiver merge is unaffected
                                        // and nothing is counted; once leg 1
                                        // is lost the drop is the output's.
                                        if counts {
                                            send_stats2.packets_dropped.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("SRT output '{}' leg2 send error: {e}", output_id2);
                                        break;
                                    }
                                }
                            }
                        });
                        socket_leg2 = Some(sock2);
                        send_tx_leg2_opt = Some(tx2);
                        leg2_up = true;
                        // Its own message: the manager folds an event into
                        // an unacknowledged row with the same message, so
                        // leg 1's text would hide this one in leg 1's row.
                        events.emit_output_with_details(
                            EventSeverity::Info, category::SRT,
                            format!("SRT output '{}' 2022-7 leg 2 connected", config.id), &config.id,
                            serde_json::json!({
                                "mode": format!("{:?}", redundancy.mode),
                                "leg": 2,
                                "local_addr": redundancy.local_addr.as_deref().unwrap_or("auto"),
                            }),
                        );
                    }
                    continue;
                }
                result = rx.recv() => {
                    match result {
                        Ok(packet) => {
                            // Apply program filter if configured.
                            let recv_time_us = packet.recv_time_us;
                            let payload = if let Some(ref mut filter) = program_filter {
                                match filter.filter_packet(&packet, &mut filter_scratch) {
                                    Some(b) => b,
                                    None => continue,
                                }
                            } else {
                                packet.data
                            };

                            // Per-program role-keyed PID rewrite (passthrough
                            // only — the transcoded path runs through
                            // replacers earlier).
                            let payload = if let Some(ref mut rw) = pid_overrides_rewriter {
                                let temp = RtpPacket {
                                    data: payload,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
                                };
                                match rw.process_packet(&temp, &mut overrides_scratch) {
                                    Some(b) => b,
                                    None => continue,
                                }
                            } else {
                                payload
                            };

                            // Apply PID remap so both legs carry the remapped bytes.
                            let payload = if let Some(ref mut remapper) = pid_remapper {
                                let temp = RtpPacket {
                                    data: payload,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
                                };
                                match remapper.process_packet(&temp, &mut remap_scratch) {
                                    Some(b) => b,
                                    None => continue,
                                }
                            } else {
                                payload
                            };

                            if let Some(ref mut db) = delay_buf {
                                db.push(RtpPacket {
                                    data: payload,
                                    sequence_number: packet.sequence_number,
                                    rtp_timestamp: packet.rtp_timestamp,
                                    recv_time_us: packet.recv_time_us,
                                    is_raw_ts: packet.is_raw_ts,
                                    upstream_seq: None,
                                    upstream_leg_id: None,
                                    sender_timestamp_us: None,
                                });
                            } else {
                                payloads_to_send.push((payload, recv_time_us));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            stats.packets_dropped.fetch_add(n, Ordering::Relaxed);
                            tracing::warn!(
                                "SRT output '{}' (redundant) lagged, dropped {n} packets",
                                config.id
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!(
                                "SRT output '{}' broadcast channel closed",
                                config.id
                            );
                            broadcast_closed = true;
                            break;
                        }
                    }
                }
                _ = &mut delay_sleep, if delay_buf.as_ref().is_some_and(|db| db.len() > 0) => {
                    let db = delay_buf.as_mut().unwrap();
                    let now = now_us();
                    for packet in db.drain_ready(now) {
                        payloads_to_send.push((packet.data, packet.recv_time_us));
                    }
                }
            }

            // Send to both legs.
            //
            // `Full` and `Closed` are **different verdicts** and must not both
            // mean "this leg is lost". `Full` is backpressure — the queue doing
            // exactly its job, shedding a packet while the leg stays up — and
            // treating it as a lost leg violates the node-wide rule that slow
            // consumers see dropped packets, never cascading teardown. Only
            // `Closed` (the leg's send task has died) is a liveness verdict.
            //
            // This mattered little while `Full` needed 4096 queued whole access
            // units (~164 MB, effectively unreachable). The 8 MiB ceiling makes
            // it ~56-66 IDRs, which an ordinary burst against a briefly-stalled
            // srt-io task reaches — so a transient drain stall would otherwise
            // become a real dual-socket teardown plus an operator alarm, on
            // precisely the source class this change was written for.
            let mut all_legs_lost = false;
            for (payload, recv_time_us) in payloads_to_send {
                let leg1_alive = match send_tx_leg1.try_send((payload.clone(), recv_time_us)) {
                    Ok(()) => true,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                        true
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                };

                // `None` while no leg-2 socket is attached.
                let leg2_queued = send_tx_leg2_opt.as_ref().map(|tx2| {
                    match tx2.try_send((payload, recv_time_us)) {
                        Ok(()) => true,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                            true
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
                    }
                });
                // An absent leg 2 is "not connected yet", not "lost", while
                // its connect is pending. Leg 2 attaches lazily once its
                // connect future resolves, and while it was counted as a
                // dead leg a single rejection on leg 1 was enough to satisfy
                // the teardown condition for the whole pre-connection
                // window. Once the connect has resolved with no socket
                // (`leg2_done`) it is gone, so a lost leg 1 reconnects the
                // pair, whether leg 1 went after leg 2 gave up or while it
                // was connecting: in that order the next payload after the
                // give-up finds both gone.
                let leg2_alive = redundant_leg2_alive(leg2_queued, leg2_done);

                // Each leg's loss is raised here, at most once per cycle,
                // and not from its send task: at teardown the sockets close
                // while leg 2's task, which is never awaited, may still be
                // draining, and its send error then is no lost leg.
                if leg1_up && !leg1_alive {
                    leg1_up = false;
                    leg1_poller.cancel();
                    emit_redundant_output_leg_lost(
                        events,
                        &config.id,
                        1,
                        config.mode,
                        redundant_other_leg_state(leg2_queued, leg2_done),
                    );
                }
                if leg2_up && !leg2_alive {
                    leg2_up = false;
                    leg2_poller.cancel();
                    emit_redundant_output_leg_lost(
                        events,
                        &config.id,
                        2,
                        redundancy.mode,
                        redundant_other_leg_state(Some(leg1_alive), true),
                    );
                }

                if !leg1_alive && !leg2_alive {
                    // Reaching here now means both legs' send tasks are
                    // genuinely gone (`Closed`), not merely congested. Log the
                    // queue state anyway so a teardown is always traceable to
                    // the state that produced it.
                    tracing::warn!(
                        output_id = %config.id,
                        leg1_queued_bytes = send_tx_leg1.queued_bytes(),
                        leg1_byte_bound_rejections = send_tx_leg1.byte_bound_rejections(),
                        leg2_queued_bytes = send_tx_leg2_opt
                            .as_ref()
                            .map(|t| t.queued_bytes()),
                        leg2_byte_bound_rejections = send_tx_leg2_opt
                            .as_ref()
                            .map(|t| t.byte_bound_rejections()),
                        max_bytes = SRT_SEND_QUEUE_MAX_BYTES,
                        "SRT output all legs lost, will reconnect"
                    );
                    all_legs_lost = true;
                    break;
                }
            }
            if all_legs_lost {
                break;
            }
        }

        // Cleanup sockets before reconnect (listeners stay open). The
        // pollers go first, so neither leg's stats outlive its socket. (On
        // the cancel arm above they are already gone: both are children of
        // `cancel`.)
        leg1_poller.cancel();
        leg2_poller.cancel();
        drop(send_tx_leg1);
        drop(send_tx_leg2_opt);
        let _ = send_handle1.await;
        let _ = socket_leg1.close().await;
        if let Some(ref s) = socket_leg2 {
            let _ = s.close().await;
        }

        if broadcast_closed {
            if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
            if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
            return Ok(());
        }

        // Brief delay before reconnect attempt
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("SRT output '{}' (redundant) stopping during reconnect delay", config.id);
                if let Some(ref l) = listener_leg1 { let _ = l.close().await; }
                if let Some(ref l) = listener_leg2 { let _ = l.close().await; }
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }

        events.emit_output_with_details(
            EventSeverity::Warning, category::SRT,
            format!("SRT output '{}' disconnected", config.id), &config.id,
            serde_json::json!({
                "mode": format!("{:?}", config.mode),
                "redundant": true,
            }),
        );
        tracing::info!("SRT output '{}' (redundant) attempting reconnection...", config.id);
    }
}

/// One step of the compressed-audio bridge for the SRT output's 302M mode.
///
/// Demuxes any TS packets in `packet`, decodes ADTS AAC frames into planar
/// f32 PCM, and feeds them through `S302mOutputPipeline::process_planar`.
/// The AAC decoder and the pipeline are both built lazily on the first
/// successfully-parsed AAC frame, once the sample rate and channel count
/// are known.
///
/// Mirrors `run_compressed_302m_step` in `output_rtp_audio.rs` — same
/// lifecycle, same lazy-init logic, same once-per-kind log deduplication.
fn srt_run_compressed_302m_step(
    packet: &RtpPacket,
    demuxer: &mut crate::engine::ts_demux::TsDemuxer,
    decoder: &mut Option<crate::engine::audio_decode::AacDecoder>,
    pipeline: &mut Option<S302mOutputPipeline>,
    config: &SrtOutputConfig,
    logged_init_failure: &mut bool,
    logged_decode_error: &mut Option<String>,
    decode_stats: &std::sync::Arc<crate::engine::audio_decode::DecodeStats>,
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
                        "SRT output '{}' (302M) AAC frame without cached config",
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
                    match S302mOutputPipeline::new(input, out_channels, 24, 4_000, config.pid_overrides.as_ref()) {
                        Ok(p) => {
                            tracing::info!(
                                "SRT output '{}' (302M) compressed bridge ready: \
                                 AAC-LC {sample_rate} Hz, {channels} ch -> 48000 Hz {out_channels} ch L24 302M",
                                config.id
                            );
                            *decoder = Some(d);
                            *pipeline = Some(p);
                        }
                        Err(e) => {
                            if !*logged_init_failure {
                                tracing::error!(
                                    "SRT output '{}' (302M) S302mOutputPipeline build failed: {e}",
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
                            "SRT output '{}' (302M) AAC decoder init failed: {e}",
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

        decode_stats.inc_input();
        match dec.decode_frame(&data) {
            Ok(planar) => {
                decode_stats.inc_output();
                p.process_planar(&planar, packet.recv_time_us);
            }
            Err(e) => {
                decode_stats.inc_error();
                let kind = format!("{e}");
                if logged_decode_error.as_deref() != Some(kind.as_str()) {
                    tracing::warn!(
                        "SRT output '{}' (302M) AAC decode error: {e}",
                        config.id
                    );
                    *logged_decode_error = Some(kind);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Native libsrt SRT bonding (socket group) output
// ---------------------------------------------------------------------------

/// Bonded SRT output (libsrt native socket group).
///
/// Reuses the shared [`srt_output_forward_loop`] pipeline so `program_number`,
/// `audio_encode` (with optional companion `transcode`), `video_encode`,
/// output `delay`, and `transport_mode: audio_302m` all compose with
/// bonding the same way they compose with a plain SRT output — the only
/// change at the transport edge is that outbound payloads land on an
/// `SrtGroup::send()` (caller mode) or on an accepted listener socket
/// whose libsrt session is a group internally (listener mode).
///
/// Caller mode opens the group against every `bonding.endpoints[]` entry.
/// Listener mode binds once with `SRTO_GROUPCONNECT=true`; libsrt returns
/// a socket handle on accept whose `srt_send` / `srt_recv` / `srt_bstats`
/// calls all operate on the underlying group.
#[allow(clippy::too_many_arguments)]
async fn srt_output_bonded_loop(
    config: &SrtOutputConfig,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    stats: Arc<OutputStatsAccumulator>,
    cancel: CancellationToken,
    events: &EventSender,
    _flow_id: &str,
    input_format: Option<InputFormat>,
    compressed_audio_input: bool,
    frame_rate_rx: Option<tokio::sync::watch::Receiver<Option<f64>>>,
    active_input_rx: tokio::sync::watch::Receiver<String>,
) -> anyhow::Result<()> {
    let bond = config
        .bonding
        .as_ref()
        .expect("bonding config must be present")
        .clone();

    // Shared pipeline state — persists across reconnects exactly like the
    // non-bonded caller/listener loops.
    let mut program_filter = config.program_number.map(|n| {
        tracing::info!(
            "SRT output '{}' (bonded): program filter enabled, target program_number = {}",
            config.id, n
        );
        TsProgramFilter::new(n)
    });
    let mut pid_remapper = build_pid_remapper(config);
    let mut pid_overrides_rewriter = build_pid_overrides_rewriter(config);
    let mut transcode_chain =
        build_transcode_chain_for_srt(config, events, &stats);
    let mut null_padder = config
        .cbr_pad_to_kbps
        .map(crate::engine::ts_null_padder::TsNullPadder::new);
    spawn_replacer_switch_watcher(
        &config.id,
        transcode_chain.as_ref(),
        active_input_rx,
        cancel.clone(),
    );

    match config.mode {
        SrtMode::Listener => {
            let mut listener = bind_srt_listener_for_bonded_output(config).await?;
            loop {
                let socket = match accept_srt_connection(&mut listener, &cancel).await {
                    Ok(s) => s,
                    Err(_) if cancel.is_cancelled() => {
                        let _ = listener.close().await;
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::error!(
                            "SRT bonded output '{}' accept failed: {e}",
                            config.id
                        );
                        let _ = listener.close().await;
                        return Err(e);
                    }
                };
                tracing::info!(
                    "SRT bonded output '{}' accepted bonded caller (local={}, mode={:?})",
                    config.id,
                    config.local_addr.as_deref().unwrap_or("auto"),
                    bond.mode,
                );
                events.emit_output_with_details(
                    EventSeverity::Info,
                    category::SRT,
                    format!("SRT bonded output '{}' connected", config.id),
                    &config.id,
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
                    stats.srt_bonding_stats_cache.clone(),
                    None,
                    poller_cancel.clone(),
                    Some(BondHealthTracker::new(
                        BondEvents::new(
                            BondEventScope::Output { output_id: config.id.clone() },
                            events.clone(),
                            bond.mode,
                            "listener-bonded",
                        ),
                        degrade_after(config.connect_timeout_secs),
                    )),
                );

                let mut rx = broadcast_tx.subscribe();
                let sink = SrtSendSink::Socket(socket.clone());
                let disconnected = srt_output_forward_loop(
                    config, &mut rx, &stats, &cancel, &sink,
                    &mut program_filter, &mut pid_remapper, &mut pid_overrides_rewriter,
                    &mut transcode_chain,
                    &mut null_padder,
                    input_format, compressed_audio_input, frame_rate_rx.clone(),
                )
                .await?;
                poller_cancel.cancel();
                let _ = socket.close().await;

                if !disconnected {
                    let _ = listener.close().await;
                    return Ok(());
                }
                events.emit_output_with_details(
                    EventSeverity::Warning,
                    category::SRT,
                    format!("SRT bonded output '{}' disconnected", config.id),
                    &config.id,
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
        _ => {
            // Validation guarantees the plan; if it cannot be made, the
            // connect below fails with the same error.
            let plans = plan_bond_members(&bond).unwrap_or_default();
            // Outlives each connected group (see `BondLatch`).
            let latch = Arc::new(BondLatch::default());
            let bond_events = BondEvents::new(
                BondEventScope::Output { output_id: config.id.clone() },
                events.clone(),
                bond.mode,
                "caller-bonded",
            );
            loop {
                let params = SrtConnectionParams::from(config);
                let group = match connect_srt_group(&params, &bond, &cancel).await {
                    Ok(g) => g,
                    Err(e) => {
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        tracing::error!(
                            "SRT bonded output '{}' connect failed: {e}",
                            config.id
                        );
                        return Err(e);
                    }
                };
                tracing::info!(
                    "SRT bonded output '{}' connected: mode=caller bonding={:?} endpoints={}",
                    config.id,
                    bond.mode,
                    bond.endpoints.len(),
                );
                events.emit_output_with_details(
                    EventSeverity::Info,
                    category::SRT,
                    format!("SRT bonded output '{}' connected", config.id),
                    &config.id,
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
                    stats.srt_bonding_stats_cache.clone(),
                    None,
                    poller_cancel.clone(),
                    supervisor,
                );

                let mut rx = broadcast_tx.subscribe();
                let sink = SrtSendSink::Group(group.clone());
                let disconnected = srt_output_forward_loop(
                    config, &mut rx, &stats, &cancel, &sink,
                    &mut program_filter, &mut pid_remapper, &mut pid_overrides_rewriter,
                    &mut transcode_chain,
                    &mut null_padder,
                    input_format, compressed_audio_input, frame_rate_rx.clone(),
                )
                .await?;
                poller_cancel.cancel();
                // Join the poller first: it hands back the supervisor whose
                // member tracks the report is built from.
                let members = if disconnected {
                    final_member_report(poller).await
                } else {
                    serde_json::Value::Null
                };
                let _ = group.close().await;

                if !disconnected {
                    return Ok(());
                }
                events.emit_output_with_details(
                    EventSeverity::Warning,
                    category::SRT,
                    format!("SRT bonded output '{}' disconnected", config.id),
                    &config.id,
                    serde_json::json!({ "mode": "caller-bonded", "members": members }),
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TrySendError;

    fn bonded_caller_output(endpoints: serde_json::Value) -> SrtOutputConfig {
        serde_json::from_value(serde_json::json!({
            "id": "o1",
            "name": "o1",
            "mode": "caller",
            "remote_addr": "127.0.0.1:9000",
            "bonding": { "mode": "broadcast", "endpoints": endpoints },
        }))
        .expect("SrtOutputConfig")
    }

    /// A bonding member's NIC address is no longer baked into the config at
    /// spawn: it is resolved again on every connect and re-add, so a member
    /// follows a NIC whose address changes, and an explicit `local_addr` can
    /// still be told apart from a resolved one.
    #[cfg(target_os = "linux")]
    #[test]
    fn bonding_endpoints_are_checked_not_baked_in() {
        let mut config = bonded_caller_output(serde_json::json!([
            { "addr": "127.0.0.1:9000", "interface_binding": { "name": "lo" } },
            { "addr": "127.0.0.1:9001" },
        ]));
        apply_srt_output_interface_bindings(&mut config).unwrap();
        let bonding = config.bonding.unwrap();
        assert!(bonding.endpoints.iter().all(|ep| ep.local_addr.is_none()));
    }

    /// The spawn-time abort is unchanged: a bonding endpoint whose NIC is
    /// missing still refuses to start the output, unless it has an explicit
    /// `local_addr`, which wins and leaves the NIC unread.
    #[test]
    fn a_bonding_endpoint_on_a_missing_nic_still_refuses_to_start() {
        let mut config = bonded_caller_output(serde_json::json!([
            { "addr": "127.0.0.1:9000", "interface_binding": { "name": "no-such-nic0" } },
            { "addr": "127.0.0.1:9001" },
        ]));
        assert!(apply_srt_output_interface_bindings(&mut config).is_err());
        let mut config = bonded_caller_output(serde_json::json!([
            { "addr": "127.0.0.1:9000", "local_addr": "127.0.0.1:0",
              "interface_binding": { "name": "no-such-nic0" } },
            { "addr": "127.0.0.1:9001" },
        ]));
        assert!(apply_srt_output_interface_bindings(&mut config).is_ok());
    }

    fn item(bytes: usize) -> (Bytes, u64) {
        (Bytes::from(vec![0u8; bytes]), 0)
    }

    // ── Egress queue byte bound (issue #98) ──────────────────────────
    //
    // An item is a whole upstream `RtpPacket`, and its size is set by the
    // publisher: 188 B from SDI before edge#166, ~1316 B from TS passthrough, a whole
    // access unit (123-144 KB measured) from RTMP/RTSP. A slot count is
    // therefore not a memory bound.

    #[test]
    fn ts_class_items_are_still_bounded_by_slots_not_bytes() {
        // The property that preserves #95: for the two item shapes that
        // change actually measured, the slot cap must bind first, or this
        // change would silently undo the fix it sits on top of.
        const { assert!(SRT_SEND_QUEUE_CAPACITY * 1316 < SRT_SEND_QUEUE_MAX_BYTES) };
        const { assert!(SRT_SEND_QUEUE_CAPACITY * 188 < SRT_SEND_QUEUE_MAX_BYTES) };
    }

    #[test]
    fn the_bound_crossover_is_two_kilobytes_per_item() {
        // Pin the honest statement of when each bound binds. The rationale
        // comment claimed the slot cap binds first for "every class the
        // depth was raised for"; it does not. Above 2 KiB/item the byte
        // ceiling binds, and that is reachable from config — a
        // media-player or test-pattern input may set
        // `ts_packets_per_datagram` up to 348 (65 424 B), validated in
        // `config::validation`, which is 128 items rather than 4096.
        let crossover = SRT_SEND_QUEUE_MAX_BYTES / SRT_SEND_QUEUE_CAPACITY;
        assert_eq!(crossover, 2048);
        const { assert!(SRT_SEND_QUEUE_MAX_BYTES / 65_424 < SRT_SEND_QUEUE_CAPACITY) };
    }

    #[test]
    fn whole_access_unit_items_are_bounded_by_bytes() {
        // Use the IDR sizes actually measured on this path (123-144 KB),
        // not a flattering round number: 4096 slots would be 492-576 MB.
        for au in [123 * 1024, 144 * 1024] {
            assert!(SRT_SEND_QUEUE_MAX_BYTES / au < SRT_SEND_QUEUE_CAPACITY);
            // Still far more absorption than the ~2 items an IDR burst needs.
            assert!(SRT_SEND_QUEUE_MAX_BYTES / au >= 50);
        }
    }

    #[test]
    fn byte_cap_rejects_before_the_slot_cap_for_large_items() {
        let (tx, _rx) = send_queue();
        let big = 1024 * 1024; // 1 MiB items
        let mut accepted = 0;
        for _ in 0..SRT_SEND_QUEUE_CAPACITY {
            if tx.try_send(item(big)).is_err() {
                break;
            }
            accepted += 1;
        }
        // 8 MiB ceiling / 1 MiB items
        assert_eq!(accepted, SRT_SEND_QUEUE_MAX_BYTES / big);
        assert!(accepted < SRT_SEND_QUEUE_CAPACITY, "slot cap must not be what bound it");
        assert_eq!(tx.byte_bound_rejections(), 1, "the rejection must be attributed to bytes");
    }

    #[tokio::test]
    async fn dequeue_releases_the_reserved_bytes() {
        // Without the release the queue would wedge shut after one fill,
        // which is worse than the unbounded-memory bug being fixed.
        let (tx, mut rx) = send_queue();
        let big = 1024 * 1024;
        while tx.try_send(item(big)).is_ok() {}
        assert!(tx.try_send(item(big)).is_err(), "should be full");
        let got = rx.recv().await;
        assert!(got.is_some());
        tx.try_send(item(big))
            .expect("a dequeue must free room for another item");
    }

    #[test]
    fn rejection_does_not_leak_reserved_bytes() {
        // The reserve-then-offer order must unwind on rejection, or every
        // rejected item would permanently shrink the queue.
        let (tx, _rx) = send_queue();
        let big = 1024 * 1024;
        while tx.try_send(item(big)).is_ok() {}
        let settled = tx.queued_bytes();
        for _ in 0..10 {
            assert!(tx.try_send(item(big)).is_err());
        }
        assert_eq!(
            tx.queued_bytes(),
            settled,
            "a rejected item must not consume budget"
        );
    }

    #[test]
    fn an_item_larger_than_the_whole_ceiling_is_admitted_to_an_empty_queue() {
        // Liveness. Without the empty-queue exemption such an item is not
        // "transiently full" but permanently un-sendable, and in 2022-7
        // mode both legs reject it in lockstep, which the redundant loop
        // reads as `all_legs_lost` and answers by closing two healthy SRT
        // sockets — forever. ~9 MB frames are on record from the
        // media-player still-image path.
        let (tx, _rx) = send_queue();
        let oversized = SRT_SEND_QUEUE_MAX_BYTES + 1;
        tx.try_send(item(oversized))
            .expect("an empty queue must always admit one item, whatever its size");
        // ...and it is the *only* one admitted, so the bound really is
        // `max(ceiling, one item)` rather than unbounded.
        assert!(tx.try_send(item(1316)).is_err());
    }

    #[tokio::test]
    async fn an_oversized_item_does_not_wedge_the_queue_after_it_drains() {
        let (tx, mut rx) = send_queue();
        tx.try_send(item(SRT_SEND_QUEUE_MAX_BYTES + 1)).unwrap();
        assert!(rx.recv().await.is_some());
        assert_eq!(tx.queued_bytes(), 0, "draining must release the whole reservation");
        tx.try_send(item(1316))
            .expect("normal traffic must resume once the oversized item is gone");
    }

    #[test]
    fn dropping_the_receiver_releases_every_queued_reservation() {
        // The send task `break`s out of its recv loop on a non-transient
        // send error, dropping the receiver with items still queued. Those
        // reservations must not survive it.
        let (tx, rx) = send_queue();
        let big = 1024 * 1024;
        while tx.try_send(item(big)).is_ok() {}
        assert!(tx.queued_bytes() > 0);
        drop(rx);
        assert_eq!(tx.queued_bytes(), 0, "a dropped receiver must release what it held");
    }

    #[test]
    fn the_byte_ceiling_is_reached_at_the_measured_idr_sizes() {
        // Pins the depth at which `Full` starts appearing for whole-AU
        // publishers, because in the redundant loop that depth is the
        // trigger point for leg-liveness control flow — it should be
        // fixed by a test, not left implicit in a doc comment's arithmetic.
        for (au, want) in [(123 * 1024usize, 66usize), (144 * 1024, 56)] {
            let (tx, _rx) = send_queue();
            let mut accepted = 0;
            while tx.try_send(item(au)).is_ok() {
                accepted += 1;
            }
            assert_eq!(accepted, want, "au={au}");
        }
    }

    #[test]
    fn both_2022_7_legs_shed_the_same_oversized_item_without_either_dying() {
        // 2022-7 legs carry identical items, so they cross any size-derived
        // bound in lockstep. The redundant loop must not read that as "all
        // legs lost" — `Full` is backpressure, only `Closed` is a liveness
        // verdict. Both halves of that contract are asserted here: the
        // oversized item is admitted (not rejected in lockstep), and a
        // genuinely full queue still reports `Full` rather than `Closed`.
        let (leg1, _rx1) = send_queue();
        let (leg2, _rx2) = send_queue();
        let oversized = SRT_SEND_QUEUE_MAX_BYTES + 1;
        assert!(leg1.try_send(item(oversized)).is_ok());
        assert!(leg2.try_send(item(oversized)).is_ok());
        for (leg, label) in [(&leg1, "leg1"), (&leg2, "leg2")] {
            match leg.try_send(item(1316)) {
                Err(TrySendError::Full(_)) => {}
                other => panic!("{label}: a congested live leg must report Full, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_dead_consumer_reports_closed_not_full_even_at_the_byte_ceiling() {
        // The wedge this guard exists to prevent: the byte check runs ahead
        // of `tx.try_send`, so a counter sitting at the ceiling would
        // report `Full` where tokio would say `Closed`. `Closed` is the
        // only reconnect trigger on a bonded output — `peer_is_gone()` is
        // hardwired `false` for an `SrtGroup` — so masking it drops every
        // packet forever with no reconnect. Simulate the wedged counter
        // directly rather than relying on a leak the Drop impl now fixes.
        let (tx, rx) = send_queue();
        drop(rx);
        tx.queued_bytes
            .store(SRT_SEND_QUEUE_MAX_BYTES, Ordering::Relaxed);
        match tx.try_send(item(1316)) {
            Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(_)) => {
                panic!("a dead consumer must never be reported as transient fullness")
            }
            Ok(()) => panic!("a closed channel must not accept"),
        }
    }

    // ── SMPTE 2022-7 output legs (edge#181) ──────────────────────────

    #[test]
    fn the_other_leg_is_reported_by_what_it_carries() {
        // Queued, or shed on backpressure: a full queue is a live leg.
        assert_eq!(redundant_other_leg_state(Some(true), true), "carrying");
        // Its send task is gone.
        assert_eq!(redundant_other_leg_state(Some(false), true), "down");
        // No socket yet and its connect still pending. The pair is kept up
        // for it, but it carries nothing, so it must not read as carrying.
        assert_eq!(redundant_other_leg_state(None, false), "connecting");
        // No socket, and its connect has given up.
        assert_eq!(redundant_other_leg_state(None, true), "down");
    }

    #[test]
    fn a_leg_2_whose_connect_gave_up_is_gone() {
        // Queued, or shed on backpressure: alive. Send task gone: not.
        assert!(redundant_leg2_alive(Some(true), true));
        assert!(!redundant_leg2_alive(Some(false), true));
        // No socket, connect still pending: kept for its budget, so the
        // pair is not torn down under it.
        assert!(redundant_leg2_alive(None, false));
        // No socket, connect given up: gone, so a lost leg 1 reconnects
        // the pair. It counted as alive, and the output stayed dark.
        assert!(!redundant_leg2_alive(None, true));
        // The teardown and the leg-lost event agree on every input.
        for queued in [Some(true), Some(false), None] {
            for connect_done in [false, true] {
                assert_eq!(
                    redundant_leg2_alive(queued, connect_done),
                    redundant_other_leg_state(queued, connect_done) != "down",
                    "queued {queued:?}, connect done {connect_done}"
                );
            }
        }
    }

    /// A 2022-7 SRT output with both legs listening on loopback, fed a
    /// 1316-byte null-TS item every 2 ms, and the events it raised.
    struct PairHarness {
        stats: Arc<OutputStatsAccumulator>,
        events_rx: tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        events: Vec<crate::manager::events::Event>,
        published: Arc<AtomicU64>,
        ports: (u16, u16),
        cancel: CancellationToken,
        output: Option<JoinHandle<()>>,
    }

    fn event_leg(event: &crate::manager::events::Event) -> Option<u64> {
        event.details.as_ref().and_then(|d| d["leg"].as_u64())
    }

    impl PairHarness {
        fn start() -> Self {
            Self::start_with(None)
        }

        /// Like [`PairHarness::start`], with leg 2 given `budget` to connect
        /// in each cycle, so a test reaches its give-up without waiting
        /// out the 15 s. It runs the redundant loop directly, which is
        /// what `spawn_srt_output` runs for a 2022-7 output.
        fn start_with_leg2_budget(budget: Duration) -> Self {
            Self::start_with(Some(budget))
        }

        fn start_with(leg2_budget: Option<Duration>) -> Self {
            let free_port = || {
                std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
            };
            let ports = (free_port(), free_port());
            // A latency well above loopback scheduling jitter on a loaded
            // test host, so libsrt drops nothing of its own.
            let config: SrtOutputConfig = serde_json::from_value(serde_json::json!({
                "id": "o1", "name": "o1", "mode": "listener",
                "local_addr": format!("127.0.0.1:{}", ports.0), "latency_ms": 200,
                "redundancy": {
                    "mode": "listener", "local_addr": format!("127.0.0.1:{}", ports.1),
                    "latency_ms": 200
                }
            }))
            .expect("SRT output fixture");
            let stats = Arc::new(OutputStatsAccumulator::new("o1".into(), "o1".into(), "srt".into()));
            let (tx, _rx) = broadcast::channel(4096);
            let (event_sender, events_rx) = crate::manager::events::event_channel();
            let cancel = CancellationToken::new();
            let active_input_rx = tokio::sync::watch::channel(String::new()).1;
            let output = match leg2_budget {
                None => spawn_srt_output(
                    config,
                    &tx,
                    stats.clone(),
                    cancel.child_token(),
                    SrtOutputCtx {
                        event_sender,
                        flow_id: "f1".into(),
                        input_format: None,
                        compressed_audio_input: false,
                        frame_rate_rx: None,
                        active_input_rx,
                    },
                ),
                Some(budget) => {
                    let (tx, stats, cancel) = (tx.clone(), stats.clone(), cancel.child_token());
                    tokio::spawn(async move {
                        srt_output_redundant_loop(
                            &config,
                            &tx,
                            stats,
                            cancel,
                            &event_sender,
                            "f1",
                            None,
                            active_input_rx,
                            budget,
                        )
                        .await
                        .expect("the 2022-7 output runs");
                    })
                }
            };
            let published = Arc::new(AtomicU64::new(0));
            let (count, stop) = (published.clone(), cancel.clone());
            tokio::spawn(async move {
                let mut ts = Vec::with_capacity(1316);
                for _ in 0..7 {
                    ts.extend_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
                    ts.extend_from_slice(&[0xFF; 184]);
                }
                let data = Bytes::from(ts);
                let mut tick = tokio::time::interval(Duration::from_millis(2));
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tick.tick() => {
                            // Counted before it is offered, so what the
                            // output sent can never run ahead of it.
                            count.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(RtpPacket {
                                data: data.clone(),
                                sequence_number: 0,
                                rtp_timestamp: 0,
                                recv_time_us: now_us(),
                                is_raw_ts: true,
                                upstream_seq: None,
                                upstream_leg_id: None,
                                sender_timestamp_us: None,
                            });
                        }
                    }
                }
            });
            Self {
                stats,
                events_rx,
                events: Vec::new(),
                published,
                ports,
                cancel,
                output: Some(output),
            }
        }

        /// A receiver calling `port`, drained by a task counting what it
        /// receives.
        async fn connect(&self, port: u16) -> (Arc<SrtSocket>, Arc<AtomicU64>) {
            let config: crate::config::models::SrtInputConfig =
                serde_json::from_value(serde_json::json!({
                    "mode": "caller", "remote_addr": format!("127.0.0.1:{port}"),
                    "latency_ms": 200
                }))
                .expect("SRT caller fixture");
            let socket = crate::srt::connection::connect_srt_input(&config, &self.cancel)
                .await
                .expect("caller connects");
            let received = Arc::new(AtomicU64::new(0));
            let (drain, count) = (socket.clone(), received.clone());
            tokio::spawn(async move {
                while drain.recv().await.is_ok() {
                    count.fetch_add(1, Ordering::Relaxed);
                }
            });
            (socket, received)
        }

        fn pump_events(&mut self) {
            while let Ok(event) = self.events_rx.try_recv() {
                self.events.push(event);
            }
        }

        async fn wait_until(&mut self, what: &str, cond: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                self.pump_events();
                if cond(self) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        fn leg_lost(&self, leg: u64) -> Vec<&crate::manager::events::Event> {
            self.events
                .iter()
                .filter(|e| e.category == category::REDUNDANCY && event_leg(e) == Some(leg))
                .collect()
        }

        fn connected(&self, leg: u64) -> Option<&crate::manager::events::Event> {
            self.events.iter().find(|e| {
                e.category == category::SRT
                    && e.severity == EventSeverity::Info
                    && event_leg(e) == Some(leg)
            })
        }

        /// How many times `leg` has connected: once per connect cycle.
        fn connects(&self, leg: u64) -> usize {
            self.events
                .iter()
                .filter(|e| {
                    e.category == category::SRT
                        && e.severity == EventSeverity::Info
                        && event_leg(e) == Some(leg)
                })
                .count()
        }

        /// Leg 2's "did not connect" Warnings: one per cycle it gave up in.
        fn leg_2_gave_up(&self) -> Vec<&crate::manager::events::Event> {
            self.events
                .iter()
                .filter(|e| e.message.contains("2022-7 leg 2 did not connect within"))
                .collect()
        }

        fn disconnected(&self) -> bool {
            self.events.iter().any(|e| e.message.ends_with("disconnected"))
        }

        fn both_legs_report(&self) -> bool {
            let snap = self.stats.snapshot();
            snap.srt_stats.is_some() && snap.srt_leg2_stats.is_some()
        }

        /// Stop the output and wait for it to return.
        async fn stop(&mut self) {
            self.cancel.cancel();
            if let Some(output) = self.output.take() {
                tokio::time::timeout(Duration::from_secs(10), output)
                    .await
                    .expect("the output stops")
                    .expect("the output task does not panic");
            }
        }
    }

    impl Drop for PairHarness {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    fn assert_leg_lost(event: &crate::manager::events::Event, leg: u64, other_leg: &str) {
        assert_eq!(event.severity, EventSeverity::Warning);
        assert_eq!(event.output_id.as_deref(), Some("o1"));
        assert_eq!(event.flow_id, None, "an output event");
        assert_eq!(
            event.message,
            format!("SRT output 'o1' 2022-7 leg {leg} lost; down until the pair reconnects"),
            "fixed per leg, whatever the other leg is doing"
        );
        let details = event.details.as_ref().expect("details");
        assert_eq!(details["error_code"], "redundancy_output_leg_lost");
        assert_eq!(details["mode"], "Listener");
        assert_eq!(details["other_leg"], other_leg);
    }

    /// Both legs report their own SRT stats; leg 2 announces itself in a
    /// row of its own; the pair counts each item once; and when leg 1 is
    /// lost the output says so, drops leg 1's stats, and goes on counting
    /// what leg 2 carries, so it does not read idle. Before edge#181 a
    /// 2022-7 output published no SRT stats for either leg, raised nothing
    /// until both legs were gone, and read idle on leg 2 alone.
    #[tokio::test]
    async fn a_2022_7_output_reports_both_legs_and_survives_leg_1_loss() {
        let mut h = PairHarness::start();
        let (leg1, _received1) = h.connect(h.ports.0).await;
        let (_leg2, received2) = h.connect(h.ports.1).await;

        h.wait_until("both legs' stats", PairHarness::both_legs_report).await;
        h.wait_until("leg 2's connected event", |h| h.connected(2).is_some()).await;
        let (leg1_up, leg2_up) = (h.connected(1).unwrap(), h.connected(2).unwrap());
        assert_eq!(leg1_up.message, "SRT output 'o1' connected");
        assert_eq!(leg2_up.message, "SRT output 'o1' 2022-7 leg 2 connected");
        assert_eq!(leg2_up.output_id.as_deref(), Some("o1"));

        // Both legs up: leg 1 alone counts, so the pair counts each item
        // once, not twice.
        let (published0, sent0) =
            (h.published.load(Ordering::Relaxed), h.stats.packets_sent.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_secs(1)).await;
        let published = h.published.load(Ordering::Relaxed) - published0;
        let sent = h.stats.packets_sent.load(Ordering::Relaxed) - sent0;
        assert!(
            sent * 2 > published && sent * 4 < published * 5,
            "both legs up, the pair counts each item once: {sent} sent for {published} published"
        );

        leg1.close().await.unwrap();
        h.wait_until("leg 1's loss", |h| !h.leg_lost(1).is_empty()).await;
        assert_leg_lost(h.leg_lost(1)[0], 1, "carrying");
        h.wait_until("leg 1's stats to clear", |h| h.stats.snapshot().srt_stats.is_none()).await;
        assert!(h.stats.snapshot().srt_leg2_stats.is_some(), "leg 2 still reports");

        // A rate window that opens only once leg 1 is gone: the snapshot
        // samples the byte count at most once a second, so let a second
        // pass, sample, and read the rate over the next window.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let before = h.stats.snapshot();
        let received_before = received2.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let after = h.stats.snapshot();
        assert_eq!(after.state, "active", "the output carried on leg 2 is not idle");
        assert!(after.bitrate_bps > 0);
        assert!(after.packets_sent > before.packets_sent, "leg 2 now counts what it sends");
        assert!(received2.load(Ordering::Relaxed) > received_before, "leg 2 still carries");

        h.pump_events();
        assert!(!h.disconnected(), "leg 2 carries, so the pair is not torn down");
        assert!(h.leg_lost(2).is_empty());
        h.stop().await;
    }

    /// Losing leg 2 raises its own event once, drops leg 2's stats, and
    /// leaves leg 1 carrying and counting.
    #[tokio::test]
    async fn a_2022_7_output_reports_leg_2_loss_and_keeps_leg_1() {
        let mut h = PairHarness::start();
        let (_leg1, received1) = h.connect(h.ports.0).await;
        let (leg2, _received2) = h.connect(h.ports.1).await;
        h.wait_until("both legs' stats", PairHarness::both_legs_report).await;

        leg2.close().await.unwrap();
        h.wait_until("leg 2's loss", |h| !h.leg_lost(2).is_empty()).await;
        assert_leg_lost(h.leg_lost(2)[0], 2, "carrying");
        h.wait_until("leg 2's stats to clear", |h| h.stats.snapshot().srt_leg2_stats.is_none())
            .await;
        assert!(h.stats.snapshot().srt_stats.is_some(), "leg 1 still reports");

        let (received_before, sent_before) =
            (received1.load(Ordering::Relaxed), h.stats.packets_sent.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(received1.load(Ordering::Relaxed) > received_before, "leg 1 still carries");
        assert!(h.stats.packets_sent.load(Ordering::Relaxed) > sent_before);
        assert_eq!(h.stats.snapshot().state, "active");

        h.pump_events();
        assert_eq!(h.leg_lost(2).len(), 1, "a lost leg is raised once per cycle");
        assert!(h.leg_lost(1).is_empty());
        assert!(!h.disconnected());
        h.stop().await;
    }

    /// A stop is no lost leg: stopping the output with both legs up raises
    /// no leg-lost event, and clears both legs' stats.
    ///
    /// What this pins is narrow. The publisher stops on the same token as
    /// the output, so leg 2's queue is about empty at the stop and its task
    /// leaves on an empty queue, not on a send error: it guards the forward
    /// loop raising nothing at a stop, not a backlogged leg 2 draining into
    /// a closed socket. The caches clear because both pollers are children
    /// of the output's token; the explicit poller cancels at the end of a
    /// connect cycle are not reached here.
    #[tokio::test]
    async fn a_2022_7_output_stopping_raises_no_leg_loss_and_clears_both_legs() {
        let mut h = PairHarness::start();
        let (_leg1, _received1) = h.connect(h.ports.0).await;
        let (_leg2, _received2) = h.connect(h.ports.1).await;
        h.wait_until("both legs' stats", PairHarness::both_legs_report).await;

        h.stop().await;
        h.wait_until("both legs' stats to clear", |h| {
            let snap = h.stats.snapshot();
            snap.srt_stats.is_none() && snap.srt_leg2_stats.is_none()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.pump_events();
        assert!(
            h.events.iter().all(|e| e.category != category::REDUNDANCY),
            "a stop raised a leg-lost event: {:?}",
            h.events.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    /// Leg 1 lost while leg 2 is still connecting: the event says leg 2 is
    /// connecting, not carrying; the pair waits for it rather than tearing
    /// down; and when leg 2 attaches it counts what it sends from the start.
    #[tokio::test]
    async fn a_2022_7_output_losing_leg_1_while_leg_2_connects_waits_and_hands_over() {
        let mut h = PairHarness::start();
        let (leg1, _received1) = h.connect(h.ports.0).await;
        h.wait_until("leg 1's stats", |h| h.stats.snapshot().srt_stats.is_some()).await;

        leg1.close().await.unwrap();
        h.wait_until("leg 1's loss", |h| !h.leg_lost(1).is_empty()).await;
        assert_leg_lost(h.leg_lost(1)[0], 1, "connecting");
        h.wait_until("leg 1's stats to clear", |h| h.stats.snapshot().srt_stats.is_none()).await;

        // Well inside leg 2's 15 s connect budget.
        let sent_before = h.stats.packets_sent.load(Ordering::Relaxed);
        let (_leg2, received2) = h.connect(h.ports.1).await;
        h.wait_until("leg 2's stats", |h| h.stats.snapshot().srt_leg2_stats.is_some()).await;
        h.wait_until("leg 2 to count what it sends", |h| {
            h.stats.packets_sent.load(Ordering::Relaxed) > sent_before
        })
        .await;
        h.wait_until("leg 2 to carry", |_| received2.load(Ordering::Relaxed) > 0).await;

        h.pump_events();
        assert!(!h.disconnected(), "the pair waited for leg 2 rather than tearing down");
        assert_eq!(h.leg_lost(1).len(), 1);
        h.stop().await;
    }

    /// Leg 2's connect budget in the give-up tests: long enough for a
    /// loopback caller to connect inside it and for leg 1's loss to be seen
    /// inside it, short enough not to wait out the 15 s.
    const GIVE_UP_BUDGET: Duration = Duration::from_secs(4);

    fn assert_leg_2_gave_up(event: &crate::manager::events::Event) {
        assert_eq!(event.severity, EventSeverity::Warning);
        assert_eq!(event.output_id.as_deref(), Some("o1"));
        let details = event.details.as_ref().expect("details");
        assert_eq!(details["leg"], 2);
        assert_eq!(details["reason"], "accept timed out");
        assert_eq!(details["budget_ms"], GIVE_UP_BUDGET.as_millis() as u64);
    }

    /// Leg 2 gives up, leg 1 carries on alone, then leg 1 is lost: the
    /// event says leg 2 is down, and the pair reconnects, with leg 1
    /// accepted again and leg 2 given a fresh budget it now connects in.
    /// Before the fix a leg 2 whose connect had given up counted as alive,
    /// so the pair never tore down and the output was dark until restarted.
    #[tokio::test]
    async fn a_2022_7_output_whose_leg_2_gave_up_reconnects_when_leg_1_is_lost() {
        let mut h = PairHarness::start_with_leg2_budget(GIVE_UP_BUDGET);
        let (leg1, received1) = h.connect(h.ports.0).await;
        h.wait_until("leg 2 to give up", |h| !h.leg_2_gave_up().is_empty()).await;
        assert_leg_2_gave_up(h.leg_2_gave_up()[0]);

        // Giving up alone tears nothing down: leg 1 carries on.
        let received_before = received1.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(received1.load(Ordering::Relaxed) > received_before, "leg 1 carries on alone");
        h.pump_events();
        assert!(!h.disconnected(), "leg 1 still carries, so the pair stays up");
        assert!(h.leg_lost(1).is_empty() && h.leg_lost(2).is_empty());

        leg1.close().await.unwrap();
        h.wait_until("leg 1's loss", |h| !h.leg_lost(1).is_empty()).await;
        assert_leg_lost(h.leg_lost(1)[0], 1, "down");
        h.wait_until("the pair to reconnect", PairHarness::disconnected).await;

        // The next cycle: leg 1 is accepted again, and leg 2 tried again.
        let (_leg1, received1) = h.connect(h.ports.0).await;
        let (_leg2, received2) = h.connect(h.ports.1).await;
        h.wait_until("both legs' stats", PairHarness::both_legs_report).await;
        h.wait_until("leg 2 to connect in the new cycle", |h| h.connects(2) == 1).await;
        assert_eq!(h.connects(1), 2, "leg 1 connects once per cycle");
        h.wait_until("both legs to carry", |_| {
            received1.load(Ordering::Relaxed) > 0 && received2.load(Ordering::Relaxed) > 0
        })
        .await;
        assert!(h.leg_lost(2).is_empty(), "a leg 2 that never connected is never lost");
        h.stop().await;
    }

    /// Leg 1 lost while leg 2 is still connecting, and leg 2 then gives up:
    /// the pair waits out leg 2's budget, then reconnects. Before the fix
    /// nothing followed the give-up and the output was dark until
    /// restarted.
    #[tokio::test]
    async fn a_2022_7_output_losing_leg_1_before_leg_2_gives_up_reconnects() {
        let mut h = PairHarness::start_with_leg2_budget(GIVE_UP_BUDGET);
        let (leg1, _received1) = h.connect(h.ports.0).await;
        h.wait_until("leg 1's stats", |h| h.stats.snapshot().srt_stats.is_some()).await;

        leg1.close().await.unwrap();
        h.wait_until("leg 1's loss", |h| !h.leg_lost(1).is_empty()).await;
        assert!(h.leg_2_gave_up().is_empty(), "leg 1 was lost inside leg 2's budget");
        assert_leg_lost(h.leg_lost(1)[0], 1, "connecting");
        assert!(!h.disconnected(), "the pair waits out leg 2's budget");

        h.wait_until("leg 2 to give up", |h| !h.leg_2_gave_up().is_empty()).await;
        assert_leg_2_gave_up(h.leg_2_gave_up()[0]);
        h.wait_until("the pair to reconnect", PairHarness::disconnected).await;

        let (_leg1, received1) = h.connect(h.ports.0).await;
        h.wait_until("leg 1 to connect again", |h| h.connects(1) == 2).await;
        h.wait_until("leg 1 to carry again", |_| received1.load(Ordering::Relaxed) > 0).await;
        assert_eq!(h.leg_lost(1).len(), 1, "one loss in the first cycle");
        assert!(h.leg_lost(2).is_empty(), "a leg 2 that never connected is never lost");
        h.stop().await;
    }
}
