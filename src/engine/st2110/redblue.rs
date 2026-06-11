// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

// Phase 1 step 2: foundation. The helpers below are exercised by unit tests
// in this module and will be called by ST 2110 input tasks in step 4.
#![allow(dead_code)]

//! SMPTE 2022-7 dual-network ("Red"/"Blue") bind helpers for ST 2110.
//!
//! ST 2110 deployments typically use two physically disjoint networks for
//! hitless redundancy: a "Red" leg and a "Blue" leg, each carrying the same
//! RTP stream. The receiver opens one socket per leg and de-duplicates
//! packets by RTP sequence number using the existing
//! [`crate::redundancy::merger::HitlessMerger`].
//!
//! This module provides:
//!
//! - [`RedBluePair`]: a paired primary + optional secondary `UdpSocket`,
//!   bound from a primary address and an optional [`RedBlueBindConfig`].
//! - [`RedBluePair::recv_loop`]: an async loop that reads from both legs and
//!   feeds the [`HitlessMerger`], emitting deduped `(payload, leg)` pairs to
//!   a callback.
//! - Per-leg packet counters, exposed via [`LegStats`] for stats integration
//!   in step 5.
//!
//! Phase 1 step 2 ships the helpers and unit tests. Phase 1 step 4 wires
//! these into the actual ST 2110 input tasks.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::config::models::RedBlueBindConfig;
use crate::manager::events::{EventSender, EventSeverity, category};
use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
use crate::util::socket::bind_udp_input;

/// Per-leg packet and byte counters.
///
/// Atomic so they can be updated from the recv loop and read from the stats
/// snapshot path without locks. The recv loop owns the writes; readers see
/// monotonically-increasing values.
#[derive(Debug, Default)]
pub struct LegStats {
    /// Packets received on this leg, including duplicates that the merger
    /// later dropped.
    pub packets_received: AtomicU64,
    /// Bytes received on this leg.
    pub bytes_received: AtomicU64,
    /// Packets that the merger forwarded as the canonical sequence.
    pub packets_forwarded: AtomicU64,
    /// Packets dropped because they were duplicates of a sequence already
    /// forwarded by the other leg.
    pub packets_duplicate: AtomicU64,
}

impl LegStats {
    pub fn snapshot(&self) -> LegStatsSnapshot {
        LegStatsSnapshot {
            packets_received: self.packets_received.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            packets_forwarded: self.packets_forwarded.load(Ordering::Relaxed),
            packets_duplicate: self.packets_duplicate.load(Ordering::Relaxed),
        }
    }
}

/// Plain-data snapshot of [`LegStats`] for serialization.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegStatsSnapshot {
    pub packets_received: u64,
    pub bytes_received: u64,
    pub packets_forwarded: u64,
    pub packets_duplicate: u64,
}

/// Combined statistics for a Red/Blue pair: per-leg counters plus a count of
/// active-leg switches recorded by the merger.
#[derive(Debug, Default)]
pub struct RedBlueStats {
    pub red: LegStats,
    pub blue: LegStats,
    /// Number of times the merger transitioned its `active_leg` from one
    /// physical leg to the other. Indicates flapping or single-leg loss.
    pub leg_switches: AtomicU64,
}

