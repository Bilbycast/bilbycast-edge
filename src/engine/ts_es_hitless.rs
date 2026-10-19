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
//! A leg whose packets reach the bus with no sequence number — any input but
//! RTP and RIST, or one whose ingress transcode or post-process stage
//! rewrote its stream — gives a seq-aware merger nothing to merge on.
//! Validation refuses such a slot (`hitless_seq_aware_leg_unsequenced`). A
//! merger that meets such a packet regardless falls back to primary
//! preference for the rest of its life — its stall timer and failover, which
//! is all such a pair can do — raises a Warning and sets the slot's
//! [`SeqFallback`], which keeps the flow's health on that reason while it
//! lasts.
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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

/// A Hitless slot's leg: index into a merger's `[primary, backup]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leg {
    Primary = 0,
    Backup = 1,
}

impl Leg {
    fn name(self) -> &'static str {
        ["primary", "backup"][self as usize]
    }
}

/// Whether a seq-aware Hitless slot's merger has fallen back to primary
/// preference, and which leg made it: one brought a packet with no upstream
/// sequence number to merge on. One per slot, shared like
/// [`ForwardingPrimary`]: the merger sets it once, and keeps primary
/// preference from then on; the assembler's 1 Hz scan reads it through the
/// slot's [`crate::engine::ts_assembler::HitlessLegs`] into
/// `assembly_health.unsequenced_hitless_slots`, which holds the flow's
/// `hitless_seq_aware_leg_unsequenced` health reason while it is set.
///
/// Runtime state, not plan: any two compare equal.
#[derive(Debug, Clone, Default)]
pub struct SeqFallback(Arc<AtomicU8>);

impl SeqFallback {
    /// The leg that made the merger fall back, `"primary"` or `"backup"`;
    /// `None` while it merges.
    pub fn leg(&self) -> Option<&'static str> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            1 => Some(Leg::Primary.name()),
            _ => Some(Leg::Backup.name()),
        }
    }

    fn set(&self, leg: Leg) {
        self.0.store(leg as u8 + 1, Ordering::Relaxed);
    }
}

impl PartialEq for SeqFallback {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for SeqFallback {}

/// The scope of one run of flow `flow_id` — one `FlowRuntime` — for its
/// Hitless slots' bus keys ([`hitless_bus_key`]): `"{flow_id}:{generation}"`,
/// the generation counted node-wide, so no two runs of a flow share it.
///
/// Keyed by flow and slot alone, a restarted flow's merger took over a
/// channel its previous run had retired but not yet released (that run's
/// swap was waiting on its assembler), and the old swap's release then
/// took the new run's channel off the bus: the next fan-in or analyzer to
/// subscribe found nothing on the key and the slot stalled. Scoped by run,
/// each run only ever touches its own channels.
pub fn hitless_scope(flow_id: &str) -> String {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    format!("{flow_id}:{}", GENERATION.fetch_add(1, Ordering::Relaxed))
}

/// Build the synthetic bus key `(input_id, source_pid)` for the Hitless
/// slot `uid` of the flow run `scope` ([`hitless_scope`]):
/// `("hitless:{flow_id}:{generation}:{uid}", 0)`.
///
/// The bus is node-wide — every flow shares the [`NodeEsBus`] its
/// `FlowManager` owns, and `sender_for` hands a second publisher the
/// existing channel — so the key names the flow as well as the slot. Keyed
/// on the slot alone, two assembled flows with a Hitless slot at the same
/// position published onto one key and each assembler played both flows'
/// packets. `uid` is unique per Hitless slot within the flow
/// (`"slot_{program_idx}_{slot_idx}"`) and, like the generation, holds no
/// `:`, so the last two `:` separate the three and no two flows' slots, nor
/// two runs of one flow, share a key.
pub fn hitless_bus_key(scope: &str, uid: &str) -> (String, u16) {
    (format!("{HITLESS_INPUT_PREFIX}{scope}:{uid}"), 0)
}

/// The mergers running behind one assembled flow's Hitless slots, by slot
/// uid. A slot's merger is the only publisher onto the slot's bus key, so
/// the flow keeps exactly one per slot: [`Self::apply`] brings the set in
/// line with each plan before it goes on air.
#[derive(Default)]
pub struct HitlessMergers {
    running: HashMap<String, RunningMerger>,
    /// The bus channels the mergers publish onto, shared with the flow so it
    /// can release them when it stops without taking the mergers' lock.
    outputs: Arc<HitlessOutputs>,
    /// Where the mergers raise their events; `None` in tests.
    events: Option<MergerEvents>,
}

/// Where a flow's Hitless mergers raise their events.
#[derive(Clone)]
pub struct MergerEvents {
    pub sender: crate::manager::events::EventSender,
    pub flow_id: String,
}

struct RunningMerger {
    slot: PendingHitlessSlot,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// The bus channels a flow's Hitless mergers publish onto, by bus key: each
/// a clone of the merger's own sender, so the channel can be removed from
/// the bus by identity once its merger is gone for good
/// ([`NodeEsBus::remove_if_same`]). A `std` mutex, taken for an insert or a
/// drain on the control plane and never held across an `await`.
///
/// Closed for good by the flow's stop ([`Self::release_all`]); a channel
/// handed in after that ([`Self::register`]) is released there and then.
#[derive(Default)]
pub struct HitlessOutputs(std::sync::Mutex<OutputChannels>);

#[derive(Default)]
struct OutputChannels {
    channels: HashMap<(String, u16), broadcast::Sender<EsPacketAlias>>,
    /// The flow has stopped and released them.
    closed: bool,
}

/// A Hitless slot's bus channel whose merger a plan swap stopped for good:
/// the new plan has no slot on its key. Released once the assembler runs the
/// new plan ([`RetiredOutput::release`]).
pub struct RetiredOutput {
    key: (String, u16),
    sender: broadcast::Sender<EsPacketAlias>,
}

impl RetiredOutput {
    /// Take the channel off the bus. Nothing reads it any more: its merger
    /// is gone and the assembler, on the new plan, has cancelled its
    /// fan-in. The per-ES analyzer still subscribed sees it close and ends,
    /// and the ring goes with the last receiver.
    pub fn release(self, bus: &NodeEsBus) {
        bus.remove_if_same(&self.key, &self.sender);
    }

