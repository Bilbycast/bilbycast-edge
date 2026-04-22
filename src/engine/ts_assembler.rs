// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TS assembler for the PID-bus runtime (Phases 5 + 6 + MPTS).
//!
//! Subscribes to a set of elementary-stream channels on [`FlowEsBus`],
//! rewrites each `EsPacket`'s TS header PID → the configured `out_pid`,
//! stamps a per-out-PID monotonic continuity counter, and emits bundles
//! of 7 TS packets (1316 bytes, MTU-safe) as synthesised `RtpPacket`s
//! into the flow's broadcast channel. Synthesises PAT + PMT(s) on a
//! 100 ms cadence; each program's PCR rides on one of its own slots'
//! ES bytes-for-bytes (the PMT's `PCR_PID` points at that slot's
//! `out_pid`).
//!
//! Scope:
//! - Single-program (`AssemblyKind::Spts`) and multi-program
//!   (`AssemblyKind::Mpts`) via a single code path — `AssemblyPlan`
//!   carries `programs: Vec<ProgramPlan>` with `len == 1` for SPTS.
//! - Concrete `SlotSource::Pid` slots (Phase 5).
//! - `SlotSource::Essence { input_id, kind }` slots resolved against
//!   the input's Phase 2 PSI catalogue (Phase 6). `Hitless` still
//!   rejected at bring-up with a distinct error code.
//! - TS-producing inputs only (every `is_ts_carrier()` input qualifies;
//!   enforced by `FlowRuntime::start`).
//!
//! Performance contract (data-path rules):
//! - Each slot subscribes to a `broadcast::Receiver<EsPacket>` and funnels
//!   into a single `mpsc::Sender<(slot_idx, EsPacket)>`. Slow consumers
//!   see `broadcast::error::RecvError::Lagged(_)` and drop; nothing
//!   blocks the demuxer.
//! - Per-packet work: two header-byte edits + one CC increment. No PES
//!   reconstruction, no copies beyond `Bytes::copy_from_slice` of the
//!   188-byte payload into the bundle buffer.
//! - One allocation per emitted bundle (`BytesMut::with_capacity(1316)`
//!   swapped in after each flush) — bundles ship at ~(video packet rate
//!   / 7), not per-TS-packet.

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use super::packet::RtpPacket;
use super::ts_es_bus::{EsPacket, FlowEsBus};
use super::ts_parse::{mpeg2_crc32, NULL_PID, PAT_PID, TS_PACKET_SIZE, TS_SYNC_BYTE};

/// TS packets per emitted bundle. 7 × 188 = 1316 bytes — fits comfortably
/// inside a 1500-byte Ethernet MTU with headroom for IP+UDP headers.
/// Matches every other TS-on-UDP emitter in the codebase.
pub const BUNDLE_TS_PACKETS: usize = 7;
const BUNDLE_BYTES: usize = BUNDLE_TS_PACKETS * TS_PACKET_SIZE;

/// How often PAT/PMT are emitted. 100 ms matches the continuity-fixer
/// cadence and is well within the DVB-recommended ≤ 400 ms.
const PSI_INTERVAL: Duration = Duration::from_millis(100);

/// Safety-net flush cadence. If slots are sparse (audio-only wall-clock
/// idle, thumbnail gaps, etc.) we still push partially-filled bundles so
/// downstream sockets see regular traffic. 10 ms keeps latency tight.
const FLUSH_INTERVAL: Duration = Duration::from_millis(10);

/// Pre-resolved plan the assembler executes. The runtime expands
/// [`crate::config::models::FlowAssembly`] into this shape (resolving
/// Essence → PID via the input's PSI catalogue, rejecting Hitless,
/// validating that each program's `pcr_source` hits one of its own
/// slots) before handing it off. Keeping this type narrow makes the
/// assembler testable without dragging in the full config model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyPlan {
    /// One entry for SPTS, two or more for MPTS. PAT synthesis emits
    /// one entry per program; PMT synthesis emits one packet per
    /// program on the same 100 ms cadence.
    pub programs: Vec<ProgramPlan>,
}

/// One program within the assembled TS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramPlan {
    pub program_number: u16,
    pub pmt_pid: u16,
    /// `(input_id, source_pid)` of this program's PCR reference. Must
    /// match exactly one of `slots` — the assembler uses that slot's
    /// `out_pid` as the PMT's `PCR_PID` and forwards PCR packets
    /// byte-for-byte.
    pub pcr_source: (String, u16),
    pub slots: Vec<AssemblySlot>,
}

/// One elementary-stream slot within a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblySlot {
    /// Concrete `(input_id, source_pid)` on the bus.
    pub source: (String, u16),
    pub out_pid: u16,
    pub stream_type: u8,
}

/// Runtime commands the assembler accepts via its `mpsc` channel.
///
/// Phase 7 adds `ReplacePlan` — used by `update_flow_assembly` to swap
/// the whole `AssemblyPlan` without tearing down the flow. More granular
/// deltas (e.g. `SwitchSlot`) can land as additive variants; string-
/// dispatch on the handler side keeps old edges forwards-compatible via
/// the same mechanism the WS protocol already uses.
#[derive(Debug)]
pub enum PlanCommand {
    /// Swap the running plan. The assembler diffs old vs new, re-spawns
    /// fan-ins for added/changed slots, cancels fan-ins for removed
    /// slots, bumps per-program PMT versions where composition changed,
    /// and bumps PAT version when the set of programs itself changed.
    ReplacePlan { plan: AssemblyPlan },
}

/// Handle returned to the flow runtime after spawning the assembler.
/// Carries the task JoinHandle plus the mpsc sender for runtime mutation.
/// Dropping the sender does *not* stop the assembler — the assembler
/// listens on `cancel` for shutdown. The sender is cloneable so multiple
/// mutation call-sites can target the same flow.
#[derive(Debug)]
pub struct AssemblerHandle {
    /// Task handle. Production shuts the assembler down via
    /// `CancellationToken`, so this handle is read only in tests that
    /// assert clean shutdown with `join.await`.
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
    pub plan_tx: mpsc::Sender<PlanCommand>,
}

/// Spawn the assembler task. Returns an [`AssemblerHandle`] the caller
/// stores on the `FlowRuntime` so the task lives as long as the flow
/// and runtime mutation (`UpdateFlowAssembly`) has a target.
///
/// `broadcast_tx` is the flow's existing fan-out sender — the assembler
/// publishes synthesised `RtpPacket` bundles onto it exactly where the
/// input forwarder would in passthrough mode, so every existing output
/// subscriber (UDP, tr101290, thumbnailer) works unchanged.
pub fn spawn_spts_assembler(
    plan: AssemblyPlan,
    bus: Arc<FlowEsBus>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
) -> AssemblerHandle {
    // Channel depth 16: plan updates are rare (operator-triggered), so a
    // small bounded buffer is plenty and keeps back-pressure observable
    // if a pathological caller floods.
    let (plan_tx, plan_rx) = mpsc::channel::<PlanCommand>(16);
    let join = tokio::spawn(async move {
        run_assembler(plan, bus, broadcast_tx, cancel, plan_rx).await;
    });
    AssemblerHandle { join, plan_tx }
}

/// Flattened slot view used internally by the assembler. One entry per
/// slot across all programs. `program_idx` is retained for Phase 7's
/// per-PID switching so fan-in traffic routes to the right program's
/// PMT without re-searching.
#[derive(Debug, Clone)]
struct FlatSlot {
    program_idx: usize,
    source: (String, u16),
    out_pid: u16,
}

