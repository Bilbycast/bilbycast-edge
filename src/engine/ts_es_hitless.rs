// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pre-bus Hitless merger for PID-bus `SlotSource::Hitless`.
//!
//! The merger sits between the [`NodeEsBus`] and the assembler: it
//! subscribes to two source bus keys (a `primary` and a `backup`) and
//! republishes onto a third synthetic key that the assembler's slot
//! points at. The assembler itself stays PID-level — zero changes to
//! `run_assembler`.
//!
//! # Merge strategies
//!
//! [`spawn_hitless_es_merger_full`] runs one of three per slot:
//!
//! - **Primary preference** (the default): primary packets are forwarded
//!   verbatim. If no primary packet arrives for `stall_ms` (default
//!   200 ms) the merger flips to the backup, and back to the primary on its
//!   next packet.
//! - **Seq-aware** (`mode: seq_aware`): SMPTE 2022-7 merging on
//!   [`EsPacket::upstream_seq`] — the sequence number of the datagram that
//!   carried the packet, which RTP and RIST inputs stamp, and the packet's
//!   index among that datagram's packets of its PID, which
//!   [`super::ts_es_bus::TsEsDemuxer`] counts. A datagram carries seven TS
//!   packets of a video PID, all with its sequence number, so the merger
//!   merges each datagram's run packet by packet
//!   ([`HitlessRunMerger`](crate::redundancy::merger::HitlessRunMerger)):
//!   each forwarded once and in order within its run, from either leg, and
//!   a run one leg lost or cut short completed from the other.
//! - **Buffered seq-aware** (`path_differential_ms` set): the same merge
//!   behind a hold of that long, released in `(seq, index)` order at
//!   constant latency.
//!
//! A leg with no sequence number — SRT, UDP — gives a seq-aware merger
//! nothing to merge on: it forwards such a primary verbatim and discards
//! such a backup.
//!
//! # Invariants
//!
//! - No blocking on the data path. The merger task is a plain
//!   `tokio::select!` loop feeding a bus `broadcast::Sender`.
//! - Bus semantics: if downstream consumers lag, `broadcast` drops at
//!   the bus edge — same as every other bus publisher.
//! - Cancellation propagates from the parent flow token via
//!   `CancellationToken::child_token`.
//! - One merger per Hitless slot: [`HitlessMergers`] keeps a slot's merger
//!   across a plan swap that leaves the slot alone and stops it before
//!   anything else publishes onto the slot's bus key.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};
use tokio_util::sync::CancellationToken;

use super::ts_assembler::{AssemblyPlan, PendingHitlessSlot};
use super::ts_es_bus::{NodeEsBus, UpstreamSeq};

/// Default stall timer: if no primary packet arrives within this
/// window the merger flips to backup. Tight enough that standby cuts
/// over within one video frame at 25 fps (40 ms per frame × 5 frames)
/// but loose enough that bursty ingress jitter doesn't flap.
pub const DEFAULT_STALL_MS: u64 = 200;

/// Namespace for synthetic bus keys used by Hitless slots. The merger
/// publishes onto [`hitless_bus_key`] of its flow and slot. The
/// assembler's slot points at the same key, so the assembler sees a
/// single stream and stays PID-level.
pub const HITLESS_INPUT_PREFIX: &str = "hitless:";

/// Whether a Hitless slot's merger is forwarding its primary (`true`) or has
/// failed over to its backup (`false`). One per slot, shared by the merger,
/// which sets it once per flip, and the slot's
/// [`crate::engine::ts_assembler::HitlessLegs`], through which the flow's SRT
/// stats pollers read it.
///
/// Runtime state, not plan: any two compare equal, so a plan swap that keeps
/// a slot's legs leaves its program's PMT version alone.
#[derive(Debug, Clone)]
pub struct ForwardingPrimary(Arc<AtomicBool>);

/// Every merger starts on its primary.
impl Default for ForwardingPrimary {
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
}

impl ForwardingPrimary {
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, primary: bool) {
        self.0.store(primary, Ordering::Relaxed);
    }
}