    /// The slot's bus key.
    pub fn key(&self) -> &(String, u16) {
        &self.key
    }
}

impl HitlessOutputs {
    fn lock(&self) -> std::sync::MutexGuard<'_, OutputChannels> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take every channel off the bus, and every one handed in from now on:
    /// the flow has stopped. Called after its cancel, which every merger and
    /// fan-in watches first.
    pub fn release_all(&self, bus: &NodeEsBus) {
        let mut outputs = self.lock();
        outputs.closed = true;
        for (key, sender) in outputs.channels.drain() {
            bus.remove_if_same(&key, &sender);
        }
    }

    /// Hold `sender`, the channel at `key` a merger just took, for release
    /// when the flow stops — or, if it has already stopped, release it now
    /// and say so (`false`). A plan swap racing the flow's stop starts its
    /// new mergers after the stop drained the rest, and nothing would ever
    /// release theirs; checked and held under one lock, so a channel is
    /// either drained by the stop or refused after it.
    pub fn register(
        &self,
        key: (String, u16),
        sender: broadcast::Sender<EsPacketAlias>,
        bus: &NodeEsBus,
    ) -> bool {
        let mut outputs = self.lock();
        if outputs.closed {
            bus.remove_if_same(&key, &sender);
            return false;
        }
        outputs.channels.insert(key, sender);
        true
    }

    /// Stop holding the channel at `key`, and hand it over.
    fn take(&self, key: &(String, u16)) -> Option<broadcast::Sender<EsPacketAlias>> {
        self.lock().channels.remove(key)
    }
}

impl HitlessMergers {
    /// The mergers of flow `flow_id`, raising their events on `sender`.
    pub fn new(sender: crate::manager::events::EventSender, flow_id: &str) -> Self {
        Self {
            events: Some(MergerEvents { sender, flow_id: flow_id.to_string() }),
            ..Self::default()
        }
    }