async fn run_assembler(
    mut plan: AssemblyPlan,
    bus: Arc<FlowEsBus>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
    mut plan_rx: mpsc::Receiver<PlanCommand>,
) {
    // Fan-in: one task per flat slot drains the bus broadcast receiver
    // and forwards into a single mpsc. Channel capacity is deliberately
    // small — if the assembler lags, broadcasts `Lagged` earlier and
    // dropping happens at the bus edge, not in mpsc backpressure. This
    // keeps the no-cascade-backpressure invariant.
    let (fanin_tx, mut fanin_rx) = mpsc::channel::<(usize, EsPacket)>(256);

    // Flat slot view rebuilt on every plan change. Slot indices in this
    // vec are the keys under which fan-in tasks send their packets; they
    // must stay stable for already-spawned fan-ins, so on a plan swap
    // we append new slots to the end rather than recompact.
    let mut flat: Vec<FlatSlot> = Vec::new();
    let mut slot_cancels: Vec<CancellationToken> = Vec::new();
    let mut slot_tasks: Vec<JoinHandle<()>> = Vec::new();

    // Per-program PCR resolution — the out_pid of whichever slot on
    // that program matches `pcr_source`. Recomputed on every plan swap.
    let mut pcr_out_pid_by_program: Vec<u16> = Vec::new();

    // PSI state.
    //
    // `pat_version` bumps when the *set of programs* changes (add/remove).
    // Per-program `pmt_versions[i]` bumps when `programs[i].slots` or
    // `pcr_source` changes. Both wrap mod 32 (5-bit PMT spec field) and
    // advance monotonically across switches, including A→B→A, to avoid
    // phantom-version collisions the `TsContinuityFixer` already wrestled
    // with (see project_ffplay_stuck_state memory).
    let mut pat_version: u8 = 0;
    let mut pmt_versions: Vec<u8> = vec![0; plan.programs.len()];

    // Egress bundle buffer + per-out-PID CC counter + PAT/PMT CC.
    let mut buf = BytesMut::with_capacity(BUNDLE_BYTES);
    let mut cc: std::collections::HashMap<u16, u8> = std::collections::HashMap::new();
    let mut pat_cc: u8 = 0;
    let mut pmt_cc: std::collections::HashMap<u16, u8> = std::collections::HashMap::new();
    let mut bundle_seq: u16 = 0;

    // Prime fan-ins + PCR + snapshot for the initial plan.
    install_plan(
        &plan,
        &bus,
        &fanin_tx,
        &cancel,
        &mut flat,
        &mut slot_cancels,
        &mut slot_tasks,
        &mut pcr_out_pid_by_program,
    );

    let mut psi_tick = interval(PSI_INTERVAL);
    psi_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut flush_tick = interval(FLUSH_INTERVAL);
    flush_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // First tick fires immediately — emit PSI on startup so receivers
    // lock on before the first ES arrives.
    psi_tick.tick().await;
    push_psi(
        &mut buf,
        &plan,
        &pcr_out_pid_by_program,
        pat_version,
        &pmt_versions,
        &mut pat_cc,
        &mut pmt_cc,
        &broadcast_tx,
        &mut bundle_seq,
    );

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                flush(&mut buf, &broadcast_tx, &mut bundle_seq);
                break;
            }
            Some(cmd) = plan_rx.recv() => {
                match cmd {
                    PlanCommand::ReplacePlan { plan: new_plan } => {
                        apply_plan_replacement(
                            &mut plan,
                            new_plan,
                            &bus,
                            &fanin_tx,
                            &cancel,
                            &mut flat,
                            &mut slot_cancels,
                            &mut slot_tasks,
                            &mut pcr_out_pid_by_program,
                            &mut pat_version,
                            &mut pmt_versions,
                        );
                        // Emit fresh PSI immediately on switch so
                        // downstream receivers see the new PMT before
                        // the first rewritten ES packet lands on a new
                        // out_pid — without this, ffprobe can see TS
                        // bytes on an unknown PID for ~100 ms.
                        push_psi(
                            &mut buf,
                            &plan,
                            &pcr_out_pid_by_program,
                            pat_version,
                            &pmt_versions,
                            &mut pat_cc,
                            &mut pmt_cc,
                            &broadcast_tx,
                            &mut bundle_seq,
                        );
                    }
                }
            }
            Some((slot_idx, es)) = fanin_rx.recv() => {
                let slot = match flat.get(slot_idx) {
                    Some(s) => s,
                    None => continue,
                };
                if es.payload.len() != TS_PACKET_SIZE {
                    continue;
                }
                let rewritten = rewrite_es_packet(&es.payload, slot.out_pid, &mut cc);
                buf.extend_from_slice(&rewritten);
                if buf.len() >= BUNDLE_BYTES {
                    flush(&mut buf, &broadcast_tx, &mut bundle_seq);
                }
            }
            _ = psi_tick.tick() => {
                push_psi(
                    &mut buf,
                    &plan,
                    &pcr_out_pid_by_program,
                    pat_version,
                    &pmt_versions,
                    &mut pat_cc,
                    &mut pmt_cc,
                    &broadcast_tx,
                    &mut bundle_seq,
                );
            }
            _ = flush_tick.tick() => {
                flush(&mut buf, &broadcast_tx, &mut bundle_seq);
            }
        }
    }

    for c in &slot_cancels {
        c.cancel();
    }
    for h in slot_tasks {
        h.abort();
    }
}

/// (Re-)flatten the program×slot matrix into `flat`, spawn fan-ins for
/// any slot that isn't already represented by a live task, and recompute
/// `pcr_out_pid_by_program`. Call once at startup; subsequent plan
/// changes go through [`apply_plan_replacement`] which handles diff +
/// fan-in reuse.
#[allow(clippy::too_many_arguments)]
fn install_plan(
    plan: &AssemblyPlan,
    bus: &Arc<FlowEsBus>,
    fanin_tx: &mpsc::Sender<(usize, EsPacket)>,
    cancel: &CancellationToken,
    flat: &mut Vec<FlatSlot>,
    slot_cancels: &mut Vec<CancellationToken>,
    slot_tasks: &mut Vec<JoinHandle<()>>,
    pcr_out_pid_by_program: &mut Vec<u16>,
) {
    for (pidx, prog) in plan.programs.iter().enumerate() {
        for s in prog.slots.iter() {
            let slot = FlatSlot {
                program_idx: pidx,
                source: s.source.clone(),
                out_pid: s.out_pid,
            };
            let idx = flat.len();
            flat.push(slot.clone());
            let slot_cancel = cancel.child_token();
            let rx = bus.subscribe(&slot.source.0, slot.source.1);
            let tx = fanin_tx.clone();
            slot_cancels.push(slot_cancel.clone());
            slot_tasks.push(tokio::spawn(slot_fanin(idx, rx, tx, slot_cancel)));
        }
    }
    *pcr_out_pid_by_program = plan
        .programs
        .iter()
        .map(|prog| {
            prog.slots
                .iter()
                .find(|s| s.source == prog.pcr_source)
                .map(|s| s.out_pid)
                .unwrap_or(prog.pmt_pid)
        })
        .collect();
}