impl RedBlueStats {
    pub fn snapshot(&self) -> RedBlueStatsSnapshot {
        RedBlueStatsSnapshot {
            red: self.red.snapshot(),
            blue: self.blue.snapshot(),
            leg_switches: self.leg_switches.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedBlueStatsSnapshot {
    pub red: LegStatsSnapshot,
    pub blue: LegStatsSnapshot,
    pub leg_switches: u64,
}

/// A Red/Blue paired UDP receiver.
///
/// Holds the two `UdpSocket`s and a shared [`RedBlueStats`]. Construct via
/// [`RedBluePair::bind_input`]; consume via [`RedBluePair::recv_loop`].
pub struct RedBluePair {
    /// Primary ("Red") receive socket. Always present.
    pub red: UdpSocket,
    /// Secondary ("Blue") receive socket. `None` when no `redundancy` was
    /// configured.
    pub blue: Option<UdpSocket>,
    /// Shared per-leg counters.
    pub stats: Arc<RedBlueStats>,
    /// Optional event-emitter context, set via [`Self::with_events`]. The
    /// pair `(sender, flow_id)` is always set together so the runtime
    /// invariant "flow_id is required when emitting" cannot be violated by
    /// constructing one without the other.
    events: Option<LegEventCtx>,
}

/// Bundled event-emitter context for [`RedBluePair`]. Private so the
/// `(sender, flow_id)` invariant lives in one place.
#[derive(Clone)]
struct LegEventCtx {
    sender: EventSender,
    flow_id: String,
}

impl RedBluePair {
    /// Bind a Red/Blue input pair.
    ///
    /// `red_addr` is the primary leg bind address (validated upstream by
    /// `validate_socket_addr`). `red_iface` is the multicast interface for
    /// the primary leg. `red_source` is the optional SSM source for the
    /// primary leg. `blue` is the optional second leg from the parsed config;
    /// the blue leg's SSM source is read from `RedBlueBindConfig::source_addr`.
    /// `red_binding` pins the red leg to a specific NIC; `blue` carries its
    /// own `interface_binding` field — the two are independent so 2022-7 plants
    /// can pin Red and Blue to different NICs.
    pub async fn bind_input(
        red_addr: &str,
        red_iface: Option<&str>,
        red_source: Option<&str>,
        blue: Option<&RedBlueBindConfig>,
        red_binding: Option<&crate::config::models::InterfaceBinding>,
    ) -> Result<Self> {
        let red_sock = bind_udp_input(red_addr, red_iface, red_source, red_binding)
            .await
            .with_context(|| format!("Red leg bind failed for {red_addr}"))?;

        let blue_sock = match blue {
            Some(b) => Some(
                bind_udp_input(&b.addr, b.interface_addr.as_deref(), b.source_addr.as_deref(), b.interface_binding.as_ref())
                    .await
                    .with_context(|| format!("Blue leg bind failed for {}", b.addr))?,
            ),
            None => None,
        };

        Ok(Self {
            red: red_sock,
            blue: blue_sock,
            stats: Arc::new(RedBlueStats::default()),
            events: None,
        })
    }

    /// Attach a manager event sender + flow ID. Builder method so callers
    /// can opt into event reporting after `bind_input`. When set, the
    /// `recv_loop` emits one event per first-packet-on-each-leg and per
    /// active-leg switch (rate-limited by transition).
    pub fn with_events(mut self, event_sender: EventSender, flow_id: String) -> Self {
        self.events = Some(LegEventCtx {
            sender: event_sender,
            flow_id,
        });
        self
    }

    /// Run the dedupe receive loop until cancelled.
    ///
    /// For every datagram received on either leg, the loop:
    ///
    /// 1. Increments per-leg `packets_received` / `bytes_received`.
    /// 2. Extracts the RTP sequence number from the first 4 bytes
    ///    (`u16` at offset 2 in the standard RTP header).
    /// 3. Calls [`HitlessMerger::try_merge`].
    /// 4. On forward, increments `packets_forwarded`, calls `on_packet` with
    ///    the payload `Bytes`, the originating leg, and the sequence number,
    ///    and counts a leg switch if the merger's active leg changed.
    /// 5. On duplicate, increments `packets_duplicate` and drops the packet.
    ///
    /// `on_packet` returns `bool`. When it returns `false`, the loop exits
    /// (used by tests; production callers always return `true`).
    ///
    /// The loop never blocks: each leg is read concurrently via
    /// `tokio::select!`, and slow consumers receive the dropped packet via
    /// the broadcast channel that `on_packet` ultimately writes into.
    pub async fn recv_loop<F>(
        self,
        cancel: CancellationToken,
        mut on_packet: F,
    )
    where
        F: FnMut(Bytes, ActiveLeg, u16) -> bool + Send,
    {
        let mut merger = HitlessMerger::new();
        let mut last_active = ActiveLeg::None;
        let mut red_buf = vec![0u8; MAX_DGRAM];
        let mut blue_buf = vec![0u8; MAX_DGRAM];

        // Per-leg event state. We emit one info event the first time each
        // leg delivers a packet (so operators see "Red leg up" and
        // "Blue leg up" at flow start) and one warning event per leg
        // switch thereafter. Holding state in locals keeps this lock-free.
        let mut red_first_seen = false;
        let mut blue_first_seen = false;
        let dual_leg = self.blue.is_some();
        let events = self.events.clone();
        let event_sender = events.as_ref().map(|c| &c.sender);
        let event_flow_id = events.as_ref().map(|c| c.flow_id.as_str());

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("RedBluePair recv loop cancelled");
                    return;
                }
                res = self.red.recv_from(&mut red_buf) => {
                    match res {
                        Ok((n, _src)) => {
                            self.stats.red.packets_received.fetch_add(1, Ordering::Relaxed);
                            self.stats.red.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
                            if let Some(seq) = parse_rtp_seq(&red_buf[..n]) {
                                // Red is leg 1.
                                if let Some(active) = merger.try_merge(seq, ActiveLeg::Leg1) {
                                    self.stats.red.packets_forwarded.fetch_add(1, Ordering::Relaxed);
                                    if !red_first_seen {
                                        red_first_seen = true;
                                        emit_leg_event(
                                            event_sender,
                                            event_flow_id,
                                            EventSeverity::Info,
                                            "red",
                                            "Red leg up (first packet received)",
                                            dual_leg,
                                        );
                                    }
                                    if last_active != ActiveLeg::None && last_active != active {
                                        self.stats.leg_switches.fetch_add(1, Ordering::Relaxed);
                                        emit_leg_event(
                                            event_sender,
                                            event_flow_id,
                                            EventSeverity::Warning,
                                            "red",
                                            "2022-7 active leg switched to Red",
                                            dual_leg,
                                        );
                                    }
                                    last_active = active;
                                    let payload = Bytes::copy_from_slice(&red_buf[..n]);
                                    if !on_packet(payload, active, seq) {
                                        return;
                                    }
                                } else {
                                    self.stats.red.packets_duplicate.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("RedBluePair red recv error: {e}");
                        }
                    }
                }
                res = recv_blue(&self.blue, &mut blue_buf), if self.blue.is_some() => {
                    match res {
                        Ok((n, _src)) => {
                            self.stats.blue.packets_received.fetch_add(1, Ordering::Relaxed);
                            self.stats.blue.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
                            if let Some(seq) = parse_rtp_seq(&blue_buf[..n]) {
                                // Blue is leg 2.
                                if let Some(active) = merger.try_merge(seq, ActiveLeg::Leg2) {
                                    self.stats.blue.packets_forwarded.fetch_add(1, Ordering::Relaxed);
                                    if !blue_first_seen {
                                        blue_first_seen = true;
                                        emit_leg_event(
                                            event_sender,
                                            event_flow_id,
                                            EventSeverity::Info,
                                            "blue",
                                            "Blue leg up (first packet received)",
                                            dual_leg,
                                        );
                                    }
                                    if last_active != ActiveLeg::None && last_active != active {
                                        self.stats.leg_switches.fetch_add(1, Ordering::Relaxed);
                                        emit_leg_event(
                                            event_sender,
                                            event_flow_id,
                                            EventSeverity::Warning,
                                            "blue",
                                            "2022-7 active leg switched to Blue",
                                            dual_leg,
                                        );
                                    }
                                    last_active = active;
                                    let payload = Bytes::copy_from_slice(&blue_buf[..n]);
                                    if !on_packet(payload, active, seq) {
                                        return;
                                    }
                                } else {
                                    self.stats.blue.packets_duplicate.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("RedBluePair blue recv error: {e}");
                        }
                    }
                }
            }
        }
    }
}

const MAX_DGRAM: usize = 9000 + 216; // jumbo-frame payloads (ST 2110-20 UHD plants run 9000-MTU fabrics; sender validation allows payload_budget ≤ 8952) + RTP/RFC4175 header headroom

impl RedBluePair {
    /// Dedicated-thread receive loop (Linux only) — single or dual leg.
    ///
    /// `recvmmsg(2)` batches up to 64 datagrams per syscall per leg on
    /// non-blocking `std` sockets owned by one `spawn_blocking` thread;
    /// `poll(2)` (100 ms timeout) provides readiness across both legs
    /// plus the cancel-token check while the wire is silent. Required
    /// for ST 2110-20 uncompressed-video packet rates (1080p50 ≈
    /// 146 kpps per leg, 2160p50 ≈ 580 kpps) — a per-packet
    /// `tokio::select!` recv loop tops out around ~34 kpps and the
    /// kernel drops the excess at the socket buffer.
    ///
    /// Dual-leg (2022-7) runs Red and Blue on the SAME thread feeding
    /// one [`HitlessMerger`], so dedup + counter + event semantics are
    /// identical to [`Self::recv_loop`]: per-leg received / forwarded /
    /// duplicate counters (batched per syscall), one "leg up" info
    /// event on each leg's first forwarded packet, and a Warning +
    /// `leg_switches` increment on every active-leg transition. The
    /// merger's 1024-slot gap-fill window dwarfs the 64-packet batch
    /// interleave between legs, so cross-leg loss recovery behaves the
    /// same as the per-packet path.
    ///
    /// `on_packet` receives a borrowed payload — no per-packet `Bytes`
    /// allocation.
    #[cfg(target_os = "linux")]
    pub fn spawn_dedicated_loop<F>(
        self,
        cancel: CancellationToken,
        mut on_packet: F,
    ) -> Result<tokio::task::JoinHandle<()>>
    where
        F: FnMut(&[u8], ActiveLeg, u16) -> bool + Send + 'static,
    {
        let red_std = self
            .red
            .into_std()
            .context("red leg into_std for dedicated ingest loop")?;
        red_std
            .set_nonblocking(true)
            .context("red leg set_nonblocking(true)")?;
        let blue_std = match self.blue {
            Some(b) => {
                let s = b
                    .into_std()
                    .context("blue leg into_std for dedicated ingest loop")?;
                s.set_nonblocking(true)
                    .context("blue leg set_nonblocking(true)")?;
                Some(s)
            }
            None => None,
        };

        let stats = self.stats.clone();
        let events = self.events.clone();
        Ok(tokio::task::spawn_blocking(move || {
            use std::os::fd::AsRawFd;
            const BATCH: usize = 64;
            let red_fd = red_std.as_raw_fd();
            let blue_fd = blue_std.as_ref().map(|s| s.as_raw_fd());
            let dual_leg = blue_fd.is_some();

            // One shared batch buffer set — each leg's batch is fully
            // consumed before the next recvmmsg call reuses it.
            let mut bufs = vec![[0u8; MAX_DGRAM]; BATCH];
            let mut iovecs: Vec<libc::iovec> = bufs
                .iter_mut()
                .map(|b| libc::iovec {
                    iov_base: b.as_mut_ptr() as *mut libc::c_void,
                    iov_len: MAX_DGRAM,
                })
                .collect();
            let mut hdrs: Vec<libc::mmsghdr> = iovecs
                .iter_mut()
                .map(|iov| {
                    let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
                    h.msg_hdr.msg_iov = iov as *mut libc::iovec;
                    h.msg_hdr.msg_iovlen = 1;
                    h
                })
                .collect();

            let mut merger = HitlessMerger::new();
            let mut last_active = ActiveLeg::None;
            let mut red_first_seen = false;
            let mut blue_first_seen = false;
            let event_sender = events.as_ref().map(|c| &c.sender);
            let event_flow_id = events.as_ref().map(|c| c.flow_id.as_str());

            let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(2);
            pollfds.push(libc::pollfd { fd: red_fd, events: libc::POLLIN, revents: 0 });
            if let Some(bfd) = blue_fd {
                pollfds.push(libc::pollfd { fd: bfd, events: libc::POLLIN, revents: 0 });
            }

            tracing::info!(
                "ST 2110 dedicated ingest started (recvmmsg batch {BATCH}, {} leg{})",
                pollfds.len(),
                if dual_leg { "s + hitless merge" } else { "" },
            );
            let mut diag_last = std::time::Instant::now();
            let mut diag_pkts: u64 = 0;
            let mut diag_batches: u64 = 0;
            let mut diag_busy = std::time::Duration::ZERO;
            while !cancel.is_cancelled() {
                for p in pollfds.iter_mut() {
                    p.revents = 0;
                }
                let pr = unsafe {
                    libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 100)
                };
                if pr < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    tracing::warn!("ST 2110 dedicated ingest poll error: {err}");
                    continue;
                }
                if pr == 0 {
                    continue; // timeout — re-check cancel
                }
                for li in 0..pollfds.len() {
                    if pollfds[li].revents & (libc::POLLIN | libc::POLLERR) == 0 {
                        continue;
                    }
                    let fd = pollfds[li].fd;
                    // Index 0 is always Red; index 1 exists only when Blue does.
                    let leg = if li == 0 { ActiveLeg::Leg1 } else { ActiveLeg::Leg2 };
                    let n = unsafe {
                        libc::recvmmsg(
                            fd,
                            hdrs.as_mut_ptr(),
                            BATCH as libc::c_uint,
                            0,
                            std::ptr::null_mut(),
                        )
                    };
                    if n < 0 {
                        let err = std::io::Error::last_os_error();
                        match err.kind() {
                            std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted => continue,
                            _ => {
                                tracing::warn!("ST 2110 dedicated ingest recvmmsg error: {err}");
                                continue;
                            }
                        }
                    }
                    let n = n as usize;
                    let batch_t0 = std::time::Instant::now();
                    let (leg_stats, leg_name, first_seen, up_msg, switch_msg) =
                        if leg == ActiveLeg::Leg1 {
                            (
                                &stats.red,
                                "red",
                                &mut red_first_seen,
                                "Red leg up (first packet received)",
                                "2022-7 active leg switched to Red",
                            )
                        } else {
                            (
                                &stats.blue,
                                "blue",
                                &mut blue_first_seen,
                                "Blue leg up (first packet received)",
                                "2022-7 active leg switched to Blue",
                            )
                        };
                    let mut bytes: u64 = 0;
                    let mut forwarded: u64 = 0;
                    let mut dups: u64 = 0;
                    for i in 0..n {
                        let len = hdrs[i].msg_len as usize;
                        bytes += len as u64;
                        let payload = &bufs[i][..len];
                        let Some(seq) = parse_rtp_seq(payload) else {
                            continue;
                        };
                        if let Some(active) = merger.try_merge(seq, leg) {
                            forwarded += 1;
                            if !*first_seen {
                                *first_seen = true;
                                emit_leg_event(
                                    event_sender,
                                    event_flow_id,
                                    EventSeverity::Info,
                                    leg_name,
                                    up_msg,
                                    dual_leg,
                                );
                            }
                            if last_active != ActiveLeg::None && last_active != active {
                                stats.leg_switches.fetch_add(1, Ordering::Relaxed);
                                emit_leg_event(
                                    event_sender,
                                    event_flow_id,
                                    EventSeverity::Warning,
                                    leg_name,
                                    switch_msg,
                                    dual_leg,
                                );
                            }
                            last_active = active;
                            if !on_packet(payload, active, seq) {
                                // Flush the partial batch's counters before
                                // exiting so received/forwarded stay honest.
                                leg_stats
                                    .packets_received
                                    .fetch_add((i + 1) as u64, Ordering::Relaxed);
                                leg_stats.bytes_received.fetch_add(bytes, Ordering::Relaxed);
                                leg_stats
                                    .packets_forwarded
                                    .fetch_add(forwarded, Ordering::Relaxed);
                                if dups > 0 {
                                    leg_stats
                                        .packets_duplicate
                                        .fetch_add(dups, Ordering::Relaxed);
                                }
                                return;
                            }
                        } else {
                            dups += 1;
                        }
                    }
                    leg_stats.packets_received.fetch_add(n as u64, Ordering::Relaxed);
                    leg_stats.bytes_received.fetch_add(bytes, Ordering::Relaxed);
                    if forwarded > 0 {
                        leg_stats.packets_forwarded.fetch_add(forwarded, Ordering::Relaxed);
                    }
                    if dups > 0 {
                        leg_stats.packets_duplicate.fetch_add(dups, Ordering::Relaxed);
                    }
                    diag_pkts += n as u64;
                    diag_batches += 1;
                    diag_busy += batch_t0.elapsed();
                }
                if diag_last.elapsed() >= std::time::Duration::from_secs(5) {
                    let el = diag_last.elapsed().as_secs_f64();
                    let red = stats.red.snapshot();
                    let blue = stats.blue.snapshot();
                    tracing::info!(
                        "st2110 ingest diag: {:.0} pps  {:.0} batch/s  avg_batch={:.1}  consume_busy={:.1}%  red_rx={}  blue_rx={}  dups={}",
                        diag_pkts as f64 / el,
                        diag_batches as f64 / el,
                        diag_pkts as f64 / diag_batches.max(1) as f64,
                        100.0 * diag_busy.as_secs_f64() / el,
                        red.packets_received,
                        blue.packets_received,
                        red.packets_duplicate + blue.packets_duplicate,
                    );
                    diag_last = std::time::Instant::now();
                    diag_pkts = 0;
                    diag_batches = 0;
                    diag_busy = std::time::Duration::ZERO;
                }
            }
            tracing::debug!("ST 2110 dedicated ingest loop cancelled");
        }))
    }
}

/// Dispatch a `network_leg` event when an event sender is configured.
///
/// The flow ID is required for routing in the manager UI; if it is missing
/// the event is sent without a flow scope. `dual_leg` is included in the
/// details so the manager can distinguish "single Red leg" warnings from
/// real 2022-7 leg switches.
fn emit_leg_event(
    sender: Option<&EventSender>,
    flow_id: Option<&str>,
    severity: EventSeverity,
    leg: &str,
    message: &str,
    dual_leg: bool,
) {
    let Some(tx) = sender else {
        return;
    };
    let details = serde_json::json!({
        "leg": leg,
        "dual_leg": dual_leg,
    });
    tx.emit_with_details(severity, category::NETWORK_LEG, message, flow_id, details);
}

/// Helper that returns a never-ready future when there is no Blue socket,
/// so the `tokio::select!` arm above is gated cleanly by `if self.blue.is_some()`.
async fn recv_blue(
    sock: &Option<UdpSocket>,
    buf: &mut [u8],
) -> std::io::Result<(usize, std::net::SocketAddr)> {
    match sock {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

/// Pull the RTP sequence number out of an RTP packet's header.
///
/// Returns `None` for non-RTP datagrams (too short, or version != 2). The
/// merger uses RTP sequence numbers to dedupe, so non-RTP traffic on these
/// sockets is silently ignored.
fn parse_rtp_seq(buf: &[u8]) -> Option<u16> {
    if buf.len() < 4 {
        return None;
    }
    // Version is the top two bits of byte 0.
    if (buf[0] >> 6) != 2 {
        return None;
    }
    Some(u16::from_be_bytes([buf[2], buf[3]]))
}

// ─────────────── ActiveLeg ↔ Red/Blue aliases ───────────────
//
// `Red` and `Blue` are the broadcast-domain names for what
// `redundancy::merger::ActiveLeg` calls `Leg1` and `Leg2`. The merger crate
// is shared with SRT/RTP redundancy so we can't rename its variants without
// touching the rest of the codebase. Instead, we expose two module-level
// constants. ST 2110 call sites can use `RED` / `BLUE` to read naturally
// while still passing the underlying `ActiveLeg`.

/// "Red" network leg — alias for [`ActiveLeg::Leg1`].
pub const RED: ActiveLeg = ActiveLeg::Leg1;
/// "Blue" network leg — alias for [`ActiveLeg::Leg2`].
pub const BLUE: ActiveLeg = ActiveLeg::Leg2;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    #[test]
    fn test_parse_rtp_seq_valid() {
        // Minimal RTP header: V=2, PT=96, seq=0x1234, ts=0, ssrc=0
        let buf = [0x80, 0x60, 0x12, 0x34];
        assert_eq!(parse_rtp_seq(&buf), Some(0x1234));
    }

    #[test]
    fn test_parse_rtp_seq_wrong_version() {
        let buf = [0x40, 0x60, 0x12, 0x34]; // V=1
        assert!(parse_rtp_seq(&buf).is_none());
    }

    #[test]
    fn test_parse_rtp_seq_too_short() {
        assert!(parse_rtp_seq(&[]).is_none());
        assert!(parse_rtp_seq(&[0x80, 0x60]).is_none());
    }

    /// Build a minimal RTP packet with the given sequence number.
    fn rtp_pkt(seq: u16) -> Vec<u8> {
        let mut p = vec![0x80, 0x60];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        p.extend_from_slice(&0u32.to_be_bytes()); // ssrc
        p.extend_from_slice(b"hello");
        p
    }

    async fn ephemeral_pair() -> (UdpSocket, UdpSocket, std::net::SocketAddr, std::net::SocketAddr) {
        let red_recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let blue_recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let red_addr = red_recv.local_addr().unwrap();
        let blue_addr = blue_recv.local_addr().unwrap();
        (red_recv, blue_recv, red_addr, blue_addr)
    }

    #[tokio::test]
    async fn test_red_only_recv_forwards_all() {
        let (red_recv, _blue_unused, red_addr, _) = ephemeral_pair().await;
        let pair = RedBluePair {
            red: red_recv,
            blue: None,
            stats: Arc::new(RedBlueStats::default()),
            events: None,
        };
        let stats = pair.stats.clone();
        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();

        let received = Arc::new(parking_lot_lite::Mutex::new(Vec::<u16>::new()));
        let received_clone = received.clone();
        let task = tokio::spawn(async move {
            pair.recv_loop(cancel_inner, |_payload, _leg, seq| {
                let mut v = received_clone.lock();
                v.push(seq);
                v.len() < 3
            })
            .await;
        });

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for seq in [10u16, 11, 12] {
            sender.send_to(&rtp_pkt(seq), red_addr).await.unwrap();
        }
        // Wait for the loop to consume.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap();
        cancel.cancel();

        let mut v = received.lock().clone();
        v.sort_unstable();
        assert_eq!(v, vec![10, 11, 12]);
        assert_eq!(stats.red.packets_received.load(Ordering::Relaxed), 3);
        assert_eq!(stats.red.packets_forwarded.load(Ordering::Relaxed), 3);
        assert_eq!(stats.red.packets_duplicate.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_red_blue_dedup_drops_duplicates() {
        let (red_recv, blue_recv, red_addr, blue_addr) = ephemeral_pair().await;
        let pair = RedBluePair {
            red: red_recv,
            blue: Some(blue_recv),
            stats: Arc::new(RedBlueStats::default()),
            events: None,
        };
        let stats = pair.stats.clone();
        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();

        let received = Arc::new(parking_lot_lite::Mutex::new(Vec::<u16>::new()));
        let received_clone = received.clone();
        // Loop until cancelled — assert totals after the recv has had a
        // chance to drain. Per-leg counts depend on tokio scheduling and
        // are not part of the contract; the contract is that each unique
        // sequence number is forwarded exactly once and that the total
        // (forwarded + duplicate) equals the total received.
        let task = tokio::spawn(async move {
            pair.recv_loop(cancel_inner, |_payload, _leg, seq| {
                received_clone.lock().push(seq);
                true
            })
            .await;
        });

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Same sequences on both legs; merger should forward each exactly once.
        for seq in [100u16, 101, 102] {
            sender.send_to(&rtp_pkt(seq), red_addr).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            sender.send_to(&rtp_pkt(seq), blue_addr).await.unwrap();
        }
        // Let the recv loop drain.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap();

        // Each unique seq is forwarded exactly once; the order matches the
        // order in which the merger first saw the seq. Sequences are sent
        // in ascending order so the forwarded list must be [100, 101, 102].
        let mut v = received.lock().clone();
        v.sort_unstable();
        assert_eq!(v, vec![100, 101, 102]);

        // Both legs received 3 packets each (6 total).
        let total_received = stats.red.packets_received.load(Ordering::Relaxed)
            + stats.blue.packets_received.load(Ordering::Relaxed);
        assert_eq!(total_received, 6);
        // Only 3 were forwarded; the other 3 were duplicates.
        let total_forwarded = stats.red.packets_forwarded.load(Ordering::Relaxed)
            + stats.blue.packets_forwarded.load(Ordering::Relaxed);
        let total_dupe = stats.red.packets_duplicate.load(Ordering::Relaxed)
            + stats.blue.packets_duplicate.load(Ordering::Relaxed);
        assert_eq!(total_forwarded, 3);
        assert_eq!(total_dupe, 3);
    }

    /// Dedicated single-leg recvmmsg loop forwards every unique packet
    /// (parity with the recv_loop contract).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_dedicated_single_leg_forwards_all() {
        let (red_recv, _blue_unused, red_addr, _) = ephemeral_pair().await;
        let pair = RedBluePair {
            red: red_recv,
            blue: None,
            stats: Arc::new(RedBlueStats::default()),
            events: None,
        };
        let stats = pair.stats.clone();
        let cancel = CancellationToken::new();

        let received = Arc::new(parking_lot_lite::Mutex::new(Vec::<u16>::new()));
        let received_clone = received.clone();
        let handle = pair
            .spawn_dedicated_loop(cancel.clone(), move |_payload, _leg, seq| {
                let mut v = received_clone.lock();
                v.push(seq);
                v.len() < 3
            })
            .expect("spawn dedicated loop");

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for seq in [10u16, 11, 12] {
            sender.send_to(&rtp_pkt(seq), red_addr).await.unwrap();
        }
        // on_packet returns false after the 3rd forward — the thread exits.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .unwrap();
        cancel.cancel();

        let mut v = received.lock().clone();
        v.sort_unstable();
        assert_eq!(v, vec![10, 11, 12]);
        assert_eq!(stats.red.packets_received.load(Ordering::Relaxed), 3);
        assert_eq!(stats.red.packets_forwarded.load(Ordering::Relaxed), 3);
        assert_eq!(stats.red.packets_duplicate.load(Ordering::Relaxed), 0);
    }

    /// Dedicated dual-leg recvmmsg loop dedups across Red + Blue with the
    /// same totals contract as the select-based recv_loop: each unique
    /// sequence forwarded exactly once, forwarded + duplicate == received.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_dedicated_dual_leg_dedup_drops_duplicates() {
        let (red_recv, blue_recv, red_addr, blue_addr) = ephemeral_pair().await;
        let pair = RedBluePair {
            red: red_recv,
            blue: Some(blue_recv),
            stats: Arc::new(RedBlueStats::default()),
            events: None,
        };
        let stats = pair.stats.clone();
        let cancel = CancellationToken::new();

        let received = Arc::new(parking_lot_lite::Mutex::new(Vec::<u16>::new()));
        let received_clone = received.clone();
        let handle = pair
            .spawn_dedicated_loop(cancel.clone(), move |_payload, _leg, seq| {
                received_clone.lock().push(seq);
                true
            })
            .expect("spawn dedicated loop");

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Same sequences on both legs; the shared merger must forward each
        // exactly once. Alternate which leg leads so both legs forward at
        // least one packet (exercises per-leg counters + gap-fill).
        for seq in [100u16, 101, 102, 103] {
            let (first, second) = if seq % 2 == 0 {
                (red_addr, blue_addr)
            } else {
                (blue_addr, red_addr)
            };
            sender.send_to(&rtp_pkt(seq), first).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            sender.send_to(&rtp_pkt(seq), second).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Let the loop drain, then cancel (poll timeout is 100 ms).
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .unwrap();

        let mut v = received.lock().clone();
        v.sort_unstable();
        assert_eq!(v, vec![100, 101, 102, 103]);

        let total_received = stats.red.packets_received.load(Ordering::Relaxed)
            + stats.blue.packets_received.load(Ordering::Relaxed);
        assert_eq!(total_received, 8);
        let total_forwarded = stats.red.packets_forwarded.load(Ordering::Relaxed)
            + stats.blue.packets_forwarded.load(Ordering::Relaxed);
        let total_dupe = stats.red.packets_duplicate.load(Ordering::Relaxed)
            + stats.blue.packets_duplicate.load(Ordering::Relaxed);
        assert_eq!(total_forwarded, 4);
        assert_eq!(total_dupe, 4);
        // Alternating lead leg means both legs forwarded at least once.
        assert!(stats.red.packets_forwarded.load(Ordering::Relaxed) >= 1);
        assert!(stats.blue.packets_forwarded.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn test_bind_input_no_blue_returns_pair() {
        // Bind to an ephemeral port via the same helper used by inputs.
        let pair = RedBluePair::bind_input("127.0.0.1:0", None, None, None, None)
            .await
            .expect("bind red only");
        assert!(pair.blue.is_none());
    }

    #[tokio::test]
    async fn test_bind_input_with_blue_distinct_ports() {
        let red_port = pick_free_port().await;
        let blue_port = pick_free_port().await;
        let red_addr = format!("127.0.0.1:{red_port}");
        let blue = RedBlueBindConfig {
            addr: format!("127.0.0.1:{blue_port}"),
            interface_addr: None,
            source_addr: None,
            interface_binding: None,
        };
        let pair = RedBluePair::bind_input(&red_addr, None, None, Some(&blue), None)
            .await
            .expect("bind red + blue");
        assert!(pair.blue.is_some());
    }

    async fn pick_free_port() -> u16 {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.local_addr().unwrap().port()
    }

    /// Tiny mutex used only by tests above so we don't depend on `parking_lot`.
    /// Lives in this module so it stays out of the production binary.
    mod parking_lot_lite {
        use std::sync::{Mutex as StdMutex, MutexGuard};

        #[derive(Debug)]
        pub struct Mutex<T>(StdMutex<T>);
        impl<T> Mutex<T> {
            pub fn new(t: T) -> Self {
                Self(StdMutex::new(t))
            }
            pub fn lock(&self) -> MutexGuard<'_, T> {
                self.0.lock().unwrap()
            }
        }
    }
}