    /// Bring the running mergers in line with `slots`, the Hitless slots of
    /// `plan`, before `plan` goes on air.
    ///
    /// A slot the plan leaves as it was keeps its merger, uninterrupted,
    /// and `plan` takes that merger's [`ForwardingPrimary`] and
    /// [`SeqFallback`] in place of the fresh ones it was built with, so the
    /// on-air set goes on following the merger's failovers and the flow's
    /// health its fallback. A merger whose slot is gone or changed is
    /// cancelled and awaited, so it is no longer publishing onto the slot's
    /// bus key when its replacement starts there. Every slot without a
    /// merger then gets one, under `parent` — until the flow is found to
    /// have stopped meanwhile ([`HitlessOutputs::register`]), whose stop
    /// has released every channel but the one just taken: that one is
    /// released, and the rest of the slots get none.
    ///
    /// Returns the bus channels of the mergers it stopped whose key the plan
    /// has no slot on: the caller releases them once the assembler runs the
    /// plan.
    pub async fn apply(
        &mut self,
        slots: &[PendingHitlessSlot],
        plan: &mut AssemblyPlan,
        bus: &Arc<NodeEsBus>,
        parent: &CancellationToken,
    ) -> Vec<RetiredOutput> {
        let mut kept = HashMap::with_capacity(slots.len());
        let mut retired = Vec::new();
        for (uid, running) in std::mem::take(&mut self.running) {
            // `ForwardingPrimary` compares equal whatever it holds, so this
            // compares the slot's legs and settings only.
            if slots.contains(&running.slot) {
                kept.insert(uid, running);
            } else {
                running.cancel.cancel();
                let _ = running.task.await;
                let key = running.slot.bus_key;
                if !slots.iter().any(|slot| slot.bus_key == key)
                    && let Some(sender) = self.outputs.take(&key)
                {
                    retired.push(RetiredOutput { key, sender });
                }
            }
        }
        for slot in slots {
            if let Some(running) = kept.get(&slot.uid) {
                for planned in plan.programs.iter_mut().flat_map(|p| p.slots.iter_mut()) {
                    if planned.source == slot.bus_key
                        && let Some(legs) = planned.hitless_legs.as_mut()
                    {
                        legs.forwarding_primary = running.slot.forwarding_primary.clone();
                        legs.seq_fallback = running.slot.seq_fallback.clone();
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
                slot.seq_fallback.clone(),
                cancel.clone(),
                self.events.clone(),
            );
            // The channel the merger just took, for releasing it later. A
            // flow that stopped under the swap has released the others and
            // refuses it: released here, and no further merger started.
            let sender = bus.sender_for(&slot.bus_key.0, slot.bus_key.1, 0);
            if !self.outputs.register(slot.bus_key.clone(), sender, bus) {
                cancel.cancel();
                break;
            }
            kept.insert(slot.uid.clone(), RunningMerger { slot: slot.clone(), cancel, task });
        }
        self.running = kept;
        retired
    }

    /// The bus channels of these mergers, for the flow to release when it
    /// stops ([`HitlessOutputs::release_all`]).
    pub fn outputs(&self) -> Arc<HitlessOutputs> {
        self.outputs.clone()
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
/// mergers leave it on the primary while they merge: they merge on
/// `upstream_seq`, which no SRT input stamps — validation refuses such legs.
///
/// A seq-aware merger that meets a packet with no `upstream_seq` on either
/// leg has nothing to merge on: it sets `seq_fallback` and runs primary
/// preference, with `stall`, from then on, so it still fails over. It used
/// to forward an unsequenced primary alone and drop the backup for good, so
/// the slot went dark with the primary, or never fail over to an
/// unsequenced backup. `events`, when given, is where it raises its Warning
/// about that.
#[allow(clippy::too_many_arguments)]
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
    seq_fallback: SeqFallback,
    cancel: CancellationToken,
    events: Option<MergerEvents>,
) -> JoinHandle<()> {
    let notice = FallbackNotice {
        uid: output.0.clone(),
        legs: [primary.clone(), backup.clone()],
        fallback: seq_fallback,
        events,
    };
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
        if seq_aware {
            let fell_back = match path_differential_ms {
                Some(ms) => {
                    run_buffered(
                        &mut primary_rx,
                        &mut backup_rx,
                        &out_tx,
                        Duration::from_millis(ms as u64),
                        reorder_window,
                        &cancel,
                    )
                    .await
                }
                None => {
                    run_seq_aware(&uid, &mut primary_rx, &mut backup_rx, &out_tx, reorder_window, &cancel)
                        .await
                }
            };
            // `None`: cancelled, or a leg's channel closed.
            let Some(Unsequenced { leg, packet }) = fell_back else {
                return;
            };
            notice.raise(leg, stall);
            // Primary preference starts on the primary: a primary packet goes
            // out, a backup one is dropped.
            if leg == Leg::Primary {
                let _ = out_tx.send(packet);
            }
        }
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
    })
}

/// The packet with no `upstream_seq` that ended a seq-aware merge, and the
/// leg that brought it.
struct Unsequenced {
    leg: Leg,
    packet: EsPacketAlias,
}

/// Merge behind a hold of `max_path_diff` until a packet comes with no
/// `upstream_seq` (returned), or the merger is cancelled or a leg's channel
/// closes (`None`).
async fn run_buffered(
    primary_rx: &mut broadcast::Receiver<EsPacketAlias>,
    backup_rx: &mut broadcast::Receiver<EsPacketAlias>,
    out_tx: &broadcast::Sender<EsPacketAlias>,
    max_path_diff: Duration,
    reorder_window: u16,
    cancel: &CancellationToken,
) -> Option<Unsequenced> {
    use crate::redundancy::merger::{ActiveLeg, BufferedHitlessMerger, HeldRuns};
    let mut merger: BufferedHitlessMerger<EsPacketAlias, HeldRuns> =
        BufferedHitlessMerger::with_runs(max_path_diff, reorder_window);
    // Far-future placeholder used when the buffer is empty so the
    // tokio::select! `sleep_until` arm doesn't fire spuriously.
    let far_future = std::time::Instant::now() + Duration::from_secs(86_400);
    // Primary preference, which takes over, starts on the primary: what the
    // hold has of the primary's goes out now, in order, and of the backup's
    // is dropped, as primary preference drops the backup's.
    let hand_over = |merger: &mut BufferedHitlessMerger<EsPacketAlias, HeldRuns>| {
        for (leg, es) in merger.drain_expired(far_future) {
            if leg == ActiveLeg::Leg1 {
                let _ = out_tx.send(es);
            }
        }
    };

    loop {
        let drain_at = merger.next_deadline().unwrap_or(far_future);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            _ = sleep_until(tokio::time::Instant::from_std(drain_at)) => {
                for (_, es) in merger.drain_expired(std::time::Instant::now()) {
                    let _ = out_tx.send(es);
                }
            }
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        hand_over(&mut merger);
                        return Some(Unsequenced { leg: Leg::Primary, packet: es });
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest_run(seq, index, ActiveLeg::Leg1, es, now) {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        hand_over(&mut merger);
                        return Some(Unsequenced { leg: Leg::Backup, packet: es });
                    };
                    let now = std::time::Instant::now();
                    for (_, es) in merger.ingest_run(seq, index, ActiveLeg::Leg2, es, now) {
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
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

/// Merge, unbuffered, until a packet comes with no `upstream_seq`
/// (returned), or the merger is cancelled or a leg's channel closes
/// (`None`).
async fn run_seq_aware(
    uid: &str,
    primary_rx: &mut broadcast::Receiver<EsPacketAlias>,
    backup_rx: &mut broadcast::Receiver<EsPacketAlias>,
    out_tx: &broadcast::Sender<EsPacketAlias>,
    reorder_window: u16,
    cancel: &CancellationToken,
) -> Option<Unsequenced> {
    use crate::redundancy::merger::{ActiveLeg, HitlessRunMerger};
    let mut merger = HitlessRunMerger::with_window(reorder_window);
    let mut leg_log = LegLog::default();
    let mut primary_clock = DatagramClock::default();
    let mut backup_clock = DatagramClock::default();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            res = primary_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        return Some(Unsequenced { leg: Leg::Primary, packet: es });
                    };
                    let now_us = primary_clock.now_us(seq);
                    if let Some(chosen) = merger.try_merge(seq, index, ActiveLeg::Leg1, now_us) {
                        let _ = leg_log.forwarded(uid, chosen);
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            },
            res = backup_rx.recv() => match res {
                Ok(es) => {
                    let Some(UpstreamSeq { seq, index }) = es.upstream_seq else {
                        return Some(Unsequenced { leg: Leg::Backup, packet: es });
                    };
                    let now_us = backup_clock.now_us(seq);
                    if let Some(chosen) = merger.try_merge(seq, index, ActiveLeg::Leg2, now_us) {
                        let _ = leg_log.forwarded(uid, chosen);
                        let _ = out_tx.send(es);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            },
        }
    }
}

/// When a leg's packets reach a seq-aware merger, on the merger's own clock:
/// read at the first packet of each datagram and kept for the rest of its
/// run of the PID, which follows it on the bus. Not `recv_time_us`, which
/// is a different instant on each kind of leg — a RIST input's arrival
/// before its hold, a de-jittered RTP input's release, a plain RTP input's
/// socket read — so a mixed pair's stamps sit up to a hold apart, and the
/// merger would read a silence where the other leg had just been accepted.
#[derive(Default)]
struct DatagramClock(Option<(u16, u64)>);

impl DatagramClock {
    /// The time a packet of datagram `seq` reached the merger.
    fn now_us(&mut self, seq: u16) -> u64 {
        match self.0 {
            Some((datagram, us)) if datagram == seq => us,
            _ => {
                let us = crate::util::time::now_us();
                self.0 = Some((seq, us));
                us
            }
        }
    }
}

/// What a seq-aware merger says when it falls back to primary preference: it
/// sets the slot's [`SeqFallback`], logs, and raises a Warning event when it
/// has somewhere to. Validation refuses a leg that would bring it here
/// (`hitless_seq_aware_leg_unsequenced`); this is the runtime's word on one
/// that got here regardless.
struct FallbackNotice {
    uid: String,
    legs: [(String, u16); 2],
    fallback: SeqFallback,
    events: Option<MergerEvents>,
}

impl FallbackNotice {
    fn raise(&self, leg: Leg, stall: Duration) {
        self.fallback.set(leg);
        let (name, (input_id, pid)) = (leg.name(), &self.legs[leg as usize]);
        let stall_ms = stall.as_millis();
        let message = format!(
            "Hitless slot {}: the {name} leg (input '{input_id}', PID 0x{pid:04X}) brings packets \
             with no upstream sequence number, so the seq-aware merge cannot run — the slot has \
             fallen back to primary preference: it forwards the primary, fails over to the backup \
             after {stall_ms} ms without it, and merges nothing. Use mode \"primary_preference\", \
             or a leg that carries one: an RTP or RIST input with no ingress transcode, program \
             filter, PID rewrite or clock regeneration",
            self.uid
        );
        tracing::warn!("{message}");
        if let Some(events) = &self.events {
            events.sender.emit_flow_with_details(
                crate::manager::events::EventSeverity::Warning,
                crate::manager::events::category::FLOW,
                format!("Flow '{}': {message}", events.flow_id),
                &events.flow_id,
                serde_json::json!({
                    "error_code": "hitless_seq_aware_leg_unsequenced",
                    "slot": self.uid,
                    "leg": name,
                    "input_id": input_id,
                    "source_pid": pid,
                    "fallback": "primary_preference",
                    "stall_ms": stall_ms,
                }),
            );
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
            SeqFallback::default(),
            cancel.clone(), None
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
            SeqFallback::default(),
            cancel.clone(), None
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
            SeqFallback::default(),
            cancel.clone(), None
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
                    SeqFallback::default(),
                    cancel.clone(), None
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
    /// space or more apart, which reads as "too far behind". Such a cue steps
    /// back as far from its own leg's previous cue, so when the backup's copy
    /// follows the primary's, both legs' sequences broken at one seq, it
    /// re-anchors the window and the cue is forwarded once, bare or buffered.
    /// (Both mergers read their own clock, and the cues here are sent
    /// milliseconds apart, so their stale time never runs out; it is covered
    /// in `redundancy::merger`.) Half the cues used to be dropped: lost ad
    /// cues.
    #[tokio::test]
    async fn seq_aware_forwards_every_cue_of_a_sparse_pid() {
        let cue = |pid: u16, seq: u16| EsPacket {
            upstream_seq: Some(UpstreamSeq { seq, index: 0 }),
            ..es(pid, 0)
        };
        // Seq steps between cues: four in the too-far-behind band, one a
        // forward jump of a window or more, one an ordinary step.
        let steps: [u16; 7] = [40_000, 35_000, 20_000, 50_000, 33_000, 900, 61_000];
        for path_differential_ms in [None, Some(50)] {
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
                SeqFallback::default(),
                cancel.clone(), None
            );
            let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
            let pri = bus.sender_for("in-a", 0x1F0, 0x86);
            let bak = bus.sender_for("in-b", 0x1F0, 0x86);
            tokio::time::sleep(Duration::from_millis(20)).await;

            let mut seq: u16 = 100;
            let mut cues = vec![seq];
            for step in steps {
                seq = seq.wrapping_add(step);
                cues.push(seq);
            }
            for &seq in &cues {
                pri.send(cue(0x1F0, seq)).unwrap();
                bak.send(cue(0x1F0, seq)).unwrap();
                // In order through the merger task, primary before backup.
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            let quiet = Duration::from_millis(if path_differential_ms.is_some() { 300 } else { 100 });
            let mut forwarded = Vec::new();
            while let Ok(Ok(es)) = tokio::time::timeout(quiet, out_rx.recv()).await {
                forwarded.push(es.upstream_seq.map(|u| u.seq).unwrap());
            }
            assert_eq!(forwarded, cues, "path differential {path_differential_ms:?}");
            cancel.cancel();
        }
    }

    /// A bare seq-aware merger over a RIST primary, whose packets are stamped
    /// at their arrival ahead of a 1.5 s hold, and an RTP backup with ingress
    /// de-jitter, stamped at its release and 2 000 datagrams behind. The
    /// backup's first packet is too far behind the window and, a first
    /// packet, taken for a break: it may re-anchor the window only if the
    /// merger has been quiet for the stale time, which on its own clock it
    /// has not — the primary was accepted a moment ago. Read off the stamps,
    /// 1.5 s apart, it had been, and the merger forwarded a datagram the
    /// primary had delivered 2 000 earlier, then jumped forward again.
    #[tokio::test]
    async fn seq_aware_reads_its_own_clock_not_its_legs_stamps() {
        crate::util::time::init_epoch();
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
            /* path_differential_ms */ None,
            ForwardingPrimary::default(),
            SeqFallback::default(),
            cancel.clone(),
            None,
        );
        let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
        let pri = bus.sender_for("in-a", 0x100, 0x1B);
        let bak = bus.sender_for("in-b", 0x100, 0x1B);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let packet = |seq: u16, recv_time_us: u64| EsPacket {
            upstream_seq: Some(UpstreamSeq { seq, index: 0 }),
            recv_time_us,
            ..es(0x100, 0)
        };
        let released_us = 10_000_000u64;
        for seq in 3_000..3_100u16 {
            pri.send(packet(seq, released_us - 1_500_000)).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
        bak.send(packet(1_099, released_us)).unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        pri.send(packet(3_100, released_us - 1_500_000)).unwrap();

        let mut forwarded = Vec::new();
        while let Ok(Ok(es)) = tokio::time::timeout(Duration::from_millis(100), out_rx.recv()).await {
            forwarded.push(es.upstream_seq.map(|u| u.seq).unwrap());
        }
        assert_eq!(forwarded, (3_000..=3_100).collect::<Vec<u16>>());
        cancel.cancel();
    }

    /// A leg with no `upstream_seq` — an SRT input, or one whose stream an
    /// ingress stage rewrote — gives a seq-aware merger nothing to merge on,
    /// so it falls back to primary preference: it forwards the primary,
    /// fails over to the backup once the primary has stalled, and returns
    /// to the primary when it resumes, with the slot's [`SeqFallback`] set
    /// and one Warning raised. It used to forward an unsequenced primary
    /// alone and discard the backup for the merger's life, so the slot went
    /// dark when the primary did — and never failed over to an unsequenced
    /// backup either.
    #[tokio::test]
    async fn seq_aware_falls_back_to_primary_preference_on_an_unsequenced_leg() {
        let packet = |pid: u16, byte: u8, seq: Option<u16>| EsPacket {
            upstream_seq: seq.map(|seq| UpstreamSeq { seq, index: 0 }),
            ..es(pid, byte)
        };
        // (which leg is unsequenced, primary has a seq, backup has a seq)
        for (unsequenced, primary_seq, backup_seq) in
            [("primary", false, true), ("backup", true, false), ("primary", false, false)]
        {
            for path_differential_ms in [None, Some(50)] {
                let case = format!("{unsequenced} {primary_seq}/{backup_seq} {path_differential_ms:?}");
                let bus = Arc::new(NodeEsBus::new());
                let cancel = CancellationToken::new();
                let forwarding = ForwardingPrimary::default();
                let fallback = SeqFallback::default();
                let (sender, mut events) = crate::manager::events::event_channel();
                let _handle = spawn_hitless_es_merger_full(
                    hitless_bus_key("f1", "slot_0_0"),
                    ("in-a".to_string(), 0x100),
                    ("in-b".to_string(), 0x200),
                    bus.clone(),
                    Duration::from_millis(200),
                    /* seq_aware */ true,
                    crate::redundancy::merger::REORDER_WINDOW,
                    path_differential_ms,
                    forwarding.clone(),
                    fallback.clone(),
                    cancel.clone(),
                    Some(MergerEvents { sender, flow_id: "f1".into() }),
                );
                let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
                let pri = bus.sender_for("in-a", 0x100, 0x1B);
                let bak = bus.sender_for("in-b", 0x200, 0x1B);
                let next = async |out_rx: &mut broadcast::Receiver<EsPacket>| {
                    tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
                        .await
                        .ok()
                        .map(|res| res.expect("channel open").payload[4])
                };
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert_eq!(fallback.leg(), None, "{case}: merging");

                // Each leg's first packet, the primary's first: the merge
                // ends at the unsequenced one, and what follows is primary
                // preference on the primary.
                pri.send(packet(0x100, 0xA0, primary_seq.then_some(1))).unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
                bak.send(packet(0x200, 0xB0, backup_seq.then_some(1))).unwrap();
                assert_eq!(next(&mut out_rx).await, Some(0xA0), "{case}: the primary");
                assert_eq!(next(&mut out_rx).await, None, "{case}: not the backup with it");
                assert_eq!(fallback.leg(), Some(unsequenced), "{case}");
                assert!(forwarding.get(), "{case}");

                // The primary stops: past the stall the backup goes out.
                tokio::time::sleep(Duration::from_millis(150)).await;
                bak.send(packet(0x200, 0xB1, backup_seq.then_some(2))).unwrap();
                assert_eq!(next(&mut out_rx).await, Some(0xB1), "{case}: the failover");
                assert!(!forwarding.get(), "{case}: the backup on air");

                // And back to the primary when it resumes.
                pri.send(packet(0x100, 0xA1, primary_seq.then_some(3))).unwrap();
                assert_eq!(next(&mut out_rx).await, Some(0xA1), "{case}: the return");
                assert!(forwarding.get(), "{case}: the primary on air");
                assert_eq!(fallback.leg(), Some(unsequenced), "{case}: for the merger's life");

                let warned: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
                    .filter_map(|e| e.details)
                    .filter(|d| d["error_code"] == "hitless_seq_aware_leg_unsequenced")
                    .collect();
                assert_eq!(warned.len(), 1, "{case}: one Warning");
                assert_eq!(warned[0]["leg"], unsequenced, "{case}");
                assert_eq!(warned[0]["fallback"], "primary_preference", "{case}");
                cancel.cancel();
            }
        }
    }

    /// A primary with no sequence number beside a backup that has one: the
    /// merger falls back to primary preference, which forwards the live
    /// primary on its own and drops the backup, raising one Warning. It
    /// forwarded the primary on its bypass and merged the backup as well, so
    /// every packet reached the output twice.
    #[tokio::test]
    async fn seq_aware_forwards_an_unsequenced_primary_alone() {
        for path_differential_ms in [None, Some(50)] {
            let bus = Arc::new(NodeEsBus::new());
            let cancel = CancellationToken::new();
            let (sender, mut events) = crate::manager::events::event_channel();
            let _handle = spawn_hitless_es_merger_full(
                hitless_bus_key("f1", "slot_0_0"),
                ("in-a".to_string(), 0x100),
                ("in-b".to_string(), 0x200),
                bus.clone(),
                Duration::from_millis(50),
                /* seq_aware */ true,
                crate::redundancy::merger::REORDER_WINDOW,
                path_differential_ms,
                ForwardingPrimary::default(),
                SeqFallback::default(),
                cancel.clone(),
                Some(MergerEvents { sender, flow_id: "f1".into() }),
            );
            let mut out_rx = bus.subscribe(&hitless_bus_key("f1", "slot_0_0").0, 0);
            let pri = bus.sender_for("in-a", 0x100, 0x1B);
            let bak = bus.sender_for("in-b", 0x200, 0x1B);
            tokio::time::sleep(Duration::from_millis(20)).await;
            for seq in 0..20u16 {
                pri.send(es(0x100, seq as u8)).unwrap();
                bak.send(EsPacket {
                    upstream_seq: Some(UpstreamSeq { seq, index: 0 }),
                    ..es(0x200, 0x80 | seq as u8)
                })
                .unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let mut forwarded = Vec::new();
            while let Ok(Ok(es)) = tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await {
                forwarded.push(es.payload[4]);
            }
            assert_eq!(
                forwarded,
                (0..20u8).collect::<Vec<_>>(),
                "{path_differential_ms:?}: the primary alone"
            );
            let warned: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
                .filter(|e| {
                    e.details.as_ref().and_then(|d| d["error_code"].as_str())
                        == Some("hitless_seq_aware_leg_unsequenced")
                })
                .collect();
            assert_eq!(warned.len(), 1, "{path_differential_ms:?}: one Warning for the leg");
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
            SeqFallback::default(),
            cancel.clone(), None
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
                SeqFallback::default(),
                cancel.clone(), None
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
            SeqFallback::default(),
            cancel.clone(), None
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

    /// A primary-preference Hitless slot `uid` of the flow run `scope`, over
    /// `in-a` / `in-b`.
    fn pending(scope: &str, uid: &str) -> PendingHitlessSlot {
        PendingHitlessSlot {
            uid: uid.to_string(),
            bus_key: hitless_bus_key(scope, uid),
            primary: ("in-a".to_string(), 0x100),
            backup: ("in-b".to_string(), 0x200),
            stall_ms: DEFAULT_STALL_MS,
            seq_aware: false,
            reorder_window: crate::redundancy::merger::REORDER_WINDOW,
            path_differential_ms: None,
            forwarding_primary: ForwardingPrimary::default(),
            seq_fallback: SeqFallback::default(),
        }
    }

    /// A plan swap the flow's stop overtakes — the stop runs while the swap
    /// waits on the mergers it stops — starts its new mergers after the stop
    /// released the flow's channels. Theirs are released too, and nothing
    /// that subscribes to a released key afterwards (an analyzer or fan-in
    /// started on the swap's way out) brings it back: each such channel was
    /// held by nothing that releases it, an 8 192-slot ring for the life of
    /// the node.
    #[tokio::test]
    async fn a_swap_its_flows_stop_overtakes_leaves_no_channel_on_the_bus() {
        let bus = Arc::new(NodeEsBus::new());
        let flow = CancellationToken::new();
        let scope = hitless_scope("f1");
        let mut mergers = HitlessMergers::default();
        let outputs = mergers.outputs();
        let mut plan = AssemblyPlan { programs: Vec::new() };
        mergers.apply(&[pending(&scope, "slot_0_0")], &mut plan, &bus, &flow).await;

        // The flow stops: its cancel, then its channels.
        flow.cancel();
        outputs.release_all(&bus);
        // The swap goes on, onto a plan with another slot.
        let retired = mergers.apply(&[pending(&scope, "slot_0_1")], &mut plan, &bus, &flow).await;
        assert!(retired.is_empty(), "the stop took the old slot's channel");
        // On its way out the swap starts the new slot's analyzer, and the
        // assembler a fan-in on the old one.
        let mut analyzer = bus.subscribe(&hitless_bus_key(&scope, "slot_0_1").0, 0);
        let _fanin = bus.subscribe(&hitless_bus_key(&scope, "slot_0_0").0, 0);
        assert!(
            matches!(
                tokio::time::timeout(Duration::from_millis(500), analyzer.recv()).await,
                Ok(Err(broadcast::error::RecvError::Closed))
            ),
            "a released channel subscribes closed"
        );
        let left: Vec<_> =
            bus.keys().into_iter().filter(|(k, _)| k.starts_with(HITLESS_INPUT_PREFIX)).collect();
        assert!(left.is_empty(), "the stopped flow's channels are all released: {left:?}");
    }

    /// A flow restarted while a swap of its previous run waits on that run's
    /// assembler: the new run's merger on the same slot publishes onto a
    /// channel of its own, which the old swap's release of the slot's
    /// channel, once its assembler answers, leaves on the bus. Keyed by flow
    /// and slot alone, the new merger took over the retired channel, the
    /// release took it off the bus, and the next fan-in or analyzer to
    /// subscribe found nothing on the key: the slot stalled.
    #[tokio::test]
    async fn a_restarted_flow_keeps_its_channel_when_its_old_run_releases_its_own() {
        let bus = Arc::new(NodeEsBus::new());
        let mut plan = AssemblyPlan { programs: Vec::new() };
        // The old run, and a swap that drops the slot: its channel retired,
        // to be released once the run's assembler answers.
        let old_flow = CancellationToken::new();
        let old = hitless_scope("f1");
        let mut old_mergers = HitlessMergers::default();
        old_mergers.apply(&[pending(&old, "slot_0_0")], &mut plan, &bus, &old_flow).await;
        let retired = old_mergers.apply(&[], &mut plan, &bus, &old_flow).await;
        assert_eq!(retired.len(), 1, "the dropped slot's channel is retired");

        // The flow restarts, onto a plan with the slot.
        old_flow.cancel();
        old_mergers.outputs().release_all(&bus);
        let new_flow = CancellationToken::new();
        let _stop = new_flow.clone().drop_guard();
        let new = hitless_scope("f1");
        let mut new_mergers = HitlessMergers::default();
        new_mergers.apply(&[pending(&new, "slot_0_0")], &mut plan, &bus, &new_flow).await;

        // The old run's assembler answers, and its swap releases the channel
        // it retired.
        for output in retired {
            output.release(&bus);
        }
        // A fan-in subscribing now still reads the new run's merger.
        let (key, pid) = hitless_bus_key(&new, "slot_0_0");
        let mut fanin = bus.subscribe(&key, pid);
        bus.sender_for("in-a", 0x100, 0x1B)
            .send(es(0x100, 0xAA))
            .expect("the new merger reads its primary");
        let got = tokio::time::timeout(Duration::from_millis(500), fanin.recv())
            .await
            .expect("the merger forwards its primary")
            .expect("onto the slot's channel, still on the bus");
        assert_eq!(got.payload[4], 0xAA);
    }
}