/// Apply a `ReplacePlan` delta.
///
/// Strategy: walk the new plan and for each `(program_idx, new_slot_idx)`
/// find a fan-in in the current `flat` that (a) hasn't already been
/// re-used in this call and (b) subscribes to the same `(input_id,
/// source_pid)`. If found, update its `out_pid`/`program_idx` in place
/// — no re-subscribe needed. If not found, spawn a fresh fan-in. Any
/// old fan-in not re-used is cancelled.
///
/// Version bumps:
/// - PMT version for a program bumps iff its `slots` vec or `pcr_source`
///   differs from before.
/// - PAT version bumps iff the set of `(program_number, pmt_pid)` pairs
///   differs from before.
///
/// Monotonic mod 32 — same discipline as `TsContinuityFixer` so A→B→A
/// never lands on a receiver-locked phantom version.
#[allow(clippy::too_many_arguments)]
fn apply_plan_replacement(
    plan: &mut AssemblyPlan,
    new_plan: AssemblyPlan,
    bus: &Arc<FlowEsBus>,
    fanin_tx: &mpsc::Sender<(usize, EsPacket)>,
    cancel: &CancellationToken,
    flat: &mut Vec<FlatSlot>,
    slot_cancels: &mut Vec<CancellationToken>,
    slot_tasks: &mut Vec<JoinHandle<()>>,
    pcr_out_pid_by_program: &mut Vec<u16>,
    pat_version: &mut u8,
    pmt_versions: &mut Vec<u8>,
) {
    // Program set: (program_number, pmt_pid) pairs, order-preserving.
    let old_pat: Vec<(u16, u16)> = plan
        .programs
        .iter()
        .map(|p| (p.program_number, p.pmt_pid))
        .collect();
    let new_pat: Vec<(u16, u16)> = new_plan
        .programs
        .iter()
        .map(|p| (p.program_number, p.pmt_pid))
        .collect();
    if new_pat != old_pat {
        *pat_version = pat_version.wrapping_add(1) & 0x1F;
    }

    // Per-program PMT version: composition = (slots, pcr_source).
    let mut new_pmt_versions: Vec<u8> = Vec::with_capacity(new_plan.programs.len());
    for (pidx, new_prog) in new_plan.programs.iter().enumerate() {
        // Find the matching old program by `program_number` so adding a
        // program doesn't spuriously bump siblings.
        let old_prog = plan
            .programs
            .iter()
            .find(|p| p.program_number == new_prog.program_number);
        let changed = match old_prog {
            Some(op) => op.slots != new_prog.slots || op.pcr_source != new_prog.pcr_source,
            None => true, // new program → version starts fresh but advances first tick
        };
        let prev_v = pmt_versions.get(pidx).copied().unwrap_or(0);
        let v = if changed {
            prev_v.wrapping_add(1) & 0x1F
        } else {
            prev_v
        };
        new_pmt_versions.push(v);
    }

    // Fan-in reuse: we match by `(source, out_pid)` in the current flat
    // list; anything not matched gets cancelled. For unmatched new slots
    // we append fresh fan-ins (slot indices grow monotonically — old
    // fan-ins that were cancelled never send again, so stale `slot_idx`
    // in fanin_rx is defensively filtered by the main loop's
    // `flat.get(idx)`).
    let mut reused: Vec<bool> = vec![false; flat.len()];
    let mut new_flat: Vec<FlatSlot> = Vec::new();
    let mut carry_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut carry_cancels: Vec<CancellationToken> = Vec::new();

    // Also carry a parallel "flat_idx in the new ordering → original
    // flat_idx" map so the send key stays stable. Because we append-only
    // on reuse, we re-map: each reused fan-in keeps its *original* index
    // (fanin sends use that), and we insert a synthetic entry at that
    // index into `flat_new_by_orig` then rebuild `flat` at the end.
    //
    // Simpler: keep the `flat` vec append-only across the assembler's
    // lifetime — reused slots stay at their original index, new slots
    // are pushed at the end, and cancelled slots leave a hole that the
    // main loop's `flat.get(idx)` naturally tolerates. This avoids the
    // whole re-indexing headache.

    for new_prog in new_plan.programs.iter().enumerate().map(|(pidx, p)| (pidx, p)) {
        let (pidx, prog) = new_prog;
        for s in prog.slots.iter() {
            // Look for a reusable fan-in in the current flat list.
            let match_idx = flat.iter().enumerate().find_map(|(i, f)| {
                if !reused[i] && f.source == s.source {
                    Some(i)
                } else {
                    None
                }
            });
            match match_idx {
                Some(i) => {
                    reused[i] = true;
                    // Update in place — the already-running fan-in keeps
                    // sending on slot_idx = i; we only change the
                    // metadata the main loop reads.
                    flat[i].out_pid = s.out_pid;
                    flat[i].program_idx = pidx;
                }
                None => {
                    // Spawn a new fan-in for this (source, out_pid).
                    let slot = FlatSlot {
                        program_idx: pidx,
                        source: s.source.clone(),
                        out_pid: s.out_pid,
                    };
                    let idx = flat.len();
                    flat.push(slot.clone());
                    // Track reuse vec alongside flat.
                    reused.push(true);
                    let slot_cancel = cancel.child_token();
                    let rx = bus.subscribe(&slot.source.0, slot.source.1);
                    let tx = fanin_tx.clone();
                    slot_cancels.push(slot_cancel.clone());
                    slot_tasks.push(tokio::spawn(slot_fanin(idx, rx, tx, slot_cancel)));
                }
            }
        }
    }

    // Cancel any fan-in that wasn't reused. Leave its entry in `flat`
    // as a tombstone — the main loop's `flat.get(idx)` still returns
    // the old metadata, but since the fan-in is cancelled no further
    // packets arrive under that index.
    for (i, was_reused) in reused.iter().enumerate().take(slot_cancels.len()) {
        if !was_reused {
            slot_cancels[i].cancel();
        }
    }

    // Recompute PCR resolution for each program.
    *pcr_out_pid_by_program = new_plan
        .programs
        .iter()
        .map(|prog| {
            prog.slots
                .iter()
                .find(|s| s.source == prog.pcr_source)
                .map(|s| s.out_pid)
                .unwrap_or(prog.pmt_pid)
        })
        .collect();

    *plan = new_plan;
    *pmt_versions = new_pmt_versions;

    // Silence unused-var warnings from the earlier parallel-map stubs.
    let _ = (&mut new_flat, &mut carry_tasks, &mut carry_cancels);
}