impl PartialEq for ForwardingPrimary {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for ForwardingPrimary {}

/// Build the synthetic bus key `(input_id, source_pid)` for the Hitless
/// slot `uid` of flow `flow_id`: `("hitless:{flow_id}:{uid}", 0)`.
///
/// The bus is node-wide — every flow shares the [`NodeEsBus`] its
/// `FlowManager` owns, and `sender_for` hands a second publisher the
/// existing channel — so the key names the flow as well as the slot. Keyed
/// on the slot alone, two assembled flows with a Hitless slot at the same
/// position published onto one key and each assembler played both flows'
/// packets. `uid` is unique per Hitless slot within the flow
/// (`"slot_{program_idx}_{slot_idx}"`) and holds no `:`, so the last `:`
/// separates the two and no two flows' slots share a key.
pub fn hitless_bus_key(flow_id: &str, uid: &str) -> (String, u16) {
    (format!("{HITLESS_INPUT_PREFIX}{flow_id}:{uid}"), 0)
}

/// The mergers running behind one assembled flow's Hitless slots, by slot
/// uid. A slot's merger is the only publisher onto the slot's bus key, so
/// the flow keeps exactly one per slot: [`Self::apply`] brings the set in
/// line with each plan before it goes on air.
#[derive(Default)]
pub struct HitlessMergers(HashMap<String, RunningMerger>);

struct RunningMerger {
    slot: PendingHitlessSlot,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl HitlessMergers {
    /// Bring the running mergers in line with `slots`, the Hitless slots of
    /// `plan`, before `plan` goes on air.
    ///
    /// A slot the plan leaves as it was keeps its merger, uninterrupted,
    /// and `plan` takes that merger's [`ForwardingPrimary`] in place of the
    /// fresh one it was built with, so the on-air set goes on following the
    /// merger's failovers. A merger whose slot is gone or changed is
    /// cancelled and awaited, so it is no longer publishing onto the slot's
    /// bus key when its replacement starts there. Every slot without a
    /// merger then gets one, under `parent`.
    pub async fn apply(
        &mut self,
        slots: &[PendingHitlessSlot],
        plan: &mut AssemblyPlan,
        bus: &Arc<NodeEsBus>,
        parent: &CancellationToken,
    ) {
        let mut kept = HashMap::with_capacity(slots.len());
        for (uid, running) in std::mem::take(&mut self.0) {
            // `ForwardingPrimary` compares equal whatever it holds, so this
            // compares the slot's legs and settings only.
            if slots.contains(&running.slot) {
                kept.insert(uid, running);
            } else {
                running.cancel.cancel();
                let _ = running.task.await;
            }
        }
        for slot in slots {
            if let Some(running) = kept.get(&slot.uid) {
                for planned in plan.programs.iter_mut().flat_map(|p| p.slots.iter_mut()) {
                    if planned.source == slot.bus_key
                        && let Some(legs) = planned.hitless_legs.as_mut()
                    {
                        legs.forwarding_primary = running.slot.forwarding_primary.clone();
                    }
                }
                continue;
            }
            let cancel = parent.child_token();
            let task = spawn_hitless_es_merger_full(
                slot.bus_key.clone(),
                slot.primary.clone(),
                slot.backup.clone(),
                bus.clone(),
                Duration::from_millis(slot.stall_ms),
                slot.seq_aware,
                slot.reorder_window,
                slot.path_differential_ms,
                slot.forwarding_primary.clone(),
                cancel.clone(),
            );
            kept.insert(slot.uid.clone(), RunningMerger { slot: slot.clone(), cancel, task });
        }
        self.0 = kept;
    }
}

/// Spawn a Hitless merger task. The seq-aware mode now supports a
/// path-differential buffer for true SMPTE 2022-7 compliance — set
/// `path_differential_ms` to `Some(ms)` to enable.
///
/// * `seq_aware = false`        → primary-preference stall timer (legacy)
/// * `seq_aware = true, pd None`  → bare dedup (faster, no asym-path protection)
/// * `seq_aware = true, pd Some(ms)` → buffered SMPTE 2022-7 (recommended)
///
/// `reorder_window` is the seq-aware mergers' gap-fill window in sequence
/// numbers (see [`crate::redundancy::merger::HitlessMerger::with_window`]);
/// the primary-preference merger merges on no sequence number and has none,
/// which is why validation refuses the field on its slots.
///
/// The merger publishes onto `output`, the slot's [`hitless_bus_key`], and
/// names it in its logs.
///
/// The primary-preference merger sets `forwarding_primary` on each flip, so
/// the flow counts the SRT drops of the leg it forwards. The seq-aware
/// mergers leave it on the primary: they merge on `upstream_seq`, which no
/// SRT input stamps, so of SRT legs they forward the primary verbatim and
/// discard the backup.
pub fn spawn_hitless_es_merger_full(
    output: (String, u16),
    primary: (String, u16),
    backup: (String, u16),
    bus: Arc<NodeEsBus>,
    stall: Duration,
    seq_aware: bool,
    reorder_window: u16,
    path_differential_ms: Option<u32>,
    forwarding_primary: ForwardingPrimary,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut primary_rx = bus.subscribe(&primary.0, primary.1);
    let mut backup_rx = bus.subscribe(&backup.0, backup.1);
    let (out_input, out_pid) = output;
    // `stream_type = 0` (unknown) → bus channel sizes at the video-class
    // default (8192). Hitless mergers don't know the essence type up
    // front and we'd rather over-allocate by a few KB than truncate a
    // real video leg.
    let out_tx = bus.sender_for(&out_input, out_pid, 0);
    let uid = out_input;

    tokio::spawn(async move {
        match (seq_aware, path_differential_ms) {
            (true, Some(ms)) => {
                run_buffered(
                    uid,
                    primary_rx,
                    backup_rx,
                    out_tx,
                    Duration::from_millis(ms as u64),
                    reorder_window,
                    cancel,
                )
                .await;
            }
            (true, None) => {
                run_seq_aware(uid, primary_rx, backup_rx, out_tx, reorder_window, cancel).await;
            }
            (false, _) => {
                run_primary_preference(
                    uid,
                    &mut primary_rx,
                    &mut backup_rx,
                    out_tx,
                    stall,
                    &forwarding_primary,
                    cancel,
                )
                .await;
            }
        }
    })
}

async fn run_buffered(
    uid: String,
    mut primary_rx: broadcast::Receiver<EsPacketAlias>,
    mut backup_rx: broadcast::Receiver<EsPacketAlias>,
    out_tx: broadcast::Sender<EsPacketAlias>,
    max_path_diff: Duration,
    reorder_window: u16,
    cancel: CancellationToken,
) {
    use crate::redundancy::merger::{ActiveLeg, BufferedHitlessMerger, HeldRuns};
    let mut merger: BufferedHitlessMerger<EsPacketAlias, HeldRuns> =
        BufferedHitlessMerger::with_runs(max_path_diff, reorder_window);
    // Far-future placeholder used when the buffer is empty so the
    // tokio::select! `sleep_until` arm doesn't fire spuriously.
    let far_future = std::time::Instant::now() + Duration::from_secs(86_400);

    loop {
        let drain_at = merger.next_deadline().unwrap_or(far_future);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = sleep_until(tokio::time::Instant::from_std(drain_at)) => {
                for (_, es) in merger.drain_expired(std::time::Instant::now()) {
                    let _ = out_tx.send(es);
                }
            }
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        tracing::warn!(
                            uid = %uid,
                            "pid_bus hitless buffered: primary leg has no upstream_seq — \
                             forwarding without buffer (bypass path)"
                        );
                        let _ = out_tx.send(es);
                        continue;
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest_run(seq, index, ActiveLeg::Leg1, es, now) {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        continue;
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest_run(seq, index, ActiveLeg::Leg2, es, now) {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

async fn run_primary_preference(
    uid: String,
    primary_rx: &mut broadcast::Receiver<EsPacketAlias>,
    backup_rx: &mut broadcast::Receiver<EsPacketAlias>,
    out_tx: broadcast::Sender<EsPacketAlias>,
    stall: Duration,
    forwarding_primary: &ForwardingPrimary,
    cancel: CancellationToken,
) {
    let mut using_primary = true;
    let mut last_primary_at = Instant::now();

    loop {
        let stall_deadline = last_primary_at + stall;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    last_primary_at = Instant::now();
                    if !using_primary {
                        tracing::info!(
                            uid = %uid,
                            "pid_bus hitless: primary resumed — switching from backup"
                        );
                        using_primary = true;
                        forwarding_primary.set(true);
                    }
                    let _ = out_tx.send(es);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    if !using_primary {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = sleep_until(stall_deadline), if using_primary => {
                tracing::warn!(
                    uid = %uid,
                    "pid_bus hitless: primary stalled {:?} — switching to backup",
                    stall
                );
                using_primary = false;
                forwarding_primary.set(false);
            }
        }
    }
}

async fn run_seq_aware(
    uid: String,
    mut primary_rx: broadcast::Receiver<EsPacketAlias>,
    mut backup_rx: broadcast::Receiver<EsPacketAlias>,
    out_tx: broadcast::Sender<EsPacketAlias>,
    reorder_window: u16,
    cancel: CancellationToken,
) {
    use crate::redundancy::merger::{ActiveLeg, HitlessRunMerger};
    let mut merger = HitlessRunMerger::with_window(reorder_window);
    let mut leg_log = LegLog::default();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        tracing::warn!(
                            uid = %uid,
                            "pid_bus hitless seq_aware: primary leg has no upstream_seq — \
                             falling back to forward (this should be rejected at config validation)"
                        );
                        let _ = out_tx.send(es);
                        continue;
                    };
                    if let Some(chosen) = merger.try_merge(seq, index, ActiveLeg::Leg1, es.recv_time_us) {
                        let _ = leg_log.forwarded(&uid, chosen);
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        continue;
                    };
                    if let Some(chosen) = merger.try_merge(seq, index, ActiveLeg::Leg2, es.recv_time_us) {
                        let _ = leg_log.forwarded(&uid, chosen);
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Packets a leg must forward in a row before a seq-aware merger logs that
/// it has taken over. A run split between the legs, or a gap filled, moves
/// the forwarding leg for a packet or two, which can happen thousands of
/// times a second on a video PID; only a leg that stays is worth a line.
const LEG_LOG_SUSTAIN: u32 = 256;

/// Logs which leg a seq-aware merger forwards, once a change has lasted
/// [`LEG_LOG_SUSTAIN`] packets. A compare and an add per packet.
#[derive(Default)]
struct LegLog {
    /// The leg last logged as forwarding.
    logged: Option<crate::redundancy::merger::ActiveLeg>,
    /// Packets in a row forwarded from another leg than `logged`.
    run: u32,
}

impl LegLog {
    /// A packet forwarded from `leg`; returns whether that was logged.
    #[inline]
    fn forwarded(&mut self, uid: &str, leg: crate::redundancy::merger::ActiveLeg) -> bool {
        if Some(leg) == self.logged {
            self.run = 0;
            return false;
        }
        self.run += 1;
        if self.run < LEG_LOG_SUSTAIN && self.logged.is_some() {
            return false;
        }
        tracing::info!(uid = %uid, "pid_bus hitless seq_aware: leg = {:?}", leg);
        self.logged = Some(leg);
        self.run = 0;
        true
    }
}

// Type alias used in helpers above to keep the function signatures
// readable without a large lifetime ceremony — `EsPacket` is `Clone` so
// the broadcast channel of it is fine to pass through by value.
type EsPacketAlias = crate::engine::ts_es_bus::EsPacket;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ts_es_bus::EsPacket;
    use bytes::Bytes;

    fn es(pid: u16, byte: u8) -> EsPacket {
        let mut payload = vec![0u8; 188];
        payload[0] = 0x47;
        payload[1] = ((pid >> 8) & 0x1F) as u8;
        payload[2] = (pid & 0xFF) as u8;
        payload[3] = 0x10;
        for b in &mut payload[4..] {
            *b = byte;
        }
        EsPacket {
            source_pid: pid,
            stream_type: 0,
            payload: Bytes::from(payload),
            is_pusi: false,
            has_pcr: false,
            pcr: None,
            recv_time_us: 0,
            upstream_seq: None,
        }
    }

    #[tokio::test]
    async fn primary_preference_when_primary_live() {
        let bus = Arc::new(NodeEsBus::new());
        let cancel = CancellationToken::new();
        let forwarding = ForwardingPrimary::default();
        let _handle = spawn_hitless_es_merger_full(
            hitless_bus_key("f1", "slot_0_0"),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x200),
            bus.clone(),
            Duration::from_millis(500),
            /* seq_aware */ false,
            crate::redundancy::merger::REORDER_WINDOW,
            /* path_differential_ms */ None,
            forwarding.clone(),
            cancel.clone(),
        );

        let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
        let pri = bus.sender_for("in-a", 0x100, 0x1B);
        let bak = bus.sender_for("in-b", 0x200, 0x03);

        // Give the merger time to subscribe.
        tokio::time::sleep(Duration::from_millis(20)).await;

        pri.send(es(0x100, 0xAA)).unwrap();
        bak.send(es(0x200, 0xBB)).unwrap();

        let first = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .expect("merger emits within 200ms")
            .expect("channel open");
        assert_eq!(first.payload[4], 0xAA, "primary packet must be forwarded");

        // Backup packet must NOT be forwarded while primary is live.
        let second = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(second.is_err(), "backup must be suppressed while primary is live");
        assert!(forwarding.get(), "the primary stays on air");

        cancel.cancel();
    }

    #[tokio::test]
    async fn failover_to_backup_on_stall() {
        let bus = Arc::new(NodeEsBus::new());
        let cancel = CancellationToken::new();
        let forwarding = ForwardingPrimary::default();
        let _handle = spawn_hitless_es_merger_full(
            hitless_bus_key("f1", "slot_0_0"),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x200),
            bus.clone(),
            Duration::from_millis(50),
            /* seq_aware */ false,
            crate::redundancy::merger::REORDER_WINDOW,
            /* path_differential_ms */ None,
            forwarding.clone(),
            cancel.clone(),
        );

        let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
        let pri = bus.sender_for("in-a", 0x100, 0x1B);
        let bak = bus.sender_for("in-b", 0x200, 0x03);
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Prime primary so the merger anchors.
        pri.send(es(0x100, 0xAA)).unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;

        // Stall primary past 50ms, backup should take over.
        tokio::time::sleep(Duration::from_millis(120)).await;
        bak.send(es(0x200, 0xBB)).unwrap();
        let after = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .expect("merger emits backup packet after stall")
            .expect("channel open");
        assert_eq!(after.payload[4], 0xBB, "backup must forward after primary stall");
        assert!(!forwarding.get(), "the failover puts the backup on air");

        cancel.cancel();
    }

    #[tokio::test]
    async fn switches_back_to_primary_on_resume() {
        let bus = Arc::new(NodeEsBus::new());
        let cancel = CancellationToken::new();
        let forwarding = ForwardingPrimary::default();
        let _handle = spawn_hitless_es_merger_full(
            hitless_bus_key("f1", "slot_0_0"),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x200),
            bus.clone(),
            Duration::from_millis(50),
            /* seq_aware */ false,
            crate::redundancy::merger::REORDER_WINDOW,
            /* path_differential_ms */ None,
            forwarding.clone(),
            cancel.clone(),
        );

        let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
        let pri = bus.sender_for("in-a", 0x100, 0x1B);
        let bak = bus.sender_for("in-b", 0x200, 0x03);
        tokio::time::sleep(Duration::from_millis(20)).await;

        pri.send(es(0x100, 0xAA)).unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;

        // Stall → backup takes over.
        tokio::time::sleep(Duration::from_millis(120)).await;
        bak.send(es(0x200, 0xBB)).unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await;
        assert!(!forwarding.get());

        // Primary resumes; next primary packet must surface and the
        // subsequent backup packet must stop flowing.
        pri.send(es(0x100, 0xCC)).unwrap();
        let pc = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
            .await
            .expect("primary resume")
            .expect("channel open");
        assert_eq!(pc.payload[4], 0xCC);
        assert!(forwarding.get(), "the resumed primary is back on air");

        bak.send(es(0x200, 0xDD)).unwrap();
        let blocked = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
        assert!(
            blocked.is_err(),
            "backup must be re-suppressed once primary resumes"
        );

        cancel.cancel();
    }

    /// A seq-aware merger fills a gap from the backup only while the gap is
    /// inside its configured `reorder_window`, buffered or not.
    #[tokio::test]
    async fn seq_aware_merges_within_the_configured_reorder_window() {
        let seq_es = |pid: u16, seq: u16| EsPacket {
            upstream_seq: Some(UpstreamSeq { seq, index: 0 }),
            ..es(pid, 0)
        };
        for path_differential_ms in [None, Some(500)] {
            for (reorder_window, fills) in [(64u16, false), (128, true)] {
                let bus = Arc::new(NodeEsBus::new());
                let cancel = CancellationToken::new();
                let _handle = spawn_hitless_es_merger_full(
                    hitless_bus_key("f1", "slot_0_0"),
                    ("in-a".to_string(), 0x100),
                    ("in-b".to_string(), 0x200),
                    bus.clone(),
                    Duration::from_millis(50),
                    /* seq_aware */ true,
                    reorder_window,
                    path_differential_ms,
                    ForwardingPrimary::default(),
                    cancel.clone(),
                );
                let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
                let pri = bus.sender_for("in-a", 0x100, 0x1B);
                let bak = bus.sender_for("in-b", 0x200, 0x1B);
                // The primary loses 20; the backup's copy comes 80 seqs late.
                for seq in (0..=100u16).filter(|&s| s != 20) {
                    pri.send(seq_es(0x100, seq)).unwrap();
                }
                bak.send(seq_es(0x200, 20)).unwrap();

                let quiet = Duration::from_millis(if path_differential_ms.is_some() { 900 } else { 200 });
                let mut forwarded = Vec::new();
                while let Ok(Ok(es)) = tokio::time::timeout(quiet, out_rx.recv()).await {
                    forwarded.push(es.upstream_seq.map(|u| u.seq));
                }
                assert_eq!(
                    forwarded.contains(&Some(20)),
                    fills,
                    "{path_differential_ms:?} window {reorder_window}: {} forwarded",
                    forwarded.len()
                );
                assert_eq!(forwarded.len(), 100 + usize::from(fills));
                cancel.cancel();
            }
        }
    }

    /// The forwarding leg is logged once at the start and again only when a
    /// change lasts: a run split between the legs, or a gap filled, moves it
    /// for a packet at a time, thousands of times a second on a video PID.
    #[test]
    fn a_leg_change_is_logged_only_once_it_lasts() {
        use crate::redundancy::merger::ActiveLeg;
        let mut log = LegLog::default();
        assert!(log.forwarded("u", ActiveLeg::Leg1));
        let mut lines = 0;
        for i in 0..10_000 {
            let leg = if i % 3 == 0 { ActiveLeg::Leg2 } else { ActiveLeg::Leg1 };
            lines += usize::from(log.forwarded("u", leg));
        }
        assert_eq!(lines, 0, "a leg that keeps coming back is no change");
        let logged = (0..LEG_LOG_SUSTAIN).filter(|_| log.forwarded("u", ActiveLeg::Leg2)).count();
        assert_eq!(logged, 1, "a change that lasts is logged once");
    }

    /// SCTE-35 on a PID-bus Hitless slot: one TS packet a cue, a few seconds
    /// apart, so consecutive cues' datagram seqs can be half the sequence
    /// space or more apart, which reads as "too far behind". Nothing was
    /// accepted between them, so each cue re-anchors the window and is
    /// forwarded once, bare or buffered; cues closer together than the stale
    /// time are re-anchored by the backup's copy agreeing with the
    /// primary's. (A buffered merger reads its own clock rather than the
    /// packets' arrival stamps, so here, its packets sent milliseconds apart,
    /// the backup's copy re-anchors it; its stale time is covered in
    /// `redundancy::merger`.) Half the cues used to be dropped: lost ad cues.
    #[tokio::test]
    async fn seq_aware_forwards_every_cue_of_a_sparse_pid() {
        let cue = |pid: u16, seq: u16, at_us: u64| EsPacket {
            upstream_seq: Some(UpstreamSeq { seq, index: 0 }),
            recv_time_us: at_us,
            ..es(pid, 0)
        };
        // Seq steps between cues: four in the too-far-behind band, one a
        // forward jump of a window or more, one an ordinary step.
        let steps: [u16; 7] = [40_000, 35_000, 20_000, 50_000, 33_000, 900, 61_000];
        for (gap_us, path_differential_ms) in
            [(3_000_000u64, None), (3_000_000, Some(50)), (500_000, None), (500_000, Some(50))]
        {
            let bus = Arc::new(NodeEsBus::new());
            let cancel = CancellationToken::new();
            let _handle = spawn_hitless_es_merger_full(
                hitless_bus_key("f1", "slot_0_0"),
                ("in-a".to_string(), 0x1F0),
                ("in-b".to_string(), 0x1F0),
                bus.clone(),
                Duration::from_millis(50),
                /* seq_aware */ true,
                crate::redundancy::merger::REORDER_WINDOW,
                path_differential_ms,
                ForwardingPrimary::default(),
                cancel.clone(),
            );
            let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
            let pri = bus.sender_for("in-a", 0x1F0, 0x86);
            let bak = bus.sender_for("in-b", 0x1F0, 0x86);
            tokio::time::sleep(Duration::from_millis(20)).await;

            let mut seq: u16 = 100;
            let mut at_us = 1_000_000u64;
            let mut cues = vec![seq];
            for step in steps {
                seq = seq.wrapping_add(step);
                cues.push(seq);
            }
            for &seq in &cues {
                pri.send(cue(0x1F0, seq, at_us)).unwrap();
                bak.send(cue(0x1F0, seq, at_us + 30_000)).unwrap();
                // In order through the merger task, primary before backup.
                tokio::time::sleep(Duration::from_millis(2)).await;
                at_us += gap_us;
            }
            let quiet = Duration::from_millis(if path_differential_ms.is_some() { 300 } else { 100 });
            let mut forwarded = Vec::new();
            while let Ok(Ok(es)) = tokio::time::timeout(quiet, out_rx.recv()).await {
                forwarded.push(es.upstream_seq.map(|u| u.seq).unwrap());
            }
            assert_eq!(
                forwarded, cues,
                "cues {gap_us} us apart, path differential {path_differential_ms:?}"
            );
            cancel.cancel();
        }
    }

    /// SRT legs carry no `upstream_seq`, so a seq-aware merger forwards the
    /// primary verbatim and discards the backup, stalled primary or not: the
    /// primary is the leg on air.
    #[tokio::test]
    async fn seq_aware_keeps_an_srt_primary_on_air() {
        for path_differential_ms in [None, Some(50)] {
            let bus = Arc::new(NodeEsBus::new());
            let cancel = CancellationToken::new();
            let forwarding = ForwardingPrimary::default();
            let _handle = spawn_hitless_es_merger_full(
                hitless_bus_key("f1", "slot_0_0"),
                ("in-a".to_string(), 0x100),
                ("in-b".to_string(), 0x200),
                bus.clone(),
                Duration::from_millis(50),
                /* seq_aware */ true,
                crate::redundancy::merger::REORDER_WINDOW,
                path_differential_ms,
                forwarding.clone(),
                cancel.clone(),
            );
            let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
            let pri = bus.sender_for("in-a", 0x100, 0x1B);
            let bak = bus.sender_for("in-b", 0x200, 0x03);

            pri.send(es(0x100, 0xAA)).unwrap();
            let first = tokio::time::timeout(Duration::from_millis(200), out_rx.recv())
                .await
                .expect("the primary is forwarded")
                .expect("channel open");
            assert_eq!(first.payload[4], 0xAA);

            // Past any stall a primary-preference merger would act on.
            tokio::time::sleep(Duration::from_millis(120)).await;
            bak.send(es(0x200, 0xBB)).unwrap();
            let backup = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await;
            assert!(backup.is_err(), "{path_differential_ms:?}: the backup is discarded");
            assert!(forwarding.get(), "{path_differential_ms:?}: the primary stays on air");

            cancel.cancel();
        }
    }

    /// TS packets a 1316-byte datagram carries.
    const RUN: usize = 7;

    /// Datagram `seq` of a feed carrying nothing but video PID `pid`: `RUN`
    /// TS packets, packet `i` tagged `(seq, i)` in its payload so the merged
    /// output names each packet it forwards.
    fn datagram(pid: u16, seq: u16) -> crate::engine::packet::RtpPacket {
        let mut data = Vec::with_capacity(RUN * 188);
        for i in 0..RUN {
            let mut ts = [0u8; 188];
            ts[0] = 0x47;
            ts[1] = ((pid >> 8) & 0x1F) as u8;
            ts[2] = (pid & 0xFF) as u8;
            ts[3] = 0x10 | ((usize::from(seq) * RUN + i) & 0x0F) as u8;
            ts[4..6].copy_from_slice(&seq.to_be_bytes());
            ts[6] = i as u8;
            data.extend_from_slice(&ts);
        }
        crate::engine::packet::RtpPacket {
            data: Bytes::from(data),
            sequence_number: seq,
            rtp_timestamp: 0,
            recv_time_us: 0,
            is_raw_ts: true,
            upstream_seq: Some(seq),
            upstream_leg_id: None,
            sender_timestamp_us: None,
        }
    }

    /// The `(seq, index)` tag [`datagram`] wrote into a packet.
    fn tag(es: &EsPacket) -> (u16, u8) {
        (
            u16::from_be_bytes([es.payload[4], es.payload[5]]),
            es.payload[6],
        )
    }

    /// Every packet of datagrams `seqs`, in order.
    fn every_packet(seqs: impl IntoIterator<Item = u16>) -> Vec<(u16, u8)> {
        seqs.into_iter()
            .flat_map(|s| (0..RUN as u8).map(move |i| (s, i)))
            .collect()
    }

    /// A seq-aware merger over two RTP inputs' demuxers, each publishing
    /// video PID 0x100. Feeds `leg1` and `leg2`'s datagrams through the
    /// demuxers, which is where a datagram's packets get its sequence
    /// number, and returns the merged output once it goes quiet.
    async fn merge_demuxed(
        path_differential_ms: Option<u32>,
        leg1: &[u16],
        leg2: &[u16],
    ) -> Vec<(u16, u8)> {
        use crate::engine::ts_es_bus::TsEsDemuxer;
        let bus = Arc::new(NodeEsBus::new());
        let cancel = CancellationToken::new();
        let _handle = spawn_hitless_es_merger_full(
            hitless_bus_key("f1", "slot_0_0"),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x100),
            bus.clone(),
            Duration::from_millis(50),
            /* seq_aware */ true,
            crate::redundancy::merger::REORDER_WINDOW,
            path_differential_ms,
            ForwardingPrimary::default(),
            cancel.clone(),
        );
        let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
        let mut demux_a = TsEsDemuxer::new("in-a", bus.clone());
        let mut demux_b = TsEsDemuxer::new("in-b", bus.clone());
        for &seq in leg1 {
            demux_a.process(&datagram(0x100, seq));
        }
        for &seq in leg2 {
            demux_b.process(&datagram(0x100, seq));
        }
        let quiet = Duration::from_millis(if path_differential_ms.is_some() {
            300
        } else {
            100
        });
        let mut forwarded = Vec::new();
        while let Ok(Ok(es)) = tokio::time::timeout(quiet, out_rx.recv()).await {
            forwarded.push(tag(&es));
        }
        cancel.cancel();
        forwarded
    }

    /// Both legs deliver every datagram: the merged video PID carries every
    /// TS packet once, in order — not just the first of each datagram's
    /// seven, which share its sequence number.
    #[tokio::test]
    async fn seq_aware_forwards_every_packet_of_a_datagram() {
        let seqs: Vec<u16> = (0..20).collect();
        for path_differential_ms in [None, Some(50)] {
            let forwarded = merge_demuxed(path_differential_ms, &seqs, &seqs).await;
            assert_eq!(forwarded, every_packet(0..20), "{path_differential_ms:?}");
        }
    }

    /// The primary loses three datagrams and the backup fills them, whole:
    /// still every packet once. The buffered merger releases them in order;
    /// the bare one forwards a fill as it arrives, so only each datagram's
    /// own packets are in order.
    #[tokio::test]
    async fn seq_aware_fills_a_lost_datagram_from_the_other_leg() {
        let lossy: Vec<u16> = (0..20).filter(|s| ![3, 7, 8].contains(s)).collect();
        let whole: Vec<u16> = (0..20).collect();
        for path_differential_ms in [None, Some(50)] {
            let forwarded = merge_demuxed(path_differential_ms, &lossy, &whole).await;
            if path_differential_ms.is_some() {
                assert_eq!(forwarded, every_packet(0..20));
            } else {
                let mut sorted = forwarded.clone();
                sorted.sort();
                assert_eq!(sorted, every_packet(0..20), "every packet once");
                for seq in [3, 7, 8] {
                    let run: Vec<u8> = forwarded
                        .iter()
                        .filter(|(s, _)| *s == seq)
                        .map(|&(_, i)| i)
                        .collect();
                    assert_eq!(
                        run,
                        (0..RUN as u8).collect::<Vec<_>>(),
                        "datagram {seq} in order"
                    );
                }
            }
        }
    }

    /// Packet `index` of datagram `seq`'s run on PID 0x100, tagged as
    /// [`datagram`] tags it, as the demuxer publishes it.
    fn run_packet(seq: u16, index: u16) -> EsPacket {
        let at = usize::from(index) * 188;
        EsPacket {
            stream_type: 0x1B,
            payload: Bytes::copy_from_slice(&datagram(0x100, seq).data[at..at + 188]),
            upstream_seq: Some(UpstreamSeq { seq, index }),
            ..es(0x100, 0)
        }
    }

    /// Leg 2 delivers part of a datagram's run before leg 1 delivers any of
    /// it: the run is forwarded once and in order, part from each leg.
    #[tokio::test]
    async fn seq_aware_merges_a_run_part_from_each_leg() {
        for path_differential_ms in [None, Some(50)] {
            let bus = Arc::new(NodeEsBus::new());
            let cancel = CancellationToken::new();
            let _handle = spawn_hitless_es_merger_full(
                hitless_bus_key("f1", "slot_0_0"),
                ("in-a".to_string(), 0x100),
                ("in-b".to_string(), 0x100),
                bus.clone(),
                Duration::from_millis(50),
                /* seq_aware */ true,
                crate::redundancy::merger::REORDER_WINDOW,
                path_differential_ms,
                ForwardingPrimary::default(),
                cancel.clone(),
            );
            let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
            let pri = bus.sender_for("in-a", 0x100, 0x1B);
            let bak = bus.sender_for("in-b", 0x100, 0x1B);
            let settle = || tokio::time::sleep(Duration::from_millis(20));

            for index in 0..3 {
                bak.send(run_packet(10, index)).unwrap();
            }
            settle().await;
            for index in 0..RUN as u16 {
                pri.send(run_packet(10, index)).unwrap();
            }
            settle().await;
            for index in 3..RUN as u16 {
                bak.send(run_packet(10, index)).unwrap();
            }
            for seq in 11..13 {
                for index in 0..RUN as u16 {
                    pri.send(run_packet(seq, index)).unwrap();
                    bak.send(run_packet(seq, index)).unwrap();
                }
            }

            let mut forwarded = Vec::new();
            while let Ok(Ok(es)) =
                tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await
            {
                forwarded.push(tag(&es));
            }
            assert_eq!(forwarded, every_packet(10..13), "{path_differential_ms:?}");
            cancel.cancel();
        }
    }

    /// The primary's channel lags in the middle of a datagram's run: the
    /// merger forwards none of that run past the hole, the backup's copy
    /// fills it whole, and later datagrams merge as before — the lag
    /// wedges nothing.
    #[tokio::test]
    async fn seq_aware_survives_a_leg_lagging_mid_run() {
        use crate::engine::ts_es_bus::TsEsDemuxer;
        let bus = Arc::new(NodeEsBus::new());
        let cancel = CancellationToken::new();
        let _handle = spawn_hitless_es_merger_full(
            hitless_bus_key("f1", "slot_0_0"),
            ("in-a".to_string(), 0x100),
            ("in-b".to_string(), 0x100),
            bus.clone(),
            Duration::from_millis(50),
            /* seq_aware */ true,
            4096,
            /* path_differential_ms */ None,
            ForwardingPrimary::default(),
            cancel.clone(),
        );
        let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
        let mut demux_a = TsEsDemuxer::new("in-a", bus.clone());
        let mut demux_b = TsEsDemuxer::new("in-b", bus.clone());
        // 1172 datagrams overrun the 8192-packet channel by 12 packets
        // before the merger reads it: datagram 0 and five of datagram 1's
        // seven are gone, so the primary resumes at packet 5 of datagram 1.
        for seq in 0..1172 {
            demux_a.process(&datagram(0x100, seq));
        }
        let mut forwarded = Vec::new();
        let mut drain = async |forwarded: &mut Vec<(u16, u8)>| {
            while let Ok(res) =
                tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await
            {
                forwarded.push(tag(&res.expect("the test keeps up with the merger")));
            }
        };
        drain(&mut forwarded).await;
        assert_eq!(forwarded, every_packet(2..1172));
        forwarded.clear();

        demux_b.process(&datagram(0x100, 1));
        drain(&mut forwarded).await;
        assert_eq!(
            forwarded,
            every_packet(1..2),
            "the backup fills the holed run"
        );
        forwarded.clear();

        for seq in 1172..1175 {
            demux_a.process(&datagram(0x100, seq));
            demux_b.process(&datagram(0x100, seq));
        }
        drain(&mut forwarded).await;
        assert_eq!(forwarded, every_packet(1172..1175));
        cancel.cancel();
    }
}
