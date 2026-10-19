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
//! # Dedup strategy — Phase 7 first-light
//!
//! Primary-preference failover with a configurable stall timer.
//! Primary packets are forwarded verbatim. If no primary packet arrives
//! for `stall_ms` (default 200 ms) the merger flips to backup. When
//! primary traffic resumes the merger switches back after a brief
//! hold-off to avoid flapping on bursty feeds.
//!
//! This is **not** SMPTE 2022-7 sequence-aware dedup — [`EsPacket`]
//! carries no upstream RTP sequence number today. Seq-aware dedup is a
//! follow-up that requires augmenting the bus shape. Failover semantics
//! are the common operator intent for "hot standby" and robust without
//! seq plumbing.
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
use super::ts_es_bus::NodeEsBus;

/// Default stall timer: if no primary packet arrives within this
/// window the merger flips to backup. Tight enough that standby cuts
/// over within one video frame at 25 fps (40 ms per frame × 5 frames)
/// but loose enough that bursty ingress jitter doesn't flap.
pub const DEFAULT_STALL_MS: u64 = 200;

/// Namespace for synthetic bus keys used by Hitless slots. The merger
/// publishes onto `(format!("{HITLESS_INPUT_PREFIX}{uid}"), 0)`. The
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

/// Build the synthetic bus key `(input_id, source_pid)` for a Hitless
/// slot. The `uid` should be unique per Hitless slot within the flow —
/// convention is `"slot_{program_idx}_{slot_idx}"` so two Hitless slots
/// on the same flow never collide.
pub fn hitless_bus_key(uid: &str) -> (String, u16) {
    (format!("{HITLESS_INPUT_PREFIX}{uid}"), 0)
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
                let key = hitless_bus_key(&slot.uid);
                for planned in plan.programs.iter_mut().flat_map(|p| p.slots.iter_mut()) {
                    if planned.source == key
                        && let Some(legs) = planned.hitless_legs.as_mut()
                    {
                        legs.forwarding_primary = running.slot.forwarding_primary.clone();
                    }
                }
                continue;
            }
            let cancel = parent.child_token();
            let task = spawn_hitless_es_merger_full(
                slot.uid.clone(),
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
/// The primary-preference merger sets `forwarding_primary` on each flip, so
/// the flow counts the SRT drops of the leg it forwards. The seq-aware
/// mergers leave it on the primary: they merge on `upstream_seq`, which no
/// SRT input stamps, so of SRT legs they forward the primary verbatim and
/// discard the backup.
pub fn spawn_hitless_es_merger_full(
    uid: String,
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
    let (out_input, out_pid) = hitless_bus_key(&uid);
    // `stream_type = 0` (unknown) → bus channel sizes at the video-class
    // default (8192). Hitless mergers don't know the essence type up
    // front and we'd rather over-allocate by a few KB than truncate a
    // real video leg.
    let out_tx = bus.sender_for(&out_input, out_pid, 0);

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
    use crate::redundancy::merger::{ActiveLeg, BufferedHitlessMerger};
    let mut merger: BufferedHitlessMerger<EsPacketAlias> =
        BufferedHitlessMerger::with_window(max_path_diff, reorder_window);
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
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => {
                            tracing::warn!(
                                uid = %uid,
                                "pid_bus hitless buffered: primary leg has no upstream_seq — \
                                 forwarding without buffer (bypass path)"
                            );
                            let _ = out_tx.send(es);
                            continue;
                        }
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest(seq, ActiveLeg::Leg1, es, now) {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => continue,
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest(seq, ActiveLeg::Leg2, es, now) {
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
    use crate::redundancy::merger::{ActiveLeg, HitlessMerger};
    let mut merger = HitlessMerger::with_window(reorder_window);
    let mut current_leg: Option<ActiveLeg> = None;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => {
                            tracing::warn!(
                                uid = %uid,
                                "pid_bus hitless seq_aware: primary leg has no upstream_seq — \
                                 falling back to forward (this should be rejected at config validation)"
                            );
                            let _ = out_tx.send(es);
                            continue;
                        }
                    };
                    if let Some(chosen) = merger.try_merge(seq, ActiveLeg::Leg1) {
                        if Some(chosen) != current_leg {
                            tracing::info!(uid = %uid, "pid_bus hitless seq_aware: leg = {:?}", chosen);
                            current_leg = Some(chosen);
                        }
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let seq = match es.upstream_seq {
                        Some(s) => s,
                        None => continue,
                    };
                    if let Some(chosen) = merger.try_merge(seq, ActiveLeg::Leg2) {
                        if Some(chosen) != current_leg {
                            tracing::info!(uid = %uid, "pid_bus hitless seq_aware: leg = {:?}", chosen);
                            current_leg = Some(chosen);
                        }
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
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
            "slot_0_0".to_string(),
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

        let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
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
            "slot_0_0".to_string(),
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

        let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
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
            "slot_0_0".to_string(),
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

        let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
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
        let seq_es = |pid: u16, seq: u16| EsPacket { upstream_seq: Some(seq), ..es(pid, 0) };
        for path_differential_ms in [None, Some(500)] {
            for (reorder_window, fills) in [(64u16, false), (128, true)] {
                let bus = Arc::new(NodeEsBus::new());
                let cancel = CancellationToken::new();
                let _handle = spawn_hitless_es_merger_full(
                    "slot_0_0".to_string(),
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
                let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
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
                    forwarded.push(es.upstream_seq);
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
                "slot_0_0".to_string(),
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
            let mut out_rx = bus.subscribe(&format!("{HITLESS_INPUT_PREFIX}slot_0_0"), 0);
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
}