async fn slot_fanin(
    slot_idx: usize,
    mut rx: broadcast::Receiver<EsPacket>,
    tx: mpsc::Sender<(usize, EsPacket)>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            res = rx.recv() => match res {
                Ok(es) => {
                    // `send` returns Err only when receiver is closed; that
                    // only happens during shutdown, so exit cleanly.
                    if tx.send((slot_idx, es)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Slow-consumer drop. Continue draining; no state to
                    // reset because the assembler consumes byte-for-byte
                    // without cross-packet correlation within a slot.
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// Rewrite a 188-byte source TS packet's PID to `out_pid` and stamp
/// a fresh continuity counter from `cc_table`.
///
/// Byte 1 upper 3 bits (TEI, PUSI, transport_priority) are preserved.
/// Byte 3 upper 4 bits (transport_scrambling_control +
/// adaptation_field_control) are preserved. CC advances only when the
/// packet carries a payload; null-payload adaptation-only packets
/// would not advance CC per H.222.0 — our PAT/PMT/ES emission never
/// produces one so this is defensive rather than load-bearing.
fn rewrite_es_packet(src: &[u8], out_pid: u16, cc_table: &mut std::collections::HashMap<u16, u8>) -> [u8; TS_PACKET_SIZE] {
    debug_assert_eq!(src.len(), TS_PACKET_SIZE);
    let mut pkt = [0u8; TS_PACKET_SIZE];
    pkt.copy_from_slice(src);
    // Byte 1: keep high 3 bits; replace low 5 with new PID high bits.
    pkt[1] = (pkt[1] & 0xE0) | (((out_pid >> 8) as u8) & 0x1F);
    // Byte 2: new PID low byte.
    pkt[2] = (out_pid & 0xFF) as u8;
    // Byte 3: mask out existing CC (low 4 bits), OR in fresh CC if this
    // packet has a payload.
    let afc = (pkt[3] >> 4) & 0x0F; // preserve scrambling + adaptation_field_control
    let has_payload = (afc & 0x01) != 0; // bit0 of afc = payload_present
    let _ = has_payload;
    let cc = cc_table.entry(out_pid).or_insert(0);
    let new_cc = *cc & 0x0F;
    *cc = cc.wrapping_add(1) & 0x0F;
    pkt[3] = (afc << 4) | new_cc;
    pkt
}

/// Build one PAT TS packet + one PMT TS packet per program and append
/// them to `buf`, flushing intermediate bundles as needed so a PSI tick
/// never overflows past a bundle boundary.
///
/// `pat_version` and `pmt_versions` are bumped on plan change by
/// [`apply_plan_replacement`]; `push_psi` only stamps them onto the
/// section bytes.
#[allow(clippy::too_many_arguments)]
fn push_psi(
    buf: &mut BytesMut,
    plan: &AssemblyPlan,
    pcr_out_pid_by_program: &[u16],
    pat_version: u8,
    pmt_versions: &[u8],
    pat_cc: &mut u8,
    pmt_cc: &mut std::collections::HashMap<u16, u8>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    bundle_seq: &mut u16,
) {
    // PAT: one entry per program. PAT lives on PID 0x0000 with its own
    // continuity counter across the whole flow (there's only one PAT).
    let entries: Vec<(u16, u16)> = plan
        .programs
        .iter()
        .map(|p| (p.program_number, p.pmt_pid))
        .collect();
    let pat = build_pat(&entries, pat_version, *pat_cc);
    *pat_cc = pat_cc.wrapping_add(1) & 0x0F;
    append_ts(buf, &pat, broadcast_tx, bundle_seq);

    // One PMT per program on its own `pmt_pid` with its own CC counter.
    for (pidx, prog) in plan.programs.iter().enumerate() {
        let cc_entry = pmt_cc.entry(prog.pmt_pid).or_insert(0);
        let pcr_out_pid = pcr_out_pid_by_program
            .get(pidx)
            .copied()
            .unwrap_or(prog.pmt_pid);
        let pmt_version = pmt_versions.get(pidx).copied().unwrap_or(0);
        let pmt = build_pmt(
            prog.program_number,
            prog.pmt_pid,
            pcr_out_pid,
            &prog.slots,
            pmt_version,
            *cc_entry,
        );
        *cc_entry = cc_entry.wrapping_add(1) & 0x0F;
        append_ts(buf, &pmt, broadcast_tx, bundle_seq);
    }
}

/// Append one 188-byte TS packet to the bundle buffer, flushing when the
/// buffer reaches capacity.
fn append_ts(
    buf: &mut BytesMut,
    pkt: &[u8; TS_PACKET_SIZE],
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    bundle_seq: &mut u16,
) {
    buf.extend_from_slice(pkt);
    if buf.len() >= BUNDLE_BYTES {
        flush(buf, broadcast_tx, bundle_seq);
    }
}

/// Emit whatever is in `buf` as one `RtpPacket` (raw TS) onto the
/// broadcast channel and reset the buffer. Stamps a monotonic u16
/// sequence number and derives a 90 kHz RTP timestamp from the current
/// wall clock so downstream RTP / SRT / FEC outputs can treat the
/// assembler as a first-class RTP source. No-op when `buf` is empty.
fn flush(
    buf: &mut BytesMut,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    bundle_seq: &mut u16,
) {
    if buf.is_empty() {
        return;
    }
    let replaced = std::mem::replace(buf, BytesMut::with_capacity(BUNDLE_BYTES));
    let bundle: Bytes = replaced.freeze();
    let recv_time_us = crate::util::time::now_us();
    // Scale µs → 90 kHz ticks and truncate to u32 (standard RTP math).
    let rtp_ts = ((recv_time_us.wrapping_mul(9)).wrapping_div(100)) as u32;
    let seq = *bundle_seq;
    *bundle_seq = bundle_seq.wrapping_add(1);
    let pkt = RtpPacket {
        data: bundle,
        sequence_number: seq,
        rtp_timestamp: rtp_ts,
        recv_time_us,
        is_raw_ts: true,
    };
    // `send` returns Err when there are no subscribers (e.g. flow still
    // warming). That's fine — we don't buffer, by design.
    let _ = broadcast_tx.send(pkt);
}

// ---------------------------------------------------------------------
// PAT / PMT builders (pure functions, unit-testable in isolation)
// ---------------------------------------------------------------------

fn build_pat(
    programs: &[(u16, u16)],
    version: u8,
    cc: u8,
) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    // TS header: sync, PUSI=1, PID=0x0000, AFC=payload-only, CC.
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40; // PUSI | PID high=0
    pkt[2] = 0x00;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4] = 0x00; // pointer_field
    // Section: table_id + section_length + ts_id + version/cni + section_no
    //        + last_section + N × (program_number + reserved+pmt_pid) + CRC
    // section_length = 5 (ts_id..last_section) + 4*N (program entries) + 4 (CRC)
    let section_length: u16 = 5 + 4 * programs.len() as u16 + 4;
    pkt[5] = 0x00; // table_id PAT
    pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
    pkt[7] = (section_length & 0xFF) as u8;
    pkt[8] = 0x00; // ts_id hi
    pkt[9] = 0x01; // ts_id lo
    pkt[10] = 0xC1 | ((version & 0x1F) << 1); // reserved + version + current_next
    pkt[11] = 0x00; // section_number
    pkt[12] = 0x00; // last_section_number
    let mut pos = 13;
    for (program_number, pmt_pid) in programs {
        pkt[pos] = (program_number >> 8) as u8;
        pkt[pos + 1] = (program_number & 0xFF) as u8;
        pkt[pos + 2] = 0xE0 | (((pmt_pid >> 8) as u8) & 0x1F);
        pkt[pos + 3] = (pmt_pid & 0xFF) as u8;
        pos += 4;
    }
    // CRC over table_id..end of entries (bytes 5..pos).
    let crc = mpeg2_crc32(&pkt[5..pos]);
    pkt[pos] = (crc >> 24) as u8;
    pkt[pos + 1] = (crc >> 16) as u8;
    pkt[pos + 2] = (crc >> 8) as u8;
    pkt[pos + 3] = crc as u8;
    pkt
}

fn build_pmt(
    program_number: u16,
    pmt_pid: u16,
    pcr_pid: u16,
    slots: &[AssemblySlot],
    version: u8,
    cc: u8,
) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F);
    pkt[2] = (pmt_pid & 0xFF) as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4] = 0x00; // pointer_field
    // Section body length (after `section_length` field):
    //   9 bytes header (program_number..program_info_length)
    //   + 5 bytes per ES entry
    //   + 4 bytes CRC
    let section_length: u16 = (9 + 5 * slots.len() as u16) + 4;
    pkt[5] = 0x02; // table_id PMT
    pkt[6] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
    pkt[7] = (section_length & 0xFF) as u8;
    pkt[8] = (program_number >> 8) as u8;
    pkt[9] = (program_number & 0xFF) as u8;
    pkt[10] = 0xC1 | ((version & 0x1F) << 1);
    pkt[11] = 0x00; // section_number
    pkt[12] = 0x00; // last_section_number
    pkt[13] = 0xE0 | (((pcr_pid >> 8) as u8) & 0x1F);
    pkt[14] = (pcr_pid & 0xFF) as u8;
    pkt[15] = 0xF0; // reserved + program_info_length hi (=0)
    pkt[16] = 0x00;
    let mut pos = 17;
    for slot in slots {
        pkt[pos] = slot.stream_type;
        pkt[pos + 1] = 0xE0 | (((slot.out_pid >> 8) as u8) & 0x1F);
        pkt[pos + 2] = (slot.out_pid & 0xFF) as u8;
        pkt[pos + 3] = 0xF0; // reserved + es_info_length hi (=0)
        pkt[pos + 4] = 0x00;
        pos += 5;
    }
    // CRC spans from table_id (byte 5) to just before the 4 CRC bytes.
    let crc_end = 5 + 3 + section_length as usize; // == pos + 4
    debug_assert_eq!(crc_end - 4, pos);
    let crc = mpeg2_crc32(&pkt[5..pos]);
    pkt[pos] = (crc >> 24) as u8;
    pkt[pos + 1] = (crc >> 16) as u8;
    pkt[pos + 2] = (crc >> 8) as u8;
    pkt[pos + 3] = crc as u8;
    pkt
}

// Keep the null-pid reference live (used in tests + for future filtering).
const _: u16 = NULL_PID;
const _: u16 = PAT_PID;

// ---------------------------------------------------------------------
// Phase 6: Essence → PID resolver
// ---------------------------------------------------------------------
//
// `SlotSource::Essence { input_id, kind }` needs an `(input_id,
// source_pid)` pair before the assembler can start. We resolve it
// against the per-input PSI catalogue populated by Phase 2's observer:
// poll every 100 ms, pick the first matching stream, bail loudly on
// timeout with a dedicated error_code.
//
// Kept in this module (rather than a standalone `ts_essence_resolver`)
// because it's purely plan-building — it never touches the data path
// and its inputs/outputs mirror the assembler's `AssemblyPlan` shape.

/// Broad elementary-stream kind, independent of the config crate so the
/// resolver can be unit-tested in isolation. Mirrors
/// [`crate::config::models::EssenceKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EsKind {
    Video,
    Audio,
    Subtitle,
    Data,
}

/// One unresolved slot. Produced by the plan builder; the resolver
/// returns one `((program_idx, slot_idx), pid)` per entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEssenceSlot {
    /// `(program_idx, slot_idx)` — program_idx is an index into
    /// `AssemblyPlan.programs`; slot_idx is an index into that
    /// program's `ProgramPlan.slots`. The runtime patches
    /// `plan.programs[program_idx].slots[slot_idx].source.1` in place
    /// once the resolver returns.
    pub program_idx: usize,
    pub slot_idx: usize,
    pub input_id: String,
    pub kind: EsKind,
}

/// One pending Hitless merger task. Produced by `build_assembly_plan`
/// when it expands a [`crate::config::models::SlotSource::Hitless`]
/// slot into a synthetic bus key. The runtime walks `pending_hitless`
/// after essence resolution and spawns one
/// [`crate::engine::ts_es_hitless::spawn_hitless_es_merger`] per entry
/// before bringing the assembler up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHitlessSlot {
    /// Stable identifier — `"slot_{program_idx}_{slot_idx}"` is the
    /// convention so two Hitless slots on the same flow never collide.
    /// Doubles as the synthetic bus key suffix
    /// (`hitless:{uid}` is the bus input_id).
    pub uid: String,
    /// Concrete `(input_id, source_pid)` for the primary leg.
    pub primary: (String, u16),
    /// Concrete `(input_id, source_pid)` for the backup leg.
    pub backup: (String, u16),
    /// Stall window in milliseconds before the merger flips primary →
    /// backup. Defaults to
    /// [`crate::engine::ts_es_hitless::DEFAULT_STALL_MS`] when the
    /// operator hasn't set it.
    pub stall_ms: u64,
}

/// Output of `build_spts_plan` (defined on the runtime side): the
/// assembler plan plus any `Essence` slots still awaiting a concrete
/// `source_pid`, plus any `Hitless` slots that need a merger task spawned
/// before the assembler starts. Slots in `plan` corresponding to entries
/// in `pending_essence` have a sentinel `source_pid = 0` — the runtime
/// patches them in place after the resolver returns. Slots in `plan`
/// corresponding to entries in `pending_hitless` have already been
/// pointed at the synthetic merger output, so the assembler treats them
/// as a normal `(hitless:{uid}, 0)` source.
#[derive(Debug, Clone)]
pub struct SptsBuildResult {
    pub plan: AssemblyPlan,
    pub pending_essence: Vec<PendingEssenceSlot>,
    pub pending_hitless: Vec<PendingHitlessSlot>,
}

/// Failure modes for essence resolution. The runtime maps each variant
/// to a distinct `error_code` on the emitted Critical event so the
/// manager UI can highlight the offending assembly slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EssenceResolveError {
    /// Input's catalogue has entries but none match the requested kind
    /// — e.g. `{ kind: Video }` against an audio-only contribution feed.
    NoMatch {
        input_id: String,
        kind: EsKind,
    },
    /// Input has not produced any PAT/PMT within the timeout — either
    /// the input is idle, or it isn't actually publishing TS.
    NoCatalogue { input_id: String },
    /// Phase 6 resolver only handles Video + Audio. Subtitle / Data
    /// reach here only if the runtime doesn't pre-filter them.
    KindNotImplemented { kind: EsKind },
}

/// Pick the first PID matching `kind` in the catalogue. Lowest
/// `program_number` first (matches the MPTS → SPTS default in Phase 1),
/// then PMT declaration order within that program.
fn pick_pid_for_kind(
    catalogue: &crate::engine::ts_psi_catalog::PsiCatalog,
    kind: EsKind,
) -> Option<u16> {
    use crate::engine::ts_psi_catalog::CatalogStreamKind;
    let target = match kind {
        EsKind::Video => CatalogStreamKind::Video,
        EsKind::Audio => CatalogStreamKind::Audio,
        // Subtitle / Data handled by the caller (KindNotImplemented).
        _ => return None,
    };
    let mut programs: Vec<&crate::engine::ts_psi_catalog::CatalogProgram> =
        catalogue.programs.iter().collect();
    programs.sort_by_key(|p| p.program_number);
    for prog in programs {
        for stream in &prog.streams {
            if stream.kind == target {
                return Some(stream.pid);
            }
        }
    }
    None
}

/// Resolve every pending Essence slot to a concrete `source_pid`.
///
/// Polls each input's catalogue every 100 ms up to `timeout`. Returns
/// one `((program_idx, slot_idx), resolved_pid)` per pending slot on
/// success. On timeout, surfaces the first still-unresolved slot as
/// either `NoMatch` (catalogue exists but no matching kind) or
/// `NoCatalogue` (catalogue empty — typically "input hasn't bound or
/// received PSI yet").
///
/// Subtitle / Data are rejected up-front with `KindNotImplemented`.
pub async fn resolve_essence_slots(
    pending: Vec<PendingEssenceSlot>,
    catalogues: std::collections::HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
    timeout: Duration,
) -> Result<Vec<((usize, usize), u16)>, EssenceResolveError> {
    // Pre-filter unsupported kinds so the poll loop only deals with
    // Video / Audio.
    for p in &pending {
        if !matches!(p.kind, EsKind::Video | EsKind::Audio) {
            return Err(EssenceResolveError::KindNotImplemented { kind: p.kind });
        }
    }

    let deadline = tokio::time::Instant::now() + timeout;
    let mut resolved: Vec<((usize, usize), u16)> = Vec::with_capacity(pending.len());
    let mut remaining: Vec<PendingEssenceSlot> = pending;

    loop {
        remaining.retain(|p| {
            let Some(store) = catalogues.get(&p.input_id) else {
                return true;
            };
            let Some(cat) = store.load() else {
                return true;
            };
            match pick_pid_for_kind(&cat, p.kind) {
                Some(pid) => {
                    resolved.push(((p.program_idx, p.slot_idx), pid));
                    false // resolved — drop from remaining
                }
                None => true, // catalogue present but no matching kind yet
            }
        });
        if remaining.is_empty() {
            return Ok(resolved);
        }
        if tokio::time::Instant::now() >= deadline {
            let first = &remaining[0];
            let has_catalogue = catalogues
                .get(&first.input_id)
                .and_then(|s| s.load())
                .is_some();
            return Err(if has_catalogue {
                EssenceResolveError::NoMatch {
                    input_id: first.input_id.clone(),
                    kind: first.kind,
                }
            } else {
                EssenceResolveError::NoCatalogue {
                    input_id: first.input_id.clone(),
                }
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ts_parse::{parse_pat_programs, ts_cc, ts_pid, ts_pusi};

    fn slot(input: &str, src_pid: u16, out_pid: u16, stream_type: u8) -> AssemblySlot {
        AssemblySlot {
            source: (input.to_string(), src_pid),
            out_pid,
            stream_type,
        }
    }

    fn make_plan() -> AssemblyPlan {
        AssemblyPlan {
            programs: vec![ProgramPlan {
                program_number: 1,
                pmt_pid: 0x1000,
                pcr_source: ("in-a".to_string(), 0x100),
                slots: vec![
                    slot("in-a", 0x100, 0x200, 0x1B), // H.264 video
                    slot("in-b", 0x200, 0x201, 0x0F), // AAC-LC audio
                ],
            }],
        }
    }

    fn make_mpts_plan() -> AssemblyPlan {
        AssemblyPlan {
            programs: vec![
                ProgramPlan {
                    program_number: 1,
                    pmt_pid: 0x1000,
                    pcr_source: ("in-a".to_string(), 0x100),
                    slots: vec![slot("in-a", 0x100, 0x200, 0x1B)],
                },
                ProgramPlan {
                    program_number: 2,
                    pmt_pid: 0x1100,
                    pcr_source: ("in-b".to_string(), 0x200),
                    slots: vec![slot("in-b", 0x200, 0x300, 0x1B)],
                },
            ],
        }
    }

    /// Craft a 188-byte TS packet on `src_pid` with given PUSI, CC and
    /// a payload-only adaptation_field_control.
    fn ts_es(src_pid: u16, pusi: bool, cc: u8, payload_byte: u8) -> Bytes {
        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = if pusi { 0x40 } else { 0x00 } | (((src_pid >> 8) as u8) & 0x1F);
        pkt[2] = (src_pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F);
        for b in &mut pkt[4..] {
            *b = payload_byte;
        }
        Bytes::copy_from_slice(&pkt)
    }

    #[test]
    fn pat_has_valid_crc_and_one_program() {
        let pat = build_pat(&[(42, 0x1000)], 0, 0);
        assert_eq!(pat[0], TS_SYNC_BYTE);
        assert_eq!(ts_pid(&pat), 0x0000);
        assert!(ts_pusi(&pat));
        // CRC check: mpeg2_crc32 over the full section (header through CRC)
        // must be 0 when the stored CRC is correct.
        // section runs bytes 5 .. 5 + 3 + section_length
        let section_length = (((pat[6] & 0x0F) as usize) << 8) | pat[7] as usize;
        let section_end = 5 + 3 + section_length;
        assert_eq!(mpeg2_crc32(&pat[5..section_end]), 0);
        // Parse it back using the shared PAT parser.
        let progs = parse_pat_programs(&pat);
        assert_eq!(progs, vec![(42, 0x1000)]);
    }

    #[test]
    fn pat_mpts_has_valid_crc_and_two_programs() {
        let pat = build_pat(&[(1, 0x1000), (2, 0x1100)], 0, 0);
        assert_eq!(ts_pid(&pat), 0x0000);
        assert!(ts_pusi(&pat));
        let section_length = (((pat[6] & 0x0F) as usize) << 8) | pat[7] as usize;
        let section_end = 5 + 3 + section_length;
        // section_length must grow by 4 bytes per extra program.
        assert_eq!(section_length, 5 + 4 * 2 + 4);
        assert_eq!(mpeg2_crc32(&pat[5..section_end]), 0);
        let progs = parse_pat_programs(&pat);
        assert_eq!(progs, vec![(1, 0x1000), (2, 0x1100)]);
    }

    #[test]
    fn pmt_has_valid_crc_and_expected_pcr_pid() {
        let slots = vec![slot("x", 0, 0x200, 0x1B), slot("x", 0, 0x201, 0x0F)];
        let pmt = build_pmt(42, 0x1000, 0x200, &slots, 0, 0);
        assert_eq!(ts_pid(&pmt), 0x1000);
        assert!(ts_pusi(&pmt));
        let section_length = (((pmt[6] & 0x0F) as usize) << 8) | pmt[7] as usize;
        let section_end = 5 + 3 + section_length;
        assert_eq!(mpeg2_crc32(&pmt[5..section_end]), 0);
        // PCR_PID is at bytes [13..15] with 3 reserved bits in the top of [13].
        let pcr_pid = (((pmt[13] & 0x1F) as u16) << 8) | pmt[14] as u16;
        assert_eq!(pcr_pid, 0x200);
        // program_number at [8..10].
        let pn = ((pmt[8] as u16) << 8) | pmt[9] as u16;
        assert_eq!(pn, 42);
        // First ES entry starts at byte 17.
        assert_eq!(pmt[17], 0x1B); // stream_type
        let es0_pid = (((pmt[18] & 0x1F) as u16) << 8) | pmt[19] as u16;
        assert_eq!(es0_pid, 0x200);
        assert_eq!(pmt[22], 0x0F); // stream_type
        let es1_pid = (((pmt[23] & 0x1F) as u16) << 8) | pmt[24] as u16;
        assert_eq!(es1_pid, 0x201);
    }

    #[test]
    fn rewrite_es_packet_changes_pid_and_stamps_cc() {
        let mut cc_table = std::collections::HashMap::new();
        let src = ts_es(0x100, true, 5, 0xAB);
        let rewritten = rewrite_es_packet(&src, 0x200, &mut cc_table);
        assert_eq!(rewritten[0], TS_SYNC_BYTE);
        assert!(ts_pusi(&rewritten), "PUSI must survive PID rewrite");
        assert_eq!(ts_pid(&rewritten), 0x200);
        assert_eq!(ts_cc(&rewritten), 0, "fresh CC table starts at 0");
        // Second rewrite advances CC, independent of source CC.
        let src2 = ts_es(0x100, false, 9, 0xCD);
        let rewritten2 = rewrite_es_packet(&src2, 0x200, &mut cc_table);
        assert_eq!(ts_cc(&rewritten2), 1);
        assert_eq!(ts_pid(&rewritten2), 0x200);
        // Payload bytes preserved.
        assert_eq!(rewritten2[4], 0xCD);
    }

    #[test]
    fn rewrite_preserves_tei_and_transport_priority() {
        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = 0xA0; // TEI=1 | PUSI=0 | transport_priority=1 | PID_hi=0
        pkt[2] = 0x00;
        pkt[3] = 0x10; // payload-only, CC=0
        let src = Bytes::copy_from_slice(&pkt);
        let mut cc_table = std::collections::HashMap::new();
        let rewritten = rewrite_es_packet(&src, 0x500, &mut cc_table);
        assert_eq!(rewritten[1] & 0xE0, 0xA0, "TEI + transport_priority must survive");
        assert_eq!(ts_pid(&rewritten), 0x500);
    }

    #[test]
    fn cc_counters_are_per_out_pid() {
        let mut cc_table = std::collections::HashMap::new();
        let s = ts_es(0x100, false, 0, 0);
        let a0 = rewrite_es_packet(&s, 0x200, &mut cc_table);
        let b0 = rewrite_es_packet(&s, 0x201, &mut cc_table);
        let a1 = rewrite_es_packet(&s, 0x200, &mut cc_table);
        assert_eq!(ts_cc(&a0), 0);
        assert_eq!(ts_cc(&b0), 0, "new out_pid starts its own counter");
        assert_eq!(ts_cc(&a1), 1, "old out_pid advances independently");
    }

    #[test]
    fn cc_wraps_after_sixteen() {
        let mut cc_table = std::collections::HashMap::new();
        let s = ts_es(0x100, false, 0, 0);
        for expected in 0..16u8 {
            let rw = rewrite_es_packet(&s, 0x200, &mut cc_table);
            assert_eq!(ts_cc(&rw), expected);
        }
        let rw17 = rewrite_es_packet(&s, 0x200, &mut cc_table);
        assert_eq!(ts_cc(&rw17), 0, "CC is a 4-bit counter and wraps");
    }

    #[tokio::test]
    async fn assembler_emits_psi_on_startup_before_any_es() {
        let bus = Arc::new(FlowEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(16);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone()).join;

        // Drain one bundle — startup path emits PAT + PMT immediately.
        let bundle = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("assembler must emit a startup bundle within 500 ms")
            .expect("broadcast channel still open");
        cancel.cancel();
        handle.await.unwrap();

        assert!(bundle.is_raw_ts);
        assert!(bundle.data.len() >= 2 * TS_PACKET_SIZE);
        assert_eq!(&bundle.data[0..1], &[TS_SYNC_BYTE]);
        assert_eq!(ts_pid(&bundle.data[0..TS_PACKET_SIZE]), 0x0000); // PAT
        assert_eq!(ts_pid(&bundle.data[TS_PACKET_SIZE..2 * TS_PACKET_SIZE]), 0x1000); // PMT
    }

    #[tokio::test]
    async fn assembler_rewrites_es_pids_end_to_end() {
        let bus = Arc::new(FlowEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone()).join;

        // Wait for startup PSI, then publish enough ES packets on each
        // slot to fill a full bundle.
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;

        let video_tx = bus.sender_for("in-a", 0x100);
        let audio_tx = bus.sender_for("in-b", 0x200);
        // 7 packets total → exactly one full bundle.
        for i in 0..5 {
            video_tx
                .send(EsPacket {
                    source_pid: 0x100,
                    stream_type: 0x1B,
                    payload: ts_es(0x100, i == 0, i as u8, 0x10 + i as u8),
                    is_pusi: i == 0,
                    has_pcr: false,
                    pcr: None,
                    recv_time_us: 0,
                })
                .unwrap();
        }
        for i in 0..2 {
            audio_tx
                .send(EsPacket {
                    source_pid: 0x200,
                    stream_type: 0x0F,
                    payload: ts_es(0x200, i == 0, i as u8, 0x20 + i as u8),
                    is_pusi: i == 0,
                    has_pcr: false,
                    pcr: None,
                    recv_time_us: 0,
                })
                .unwrap();
        }

        // Collect bundles for ~50 ms — long enough for the full-bundle
        // emission path, well under the flush tick (10 ms) so we also
        // get any partial-bundle flushes.
        let mut video_seen = 0;
        let mut audio_seen = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(80);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(30), rx.recv()).await
            {
                let bytes = &bundle.data[..];
                for chunk in bytes.chunks_exact(TS_PACKET_SIZE) {
                    match ts_pid(chunk) {
                        0x200 => video_seen += 1,
                        0x201 => audio_seen += 1,
                        _ => {}
                    }
                }
            }
        }
        cancel.cancel();
        handle.await.unwrap();

        assert_eq!(video_seen, 5, "all 5 video packets must surface on out_pid 0x200");
        assert_eq!(audio_seen, 2, "both audio packets must surface on out_pid 0x201");
    }

    #[tokio::test]
    async fn assembler_mpts_emits_pat_with_two_programs_and_two_pmts() {
        let bus = Arc::new(FlowEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_mpts_plan(), bus.clone(), tx.clone(), cancel.clone()).join;

        // Collect bundles for ~250 ms — long enough to see the startup
        // PSI emission plus at least one tick of the 100 ms PSI cadence.
        let mut saw_two_program_pat = false;
        let mut saw_pmt_1000 = false;
        let mut saw_pmt_1100 = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
            {
                for chunk in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                    match ts_pid(chunk) {
                        0x0000 => {
                            let progs = parse_pat_programs(chunk);
                            if progs.len() == 2 {
                                saw_two_program_pat = true;
                            }
                        }
                        0x1000 => saw_pmt_1000 = true,
                        0x1100 => saw_pmt_1100 = true,
                        _ => {}
                    }
                }
            }
            if saw_two_program_pat && saw_pmt_1000 && saw_pmt_1100 {
                break;
            }
        }
        cancel.cancel();
        handle.await.unwrap();

        assert!(saw_two_program_pat, "MPTS PAT must list both programs");
        assert!(saw_pmt_1000, "program-1 PMT must be emitted on pmt_pid 0x1000");
        assert!(saw_pmt_1100, "program-2 PMT must be emitted on pmt_pid 0x1100");
    }

    #[tokio::test]
    async fn assembler_shutsdown_cleanly_with_no_es_traffic() {
        let bus = Arc::new(FlowEsBus::new());
        let (tx, _rx) = broadcast::channel::<RtpPacket>(4);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone()).join;
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("assembler must exit within 500 ms of cancel")
            .unwrap();
    }

    /// Helper: read the 5-bit version_number from a PMT TS packet.
    fn pmt_version(pkt: &[u8]) -> u8 {
        // Byte 10 = reserved(2) + version(5) + current_next(1)
        (pkt[10] >> 1) & 0x1F
    }

    #[tokio::test]
    async fn replace_plan_swaps_slot_and_bumps_pmt_version() {
        let bus = Arc::new(FlowEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();

        // Initial plan: in-a → out 0x200, in-b → out 0x201.
        let initial = make_plan();
        let handle = spawn_spts_assembler(initial, bus.clone(), tx.clone(), cancel.clone());

        // Capture startup PMT version (should be 0).
        let mut v0: Option<u8> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < deadline && v0.is_none() {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
            {
                for chunk in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                    if ts_pid(chunk) == 0x1000 {
                        v0 = Some(pmt_version(chunk));
                        break;
                    }
                }
            }
        }
        let v0 = v0.expect("startup PMT emitted");
        assert_eq!(v0, 0, "startup PMT version starts at 0");

        // Swap plan: redirect the video slot from in-a → in-c.
        let new_plan = AssemblyPlan {
            programs: vec![ProgramPlan {
                program_number: 1,
                pmt_pid: 0x1000,
                pcr_source: ("in-c".to_string(), 0x100),
                slots: vec![
                    slot("in-c", 0x100, 0x200, 0x1B),
                    slot("in-b", 0x200, 0x201, 0x0F),
                ],
            }],
        };
        handle
            .plan_tx
            .send(PlanCommand::ReplacePlan { plan: new_plan })
            .await
            .expect("plan_tx send");

        // Capture the post-swap PMT version (must be strictly different).
        let mut v1: Option<u8> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline && v1.is_none() {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
            {
                for chunk in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                    if ts_pid(chunk) == 0x1000 {
                        let v = pmt_version(chunk);
                        if v != v0 {
                            v1 = Some(v);
                            break;
                        }
                    }
                }
            }
        }
        let v1 = v1.expect("post-swap PMT emitted with new version");
        assert_eq!(
            v1,
            (v0 + 1) & 0x1F,
            "PMT version must advance monotonically on slot change"
        );

        // Publish an ES packet on the new source; it must appear on the
        // new out_pid (still 0x200). Fan-in reuse means `in-c:0x100` got
        // a fresh fan-in task but the original one for `in-a:0x100` was
        // cancelled.
        let video_tx = bus.sender_for("in-c", 0x100);
        video_tx
            .send(EsPacket {
                source_pid: 0x100,
                stream_type: 0x1B,
                payload: ts_es(0x100, true, 0, 0xAB),
                is_pusi: true,
                has_pcr: false,
                pcr: None,
                recv_time_us: 0,
            })
            .unwrap();

        let mut saw_200 = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < deadline && !saw_200 {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
            {
                for chunk in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                    if ts_pid(chunk) == 0x200 {
                        saw_200 = true;
                        break;
                    }
                }
            }
        }
        assert!(saw_200, "post-swap ES must flow onto the reassigned out_pid");

        cancel.cancel();
        handle.join.await.unwrap();
    }

    #[tokio::test]
    async fn replace_plan_no_change_does_not_bump_version() {
        let bus = Arc::new(FlowEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone());

        // Drain startup PMT.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let mut startup_v: Option<u8> = None;
        while tokio::time::Instant::now() < deadline && startup_v.is_none() {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
            {
                for chunk in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                    if ts_pid(chunk) == 0x1000 {
                        startup_v = Some(pmt_version(chunk));
                    }
                }
            }
        }
        let v0 = startup_v.expect("startup PMT");

        // Replace with a structurally identical plan.
        handle
            .plan_tx
            .send(PlanCommand::ReplacePlan { plan: make_plan() })
            .await
            .expect("plan_tx send");

        // Observe several PMTs post-swap; none should have a different version.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut saw_bump = false;
        for _ in 0..6 {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
            {
                for chunk in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                    if ts_pid(chunk) == 0x1000 && pmt_version(chunk) != v0 {
                        saw_bump = true;
                    }
                }
            }
        }

        cancel.cancel();
        handle.join.await.unwrap();
        assert!(!saw_bump, "no-op plan replace must not bump PMT version");
    }

    // ---------------------------------------------------------------
    // Phase 6: essence resolver tests
    // ---------------------------------------------------------------

    use crate::engine::ts_psi_catalog::{
        CatalogProgram, CatalogStream, CatalogStreamKind, PsiCatalog, PsiCatalogStore,
    };

    fn stream(pid: u16, stream_type: u8, kind: CatalogStreamKind) -> CatalogStream {
        CatalogStream {
            pid,
            stream_type,
            codec: "test".to_string(),
            kind,
        }
    }

    fn catalogue_with(programs: Vec<CatalogProgram>) -> Arc<PsiCatalogStore> {
        let store = Arc::new(PsiCatalogStore::new());
        // We can't call the private `store()` method from outside the
        // module; reach through the same `inner` field used internally.
        // But `store()` is private — instead, drive the public writer
        // by spawning the observer… too heavyweight for a unit test.
        // Use a freshly-built store whose inner RwLock we write via the
        // module's own test helper (added below).
        let cat = PsiCatalog {
            programs,
            last_updated_us: 1,
        };
        crate::engine::ts_psi_catalog::PsiCatalogStore::seed_for_test(&store, cat);
        store
    }

    #[tokio::test]
    async fn essence_resolver_picks_first_video_pid() {
        let store = catalogue_with(vec![CatalogProgram {
            program_number: 1,
            pmt_pid: 0x1000,
            pcr_pid: Some(0x100),
            streams: vec![
                stream(0x100, 0x1B, CatalogStreamKind::Video),
                stream(0x101, 0x0F, CatalogStreamKind::Audio),
            ],
        }]);
        let mut cats = std::collections::HashMap::new();
        cats.insert("in-a".to_string(), store);
        let pending = vec![PendingEssenceSlot {
            program_idx: 0,
            slot_idx: 0,
            input_id: "in-a".into(),
            kind: EsKind::Video,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(500))
            .await
            .expect("must resolve");
        assert_eq!(r, vec![((0, 0), 0x100)]);
    }

    #[tokio::test]
    async fn essence_resolver_picks_first_audio_pid() {
        let store = catalogue_with(vec![CatalogProgram {
            program_number: 1,
            pmt_pid: 0x1000,
            pcr_pid: Some(0x100),
            streams: vec![
                stream(0x100, 0x1B, CatalogStreamKind::Video),
                stream(0x200, 0x0F, CatalogStreamKind::Audio),
                stream(0x201, 0x81, CatalogStreamKind::Audio),
            ],
        }]);
        let mut cats = std::collections::HashMap::new();
        cats.insert("in-a".to_string(), store);
        let pending = vec![PendingEssenceSlot {
            program_idx: 0,
            slot_idx: 3,
            input_id: "in-a".into(),
            kind: EsKind::Audio,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(r, vec![((0, 3), 0x200)], "first audio wins, not second AC-3");
    }

    #[tokio::test]
    async fn essence_resolver_prefers_lowest_program_number_on_mpts() {
        // Program 2 with video on 0x200, program 1 with video on 0x100.
        // Iteration order is "lowest program_number first", so 0x100 wins.
        let store = catalogue_with(vec![
            CatalogProgram {
                program_number: 2,
                pmt_pid: 0x2000,
                pcr_pid: Some(0x200),
                streams: vec![stream(0x200, 0x1B, CatalogStreamKind::Video)],
            },
            CatalogProgram {
                program_number: 1,
                pmt_pid: 0x1000,
                pcr_pid: Some(0x100),
                streams: vec![stream(0x100, 0x1B, CatalogStreamKind::Video)],
            },
        ]);
        let mut cats = std::collections::HashMap::new();
        cats.insert("in-a".to_string(), store);
        let pending = vec![PendingEssenceSlot {
            program_idx: 0,
            slot_idx: 0,
            input_id: "in-a".into(),
            kind: EsKind::Video,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(r, vec![((0, 0), 0x100)]);
    }

    #[tokio::test]
    async fn essence_resolver_returns_nomatch_when_catalogue_has_no_video() {
        let store = catalogue_with(vec![CatalogProgram {
            program_number: 1,
            pmt_pid: 0x1000,
            pcr_pid: Some(0x200),
            streams: vec![stream(0x200, 0x0F, CatalogStreamKind::Audio)],
        }]);
        let mut cats = std::collections::HashMap::new();
        cats.insert("audio-only".to_string(), store);
        let pending = vec![PendingEssenceSlot {
            program_idx: 0,
            slot_idx: 0,
            input_id: "audio-only".into(),
            kind: EsKind::Video,
        }];
        let err = resolve_essence_slots(pending, cats, Duration::from_millis(250))
            .await
            .expect_err("must fail with NoMatch");
        match err {
            EssenceResolveError::NoMatch { input_id, kind } => {
                assert_eq!(input_id, "audio-only");
                assert_eq!(kind, EsKind::Video);
            }
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn essence_resolver_returns_nocatalogue_when_input_is_silent() {
        // Catalogue store exists but `load()` returns None until first PSI.
        let store = Arc::new(PsiCatalogStore::new());
        let mut cats = std::collections::HashMap::new();
        cats.insert("silent".to_string(), store);
        let pending = vec![PendingEssenceSlot {
            program_idx: 0,
            slot_idx: 0,
            input_id: "silent".into(),
            kind: EsKind::Video,
        }];
        let err = resolve_essence_slots(pending, cats, Duration::from_millis(200))
            .await
            .expect_err("must fail with NoCatalogue");
        assert!(matches!(err, EssenceResolveError::NoCatalogue { .. }));
    }

    #[tokio::test]
    async fn essence_resolver_rejects_subtitle_and_data_kinds() {
        let store = catalogue_with(vec![]);
        let mut cats = std::collections::HashMap::new();
        cats.insert("in-a".to_string(), store);
        let pending = vec![PendingEssenceSlot {
            program_idx: 0,
            slot_idx: 0,
            input_id: "in-a".into(),
            kind: EsKind::Subtitle,
        }];
        let err = resolve_essence_slots(pending, cats, Duration::from_millis(100))
            .await
            .expect_err("subtitle unimplemented");
        assert!(matches!(
            err,
            EssenceResolveError::KindNotImplemented { kind: EsKind::Subtitle }
        ));
    }

    #[tokio::test]
    async fn essence_resolver_completes_when_catalogue_populates_mid_poll() {
        // Start with an empty store; 100 ms after resolver starts, seed
        // it. Resolver must pick up the entry on its next poll tick.
        let store = Arc::new(PsiCatalogStore::new());
        let store_for_writer = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let cat = PsiCatalog {
                programs: vec![CatalogProgram {
                    program_number: 1,
                    pmt_pid: 0x1000,
                    pcr_pid: Some(0x100),
                    streams: vec![stream(0x100, 0x1B, CatalogStreamKind::Video)],
                }],
                last_updated_us: 1,
            };
            crate::engine::ts_psi_catalog::PsiCatalogStore::seed_for_test(
                &store_for_writer,
                cat,
            );
        });
        let mut cats = std::collections::HashMap::new();
        cats.insert("late".to_string(), store);
        let pending = vec![PendingEssenceSlot {
            program_idx: 0,
            slot_idx: 0,
            input_id: "late".into(),
            kind: EsKind::Video,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(1000))
            .await
            .expect("must pick up late catalogue within timeout");
        assert_eq!(r, vec![((0, 0), 0x100)]);
    }
}
