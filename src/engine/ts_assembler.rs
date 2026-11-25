// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TS assembler for the PID-bus runtime (Phases 5 + 6 + MPTS).
//!
//! Subscribes to a set of elementary-stream channels on [`NodeEsBus`],
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
//!   into a single `mpsc::Sender<FromFanin>`. Slow consumers
//!   see `broadcast::error::RecvError::Lagged(_)` and drop; nothing
//!   blocks the demuxer.
//! - Per-packet work: two header-byte edits + one CC increment. No PES
//!   reconstruction, no copies beyond `Bytes::copy_from_slice` of the
//!   188-byte payload into the bundle buffer.
//! - One allocation per emitted bundle (`BytesMut::with_capacity(1316)`
//!   swapped in after each flush) — bundles ship at ~(video packet rate
//!   / 7), not per-TS-packet.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use super::master_clock::ClockIdentity;
use super::packet::RtpPacket;
use super::ts_es_bus::{EsPacket, NodeEsBus};
use super::ts_es_hitless::{ForwardingPrimary, SeqFallback};
use super::ts_parse::{
    extract_pcr, mpeg2_crc32, set_discontinuity_indicator, NULL_PID, PAT_PID, TS_PACKET_SIZE,
    TS_SYNC_BYTE,
};
use crate::stats::collector::{AssembledOnAir, OnAirInputs};

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

/// A slot is declared stalled once it has forwarded no ES packet for
/// this long. PSI keeps flowing regardless (the assembler synthesises
/// PAT/PMT on its own cadence), which is exactly why a byte-level
/// liveness check on the flow output can never catch a dead slot —
/// the stall must be measured per slot, here.
const SLOT_STALL_THRESHOLD: Duration = Duration::from_secs(5);

/// No stall is declared during the first 10 s after assembler start —
/// sources are still connecting at flow bring-up. Hot-swapped-in slots
/// get their own implicit grace via `SlotVitals.registered_at`.
const SLOT_STALL_START_GRACE: Duration = Duration::from_secs(10);

/// Cadence of the per-slot stall scan + `AssemblyHealth` publication.
const STALL_SCAN_INTERVAL: Duration = Duration::from_secs(1);

/// How long a program may be held for its new leg's first PCR before an event
/// says so: after a Take `pid_bus_take_no_pcr`, after a plan swap
/// `pid_bus_swap_no_pcr`, which also ends the hold. Twenty times the 100 ms
/// ISO/IEC 13818-1 allows between PCRs.
const PCR_HOLD_ALARM: Duration = Duration::from_secs(2);

/// Packets the fan-ins may have queued for the assembler. Small: if it lags,
/// the bus rings lag and drop at their edge, not behind this queue.
const FANIN_DEPTH: usize = 256;

/// Shared cell the assembler publishes its per-slot health snapshot
/// into. Written by the assembler at the 1 Hz stall scan, read by
/// `FlowStatsAccumulator::snapshot()` — both sides touch it at ~1 Hz,
/// never on the packet path, so a plain `RwLock` matches the existing
/// `master_clock_state` idiom.
pub type AssemblyHealthCell =
    std::sync::RwLock<Option<crate::stats::models::AssemblyHealth>>;

/// Per-slot liveness state, keyed by `out_pid` like the rest of what the
/// assembler keeps of a slot: it is unique in the mux
/// (`config::validation::validate_flow_assembly`), and a `ReplacePlan` that
/// moves or renumbers the slot's program leaves it. Owned exclusively by
/// the assembler task — the forward path bumps `packets`, the 1 Hz scan
/// derives `last_data` from the counter delta — so no atomics or locks
/// are needed (single-task ownership keeps the packet path lock-free,
/// and the counter approach avoids a clock read per packet).
#[derive(Debug)]
struct SlotVitals {
    /// When the slot entered the running plan (assembler start, or the
    /// `ReplacePlan` that introduced it). Stall anchor until the first
    /// ES packet arrives.
    registered_at: Instant,
    /// ES packets forwarded on the slot's `out_pid`. For Switch slots
    /// only the active leg forwards, so a dead active leg stalls the
    /// slot even while passive legs carry traffic on the bus.
    packets: u64,
    /// `packets` as of the previous stall scan.
    scanned_packets: u64,
    /// Wall time of the scan that last observed the counter moving.
    /// `None` until the first ES packet (granularity = scan cadence,
    /// plenty for a 5 s threshold).
    last_data: Option<Instant>,
    /// Latched stall flag: set by the scan when the threshold is
    /// crossed (fires `pid_bus_slot_stalled` once), cleared by the
    /// forward path on the first ES packet after the stall (fires
    /// `pid_bus_slot_recovered`).
    stalled: bool,
}

impl SlotVitals {
    fn new(now: Instant) -> Self {
        Self {
            registered_at: now,
            packets: 0,
            scanned_packets: 0,
            last_data: None,
            stalled: false,
        }
    }
}

/// Whether a slot's stream type carries continuous media that is
/// expected to flow at all times — the only class the stall scan may
/// judge. Sparse essences (SCTE-35 cues, DVB subtitles/teletext on
/// 0x06, private sections, generic data) are legitimately silent for
/// minutes between bursts; scanning them latches false
/// `pid_bus_slot_stalled` alarms that permanently degrade
/// `assembly_health` and train operators to ignore the signal. 0x06 is
/// exempt wholesale: the plan carries no descriptor info, so a 0x06
/// slot cannot be distinguished between DVB AC-3 (continuous) and
/// teletext (sparse) here — false negatives beat false alarms.
fn stall_scan_eligible(stream_type: u8) -> bool {
    matches!(
        stream_type,
        // Video: MPEG-1/2, MPEG-4 part 2, H.264 (+SVC/MVC), HEVC, JPEG-2000, AVS, VC-1
        0x01 | 0x02 | 0x10 | 0x1B | 0x1F | 0x20 | 0x21 | 0x24 | 0x42 | 0xEA
        // Audio: MPEG-1/2 layer I/II, AAC ADTS, AAC LATM, ATSC AC-3 / E-AC-3
        | 0x03 | 0x04 | 0x0F | 0x11 | 0x81 | 0x87
    )
}

/// Pure stall predicate. Returns `Some(seconds_since_data)` when the
/// slot crosses the stall threshold, `None` while healthy or inside the
/// post-start grace window.
fn stall_seconds(now: Instant, assembler_start: Instant, v: &SlotVitals) -> Option<u64> {
    if now.duration_since(assembler_start) < SLOT_STALL_START_GRACE {
        return None;
    }
    let anchor = v.last_data.unwrap_or(v.registered_at);
    let since = now.duration_since(anchor);
    (since >= SLOT_STALL_THRESHOLD).then_some(since.as_secs())
}

/// Reconcile the vitals map against the (possibly just-replaced) plan:
/// drop entries for slots no longer present, register fresh entries
/// (with a fresh grace anchor) for slots that just appeared. Entries
/// that survive a `ReplacePlan` keep their history so an already-stalled
/// slot doesn't silently reset on an unrelated plan edit.
fn sync_slot_vitals(
    plan: &AssemblyPlan,
    vitals: &mut HashMap<u16, SlotVitals>,
    now: Instant,
) {
    let keys: HashSet<u16> =
        plan.programs.iter().flat_map(|prog| &prog.slots).map(|s| s.out_pid).collect();
    vitals.retain(|k, _| keys.contains(k));
    for k in keys {
        vitals.entry(k).or_insert_with(|| SlotVitals::new(now));
    }
}

/// Resolve the `(input_id, source_pid)` identity a slot's health should
/// report: the active leg for Switch slots, the fixed source otherwise.
fn slot_health_identity(
    slot: &AssemblySlot,
    active_leg_input: &HashMap<u16, String>,
) -> (String, u16) {
    if let (Some(legs), Some(active)) = (&slot.switch_legs, active_leg_input.get(&slot.out_pid))
        && let Some((iid, pid)) = legs.iter().find(|(iid, _)| iid == active) {
            return (iid.clone(), *pid);
        }
    slot.source.clone()
}

/// What carries a program's PCR: its slot's `out_pid`, and the input and PID
/// that slot plays.
struct PcrLeg {
    out_pid: u16,
    source: (String, u16),
}

/// The [`PcrLeg`] of each program of `plan`, in its order.
fn pcr_legs(
    plan: &AssemblyPlan,
    pcr_out_pid_by_program: &[u16],
    active_leg_input: &HashMap<u16, String>,
) -> Vec<Option<PcrLeg>> {
    let leg = |(prog, out_pid): (&ProgramPlan, &u16)| {
        let slot = prog.slots.iter().find(|s| s.out_pid == *out_pid)?;
        let source = slot_health_identity(slot, active_leg_input);
        Some(PcrLeg { out_pid: *out_pid, source })
    };
    plan.programs.iter().zip(pcr_out_pid_by_program).map(leg).collect()
}

/// The [`PcrLeg`] of the program numbered `program_number`.
fn pcr_leg_of(
    program_number: u16,
    plan: &AssemblyPlan,
    pcr_out_pid_by_program: &[u16],
    active_leg_input: &HashMap<u16, String>,
) -> Option<PcrLeg> {
    let pidx = plan.programs.iter().position(|p| p.program_number == program_number)?;
    pcr_legs(plan, pcr_out_pid_by_program, active_leg_input).swap_remove(pidx)
}

/// What the assembled output plays from: each Pid / Essence slot's source,
/// each Switch slot's active leg and each Hitless slot's legs, of which the
/// one its merger is forwarding is on air — never the merger's synthetic
/// bus key.
pub(crate) fn on_air_inputs(
    plan: &AssemblyPlan,
    active_leg_input: &HashMap<u16, String>,
) -> AssembledOnAir {
    let mut on_air = AssembledOnAir::default();
    for slot in plan.programs.iter().flat_map(|prog| &prog.slots) {
        match &slot.hitless_legs {
            Some(legs) => on_air.hitless.push(legs.clone()),
            None => {
                on_air.inputs.insert(slot_health_identity(slot, active_leg_input).0);
            }
        }
    }
    on_air
}

/// Publish [`on_air_inputs`] for the flow's SRT drop accounting, which
/// counts a drop against the flow only while its input is on air. Called
/// on every change to the plan or to a Switch slot's active leg — a Take, a
/// PES-aligned commit, a splice that fell back on timeout or codec mismatch
/// — so a Take moves the attribution when the cut happens on air, not when
/// it was asked for. A Hitless merger's failover needs no publication: the
/// published legs carry the flag the merger flips.
fn publish_on_air(
    on_air: Option<&OnAirInputs>,
    plan: &AssemblyPlan,
    active_leg_input: &HashMap<u16, String>,
) {
    if let Some(on_air) = on_air {
        on_air.set_assembled(on_air_inputs(plan, active_leg_input));
    }
}

/// 1 Hz stall scan. Advances `last_data` for slots whose forward counter
/// moved since the previous scan, latches newly-stalled slots, and
/// builds the [`AssemblyHealth`] wire snapshot. Returns the snapshot
/// plus the slots that *newly* stalled on this scan (the caller emits
/// one `pid_bus_slot_stalled` event per entry — latched, so a dead slot
/// alarms once, re-arming only after recovery).
fn scan_slot_stalls(
    plan: &AssemblyPlan,
    active_leg_input: &HashMap<u16, String>,
    vitals: &mut HashMap<u16, SlotVitals>,
    now: Instant,
    assembler_start: Instant,
) -> (
    crate::stats::models::AssemblyHealth,
    Vec<crate::stats::models::StalledSlotStats>,
) {
    use crate::stats::models::{AssemblyHealth, StalledSlotStats, UnsequencedHitlessSlot};
    let mut health = AssemblyHealth::default();
    let mut newly_stalled: Vec<StalledSlotStats> = Vec::new();
    for prog in &plan.programs {
        for slot in &prog.slots {
            health.total_slots += 1;
            // Ahead of the liveness checks, which skip a slot that is
            // moving: a merger that fell back still forwards.
            if let Some(legs) = &slot.hitless_legs
                && let Some(leg) = legs.seq_fallback.leg()
            {
                health.unsequenced_hitless_slots.push(UnsequencedHitlessSlot {
                    program_number: prog.program_number,
                    out_pid: slot.out_pid,
                    leg: leg.to_string(),
                    input_id: if leg == "primary" { &legs.primary } else { &legs.backup }.clone(),
                });
            }
            let Some(v) = vitals.get_mut(&slot.out_pid) else {
                continue; // unsynced entry — next sync_slot_vitals heals it
            };
            if v.packets != v.scanned_packets {
                v.scanned_packets = v.packets;
                v.last_data = Some(now);
                continue; // moving — recovery latch clears on the forward path
            }
            if !stall_scan_eligible(slot.stream_type) {
                continue; // sparse essence (SCTE-35/subs/teletext/data) — never judged
            }
            let (was_stalled, since) =
                match (v.stalled, stall_seconds(now, assembler_start, v)) {
                    // Already latched: keep reporting with a fresh age.
                    (true, _) => (
                        true,
                        now.duration_since(v.last_data.unwrap_or(v.registered_at))
                            .as_secs(),
                    ),
                    (false, Some(secs)) => {
                        v.stalled = true;
                        (false, secs)
                    }
                    (false, None) => continue,
                };
            let (input_id, source_pid) = slot_health_identity(slot, active_leg_input);
            let entry = StalledSlotStats {
                program_number: prog.program_number,
                out_pid: slot.out_pid,
                input_id,
                source_pid,
                seconds_since_data: since,
            };
            if !was_stalled {
                newly_stalled.push(entry.clone());
            }
            health.stalled_slot_count += 1;
            health.stalled_slots.push(entry);
        }
    }
    (health, newly_stalled)
}

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
    /// The clock of each of the flow's inputs, as
    /// [`check_assembly_clock_compatibility`] was given it and
    /// [`PlanCommand::InputClocks`] brings up to date. An input of another
    /// flow is absent.
    pub clocks: HashMap<String, ClockIdentity>,
}

impl AssemblyPlan {
    /// Whether two inputs are known to run on one clock, so that the
    /// timestamps of one can be compared with the other's.
    fn share_clock(&self, a: &str, b: &str) -> bool {
        self.clocks.get(a).is_some_and(|clock| self.clocks.get(b) == Some(clock))
    }
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
    /// Concrete `(input_id, source_pid)` on the bus. For Switch slots this
    /// is the leg the slot starts on; a Take moves only the assembler's
    /// `active_leg_input`.
    pub source: (String, u16),
    pub out_pid: u16,
    pub stream_type: u8,
    /// When `Some(legs)`, this slot is a [`SlotSource::Switch`] and the
    /// assembler subscribes to every leg concurrently (warm). Only the
    /// leg whose `input_id` matches the flow's currently-active input
    /// forwards bytes on `out_pid`. On `SwitchActiveInput` the active
    /// leg pointer flips, the program's PMT version bumps, and DI=1
    /// is armed on the next PCR for `out_pid` so receivers stay locked
    /// without re-tuning. `None` for non-switch slots.
    #[allow(dead_code)] // read by build_assembly_plan / SwitchActiveInput handler
    pub switch_legs: Option<Vec<(String, u16)>>,
    /// When `Some(legs)`, this slot is a [`SlotSource::Hitless`] and
    /// `source` is its merger's synthetic bus key: these are the two inputs
    /// behind it, and which of them the merger is forwarding. `None` for
    /// other slots.
    pub hitless_legs: Option<HitlessLegs>,
    /// PES Switch Phase 4. For Switch slots, controls the splice
    /// behaviour at `SwitchActiveInput` time:
    /// - [`SpliceMode::PmtBump`] (default): today's PMT-version-bump
    ///   + DI=1 path.
    /// - [`SpliceMode::PesAligned`]: assembler buffers the slot's
    ///   outbound bytes at the last PES boundary on the from-leg,
    ///   then concatenates the to-leg's next PES with
    ///   `PTS ≥ threshold`. Honoured for both audio (PES-boundary
    ///   aligned) and video (H.264 / HEVC, IDR-aligned). Slots whose
    ///   codec supports neither fall through to `PmtBump` and the
    ///   assembler emits `pes_splice_degraded`. Ignored for non-switch
    ///   slots.
    pub splice_mode: crate::config::models::SpliceMode,
    /// PES Switch Phase 4. Splice-budget override in ms for
    /// `PesAligned` mode. `None` falls back to the default
    /// (200 ms for audio). Ignored when `splice_mode = PmtBump`.
    pub splice_budget_ms: Option<u32>,
}

impl AssemblySlot {
    /// Whether the slot reads `source`: its own, or one of its Switch legs.
    pub fn reads(&self, source: &(String, u16)) -> bool {
        &self.source == source
            || self.switch_legs.as_ref().is_some_and(|legs| legs.contains(source))
    }

    /// What the slot reads from `input_id`, if it does.
    pub fn source_on(&self, input_id: &str) -> Option<&(String, u16)> {
        match &self.switch_legs {
            Some(legs) => legs.iter().find(|leg| leg.0 == input_id),
            None => Some(&self.source).filter(|source| source.0 == input_id),
        }
    }
}

/// The two inputs behind a [`SlotSource::Hitless`] slot's merger, and which
/// of them it is forwarding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitlessLegs {
    pub primary: String,
    pub backup: String,
    /// Shared with the slot's merger, which flips it; read by the flow's SRT
    /// stats pollers through [`OnAirInputs`].
    pub forwarding_primary: ForwardingPrimary,
    /// Shared with the slot's merger, which sets it if a seq-aware merge
    /// falls back to primary preference; read by the 1 Hz stall scan into
    /// `AssemblyHealth::unsequenced_hitless_slots`.
    pub seq_fallback: SeqFallback,
}

impl HitlessLegs {
    /// The leg on air: the one the merger is forwarding.
    pub fn on_air(&self) -> &str {
        if self.forwarding_primary.get() {
            &self.primary
        } else {
            &self.backup
        }
    }
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
    /// Swap the running plan. The assembler diffs old vs new, spawns
    /// fan-ins for sources it has none on, cancels those the new plan does
    /// not read, bumps per-program PMT versions where composition changed,
    /// and bumps PAT version when the set of programs itself changed. What
    /// a cancelled fan-in had queued goes out first, on the plan before.
    ///
    /// What the swap does not edit it leaves as it is, wherever the new plan
    /// puts it: a slot is known by its `out_pid`, and a program by its slots
    /// or its number ([`programs_before`]).
    ///
    /// A splice pending in a slot that keeps both its legs goes on, and any
    /// other ends ([`legs_after_swap`]). A program whose PCR the swap moves
    /// to an input not known to share the clock of the one before is held
    /// for the first PCR from there, but not for good ([`PcrHold::Swap`]).
    ReplacePlan {
        plan: AssemblyPlan,
        /// Per-swap splice request carried through from the manager's
        /// `update_flow_assembly` command (a bus_route Swap / assembled
        /// Take). Slot retargeting via plan replacement is inherently a
        /// PMT-bump-style cut (old fan-in cancelled, new fan-in spawned,
        /// PMT version bump + DI armed) — `Some(PmtBump)` and `None` are
        /// therefore no-ops. `Some(PesAligned)` cannot be honoured on
        /// this path (the PES splice state machine only drives
        /// Switch-slot legs); the assembler emits a Warning
        /// `splice_override_ignored` so the operator's request is never
        /// silently dropped.
        splice_mode_override: Option<crate::config::models::SpliceMode>,
        /// Per-input PSI catalogue handles for the NEW plan's sources —
        /// replaces the assembler's descriptor-copy-through map so swap
        /// targets unknown at spawn time resolve their ES descriptors.
        /// Empty in tests (descriptor-less PMTs, yesterday's shape).
        catalogs: HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
        /// Told once the new plan is in place: every fan-in it no longer
        /// reads is cancelled. The swap waits for it — a few seconds at most
        /// (`ASSEMBLER_ACK_TIMEOUT` in `engine/flow.rs`), past which it keeps
        /// them — before it releases the bus channels of the Hitless slots
        /// the plan dropped, so no fan-in still reading one sees it close.
        applied: Option<tokio::sync::oneshot::Sender<()>>,
    },
    /// Operator-driven switch-slot retarget. Sent by
    /// `engine::manager::FlowManager::switch_active_input` when an
    /// `ActivateInput` arrives for a flow running in assembly mode.
    /// The assembler walks every switch slot whose leg list contains
    /// `new_input_id` and, per slot's `splice_mode`:
    ///
    /// - [`SpliceMode::PmtBump`] (default): atomically flips that
    ///   slot's active-leg pointer, bumps the owning program's PMT
    ///   version (mod 32, monotonic), and arms DI=1 on the next PCR
    ///   for the slot's `out_pid`.
    /// - [`SpliceMode::PesAligned`]: arms the per-slot splice state
    ///   machine — [`crate::engine::pes_splice::AudioSpliceState`] for
    ///   supported audio codecs, or
    ///   [`crate::engine::pes_splice::VideoSpliceState`] for H.264
    ///   (`0x1B`) / HEVC (`0x24`). The active-leg flip is deferred
    ///   until either B produces an aligned PES (audio: PUSI=1 at
    ///   `pts ≥ threshold`; video: the same plus an IDR access unit)
    ///   — commit — or the splice budget expires (fall back to
    ///   PmtBump, emits `pes_splice_timeout`). Slots whose codec
    ///   supports neither, and legs not known to run on one clock,
    ///   whose PTS cannot be compared, fall through to the PmtBump
    ///   path and emit `pes_splice_degraded`.
    ///
    /// A splice still pending from an earlier Take is dropped for this
    /// one. Where the slot that carries a program's PCR moves to another
    /// clock, the program's packets are held until the first PCR from the
    /// new leg ([`PcrHold::Take`]): a PES ahead of it would go out on the old
    /// leg's clock.
    ///
    /// Slots whose leg list does not include `new_input_id` are left
    /// untouched — per design, `ActivateInput` is silent for slots
    /// that don't speak that source.
    SwitchActiveInput {
        new_input_id: String,
        /// PES Switch Phase 4. `None` means "honour each slot's
        /// config-time `splice_mode`" (which itself defaults to
        /// [`SpliceMode::PmtBump`]) — the back-compatible behaviour for
        /// callers that don't set it. When `Some`, the command
        /// overrides each slot's config-time `splice_mode` for this one
        /// switch — useful for ad-hoc operator overrides without
        /// reconfiguring the flow. Resolved as
        /// `splice_mode_override.unwrap_or(slot.splice_mode)` in the
        /// handler.
        splice_mode_override: Option<crate::config::models::SpliceMode>,
    },
    /// The flow's clocks, [`AssemblyPlan::clocks`], after an input joined it:
    /// the running plan may have a Switch leg on that input.
    InputClocks(HashMap<String, ClockIdentity>),
    /// Tests: hold the assembler, as a host too starved to run it would,
    /// until `release` is sent or dropped.
    #[cfg(test)]
    Stall { release: tokio::sync::oneshot::Receiver<()> },
}

/// Two slots of one program that can play from inputs on different clocks at
/// the same time, as [`check_assembly_clock_compatibility`] and
/// [`check_added_input_clock`] report it. Its [`Display`](std::fmt::Display)
/// and [`details`](Self::details) go into the `pid_bus_master_clock_mismatch`
/// event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterClockMismatch {
    /// MPEG-TS program number containing the conflict (matches `ProgramPlan.program_number`).
    pub program_number: u16,
    /// `out_pid` of the slot that breaks the program's clock: the later of
    /// the two in the program.
    pub slot_out_pid: u16,
    /// The input that slot would play.
    pub slot_input_id: String,
    /// Human-readable label of that input's clock
    /// (e.g. `"source_pcr:input=in-srt-a"`, `"ptp:domain=127"`).
    pub slot_clock: String,
    /// `out_pid` of the other slot.
    pub reference_out_pid: u16,
    /// The input the other slot would play beside it.
    pub reference_input_id: String,
    /// Human-readable label of that input's clock.
    pub reference_clock: String,
    /// The input whose Take puts the pair on air. `None` when the flow
    /// starts on it.
    pub take_input_id: Option<String>,
}

impl std::fmt::Display for MasterClockMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "program {} slot 0x{:04X} input '{}' has master clock {} but slot 0x{:04X} \
             plays input '{}' on clock {} beside it ",
            self.program_number,
            self.slot_out_pid,
            self.slot_input_id,
            self.slot_clock,
            self.reference_out_pid,
            self.reference_input_id,
            self.reference_clock,
        )?;
        match &self.take_input_id {
            Some(input) => write!(f, "after a Take to '{input}'")?,
            None => write!(f, "from the start")?,
        }
        write!(f, " — combining incompatible clocks would silently drift")
    }
}

impl MasterClockMismatch {
    /// The `details` of the `pid_bus_master_clock_mismatch` event.
    pub fn details(&self) -> serde_json::Value {
        serde_json::json!({
            "error_code": "pid_bus_master_clock_mismatch",
            "program_number": self.program_number,
            "slot_out_pid": self.slot_out_pid,
            "slot_input_id": self.slot_input_id,
            "slot_clock": self.slot_clock,
            "reference_out_pid": self.reference_out_pid,
            "reference_input_id": self.reference_input_id,
            "reference_clock": self.reference_clock,
            "take_input_id": self.take_input_id,
        })
    }
}

/// What the clock rule reads of one slot.
struct SlotInputs<'a> {
    out_pid: u16,
    /// The input it starts on. A Hitless slot's is its merger, whose clock is
    /// not known.
    start: &'a str,
    /// The inputs of a Switch slot's legs, the one input of any other slot.
    inputs: Vec<&'a str>,
    /// The input a refusal names for it: a Hitless slot's primary, the input
    /// any other slot starts on.
    name: &'a str,
}

impl SlotInputs<'_> {
    /// Whether a Take can move the slot off the input it starts on.
    fn moves(&self) -> bool {
        self.inputs.iter().any(|input| *input != self.start)
    }
}

/// The pairs of inputs slots `a` and `b` of one program can play at the same
/// time, as `(input of a, input of b, input whose Take brings them on air)`,
/// first the pair they start on, with no Take.
///
/// On a running plan only `PlanCommand::SwitchActiveInput { x }` moves a
/// slot: it puts every Switch slot with a leg on `x` on that leg and leaves
/// the others. So, beside the start pair:
///
/// - Both slots have a leg on `x`: a Take to `x` puts both on it. With a leg
///   each on `y` as well, nothing puts one on `x` and the other on `y`.
/// - Only one has a leg on `x`: a Take to `x` puts it there and leaves the
///   other where it is, which can be any of its legs, as a Take to that leg
///   may have come first.
///
/// A splice ends on the leg its Take named whether it commits, times out,
/// degrades or is refused; it runs only between inputs on one clock, so the
/// wait for it mixes none; and the next Take of its slot, or a plan swap that
/// takes one of its legs, ends one still pending. A plan swap keeps the legs
/// its Switch slots are on, which the plan before it was judged for, and puts
/// a slot it adds beside them, or where a splice is taking them. A leg whose
/// input is gone plays nothing.
fn pairs_on_air<'a>(
    a: &SlotInputs<'a>,
    b: &SlotInputs<'a>,
) -> Vec<(&'a str, &'a str, Option<&'a str>)> {
    let mut pairs = vec![(a.start, b.start, None)];
    for &x in &a.inputs {
        for &y in &b.inputs {
            let a_goes_alone = !b.inputs.contains(&x);
            let b_goes_alone = b.moves() && !a.inputs.contains(&y);
            if b_goes_alone && (a.start == x || !a_goes_alone) {
                pairs.push((x, y, Some(y)));
            } else if a_goes_alone {
                pairs.push((x, y, Some(x)));
            }
        }
    }
    pairs
}

/// An input of `moving` that cannot play beside `still`, whose clock is not
/// known, with the Take that brings it on air: `None` unless Takes that leave
/// `still` where it starts, to inputs it has no leg on, put `moving` on two
/// clocks, of which `still` can be on one at most. With `added`, one of those
/// Takes must be to it.
fn apart_from_unknown<'a>(
    still: &SlotInputs<'a>,
    moving: &SlotInputs<'a>,
    clocks: &HashMap<String, ClockIdentity>,
    added: Option<&str>,
) -> Option<(&'a str, Option<&'a str>)> {
    if clocks.contains_key(still.start) {
        return None;
    }
    let beside: Vec<(&str, &ClockIdentity)> = moving
        .inputs
        .iter()
        .filter(|x| !still.inputs.contains(*x))
        .filter_map(|x| Some((*x, clocks.get(*x)?)))
        .collect();
    let (_, first) = beside.first()?;
    if beside.iter().all(|(_, clock)| clock == first)
        || added.is_some_and(|added| !beside.iter().any(|(x, _)| *x == added))
    {
        return None;
    }
    let of_still =
        clocks.get(still.name).or_else(|| clocks.get(moving.start)).unwrap_or(first);
    let (x, _) = beside.iter().find(|(_, clock)| *clock != of_still)?;
    Some((x, (*x != moving.start).then_some(*x)))
}

/// The first two of `slots` that can play from inputs on different clocks at
/// the same time, in slot order. An input with no clock in `clocks`, which is
/// another flow's, and a Hitless slot are on a clock that is not known: all
/// that is certain of it is that it is one ([`apart_from_unknown`]). With
/// `added`, only what a Take to one of `added`'s Switch legs takes part in.
fn first_clock_mismatch(
    program_number: u16,
    slots: &[SlotInputs<'_>],
    clocks: &HashMap<String, ClockIdentity>,
    added: Option<&str>,
) -> Result<(), Box<MasterClockMismatch>> {
    let label = |input: &str| clocks.get(input).map_or_else(|| "unknown".into(), |c| c.label());
    for (later, b) in slots.iter().enumerate() {
        for a in &slots[..later] {
            let known = pairs_on_air(a, b).into_iter().find(|(x, y, take)| {
                let counts = added.is_none_or(|added| {
                    take.is_some()
                        && ((*x == added && a.moves()) || (*y == added && b.moves()))
                });
                let (x, y) = (clocks.get(*x), clocks.get(*y));
                counts && x.is_some() && y.is_some() && x != y
            });
            let mixed = known
                .or_else(|| {
                    let (y, take) = apart_from_unknown(a, b, clocks, added)?;
                    Some((a.name, y, take))
                })
                .or_else(|| {
                    let (x, take) = apart_from_unknown(b, a, clocks, added)?;
                    Some((x, b.name, take))
                });
            if let Some((x, y, take)) = mixed {
                return Err(Box::new(MasterClockMismatch {
                    program_number,
                    slot_out_pid: b.out_pid,
                    slot_input_id: y.to_string(),
                    slot_clock: label(y),
                    reference_out_pid: a.out_pid,
                    reference_input_id: x.to_string(),
                    reference_clock: label(x),
                    take_input_id: take.map(str::to_string),
                }));
            }
        }
    }
    Ok(())
}

/// Refuse a plan in which two slots of one program can play from inputs on
/// different master clocks at the same time.
///
/// Two encoders run independent 27 MHz oscillators, so video from one with
/// audio from the other drifts out of lip-sync without bound and shows on no
/// counter. Mute beats wrong-sync.
///
/// The legs of a Switch slot are alternatives: they never play together, so
/// what is judged is every pair [`pairs_on_air`] names, not every leg. Two
/// Switch slots with the same legs that start on one input follow each Take
/// together, and their program plays one clock at a time whatever the legs'
/// clocks are. A slot fixed to one of those legs, or with a leg the other
/// does not have, stays behind on a Take and mixes two clocks.
///
/// Programs are independent: a receiver acquires a clock for each.
///
/// `input_clocks` holds the flow's own inputs. An input of another flow and
/// a Hitless slot's merged source are not in it, and are judged only beside a
/// slot that changes clock while they stay.
pub fn check_assembly_clock_compatibility(
    plan: &AssemblyPlan,
    input_clocks: &HashMap<String, ClockIdentity>,
) -> Result<(), Box<MasterClockMismatch>> {
    for program in &plan.programs {
        let slots: Vec<SlotInputs> = program
            .slots
            .iter()
            .map(|slot| SlotInputs {
                out_pid: slot.out_pid,
                start: &slot.source.0,
                inputs: match &slot.switch_legs {
                    Some(legs) => legs.iter().map(|leg| leg.0.as_str()).collect(),
                    None => vec![slot.source.0.as_str()],
                },
                name: slot.hitless_legs.as_ref().map_or(&slot.source.0, |legs| &legs.primary),
            })
            .collect();
        first_clock_mismatch(program.program_number, &slots, input_clocks, None)?;
    }
    Ok(())
}

/// [`check_assembly_clock_compatibility`] for an input about to join a
/// running flow whose assembly already names it: until then it was not one
/// of the flow's own, so it was not judged and no Take could reach it. Only
/// what a Take to one of its Switch legs brings on air counts; a slot fixed
/// to it plays it already.
pub fn check_added_input_clock(
    assembly: &crate::config::models::FlowAssembly,
    input_clocks: &HashMap<String, ClockIdentity>,
    added: &str,
) -> Result<(), Box<MasterClockMismatch>> {
    use crate::config::models::SlotSource;
    for program in &assembly.programs {
        let slots: Vec<SlotInputs> = program
            .streams
            .iter()
            .map(|stream| {
                let (start, inputs, name) = match &stream.source {
                    SlotSource::Pid { input_id, .. } | SlotSource::Essence { input_id, .. } => {
                        (input_id.as_str(), vec![input_id.as_str()], input_id.as_str())
                    }
                    SlotSource::Switch { legs, initial_input_id, .. } => (
                        initial_input_id.as_str(),
                        legs.iter().map(|leg| leg.input_id()).collect(),
                        initial_input_id.as_str(),
                    ),
                    // No input is named "".
                    SlotSource::Hitless { primary, .. } => (
                        "",
                        vec![""],
                        match primary.as_ref() {
                            SlotSource::Pid { input_id, .. } => input_id.as_str(),
                            _ => "",
                        },
                    ),
                };
                SlotInputs { out_pid: stream.out_pid, start, inputs, name }
            })
            .collect();
        first_clock_mismatch(program.program_number, &slots, input_clocks, Some(added))?;
    }
    Ok(())
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
// `event_sender` carries `pes_splice_completed` / `pes_splice_timeout`
// to the manager (PES Switch Phase 4). Tests pass `None`; the production
// site in `flow.rs::finalize_spts_assembler` wires the flow's real
// `EventSender`. `flow_id` is stamped on every emitted event
// (empty in tests). `health_cell` receives the 1 Hz per-slot
// `AssemblyHealth` snapshot for `FlowStats.assembly_health`; `None`
// skips publication (tests / legacy callers). `catalogs` maps each
// member input to its live PSI catalogue store — the assembler copies
// whitelisted source-PMT ES descriptors onto the emitted PMT per slot
// (DVB 0x06 audio is unidentifiable downstream without them); an empty
// map (tests / legacy callers) emits descriptor-less PMTs as before.
#[allow(clippy::too_many_arguments)]
pub fn spawn_spts_assembler(
    plan: AssemblyPlan,
    bus: Arc<NodeEsBus>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
    event_sender: Option<crate::manager::events::EventSender>,
    flow_id: String,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    health_cell: Option<Arc<AssemblyHealthCell>>,
    on_air: Option<Arc<OnAirInputs>>,
    catalogs: HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
) -> AssemblerHandle {
    // Channel depth 16: plan updates are rare (operator-triggered), so a
    // small bounded buffer is plenty and keeps back-pressure observable
    // if a pathological caller floods.
    let (plan_tx, plan_rx) = mpsc::channel::<PlanCommand>(16);
    // Spawn on a dedicated SCHED_FIFO OS thread with its own
    // tokio::current_thread runtime. Lifts the assembler off the main
    // runtime's worker pool, so Tokio scheduling latency in other
    // tasks doesn't leak into PAT/PMT version cadence, slot fan-in,
    // or PCR-bearing-packet emission timing. CPU pinning honoured via
    // `BILBYCAST_PID_BUS_CPUS`.
    let who = if flow_id.is_empty() {
        "assembler".to_string()
    } else {
        format!("assembler-{flow_id}")
    };
    let thread_handle = crate::engine::dedicated_runtime::spawn_dedicated(
        crate::engine::dedicated_runtime::DedicatedRuntimeConfig::new(who, "BILBYCAST_PID_BUS_CPUS"),
        async move {
            run_assembler(plan, bus, broadcast_tx, cancel, plan_rx, event_sender, flow_id, av_sync_pacer, health_cell, on_air, catalogs).await;
        },
    );
    // The original `AssemblerHandle.join` is a `tokio::task::JoinHandle<()>`.
    // Tests + lifecycle callers `.await` it. We need to preserve that
    // shape, so wrap the OS-thread JoinHandle in a small bridge: a
    // tokio task that blocks-in-place on the OS thread join. This
    // bridge task runs on the main runtime (cheap — just a join wait),
    // but the actual assembler work happens on the dedicated thread.
    let join = tokio::spawn(async move {
        // `tokio::task::spawn_blocking` is the right primitive here:
        // `JoinHandle::join` is a blocking syscall (`pthread_join`).
        // We don't await the spawn_blocking handle's result because
        // the underlying future returned () already.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = thread_handle.join();
        }).await;
    });
    AssemblerHandle { join, plan_tx }
}

/// Flattened slot view used internally by the assembler. One entry per
/// **bus subscription** — for `Pid` / `Essence` / `Hitless` slots that's
/// one FlatSlot per AssemblySlot, but for Switch slots it's one
/// FlatSlot per leg (all sharing the same `out_pid` and `program_idx`).
/// `program_idx` is the program's place in the running plan; a plan swap
/// sets it anew on every fan-in it keeps.
#[derive(Debug, Clone)]
struct FlatSlot {
    program_idx: usize,
    source: (String, u16),
    out_pid: u16,
    /// `true` iff this FlatSlot is one leg of a [`AssemblySlot::switch_legs`]
    /// group. The main loop drops fan-in packets whose `source.0`
    /// (this leg's input_id) doesn't match
    /// `active_leg_input.get(&out_pid)`. Non-switch slots
    /// have this `false` and forward unconditionally.
    is_switch_leg: bool,
    /// MPEG-TS `stream_type` of the parent [`AssemblySlot`]. Carried on
    /// the flattened view so the PES Switch Phase 4 codec-param
    /// sentinel can short-circuit the AAC ADTS-only path without a
    /// PSI lookup on every fan-in packet.
    stream_type: u8,
}

/// The bus subscriptions of `plan`, in its order.
fn flatten(plan: &AssemblyPlan) -> Vec<FlatSlot> {
    let mut flat = Vec::new();
    for (program_idx, prog) in plan.programs.iter().enumerate() {
        for slot in &prog.slots {
            let sources = slot.switch_legs.as_deref().unwrap_or(std::slice::from_ref(&slot.source));
            flat.extend(sources.iter().map(|source| FlatSlot {
                program_idx,
                source: source.clone(),
                out_pid: slot.out_pid,
                is_switch_leg: slot.switch_legs.is_some(),
                stream_type: slot.stream_type,
            }));
        }
    }
    flat
}

/// What a fan-in hands the assembler: its index in the assembler's `flat`
/// with a packet, or with `None` when its bus channel has closed.
type FromFanin = (usize, Option<EsPacket>);

/// Why a program waits for a PCR: what moved the slot that carries its PCR to
/// a leg not known to be on the clock of the one before. Ahead of the new
/// leg's first PCR its PES would go out on the old one's clock.
enum PcrHold {
    /// A Take, `since` then: it lasts until the PCR. `reported` once the Take
    /// has been said to wait for it in vain.
    Take { since: Instant, reported: bool },
    /// A plan swap. An input that sends its PCR on a PID the program does not
    /// carry, which no swap refuses, would leave it off air for good: the hold
    /// ends [`PCR_HOLD_ALARM`] after the first packet it withheld.
    Swap { withheld: Option<Instant> },
}

/// A Switch slot as a plan swap finds it: the place of its program in the
/// plan before, the leg it is on, and the one a pending splice takes it to.
struct Switched {
    program: usize,
    on: String,
    to: Option<String>,
}

/// The version each program's PMT was last sent with, by program number and
/// by PMT PID, either of which a receiver may keep it by. A program that goes
/// keeps its own for when it comes back.
#[derive(Default)]
struct PmtVersions {
    by_program: HashMap<u16, u8>,
    by_pid: HashMap<u16, u8>,
}

impl PmtVersions {
    fn of(&self, prog: &ProgramPlan) -> u8 {
        self.by_program.get(&prog.program_number).copied().unwrap_or(0)
    }

    /// `prog`'s PMT is another table from here on: the version after the last
    /// of its number and the last on its PID, mod 32.
    fn bump(&mut self, prog: &ProgramPlan) {
        let of_number = self.by_program.get(&prog.program_number).copied();
        let on_pid = self.by_pid.get(&prog.pmt_pid).copied();
        let last = match (of_number, on_pid) {
            (Some(n), Some(p)) if p.wrapping_sub(n) & 0x1F < 16 => Some(p),
            (n, p) => n.or(p),
        };
        let next = last.map_or(0, |v| v.wrapping_add(1) & 0x1F);
        self.by_program.insert(prog.program_number, next);
        self.by_pid.insert(prog.pmt_pid, next);
    }
}

/// A plan swap taken off the command queue, to apply once what the fan-ins it
/// cancels had queued has gone out.
struct PlanSwap {
    plan: AssemblyPlan,
    splice_mode_override: Option<crate::config::models::SpliceMode>,
    catalogs: HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
    applied: Option<tokio::sync::oneshot::Sender<()>>,
    /// The program of the running plan each of the new one's is.
    before: Vec<Option<usize>>,
    /// The new plan's subscriptions, each with the fan-in that goes on in it.
    fanins: Vec<(FlatSlot, Option<usize>)>,
    /// What the other fan-ins had queued: it goes out on the new plan.
    queued: VecDeque<FromFanin>,
}

/// The next packet of `queued`, or of the fan-ins once there is none.
async fn next_packet(
    queued: &mut VecDeque<FromFanin>,
    fanins: &mut mpsc::Receiver<FromFanin>,
) -> Option<FromFanin> {
    match queued.pop_front() {
        Some(packet) => Some(packet),
        None => fanins.recv().await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_assembler(
    mut plan: AssemblyPlan,
    bus: Arc<NodeEsBus>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
    mut plan_rx: mpsc::Receiver<PlanCommand>,
    event_sender: Option<crate::manager::events::EventSender>,
    flow_id: String,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
    health_cell: Option<Arc<AssemblyHealthCell>>,
    on_air: Option<Arc<OnAirInputs>>,
    mut catalogs: HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
) {
    // Muxer-mode rewriter on the assembled output. Single shared
    // anchor across every input contributing to the program — fixes
    // the per-input-anchor problem the input-side rewriter has on
    // PID-bus / Node-Bus flows. Industry-standard remux behaviour:
    // master-clock PCR + PES PTS, PCR_RR + PSI_RR + DI=1 + SCTE-35
    // compliance applied to the *assembled* output. `None` when the
    // flow doesn't have a pacer (legacy + test paths).
    let mut pts_rewriter: Option<crate::engine::ts_pts_rewriter::TsPtsRewriter> =
        av_sync_pacer.map(crate::engine::ts_pts_rewriter::TsPtsRewriter::new);
    use crate::engine::pes_splice::{
        AacAudioParams, AudioSpliceState, FromPacketAction, SpliceOutcome,
        VideoCodecParams, VideoSpliceState, DEFAULT_AUDIO_SPLICE_BUDGET_MS,
        DEFAULT_VIDEO_SPLICE_BUDGET_MS, extract_aac_params_from_pes,
        extract_video_params_from_pes, is_supported_audio_stream_type,
        is_supported_video_stream_type,
    };
    use std::time::Duration;
    // What the assembler keeps of a slot is keyed by its `out_pid`: unique in
    // the mux whatever its programs are called and in whatever order the plan
    // has them, so a plan swap that leaves a slot alone leaves its state.
    //
    // PES Switch Phase 4 — per-switch-slot splice state.
    // Default `Idle`; flipped to `Pending` on `SwitchActiveInput` with
    // splice_mode = PesAligned, reset to `Idle` on commit or timeout.
    let mut splice_state: HashMap<u16, AudioSpliceState> = HashMap::new();
    // Last PUSI=1 PTS observed on each switch slot — needed to arm the
    // splice machine at SwitchActiveInput time. Updated on every
    // active-leg PUSI=1 packet; cheap (one HashMap insert per AU, hot
    // path stays clean).
    let mut last_a_pts: HashMap<u16, u64> = HashMap::new();
    // Most recent AAC AudioSpecificConfig observed on the active leg of
    // each switch slot — feeds the codec-param sentinel at arm time. Only
    // populated for AAC ADTS (stream_type 0x0F) slots; every other audio
    // codec leaves this `None`.
    let mut last_a_aac_params: HashMap<u16, AacAudioParams> = HashMap::new();
    // PES Switch Phase 4 (edge 0.66.0) — per-switch-slot video splice
    // state. Disjoint from `splice_state` by construction: each slot's
    // `stream_type` is either an audio codec (audio state map) or a
    // supported video codec (this map), never both.
    let mut video_splice_state: HashMap<u16, VideoSpliceState> = HashMap::new();
    // PES Switch Phase 4 Session B — most recent SPS-derived parameter
    // snapshot observed on the active leg of each video switch slot.
    // Refreshed on every active-leg PUSI=1 IDR PES that carries a
    // parseable SPS. Feeds the codec-param sentinel at splice arm time so
    // the state machine can compare A's snapshot against B's first IDR.
    let mut last_a_video_params: HashMap<u16, VideoCodecParams> = HashMap::new();
    // Fan-in: one task per flat slot drains the bus broadcast receiver
    // and forwards into a single mpsc. Channel capacity is deliberately
    // small — if the assembler lags, broadcasts `Lagged` earlier and
    // dropping happens at the bus edge, not in mpsc backpressure. This
    // keeps the no-cascade-backpressure invariant.
    let (fanin_tx, mut fanin_rx) = mpsc::channel::<FromFanin>(FANIN_DEPTH);

    // Flat slot view rebuilt on every plan change. Slot indices in this
    // vec are the keys under which fan-in tasks send their packets; they
    // must stay stable for already-spawned fan-ins, so on a plan swap
    // we append new slots to the end rather than recompact. A fan-in that
    // is gone leaves `None`, and a packet it still sends is dropped.
    let mut flat: Vec<Option<FlatSlot>> = Vec::new();
    let mut slot_cancels: Vec<CancellationToken> = Vec::new();
    let mut slot_tasks: Vec<JoinHandle<()>> = Vec::new();

    // A plan swap to apply, and the packets it took off the queue.
    let mut swap: Option<PlanSwap> = None;
    let mut queued: VecDeque<FromFanin> = VecDeque::new();

    // Per-program PCR resolution — the out_pid of whichever slot on
    // that program matches `pcr_source`. Recomputed on every plan swap.
    let mut pcr_out_pid_by_program: Vec<u16> = Vec::new();

    // PSI state.
    //
    // `pat_version` bumps when the list of programs changes, a program's
    // in `pmt_versions` when its slots, its `pcr_source`, its number or
    // its PMT PID changes. Both wrap mod 32 (5-bit PMT spec field) and
    // advance monotonically across switches, including A→B→A, to avoid
    // phantom-version collisions the `TsContinuityFixer` already wrestled
    // with (see project_ffplay_stuck_state memory).
    let mut pat_version: u8 = 0;
    let mut pmt_versions = PmtVersions::default();
    for prog in &plan.programs {
        pmt_versions.bump(prog);
    }

    // Per-out_pid one-shot DI arming. A `ReplacePlan` arms the slots it
    // has play another source and the one a program's PCR moves to, a
    // `SwitchActiveInput` the switch slots it flips.
    // The flag is consumed when the next PCR-bearing TS packet
    // flows through the ES rewrite path on that out_pid; the assembler
    // sets `discontinuity_indicator` (AF flags 0x80) so receivers (VLC,
    // ffplay, broadcast-grade hardware) re-anchor STC instead of treating
    // the post-swap PCR-epoch jump as a clock fault. Per-out_pid keying
    // means a switch on slot X doesn't false-trigger DI on a sibling
    // slot Y. Mirror of `TsContinuityFixer::pending_di_on_pcr` for the
    // PID-bus / Flow Assembly path.
    let mut pending_di_for_out_pid: std::collections::HashMap<u16, bool> =
        std::collections::HashMap::new();

    // Programs moved to another clock, by program number, until their first
    // PCR from the new leg. A plan swap gives a program it renumbers its hold
    // under the new number.
    let mut awaiting_pcr: HashMap<u16, PcrHold> = HashMap::new();

    // Per-switch-slot active-leg pointer. Set from each switch slot's
    // initial `source.0` by `install_plan`, by `legs_after_swap` on a plan
    // swap and by `SwitchActiveInput`. Fan-in packets from non-active legs
    // are dropped at the main-loop edge (cheap string compare, no rewrite).
    let mut active_leg_input: HashMap<u16, String> = HashMap::new();

    // Copy-through ES descriptors per slot, resolved from each slot's
    // current source against the per-input
    // PSI catalogues. Refreshed on plan install/replace and on every
    // PSI tick (cheap — Arc load + small decode per slot at 10 Hz) so
    // a catalogue that arrives after flow start, or a switch-leg flip,
    // converges within one PSI cadence. A descriptor change bumps the
    // owning program's PMT version like any other composition change.
    let mut es_info_by_slot: HashMap<u16, Vec<u8>> = HashMap::new();

    // Declared-vs-source stream_type mismatch debounce per slot:
    // `(observed stream_type, ticks seen)`. See `reconcile_slot_stream_types`.
    let mut st_mismatch_stability: HashMap<u16, (u8, u8)> = HashMap::new();
    // What the config declares for a slot whose `stream_type` that corrected.
    let mut declared_stream_types: HashMap<u16, u8> = HashMap::new();

    // Egress bundle buffer + per-PID CC counters (ES and PMT PIDs, one of
    // which a plan swap can turn into the other) + PAT CC.
    let mut buf = BytesMut::with_capacity(BUNDLE_BYTES);
    let mut cc: HashMap<u16, u8> = HashMap::new();
    let mut pat_cc: u8 = 0;
    let mut bundle_seq: u16 = 0;

    // Per-slot liveness. Forward path bumps the counter; the 1 Hz stall
    // scan latches stalls and publishes `AssemblyHealth` into `health_cell`.
    let assembler_start = Instant::now();
    let mut slot_vitals: HashMap<u16, SlotVitals> = HashMap::new();

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
        &mut active_leg_input,
    );
    publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
    sync_slot_vitals(&plan, &mut slot_vitals, assembler_start);
    let _ = refresh_slot_es_info(&plan, &catalogs, &mut es_info_by_slot, |_| false);

    let mut psi_tick = interval(PSI_INTERVAL);
    psi_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut flush_tick = interval(FLUSH_INTERVAL);
    flush_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stall_tick = interval(STALL_SCAN_INTERVAL);
    stall_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
        &mut cc,
        &broadcast_tx,
        &mut bundle_seq,
        &es_info_by_slot,
        pts_rewriter.as_mut(),
    );

    loop {
        if queued.is_empty() && let Some(next) = swap.take() {
            let PlanSwap {
                plan: new_plan,
                splice_mode_override,
                catalogs: new_catalogs,
                applied,
                before,
                fanins,
                queued: rest,
            } = next;
            queued = rest;
            // Adopt the new plan's catalogue handles so retargeted slots
            // resolve descriptors from their (possibly brand-new) sources.
            if !new_catalogs.is_empty() {
                catalogs = new_catalogs;
            }
            let pcr_from = pcr_legs(&plan, &pcr_out_pid_by_program, &active_leg_input);
            let old = std::mem::replace(&mut plan, new_plan);

            // The leg each Switch slot is on now. A splice the plan has no leg
            // or no slot for ends here: left to run it would move a slot to a
            // leg that is not there.
            let splicing_to = |out_pid: &u16| {
                let audio = splice_state.get(out_pid).and_then(|s| s.pending_to_input_id());
                let video = || video_splice_state.get(out_pid)?.pending_to_input_id();
                audio.or_else(video).map(str::to_string)
            };
            let mut switched = HashMap::new();
            for (program, prog) in old.programs.iter().enumerate() {
                for slot in &prog.slots {
                    if let Some(on) = active_leg_input.get(&slot.out_pid).cloned() {
                        let to = splicing_to(&slot.out_pid);
                        switched.insert(slot.out_pid, Switched { program, on, to });
                    }
                }
            }
            let (legs, spliced) = legs_after_swap(&plan, &before, &switched);
            let legs_from = std::mem::replace(&mut active_leg_input, legs);
            splice_state.retain(|out_pid, _| spliced.contains(out_pid));
            video_splice_state.retain(|out_pid, _| spliced.contains(out_pid));
            // Nor is the next splice measured against a leg on another clock.
            let measures = |out_pid: &u16| match (legs_from.get(out_pid), active_leg_input.get(out_pid)) {
                (Some(from), Some(now)) => from == now || plan.share_clock(from, now),
                _ => false,
            };
            last_a_pts.retain(|out_pid, _| measures(out_pid));
            last_a_aac_params.retain(|out_pid, _| measures(out_pid));
            last_a_video_params.retain(|out_pid, _| measures(out_pid));

            // The fan-ins: those that go on, where the plan has them now.
            flat.fill(None);
            for (slot, fanin) in fanins {
                match fanin {
                    Some(i) => flat[i] = Some(slot),
                    None => spawn_fanin(slot, &bus, &fanin_tx, &cancel, &mut flat, &mut slot_cancels, &mut slot_tasks),
                }
            }

            // PSI versions. The PAT lists the programs in the plan's order.
            let listed = |plan: &AssemblyPlan| -> Vec<(u16, u16)> {
                plan.programs.iter().map(|p| (p.program_number, p.pmt_pid)).collect()
            };
            if listed(&old) != listed(&plan) {
                pat_version = pat_version.wrapping_add(1) & 0x1F;
            }
            let mut bumped: HashSet<u16> = HashSet::new();
            for (prog, before) in plan.programs.iter().zip(&before) {
                if before.map(|b| &old.programs[b]) != Some(prog) {
                    pmt_versions.bump(prog);
                    bumped.insert(prog.program_number);
                }
            }

            pcr_out_pid_by_program = pcr_out_pids(&plan);
            let pcr_now = pcr_legs(&plan, &pcr_out_pid_by_program, &active_leg_input);
            let was: HashMap<u16, &AssemblySlot> =
                old.programs.iter().flat_map(|p| &p.slots).map(|s| (s.out_pid, s)).collect();
            let mut held = std::mem::take(&mut awaiting_pcr);
            for (pidx, prog) in plan.programs.iter().enumerate() {
                let was_prog = before[pidx].map(|b| &old.programs[b]);
                let from = before[pidx].and_then(|b| pcr_from[b].as_ref());
                // Where its PCR comes from another input now, the program is
                // held for the first one, unless the two are known to share
                // a clock and it was not held already. One the swap adds was
                // on no clock.
                if let (Some(was_prog), Some(from), Some(now)) = (was_prog, from, &pcr_now[pidx]) {
                    let hold = held.remove(&was_prog.program_number);
                    let (from, now) = (&from.source.0, &now.source.0);
                    // A hold goes on as it is in the program it is of, which
                    // has its number or its slots.
                    let same = was_prog.program_number == prog.program_number
                        || was_prog.slots == prog.slots;
                    let hold = match hold {
                        Some(hold) if from == now && same => Some(hold),
                        None if from == now || plan.share_clock(from, now) => None,
                        _ => Some(PcrHold::Swap { withheld: None }),
                    };
                    if let Some(hold) = hold {
                        awaiting_pcr.insert(prog.program_number, hold);
                    }
                }
                // The next PCR of a slot that plays another source, and of one
                // that takes the program's PCR over, carries
                // `discontinuity_indicator = 1`. Without it receivers
                // see the PCR epoch jump as a clock fault and freeze on their
                // last decoded frame even though the PMT version bumped.
                let pcr_moved = from.map(|from| from.out_pid) != Some(pcr_out_pid_by_program[pidx]);
                for slot in &prog.slots {
                    let plays = slot_health_identity(slot, &active_leg_input);
                    let played = was.get(&slot.out_pid).map(|s| slot_health_identity(s, &legs_from));
                    if played != Some(plays)
                        || (pcr_moved && slot.out_pid == pcr_out_pid_by_program[pidx])
                    {
                        pending_di_for_out_pid.insert(slot.out_pid, true);
                    }
                }
            }
            publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
            // Drop vitals for removed slots; fresh grace anchor for
            // hot-swapped-in ones.
            sync_slot_vitals(&plan, &mut slot_vitals, Instant::now());
            // Re-resolve copy-through descriptors against the (possibly
            // retargeted) slot sources before the immediate PSI push, so the
            // swap-time PMT already identifies the new sources' codecs.
            let same_source =
                |slot: &AssemblySlot| was.get(&slot.out_pid).is_some_and(|s| s.source == slot.source);
            for pn in refresh_slot_es_info(&plan, &catalogs, &mut es_info_by_slot, same_source) {
                if !bumped.contains(&pn)
                    && let Some(prog) = plan.programs.iter().find(|p| p.program_number == pn)
                {
                    pmt_versions.bump(prog);
                }
            }
            // A bus_route Swap is a plan replacement — old fan-in cancelled,
            // new fan-in spawned, PMT bump + DI. `PesAligned` cannot be
            // honoured here (the PES splice machine only drives Switch-slot
            // legs); say so instead of silently dropping the operator's
            // request.
            if matches!(splice_mode_override, Some(crate::config::models::SpliceMode::PesAligned))
                && let Some(es_) = &event_sender
            {
                es_.emit_flow_with_details(
                    crate::manager::events::EventSeverity::Warning,
                    crate::manager::events::category::FLOW,
                    format!(
                        "Flow '{flow_id}': splice override 'pes_aligned' ignored — \
                         this switch re-pinned slot sources (bus_route swap), which \
                         is inherently a PMT-bump cut. PES-aligned splicing needs a \
                         Switch slot with both inputs as legs."
                    ),
                    &flow_id,
                    serde_json::json!({
                        "error_code": "splice_override_ignored",
                        "requested_mode": "pes_aligned",
                        "reason": "bus_route_slot_retarget",
                    }),
                );
            }
            // Emit fresh PSI immediately on switch so downstream receivers
            // see the new PMT before the first rewritten ES packet lands on a
            // new out_pid — without this, ffprobe can see TS bytes on an
            // unknown PID for ~100 ms.
            push_psi(
                &mut buf,
                &plan,
                &pcr_out_pid_by_program,
                pat_version,
                &pmt_versions,
                &mut pat_cc,
                &mut cc,
                &broadcast_tx,
                &mut bundle_seq,
                &es_info_by_slot,
                pts_rewriter.as_mut(),
            );
            if let Some(applied) = applied {
                let _ = applied.send(());
            }
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                flush(&mut buf, &broadcast_tx, &mut bundle_seq, pts_rewriter.as_mut());
                break;
            }
            Some(cmd) = plan_rx.recv(), if swap.is_none() => {
                match cmd {
                    PlanCommand::ReplacePlan { plan: mut new_plan, splice_mode_override, catalogs, applied } => {
                        keep_corrections(&plan, &mut new_plan, &mut declared_stream_types);
                        let before = programs_before(&plan, &new_plan);
                        let fanins = reuse_fanins(&flat, &new_plan, &before);
                        let kept: HashSet<usize> = fanins.iter().filter_map(|(_, fanin)| *fanin).collect();
                        for (_, fanin) in slot_cancels.iter().enumerate().filter(|(i, _)| !kept.contains(i)) {
                            fanin.cancel();
                        }
                        // What a cancelled fan-in has queued, with the one
                        // packet it waits to queue if there was no room, goes
                        // out on the plan it was for, before the swap. Sent on
                        // after it, it was the old source's on the slot's PID.
                        let full = fanin_rx.len() == FANIN_DEPTH;
                        take_queued(&mut fanin_rx, &mut queued);
                        if full {
                            tokio::task::yield_now().await;
                            take_queued(&mut fanin_rx, &mut queued);
                        }
                        let (rest, first) =
                            queued.into_iter().partition(|(i, _)| kept.contains(i));
                        queued = first;
                        swap = Some(PlanSwap {
                            plan: new_plan,
                            splice_mode_override,
                            catalogs,
                            applied,
                            before,
                            fanins,
                            queued: rest,
                        });
                    }
                    PlanCommand::SwitchActiveInput {
                        new_input_id,
                        splice_mode_override,
                    } => {
                        // Walk every switch slot. If its leg list
                        // contains `new_input_id`, route by the slot's
                        // config-time `splice_mode`:
                        //
                        // - PmtBump (default): immediate flip + PMT
                        //   v+1 mod 32 + DI=1 on next PCR. Identical
                        //   to pre-Phase-4 behaviour.
                        // - PesAligned (audio only): arm the per-slot
                        //   audio splice state machine. The active-leg
                        //   flip is deferred until B aligns (commit in
                        //   fanin handler) or budget expires (PmtBump
                        //   fallback on flush_tick). Non-audio slots
                        //   fall through to PmtBump silently — video
                        //   splice is a Phase 4 follow-up.
                        //
                        // Slots without a matching leg are silently
                        // skipped — `ActivateInput` is silent for slots
                        // that don't speak that source by design.
                        let mut any_flipped = false;
                        for (pidx, prog) in plan.programs.iter().enumerate() {
                            for slot in &prog.slots {
                                let Some(legs) = &slot.switch_legs else {
                                    continue;
                                };
                                if !legs.iter().any(|(iid, _)| iid == &new_input_id) {
                                    continue;
                                }
                                let key = slot.out_pid;
                                // This Take replaces one whose splice is
                                // still pending, a Take back included.
                                splice_state.remove(&key);
                                video_splice_state.remove(&key);
                                let prev = active_leg_input.get(&key).cloned();
                                if prev.as_deref() == Some(new_input_id.as_str()) {
                                    continue; // already active
                                }
                                let same_clock = prev
                                    .as_deref()
                                    .is_some_and(|prev| plan.share_clock(prev, &new_input_id));

                                // Decide splice path. PesAligned needs:
                                //  (1) effective splice_mode == PesAligned,
                                //  (2) slot is a known audio OR video codec,
                                //  (3) both legs on one clock: B's PTS is
                                //      compared with A's,
                                //  (4) we've seen at least one PUSI=1 PTS
                                //      on the active leg (otherwise we
                                //      have no threshold to align B to).
                                //
                                // The override beats the slot's config-time
                                // `splice_mode` for this one switch only —
                                // None falls back to today's behaviour.
                                use crate::config::models::SpliceMode;
                                let effective_mode = splice_mode_override
                                    .unwrap_or(slot.splice_mode);
                                let pes_aligned =
                                    matches!(effective_mode, SpliceMode::PesAligned);
                                let want_audio = pes_aligned
                                    && same_clock
                                    && is_supported_audio_stream_type(slot.stream_type);
                                let want_video = pes_aligned
                                    && same_clock
                                    && is_supported_video_stream_type(slot.stream_type);
                                if want_audio {
                                    if let Some(&last_pts) = last_a_pts.get(&key) {
                                        let budget_ms = slot
                                            .splice_budget_ms
                                            .unwrap_or(DEFAULT_AUDIO_SPLICE_BUDGET_MS);
                                        let state = splice_state
                                            .entry(key)
                                            .or_default();
                                        // Snapshot A's last AAC params for
                                        // the codec sentinel. `None` for
                                        // non-AAC or when ADTS hasn't
                                        // been parseable yet — the state
                                        // machine falls through to PTS-
                                        // only commit in that case.
                                        let a_params = last_a_aac_params
                                            .get(&key)
                                            .copied();
                                        if state.arm(
                                            new_input_id.clone(),
                                            slot.stream_type,
                                            last_pts,
                                            std::time::Instant::now(),
                                            Duration::from_millis(budget_ms.into()),
                                            a_params,
                                        ) {
                                            // Splice armed — defer the
                                            // flip. The fanin handler
                                            // commits on B's first
                                            // aligned PES, or
                                            // flush_tick falls back to
                                            // PmtBump on timeout.
                                            continue;
                                        }
                                        // arm() returned false →
                                        // stream_type isn't supported.
                                        // Fall through to PmtBump.
                                    }
                                    // No prior PTS observed → fall
                                    // through to PmtBump silently
                                    // (first switch on a freshly-armed
                                    // flow).
                                } else if want_video
                                    && let Some(&last_pts) = last_a_pts.get(&key) {
                                        let budget_ms = slot
                                            .splice_budget_ms
                                            .unwrap_or(DEFAULT_VIDEO_SPLICE_BUDGET_MS);
                                        let state = video_splice_state
                                            .entry(key)
                                            .or_default();
                                        // Snapshot A's last SPS-derived
                                        // codec params for the sentinel.
                                        // `None` when A's encoder hasn't
                                        // emitted a parseable SPS in the
                                        // current GoP — the state machine
                                        // falls through to IDR+PTS-only
                                        // commit in that case.
                                        let a_params = last_a_video_params
                                            .get(&key)
                                            .copied();
                                        if state.arm(
                                            new_input_id.clone(),
                                            slot.stream_type,
                                            last_pts,
                                            std::time::Instant::now(),
                                            Duration::from_millis(budget_ms.into()),
                                            a_params,
                                        ) {
                                            // Splice armed — defer the
                                            // flip. The fanin handler
                                            // commits on B's first
                                            // IDR PES at/past
                                            // threshold, or flush_tick
                                            // falls back to PmtBump
                                            // on budget exhaustion.
                                            continue;
                                        }
                                        // arm() returned false → fall through.
                                    }
                                    // No prior PTS observed → fall
                                    // through to PmtBump silently.

                                // PES-aligned was requested (override or
                                // slot config) but we reached the PmtBump
                                // fall-through — emit a degrade Warning so
                                // the operator isn't left thinking the cut
                                // was glitchless. Reasons: the two legs are
                                // not known to run on one clock, the slot's
                                // codec is not a supported PES-splice type, or
                                // no active-leg PTS reference has been observed
                                // yet (first switch on a freshly-armed flow).
                                // Budget-exhaustion degrades emit
                                // `pes_splice_timeout` separately on the
                                // flush tick.
                                if pes_aligned
                                    && let Some(es_) = &event_sender {
                                        let reason = if !same_clock {
                                            "independent_clocks"
                                        } else if want_audio || want_video {
                                            "no_aligned_pes_reference"
                                        } else {
                                            "unsupported_codec"
                                        };
                                        es_.emit_flow_with_details(
                                            crate::manager::events::EventSeverity::Warning,
                                            crate::manager::events::category::FLOW,
                                            format!(
                                                "Flow '{flow_id}': PES-aligned splice on program {} \
                                                 out_pid 0x{:04X} → '{new_input_id}' degraded to \
                                                 PMT-bump ({reason}, stream_type 0x{:02X})",
                                                prog.program_number,
                                                slot.out_pid,
                                                slot.stream_type,
                                            ),
                                            &flow_id,
                                            serde_json::json!({
                                                "error_code": "pes_splice_degraded",
                                                "reason": reason,
                                                "forced": splice_mode_override.is_some(),
                                                "stream_type": slot.stream_type,
                                                "program_number": prog.program_number,
                                                "out_pid": slot.out_pid,
                                                "to_input_id": new_input_id,
                                            }),
                                        );
                                    }

                                // PmtBump path (today's behaviour).
                                active_leg_input.insert(key, new_input_id.clone());
                                pmt_versions.bump(prog);
                                pending_di_for_out_pid.insert(slot.out_pid, true);
                                any_flipped = true;
                                if !same_clock {
                                    // The next splice has nothing to measure
                                    // until the new leg's first PES: the old
                                    // leg's PTS and codec parameters are no
                                    // reference for it.
                                    last_a_pts.remove(&key);
                                    last_a_aac_params.remove(&key);
                                    last_a_video_params.remove(&key);
                                    // The other slots of the program move with
                                    // the one that carries its PCR.
                                    if pcr_out_pid_by_program.get(pidx) == Some(&slot.out_pid) {
                                        let hold =
                                            PcrHold::Take { since: Instant::now(), reported: false };
                                        awaiting_pcr.insert(prog.program_number, hold);
                                    }
                                }
                            }
                        }
                        if any_flipped {
                            // A slot armed for a PES-aligned splice keeps
                            // its old leg on air until the commit, which
                            // publishes then.
                            publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
                            // Push fresh PSI so receivers see new PMT
                            // versions before the first post-switch ES
                            // byte from the new active leg lands. For
                            // PesAligned splices, PSI is pushed at
                            // commit time inside the fanin handler.
                            push_psi(
                                &mut buf,
                                &plan,
                                &pcr_out_pid_by_program,
                                pat_version,
                                &pmt_versions,
                                &mut pat_cc,
                                &mut cc,
                                &broadcast_tx,
                                &mut bundle_seq,
                                &es_info_by_slot,
                                pts_rewriter.as_mut(),
                            );
                        }
                    }
                    PlanCommand::InputClocks(clocks) => plan.clocks = clocks,
                    #[cfg(test)]
                    PlanCommand::Stall { release } => {
                        let _ = release.await;
                    }
                }
            }
            Some((slot_idx, es)) = next_packet(&mut queued, &mut fanin_rx) => {
                // `None` too for a fan-in a plan swap cancelled while it had
                // a packet to queue.
                let Some(slot) = flat.get(slot_idx).and_then(Option::as_ref) else {
                    continue;
                };
                let slot_program_number = plan
                    .programs
                    .get(slot.program_idx)
                    .map(|p| p.program_number)
                    .unwrap_or(0);
                let Some(es) = es else {
                    // Upstream publisher dropped — the input task that
                    // owns this `(input_id, source_pid)` channel exited.
                    // Most commonly the owning flow was stopped while
                    // sibling flows still reference its inputs via
                    // assembly slots (the "input-host flow" pattern). The
                    // operator needs to see why output went silent.
                    if let Some(es_) = &event_sender {
                        es_.emit_flow_with_details(
                            crate::manager::events::EventSeverity::Warning,
                            crate::manager::events::category::FLOW,
                            format!(
                                "Flow '{flow_id}': assembly slot source disappeared — \
                                 input '{}' source_pid 0x{:04X} (program {} out_pid 0x{:04X}). \
                                 Most likely the owning flow stopped; restart it to recover.",
                                slot.source.0,
                                slot.source.1,
                                slot_program_number,
                                slot.out_pid,
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "pid_bus_slot_source_closed",
                                "program_number": slot_program_number,
                                "out_pid": slot.out_pid,
                                "source_input_id": slot.source.0,
                                "source_pid": slot.source.1,
                            }),
                        );
                    }
                    flat[slot_idx] = None;
                    continue;
                };
                if es.payload.len() != TS_PACKET_SIZE {
                    continue;
                }
                // Switch-slot leg gating: drop packets from non-active
                // legs at the main-loop edge — cheap compare, no rewrite,
                // no CC advance (CC stays monotonic on out_pid because
                // only the active leg ever wraps cc_table[out_pid]).
                //
                // PES Switch Phase 4: when a splice is pending, the
                // gating becomes splice-aware:
                //  - from-leg packets pass through `observe_a_packet`
                //    which forwards them until A's next PUSI=1 (AU
                //    completion), then drops them so the receiver sees
                //    a clean AU boundary on A's last fully-emitted PES.
                //  - to-leg packets pass through `observe_b_packet`
                //    which commits the splice on B's first PUSI=1 PES
                //    with `pts ≥ threshold_pts`. On commit we
                //    atomically flip active_leg_input, bump PMT,
                //    arm DI=1, push PSI, emit `pes_splice_completed`,
                //    and let this very packet be forwarded as the
                //    first byte of the new active leg.
                //  - other-leg packets (neither active nor splice-to)
                //    drop as today.
                if slot.is_switch_leg {
                    let key = slot.out_pid;
                    let is_active = active_leg_input
                        .get(&key)
                        .map(|active| active == &slot.source.0)
                        .unwrap_or(true); // missing entry = treat as active (no switch_legs yet)

                    // Two disjoint splice state maps — audio + video.
                    // A slot's `stream_type` chooses at most one; both
                    // cannot be pending simultaneously for the same
                    // `(program_idx, out_pid)`. The boolean OR below
                    // drives the "are we mid-splice?" gate; the two
                    // observe_a / observe_b call sites dispatch on
                    // which state machine is actually pending.
                    let audio_splice_pending = splice_state
                        .get(&key)
                        .map(|s| s.is_pending())
                        .unwrap_or(false);
                    let video_splice_pending = video_splice_state
                        .get(&key)
                        .map(|s| s.is_pending())
                        .unwrap_or(false);

                    if is_active {
                        // Track active-leg PUSI=1 PTSes so the next
                        // splice has a valid `last_a_pts` threshold —
                        // shared by both audio and video state
                        // machines because both arm off the same map.
                        // For AAC ADTS slots additionally snapshot the
                        // AudioSpecificConfig so the codec sentinel
                        // has a baseline to compare against B with.
                        if crate::engine::ts_parse::ts_pusi(&es.payload) {
                            if let Some(pts) = crate::engine::ts_parse::extract_pes_pts(
                                &es.payload,
                            ) {
                                last_a_pts.insert(key, pts);
                            }
                            if matches!(slot.stream_type, 0x0F | 0x11)
                                && let Some(p) = extract_aac_params_from_pes(
                                    &es.payload,
                                    slot.stream_type,
                                ) {
                                    last_a_aac_params.insert(key, p);
                                }
                            // Snapshot SPS-derived params for video slots
                            // on every active-leg PUSI=1 that carries a
                            // parseable SPS. Most encoders emit SPS on
                            // every IDR; some emit it once per GoP. The
                            // last-seen value is what the sentinel
                            // compares against B at arm time. Cost:
                            // bounded ~180 B Annex-B walk + RBSP unescape
                            // + Exp-Golomb parse per PUSI=1 of an active
                            // video splice slot. Well off the per-packet
                            // hot path (PUSI=1 is once per frame, not per
                            // 188 B).
                            if is_supported_video_stream_type(slot.stream_type)
                                && let Some(p) = extract_video_params_from_pes(
                                    &es.payload,
                                    slot.stream_type,
                                ) {
                                    last_a_video_params.insert(key, p);
                                }
                        }
                        if audio_splice_pending {
                            // Pending audio splice: keep A's AAC params
                            // snapshot fresh inside the state machine
                            // (matters when A produces another PUSI
                            // between arm and AU-completion). Then
                            // decide forward vs drop based on AU
                            // completion state.
                            if matches!(slot.stream_type, 0x0F | 0x11)
                                && crate::engine::ts_parse::ts_pusi(&es.payload)
                                && let Some(s) = splice_state.get_mut(&key) {
                                    s.record_a_audio_params(&es.payload);
                                }
                            let action = splice_state
                                .get_mut(&key)
                                .map(|s| s.observe_a_packet(&es.payload))
                                .unwrap_or(FromPacketAction::Forward);
                            if matches!(action, FromPacketAction::Drop) {
                                continue;
                            }
                        } else if video_splice_pending {
                            // Pending video splice: keep A's SPS-derived
                            // params snapshot fresh inside the state
                            // machine on every PUSI=1 before AU
                            // completion (matters when A emits another
                            // IDR between arm and AU end). Then decide
                            // forward vs drop based on AU completion.
                            if crate::engine::ts_parse::ts_pusi(&es.payload)
                                && let Some(s) = video_splice_state.get_mut(&key) {
                                    s.record_a_video_params(&es.payload);
                                }
                            let action = video_splice_state
                                .get_mut(&key)
                                .map(|s| s.observe_a_packet(&es.payload))
                                .unwrap_or(FromPacketAction::Forward);
                            if matches!(action, FromPacketAction::Drop) {
                                continue;
                            }
                        }
                        // Active + (no splice OR splice still forwarding A)
                        // → fall through to rewrite + emit.
                    } else if video_splice_pending {
                        // Non-active leg with a pending VIDEO splice.
                        // Is this leg the splice's to-leg?
                        let is_splice_to = video_splice_state
                            .get(&key)
                            .and_then(|s| s.pending_to_input_id())
                            .map(|to| to == slot.source.0.as_str())
                            .unwrap_or(false);
                        if !is_splice_to {
                            continue; // some other leg, drop as today
                        }
                        // To-leg packet during pending video splice.
                        // observe_b_packet returns Committed only when
                        // PUSI=1 AND pts ≥ threshold AND the PES
                        // carries an IDR NAL. Non-IDR keyframe-less
                        // PES → None → drop; the next IDR PES from B
                        // will commit (or the budget will expire and
                        // flush_tick falls back to PmtBump).
                        let outcome = video_splice_state
                            .get_mut(&key)
                            .and_then(|s| s.observe_b_packet(&es.payload));
                        match outcome {
                            Some(SpliceOutcome::Committed { first_b_pts }) => {
                                // Atomic flip: active_leg → to_input_id,
                                // PMT v+1 mod 32, DI=1 on next PCR.
                                active_leg_input
                                    .insert(key, slot.source.0.clone());
                                publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
                                pmt_versions.bump(&plan.programs[slot.program_idx]);
                                pending_di_for_out_pid.insert(slot.out_pid, true);
                                push_psi(
                                    &mut buf,
                                    &plan,
                                    &pcr_out_pid_by_program,
                                    pat_version,
                                    &pmt_versions,
                                    &mut pat_cc,
                                    &mut cc,
                                    &broadcast_tx,
                                    &mut bundle_seq,
                                    &es_info_by_slot,
                                    pts_rewriter.as_mut(),
                                );
                                if let Some(es_) = &event_sender {
                                    es_.emit_flow_with_details(
                                        crate::manager::events::EventSeverity::Info,
                                        crate::manager::events::category::FLOW,
                                        format!(
                                            "Flow '{flow_id}': PES-aligned video splice committed on \
                                             program {} out_pid 0x{:04X} → '{}' (IDR @ first_b_pts={first_b_pts})",
                                            slot_program_number,
                                            slot.out_pid,
                                            slot.source.0,
                                        ),
                                        &flow_id,
                                        serde_json::json!({
                                            "error_code": "pes_splice_completed",
                                            "kind": "video",
                                            "program_number": slot_program_number,
                                            "out_pid": slot.out_pid,
                                            "to_input_id": slot.source.0,
                                            "first_b_pts": first_b_pts,
                                        }),
                                    );
                                }
                                // Seed last_a_pts with B's first PTS so the
                                // next splice arm has a clean threshold.
                                last_a_pts.insert(key, first_b_pts);
                                // Fall through to rewrite + emit this
                                // packet — it's now the first byte of
                                // the new active leg and starts on an
                                // IDR so the receiver decodes cleanly.
                            }
                            Some(SpliceOutcome::VideoCodecParamMismatch {
                                to_input_id,
                                a_params,
                                b_params,
                            }) => {
                                // Refuse the mid-PES splice — receiver
                                // would need to re-init its decoder
                                // (different profile / resolution /
                                // bit-depth / chroma). Fall back to
                                // PmtBump (flip + PMT v+1 + DI=1) so
                                // the receiver re-init's cleanly on the
                                // new params via the PMT bump, and emit
                                // `pes_splice_codec_param_mismatch
                                // { kind: "video", ... }` with the full
                                // SPS-field diff for operator triage.
                                active_leg_input.insert(key, to_input_id.clone());
                                publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
                                pmt_versions.bump(&plan.programs[slot.program_idx]);
                                pending_di_for_out_pid.insert(slot.out_pid, true);
                                push_psi(
                                    &mut buf,
                                    &plan,
                                    &pcr_out_pid_by_program,
                                    pat_version,
                                    &pmt_versions,
                                    &mut pat_cc,
                                    &mut cc,
                                    &broadcast_tx,
                                    &mut bundle_seq,
                                    &es_info_by_slot,
                                    pts_rewriter.as_mut(),
                                );
                                if let Some(es_) = &event_sender {
                                    es_.emit_flow_with_details(
                                        crate::manager::events::EventSeverity::Warning,
                                        crate::manager::events::category::FLOW,
                                        format!(
                                            "Flow '{flow_id}': PES-aligned video splice refused on \
                                             program {} out_pid 0x{:04X} → '{to_input_id}': SPS \
                                             params changed (profile {}→{}, level {}→{}, \
                                             {}×{}→{}×{}, bit-depth {}→{}); falling back to PMT-bump",
                                            slot_program_number,
                                            slot.out_pid,
                                            a_params.profile_idc,
                                            b_params.profile_idc,
                                            a_params.level_idc,
                                            b_params.level_idc,
                                            a_params.width,
                                            a_params.height,
                                            b_params.width,
                                            b_params.height,
                                            a_params.bit_depth_luma,
                                            b_params.bit_depth_luma,
                                        ),
                                        &flow_id,
                                        serde_json::json!({
                                            "error_code": "pes_splice_codec_param_mismatch",
                                            "kind": "video",
                                            "program_number": slot_program_number,
                                            "out_pid": slot.out_pid,
                                            "to_input_id": to_input_id,
                                            "a_video_params": {
                                                "profile_idc": a_params.profile_idc,
                                                "level_idc": a_params.level_idc,
                                                "chroma_format_idc": a_params.chroma_format_idc,
                                                "bit_depth_luma": a_params.bit_depth_luma,
                                                "bit_depth_chroma": a_params.bit_depth_chroma,
                                                "width": a_params.width,
                                                "height": a_params.height,
                                            },
                                            "b_video_params": {
                                                "profile_idc": b_params.profile_idc,
                                                "level_idc": b_params.level_idc,
                                                "chroma_format_idc": b_params.chroma_format_idc,
                                                "bit_depth_luma": b_params.bit_depth_luma,
                                                "bit_depth_chroma": b_params.bit_depth_chroma,
                                                "width": b_params.width,
                                                "height": b_params.height,
                                            },
                                        }),
                                    );
                                }
                                // Seed last_a_pts + video-params snapshot
                                // off B's first IDR so a subsequent
                                // splice back to A has a clean baseline.
                                if let Some(pts) = crate::engine::ts_parse::extract_pes_pts(
                                    &es.payload,
                                ) {
                                    last_a_pts.insert(key, pts);
                                }
                                last_a_video_params.insert(key, b_params);
                                // Fall through to rewrite + emit this
                                // packet as the first byte of the new
                                // active leg. Receiver sees: fresh PSI
                                // w/ bumped PMT version → DI=1 PCR → B's
                                // first IDR PES, so the decoder re-init's
                                // on the new params cleanly.
                            }
                            Some(SpliceOutcome::CodecParamMismatch { .. }) => {
                                // CodecParamMismatch is the audio variant.
                                // The video state machine never emits it
                                // — VideoCodecParamMismatch is the video
                                // variant handled above. Unreachable.
                                continue;
                            }
                            Some(SpliceOutcome::Timeout { .. }) => {
                                // observe_b_packet never returns Timeout —
                                // that path is owned by check_timeout in
                                // flush_tick. Unreachable but safe.
                                continue;
                            }
                            None => {
                                continue; // not aligned yet (mid-PES, sub-threshold, or non-IDR), drop
                            }
                        }
                    } else if audio_splice_pending {
                        // Non-active leg with a pending AUDIO splice.
                        // Is this leg the splice's to-leg?
                        let is_splice_to = splice_state
                            .get(&key)
                            .and_then(|s| s.pending_to_input_id())
                            .map(|to| to == slot.source.0.as_str())
                            .unwrap_or(false);
                        if !is_splice_to {
                            continue; // some other leg, drop as today
                        }
                        // To-leg packet during pending splice. Test
                        // for alignment.
                        let outcome = splice_state
                            .get_mut(&key)
                            .and_then(|s| s.observe_b_packet(&es.payload));
                        match outcome {
                            Some(SpliceOutcome::Committed { first_b_pts }) => {
                                // Atomic flip: active_leg → to_input_id,
                                // PMT v+1 mod 32, DI=1 on next PCR.
                                active_leg_input
                                    .insert(key, slot.source.0.clone());
                                publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
                                pmt_versions.bump(&plan.programs[slot.program_idx]);
                                pending_di_for_out_pid.insert(slot.out_pid, true);
                                push_psi(
                                    &mut buf,
                                    &plan,
                                    &pcr_out_pid_by_program,
                                    pat_version,
                                    &pmt_versions,
                                    &mut pat_cc,
                                    &mut cc,
                                    &broadcast_tx,
                                    &mut bundle_seq,
                                    &es_info_by_slot,
                                    pts_rewriter.as_mut(),
                                );
                                if let Some(es_) = &event_sender {
                                    es_.emit_flow_with_details(
                                        crate::manager::events::EventSeverity::Info,
                                        crate::manager::events::category::FLOW,
                                        format!(
                                            "Flow '{flow_id}': PES-aligned splice committed on program \
                                             {} out_pid 0x{:04X} → '{}' (first_b_pts={first_b_pts})",
                                            slot_program_number,
                                            slot.out_pid,
                                            slot.source.0,
                                        ),
                                        &flow_id,
                                        serde_json::json!({
                                            "error_code": "pes_splice_completed",
                                            "program_number": slot_program_number,
                                            "out_pid": slot.out_pid,
                                            "to_input_id": slot.source.0,
                                            "first_b_pts": first_b_pts,
                                        }),
                                    );
                                }
                                // Seed last_a_pts with B's first PTS so the
                                // next splice arm has a clean threshold.
                                last_a_pts.insert(key, first_b_pts);
                                // Refresh A's AAC snapshot to B's params
                                // (B is the new A for the next splice).
                                if matches!(slot.stream_type, 0x0F | 0x11)
                                    && let Some(p) = extract_aac_params_from_pes(
                                        &es.payload,
                                        slot.stream_type,
                                    ) {
                                        last_a_aac_params.insert(key, p);
                                    }
                                // Fall through to rewrite + emit this packet
                                // — it's now the first byte of the new
                                // active leg.
                            }
                            Some(SpliceOutcome::CodecParamMismatch {
                                to_input_id,
                                a_params,
                                b_params,
                            }) => {
                                // Refuse the mid-PES splice — params
                                // would click. Fall back to PmtBump
                                // (flip + PMT v+1 + DI=1) so the
                                // receiver re-init's its AAC decoder
                                // on the new params cleanly. Emit
                                // `pes_splice_codec_param_mismatch`
                                // for operator visibility.
                                active_leg_input.insert(key, to_input_id.clone());
                                publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
                                pmt_versions.bump(&plan.programs[slot.program_idx]);
                                pending_di_for_out_pid.insert(slot.out_pid, true);
                                push_psi(
                                    &mut buf,
                                    &plan,
                                    &pcr_out_pid_by_program,
                                    pat_version,
                                    &pmt_versions,
                                    &mut pat_cc,
                                    &mut cc,
                                    &broadcast_tx,
                                    &mut bundle_seq,
                                    &es_info_by_slot,
                                    pts_rewriter.as_mut(),
                                );
                                if let Some(es_) = &event_sender {
                                    es_.emit_flow_with_details(
                                        crate::manager::events::EventSeverity::Warning,
                                        crate::manager::events::category::FLOW,
                                        format!(
                                            "Flow '{flow_id}': PES-aligned splice refused on program \
                                             {} out_pid 0x{:04X} → '{to_input_id}': AAC params \
                                             changed (profile {}→{}, sample_rate idx {}→{}, ch {}→{}); \
                                             falling back to PMT-bump",
                                            slot_program_number,
                                            slot.out_pid,
                                            a_params.profile,
                                            b_params.profile,
                                            a_params.sample_rate_idx,
                                            b_params.sample_rate_idx,
                                            a_params.channel_config,
                                            b_params.channel_config,
                                        ),
                                        &flow_id,
                                        serde_json::json!({
                                            "error_code": "pes_splice_codec_param_mismatch",
                                            "program_number": slot_program_number,
                                            "out_pid": slot.out_pid,
                                            "to_input_id": to_input_id,
                                            "a_aac_params": {
                                                "profile": a_params.profile,
                                                "sample_rate_idx": a_params.sample_rate_idx,
                                                "sample_rate_hz": a_params.sample_rate_hz(),
                                                "channel_config": a_params.channel_config,
                                            },
                                            "b_aac_params": {
                                                "profile": b_params.profile,
                                                "sample_rate_idx": b_params.sample_rate_idx,
                                                "sample_rate_hz": b_params.sample_rate_hz(),
                                                "channel_config": b_params.channel_config,
                                            },
                                        }),
                                    );
                                }
                                // Seed last_a_pts + AAC snapshot off
                                // B's first PES so a subsequent splice
                                // back to A has a clean baseline.
                                if let Some(pts) = crate::engine::ts_parse::extract_pes_pts(
                                    &es.payload,
                                ) {
                                    last_a_pts.insert(key, pts);
                                }
                                last_a_aac_params.insert(key, b_params);
                                // Fall through to rewrite + emit this
                                // packet as the first byte of the new
                                // active leg — receiver sees: fresh
                                // PSI w/ bumped PMT version → DI=1
                                // PCR → B's first PES. AAC decoder
                                // re-init's on the bumped PMT.
                            }
                            Some(SpliceOutcome::Timeout { .. }) => {
                                // observe_b_packet never returns
                                // Timeout — that path is owned by
                                // check_timeout in flush_tick. Treat
                                // as unreachable but safely no-op
                                // rather than panic on the data path.
                                continue;
                            }
                            Some(SpliceOutcome::VideoCodecParamMismatch { .. }) => {
                                // VideoCodecParamMismatch is the video
                                // variant — the audio state machine
                                // never emits it. Unreachable on the
                                // audio observe_b path; treat as no-op.
                                continue;
                            }
                            None => {
                                continue; // not aligned yet, drop
                            }
                        }
                    } else {
                        // Non-active leg, no pending splice → drop.
                        continue;
                    }
                }
                // From here on: forward the packet (was active leg with
                // splice forwarding A, or B's first committed PES).
                //
                // The program was moved to another clock: nothing of it goes
                // out until a PCR of the new leg does.
                if let Some(hold) = awaiting_pcr.get_mut(&slot_program_number) {
                    if pcr_out_pid_by_program.get(slot.program_idx) != Some(&slot.out_pid)
                        || extract_pcr(&es.payload).is_none()
                    {
                        if let PcrHold::Swap { withheld: withheld @ None } = hold {
                            *withheld = Some(Instant::now());
                        }
                        continue;
                    }
                    awaiting_pcr.remove(&slot_program_number);
                }
                // Slot liveness: count the forwarded packet. The first
                // ES packet after a latched stall fires the recovery
                // event and re-arms stall detection for the slot.
                if let Some(v) = slot_vitals.get_mut(&slot.out_pid) {
                    v.packets = v.packets.wrapping_add(1);
                    if v.stalled {
                        v.stalled = false;
                        v.last_data = Some(Instant::now());
                        if let Some(es_) = &event_sender {
                            let program_number = slot_program_number;
                            es_.emit_flow_with_details(
                                crate::manager::events::EventSeverity::Info,
                                crate::manager::events::category::FLOW,
                                format!(
                                    "Flow '{flow_id}': assembly slot recovered — input '{}' \
                                     source_pid 0x{:04X} resumed (program {program_number} \
                                     out_pid 0x{:04X})",
                                    slot.source.0,
                                    slot.source.1,
                                    slot.out_pid,
                                ),
                                &flow_id,
                                serde_json::json!({
                                    "error_code": "pid_bus_slot_recovered",
                                    "flow_id": flow_id,
                                    "program_number": program_number,
                                    "out_pid": slot.out_pid,
                                    "input_id": slot.source.0,
                                    "source_pid": slot.source.1,
                                }),
                            );
                        }
                    }
                }
                let mut rewritten = rewrite_es_packet(&es.payload, slot.out_pid, &mut cc);
                // After a plan swap or switch-slot flip, the first
                // PCR-bearing TS packet on the affected out_pid carries
                // DI=1 in its adaptation field so the receiver re-anchors
                // STC on the new source's PCR epoch. PCR can ride either
                // a payload-bearing PUSI packet or an AF-only packet;
                // `extract_pcr` handles both. Per-out_pid keying so a
                // switch on one slot doesn't false-trigger sibling
                // slots' DI flags. Mirror of
                // `TsContinuityFixer::pending_di_on_pcr` for the
                // PID-bus / Flow Assembly path.
                if pending_di_for_out_pid.get(&slot.out_pid).copied().unwrap_or(false)
                    && extract_pcr(&rewritten).is_some()
                {
                    set_discontinuity_indicator(&mut rewritten);
                    pending_di_for_out_pid.insert(slot.out_pid, false);
                }
                buf.extend_from_slice(&rewritten);
                if buf.len() >= BUNDLE_BYTES {
                    flush(&mut buf, &broadcast_tx, &mut bundle_seq, pts_rewriter.as_mut());
                }
            }
            _ = psi_tick.tick() => {
                // Reconcile declared video stream_types against the live
                // source PMTs. An applied correction propagates to the
                // flat fan-in view (PES-splice sentinels key off it),
                // bumps the program's PMT version so every downstream
                // TS demuxer rebuilds for the new codec, and raises an
                // operator event — the config still declares the stale
                // byte, so the operator should fix the assembly.
                for (pidx, out_pid, declared, observed, source) in
                    reconcile_slot_stream_types(&mut plan, &catalogs, &mut st_mismatch_stability)
                {
                    for f in flat.iter_mut().flatten() {
                        if f.out_pid == out_pid {
                            f.stream_type = observed;
                        }
                    }
                    declared_stream_types.entry(out_pid).or_insert(declared);
                    pmt_versions.bump(&plan.programs[pidx]);
                    let program_number = plan.programs[pidx].program_number;
                    tracing::warn!(
                        flow_id = %flow_id,
                        program_number,
                        out_pid,
                        declared_stream_type = declared,
                        observed_stream_type = observed,
                        source_input_id = %source.0,
                        source_pid = source.1,
                        "Flow assembly: PMT stream_type corrected to match the live source codec"
                    );
                    if let Some(es) = &event_sender {
                        es.emit_flow_with_details(
                            crate::manager::events::EventSeverity::Warning,
                            crate::manager::events::category::FLOW,
                            format!(
                                "Flow '{flow_id}': assembly slot out_pid 0x{out_pid:04X} \
                                 (program {program_number}) declares stream_type 0x{declared:02X} \
                                 but input '{}' PID 0x{:04X} carries 0x{observed:02X} — \
                                 PMT corrected to the source codec. Update the assembly config \
                                 to clear this warning.",
                                source.0, source.1,
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "pid_bus_stream_type_mismatch",
                                "program_number": program_number,
                                "out_pid": out_pid,
                                "declared_stream_type": declared,
                                "observed_stream_type": observed,
                                "source_input_id": source.0,
                                "source_pid": source.1,
                            }),
                        );
                    }
                }
                // Converge copy-through descriptors against catalogues
                // that updated since the last tick (late PSI discovery
                // on a cold-started input, source PMT change, deferred
                // PES-splice leg commits). Kept on a miss so a
                // briefly-empty catalogue (input restart) doesn't flap
                // the PMT version. A real change bumps the owning
                // program's PMT version before this tick's PSI goes out.
                for pn in refresh_slot_es_info(&plan, &catalogs, &mut es_info_by_slot, |_| true) {
                    if let Some(prog) = plan.programs.iter().find(|p| p.program_number == pn) {
                        pmt_versions.bump(prog);
                    }
                }
                push_psi(
                    &mut buf,
                    &plan,
                    &pcr_out_pid_by_program,
                    pat_version,
                    &pmt_versions,
                    &mut pat_cc,
                    &mut cc,
                    &broadcast_tx,
                    &mut bundle_seq,
                    &es_info_by_slot,
                    pts_rewriter.as_mut(),
                );
            }
            _ = flush_tick.tick() => {
                flush(&mut buf, &broadcast_tx, &mut bundle_seq, pts_rewriter.as_mut());
                // PES Switch Phase 4 — splice budget deadlines. Walk
                // every pending splice; on timeout, fall back to the
                // legacy PmtBump path and emit `pes_splice_timeout`
                // so the operator sees that B never aligned within
                // budget. Collected-and-applied in two passes so we
                // don't mutate the splice_state map while iterating.
                let now = std::time::Instant::now();
                // A plan swap's hold that has withheld packets for as long as
                // it may, no PCR among them, ends: the program goes out
                // without one.
                awaiting_pcr.retain(|program_number, hold| {
                    let PcrHold::Swap { withheld: Some(since) } = hold else {
                        return true;
                    };
                    if now.duration_since(*since) < PCR_HOLD_ALARM {
                        return true;
                    }
                    if let Some(es_) = &event_sender
                        && let Some(PcrLeg { out_pid, source: (input_id, source_pid) }) =
                            pcr_leg_of(*program_number, &plan, &pcr_out_pid_by_program, &active_leg_input)
                    {
                        es_.emit_flow_with_details(
                            crate::manager::events::EventSeverity::Warning,
                            crate::manager::events::category::FLOW,
                            format!(
                                "Flow '{flow_id}': program {program_number} goes out without a \
                                 PCR since the plan swap to '{input_id}' — none on its source_pid \
                                 0x{source_pid:04X}, which carries the program's clock on out_pid \
                                 0x{out_pid:04X}, in the 2 s the program was held for it",
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "pid_bus_swap_no_pcr",
                                "program_number": program_number,
                                "out_pid": out_pid,
                                "input_id": input_id,
                                "source_pid": source_pid,
                            }),
                        );
                    }
                    false
                });
                // (key, to_input_id, kind) tuples. `kind` is folded
                // into the emitted event so the manager UI can tell
                // audio splice timeouts apart from video splice
                // timeouts (different operator action — video timeouts
                // usually mean the encoder's GoP exceeds the budget).
                let mut timeouts: Vec<(u16, String, &'static str)> = Vec::new();
                for (key, state) in splice_state.iter_mut() {
                    if let Some(SpliceOutcome::Timeout { to_input_id }) =
                        state.check_timeout(now)
                    {
                        timeouts.push((*key, to_input_id, "audio"));
                    }
                }
                for (key, state) in video_splice_state.iter_mut() {
                    if let Some(SpliceOutcome::Timeout { to_input_id }) =
                        state.check_timeout(now)
                    {
                        timeouts.push((*key, to_input_id, "video"));
                    }
                }
                if !timeouts.is_empty() {
                    let mut any_flipped = false;
                    for (out_pid, to_input_id, kind) in &timeouts {
                        let out_pid = *out_pid;
                        let carries = |p: &&ProgramPlan| p.slots.iter().any(|s| s.out_pid == out_pid);
                        let Some(prog) = plan.programs.iter().find(carries) else {
                            continue;
                        };
                        active_leg_input.insert(out_pid, to_input_id.clone());
                        pmt_versions.bump(prog);
                        pending_di_for_out_pid.insert(out_pid, true);
                        any_flipped = true;
                        if let Some(es_) = &event_sender {
                            es_.emit_flow_with_details(
                                crate::manager::events::EventSeverity::Warning,
                                crate::manager::events::category::FLOW,
                                format!(
                                    "Flow '{flow_id}': PES-aligned {kind} splice timed out on program \
                                     {} out_pid 0x{:04X} → '{to_input_id}'; falling back to PMT-bump",
                                    prog.program_number,
                                    out_pid,
                                ),
                                &flow_id,
                                serde_json::json!({
                                    "error_code": "pes_splice_timeout",
                                    "kind": kind,
                                    "program_number": prog.program_number,
                                    "out_pid": out_pid,
                                    "to_input_id": to_input_id,
                                }),
                            );
                        }
                    }
                    if any_flipped {
                        publish_on_air(on_air.as_deref(), &plan, &active_leg_input);
                        push_psi(
                            &mut buf,
                            &plan,
                            &pcr_out_pid_by_program,
                            pat_version,
                            &pmt_versions,
                            &mut pat_cc,
                            &mut cc,
                            &broadcast_tx,
                            &mut bundle_seq,
                            &es_info_by_slot,
                            pts_rewriter.as_mut(),
                        );
                    }
                }
            }
            _ = stall_tick.tick() => {
                // Per-slot stall scan + AssemblyHealth publication. A
                // slot with no ES packet for ≥ SLOT_STALL_THRESHOLD
                // (after the start grace) latches stalled and alarms
                // once; recovery is detected on the forward path. This
                // is the flow-level signal the 2026-06 'srt-to-udp'
                // incident lacked — a dead source behind an assembled
                // flow used to emit PSI-only with zero events.
                let now = Instant::now();
                for (program_number, hold) in awaiting_pcr.iter_mut() {
                    let PcrHold::Take { since, reported } = hold else {
                        continue;
                    };
                    if *reported || now.duration_since(*since) < PCR_HOLD_ALARM {
                        continue;
                    }
                    *reported = true;
                    if let Some(es_) = &event_sender
                        && let Some(PcrLeg { out_pid, source: (input_id, source_pid) }) =
                            pcr_leg_of(*program_number, &plan, &pcr_out_pid_by_program, &active_leg_input)
                    {
                        es_.emit_flow_with_details(
                            crate::manager::events::EventSeverity::Warning,
                            crate::manager::events::category::FLOW,
                            format!(
                                "Flow '{flow_id}': program {program_number} is off air since \
                                 the Take to '{input_id}' — no PCR on its source_pid \
                                 0x{source_pid:04X}, which carries the program's clock on \
                                 out_pid 0x{out_pid:04X}. It goes out again with the first \
                                 PCR there, with a Take to an input that sends one, or with a \
                                 plan swap that takes the program's PCR to another input",
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "pid_bus_take_no_pcr",
                                "program_number": program_number,
                                "out_pid": out_pid,
                                "input_id": input_id,
                                "source_pid": source_pid,
                            }),
                        );
                    }
                }
                let (health, newly_stalled) = scan_slot_stalls(
                    &plan,
                    &active_leg_input,
                    &mut slot_vitals,
                    now,
                    assembler_start,
                );
                if let Some(es_) = &event_sender {
                    for s in &newly_stalled {
                        es_.emit_flow_with_details(
                            crate::manager::events::EventSeverity::Warning,
                            crate::manager::events::category::FLOW,
                            format!(
                                "Flow '{flow_id}': assembly slot stalled — no ES data from \
                                 input '{}' source_pid 0x{:04X} for {} s (program {} \
                                 out_pid 0x{:04X}). Outputs carry PSI-only for this slot \
                                 until the source resumes",
                                s.input_id,
                                s.source_pid,
                                s.seconds_since_data,
                                s.program_number,
                                s.out_pid,
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "pid_bus_slot_stalled",
                                "flow_id": flow_id,
                                "program_number": s.program_number,
                                "out_pid": s.out_pid,
                                "input_id": s.input_id,
                                "source_pid": s.source_pid,
                                "seconds_since_data": s.seconds_since_data,
                            }),
                        );
                    }
                }
                if let Some(cell) = &health_cell
                    && let Ok(mut g) = cell.write() {
                        *g = Some(health);
                    }
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

/// Flatten the plan into `flat`, spawn its fan-ins and compute
/// `pcr_out_pid_by_program`. Called once at startup; a plan swap changes
/// them for what it edits.
#[allow(clippy::too_many_arguments)]
fn install_plan(
    plan: &AssemblyPlan,
    bus: &Arc<NodeEsBus>,
    fanin_tx: &mpsc::Sender<FromFanin>,
    cancel: &CancellationToken,
    flat: &mut Vec<Option<FlatSlot>>,
    slot_cancels: &mut Vec<CancellationToken>,
    slot_tasks: &mut Vec<JoinHandle<()>>,
    pcr_out_pid_by_program: &mut Vec<u16>,
    active_leg_input: &mut HashMap<u16, String>,
) {
    // Spawn one fan-in per bus subscription. Pid / Essence / Hitless slots
    // produce one. Switch slots produce one per leg, all sharing the slot's
    // `out_pid` so the main loop can gate on a single out_pid → active_input
    // lookup and drop non-active-leg packets at the edge.
    for slot in flatten(plan) {
        spawn_fanin(slot, bus, fanin_tx, cancel, flat, slot_cancels, slot_tasks);
    }
    for s in plan.programs.iter().flat_map(|prog| &prog.slots) {
        if s.switch_legs.is_some() {
            // Initial active leg = AssemblySlot.source — populated
            // by build_assembly_plan from the config's
            // initial_input_id (or flow.active_input_id on restart).
            active_leg_input.insert(s.out_pid, s.source.0.clone());
        }
    }
    *pcr_out_pid_by_program = pcr_out_pids(plan);
}

/// The `out_pid` that carries each program's PCR: that of the slot that reads
/// its `pcr_source`. Any leg of a Switch slot counts: its active leg flips at
/// runtime and the PCR still rides the slot's fixed `out_pid`.
fn pcr_out_pids(plan: &AssemblyPlan) -> Vec<u16> {
    plan.programs
        .iter()
        .map(|prog| {
            prog.slots
                .iter()
                .find(|s| s.reads(&prog.pcr_source))
                .map(|s| s.out_pid)
                .unwrap_or(prog.pmt_pid)
        })
        .collect()
}

/// Subscribe `slot` to its source, at the end of `flat`.
fn spawn_fanin(
    slot: FlatSlot,
    bus: &Arc<NodeEsBus>,
    fanin_tx: &mpsc::Sender<FromFanin>,
    cancel: &CancellationToken,
    flat: &mut Vec<Option<FlatSlot>>,
    slot_cancels: &mut Vec<CancellationToken>,
    slot_tasks: &mut Vec<JoinHandle<()>>,
) {
    let slot_cancel = cancel.child_token();
    let rx = bus.subscribe(&slot.source.0, slot.source.1);
    slot_tasks.push(tokio::spawn(slot_fanin(flat.len(), rx, fanin_tx.clone(), slot_cancel.clone())));
    slot_cancels.push(slot_cancel);
    flat.push(Some(slot));
}

/// The program of `old` each program of `new` is, in `new`'s order: the one
/// with the same slots, under whatever number, else the one with its number,
/// else one that shares an `out_pid` with it, else, in a transport stream of
/// one program, the only one there was. `None` for one the swap adds.
///
/// The numbers of a transport stream are unique, and so are its PIDs
/// (`config::validation::validate_flow_assembly`), but only within one plan:
/// a swap may give a number or an `out_pid` of one program to another.
fn programs_before(old: &AssemblyPlan, new: &AssemblyPlan) -> Vec<Option<usize>> {
    let shares_a_pid = |old: &ProgramPlan, new: &ProgramPlan| {
        new.slots.iter().any(|n| old.slots.iter().any(|o| o.out_pid == n.out_pid))
    };
    let only_one = old.programs.len() == 1 && new.programs.len() == 1;
    let rules: [&dyn Fn(&ProgramPlan, &ProgramPlan) -> bool; 4] = [
        &|old, new| old.slots == new.slots,
        &|old, new| old.program_number == new.program_number,
        &shares_a_pid,
        &|_, _| only_one,
    ];
    let mut before: Vec<Option<usize>> = vec![None; new.programs.len()];
    for same in rules {
        for (n, prog) in new.programs.iter().enumerate() {
            if before[n].is_none() {
                before[n] = (0..old.programs.len())
                    .find(|o| !before.contains(&Some(*o)) && same(&old.programs[*o], prog));
            }
        }
    }
    before
}

/// The subscriptions of `new`, each with the fan-in of `flat` that goes on in
/// it: one that reads its source. Its own comes first, and one of its program
/// before any other: each is at its own place in the source.
fn reuse_fanins(
    flat: &[Option<FlatSlot>],
    new: &AssemblyPlan,
    before: &[Option<usize>],
) -> Vec<(FlatSlot, Option<usize>)> {
    let mut fanins: Vec<(FlatSlot, Option<usize>)> =
        flatten(new).into_iter().map(|slot| (slot, None)).collect();
    let mut free: Vec<(usize, &FlatSlot)> =
        flat.iter().enumerate().filter_map(|(i, f)| Some((i, f.as_ref()?))).collect();
    let own = |f: &FlatSlot, slot: &FlatSlot| f.out_pid == slot.out_pid;
    let of_its_program =
        |f: &FlatSlot, slot: &FlatSlot| before[slot.program_idx] == Some(f.program_idx);
    let rules: [&dyn Fn(&FlatSlot, &FlatSlot) -> bool; 3] = [&own, &of_its_program, &|_, _| true];
    for suits in rules {
        for (slot, fanin) in fanins.iter_mut().filter(|(_, fanin)| fanin.is_none()) {
            let found = free.iter().position(|(_, f)| f.source == slot.source && suits(f, slot));
            *fanin = found.map(|at| free.remove(at).0);
        }
    }
    fanins
}

/// Move what the fan-ins have queued to the end of `queued`.
fn take_queued(fanins: &mut mpsc::Receiver<FromFanin>, queued: &mut VecDeque<FromFanin>) {
    for _ in 0..fanins.len() {
        queued.extend(fanins.try_recv());
    }
}

/// A slot of `new` that declares, for the source it had, the `stream_type` the
/// assembler has corrected ([`reconcile_slot_stream_types`]) keeps the
/// correction: judged anew, its program's PMT went back to the declared one
/// for two ticks.
fn keep_corrections(old: &AssemblyPlan, new: &mut AssemblyPlan, declared: &mut HashMap<u16, u8>) {
    let corrected = std::mem::take(declared);
    for slot in new.programs.iter_mut().flat_map(|p| &mut p.slots) {
        let before = old.programs.iter().flat_map(|p| &p.slots).find(|s| s.out_pid == slot.out_pid);
        if let Some(before) = before
            && before.source == slot.source
            && corrected.get(&slot.out_pid) == Some(&slot.stream_type)
        {
            declared.insert(slot.out_pid, slot.stream_type);
            slot.stream_type = before.stream_type;
        }
    }
}

/// The leg each Switch slot of `plan` is on once a swap has put it in place
/// of the plan `switched` is of, and the slots whose splice goes on. `before`
/// is what the programs of `plan` were ([`programs_before`]).
///
/// A slot that has the leg it is on keeps it, and its splice if it has the leg
/// that goes to as well. One that has only the latter is put on it. One with
/// neither starts where a slot of its program that kept a leg is, or is being
/// spliced to, if it has that leg: a Take moved that slot, and the plan's
/// `source.0` (the initial leg) could be another clock. Without one it starts
/// there.
///
/// What the slots of one program are on is one clock; the slots of two can be
/// on two. So the slots that keep a leg are its own before the swap, or where
/// it has none of them, as one the swap adds, those of one other program. The
/// rest are new to it.
fn legs_after_swap(
    plan: &AssemblyPlan,
    before: &[Option<usize>],
    switched: &HashMap<u16, Switched>,
) -> (HashMap<u16, String>, HashSet<u16>) {
    let mut legs = HashMap::new();
    let mut spliced = HashSet::new();
    for (prog, before) in plan.programs.iter().zip(before) {
        let switches = || prog.slots.iter().filter(|s| s.switch_legs.is_some());
        let mut homes = switches().filter_map(|s| switched.get(&s.out_pid)).map(|was| was.program);
        let first = homes.clone().next();
        let home = homes.find(|home| Some(*home) == *before).or(first);
        let mut kept: Vec<&String> = Vec::new();
        for s in switches() {
            let was = switched.get(&s.out_pid).filter(|was| Some(was.program) == home);
            let has = |leg: &&String| s.source_on(leg).is_some();
            let on = was.map(|was| &was.on).filter(has);
            let to = was.and_then(|was| was.to.as_ref()).filter(has);
            if let Some(leg) = on.or(to) {
                legs.insert(s.out_pid, leg.clone());
            }
            if on.is_some() && to.is_some() {
                spliced.insert(s.out_pid);
            }
            kept.extend(to.or(on));
        }
        for s in switches() {
            legs.entry(s.out_pid).or_insert_with(|| {
                let joined = kept.iter().find(|leg| s.source_on(leg).is_some());
                joined.map_or(&s.source.0, |leg| *leg).clone()
            });
        }
    }
    (legs, spliced)
}

async fn slot_fanin(
    slot_idx: usize,
    mut rx: broadcast::Receiver<EsPacket>,
    tx: mpsc::Sender<FromFanin>,
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
                    if tx.send((slot_idx, Some(es))).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Slow-consumer drop. Continue draining; no state to
                    // reset because the assembler consumes byte-for-byte
                    // without cross-packet correlation within a slot.
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // The assembler says whose source closed: a plan swap
                    // may have moved the slot since. The
                    // `cancel.cancelled()` arm above wins when the assembler
                    // is being torn down (parent flow stop / hot-swap / edge
                    // shutdown), so this only fires on truly unexpected
                    // closes.
                    let _ = tx.send((slot_idx, None)).await;
                    break;
                }
            }
        }
    }
}

/// Rewrite a 188-byte source TS packet's PID to `out_pid` and stamp
/// a fresh continuity counter from `cc_table`.
///
/// Byte 1 upper 3 bits (TEI, PUSI, transport_priority) are preserved.
/// Byte 3 upper 4 bits (transport_scrambling_control +
/// adaptation_field_control) are preserved.
///
/// CC handling follows H.222.0 §2.4.3.3 exactly:
/// - **Payload-bearing packet** (AFC = `0b01` payload-only, or `0b11`
///   AF+payload): stamp the next CC value from `cc_table`, then advance
///   `cc_table` by one.
/// - **AF-only packet** (AFC = `0b10`): stamp the previously-issued
///   payload CC value, do NOT advance `cc_table`. Receivers expect the
///   AF-only packet to carry the SAME CC as the prior payload packet
///   on the same PID — advancing the counter on AF-only would produce
///   a phantom CC_error on every PCR-only adaptation packet in the
///   source, manifesting as 159-of-394 296 unexpected discontinuities
///   in a 180 s sync-test SPTS run (one per source AF-only packet,
///   spread across the whole capture, NOT clustered at splice
///   boundaries). Previously the code unconditionally advanced
///   `cc_table` and stamped the advanced value, breaking the contract.
fn rewrite_es_packet(src: &[u8], out_pid: u16, cc_table: &mut std::collections::HashMap<u16, u8>) -> [u8; TS_PACKET_SIZE] {
    debug_assert_eq!(src.len(), TS_PACKET_SIZE);
    let mut pkt = [0u8; TS_PACKET_SIZE];
    pkt.copy_from_slice(src);
    // Byte 1: keep high 3 bits; replace low 5 with new PID high bits.
    pkt[1] = (pkt[1] & 0xE0) | (((out_pid >> 8) as u8) & 0x1F);
    // Byte 2: new PID low byte.
    pkt[2] = (out_pid & 0xFF) as u8;
    // Byte 3: preserve scrambling + adaptation_field_control nibble,
    // rebuild CC nibble per H.222.0 §2.4.3.3.
    let afc = (pkt[3] >> 4) & 0x0F;
    let has_payload = (afc & 0x01) != 0; // low bit of AFC = payload_present
    let cc_entry = cc_table.entry(out_pid).or_insert(0);
    let new_cc = if has_payload {
        let stamped = *cc_entry & 0x0F;
        *cc_entry = cc_entry.wrapping_add(1) & 0x0F;
        stamped
    } else {
        // AF-only: keep the prior payload's CC (= cc_entry - 1 mod 16,
        // since cc_entry holds the next-to-issue value and has already
        // advanced past the last issued payload). On cold start
        // (cc_entry still 0), wrapping_sub yields 0xF — this is a
        // harmless cold-start stamp because the receiver hasn't seen
        // a baseline on this PID yet; the next payload packet will
        // stamp CC=0 which the receiver accepts as a clean +1 from
        // the 0xF baseline.
        cc_entry.wrapping_sub(1) & 0x0F
    };
    pkt[3] = (afc << 4) | new_cc;
    pkt
}

/// Build one PAT TS packet + one PMT TS packet per program and append
/// them to `buf`, flushing intermediate bundles as needed so a PSI tick
/// never overflows past a bundle boundary.
///
/// `pat_version` and `pmt_versions` are bumped on plan change by
/// [`run_assembler`]; `push_psi` only stamps them onto the
/// section bytes.
#[allow(clippy::too_many_arguments)]
fn push_psi(
    buf: &mut BytesMut,
    plan: &AssemblyPlan,
    pcr_out_pid_by_program: &[u16],
    pat_version: u8,
    pmt_versions: &PmtVersions,
    pat_cc: &mut u8,
    cc: &mut HashMap<u16, u8>,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    bundle_seq: &mut u16,
    es_info: &HashMap<u16, Vec<u8>>,
    rewriter: Option<&mut crate::engine::ts_pts_rewriter::TsPtsRewriter>,
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
    // Reborrow rewriter so we can use it across multiple append_ts calls
    // (each consumes the borrow; we hand out fresh borrows from the
    // Option). `as_deref_mut` produces `Option<&mut TsPtsRewriter>` from
    // `Option<&mut TsPtsRewriter>` shape-equivalent.
    let mut rewriter = rewriter;
    append_ts(buf, &pat, broadcast_tx, bundle_seq, rewriter.as_deref_mut());

    // One PMT per program on its own `pmt_pid`, with that PID's CC counter.
    for (pidx, prog) in plan.programs.iter().enumerate() {
        let cc_entry = cc.entry(prog.pmt_pid).or_insert(0);
        let pcr_out_pid = pcr_out_pid_by_program
            .get(pidx)
            .copied()
            .unwrap_or(prog.pmt_pid);
        let pmt_version = pmt_versions.of(prog);
        let pmt = build_pmt(
            prog.program_number,
            prog.pmt_pid,
            pcr_out_pid,
            &prog.slots,
            pmt_version,
            *cc_entry,
            es_info,
        );
        *cc_entry = cc_entry.wrapping_add(1) & 0x0F;
        append_ts(buf, &pmt, broadcast_tx, bundle_seq, rewriter.as_deref_mut());
    }
}

/// Append one 188-byte TS packet to the bundle buffer, flushing when the
/// buffer reaches capacity.
fn append_ts(
    buf: &mut BytesMut,
    pkt: &[u8; TS_PACKET_SIZE],
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    bundle_seq: &mut u16,
    rewriter: Option<&mut crate::engine::ts_pts_rewriter::TsPtsRewriter>,
) {
    buf.extend_from_slice(pkt);
    if buf.len() >= BUNDLE_BYTES {
        flush(buf, broadcast_tx, bundle_seq, rewriter);
    }
}

/// Emit whatever is in `buf` as one `RtpPacket` (raw TS) onto the
/// broadcast channel and reset the buffer.
///
/// **Muxer-mode rewrite**: when `rewriter` is `Some`, the assembled
/// bundle bytes pass through `TsPtsRewriter::process` before being
/// wrapped in the `RtpPacket`. This is the industry-standard mux
/// behaviour for PID-bus / Node-Bus flows — one shared anchor across
/// every input contributing to the output, master-clock-derived PCR
/// + PES PTS, PCR_RR + PSI_RR + DI=1 + SCTE-35 compliance applied to
/// the *assembled* output (not per-input, which would produce
/// mismatched anchors). When `None`, source PCR + PES PTS flow
/// through unchanged (legacy behaviour, used by tests that don't
/// build a flow context).
///
/// Stamps a monotonic u16 sequence number and derives a 90 kHz RTP
/// timestamp from the current wall clock so downstream RTP / SRT /
/// FEC outputs can treat the assembler as a first-class RTP source.
/// No-op when `buf` is empty.
fn flush(
    buf: &mut BytesMut,
    broadcast_tx: &broadcast::Sender<RtpPacket>,
    bundle_seq: &mut u16,
    rewriter: Option<&mut crate::engine::ts_pts_rewriter::TsPtsRewriter>,
) {
    if buf.is_empty() {
        return;
    }
    let replaced = std::mem::replace(buf, BytesMut::with_capacity(BUNDLE_BYTES));
    let source: Bytes = replaced.freeze();
    let bundle = if let Some(rw) = rewriter {
        let mut rewritten = Vec::with_capacity(source.len());
        rw.process(&source, &mut rewritten);
        Bytes::from(rewritten)
    } else {
        source
    };
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
        upstream_seq: None,
        upstream_leg_id: None,
        sender_timestamp_us: None,
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

/// Largest `section_length` that keeps the whole PMT section inside one
/// TS packet: 188 − 4 (TS header) − 1 (pointer_field) − 3 (table_id +
/// section_length bytes) = 180. The assembler emits single-packet PMTs;
/// copy-through descriptors that would overflow are dropped per slot
/// (codec-identifying tags are small — overflow needs many slots with
/// fat descriptor loops).
const PMT_MAX_SECTION_LENGTH: usize = 180;

fn build_pmt(
    program_number: u16,
    pmt_pid: u16,
    pcr_pid: u16,
    slots: &[AssemblySlot],
    version: u8,
    cc: u8,
    es_info: &HashMap<u16, Vec<u8>>,
) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40 | (((pmt_pid >> 8) as u8) & 0x1F);
    pkt[2] = (pmt_pid & 0xFF) as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4] = 0x00; // pointer_field
    // Per-slot copy-through descriptors, greedily admitted in slot order
    // while the section still fits in one packet. A slot whose loop
    // doesn't fit emits with es_info_length = 0 (yesterday's behaviour)
    // rather than splitting the section.
    let mut kept: Vec<&[u8]> = Vec::with_capacity(slots.len());
    let mut es_total: usize = 0;
    let base_len = 9 + 5 * slots.len() + 4;
    for slot in slots {
        let d = es_info
            .get(&slot.out_pid)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if !d.is_empty() && d.len() <= 0x3FF && base_len + es_total + d.len() <= PMT_MAX_SECTION_LENGTH {
            kept.push(d);
            es_total += d.len();
        } else {
            kept.push(&[]);
        }
    }
    // Section body length (after `section_length` field):
    //   9 bytes header (program_number..program_info_length)
    //   + 5 bytes per ES entry + per-entry descriptor bytes
    //   + 4 bytes CRC
    let section_length: u16 = (base_len + es_total) as u16;
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
    for (slot, desc) in slots.iter().zip(kept.iter()) {
        pkt[pos] = slot.stream_type;
        pkt[pos + 1] = 0xE0 | (((slot.out_pid >> 8) as u8) & 0x1F);
        pkt[pos + 2] = (slot.out_pid & 0xFF) as u8;
        pkt[pos + 3] = 0xF0 | (((desc.len() >> 8) as u8) & 0x0F);
        pkt[pos + 4] = (desc.len() & 0xFF) as u8;
        pkt[pos + 5..pos + 5 + desc.len()].copy_from_slice(desc);
        pos += 5 + desc.len();
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

/// Copy-through whitelist for source-PMT ES descriptors onto the
/// assembled output. Codec identity (DVB AC-3 0x6A / E-AC-3 0x7A / DTS
/// 0x7B / AAC 0x7C / registration 0x05 / extension 0x7F), language
/// (ISO-639 0x0A), stream identifier (0x52), and teletext / subtitling
/// composition (0x56 / 0x59) survive; CA and network-private tags must
/// not ride through — the assembled mux is not the source mux.
fn es_descriptor_whitelisted(tag: u8) -> bool {
    matches!(
        tag,
        0x05 | 0x0A | 0x52 | 0x56 | 0x59 | 0x6A | 0x7A | 0x7B | 0x7C | 0x7F
    )
}

/// Filter a raw ES_info descriptor loop through the whitelist,
/// preserving order. Truncated loops drop the malformed tail.
fn filter_es_descriptors(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 2 <= raw.len() {
        let tag = raw[pos];
        let len = raw[pos + 1] as usize;
        if pos + 2 + len > raw.len() {
            break;
        }
        if es_descriptor_whitelisted(tag) {
            out.extend_from_slice(&raw[pos..pos + 2 + len]);
        }
        pos += 2 + len;
    }
    out
}

/// Re-resolve every slot's copy-through ES descriptors from the
/// per-input PSI catalogues. Returns the `program_number`s whose descriptor
/// set changed so the caller can bump those PMT versions (skipped on
/// install, whose PMTs are the first).
///
/// `keep_on_miss`: whether a slot whose source has no catalogue
/// entry *right now* (input restarting, catalogue not yet observed)
/// keeps the previously-resolved bytes instead of flapping to empty —
/// every one on the periodic tick, and on a plan swap those whose source it
/// leaves, so a retargeted slot never inherits the old source's descriptors.
fn refresh_slot_es_info(
    plan: &AssemblyPlan,
    catalogs: &HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
    es_info: &mut HashMap<u16, Vec<u8>>,
    keep_on_miss: impl Fn(&AssemblySlot) -> bool,
) -> Vec<u16> {
    let mut changed: Vec<u16> = Vec::new();
    let mut next: HashMap<u16, Vec<u8>> = HashMap::with_capacity(es_info.len());
    for prog in &plan.programs {
        for slot in &prog.slots {
            let key = slot.out_pid;
            // `Some(bytes)` = source ES found in its catalogue (bytes may
            // legitimately be empty); `None` = no catalogue / ES missing.
            let resolved: Option<Vec<u8>> = catalogs
                .get(&slot.source.0)
                .and_then(|store| store.load())
                .and_then(|cat| {
                    cat.programs
                        .iter()
                        .flat_map(|p| p.streams.iter())
                        .find(|s| s.pid == slot.source.1)
                        .map(|s| {
                            crate::engine::ts_psi_catalog::hex_decode(&s.descriptors_hex)
                                .unwrap_or_default()
                        })
                })
                .map(|raw| filter_es_descriptors(&raw));
            let bytes = match resolved {
                Some(b) => b,
                None if keep_on_miss(slot) => {
                    es_info.get(&key).cloned().unwrap_or_default()
                }
                None => Vec::new(),
            };
            if es_info.get(&key) != Some(&bytes) && !changed.contains(&prog.program_number) {
                changed.push(prog.program_number);
            }
            next.insert(key, bytes);
        }
    }
    *es_info = next;
    changed
}

/// MPEG-TS video `stream_type`s the assembler will auto-correct between
/// when the declared slot value contradicts the live source PMT. Kept to
/// the unambiguous video family — the correction must never move a slot
/// across the audio/video boundary (the PES-splice state machines key
/// their audio-vs-video map disjointness on `slot.stream_type`), and DVB
/// audio signalling (0x06 + descriptor) is legitimately declared under a
/// different byte than ATSC-style sources, so audio is out of scope.
fn is_correctable_video_stream_type(st: u8) -> bool {
    matches!(st, 0x01 | 0x02 | 0x1B | 0x24)
}

/// Live source PMT `stream_type` for `source`, from the per-input PSI
/// catalogue. `None` when the input has no catalogue yet (cold start,
/// synthesised PCM/AES3 carriers) or the PID isn't in its PMT.
fn observed_source_stream_type(
    catalogs: &HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
    source: &(String, u16),
) -> Option<u8> {
    catalogs.get(&source.0)?.load().and_then(|cat| {
        cat.programs
            .iter()
            .flat_map(|p| p.streams.iter())
            .find(|s| s.pid == source.1)
            .map(|s| s.stream_type)
    })
}

/// Reconcile every video slot's declared `stream_type` against the live
/// source PMT (PSI-tick cadence). An assembly declaring 0x24 (HEVC) over
/// an H.264 ES feeds H.264 NALs to downstream HEVC decoders — video
/// idles silently while audio keeps running (2026-06-12 defect #3). The
/// same path heals a live source codec change under an unchanged
/// assembly: the PMT correction bumps the program's version, so every
/// TS-demuxing consumer rebuilds its PES assembler and decoder without a
/// flow restart.
///
/// Corrections are debounced — the same observed value must hold for two
/// consecutive ticks (catalogue updates are already PMT-version-gated)
/// — and constrained to the video family in both directions. Returns
/// `(program_idx, out_pid, declared, observed, source)` per applied
/// correction; the caller mutates the flat view, bumps PMT versions and
/// emits the operator event.
fn reconcile_slot_stream_types(
    plan: &mut AssemblyPlan,
    catalogs: &HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
    stability: &mut HashMap<u16, (u8, u8)>,
) -> Vec<(usize, u16, u8, u8, (String, u16))> {
    let mut corrections = Vec::new();
    for (pidx, prog) in plan.programs.iter_mut().enumerate() {
        for slot in prog.slots.iter_mut() {
            if !is_correctable_video_stream_type(slot.stream_type) {
                continue;
            }
            // Switch slots: `slot.source` holds the INITIAL leg forever
            // (SwitchActiveInput mutates only `active_leg_input`), so a
            // catalogue comparison here would judge the declared byte
            // against an idle leg and could rewrite a PMT that is
            // correct for the live one. Leg codec compatibility is the
            // Take path's job — skip.
            if slot.switch_legs.is_some() {
                continue;
            }
            let key = slot.out_pid;
            let observed = match observed_source_stream_type(catalogs, &slot.source) {
                Some(o) => o,
                None => {
                    stability.remove(&key);
                    continue;
                }
            };
            if observed == slot.stream_type || !is_correctable_video_stream_type(observed) {
                stability.remove(&key);
                continue;
            }
            let entry = stability.entry(key).or_insert((observed, 0));
            if entry.0 != observed {
                *entry = (observed, 0);
            }
            entry.1 = entry.1.saturating_add(1);
            if entry.1 < 2 {
                continue;
            }
            stability.remove(&key);
            corrections.push((
                pidx,
                slot.out_pid,
                slot.stream_type,
                observed,
                slot.source.clone(),
            ));
            slot.stream_type = observed;
        }
    }
    corrections
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
/// returns one `((program_idx, slot_idx, leg_idx), pid)` per entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEssenceSlot {
    /// `(program_idx, slot_idx)` — program_idx is an index into
    /// `AssemblyPlan.programs`; slot_idx is an index into that
    /// program's `ProgramPlan.slots`.
    ///
    /// When `leg_idx.is_none()`, this is a top-level
    /// `SlotSource::Essence` slot — the runtime patches
    /// `plan.programs[program_idx].slots[slot_idx].source.1` in place.
    ///
    /// When `leg_idx.is_some()`, this is one Essence-typed leg of a
    /// `SlotSource::Switch` slot — the runtime patches
    /// `plan.programs[program_idx].slots[slot_idx].switch_legs[leg_idx].1`
    /// and, if this leg is the slot's currently-active source (matched
    /// on `input_id`), also patches `slots[slot_idx].source.1`.
    pub program_idx: usize,
    pub slot_idx: usize,
    pub input_id: String,
    pub kind: EsKind,
    /// `Some(leg_idx)` for Switch-leg essence resolution. `None` for
    /// top-level Essence slots.
    pub leg_idx: Option<usize>,
}

/// One pending Hitless merger task. Produced by `build_assembly_plan`
/// when it expands a [`crate::config::models::SlotSource::Hitless`]
/// slot into a synthetic bus key. The runtime walks `pending_hitless`
/// after essence resolution and spawns one
/// [`crate::engine::ts_es_hitless::spawn_hitless_es_merger_full`] per entry
/// before bringing the assembler up.
#[derive(Debug, Clone)]
pub struct PendingHitlessSlot {
    /// The slot's `out_pid`, which tells it from the plan's other slots
    /// wherever the plan has it.
    pub out_pid: u16,
    /// The flow run's scope of the bus key its merger publishes onto and the
    /// slot reads ([`crate::engine::ts_es_hitless::hitless_bus_key`]), so no
    /// other flow's slot, nor another run's, shares it.
    pub scope: String,
    /// Concrete `(input_id, source_pid)` for the primary leg.
    pub primary: (String, u16),
    /// Concrete `(input_id, source_pid)` for the backup leg.
    pub backup: (String, u16),
    /// Stall window in milliseconds before the merger flips primary →
    /// backup. Defaults to
    /// [`crate::engine::ts_es_hitless::DEFAULT_STALL_MS`] when the
    /// operator hasn't set it. A seq-aware merger reads it only if it falls
    /// back to primary preference.
    pub stall_ms: u64,
    /// When `true`, the merger uses true SMPTE 2022-7 seq-aware
    /// dedup + gap-fill (driven by `EsPacket.upstream_seq`) instead of
    /// stall-timer-based primary preference.
    pub seq_aware: bool,
    /// Gap-fill window of the seq-aware merger, in sequence numbers
    /// (multiple of 64 in 64..=4096). Validation refuses it on a slot
    /// that is not seq-aware, whose merger has no window.
    pub reorder_window: u16,
    /// Path-differential / skew-accommodation buffer in milliseconds.
    /// `Some(ms)` enables the buffered SMPTE 2022-7 merger (industry
    /// standard — emits at constant `ms` latency, hitlessly fills loss
    /// within the window). `None` falls back to the stateless dedup-
    /// only path.
    pub path_differential_ms: Option<u32>,
    /// How far one leg's packets reach a seq-aware merger behind the
    /// other's by configuration: the difference of the legs'
    /// [`crate::engine::ts_es_hitless::leg_receive_hold`]s as their inputs
    /// run, which the merger's stale time is sized to and follows. Zero on a
    /// primary-preference slot. Compares equal whatever it holds, so a plan
    /// swap never restarts a merger for it.
    pub leg_lag: crate::engine::ts_es_hitless::LegLag,
    /// The slot's [`HitlessLegs::forwarding_primary`], for the merger to
    /// flip.
    pub forwarding_primary: ForwardingPrimary,
    /// The slot's [`HitlessLegs::seq_fallback`], for the merger to set.
    pub seq_fallback: SeqFallback,
}

impl PendingHitlessSlot {
    /// Whether the merger of `other` is the one this slot wants: the same legs,
    /// merged the same way.
    pub fn merges_as(&self, other: &Self) -> bool {
        let settings = |slot: &Self| {
            (slot.stall_ms, slot.seq_aware, slot.reorder_window, slot.path_differential_ms)
        };
        (&self.primary, &self.backup) == (&other.primary, &other.backup)
            && settings(self) == settings(other)
    }
}

/// Output of `build_spts_plan` (defined on the runtime side): the
/// assembler plan plus any `Essence` slots still awaiting a concrete
/// `source_pid`, plus any `Hitless` slots that need a merger task spawned
/// before the assembler starts. Slots in `plan` corresponding to entries
/// in `pending_essence` have a sentinel `source_pid = 0` — the runtime
/// patches them in place after the resolver returns. Slots in `plan`
/// corresponding to entries in `pending_hitless` have already been
/// pointed at the synthetic merger output, so the assembler treats them
/// as a normal `(hitless:{flow_id}:{generation}:{uid}, 0)` source.
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
) -> Result<Vec<((usize, usize, Option<usize>), u16)>, EssenceResolveError> {
    // Pre-filter unsupported kinds so the poll loop only deals with
    // Video / Audio.
    for p in &pending {
        if !matches!(p.kind, EsKind::Video | EsKind::Audio) {
            return Err(EssenceResolveError::KindNotImplemented { kind: p.kind });
        }
    }

    let deadline = tokio::time::Instant::now() + timeout;
    let mut resolved: Vec<((usize, usize, Option<usize>), u16)> =
        Vec::with_capacity(pending.len());
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
                    resolved.push(((p.program_idx, p.slot_idx, p.leg_idx), pid));
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
            switch_legs: None,
            hitless_legs: None,
            splice_mode: Default::default(),
            splice_budget_ms: None,
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
            clocks: HashMap::new(),
        }
    }

    // ── Declared-vs-source stream_type reconciliation ───────────────────

    fn seed_catalog(input: &str, pid: u16, stream_type: u8) -> HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>> {
        use crate::engine::ts_psi_catalog::*;
        let store = PsiCatalogStore::new();
        PsiCatalogStore::seed_for_test(
            &store,
            PsiCatalog {
                programs: vec![CatalogProgram {
                    program_number: 1,
                    pmt_pid: 0x1000,
                    pcr_pid: Some(pid),
                    streams: vec![CatalogStream {
                        pid,
                        stream_type,
                        codec: "test".to_string(),
                        kind: CatalogStreamKind::Video,
                        descriptors_hex: String::new(),
                    }],
                }],
                last_updated_us: 1,
            },
        );
        let mut m = HashMap::new();
        m.insert(input.to_string(), Arc::new(store));
        m
    }

    /// A video slot declaring HEVC over an H.264 source corrects after
    /// two consecutive ticks (debounce) and reports the correction once.
    #[test]
    fn reconcile_corrects_video_stream_type_after_debounce() {
        let mut plan = make_plan(); // slot 0: in-a 0x100 → out 0x200, declared 0x1B
        plan.programs[0].slots[0].stream_type = 0x24; // operator typo: HEVC
        let catalogs = seed_catalog("in-a", 0x100, 0x1B); // source is H.264
        let mut stability = std::collections::HashMap::new();

        // Tick 1: observed but debounced — no correction yet.
        let c1 = reconcile_slot_stream_types(&mut plan, &catalogs, &mut stability);
        assert!(c1.is_empty());
        assert_eq!(plan.programs[0].slots[0].stream_type, 0x24);

        // Tick 2: correction applied.
        let c2 = reconcile_slot_stream_types(&mut plan, &catalogs, &mut stability);
        assert_eq!(c2.len(), 1);
        let (pidx, out_pid, declared, observed, source) = &c2[0];
        assert_eq!((*pidx, *out_pid, *declared, *observed), (0, 0x200, 0x24, 0x1B));
        assert_eq!(source, &("in-a".to_string(), 0x100));
        assert_eq!(plan.programs[0].slots[0].stream_type, 0x1B);

        // Tick 3: agreement — nothing further.
        let c3 = reconcile_slot_stream_types(&mut plan, &catalogs, &mut stability);
        assert!(c3.is_empty());
    }

    /// Audio slots and non-video observations never correct; missing
    /// catalogues (synthesised PCM carriers) are skipped entirely.
    #[test]
    fn reconcile_ignores_audio_and_missing_catalogs() {
        let mut plan = make_plan(); // slot 1: in-b audio 0x0F
        // Audio slot mismatching is out of scope even if a catalogue says so.
        let catalogs = seed_catalog("in-b", 0x200, 0x1B);
        let mut stability = std::collections::HashMap::new();
        for _ in 0..3 {
            assert!(reconcile_slot_stream_types(&mut plan, &catalogs, &mut stability).is_empty());
        }
        assert_eq!(plan.programs[0].slots[1].stream_type, 0x0F);
        // Video slot with NO catalogue for its input: skipped.
        assert_eq!(plan.programs[0].slots[0].stream_type, 0x1B);
    }

    // ── PMT descriptor copy-through ────────────────────────────────────

    #[test]
    fn build_pmt_carries_copy_through_descriptors() {
        let slots = vec![
            slot("a", 0x100, 0x100, 0x1B),
            slot("a", 0x101, 0x101, 0x06), // DVB private PES (e.g. AC-3)
        ];
        let mut es_info = std::collections::HashMap::new();
        es_info.insert(0x101u16, vec![0x6A, 0x01, 0x00]); // AC-3 descriptor
        let pkt = build_pmt(1, 0x1000, 0x100, &slots, 0, 0, &es_info);
        // Entries start at byte 17. Entry 0: video, empty loop.
        assert_eq!(pkt[17], 0x1B);
        assert_eq!(((pkt[20] as usize & 0x0F) << 8) | pkt[21] as usize, 0);
        // Entry 1 at 22: stream_type 0x06, 3-byte descriptor loop.
        assert_eq!(pkt[22], 0x06);
        assert_eq!(((pkt[25] as usize & 0x0F) << 8) | pkt[26] as usize, 3);
        assert_eq!(&pkt[27..30], &[0x6A, 0x01, 0x00]);
        // CRC valid over the section.
        let section_length = (((pkt[6] & 0x0F) as usize) << 8) | pkt[7] as usize;
        let crc_start = 5 + 3 + section_length - 4;
        let want = u32::from_be_bytes([
            pkt[crc_start],
            pkt[crc_start + 1],
            pkt[crc_start + 2],
            pkt[crc_start + 3],
        ]);
        assert_eq!(mpeg2_crc32(&pkt[5..crc_start]), want);
    }

    #[test]
    fn build_pmt_without_descriptors_matches_legacy_shape() {
        let slots = vec![slot("a", 0x100, 0x100, 0x1B)];
        let pkt = build_pmt(1, 0x1000, 0x100, &slots, 0, 0, &std::collections::HashMap::new());
        let section_length = (((pkt[6] & 0x0F) as usize) << 8) | pkt[7] as usize;
        assert_eq!(section_length, 9 + 5 + 4);
        assert_eq!(((pkt[20] as usize & 0x0F) << 8) | pkt[21] as usize, 0);
    }

    #[test]
    fn build_pmt_drops_descriptor_loops_that_overflow_one_packet() {
        let slots = vec![slot("a", 0x100, 0x100, 0x06)];
        let mut es_info = std::collections::HashMap::new();
        es_info.insert(0x100u16, vec![0x0A; 200]); // can't fit in 180
        let pkt = build_pmt(1, 0x1000, 0x100, &slots, 0, 0, &es_info);
        let section_length = (((pkt[6] & 0x0F) as usize) << 8) | pkt[7] as usize;
        assert_eq!(section_length, 9 + 5 + 4, "oversized loop must be dropped");
        assert_eq!(((pkt[20] as usize & 0x0F) << 8) | pkt[21] as usize, 0);
    }

    #[test]
    fn filter_es_descriptors_keeps_codec_tags_strips_ca() {
        // CA descriptor (0x09) must not ride through; AC-3 (0x6A) +
        // ISO-639 language (0x0A) must.
        let raw = [
            0x09, 0x04, 0x12, 0x34, 0xE0, 0x01, // CA — stripped
            0x6A, 0x00, // AC-3
            0x0A, 0x04, 0x65, 0x6E, 0x67, 0x00, // eng
        ];
        assert_eq!(
            filter_es_descriptors(&raw),
            vec![0x6A, 0x00, 0x0A, 0x04, 0x65, 0x6E, 0x67, 0x00]
        );
        // Truncated tail dropped.
        assert_eq!(filter_es_descriptors(&[0x6A, 0x05, 0x00]), Vec::<u8>::new());
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
            clocks: HashMap::new(),
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
        let pmt = build_pmt(42, 0x1000, 0x200, &slots, 0, 0, &std::collections::HashMap::new());
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

    /// Build an AF-only source packet (PCR-bearing, no PES payload).
    /// AFC = `0b10`. Used to exercise the H.222.0 §2.4.3.3 rule that
    /// AF-only packets MUST NOT advance the continuity counter — they
    /// carry the previously-issued payload's CC value verbatim.
    fn ts_af_only(src_pid: u16) -> Bytes {
        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = ((src_pid >> 8) as u8) & 0x1F;
        pkt[2] = (src_pid & 0xFF) as u8;
        pkt[3] = 0x20; // AFC = 0b10 (AF only, no payload), CC = 0
        pkt[4] = 183; // AF length = 183 (fills remaining 188-5)
        pkt[5] = 0x10; // PCR flag set
        // PCR bytes 6..12 zeroed — content not checked by rewrite_es_packet.
        Bytes::copy_from_slice(&pkt)
    }

    /// AF-only source packets MUST NOT advance the CC counter. The
    /// previous behaviour unconditionally advanced cc_table, stamping
    /// the AF-only packet with the next-payload CC value — receivers
    /// then saw a `+1` jump between the prior payload and the AF-only
    /// packet, counting one unexpected CC discontinuity per source
    /// AF-only packet (~1 per video PCR_RR period, accumulating to
    /// hundreds-per-minute on assembled SPTS outputs). Regression
    /// reference: Cell 8 / Finding #2 in
    /// `testbed/full_test_2026-05-21/v2/REPORT_v2.md`.
    #[test]
    fn rewrite_es_packet_af_only_does_not_advance_cc() {
        let mut cc_table = std::collections::HashMap::new();
        // Payload 1 → CC=0, cc_table advances to 1.
        let p1 = rewrite_es_packet(&ts_es(0x100, true, 5, 0xAB), 0x200, &mut cc_table);
        assert_eq!(ts_cc(&p1), 0, "first payload starts the CC sequence at 0");
        assert_eq!(*cc_table.get(&0x200).unwrap(), 1, "cc_table advances to 1 after payload");

        // AF-only between two payloads must stamp the prior payload's CC
        // (= 0, the last issued), NOT advance the counter.
        let af = rewrite_es_packet(&ts_af_only(0x100), 0x200, &mut cc_table);
        assert_eq!(
            ts_cc(&af),
            0,
            "AF-only must stamp the LAST PAYLOAD's CC (0), not the next-to-issue (1)"
        );
        assert_eq!(
            *cc_table.get(&0x200).unwrap(),
            1,
            "cc_table MUST NOT advance on AF-only — receivers expect AF-only's CC to equal the prior payload's"
        );

        // Next payload picks up CC=1 — a clean +1 from the prior payload.
        // Pre-fix code would have stamped CC=2 here, causing the receiver
        // to count an unexpected discontinuity at every AF-only packet.
        let p2 = rewrite_es_packet(&ts_es(0x100, false, 9, 0xCD), 0x200, &mut cc_table);
        assert_eq!(ts_cc(&p2), 1, "second payload is +1 from first (NOT +2 across the AF-only)");
        assert_eq!(*cc_table.get(&0x200).unwrap(), 2);
    }

    /// CC wrap is correctly preserved across AF-only packets. After
    /// emitting payloads 0..=15, cc_table wraps from 15→0; the next
    /// AF-only must stamp 15 (the prior payload's value, NOT
    /// `cc_table - 1 = 0xF` mishandled as cold-start).
    #[test]
    fn af_only_after_cc_wrap_stamps_prior_payload_value() {
        let mut cc_table = std::collections::HashMap::new();
        // Burn through CC values 0..=15 with payload packets.
        for _ in 0..16 {
            let _ = rewrite_es_packet(&ts_es(0x100, false, 0, 0), 0x200, &mut cc_table);
        }
        assert_eq!(*cc_table.get(&0x200).unwrap(), 0, "cc_table wrapped to 0 after 16 payloads");
        // AF-only after the wrap: stamp 15 (the last issued payload's
        // CC), NOT 0xF re-interpreted from cold start. wrapping_sub(1)
        // on 0 gives 0xFF, masked to 0x0F = 15 ✓ — happens to be the
        // same byte value as cold-start 0xF, but the meaning differs:
        // here it's a valid "last payload CC" rather than a cold-start
        // sentinel.
        let af = rewrite_es_packet(&ts_af_only(0x100), 0x200, &mut cc_table);
        assert_eq!(ts_cc(&af), 0x0F, "AF-only after wrap must stamp prior payload CC (15)");
        assert_eq!(*cc_table.get(&0x200).unwrap(), 0, "cc_table unchanged on AF-only");
        // Next payload picks up CC=0 — clean +1 wrap from 15.
        let next = rewrite_es_packet(&ts_es(0x100, false, 0, 0), 0x200, &mut cc_table);
        assert_eq!(ts_cc(&next), 0);
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
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(16);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None, None, None, Default::default()).join;

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
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None, None, None, Default::default()).join;

        // Wait for startup PSI, then publish enough ES packets on each
        // slot to fill a full bundle.
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;

        let video_tx = bus.sender_for("in-a", 0x100, 0x1B);
        let audio_tx = bus.sender_for("in-b", 0x200, 0x03);
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
                    upstream_seq: None,
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
                    upstream_seq: None,
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

    /// A Pid slot on an input whose id starts with `hitless:`, the Hitless
    /// mergers' key prefix: its fan-in subscribes before the input's first
    /// packet, and the packets the input then publishes are assembled. The
    /// bus judged its key a released merger output by the prefix alone and
    /// handed the fan-in a closed channel, so the slot stayed dark for the
    /// flow's life (`pid_bus_slot_source_closed`).
    #[tokio::test]
    async fn a_slot_on_an_input_named_like_a_merger_key_is_fed() {
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        // A failed assertion still stops the assembler.
        let _stop = cancel.clone().drop_guard();
        let plan = AssemblyPlan {
            programs: vec![ProgramPlan {
                program_number: 1,
                pmt_pid: 0x1000,
                pcr_source: ("hitless:cam1".to_string(), 0x100),
                slots: vec![slot("hitless:cam1", 0x100, 0x200, 0x1B)],
            }],
            clocks: HashMap::new(),
        };
        let handle = spawn_spts_assembler(plan, bus.clone(), tx, cancel.clone(), None, String::new(), None, None, None, Default::default()).join;
        // The startup PSI: the fan-in has subscribed, before any packet.
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;

        // The input's first packet makes its channel, as its demuxer does.
        let video_tx = bus.sender_for("hitless:cam1", 0x100, 0x1B);
        for i in 0..7u8 {
            video_tx
                .send(EsPacket {
                    source_pid: 0x100,
                    stream_type: 0x1B,
                    payload: ts_es(0x100, i == 0, i, 0x10 + i),
                    is_pusi: i == 0,
                    has_pcr: false,
                    pcr: None,
                    recv_time_us: 0,
                    upstream_seq: None,
                })
                .expect("the fan-in reads the input's channel");
        }
        let mut video_seen = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < deadline && video_seen < 7 {
            if let Ok(Ok(bundle)) =
                tokio::time::timeout(Duration::from_millis(30), rx.recv()).await
            {
                let video = bundle.data.chunks_exact(TS_PACKET_SIZE).filter(|p| ts_pid(p) == 0x200);
                video_seen += video.count();
            }
        }
        cancel.cancel();
        handle.await.unwrap();
        assert_eq!(video_seen, 7, "the slot forwards its input's packets on out_pid 0x200");
    }

    #[tokio::test]
    async fn assembler_mpts_emits_pat_with_two_programs_and_two_pmts() {
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_mpts_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None, None, None, Default::default()).join;

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
        let bus = Arc::new(NodeEsBus::new());
        let (tx, _rx) = broadcast::channel::<RtpPacket>(4);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None, None, None, Default::default()).join;
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
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();

        // Initial plan: in-a → out 0x200, in-b → out 0x201.
        let initial = make_plan();
        let handle = spawn_spts_assembler(initial, bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None, None, None, Default::default());

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
            // One clock, so the program is not held for in-c's PCR.
            clocks: clocks(&[("in-a", ClockIdentity::Wallclock), ("in-c", ClockIdentity::Wallclock)]),
        };
        handle
            .plan_tx
            .send(PlanCommand::ReplacePlan { plan: new_plan, splice_mode_override: None, catalogs: Default::default(), applied: None })
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
        let video_tx = bus.sender_for("in-c", 0x100, 0x1B);
        video_tx
            .send(EsPacket {
                source_pid: 0x100,
                stream_type: 0x1B,
                payload: ts_es(0x100, true, 0, 0xAB),
                is_pusi: true,
                has_pcr: false,
                pcr: None,
                recv_time_us: 0,
                upstream_seq: None,
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
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None, None, None, Default::default());

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
            .send(PlanCommand::ReplacePlan { plan: make_plan(), splice_mode_override: None, catalogs: Default::default(), applied: None })
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
            descriptors_hex: String::new(),
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
                    leg_idx: None,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(500))
            .await
            .expect("must resolve");
        assert_eq!(r, vec![((0, 0, None), 0x100)]);
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
                    leg_idx: None,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(r, vec![((0, 3, None), 0x200)], "first audio wins, not second AC-3");
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
                    leg_idx: None,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(r, vec![((0, 0, None), 0x100)]);
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
                    leg_idx: None,
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
                    leg_idx: None,
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
                    leg_idx: None,
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
                    leg_idx: None,
        }];
        let r = resolve_essence_slots(pending, cats, Duration::from_millis(1000))
            .await
            .expect("must pick up late catalogue within timeout");
        assert_eq!(r, vec![((0, 0, None), 0x100)]);
    }

    // ─── Cross-clock compatibility check ──────────────────────────────

    fn clocks(entries: &[(&str, ClockIdentity)]) -> HashMap<String, ClockIdentity> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn clock_compat_accepts_same_source_pcr_within_program() {
        let plan = make_plan();
        // Same input on both slots: trivially compatible.
        let map = clocks(&[
            ("in-a", ClockIdentity::SourcePcr { input_id: "in-a".into() }),
            ("in-b", ClockIdentity::SourcePcr { input_id: "in-a".into() }),
        ]);
        check_assembly_clock_compatibility(&plan, &map).expect("two slots, same clock → OK");
    }

    #[test]
    fn clock_compat_rejects_two_source_pcrs_in_one_program() {
        // The realistic Phase 2 failure mode: video from encoder A,
        // audio from encoder B. Each has its own SourcePcr identity.
        let plan = make_plan();
        let map = clocks(&[
            ("in-a", ClockIdentity::SourcePcr { input_id: "in-a".into() }),
            ("in-b", ClockIdentity::SourcePcr { input_id: "in-b".into() }),
        ]);
        let err = check_assembly_clock_compatibility(&plan, &map)
            .expect_err("must reject cross-encoder PCR mix");
        assert_eq!(err.program_number, 1);
        assert_eq!(err.slot_out_pid, 0x201);
        assert_eq!(err.slot_input_id, "in-b");
        assert_eq!(err.reference_input_id, "in-a");
        assert!(err.slot_clock.contains("in-b"));
        assert!(err.reference_clock.contains("in-a"));
    }

    #[test]
    fn clock_compat_accepts_coherent_ptp_inputs() {
        // Two ST 2110 inputs on the same PTP domain: co-clocked.
        let plan = make_plan();
        let map = clocks(&[
            ("in-a", ClockIdentity::Ptp { domain: 127 }),
            ("in-b", ClockIdentity::Ptp { domain: 127 }),
        ]);
        check_assembly_clock_compatibility(&plan, &map)
            .expect("same PTP domain → OK");
    }

    #[test]
    fn clock_compat_rejects_cross_ptp_domain() {
        let plan = make_plan();
        let map = clocks(&[
            ("in-a", ClockIdentity::Ptp { domain: 0 }),
            ("in-b", ClockIdentity::Ptp { domain: 127 }),
        ]);
        let err = check_assembly_clock_compatibility(&plan, &map)
            .expect_err("cross-domain PTP must be rejected");
        assert!(err.slot_clock.contains("domain=127"));
        assert!(err.reference_clock.contains("domain=0"));
    }

    #[test]
    fn clock_compat_rejects_kind_mix() {
        let plan = make_plan();
        let map = clocks(&[
            ("in-a", ClockIdentity::Ptp { domain: 0 }),
            ("in-b", ClockIdentity::SourcePcr { input_id: "in-b".into() }),
        ]);
        let err = check_assembly_clock_compatibility(&plan, &map)
            .expect_err("PTP + SourcePcr is never coherent");
        assert_eq!(err.slot_input_id, "in-b");
    }

    #[test]
    fn clock_compat_allows_cross_program_difference_in_mpts() {
        // MPTS receivers re-acquire per program, so an MPTS with
        // program 1 from input A and program 2 from input B is fine
        // even when A and B have different clocks.
        let plan = make_mpts_plan();
        let map = clocks(&[
            ("in-a", ClockIdentity::SourcePcr { input_id: "in-a".into() }),
            ("in-b", ClockIdentity::SourcePcr { input_id: "in-b".into() }),
        ]);
        check_assembly_clock_compatibility(&plan, &map)
            .expect("two programs from different sources is allowed");
    }

    fn source_pcr(id: &str) -> ClockIdentity {
        ClockIdentity::SourcePcr { input_id: id.into() }
    }

    /// A slot with a leg on each of `legs` (the PID tells the slot's own apart),
    /// starting on the first.
    fn switch_slot(legs: &[&str], src_pid: u16, out_pid: u16, stream_type: u8) -> AssemblySlot {
        let mut s = slot(legs[0], src_pid, out_pid, stream_type);
        s.switch_legs = Some(legs.iter().map(|leg| (leg.to_string(), src_pid)).collect());
        s
    }

    /// One program of `slots`, its PCR on the first, every input on a clock
    /// of its own.
    fn one_program(slots: Vec<AssemblySlot>) -> AssemblyPlan {
        let mut clocks = HashMap::new();
        for id in ["a", "b", "c"] {
            clocks.insert(id.to_string(), source_pcr(id));
        }
        AssemblyPlan {
            programs: vec![ProgramPlan {
                program_number: 1,
                pmt_pid: 0x1000,
                pcr_source: slots[0].source.clone(),
                slots,
            }],
            clocks,
        }
    }

    /// Two encoders switched as a whole: the legs of each slot are
    /// alternatives, and both slots follow every Take.
    #[test]
    fn clock_compat_accepts_a_program_that_switches_as_one() {
        let plan = one_program(vec![
            switch_slot(&["a", "b"], 0x100, 0x200, 0x1B),
            switch_slot(&["a", "b"], 0x101, 0x201, 0x0F),
        ]);
        check_assembly_clock_compatibility(&plan, &plan.clocks).expect("never on two clocks");
    }

    /// What a Switch slot can be left beside: a slot pinned to one of its
    /// legs, a Switch slot without one of its legs, and one that starts on
    /// another leg. The refusal names the two inputs and what brings them
    /// on air together.
    #[test]
    fn clock_compat_refuses_a_program_a_take_would_split() {
        let pinned = one_program(vec![
            switch_slot(&["a", "b"], 0x100, 0x200, 0x1B),
            slot("a", 0x101, 0x201, 0x0F),
        ]);
        let err = check_assembly_clock_compatibility(&pinned, &pinned.clocks)
            .expect_err("a Take to b leaves the audio on a");
        assert_eq!(
            *err,
            MasterClockMismatch {
                program_number: 1,
                slot_out_pid: 0x201,
                slot_input_id: "a".into(),
                slot_clock: "source_pcr:input=a".into(),
                reference_out_pid: 0x200,
                reference_input_id: "b".into(),
                reference_clock: "source_pcr:input=b".into(),
                take_input_id: Some("b".into()),
            }
        );
        assert_eq!(
            err.to_string(),
            "program 1 slot 0x0201 input 'a' has master clock source_pcr:input=a but slot \
             0x0200 plays input 'b' on clock source_pcr:input=b beside it after a Take to 'b' \
             — combining incompatible clocks would silently drift"
        );
        assert_eq!(
            err.details(),
            serde_json::json!({
                "error_code": "pid_bus_master_clock_mismatch",
                "program_number": 1,
                "slot_out_pid": 0x201,
                "slot_input_id": "a",
                "slot_clock": "source_pcr:input=a",
                "reference_out_pid": 0x200,
                "reference_input_id": "b",
                "reference_clock": "source_pcr:input=b",
                "take_input_id": "b",
            })
        );

        let third = one_program(vec![
            switch_slot(&["a", "b"], 0x100, 0x200, 0x1B),
            switch_slot(&["a", "b", "c"], 0x101, 0x201, 0x0F),
        ]);
        let err = check_assembly_clock_compatibility(&third, &third.clocks)
            .expect_err("a Take to c leaves the video behind");
        assert_eq!(err.take_input_id.as_deref(), Some("c"));
        assert_eq!((err.reference_input_id.as_str(), err.slot_input_id.as_str()), ("a", "c"));

        let start = one_program(vec![
            switch_slot(&["a", "b"], 0x100, 0x200, 0x1B),
            switch_slot(&["b", "a"], 0x101, 0x201, 0x0F),
        ]);
        let err = check_assembly_clock_compatibility(&start, &start.clocks)
            .expect_err("it starts on a and b");
        assert_eq!(err.take_input_id, None);
        assert!(err.to_string().contains("beside it from the start"), "{err}");
    }

    /// What [`clock_rule_matches_every_small_plan`] runs a plan on: the inputs
    /// of each slot and the one it starts on.
    type Legs = (Vec<usize>, usize);

    /// The plan of `slots`.
    fn plan_of(slots: &[Legs]) -> AssemblyPlan {
        let slots = slots.iter().enumerate().map(|(i, (legs, start))| {
            let mut s = slot(IDS[*start], 0x100, 0x200 + i as u16, 0x1B);
            if legs.len() > 1 {
                s.switch_legs = Some(legs.iter().map(|l| (IDS[*l].to_string(), 0x100)).collect());
            }
            s
        });
        one_program(slots.collect())
    }

    /// Every state a plan can be in, by playing Takes to `takeable` from the
    /// start: what each slot is on.
    fn states(slots: &[Legs], takeable: &[usize]) -> std::collections::HashSet<Vec<usize>> {
        let mut seen = std::collections::HashSet::new();
        let mut todo = vec![slots.iter().map(|(_, start)| *start).collect::<Vec<_>>()];
        while let Some(state) = todo.pop() {
            if !seen.insert(state.clone()) {
                continue;
            }
            for &take in takeable {
                todo.push(after_take(slots, &state, take));
            }
        }
        seen
    }

    fn after_take(slots: &[Legs], state: &[usize], take: usize) -> Vec<usize> {
        let on = |((legs, _), now): (&Legs, &usize)| if legs.contains(&take) { take } else { *now };
        slots.iter().zip(state).map(on).collect()
    }

    const IDS: [&str; 3] = ["x", "y", "z"];

    /// Every leg set of three inputs with every start, a slot each.
    fn every_slot() -> Vec<Legs> {
        (1..8usize)
            .flat_map(|set| {
                let legs: Vec<usize> = (0..3).filter(|i| set & (1 << i) != 0).collect();
                legs.clone().into_iter().map(move |start| (legs.clone(), start))
            })
            .collect()
    }

    /// Every plan of up to three slots.
    fn every_plan() -> Vec<Vec<Legs>> {
        let slots = every_slot();
        let mut all = Vec::new();
        let mut plans: Vec<Vec<Legs>> = vec![Vec::new()];
        for _ in 0..3 {
            plans = plans
                .iter()
                .flat_map(|plan| {
                    slots.iter().map(move |s| plan.iter().cloned().chain([s.clone()]).collect())
                })
                .collect();
            all.extend(plans.iter().cloned());
        }
        all
    }

    /// Whether `state` plays the pair `err` names.
    fn plays(state: &[usize], err: &MasterClockMismatch) -> bool {
        let on = |out_pid: u16| IDS[state[(out_pid - 0x200) as usize]];
        on(err.slot_out_pid) == err.slot_input_id
            && on(err.reference_out_pid) == err.reference_input_id
    }

    /// The check against a run of every plan of up to three slots on three
    /// inputs, each on one of two clocks or on none known (another flow's
    /// input, which no Take reaches): refused exactly when some run of Takes
    /// puts two slots on inputs of different clocks, or one on two clocks
    /// beside another that stays on an input of unknown clock, with a pair
    /// such a run does put on air, brought there by the Take it names.
    #[test]
    fn clock_rule_matches_every_small_plan() {
        let plans = every_plan();
        let (mut refused, mut accepted) = (0, 0);
        for clocks in 0..27usize {
            let clock = |input: usize| match clocks / 3usize.pow(input as u32) % 3 {
                0 => None,
                domain => Some(domain as u8),
            };
            let map: HashMap<String, ClockIdentity> = (0..3)
                .filter_map(|i| clock(i).map(|d| (IDS[i].to_string(), ClockIdentity::Ptp { domain: d })))
                .collect();
            let takeable: Vec<usize> = (0..3).filter(|i| clock(*i).is_some()).collect();
            for legs in &plans {
                let plan = plan_of(legs);
                let states = states(legs, &takeable);
                let known_mix = states.iter().any(|state| {
                    let mut known = state.iter().filter_map(|i| clock(*i));
                    let first = known.next();
                    known.any(|c| Some(c) != first)
                });
                // A slot on an input whose clock is not known, with another
                // slot on one clock in one state and on another in the next.
                let mut beside = std::collections::HashMap::new();
                let unknown_mix = states.iter().any(|state| {
                    (0..state.len()).any(|j| {
                        clock(state[j]).is_none()
                            && (0..state.len()).filter_map(|i| Some((i, clock(state[i])?))).any(
                                |(i, c)| *beside.entry((j, state[j], i)).or_insert(c) != c,
                            )
                    })
                });
                let mixes = known_mix || unknown_mix;
                match check_assembly_clock_compatibility(&plan, &map) {
                    Ok(()) => {
                        assert!(!mixes, "accepted {legs:?} on clocks {clocks}");
                        accepted += 1;
                    }
                    Err(err) => {
                        assert!(mixes, "refused {legs:?} on clocks {clocks}: {err}");
                        assert!(err.slot_clock != err.reference_clock);
                        let by_unknown =
                            err.slot_clock == "unknown" || err.reference_clock == "unknown";
                        assert!(if by_unknown { unknown_mix } else { known_mix }, "{legs:?}: {err}");
                        let start: Vec<usize> = legs.iter().map(|(_, start)| *start).collect();
                        match &err.take_input_id {
                            None => assert!(plays(&start, &err), "{legs:?}: {err}"),
                            Some(id) => {
                                let take = IDS.iter().position(|i| i == id).unwrap();
                                assert!(
                                    states.iter().any(|from| {
                                        !plays(from, &err)
                                            && plays(&after_take(legs, from, take), &err)
                                    }),
                                    "{legs:?}: {err}"
                                );
                            }
                        }
                        refused += 1;
                    }
                }
            }
        }
        // 12 slots, so 12 + 144 + 1728 plans, on 27 clock settings.
        assert_eq!(refused + accepted, 27 * 1884);
        assert!(refused > 1000 && accepted > 1000, "{refused} refused, {accepted} accepted");
    }

    /// What a slot is on, and what a splice pending takes it to.
    type OnTo = (usize, Option<usize>);

    /// Every state Takes put `slots` in from `from`: one on another input of
    /// the same clock is a cut, or a splice that lands before the next or not.
    fn spliced_states(
        slots: &[Legs],
        from: Vec<OnTo>,
        clock: &[usize; 3],
    ) -> HashSet<Vec<OnTo>> {
        let mut seen = HashSet::new();
        let mut todo = vec![from];
        while let Some(state) = todo.pop() {
            if !seen.insert(state.clone()) {
                continue;
            }
            for i in 0..state.len() {
                if let Some(to) = state[i].1 {
                    let mut landed = state.clone();
                    landed[i] = (to, None);
                    todo.push(landed);
                }
            }
            for take in 0..3 {
                for splice in [false, true] {
                    let taken = |((legs, _), (on, to)): (&Legs, &OnTo)| {
                        if !legs.contains(&take) {
                            (*on, *to)
                        } else if splice && *on != take && clock[*on] == clock[take] {
                            (*on, Some(take))
                        } else {
                            (take, None)
                        }
                    };
                    todo.push(slots.iter().zip(&state).map(taken).collect());
                }
            }
        }
        seen
    }

    /// [`legs_after_swap`], from every state a plan of three slots the check
    /// accepts can be in, any of them new to the swap, to every other such
    /// plan: no Take after the swap and no splice it left pending puts two of
    /// them on different clocks. Its slots may come from two programs of the
    /// plan before, in states of their own, whether the program is one of them
    /// or new.
    #[test]
    fn a_plan_swap_mixes_no_clocks() {
        let mut swaps = 0;
        for clock in [[0, 1, 2], [0, 0, 1]] {
            let map: HashMap<String, ClockIdentity> = (0..3)
                .map(|i| (IDS[i].to_string(), ClockIdentity::Ptp { domain: clock[i] as u8 }))
                .collect();
            let accepted: Vec<Vec<Legs>> = every_plan()
                .into_iter()
                .filter(|legs| legs.len() == 3)
                .filter(|legs| check_assembly_clock_compatibility(&plan_of(legs), &map).is_ok())
                .collect();
            let start = |legs: &[Legs]| legs.iter().map(|(_, start)| (*start, None)).collect();
            let before: HashSet<Vec<OnTo>> = accepted
                .iter()
                .flat_map(|legs| spliced_states(legs, start(legs), &clock))
                .collect();
            // What the slots of each set (a bit each) can be on at once.
            let places: Vec<HashSet<Vec<Option<OnTo>>>> = (0..8)
                .map(|set| {
                    let of = |state: &Vec<OnTo>| {
                        (0..3).map(|i| (set & (1 << i) != 0).then_some(state[i])).collect()
                    };
                    before.iter().map(of).collect()
                })
                .collect();
            let key = |i: usize| 0x200 + i as u16;
            for legs in &accepted {
                let plan = plan_of(legs);
                let mut checked = HashSet::new();
                // The slots kept of program 0 of the plan before, and of 1.
                let sets = (0..8).flat_map(|a| (0..8).map(move |b| (a, b))).filter(|(a, b)| a & b == 0);
                let pairs = sets.flat_map(|(a, b)| {
                    places[a].iter().flat_map(|x| places[b].iter().map(move |y| (x, y))).collect::<Vec<_>>()
                });
                for ((x, y), program) in pairs.flat_map(|p| [Some(0), None].map(|program| (p, program))) {
                    let switched: HashMap<u16, Switched> = (0..3)
                        .filter_map(|i| {
                            let program = usize::from(x[i].is_none());
                            let (on, to) = x[i].or(y[i])?;
                            let (on, to) = (IDS[on].to_string(), to.map(|to| IDS[to].to_string()));
                            Some((key(i), Switched { program, on, to }))
                        })
                        .collect();
                    let (now, spliced) = legs_after_swap(&plan, &[program], &switched);
                    let after: Vec<OnTo> = (0..3)
                        .map(|i| {
                            let Some(leg) = now.get(&key(i)) else { return (legs[i].1, None) };
                            let leg = IDS.iter().position(|id| id == leg).unwrap();
                            let to = x[i].or(y[i]).and_then(|(_, to)| to);
                            (leg, to.filter(|_| spliced.contains(&key(i))))
                        })
                        .collect();
                    swaps += 1;
                    if !checked.insert(after.clone()) {
                        continue;
                    }
                    for then in spliced_states(legs, after.clone(), &clock) {
                        assert!(
                            then.iter().all(|(on, _)| clock[*on] == clock[then[0].0]),
                            "{x:?} and {y:?} to {legs:?} on clocks {clock:?}: {after:?}, {then:?}"
                        );
                        assert!(
                            then.iter().zip(legs).all(|((on, to), (legs, _))| {
                                legs.contains(on) && to.is_none_or(|to| legs.contains(&to))
                            }),
                            "{x:?} and {y:?} to {legs:?}: {after:?}"
                        );
                    }
                }
            }
        }
        assert!(swaps > 10_000, "{swaps}");
    }

    /// [`check_added_input_clock`] against the same plans, `z` the input
    /// joining the flow: refused exactly when a Take to `z`, or another that
    /// leaves a slot on `z`, brings it on air beside a slot on another clock.
    #[test]
    fn added_input_rule_matches_every_small_plan() {
        let mut refused = 0;
        for clocks in 0..4u8 {
            // x and y on one clock or two, z on the first or on its own.
            let domains = [1, 1 + (clocks & 1), if clocks & 2 == 0 { 1 } else { 3 }];
            let map: HashMap<String, ClockIdentity> = (0..3)
                .map(|i| (IDS[i].to_string(), ClockIdentity::Ptp { domain: domains[i] }))
                .collect();
            for legs in &every_plan() {
                let streams: Vec<serde_json::Value> = legs
                    .iter()
                    .enumerate()
                    .map(|(i, (legs, start))| {
                        let leg = |l: usize| {
                            serde_json::json!({ "type": "pid", "input_id": IDS[l], "source_pid": 256 })
                        };
                        let source = if legs.len() > 1 {
                            serde_json::json!({
                                "type": "switch", "initial_input_id": IDS[*start],
                                "legs": legs.iter().map(|l| leg(*l)).collect::<Vec<_>>()
                            })
                        } else {
                            leg(legs[0])
                        };
                        serde_json::json!({
                            "source": source, "out_pid": 0x200 + i, "stream_type": 27
                        })
                    })
                    .collect();
                let assembly: crate::config::models::FlowAssembly =
                    serde_json::from_value(serde_json::json!({
                        "kind": "spts",
                        "programs": [{
                            "program_number": 1, "pmt_pid": 4096, "streams": streams
                        }]
                    }))
                    .expect("assembly");
                // The pairs of slots a Take moves one of onto its place in,
                // with a slot that can move on `z`, and the Takes.
                let mut brought = std::collections::HashMap::<_, Vec<&str>>::new();
                for from in states(legs, &[0, 1, 2]) {
                    for take in 0..3 {
                        let to = after_take(legs, &from, take);
                        for j in 0..legs.len() {
                            for i in 0..j {
                                let z_moves = |s: usize| to[s] == 2 && legs[s].0.len() > 1;
                                if (from[i], from[j]) != (to[i], to[j]) && (z_moves(i) || z_moves(j))
                                    && domains[to[i]] != domains[to[j]]
                                {
                                    brought.entry((i, to[i], j, to[j])).or_default().push(IDS[take]);
                                }
                            }
                        }
                    }
                }
                match check_added_input_clock(&assembly, &map, "z") {
                    Ok(()) => assert!(brought.is_empty(), "{legs:?} {domains:?}: {brought:?}"),
                    Err(err) => {
                        let at = |out_pid: u16| (out_pid - 0x200) as usize;
                        let id = |id: &str| IDS.iter().position(|i| *i == id).unwrap();
                        let pair = (
                            at(err.reference_out_pid),
                            id(&err.reference_input_id),
                            at(err.slot_out_pid),
                            id(&err.slot_input_id),
                        );
                        let takes = brought.get(&pair).map_or(&[][..], Vec::as_slice);
                        let take = err.take_input_id.as_deref().expect("a Take");
                        assert!(takes.contains(&take), "{legs:?} {domains:?}: {err}");
                        refused += 1;
                    }
                }
            }
        }
        assert!(refused > 1000, "{refused} refused");
    }

    /// A Hitless slot fails over by itself, so its legs are not judged, but no
    /// Take moves it: beside it a Switch slot cannot change clock, and one on
    /// one clock can switch. The refusal names its primary, and whichever leg
    /// of the Switch slot is not on that one's clock.
    #[test]
    fn clock_compat_refuses_a_take_beside_a_hitless_slot() {
        let hitless = |primary: &str| {
            let mut s = slot("hitless:f1:0:slot_0_1", 0, 0x201, 0x0F);
            s.hitless_legs = Some(HitlessLegs {
                primary: primary.to_string(),
                backup: "c".to_string(),
                forwarding_primary: Default::default(),
                seq_fallback: Default::default(),
            });
            s
        };
        for (primary, switch_input, clock, take) in [
            ("a", "b", "source_pcr:input=a", Some("b")),
            ("b", "a", "source_pcr:input=b", None),
            ("elsewhere", "b", "unknown", Some("b")),
        ] {
            let mut switch = switch_slot(&["a", "b"], 0x100, 0x200, 0x1B);
            switch.switch_legs.as_mut().unwrap().reverse();
            let mut plan = one_program(vec![switch, hitless(primary)]);
            let err = check_assembly_clock_compatibility(&plan, &plan.clocks)
                .expect_err("a Take leaves the Hitless slot");
            assert_eq!((err.slot_out_pid, err.reference_out_pid), (0x201, 0x200));
            assert_eq!((err.slot_input_id.as_str(), err.slot_clock.as_str()), (primary, clock));
            assert_eq!(err.reference_input_id, switch_input);
            assert_eq!(err.take_input_id.as_deref(), take);

            plan.programs[0].slots.reverse();
            let err = check_assembly_clock_compatibility(&plan, &plan.clocks)
                .expect_err("in either order");
            assert_eq!((err.slot_out_pid, err.slot_input_id.as_str()), (0x200, switch_input));
            assert_eq!((err.reference_out_pid, err.reference_input_id.as_str()), (0x201, primary));
            assert_eq!(err.reference_clock, clock);

            plan.clocks.insert("b".into(), source_pcr("a"));
            check_assembly_clock_compatibility(&plan, &plan.clocks).expect("one clock");
        }
        let fixed = one_program(vec![slot("a", 0x100, 0x200, 0x1B), hitless("b")]);
        check_assembly_clock_compatibility(&fixed, &fixed.clocks).expect("not judged on its legs");
    }

    /// Nor does an input join whose Take would leave a Hitless slot, or a slot
    /// on another flow's input, on a clock the program played before.
    #[test]
    fn added_input_rule_keeps_a_hitless_slot_company() {
        let assembly = |still: serde_json::Value| -> crate::config::models::FlowAssembly {
            serde_json::from_value(serde_json::json!({
                "kind": "spts",
                "programs": [{
                    "program_number": 1, "pmt_pid": 4096,
                    "streams": [
                        { "source": still, "out_pid": 512, "stream_type": 27 },
                        { "source": { "type": "switch", "initial_input_id": "b", "legs": [
                            { "type": "essence", "input_id": "b", "kind": "audio" },
                            { "type": "essence", "input_id": "c", "kind": "audio" } ] },
                          "out_pid": 513, "stream_type": 15 }
                    ]
                }]
            }))
            .expect("assembly")
        };
        let pid = |id: &str| serde_json::json!({ "type": "pid", "input_id": id, "source_pid": 256 });
        let hitless = assembly(serde_json::json!({
            "type": "hitless", "primary": pid("b"), "backup": pid("b")
        }));
        let foreign = assembly(pid("elsewhere"));
        let map = clocks(&[("b", source_pcr("b")), ("c", source_pcr("c")), ("d", source_pcr("d"))]);
        for (assembly, name, clock) in
            [(&hitless, "b", "source_pcr:input=b"), (&foreign, "elsewhere", "unknown")]
        {
            let err = check_added_input_clock(assembly, &map, "c").expect_err("a Take to c");
            assert_eq!((err.slot_out_pid, err.slot_input_id.as_str()), (513, "c"));
            assert_eq!((err.reference_out_pid, err.reference_input_id.as_str()), (512, name));
            assert_eq!(err.reference_clock, clock);
            assert_eq!(err.take_input_id.as_deref(), Some("c"));
            check_added_input_clock(assembly, &map, "d").expect("an input it does not name");
            let same = clocks(&[("b", source_pcr("b")), ("c", source_pcr("b"))]);
            check_added_input_clock(assembly, &same, "c").expect("one clock");
        }
    }

    #[test]
    fn clock_compat_skips_missing_inputs_defensively() {
        // If the caller didn't populate the map for an input, the
        // check skips it rather than failing — the upstream essence
        // resolver is the authority on "input not found".
        let plan = make_plan();
        let map = clocks(&[("in-a", ClockIdentity::Wallclock)]);
        // in-b is absent from the map; the check should accept the
        // plan rather than spuriously rejecting.
        check_assembly_clock_compatibility(&plan, &map)
            .expect("missing-map-entry must not cause spurious rejection");
    }

    // ── Slot-stall detection (pid_bus_slot_stalled / _recovered) ──

    #[test]
    fn stall_scan_respects_start_grace() {
        let plan = make_plan();
        let active = HashMap::new();
        let mut vitals = HashMap::new();
        let start = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, start);
        // 9 s in: inside the 10 s grace — nothing stalls even though
        // no slot has ever received data.
        let (health, newly) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(9),
            start,
        );
        assert_eq!(health.total_slots, 2);
        assert_eq!(health.stalled_slot_count, 0);
        assert!(health.stalled_slots.is_empty());
        assert!(newly.is_empty());
    }

    #[test]
    fn stall_scan_latches_never_fed_slot_after_grace() {
        // The 'srt-to-udp' incident shape: a slot whose source never
        // delivers a single ES packet must alarm once the grace ends.
        let plan = make_plan();
        let active = HashMap::new();
        let mut vitals = HashMap::new();
        let start = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, start);
        let now = start + Duration::from_secs(11);
        let (health, newly) = scan_slot_stalls(&plan, &active, &mut vitals, now, start);
        assert_eq!(health.total_slots, 2);
        assert_eq!(health.stalled_slot_count, 2);
        assert_eq!(newly.len(), 2, "both never-fed slots latch on the same scan");
        let video = newly.iter().find(|s| s.out_pid == 0x200).unwrap();
        assert_eq!(video.program_number, 1);
        assert_eq!(video.input_id, "in-a");
        assert_eq!(video.source_pid, 0x100);
        assert!(video.seconds_since_data >= 11);
        // Next scan: still stalled but latched — reported in the
        // snapshot, NOT re-announced as newly stalled.
        let (health2, newly2) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            now + Duration::from_secs(1),
            start,
        );
        assert_eq!(health2.stalled_slot_count, 2);
        assert!(newly2.is_empty(), "stall events are latched per slot");
    }

    #[test]
    fn stall_scan_measures_from_last_data_not_start() {
        let plan = make_plan();
        let active = HashMap::new();
        let mut vitals = HashMap::new();
        let start = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, start);
        // Both slots forward packets before the t=12 scan — the moved
        // counter advances last_data to the scan time.
        for v in vitals.values_mut() {
            v.packets += 7;
        }
        let (h, n) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(12),
            start,
        );
        assert_eq!(h.stalled_slot_count, 0);
        assert!(n.is_empty());
        // t=16: only 4 s of silence — under the 5 s threshold.
        let (h, _) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(16),
            start,
        );
        assert_eq!(h.stalled_slot_count, 0);
        // t=17: 5 s of silence — both slots latch with the honest age.
        let (h, n) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(17),
            start,
        );
        assert_eq!(h.stalled_slot_count, 2);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].seconds_since_data, 5);
    }

    #[test]
    fn stall_relatches_after_recovery() {
        let plan = make_plan();
        let active = HashMap::new();
        let mut vitals = HashMap::new();
        let start = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, start);
        let (_, n) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(11),
            start,
        );
        assert_eq!(n.len(), 2);
        // Simulate the forward-path recovery on the video slot exactly
        // as the fanin arm performs it: counter bump + latch clear +
        // last_data refresh.
        {
            let v = vitals.get_mut(&0x200).unwrap();
            v.packets += 1;
            v.stalled = false;
            v.last_data = Some(start + Duration::from_secs(12));
        }
        let (h, n) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(13),
            start,
        );
        assert_eq!(h.stalled_slot_count, 1, "only the audio slot stays stalled");
        assert!(n.is_empty());
        // Silence resumes on the recovered slot: it must re-latch (and
        // re-announce) 5 s after the t=13 scan observed its movement.
        let (h, n) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(18),
            start,
        );
        assert_eq!(h.stalled_slot_count, 2);
        assert_eq!(n.len(), 1, "recovered-then-dead slot re-arms the latch");
        assert_eq!(n[0].out_pid, 0x200);
    }

    #[test]
    fn stalled_slot_reports_active_switch_leg() {
        let mut plan = make_plan();
        plan.programs[0].slots[0].switch_legs = Some(vec![
            ("in-a".to_string(), 0x100),
            ("in-c".to_string(), 0x300),
        ]);
        let mut active = HashMap::new();
        active.insert(0x200u16, "in-c".to_string());
        let mut vitals = HashMap::new();
        let start = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, start);
        let (_, newly) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(11),
            start,
        );
        let s = newly.iter().find(|s| s.out_pid == 0x200).unwrap();
        assert_eq!(s.input_id, "in-c", "switch slots report the ACTIVE leg");
        assert_eq!(s.source_pid, 0x300);
    }

    #[test]
    fn sync_slot_vitals_reconciles_plan_changes() {
        let plan = make_plan();
        let mut vitals = HashMap::new();
        let t0 = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, t0);
        assert_eq!(vitals.len(), 2);
        // State survives a no-op re-sync (an unrelated ReplacePlan must
        // not reset an already-latched stall).
        vitals.get_mut(&0x200).unwrap().stalled = true;
        sync_slot_vitals(&plan, &mut vitals, t0 + Duration::from_secs(30));
        assert!(vitals.get(&0x200).unwrap().stalled);
        // Swap the audio slot: old key dropped, new key registered with
        // a fresh grace anchor at the swap time.
        let mut plan2 = make_plan();
        plan2.programs[0].slots[1] = slot("in-c", 0x400, 0x401, 0x0F);
        let t_swap = t0 + Duration::from_secs(60);
        sync_slot_vitals(&plan2, &mut vitals, t_swap);
        assert_eq!(vitals.len(), 2);
        assert!(!vitals.contains_key(&0x201));
        let fresh = vitals.get(&0x401).unwrap();
        assert_eq!(fresh.registered_at, t_swap);
        assert!(!fresh.stalled);
    }

    #[test]
    fn stall_scan_exempts_sparse_data_slots() {
        // SCTE-35 / DVB-subtitle / teletext slots are legitimately
        // silent between bursts — they must never latch a stall, while
        // continuous-media siblings in the same program still do.
        let mut plan = make_plan();
        plan.programs[0]
            .slots
            .push(slot("in-d", 0x500, 0x501, 0x86)); // SCTE-35
        plan.programs[0]
            .slots
            .push(slot("in-d", 0x510, 0x511, 0x06)); // private PES (subs/teletext)
        let active = HashMap::new();
        let mut vitals = HashMap::new();
        let start = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, start);
        let (health, newly) = scan_slot_stalls(
            &plan,
            &active,
            &mut vitals,
            start + Duration::from_secs(60),
            start,
        );
        assert_eq!(health.total_slots, 4, "exempt slots still count in total");
        assert_eq!(
            health.stalled_slot_count, 2,
            "only the video + audio slots may stall"
        );
        assert!(newly.iter().all(|s| s.out_pid == 0x200 || s.out_pid == 0x201));
    }

    #[test]
    fn stall_state_survives_program_reorder() {
        // ReplacePlan that prepends a program, or renumbers one, must not
        // reset surviving slots' stall history (vitals are keyed by out_pid).
        let plan = make_plan();
        let mut vitals = HashMap::new();
        let t0 = Instant::now();
        sync_slot_vitals(&plan, &mut vitals, t0);
        vitals.get_mut(&0x200).unwrap().stalled = true;
        let mut plan2 = AssemblyPlan {
            programs: vec![ProgramPlan {
                program_number: 2,
                pmt_pid: 0x1010,
                pcr_source: ("in-e".to_string(), 0x600),
                slots: vec![slot("in-e", 0x600, 0x601, 0x1B)],
            }],
            clocks: HashMap::new(),
        };
        plan2.programs.extend(plan.programs.clone());
        plan2.programs[1].program_number = 7;
        sync_slot_vitals(&plan2, &mut vitals, t0 + Duration::from_secs(30));
        assert!(
            vitals.get(&0x200).unwrap().stalled,
            "latched stall survives the reorder"
        );
        assert!(vitals.contains_key(&0x601), "new program registered");
    }

    #[tokio::test]
    async fn assembler_publishes_health_snapshot() {
        let bus = Arc::new(NodeEsBus::new());
        let (tx, _rx) = broadcast::channel::<RtpPacket>(16);
        let cancel = CancellationToken::new();
        let cell: Arc<AssemblyHealthCell> = Arc::new(std::sync::RwLock::new(None));
        let handle = spawn_spts_assembler(
            make_plan(),
            bus.clone(),
            tx.clone(),
            cancel.clone(),
            None,
            String::new(),
            None,
            Some(cell.clone()),
            None,
            Default::default(),
        );
        // The stall-scan interval's first tick fires shortly after
        // start; poll the cell instead of sleeping a fixed amount.
        let mut published = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(h) = cell.read().ok().and_then(|g| g.clone()) {
                published = Some(h);
                break;
            }
        }
        cancel.cancel();
        handle.join.await.unwrap();
        let h = published.expect("assembler must publish AssemblyHealth within ~2 s of start");
        assert_eq!(h.total_slots, 2);
        assert_eq!(
            h.stalled_slot_count, 0,
            "inside the start grace nothing may be reported stalled"
        );
    }

    /// A Switch slot whose legs are `srt-main` (active) and `srt-backup`, on
    /// one clock, next to a Hitless slot whose merger reads `srt-pri` and
    /// `srt-bak`.
    fn switch_and_hitless_plan(
        splice_mode: crate::config::models::SpliceMode,
        splice_budget_ms: Option<u32>,
    ) -> AssemblyPlan {
        let mut switch = slot("srt-main", 0x101, 0x201, 0x03);
        switch.switch_legs = Some(vec![
            ("srt-main".to_string(), 0x101),
            ("srt-backup".to_string(), 0x101),
        ]);
        switch.splice_mode = splice_mode;
        switch.splice_budget_ms = splice_budget_ms;
        let (merged, merged_pid) = crate::engine::ts_es_hitless::hitless_bus_key("f1", "slot_0_1");
        let mut hitless = slot(&merged, merged_pid, 0x202, 0x1B);
        hitless.hitless_legs = Some(HitlessLegs {
            primary: "srt-pri".to_string(),
            backup: "srt-bak".to_string(),
            forwarding_primary: Default::default(),
            seq_fallback: Default::default(),
        });
        AssemblyPlan {
            programs: vec![ProgramPlan {
                program_number: 1,
                pmt_pid: 0x1000,
                pcr_source: ("srt-main".to_string(), 0x101),
                slots: vec![switch, hitless],
            }],
            clocks: clocks(&[
                ("srt-main", ClockIdentity::Wallclock),
                ("srt-backup", ClockIdentity::Wallclock),
            ]),
        }
    }

    async fn until(what: &str, done: impl Fn() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// One TS packet opening an audio PES stamped `pts`.
    fn audio_pes(src_pid: u16, pts: u64) -> EsPacket {
        let mut pkt = ts_es(src_pid, true, 0, 0xFF).to_vec();
        pkt[4..18].copy_from_slice(&[
            0x00,
            0x00,
            0x01,
            0xC0,
            0x00,
            0x00,
            0x80,
            0x80, // PTS only
            0x05,
            0x21 | ((((pts >> 30) & 0x07) as u8) << 1),
            (pts >> 22) as u8,
            0x01 | ((((pts >> 15) & 0x7F) as u8) << 1),
            (pts >> 7) as u8,
            0x01 | (((pts & 0x7F) as u8) << 1),
        ]);
        EsPacket {
            source_pid: src_pid,
            stream_type: 0x03,
            payload: Bytes::from(pkt),
            is_pusi: true,
            has_pcr: false,
            pcr: None,
            recv_time_us: 0,
            upstream_seq: None,
        }
    }

    fn hitless_legs(plan: &AssemblyPlan) -> &HitlessLegs {
        plan.programs[0].slots[1].hitless_legs.as_ref().expect("the Hitless slot")
    }

    #[test]
    fn on_air_is_each_switch_slots_active_leg_and_the_hitless_leg_forwarded() {
        let plan = switch_and_hitless_plan(Default::default(), None);
        let published = |legs: &HashMap<u16, String>| {
            let on_air = OnAirInputs::default();
            on_air.set_assembled(on_air_inputs(&plan, legs));
            on_air
        };
        let playing = |on_air: &OnAirInputs| -> Vec<&str> {
            ["srt-main", "srt-backup", "srt-pri", "srt-bak"]
                .into_iter()
                .filter(|id| on_air.contains(id))
                .collect()
        };
        // Before any Take the Switch slot plays the leg the plan names, and
        // the Hitless merger forwards its primary.
        let on_air = published(&HashMap::new());
        assert_eq!(playing(&on_air), ["srt-main", "srt-pri"]);
        // A failover puts the backup on air in the primary's place, with no
        // publication in between.
        hitless_legs(&plan).forwarding_primary.set(false);
        assert_eq!(playing(&on_air), ["srt-main", "srt-bak"]);
        hitless_legs(&plan).forwarding_primary.set(true);
        let taken = HashMap::from([(0x201, "srt-backup".to_string())]);
        assert_eq!(playing(&published(&taken)), ["srt-backup", "srt-pri"]);
    }

    /// Which leg a Hitless merger forwards is not part of the plan: a swap
    /// that keeps a slot's legs is no change to its program.
    #[test]
    fn a_hitless_failover_is_not_a_plan_change() {
        let plan = switch_and_hitless_plan(Default::default(), None);
        hitless_legs(&plan).forwarding_primary.set(false);
        let fresh = switch_and_hitless_plan(Default::default(), None);
        assert_eq!(plan.programs[0].slots, fresh.programs[0].slots);
    }

    /// A Take on a Switch slot moves which leg's SRT drops are the flow's,
    /// through a real assembler and a real flow accumulator; of a Hitless
    /// slot's legs only the primary its merger forwards counts.
    #[tokio::test]
    async fn a_switch_slot_take_moves_srt_drop_attribution() {
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f1".into(),
            "flow-1".into(),
            "srt".into(),
        );
        let srt: HashMap<&str, _> = ["srt-main", "srt-backup", "srt-pri", "srt-bak"]
            .into_iter()
            .map(|id| (id, flow.register_srt_input_stats(id)))
            .collect();
        flow.set_active_input_id("srt-main");
        let bus = Arc::new(NodeEsBus::new());
        let (tx, _rx) = broadcast::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        // A failed assertion must stop the assembler thread too, or the
        // runtime waits on its join for ever.
        let _stop_on_failure = cancel.clone().drop_guard();
        let handle = spawn_spts_assembler(
            switch_and_hitless_plan(Default::default(), None),
            bus,
            tx,
            cancel.clone(),
            None,
            String::new(),
            None,
            None,
            Some(flow.on_air.clone()),
            Default::default(),
        );
        until("the assembler's first publication", || flow.on_air.contains("srt-pri")).await;

        srt["srt-backup"].count_recv_drops(100);
        srt["srt-pri"].count_recv_drops(5);
        srt["srt-bak"].count_recv_drops(7);
        assert_eq!(flow.snapshot().input.packets_lost, 5, "the standby legs' drops are their own");

        handle
            .plan_tx
            .send(PlanCommand::SwitchActiveInput {
                new_input_id: "srt-backup".to_string(),
                splice_mode_override: None,
            })
            .await
            .unwrap();
        until("the Take to reach the on-air set", || flow.on_air.contains("srt-backup")).await;
        assert!(!flow.on_air.contains("srt-main"));
        srt["srt-backup"].count_recv_drops(300);
        srt["srt-main"].count_recv_drops(50);
        assert_eq!(flow.snapshot().input.packets_lost, 305);

        cancel.cancel();
        handle.join.await.unwrap();
    }

    /// A Hitless merger's failover moves which leg's SRT drops are the
    /// flow's, and its return to the primary moves them back, through the
    /// real merger and assembler sharing the plan's flag as `FlowRuntime`
    /// wires them.
    #[tokio::test]
    async fn a_hitless_failover_moves_srt_drop_attribution() {
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f1".into(),
            "flow-1".into(),
            "srt".into(),
        );
        let pri = flow.register_srt_input_stats("srt-pri");
        let bak = flow.register_srt_input_stats("srt-bak");
        let plan = switch_and_hitless_plan(Default::default(), None);
        let legs = hitless_legs(&plan).clone();
        let bus = Arc::new(NodeEsBus::new());
        let (tx, _rx) = broadcast::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let _stop_on_failure = cancel.clone().drop_guard();
        let _merger = crate::engine::ts_es_hitless::spawn_hitless_es_merger_full(
            crate::engine::ts_es_hitless::hitless_bus_key("f1", "slot_0_1"),
            ("srt-pri".to_string(), 0x102),
            ("srt-bak".to_string(), 0x102),
            bus.clone(),
            Duration::from_millis(50),
            false,
            crate::redundancy::merger::REORDER_WINDOW,
            None,
            /* leg_lag */ crate::engine::ts_es_hitless::LegLag::fixed(Duration::ZERO),
            legs.forwarding_primary.clone(),
            legs.seq_fallback.clone(),
            cancel.child_token(), None
        );
        let handle = spawn_spts_assembler(
            plan,
            bus.clone(),
            tx,
            cancel.clone(),
            None,
            String::new(),
            None,
            None,
            Some(flow.on_air.clone()),
            Default::default(),
        );
        let primary = bus.sender_for("srt-pri", 0x102, 0x1B);
        let es = || EsPacket {
            source_pid: 0x102,
            stream_type: 0x1B,
            payload: ts_es(0x102, false, 0, 0xAA),
            is_pusi: false,
            has_pcr: false,
            pcr: None,
            recv_time_us: 0,
            upstream_seq: None,
        };
        // The merger's stall timer runs from its spawn, so it may have
        // failed over already.
        until("the assembler's first publication", || {
            flow.on_air.contains("srt-pri") || flow.on_air.contains("srt-bak")
        })
        .await;
        primary.send(es()).unwrap();

        // The primary stalls: the merger fails over to the backup.
        until("the failover", || flow.on_air.contains("srt-bak")).await;
        assert!(!flow.on_air.contains("srt-pri"));
        pri.count_recv_drops(4);
        bak.count_recv_drops(9);
        assert_eq!(flow.snapshot().input.packets_lost, 9);

        // The primary resumes. Nothing below yields, so the merger cannot
        // fail over again before the drops are counted.
        primary.send(es()).unwrap();
        until("the return to the primary", || flow.on_air.contains("srt-pri")).await;
        assert!(!flow.on_air.contains("srt-bak"));
        pri.count_recv_drops(6);
        bak.count_recv_drops(20);
        assert_eq!(flow.snapshot().input.packets_lost, 15);

        cancel.cancel();
        handle.join.await.unwrap();
    }

    /// A seq-aware merger that falls back to primary preference — its
    /// primary brings a packet with no upstream sequence number — shows on
    /// the flow's stats for as long as it runs: the slot in
    /// `assembly_health.unsequenced_hitless_slots` and a Warning health
    /// reason, through the real merger, assembler and flow accumulator
    /// sharing the plan's flag as `FlowRuntime` wires them. A one-off event
    /// was all it raised, beside a Hitless slot that looked healthy.
    #[tokio::test]
    async fn a_seq_aware_fallback_holds_a_health_reason() {
        let flow = crate::stats::collector::FlowStatsAccumulator::new(
            "f1".into(),
            "flow-1".into(),
            "srt".into(),
        );
        let plan = switch_and_hitless_plan(Default::default(), None);
        let legs = hitless_legs(&plan).clone();
        let bus = Arc::new(NodeEsBus::new());
        let (tx, _rx) = broadcast::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let _stop_on_failure = cancel.clone().drop_guard();
        let _merger = crate::engine::ts_es_hitless::spawn_hitless_es_merger_full(
            crate::engine::ts_es_hitless::hitless_bus_key("f1", "slot_0_1"),
            ("srt-pri".to_string(), 0x102),
            ("srt-bak".to_string(), 0x102),
            bus.clone(),
            Duration::from_millis(200),
            /* seq_aware */ true,
            crate::redundancy::merger::REORDER_WINDOW,
            None,
            /* leg_lag */ crate::engine::ts_es_hitless::LegLag::fixed(Duration::ZERO),
            legs.forwarding_primary.clone(),
            legs.seq_fallback.clone(),
            cancel.child_token(),
            None,
        );
        let handle = spawn_spts_assembler(
            plan,
            bus.clone(),
            tx,
            cancel.clone(),
            None,
            String::new(),
            None,
            Some(flow.assembly_health.clone()),
            Some(flow.on_air.clone()),
            Default::default(),
        );
        let reason = || {
            flow.snapshot()
                .health_reasons
                .into_iter()
                .find(|r| r.code == "hitless_seq_aware_leg_unsequenced")
        };
        until("the assembler's first publication", || flow.on_air.contains("srt-pri")).await;
        assert!(reason().is_none(), "merging");

        bus.sender_for("srt-pri", 0x102, 0x1B)
            .send(EsPacket {
                source_pid: 0x102,
                stream_type: 0x1B,
                payload: ts_es(0x102, false, 0, 0xAA),
                is_pusi: false,
                has_pcr: false,
                pcr: None,
                recv_time_us: 0,
                upstream_seq: None,
            })
            .unwrap();
        // The scan runs once a second.
        for _ in 0..300 {
            if reason().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let reason = reason().expect("the fallback raises a health reason");
        assert_eq!(reason.severity, crate::stats::models::FlowHealth::Warning);
        assert!(reason.detail.contains("primary leg 'srt-pri'"), "{}", reason.detail);
        let health = flow.snapshot().assembly_health.expect("an assembled flow");
        assert_eq!(health.unsequenced_hitless_slots.len(), 1);
        let slot = &health.unsequenced_hitless_slots[0];
        assert_eq!(
            (slot.program_number, slot.out_pid, slot.leg.as_str(), slot.input_id.as_str()),
            (1, 0x202, "primary", "srt-pri")
        );

        cancel.cancel();
        handle.join.await.unwrap();
    }

    /// A PES-aligned Take keeps the old leg on air until the splice
    /// commits, then moves it.
    #[tokio::test]
    async fn a_pes_aligned_take_moves_on_air_when_the_splice_commits() {
        let on_air = Arc::new(OnAirInputs::default());
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(256);
        let cancel = CancellationToken::new();
        let _stop_on_failure = cancel.clone().drop_guard();
        // A budget long enough that only the commit can move the leg.
        let handle = spawn_spts_assembler(
            switch_and_hitless_plan(crate::config::models::SpliceMode::PesAligned, Some(60_000)),
            bus.clone(),
            tx,
            cancel.clone(),
            None,
            String::new(),
            None,
            None,
            Some(on_air.clone()),
            Default::default(),
        );
        until("the assembler's first publication", || on_air.contains("srt-main")).await;

        // The active leg's PES gives the splice its alignment threshold.
        bus.sender_for("srt-main", 0x101, 0x03).send(audio_pes(0x101, 90_000)).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut forwarded = false;
        while !forwarded && tokio::time::Instant::now() < deadline {
            if let Ok(Ok(bundle)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                forwarded = bundle.data.chunks_exact(TS_PACKET_SIZE).any(|p| ts_pid(p) == 0x201);
            }
        }
        assert!(forwarded, "the active leg's PES reaches the output");

        handle
            .plan_tx
            .send(PlanCommand::SwitchActiveInput {
                new_input_id: "srt-backup".to_string(),
                splice_mode_override: None,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            on_air.contains("srt-main") && !on_air.contains("srt-backup"),
            "armed, not yet cut"
        );

        bus.sender_for("srt-backup", 0x101, 0x03).send(audio_pes(0x101, 180_000)).unwrap();
        until("the commit to reach the on-air set", || on_air.contains("srt-backup")).await;
        assert!(!on_air.contains("srt-main"));

        cancel.cancel();
        handle.join.await.unwrap();
    }

    /// A running assembler, its inputs and what it sends.
    struct Rig {
        bus: Arc<NodeEsBus>,
        on_air: Arc<OnAirInputs>,
        out: broadcast::Receiver<RtpPacket>,
        events: tokio::sync::mpsc::Receiver<crate::manager::events::Event>,
        handle: AssemblerHandle,
        _stop: tokio_util::sync::DropGuard,
    }

    impl Rig {
        async fn start(plan: AssemblyPlan) -> Self {
            Self::start_with(plan, Default::default()).await
        }

        async fn start_with(
            plan: AssemblyPlan,
            catalogs: HashMap<String, Arc<crate::engine::ts_psi_catalog::PsiCatalogStore>>,
        ) -> Self {
            let on_air = Arc::new(OnAirInputs::default());
            let bus = Arc::new(NodeEsBus::new());
            let (tx, out) = broadcast::channel::<RtpPacket>(1024);
            let cancel = CancellationToken::new();
            let (events_tx, events) = crate::manager::events::event_channel();
            let first = plan.programs[0].slots[0].source.0.clone();
            let handle = spawn_spts_assembler(
                plan,
                bus.clone(),
                tx,
                cancel.clone(),
                Some(events_tx),
                "f1".to_string(),
                None,
                None,
                Some(on_air.clone()),
                catalogs,
            );
            until("the assembler's first publication", || on_air.contains(&first)).await;
            Rig { bus, on_air, out, events, handle, _stop: cancel.drop_guard() }
        }

        fn send(&self, input: &str, packet: EsPacket) {
            self.bus.sender_for(input, packet.source_pid, packet.stream_type).send(packet).unwrap();
        }

        async fn take(&self, input: &str, mode: Option<crate::config::models::SpliceMode>) {
            let take = PlanCommand::SwitchActiveInput {
                new_input_id: input.to_string(),
                splice_mode_override: mode,
            };
            self.handle.plan_tx.send(take).await.unwrap();
        }

        async fn replace(&self, plan: AssemblyPlan) {
            self.replaced(plan).await.await.unwrap();
        }

        /// Ask for the swap: what it answers says when it is on air.
        async fn replaced(&self, plan: AssemblyPlan) -> tokio::sync::oneshot::Receiver<()> {
            let (applied, done) = tokio::sync::oneshot::channel();
            let replace = PlanCommand::ReplacePlan {
                plan,
                splice_mode_override: None,
                catalogs: Default::default(),
                applied: Some(applied),
            };
            self.handle.plan_tx.send(replace).await.unwrap();
            done
        }

        /// Hold the assembler, its fan-ins running, until this is dropped.
        async fn stall(&self) -> tokio::sync::oneshot::Sender<()> {
            let (release, stalled) = tokio::sync::oneshot::channel();
            self.handle.plan_tx.send(PlanCommand::Stall { release: stalled }).await.unwrap();
            release
        }

        /// Everything that goes out in `wait`, in order.
        async fn wire(&mut self, wait: Duration) -> Vec<(u16, Vec<u8>)> {
            let end = tokio::time::Instant::now() + wait;
            let mut out = Vec::new();
            while let Ok(Ok(bundle)) = tokio::time::timeout_at(end, self.out.recv()).await {
                out.extend(bundle.data.chunks_exact(TS_PACKET_SIZE).map(|p| (ts_pid(p), p.to_vec())));
            }
            out
        }

        /// How many packets of `input`'s `pid` its fan-ins have yet to take.
        fn untaken(&self, input: &str, pid: u16) -> usize {
            self.bus.sender_for(input, pid, 0).len()
        }

        /// The version of the next PMT on each of `pmt_pids`.
        async fn versions(&mut self, pmt_pids: &[u16]) -> Vec<u8> {
            while self.out.try_recv().is_ok() {}
            let mut found: HashMap<u16, u8> = HashMap::new();
            let end = tokio::time::Instant::now() + Duration::from_secs(5);
            while !pmt_pids.iter().all(|pid| found.contains_key(pid)) {
                let bundle = tokio::time::timeout_at(end, self.out.recv()).await.expect("the PMTs");
                for p in bundle.unwrap().data.chunks_exact(TS_PACKET_SIZE) {
                    if pmt_pids.contains(&ts_pid(p)) {
                        found.insert(ts_pid(p), pmt_version(p));
                    }
                }
            }
            pmt_pids.iter().map(|pid| found[pid]).collect()
        }

        /// Whether the program plays from exactly `inputs` of `a`, `b`, `c`.
        fn plays(&self, inputs: &[&str]) -> bool {
            ["a", "b", "c"].iter().all(|id| self.on_air.contains(id) == inputs.contains(id))
        }

        /// The ES packets that go out, in order: `expected` of them, however
        /// long a busy host takes, and what follows within 100 ms.
        async fn sent(&mut self, expected: usize) -> Vec<(u16, Vec<u8>)> {
            let mut es = Vec::new();
            for wait in [Duration::from_secs(5), Duration::from_millis(100)] {
                let end = tokio::time::Instant::now() + wait;
                while (wait.as_secs() == 0 || es.len() < expected)
                    && let Ok(Ok(bundle)) = tokio::time::timeout_at(end, self.out.recv()).await
                {
                    for p in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                        if (0x200..0x400).contains(&ts_pid(p)) {
                            es.push((ts_pid(p), p.to_vec()));
                        }
                    }
                }
            }
            es
        }

        /// The PIDs of [`Self::sent`], sorted: slots are read by tasks of
        /// their own.
        async fn sent_pids(&mut self, expected: usize) -> Vec<u16> {
            let mut pids: Vec<u16> =
                self.sent(expected).await.into_iter().map(|(pid, _)| pid).collect();
            pids.sort_unstable();
            pids
        }

        fn raised(&mut self, code: &str) -> Vec<serde_json::Value> {
            std::iter::from_fn(|| self.events.try_recv().ok())
                .filter_map(|e| e.details)
                .filter(|d| d["error_code"] == code)
                .collect()
        }
    }

    /// A packet of `input`'s ES on `src_pid`, opening a PES stamped `pts`.
    fn es_pes(src_pid: u16, stream_type: u8, pts: u64) -> EsPacket {
        EsPacket { stream_type, ..audio_pes(src_pid, pts) }
    }

    /// An adaptation-field-only packet with a PCR.
    fn es_pcr(src_pid: u16, stream_type: u8) -> EsPacket {
        EsPacket {
            stream_type,
            payload: ts_af_only(src_pid),
            is_pusi: false,
            has_pcr: true,
            pcr: Some(0),
            ..audio_pes(src_pid, 0)
        }
    }

    /// Video and audio of one program, each with a leg on `a` and on `b`.
    fn switched_program() -> AssemblyPlan {
        one_program(vec![
            switch_slot(&["a", "b"], 0x100, 0x200, 0x1B),
            switch_slot(&["a", "b"], 0x101, 0x201, 0x0F),
        ])
    }

    /// The same, both legs on one clock.
    fn one_clock(mut plan: AssemblyPlan) -> AssemblyPlan {
        plan.clocks.insert("b".into(), source_pcr("a"));
        plan
    }

    fn pes_aligned(mut plan: AssemblyPlan, budget_ms: u32) -> AssemblyPlan {
        for slot in plan.programs.iter_mut().flat_map(|p| &mut p.slots) {
            slot.splice_mode = crate::config::models::SpliceMode::PesAligned;
            slot.splice_budget_ms = Some(budget_ms);
        }
        plan
    }

    /// A PES of each slot's leg on `input`: whether they reach the output, and
    /// what a splice from it is aligned to.
    async fn feed(rig: &mut Rig, input: &str, pts: u64, expected: usize) -> Vec<u16> {
        rig.send(input, es_pes(0x100, 0x1B, pts));
        rig.send(input, es_pes(0x101, 0x0F, pts));
        rig.sent_pids(expected).await
    }

    /// The PMT's PCR_PID is the slot that reads the PCR source: the slot pinned
    /// to it, or the Switch slot with a leg on it, whichever it starts on, at
    /// start and after a swap.
    #[tokio::test]
    async fn the_pcr_pid_is_the_slot_that_reads_the_pcr_source() {
        let pcr_pid = async |rig: &mut Rig| {
            let end = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let bundle =
                    tokio::time::timeout_at(end, rig.out.recv()).await.expect("a PMT").unwrap();
                for p in bundle.data.chunks_exact(TS_PACKET_SIZE) {
                    if ts_pid(p) == 0x1000 {
                        return u16::from_be_bytes([p[13], p[14]]) & 0x1FFF;
                    }
                }
            }
        };
        let mut pinned =
            one_program(vec![slot("a", 0x101, 0x201, 0x0F), slot("a", 0x100, 0x200, 0x1B)]);
        pinned.programs[0].pcr_source = ("a".to_string(), 0x100);
        let mut switched = one_program(vec![
            switch_slot(&["a", "b"], 0x101, 0x201, 0x0F),
            switch_slot(&["a", "b"], 0x100, 0x200, 0x1B),
        ]);
        switched.programs[0].pcr_source = ("b".to_string(), 0x100);
        for (plan, next) in [(&pinned, &switched), (&switched, &pinned)] {
            let mut rig = Rig::start(plan.clone()).await;
            assert_eq!(pcr_pid(&mut rig).await, 0x200);
            rig.replace(next.clone()).await;
            while rig.out.try_recv().is_ok() {}
            assert_eq!(pcr_pid(&mut rig).await, 0x200, "after the swap");
        }
    }

    /// A Take to another clock: nothing of the program goes out until a PCR
    /// of the new leg has, on the slot that carries the program's PCR. A
    /// PES ahead of it went out on the old leg's clock, hours off for two
    /// encoders. The other program does not notice, and neither does a
    /// Take between inputs on one clock.
    #[tokio::test]
    async fn a_take_to_another_clock_waits_for_its_pcr() {
        let mut rig = Rig::start(two_programs()).await;
        assert_eq!(feed(&mut rig, "a", 90_000, 2).await, [0x200, 0x201]);

        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b", "c"])).await;
        // PES without PCR, and a PCR on the slot that does not carry it.
        assert_eq!(feed(&mut rig, "b", 90_000, 0).await, [0u16; 0], "held for b's PCR");
        rig.send("b", es_pcr(0x101, 0x0F));
        rig.send("c", es_pes(0x100, 0x1B, 90_000));
        assert_eq!(rig.sent_pids(1).await, [0x300], "only the other program");

        rig.send("b", es_pcr(0x100, 0x1B));
        let sent = rig.sent(1).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, 0x200);
        assert!(extract_pcr(&sent[0].1).is_some() && sent[0].1[5] & 0x80 != 0, "PCR with DI");
        assert_eq!(feed(&mut rig, "b", 93_600, 2).await, [0x200, 0x201], "and it plays on");

        // Back to a and to b again before a sent a PCR: b's releases it.
        rig.take("a", None).await;
        until("the cut back", || rig.plays(&["a", "c"])).await;
        assert_eq!(feed(&mut rig, "a", 93_600, 0).await, [0u16; 0], "held for a's PCR");
        rig.take("b", None).await;
        until("the second cut", || rig.plays(&["b", "c"])).await;
        assert_eq!(feed(&mut rig, "b", 97_200, 0).await, [0u16; 0]);
        rig.send("b", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);

        let mut rig = Rig::start(one_clock(switched_program())).await;
        feed(&mut rig, "a", 90_000, 2).await;
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b"])).await;
        assert_eq!(feed(&mut rig, "b", 90_000, 2).await, [0x200, 0x201], "one clock, nothing to wait for");
    }

    /// A Take that leaves the slot carrying the program's PCR alone, which
    /// only happens beside an input whose clock is not known, holds nothing:
    /// no PCR of the new leg is to come.
    #[tokio::test]
    async fn a_take_that_leaves_the_pcr_alone_holds_nothing() {
        let mut plan = one_program(vec![
            slot("a", 0x100, 0x200, 0x1B),
            switch_slot(&["c", "b"], 0x101, 0x201, 0x0F),
        ]);
        plan.clocks.clear();
        let mut rig = Rig::start(plan).await;
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["a", "b"])).await;
        rig.send("a", es_pes(0x100, 0x1B, 90_000));
        rig.send("b", es_pes(0x101, 0x0F, 90_000));
        assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
    }

    /// A program held past the alarm says which leg sends no PCR, once, and
    /// plays again with the first one.
    #[tokio::test]
    async fn a_take_to_a_leg_without_pcr_is_reported() {
        let mut rig = Rig::start(switched_program()).await;
        let code = "pid_bus_take_no_pcr";
        rig.take("b", None).await;
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(rig.raised(code).is_empty(), "not in the first 2 s");
        let mut details = Vec::new();
        for _ in 0..100 {
            details = rig.raised(code);
            if !details.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            details,
            [serde_json::json!({
                "error_code": code,
                "program_number": 1,
                "out_pid": 0x200,
                "input_id": "b",
                "source_pid": 0x100,
            })]
        );
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(rig.raised(code).is_empty(), "once");
        rig.send("b", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
        tokio::time::sleep(Duration::from_millis(3_000)).await;
        assert!(rig.raised(code).is_empty(), "and it plays");
    }

    /// `pes_aligned` compares the two legs' timestamps, which means nothing
    /// between two clocks: the audio went over at once or after its budget,
    /// the video a GOP or 2 s later, and the audio in between carried
    /// timestamps off by the difference of the clocks. Such a Take is a cut,
    /// of every slot at once, and says so, whichever asked for the splice.
    #[tokio::test]
    async fn a_splice_between_two_clocks_is_a_cut() {
        use crate::config::models::SpliceMode;
        let mut unknown = pes_aligned(switched_program(), 60_000);
        unknown.clocks.clear();
        for (plan, mode, forced) in [
            (pes_aligned(switched_program(), 60_000), None, false),
            (switched_program(), Some(SpliceMode::PesAligned), true),
            (unknown, None, false),
        ] {
            let mut rig = Rig::start(plan).await;
            assert_eq!(feed(&mut rig, "a", 90_000, 2).await, [0x200, 0x201]);
            rig.take("b", mode).await;
            until("the cut", || rig.plays(&["b"])).await;
            let degraded: Vec<serde_json::Value> = rig.raised("pes_splice_degraded");
            let said: Vec<_> = degraded
                .iter()
                .map(|d| (d["out_pid"].clone(), d["reason"].clone(), d["forced"].clone()))
                .collect();
            let reason = serde_json::json!("independent_clocks");
            assert_eq!(
                said,
                [
                    (0x200.into(), reason.clone(), forced.into()),
                    (0x201.into(), reason.clone(), forced.into()),
                ]
            );
        }
        // On one clock the reasons are the ones they were, and a slot that can
        // be spliced waits for its alignment.
        let mut plan = pes_aligned(one_clock(switched_program()), 60_000);
        plan.programs[0].slots[0].stream_type = 0x02;
        let mut rig = Rig::start(plan).await;
        rig.take("b", None).await;
        until("the cut of both", || rig.plays(&["b"])).await;
        let reasons: Vec<_> =
            rig.raised("pes_splice_degraded").iter().map(|d| d["reason"].clone()).collect();
        assert_eq!(reasons, ["unsupported_codec", "no_aligned_pes_reference"]);

        let mut rig = Rig::start(pes_aligned(one_clock(switched_program()), 60_000)).await;
        feed(&mut rig, "a", 90_000, 2).await;
        rig.take("b", None).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rig.plays(&["a"]), "armed, not yet cut");
        assert!(rig.raised("pes_splice_degraded").is_empty());
    }

    /// The last Take decides: one that takes a slot elsewhere, or back to the
    /// leg it is still on, ends a splice that has not landed. Left pending it
    /// went on to move the slot at its commit or timeout, alone.
    #[tokio::test]
    async fn a_take_ends_a_pending_splice() {
        // a and b on one clock, c on another, so a Take to c is a cut.
        let legs = ["a", "b", "c"];
        for back_to in ["a", "c"] {
            let plan = one_program(vec![
                switch_slot(&legs, 0x100, 0x200, 0x1B),
                switch_slot(&legs, 0x101, 0x201, 0x0F),
            ]);
            let mut rig = Rig::start(pes_aligned(one_clock(plan), 1_500)).await;
            assert_eq!(feed(&mut rig, "a", 90_000, 2).await, [0x200, 0x201]);
            rig.take("b", None).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(rig.plays(&["a"]), "both slots wait for b");
            rig.take(back_to, None).await;
            until("the second Take", || rig.plays(&[back_to])).await;
            tokio::time::sleep(Duration::from_millis(1_800)).await;
            assert!(rig.plays(&[back_to]), "past the splice's budget, still on {back_to}");
            assert!(rig.raised("pes_splice_timeout").is_empty());
            rig.send(back_to, es_pcr(0x100, 0x1B));
            assert_eq!(rig.sent_pids(1).await, [0x200]);
            assert_eq!(feed(&mut rig, back_to, 97_200, 2).await, [0x200, 0x201], "{back_to} plays");
        }
    }

    /// [`switched_program`] beside a program 2 pinned to `c`.
    fn two_programs() -> AssemblyPlan {
        let mut plan = switched_program();
        plan.programs.push(ProgramPlan {
            program_number: 2,
            pmt_pid: 0x1001,
            pcr_source: ("c".to_string(), 0x100),
            slots: vec![slot("c", 0x100, 0x300, 0x1B)],
        });
        plan
    }

    /// A plan swap leaves a splice it takes no leg of to land as it would
    /// have: the audio on its aligned PES, the video here at its budget. Cut
    /// short it went over in mid-GOP, whatever the swap was for. A slot the
    /// swap adds is on the leg the Take named from the start.
    #[tokio::test]
    async fn a_plan_swap_leaves_a_splice_pending() {
        let plan = pes_aligned(one_clock(two_programs()), 3_000);
        let mut rig = Rig::start(plan.clone()).await;
        feed(&mut rig, "a", 90_000, 2).await;
        rig.take("b", None).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut changed = plan;
        changed.programs[0].slots.push(switch_slot(&["a", "b"], 0x102, 0x202, 0x0F));
        changed.programs[1].slots.push(slot("c", 0x101, 0x301, 0x0F));
        rig.replace(changed).await;
        assert!(rig.plays(&["a", "b", "c"]), "two slots on a, the new one on b");
        rig.send("b", es_pes(0x102, 0x0F, 90_000));
        assert_eq!(rig.sent_pids(1).await, [0x202]);

        // Both legs' packets still go through the splice: a's next PES ends
        // what a sends, b's is the first of b.
        assert_eq!(feed(&mut rig, "a", 93_600, 0).await, [0u16; 0]);
        assert_eq!(feed(&mut rig, "b", 97_200, 1).await, [0x201]);
        let completed: Vec<_> = rig
            .raised("pes_splice_completed")
            .iter()
            .map(|d| (d["out_pid"].clone(), d["first_b_pts"].clone()))
            .collect();
        assert_eq!(completed, [(0x201.into(), 97_200.into())]);
        assert!(rig.plays(&["a", "b", "c"]), "the video waits for an IDR");
        let before = rig.versions(&[0x1000]).await[0];
        tokio::time::sleep(Duration::from_millis(1_800)).await;
        until("the video's budget", || rig.plays(&["b", "c"])).await;
        assert_eq!(rig.versions(&[0x1000]).await, [(before + 1) & 0x1F]);
        let timeouts: Vec<_> =
            rig.raised("pes_splice_timeout").iter().map(|d| d["out_pid"].clone()).collect();
        assert_eq!(timeouts, [serde_json::json!(0x200)]);
    }

    /// A plan swap that takes a leg of a pending splice, or its slot, ends it:
    /// the slot keeps the leg it is on, or goes to the one the Take named if
    /// only that is left. Left pending it moved the slot later, to a leg the
    /// new plan may not have, and a program it was for being gone ended the
    /// assembler.
    #[tokio::test]
    async fn a_plan_swap_ends_a_splice_it_takes_a_leg_of() {
        let switched = |legs: &[&str]| {
            one_clock(one_program(vec![
                switch_slot(legs, 0x100, 0x200, 0x1B),
                switch_slot(legs, 0x101, 0x201, 0x0F),
            ]))
        };
        let pinned = |id: &str| {
            one_program(vec![slot(id, 0x100, 0x200, 0x1B), slot(id, 0x101, 0x201, 0x0F)])
        };
        for (next, on) in [
            (switched(&["c", "a"]), "a"),
            (switched(&["c", "b"]), "b"),
            (switched(&["c"]), "c"),
            (pinned("a"), "a"),
        ] {
            let mut rig = Rig::start(pes_aligned(one_clock(switched_program()), 1_500)).await;
            feed(&mut rig, "a", 90_000, 2).await;
            rig.take("b", None).await;
            assert_eq!(feed(&mut rig, "a", 93_600, 0).await, [0u16; 0], "waiting for b");
            assert!(rig.plays(&["a"]));
            let legs = next.programs[0].slots[0].switch_legs.clone();
            rig.replace(next).await;
            assert!(rig.plays(&[on]), "{legs:?}");
            if on == "c" {
                rig.send(on, es_pcr(0x100, 0x1B));
                assert_eq!(rig.sent_pids(1).await, [0x200], "c is on a clock of its own");
            }
            assert_eq!(feed(&mut rig, on, 97_200, 2).await, [0x200, 0x201], "{legs:?}");
            tokio::time::sleep(Duration::from_millis(1_800)).await;
            assert!(rig.raised("pes_splice_timeout").is_empty(), "nothing left to time out");
            assert!(rig.plays(&[on]), "{legs:?}");
        }

        let one = pes_aligned(one_clock(switched_program()), 1_500);
        let mut plan = one.clone();
        plan.programs.push(ProgramPlan {
            program_number: 2,
            pmt_pid: 0x1001,
            pcr_source: ("a".to_string(), 0x100),
            slots: vec![switch_slot(&["a", "b"], 0x100, 0x300, 0x1B)],
        });
        let mut rig = Rig::start(pes_aligned(plan, 1_500)).await;
        rig.send("a", es_pes(0x100, 0x1B, 90_000));
        assert_eq!(rig.sent_pids(2).await, [0x200, 0x300]);
        rig.take("b", None).await;
        rig.replace(one).await;
        until("program 1's budget", || rig.plays(&["b"])).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let timeouts: Vec<_> =
            rig.raised("pes_splice_timeout").iter().map(|d| d["out_pid"].clone()).collect();
        assert_eq!(timeouts, [serde_json::json!(0x200)], "and not program 2's");
        assert!(!rig.handle.join.is_finished(), "the assembler runs");
        rig.send("b", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
    }

    /// A slot that loses the leg it is on lands its splice rather than join
    /// another slot, and one with nothing to keep joins the first it can.
    #[test]
    fn a_slot_lands_its_splice_before_it_joins() {
        let plan = one_program(vec![
            switch_slot(&["c"], 0x100, 0x200, 0x1B),
            switch_slot(&["c", "b"], 0x101, 0x201, 0x0F),
            switch_slot(&["a", "b", "c"], 0x102, 0x202, 0x0F),
        ]);
        let on = |on: &str, to: Option<&str>| Switched {
            program: 0,
            on: on.to_string(),
            to: to.map(str::to_string),
        };
        let switched = [(0x200, on("c", None)), (0x201, on("a", Some("b")))].into();
        let (legs, spliced) = legs_after_swap(&plan, &[Some(0)], &switched);
        assert_eq!(legs[&0x201], "b");
        assert_eq!(legs[&0x202], "c");
        assert!(spliced.is_empty());
    }

    /// The slots of two programs can be on two clocks. Where a swap puts them in
    /// one program, that program's own keep the leg they are on and the others
    /// join them; in a program it adds, the first one's do.
    #[test]
    fn a_slot_of_another_program_is_new_to_a_program() {
        let plan = one_program(vec![
            switch_slot(&["a", "b"], 0x101, 0x201, 0x0F),
            switch_slot(&["a", "b"], 0x100, 0x200, 0x1B),
            switch_slot(&["a", "b"], 0x102, 0x202, 0x0F),
        ]);
        let on = |program: usize, on: &str, to: Option<&str>| Switched {
            program,
            on: on.to_string(),
            to: to.map(str::to_string),
        };
        let legs = |before: Option<usize>, switched: HashMap<u16, Switched>| {
            let (legs, spliced) = legs_after_swap(&plan, &[before], &switched);
            ([0x201, 0x200, 0x202].map(|pid| legs[&pid].clone()), spliced)
        };
        let switched = || HashMap::from([(0x201, on(1, "b", Some("a"))), (0x200, on(0, "a", None))]);
        let (now, spliced) = legs(Some(0), switched());
        assert_eq!(now, ["a", "a", "a"], "where program 0's is");
        assert!(spliced.is_empty(), "nor is another program's splice one of its own");
        let (now, spliced) = legs(Some(1), switched());
        assert_eq!(now, ["b", "a", "a"], "where program 1's is, or is being spliced to");
        assert!(spliced.contains(&0x201));
        for before in [None, Some(2)] {
            let (now, _) = legs(before, HashMap::from([(0x200, on(0, "b", None))]));
            assert_eq!(now, ["b", "b", "b"], "{before:?}: there is no other");
        }
    }

    /// The hold of a Take stays with its program over a plan swap for as long
    /// as its PCR comes from the input the Take named. Kept by the program's
    /// place in the plan it held the one a swap put there, for which no Take
    /// was.
    #[tokio::test]
    async fn a_hold_stays_with_its_program() {
        let code = "pid_bus_take_no_pcr";
        let held = async || {
            let rig = Rig::start(two_programs()).await;
            rig.take("b", None).await;
            until("the cut", || rig.plays(&["b", "c"])).await;
            rig
        };

        // Program 1 gone, program 2 is the first.
        let mut rig = held().await;
        let mut second = two_programs();
        second.programs.remove(0);
        rig.replace(second).await;
        rig.send("c", es_pes(0x100, 0x1B, 90_000));
        assert_eq!(rig.sent_pids(1).await, [0x300]);
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        assert!(rig.raised(code).is_empty(), "no Take moved it");
        rig.replace(two_programs()).await;
        assert_eq!(feed(&mut rig, "a", 93_600, 2).await, [0x200, 0x201], "nor the new program 1");

        // Both there, the other way round: program 1 is still held, program 2
        // still not.
        let mut rig = held().await;
        let mut plan = two_programs();
        plan.programs[0].slots = vec![
            switch_slot(&["b", "a"], 0x100, 0x200, 0x1B),
            switch_slot(&["b", "a"], 0x101, 0x201, 0x0F),
        ];
        plan.programs.reverse();
        rig.replace(plan).await;
        rig.send("c", es_pes(0x100, 0x1B, 90_000));
        assert_eq!(feed(&mut rig, "b", 90_000, 1).await, [0x300]);
        rig.send("b", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
        assert_eq!(feed(&mut rig, "b", 93_600, 2).await, [0x200, 0x201]);

        // Slots added to both: held, and said so.
        let mut rig = held().await;
        let mut plan = two_programs();
        plan.programs[1].slots.push(slot("c", 0x101, 0x301, 0x0F));
        plan.programs[0].slots.push(switch_slot(&["a", "b"], 0x102, 0x202, 0x0F));
        rig.replace(plan.clone()).await;
        assert_eq!(feed(&mut rig, "b", 90_000, 0).await, [0u16; 0], "held for b's PCR");
        let mut said = Vec::new();
        for _ in 0..100 {
            said = rig.raised(code);
            if !said.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let said: Vec<_> = said
            .iter()
            .map(|d| (d["program_number"].clone(), d["out_pid"].clone(), d["input_id"].clone()))
            .collect();
        assert_eq!(said, [(1.into(), 0x200.into(), "b".into())]);

        // Its PCR on another slot, from b still: b's PCR there lets it out.
        plan.programs[0].pcr_source = ("a".to_string(), 0x101);
        rig.replace(plan).await;
        rig.send("b", es_pcr(0x100, 0x1B));
        assert_eq!(feed(&mut rig, "b", 93_600, 0).await, [0u16; 0], "not the slot that carries it");
        rig.send("b", es_pcr(0x101, 0x0F));
        assert_eq!(rig.sent_pids(1).await, [0x201]);
        assert_eq!(feed(&mut rig, "b", 97_200, 2).await, [0x200, 0x201], "and it plays on");

        // Fixed to b it waits for b, fixed to another input for that one.
        for id in ["b", "c"] {
            let mut rig = held().await;
            let mut pinned = two_programs();
            pinned.programs[0].slots =
                vec![slot(id, 0x100, 0x200, 0x1B), slot(id, 0x101, 0x201, 0x0F)];
            pinned.programs[0].pcr_source = (id.to_string(), 0x100);
            pinned.programs.truncate(1);
            rig.replace(pinned).await;
            assert_eq!(feed(&mut rig, id, 90_000, 0).await, [0u16; 0], "{id}");
            rig.send(id, es_pcr(0x100, 0x1B));
            assert_eq!(rig.sent_pids(1).await, [0x200], "{id}");
        }

        // Two programs held by one Take: each waits for a PCR of its own.
        let mut plan = two_programs();
        plan.programs[1].slots = vec![switch_slot(&["a", "b"], 0x110, 0x300, 0x1B)];
        plan.programs[1].pcr_source = ("a".to_string(), 0x110);
        let mut rig = Rig::start(plan).await;
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b"])).await;
        rig.send("b", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
        rig.send("b", es_pes(0x110, 0x1B, 90_000));
        assert_eq!(feed(&mut rig, "b", 90_000, 2).await, [0x200, 0x201], "program 2 still held");
        rig.send("b", es_pcr(0x110, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x300]);
    }

    /// [`es_pes`] whose ES is `es`, zeros after it.
    fn es_pes_of(src_pid: u16, stream_type: u8, pts: u64, es: &[u8]) -> EsPacket {
        let mut packet = es_pes(src_pid, stream_type, pts);
        let mut pkt = packet.payload.to_vec();
        pkt[18..].fill(0);
        pkt[18..18 + es.len()].copy_from_slice(es);
        packet.payload = Bytes::from(pkt);
        packet
    }

    /// After a cut to another clock nothing of the leg before it is compared
    /// with a splice's: not its PTS, for which the slots waited out their
    /// budget with the program off air, nor its codec parameters, for which a
    /// splice was refused.
    #[tokio::test]
    async fn a_splice_is_not_measured_against_a_leg_on_another_clock() {
        // b and c on one clock, a on its own.
        let legs = ["a", "b", "c"];
        let mut plan = one_program(vec![
            switch_slot(&legs, 0x100, 0x200, 0x1B),
            switch_slot(&legs, 0x101, 0x201, 0x0F),
        ]);
        plan.clocks.insert("c".into(), source_pcr("b"));
        let plan = pes_aligned(plan, 60_000);
        let reasons = |rig: &mut Rig| -> Vec<serde_json::Value> {
            rig.raised("pes_splice_degraded").iter().map(|d| d["reason"].clone()).collect()
        };

        // b sends nothing: there is no PES to align c to.
        let mut rig = Rig::start(plan.clone()).await;
        feed(&mut rig, "a", 90_000, 2).await;
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b"])).await;
        assert_eq!(reasons(&mut rig), ["independent_clocks"; 2]);
        rig.take("c", None).await;
        until("the second cut", || rig.plays(&["c"])).await;
        assert_eq!(reasons(&mut rig), ["no_aligned_pes_reference"; 2]);
        rig.send("c", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200], "held until a PCR, b's or c's");

        // b sends PES and neither an SPS nor an ADTS header.
        let adts = |rate_idx: u8| [0xFF, 0xF1, 0x40 | rate_idx << 2, 0x80];
        let idr = |level: u8| {
            [0, 0, 0, 1, 0x67, 66, 0, level, 0xF4, 0x0A, 0x0F, 0x80, 0, 0, 0, 1, 0x65]
        };
        let mut rig = Rig::start(plan).await;
        rig.send("a", es_pes_of(0x100, 0x1B, 90_000, &idr(30)));
        rig.send("a", es_pes_of(0x101, 0x0F, 90_000, &adts(3)));
        assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b"])).await;
        rig.send("b", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
        rig.send("b", es_pes_of(0x100, 0x1B, 93_600, &[]));
        rig.send("b", es_pes_of(0x101, 0x0F, 93_600, &[]));
        assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
        rig.take("c", None).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rig.plays(&["b"]), "both slots wait for c");
        rig.send("c", es_pes_of(0x100, 0x1B, 97_200, &idr(40)));
        rig.send("c", es_pes_of(0x101, 0x0F, 97_200, &adts(4)));
        assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
        until("the splice", || rig.plays(&["c"])).await;
        let events: Vec<_> = std::iter::from_fn(|| rig.events.try_recv().ok())
            .filter_map(|e| Some(e.details?["error_code"].clone()))
            .filter(|code| code != "pes_splice_degraded")
            .collect();
        assert_eq!(events, ["pes_splice_completed"; 2], "nothing to refuse it for");

        // A cut on one clock leaves the reference: its PTS still compare.
        let mut rig = Rig::start(pes_aligned(one_clock(switched_program()), 60_000)).await;
        feed(&mut rig, "a", 90_000, 2).await;
        rig.take("b", Some(crate::config::models::SpliceMode::PmtBump)).await;
        until("the cut", || rig.plays(&["b"])).await;
        rig.take("a", None).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rig.plays(&["b"]), "both slots wait for a");
        assert!(rig.raised("pes_splice_degraded").is_empty());
    }

    /// A plan swap that adds a slot to a program a Take moved starts it
    /// where the program is, not on its initial leg, another clock. A slot
    /// without that leg starts on its own, and so does another program's.
    #[tokio::test]
    async fn a_slot_a_swap_adds_joins_its_program() {
        let rig = Rig::start(switched_program()).await;
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b"])).await;

        // It takes the place the kept slots are on, wherever it stands.
        let mut plan = switched_program();
        plan.programs[0].slots.insert(0, switch_slot(&["a", "b"], 0x102, 0x202, 0x0F));
        rig.replace(plan.clone()).await;
        assert!(rig.plays(&["b"]), "all three on b");

        // A leg dropped from a slot that was on it, with a sibling still on.
        let mut dropped = switched_program();
        dropped.programs[0].slots[1] = switch_slot(&["a", "c"], 0x101, 0x201, 0x0F);
        rig.replace(dropped).await;
        assert!(rig.plays(&["b", "a"]), "without b, its own initial leg");

        rig.replace(plan).await;
        assert!(rig.plays(&["b", "a"]), "a kept leg is kept");
        rig.take("b", None).await;
        until("all on b", || rig.plays(&["b"])).await;

        let mut two = switched_program();
        two.programs.push(ProgramPlan {
            program_number: 2,
            pmt_pid: 0x1001,
            pcr_source: ("c".to_string(), 0x100),
            slots: vec![switch_slot(&["c", "b"], 0x100, 0x300, 0x1B)],
        });
        rig.replace(two).await;
        assert!(rig.plays(&["b", "c"]), "another program starts on its own leg");

        // A slot the swap pins keeps no leg for another to join.
        let mut pinned = one_clock(one_program(vec![
            slot("b", 0x100, 0x200, 0x1B),
            switch_slot(&["a", "b"], 0x101, 0x203, 0x0F),
        ]));
        pinned.programs[0].pcr_source = ("b".to_string(), 0x100);
        rig.replace(pinned).await;
        assert!(rig.plays(&["a", "b"]), "the added slot starts on its own leg");
    }

    // ── What a plan swap holds, and what it leaves alone ───────────────

    /// `id`'s video and audio on program 1, beside a program 2 pinned to `c`.
    fn pinned_beside(id: &str) -> AssemblyPlan {
        beside(one_program(vec![slot(id, 0x100, 0x200, 0x1B), slot(id, 0x101, 0x201, 0x0F)]))
    }

    /// `plan` with a program 2 pinned to `c` after it.
    fn beside(mut plan: AssemblyPlan) -> AssemblyPlan {
        plan.programs.push(ProgramPlan {
            program_number: 2,
            pmt_pid: 0x1001,
            pcr_source: ("c".to_string(), 0x110),
            slots: vec![slot("c", 0x110, 0x300, 0x1B)],
        });
        plan
    }

    /// A swap that takes a program's PCR to an input on another clock, or on
    /// one not known to be the same: nothing of the program goes out until a
    /// PCR of that input has, on the slot that carries the program's. PES ahead
    /// of it went out on the clock before, hours off for two encoders. The
    /// other program does not notice.
    #[tokio::test]
    async fn a_swap_to_another_clock_waits_for_its_pcr() {
        let switched = |legs: &[&str]| {
            beside(one_program(vec![
                switch_slot(legs, 0x100, 0x200, 0x1B),
                switch_slot(legs, 0x101, 0x201, 0x0F),
            ]))
        };
        // The PCR moves from one slot to another, each on an input of its own.
        let mut two = pinned_beside("x");
        two.programs[0].slots.push(slot("y", 0x102, 0x202, 0x0F));
        let mut other = two.clone();
        other.programs[0].pcr_source = ("y".to_string(), 0x102);
        for (before, from, after, to) in [
            (pinned_beside("a"), "a", pinned_beside("b"), "b"),
            // Another flow's inputs: their clocks are not known.
            (pinned_beside("a"), "a", pinned_beside("x"), "x"),
            (pinned_beside("x"), "x", pinned_beside("a"), "a"),
            (pinned_beside("x"), "x", pinned_beside("y"), "y"),
            (switched(&["a", "c"]), "a", switched(&["b", "c"]), "b"),
            (switched(&["a", "b"]), "a", pinned_beside("b"), "b"),
            (pinned_beside("a"), "a", switched(&["b", "a"]), "b"),
        ] {
            let what = format!("{from} to {to}");
            let mut rig = Rig::start(before).await;
            assert_eq!(feed(&mut rig, from, 90_000, 2).await, [0x200, 0x201], "{what}");
            rig.replace(after).await;
            // PES without PCR, and a PCR on the slot that does not carry it.
            assert_eq!(feed(&mut rig, to, 90_000, 0).await, [0u16; 0], "{what}: held");
            rig.send(to, es_pcr(0x101, 0x0F));
            rig.send("c", es_pes(0x110, 0x1B, 90_000));
            assert_eq!(rig.sent_pids(1).await, [0x300], "{what}: only the other program");
            rig.send(to, es_pcr(0x100, 0x1B));
            let sent = rig.sent(1).await;
            assert_eq!(sent.len(), 1, "{what}");
            assert_eq!(sent[0].0, 0x200, "{what}");
            assert!(extract_pcr(&sent[0].1).is_some() && sent[0].1[5] & 0x80 != 0, "{what}: with DI");
            assert_eq!(feed(&mut rig, to, 93_600, 2).await, [0x200, 0x201], "{what}: it plays on");
        }
        let mut rig = Rig::start(two).await;
        rig.replace(other).await;
        rig.send("x", es_pcr(0x100, 0x1B));
        assert_eq!(feed(&mut rig, "x", 90_000, 0).await, [0u16; 0], "held for y's, not x's");
        rig.send("y", es_pcr(0x102, 0x0F));
        let sent = rig.sent(1).await;
        assert_eq!((sent.len(), sent[0].0), (1, 0x202));
        assert!(sent[0].1[5] & 0x80 != 0, "the PCR is on another slot now: DI");
        assert_eq!(feed(&mut rig, "x", 93_600, 2).await, [0x200, 0x201]);
        rig.send("x", es_pcr(0x100, 0x1B));
        let sent = rig.sent(1).await;
        assert_eq!(sent[0].1[5] & 0x80, 0, "x's is what it was");
    }

    /// A swap that takes a program's PCR to no other clock waits for nothing:
    /// to an input on the same one, to another PID of its input, whose clock
    /// need not be known, and one that moves only slots beside it.
    #[tokio::test]
    async fn a_swap_on_one_clock_holds_nothing() {
        let on = |id: &str, pid: u16| {
            one_program(vec![slot(id, pid, 0x200, 0x1B), slot(id, pid + 1, 0x201, 0x0F)])
        };
        let mut audio = on("a", 0x100);
        audio.programs[0].slots[1] = slot("x", 0x101, 0x201, 0x0F);
        let mut moved = audio.clone();
        moved.programs[0].slots[1] = slot("y", 0x101, 0x201, 0x0F);
        let mut pcr_on_audio = on("x", 0x100);
        pcr_on_audio.programs[0].pcr_source = ("x".to_string(), 0x101);
        for (before, after, [video, sound]) in [
            (one_clock(on("a", 0x100)), one_clock(on("b", 0x100)), [("b", 0x100), ("b", 0x101)]),
            (on("a", 0x100), on("a", 0x110), [("a", 0x110), ("a", 0x111)]),
            (on("x", 0x100), on("x", 0x110), [("x", 0x110), ("x", 0x111)]),
            (audio, moved, [("a", 0x100), ("y", 0x101)]),
            (on("x", 0x100), pcr_on_audio, [("x", 0x100), ("x", 0x101)]),
        ] {
            let mut rig = Rig::start(before).await;
            rig.replace(after).await;
            rig.send(video.0, es_pes(video.1, 0x1B, 90_000));
            rig.send(sound.0, es_pes(sound.1, 0x0F, 90_000));
            assert_eq!(rig.sent_pids(2).await, [0x200, 0x201], "{video:?} {sound:?}");
        }
    }

    /// A program of a plan of one is that one's whatever else a swap changes of
    /// it; one a swap adds was on no clock.
    #[tokio::test]
    async fn a_swap_holds_the_program_that_was_there() {
        let on = |id: &str, number: u16, out_pid: u16| {
            let mut plan = one_program(vec![slot(id, 0x100, out_pid, 0x1B)]);
            plan.programs[0].program_number = number;
            plan.programs[0].pmt_pid = 0x1000 + number;
            plan
        };
        for (number, out_pid) in [(1, 0x300), (7, 0x200), (7, 0x300)] {
            let mut rig = Rig::start(on("a", 1, 0x200)).await;
            rig.replace(on("b", number, out_pid)).await;
            rig.send("b", es_pes(0x100, 0x1B, 90_000));
            assert_eq!(rig.sent_pids(0).await, [0u16; 0], "{number} on 0x{out_pid:X}");
            rig.send("b", es_pcr(0x100, 0x1B));
            assert_eq!(rig.sent_pids(1).await, [out_pid]);
        }
        let mut rig = Rig::start(on("a", 1, 0x200)).await;
        let mut two = on("a", 1, 0x200);
        two.programs.extend(on("b", 2, 0x300).programs);
        rig.replace(two.clone()).await;
        rig.send("b", es_pes(0x100, 0x1B, 90_000));
        rig.send("a", es_pes(0x100, 0x1B, 90_000));
        assert_eq!(rig.sent_pids(2).await, [0x200, 0x300]);
        // Nor is a program held for one that goes.
        two.programs.remove(0);
        rig.replace(two).await;
        rig.send("b", es_pes(0x100, 0x1B, 93_600));
        assert_eq!(rig.sent_pids(1).await, [0x300]);
    }

    /// An input that sends its PCR on a PID the program does not carry is no
    /// reason to leave it off air: the hold of a swap ends 2 s after the first
    /// packet it withheld, and says so, once. Until a packet comes there is
    /// nothing to end.
    #[tokio::test]
    async fn a_swap_to_an_input_without_pcr_holds_for_2_s() {
        let code = "pid_bus_swap_no_pcr";
        let mut rig = Rig::start(pinned_beside("a")).await;
        rig.replace(pinned_beside("b")).await;
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(rig.raised(code).is_empty(), "b sends nothing");
        let first = Instant::now();
        let held_for = loop {
            rig.send("b", es_pes(0x101, 0x0F, 90_000));
            if !rig.sent_pids(0).await.is_empty() {
                break first.elapsed();
            }
            assert!(first.elapsed() < Duration::from_secs(5), "b is on air");
        };
        assert!(held_for >= PCR_HOLD_ALARM, "{held_for:?}");
        assert_eq!(
            rig.raised(code),
            [serde_json::json!({
                "error_code": code,
                "program_number": 1,
                "out_pid": 0x200,
                "input_id": "b",
                "source_pid": 0x100,
            })]
        );
        assert_eq!(feed(&mut rig, "b", 93_600, 2).await, [0x200, 0x201]);
        // It is the program's clock from now on: a swap that leaves it alone
        // holds nothing, and the next one to another does.
        let mut more = pinned_beside("b");
        more.programs[0].slots.push(slot("b", 0x102, 0x202, 0x0F));
        rig.replace(more).await;
        assert_eq!(feed(&mut rig, "b", 97_200, 2).await, [0x200, 0x201]);
        rig.replace(pinned_beside("a")).await;
        assert_eq!(feed(&mut rig, "a", 90_000, 0).await, [0u16; 0]);
        rig.send("a", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(rig.raised(code).is_empty(), "nor once it has its PCR");
        assert!(rig.raised("pid_bus_take_no_pcr").is_empty(), "no Take was");
    }

    /// A swap that leaves a program waiting where a Take put it leaves it to
    /// wait as long as the Take would, under another number too; one that moves
    /// it again, on the clock it waited for as well, is what it waits for from
    /// then on.
    #[tokio::test]
    async fn a_swap_bounds_only_its_own_hold() {
        let taken = async || {
            let rig = Rig::start(beside(switched_program())).await;
            rig.take("b", None).await;
            until("the cut", || rig.plays(&["b", "c"])).await;
            rig
        };
        let mut more = beside(switched_program());
        more.programs[0].slots.push(switch_slot(&["a", "b"], 0x102, 0x202, 0x0F));
        // A program with neither its number nor its slots is not the Take's:
        // one the swap made of it goes out after 2 s.
        let mut renumbered = beside(switched_program());
        renumbered.programs[0].program_number = 7;
        let mut made_of_it = more.clone();
        made_of_it.programs[0].program_number = 7;
        for (plan, goes_out) in [(renumbered, false), (made_of_it, true)] {
            let mut rig = taken().await;
            rig.replace(plan).await;
            assert_eq!(feed(&mut rig, "b", 90_000, 0).await, [0u16; 0]);
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            let out = if goes_out { vec![0x200, 0x201] } else { vec![] };
            assert_eq!(feed(&mut rig, "b", 93_600, out.len()).await, out);
            assert_eq!(rig.raised("pid_bus_swap_no_pcr").len(), usize::from(goes_out));
        }
        let mut rig = taken().await;
        rig.replace(more).await;
        assert_eq!(feed(&mut rig, "b", 90_000, 0).await, [0u16; 0]);
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert_eq!(feed(&mut rig, "b", 93_600, 0).await, [0u16; 0], "held as long as it takes");
        assert!(rig.raised("pid_bus_swap_no_pcr").is_empty());

        // c on b's clock: nothing of it is on air yet either.
        let mut moved = pinned_beside("c");
        moved.clocks.insert("c".into(), source_pcr("b"));
        rig.replace(moved).await;
        assert_eq!(feed(&mut rig, "c", 90_000, 0).await, [0u16; 0]);
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert_eq!(feed(&mut rig, "c", 93_600, 2).await, [0x200, 0x201]);
        assert_eq!(rig.raised("pid_bus_swap_no_pcr").len(), 1);
        // A Take after it waits as a Take does.
        rig.replace(beside(switched_program())).await;
        rig.send("a", es_pcr(0x100, 0x1B));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
        rig.replace(pinned_beside("b")).await;
        rig.replace(beside(one_program(vec![switch_slot(&["b", "a"], 0x100, 0x200, 0x1B)]))).await;
        rig.take("a", None).await;
        until("the cut", || rig.plays(&["a", "c"])).await;
        for _ in 0..3 {
            rig.send("a", es_pes(0x100, 0x1B, 90_000));
            assert_eq!(rig.sent_pids(0).await, [0u16; 0]);
            tokio::time::sleep(Duration::from_millis(900)).await;
        }
        assert!(rig.raised("pid_bus_swap_no_pcr").is_empty());
    }

    /// The source and PTS of each PES in `sent`, by the order they went out.
    fn stamps(sent: &[(u16, Vec<u8>)]) -> Vec<(u16, u64)> {
        sent.iter()
            .filter_map(|(pid, p)| Some((*pid, crate::engine::ts_parse::extract_pes_pts(p)?)))
            .collect()
    }

    /// What a fan-in the swap cancels had queued, with the packet it waits to
    /// queue, goes out on the plan it was for, ahead of the swap's PSI, none
    /// of it lost: on a stalled assembler that is a queue's depth. Nothing of
    /// it goes out after, on the slot's PID or on a removed slot's, and no PCR
    /// of it lets out a program that waits for the new input's.
    #[tokio::test]
    async fn what_a_cancelled_fan_in_queued_goes_out_before_the_swap() {
        for (after, clock) in [("b", "a"), ("b", "b")] {
            // The audio slot goes, the video is another input's.
            let mut next = one_program(vec![slot(after, 0x100, 0x200, 0x1B)]);
            next.clocks.insert("b".into(), source_pcr(clock));
            let pinned = one_program(vec![slot("a", 0x100, 0x200, 0x1B), slot("a", 0x101, 0x201, 0x0F)]);
            let mut rig = Rig::start(pinned).await;
            let stall = rig.stall().await;
            // More than there is room for: the rest is the bus ring's to lose.
            for pts in 0..400u64 {
                rig.send("a", es_pes(0x100, 0x1B, 90_000 + pts));
                rig.send("a", es_pes(0x101, 0x0F, 90_000 + pts));
            }
            rig.send("a", es_pcr(0x100, 0x1B));
            let taken = || 801 - rig.untaken("a", 0x100) - rig.untaken("a", 0x101);
            until("a full queue, and what each waits to queue", || taken() == FANIN_DEPTH + 2).await;
            while rig.out.try_recv().is_ok() {}
            let done = rig.replaced(next).await;
            drop(stall);
            done.await.unwrap();
            rig.send("b", es_pes(0x100, 0x1B, 5));
            let wire = rig.wire(Duration::from_millis(300)).await;
            let swap = wire
                .iter()
                .position(|(pid, p)| *pid == 0x1000 && pmt_version(p) == 1)
                .expect("the swap's PMT");
            let before = stamps(&wire[..swap]);
            assert_eq!(before.len(), FANIN_DEPTH + 2, "all they had taken");
            for pid in [0x200, 0x201] {
                let of: Vec<u64> =
                    before.iter().filter(|(p, _)| *p == pid).map(|(_, pts)| *pts).collect();
                let all: Vec<u64> = (0..of.len() as u64).map(|i| 90_000 + i).collect();
                assert_eq!(of, all, "0x{pid:X}: each once, in order");
            }
            let next: &[(u16, u64)] = if clock == "a" { &[(0x200, 5)] } else { &[] };
            assert_eq!(stamps(&wire[swap..]), next, "only b's, if it need not wait");
            rig.send("b", es_pcr(0x100, 0x1B));
            assert_eq!(rig.sent_pids(1).await, [0x200]);
        }
    }

    /// What a fan-in the swap keeps had queued goes out on the new plan: the
    /// packets of a leg it puts on air.
    #[tokio::test]
    async fn what_a_kept_fan_in_queued_goes_out_after_the_swap() {
        let mut rig = Rig::start(one_clock(switched_program())).await;
        let stall = rig.stall().await;
        for pts in 0..5u64 {
            rig.send("a", es_pes(0x100, 0x1B, 90_000 + pts));
            rig.send("b", es_pes(0x100, 0x1B, 95_000 + pts));
        }
        until("the queue", || rig.untaken("a", 0x100) + rig.untaken("b", 0x100) == 0).await;
        let mut pinned = one_program(vec![slot("b", 0x100, 0x300, 0x1B)]);
        pinned.clocks.insert("b".into(), source_pcr("a"));
        let done = rig.replaced(pinned).await;
        // The next swap finds some of them still to send.
        let mut again = one_program(vec![slot("b", 0x100, 0x301, 0x1B)]);
        again.clocks.insert("b".into(), source_pcr("a"));
        let and_again = rig.replaced(again).await;
        drop(stall);
        done.await.unwrap();
        and_again.await.unwrap();
        let of = |pid: u16, first: u64| (0..5).map(|i| (pid, first + i)).collect::<Vec<_>>();
        let sent = stamps(&rig.sent(10).await);
        assert_eq!(sent, [of(0x200, 90_000), of(0x301, 95_000)].concat());
    }

    /// Each slot goes on with its own fan-in, or for a slot whose `out_pid` is
    /// new with its program's, before one that only reads the same source: two
    /// slots that read one PID are not at one place in it.
    #[test]
    fn a_slot_goes_on_with_its_own_fan_in() {
        let program = |number: u16, out_pid: u16| ProgramPlan {
            program_number: number,
            pmt_pid: 0x1000 + number,
            pcr_source: ("a".to_string(), 0x100),
            slots: vec![slot("a", 0x100, out_pid, 0x1B), slot("a", 0x101, out_pid + 1, 0x0F)],
        };
        let plan = |programs: &[(u16, u16)]| AssemblyPlan {
            programs: programs.iter().map(|(n, pid)| program(*n, *pid)).collect(),
            clocks: HashMap::new(),
        };
        let old = plan(&[(1, 0x200), (2, 0x300), (3, 0x400)]);
        let mut flat: Vec<Option<FlatSlot>> = flatten(&old).into_iter().map(Some).collect();
        let reused = |flat: &[Option<FlatSlot>], new: &AssemblyPlan| -> Vec<Option<usize>> {
            let fanins = reuse_fanins(flat, new, &programs_before(&old, new));
            for (slot, fanin) in &fanins {
                if let Some(i) = fanin {
                    assert_eq!(flat[*i].as_ref().expect("a live one").source, slot.source);
                }
            }
            fanins.into_iter().map(|(_, fanin)| fanin).collect()
        };
        // In any order, whatever the numbers.
        for (programs, expected) in [
            ([(3, 0x400), (2, 0x300), (1, 0x200)], [4, 5, 2, 3, 0, 1]),
            ([(7, 0x400), (1, 0x300), (2, 0x200)], [4, 5, 2, 3, 0, 1]),
            // New PIDs: by the number. Then one of them has its own again.
            ([(2, 0x310), (3, 0x410), (1, 0x210)], [2, 3, 4, 5, 0, 1]),
            ([(2, 0x310), (3, 0x300), (1, 0x210)], [4, 5, 2, 3, 0, 1]),
        ] {
            assert_eq!(reused(&flat, &plan(&programs)), expected.map(Some), "{programs:?}");
        }
        // Any of its source, for a program that is new, while there is one.
        let more = plan(&[(8, 0x500), (2, 0x300), (9, 0x600), (10, 0x700)]);
        let fanins = reused(&flat, &more);
        assert_eq!(fanins[2..4], [Some(2), Some(3)]);
        assert_eq!([fanins[0], fanins[4], fanins[6]], [Some(0), Some(4), None]);
        assert_eq!([fanins[1], fanins[5], fanins[7]], [Some(1), Some(5), None]);
        // Never one that is gone.
        flat[0] = None;
        assert_eq!(reused(&flat, &plan(&[(1, 0x200)]))[0], Some(2));

        // Two slots of one program on one PID.
        let mut twice = plan(&[(1, 0x200)]);
        twice.programs[0].slots[1] = slot("a", 0x100, 0x201, 0x1B);
        let flat: Vec<Option<FlatSlot>> = flatten(&twice).into_iter().map(Some).collect();
        let mut reversed = twice.clone();
        reversed.programs[0].slots.reverse();
        let fanins = reuse_fanins(&flat, &reversed, &programs_before(&twice, &reversed));
        assert_eq!(fanins.iter().map(|(_, fanin)| *fanin).collect::<Vec<_>>(), [Some(1), Some(0)]);
    }

    /// A program is the one with its slots wherever a plan has it and whatever
    /// its number: every way to order, leave out and renumber three programs.
    #[test]
    fn a_program_is_known_by_its_slots_and_its_number() {
        let program = |number: u16, out_pid: u16| ProgramPlan {
            program_number: number,
            pmt_pid: 0x1000 + number,
            pcr_source: ("a".to_string(), 0x100),
            slots: vec![slot("a", 0x100, out_pid, 0x1B), slot("a", 0x101, out_pid + 1, 0x0F)],
        };
        let of = |programs: Vec<ProgramPlan>| AssemblyPlan { programs, clocks: HashMap::new() };
        let pids = [0x200, 0x300, 0x400];
        let old = of((0..3).map(|i| program(i as u16 + 1, pids[i])).collect());
        let orders: Vec<Vec<usize>> = (0..64usize)
            .map(|i| vec![i % 4, i / 4 % 4, i / 16])
            .map(|order| order.into_iter().filter(|o| *o < 3).collect::<Vec<_>>())
            .filter(|order| order.iter().collect::<HashSet<_>>().len() == order.len())
            .collect();
        let mut seen = 0;
        for order in &orders {
            for numbers in 0..125usize {
                // Each a number of its own: one of the three, or two others.
                let number = |i: usize| (numbers / 5usize.pow(i as u32) % 5 + 1) as u16;
                let all: HashSet<u16> = (0..order.len()).map(number).collect();
                if all.len() != order.len() {
                    continue;
                }
                let programs = order.iter().enumerate().map(|(at, o)| program(number(at), pids[*o]));
                let before = programs_before(&old, &of(programs.collect()));
                assert_eq!(before, order.iter().map(|o| Some(*o)).collect::<Vec<_>>());
                seen += 1;
            }
        }
        assert!(seen > 500, "{seen}");

        // Edited, by its number; moved to another and edited, by a PID; and
        // no program is two, nor one that shares nothing.
        let edit = |mut prog: ProgramPlan| {
            prog.slots.push(slot("a", 0x102, prog.slots[0].out_pid + 2, 0x0F));
            prog
        };
        let renumbered_pid = |number: u16, out_pid: u16, pid: u16| {
            let mut prog = program(number, out_pid);
            prog.slots[1].out_pid = pid;
            prog
        };
        for (new, before) in [
            (vec![edit(program(2, 0x500)), edit(program(1, 0x300))], vec![Some(1), Some(0)]),
            (vec![edit(program(2, 0x200)), edit(program(1, 0x300))], vec![Some(1), Some(0)]),
            (vec![edit(program(8, 0x300)), program(9, 0x600)], vec![Some(1), None]),
            (vec![renumbered_pid(8, 0x500, 0x301), program(3, 0x700)], vec![Some(1), Some(2)]),
            (vec![renumbered_pid(8, 0x200, 0x301), renumbered_pid(9, 0x500, 0x201)], vec![Some(0), None]),
            (vec![program(8, 0x500)], vec![None]),
        ] {
            assert_eq!(programs_before(&old, &of(new.clone())), before, "{new:?}");
        }
        let one = of(vec![program(1, 0x200)]);
        assert_eq!(programs_before(&one, &of(vec![program(8, 0x500)])), [Some(0)]);
        let two = of(vec![program(8, 0x500), program(9, 0x600)]);
        assert_eq!(programs_before(&one, &two), [None, None]);
    }

    /// After a swap to another clock nothing of the leg before it is compared
    /// with a splice's, as after a Take: a slot had its PTS until the new leg
    /// sent one. A swap on one clock leaves it.
    #[tokio::test]
    async fn a_swap_leaves_a_splice_no_reference_of_another_clock() {
        let legs = |legs: [&str; 2]| {
            one_program(vec![
                switch_slot(&legs, 0x100, 0x200, 0x1B),
                switch_slot(&legs, 0x101, 0x201, 0x0F),
            ])
        };
        for (shared, armed) in [("b", false), ("a", true)] {
            // a on its own clock or on b's, b and c on one.
            let of = |mut plan: AssemblyPlan| {
                plan.clocks.insert("b".into(), source_pcr(shared));
                plan.clocks.insert("c".into(), source_pcr(shared));
                pes_aligned(plan, 60_000)
            };
            let mut rig = Rig::start(of(legs(["a", "c"]))).await;
            let adts = [0xFF, 0xF1, 0x40 | 3 << 2, 0x80];
            let idr = [0, 0, 0, 1, 0x67, 66, 0, 30, 0xF4, 0x0A, 0x0F, 0x80, 0, 0, 0, 1, 0x65];
            rig.send("a", es_pes_of(0x100, 0x1B, 90_000, &idr));
            rig.send("a", es_pes_of(0x101, 0x0F, 90_000, &adts));
            assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
            rig.replace(of(legs(["b", "c"]))).await;
            assert!(rig.plays(&["b"]));
            rig.take("c", None).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let reasons: Vec<_> =
                rig.raised("pes_splice_degraded").iter().map(|d| d["reason"].clone()).collect();
            if armed {
                assert!(reasons.is_empty() && rig.plays(&["b"]), "b's PTS are a's: {reasons:?}");
                continue;
            }
            assert_eq!(reasons, ["no_aligned_pes_reference"; 2]);
            assert!(rig.plays(&["c"]));

            // Nor its codec parameters: c's differ from a's, and b sent none.
            let mut rig = Rig::start(of(legs(["a", "c"]))).await;
            rig.send("a", es_pes_of(0x100, 0x1B, 90_000, &idr));
            rig.send("a", es_pes_of(0x101, 0x0F, 90_000, &adts));
            assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
            rig.replace(of(legs(["b", "c"]))).await;
            rig.send("b", es_pcr(0x100, 0x1B));
            assert_eq!(rig.sent_pids(1).await, [0x200]);
            rig.send("b", es_pes_of(0x100, 0x1B, 90_000, &[]));
            rig.send("b", es_pes_of(0x101, 0x0F, 90_000, &[]));
            assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
            rig.take("c", None).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut other = idr;
            other[7] = 40;
            rig.send("c", es_pes_of(0x100, 0x1B, 97_200, &other));
            rig.send("c", es_pes_of(0x101, 0x0F, 97_200, &[0xFF, 0xF1, 0x40 | 4 << 2, 0x80]));
            assert_eq!(rig.sent_pids(2).await, [0x200, 0x201]);
            assert!(rig.raised("pes_splice_codec_param_mismatch").is_empty());
        }

        // An unknown clock is the same one to itself: what a slot stays on is
        // a reference still when its clock is known.
        let mut rig = Rig::start(pes_aligned(legs(["x", "y"]), 60_000)).await;
        feed(&mut rig, "x", 90_000, 2).await;
        let mut more = pes_aligned(legs(["x", "y"]), 60_000);
        more.programs[0].slots.push(slot("x", 0x102, 0x202, 0x0F));
        rig.replace(more).await;
        let known = clocks(&[("x", ClockIdentity::Wallclock), ("y", ClockIdentity::Wallclock)]);
        rig.handle.plan_tx.send(PlanCommand::InputClocks(known)).await.unwrap();
        rig.take("y", None).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rig.raised("pes_splice_degraded").is_empty(), "armed, not cut");

        // A slot that was no Switch slot in between has none.
        let of = |plan: AssemblyPlan| pes_aligned(one_clock(plan), 60_000);
        let mut rig = Rig::start(of(legs(["a", "b"]))).await;
        feed(&mut rig, "a", 90_000, 2).await;
        rig.replace(one_clock(one_program(vec![slot("a", 0x100, 0x200, 0x1B), slot("a", 0x101, 0x201, 0x0F)]))).await;
        rig.replace(of(legs(["a", "b"]))).await;
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b"])).await;
        let reasons: Vec<_> =
            rig.raised("pes_splice_degraded").iter().map(|d| d["reason"].clone()).collect();
        assert_eq!(reasons, ["no_aligned_pes_reference"; 2]);
    }

    /// A PCR carries the discontinuity where the swap gave its slot another
    /// source, or the program's PCR, and one that was due goes on being due.
    #[tokio::test]
    async fn a_swap_marks_only_the_pcrs_it_moves() {
        let mut rig = Rig::start(beside(one_clock(switched_program()))).await;
        async fn marked(rig: &mut Rig, input: &str, pid: u16) -> (u16, bool) {
            rig.send(input, es_pcr(pid, 0x1B));
            let sent = rig.sent(1).await;
            assert_eq!(sent.len(), 1);
            (sent[0].0, sent[0].1[5] & 0x80 != 0)
        }
        assert_eq!(marked(&mut rig, "a", 0x100).await, (0x200, false));
        assert_eq!(marked(&mut rig, "c", 0x110).await, (0x300, false));
        // A slot beside the PCR's is another source's: not the PCR's.
        let mut audio = beside(one_clock(switched_program()));
        audio.programs[0].slots[1] = switch_slot(&["a", "b"], 0x109, 0x201, 0x0F);
        audio.programs.reverse();
        rig.replace(audio.clone()).await;
        assert_eq!(marked(&mut rig, "a", 0x100).await, (0x200, false));
        assert_eq!(marked(&mut rig, "c", 0x110).await, (0x300, false));
        assert_eq!(marked(&mut rig, "a", 0x109).await, (0x201, true));
        // Due since a Take, and still due after a swap of nothing it touches.
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b", "c"])).await;
        audio.programs.reverse();
        rig.replace(audio.clone()).await;
        assert_eq!(marked(&mut rig, "b", 0x100).await, (0x200, true));
        assert_eq!(marked(&mut rig, "b", 0x100).await, (0x200, false), "once");
        // On a leg the swap puts it on, which it had before.
        let mut one_leg = audio.clone();
        one_leg.programs[0].slots[0] = switch_slot(&["a"], 0x100, 0x200, 0x1B);
        one_leg.programs[0].slots[1] = switch_slot(&["a"], 0x109, 0x201, 0x0F);
        rig.replace(one_leg).await;
        assert_eq!(marked(&mut rig, "a", 0x100).await, (0x200, true));
        rig.replace(audio.clone()).await;
        rig.take("b", None).await;
        until("the cut", || rig.plays(&["b", "c"])).await;
        assert_eq!(marked(&mut rig, "b", 0x100).await, (0x200, true));
        // Another PID of the same input, and the PCR on another slot.
        audio.programs[1].slots[0] = slot("c", 0x111, 0x300, 0x1B);
        audio.programs[1].pcr_source = ("c".to_string(), 0x111);
        audio.programs[0].pcr_source = ("b".to_string(), 0x109);
        rig.replace(audio).await;
        assert_eq!(marked(&mut rig, "c", 0x111).await, (0x300, true));
        assert_eq!(marked(&mut rig, "b", 0x109).await, (0x201, true));
        assert_eq!(marked(&mut rig, "b", 0x100).await, (0x200, false));
    }

    /// A changed PMT's version is the one after the last its number and its
    /// PID had, whichever program had it, and a program that comes back does
    /// not start over.
    #[test]
    fn a_pmt_version_never_steps_back_or_repeats() {
        let prog = |program_number: u16, pmt_pid: u16| ProgramPlan {
            program_number,
            pmt_pid,
            pcr_source: ("a".to_string(), 0x100),
            slots: Vec::new(),
        };
        let mut versions = PmtVersions::default();
        let mut bump = |number: u16, pid: u16| {
            versions.bump(&prog(number, pid));
            versions.of(&prog(number, pid))
        };
        assert_eq!([bump(1, 0x1000), bump(1, 0x1000), bump(1, 0x1000)], [0, 1, 2]);
        assert_eq!(bump(2, 0x1001), 0, "its own");
        assert_eq!(bump(7, 0x1000), 3, "another number on the PID");
        assert_eq!(bump(1, 0x1000), 4, "the first one back");
        assert_eq!(bump(2, 0x1000), 5, "ahead of the PID's, not of its own");
        assert_eq!(bump(2, 0x1002), 6, "and of its own on a new PID");
        assert_eq!(bump(1, 0x1002), 7);
        let mut versions = PmtVersions::default();
        let one = prog(1, 0x1000);
        for expected in (0..32).chain(0..4) {
            versions.bump(&one);
            assert_eq!(versions.of(&one), expected);
        }
        // Over the wrap: 3 is past 30.
        versions.by_program.insert(2, 30);
        versions.bump(&prog(2, 0x1000));
        assert_eq!(versions.of(&prog(2, 0x1000)), 4);
        versions.by_pid.insert(0x1005, 30);
        versions.bump(&prog(2, 0x1005));
        assert_eq!(versions.of(&prog(2, 0x1005)), 5);
    }

    /// The PSI of a swap that changes a table says so, and the packets of a PID
    /// count on when the swap makes a PMT's of an ES PID, or the reverse.
    #[tokio::test]
    async fn a_swap_that_changes_a_table_changes_its_version() {
        let mut rig = Rig::start(two_programs()).await;
        assert_eq!(rig.versions(&[0, 0x1000, 0x1001]).await, [0, 0, 0]);
        // In another order the PAT is another one, the PMTs are not.
        let mut plan = two_programs();
        plan.programs.reverse();
        rig.replace(plan.clone()).await;
        assert_eq!(rig.versions(&[0, 0x1000, 0x1001]).await, [1, 0, 0]);
        // Another PMT PID, number, PCR slot and slot.
        plan.programs[0].pmt_pid = 0x1002;
        rig.replace(plan.clone()).await;
        assert_eq!(rig.versions(&[0, 0x1000, 0x1002]).await, [2, 0, 1]);
        plan.programs[0].program_number = 5;
        rig.replace(plan.clone()).await;
        assert_eq!(rig.versions(&[0, 0x1000, 0x1002]).await, [3, 0, 2]);
        plan.programs[1].pcr_source = ("a".to_string(), 0x101);
        rig.replace(plan.clone()).await;
        assert_eq!(rig.versions(&[0, 0x1000, 0x1002]).await, [3, 1, 2]);
        plan.programs[1].slots.pop();
        plan.programs[1].pcr_source = ("a".to_string(), 0x100);
        rig.replace(plan.clone()).await;
        assert_eq!(rig.versions(&[0, 0x1000, 0x1002]).await, [3, 2, 2]);
        // A program that goes, and comes back.
        let gone = plan.programs.remove(1);
        rig.replace(plan.clone()).await;
        assert_eq!(rig.versions(&[0, 0x1002]).await, [4, 2]);
        plan.programs.push(gone);
        rig.replace(plan).await;
        assert_eq!(rig.versions(&[0, 0x1000, 0x1002]).await, [5, 3, 2]);

        // Its packets on 0x1000 are PMTs until the swap, then video.
        let plan = one_program(vec![slot("a", 0x100, 0x200, 0x1B)]);
        let mut other = one_program(vec![slot("a", 0x100, 0x1000, 0x1B)]);
        other.programs[0].pmt_pid = 0x200;
        let mut rig = Rig::start(plan).await;
        rig.send("a", es_pes(0x100, 0x1B, 90_000));
        let mut wire = rig.wire(Duration::from_millis(250)).await;
        rig.replace(other).await;
        rig.send("a", es_pes(0x100, 0x1B, 90_000));
        wire.extend(rig.wire(Duration::from_millis(250)).await);
        for pid in [0x200, 0x1000] {
            let ccs: Vec<u8> =
                wire.iter().filter(|(p, _)| *p == pid).map(|(_, p)| p[3] & 0x0F).collect();
            assert!(ccs.len() >= 3, "{ccs:?}");
            assert!(ccs.windows(2).all(|w| w[1] == (w[0] + 1) & 0x0F), "0x{pid:X}: {ccs:?}");
        }
    }

    /// What a swap leaves alone its PMT says as before: the codec the assembler
    /// corrected, for which no second event is raised, and the descriptors of a
    /// source that has no catalogue at the time. A change that came with the
    /// swap bumps the version with it.
    #[tokio::test]
    async fn a_swap_leaves_a_pmt_it_does_not_edit() {
        use crate::engine::ts_psi_catalog::*;
        let store = |entries: &[(u16, u8, &str)]| {
            let store = PsiCatalogStore::new();
            let streams = entries.iter().map(|(pid, stream_type, descriptors)| CatalogStream {
                pid: *pid,
                stream_type: *stream_type,
                codec: "test".to_string(),
                kind: CatalogStreamKind::Video,
                descriptors_hex: descriptors.to_string(),
            });
            let program = CatalogProgram {
                program_number: 1,
                pmt_pid: 0x1000,
                pcr_pid: None,
                streams: streams.collect(),
            };
            PsiCatalogStore::seed_for_test(
                &store,
                PsiCatalog { programs: vec![program], last_updated_us: 1 },
            );
            Arc::new(store)
        };
        // The program's PMT version, and its slots' stream types and descriptors.
        async fn pmt(rig: &mut Rig) -> (u8, Vec<(u8, Vec<u8>)>) {
            let wire = rig.wire(Duration::from_millis(350)).await;
            let (_, p) = wire.iter().rev().find(|(pid, _)| *pid == 0x1000).expect("a PMT");
            let end = 8 + ((p[6] as usize & 0x0F) << 8 | p[7] as usize) - 4;
            let (mut at, mut entries) = (17, Vec::new());
            while at < end {
                let len = (p[at + 3] as usize & 0x0F) << 8 | p[at + 4] as usize;
                entries.push((p[at], p[at + 5..at + 5 + len].to_vec()));
                at += 5 + len;
            }
            (pmt_version(p), entries)
        }
        let audio = vec![0x6A, 0x01, 0x00];
        let said = |video: u8| vec![(video, Vec::new()), (0x06, audio.clone())];
        // Declared HEVC, and H.264.
        let plan = || one_program(vec![slot("a", 0x100, 0x200, 0x24), slot("a", 0x101, 0x201, 0x06)]);
        let full = || {
            [("a".to_string(), store(&[(0x100, 0x1B, ""), (0x101, 0x06, "6a0100")]))]
                .into_iter()
                .collect::<HashMap<_, _>>()
        };
        let mismatches = |rig: &mut Rig| rig.raised("pid_bus_stream_type_mismatch").len();

        let mut rig = Rig::start_with(plan(), full()).await;
        assert_eq!(pmt(&mut rig).await, (1, said(0x1B)), "corrected");
        assert_eq!(mismatches(&mut rig), 1);
        let mut more = plan();
        more.programs.insert(0, beside(plan()).programs.remove(1));
        rig.replace(more.clone()).await;
        assert_eq!(pmt(&mut rig).await, (1, said(0x1B)), "as it was");
        more.programs.reverse();
        rig.replace(more.clone()).await;
        assert_eq!(pmt(&mut rig).await, (1, said(0x1B)), "and again");
        assert_eq!(mismatches(&mut rig), 0);
        // A catalogue that is empty when the swap comes.
        let empty = [("a".to_string(), Arc::new(PsiCatalogStore::new()))].into_iter().collect();
        let (applied, done) = tokio::sync::oneshot::channel();
        more.programs.reverse();
        let swap = |plan: AssemblyPlan, catalogs, applied| PlanCommand::ReplacePlan {
            plan,
            splice_mode_override: None,
            catalogs,
            applied: Some(applied),
        };
        rig.handle.plan_tx.send(swap(more.clone(), empty, applied)).await.unwrap();
        done.await.unwrap();
        assert_eq!(pmt(&mut rig).await, (1, said(0x1B)), "kept");
        // One with another descriptor.
        let other = [("a".to_string(), store(&[(0x100, 0x1B, ""), (0x101, 0x06, "7a0100")]))];
        let (applied, done) = tokio::sync::oneshot::channel();
        more.programs.reverse();
        while rig.out.try_recv().is_ok() {}
        rig.handle.plan_tx.send(swap(more.clone(), other.into_iter().collect(), applied)).await.unwrap();
        done.await.unwrap();
        for (pid, p) in rig.wire(Duration::from_millis(350)).await {
            // The descriptor is the second entry's, the first has none.
            let expected = if p[27] == 0x7A { 2 } else { 1 };
            assert!(pid != 0x1000 || pmt_version(&p) == expected, "{p:X?}");
        }
        let other = vec![(0x1B, Vec::new()), (0x06, vec![0x7A, 0x01, 0x00])];
        assert_eq!(pmt(&mut rig).await, (2, other));
        // Declared as it is now, and another way: judged anew.
        let mut edited = more.clone();
        edited.programs[0].slots[0].stream_type = 0x1B;
        rig.replace(edited.clone()).await;
        assert_eq!(pmt(&mut rig).await.0, 2, "what the PMT says already");
        edited.programs[0].slots[0].stream_type = 0x24;
        rig.replace(edited.clone()).await;
        assert_eq!(pmt(&mut rig).await.0, 4, "declared wrong, and corrected");
        assert_eq!(mismatches(&mut rig), 1);
        for (slot, pid) in edited.programs[0].slots.iter_mut().zip([0x100, 0x101]) {
            slot.source = ("b".to_string(), pid);
        }
        edited.programs[0].pcr_source = ("b".to_string(), 0x100);
        rig.replace(edited).await;
        let b = vec![(0x24, Vec::new()), (0x06, Vec::new())];
        assert_eq!(pmt(&mut rig).await.1, b, "declared so, and none of a's descriptors");
    }

    /// What became of a slot's source is said of the program it is in now.
    #[tokio::test]
    async fn a_closed_source_names_its_slots_program_as_it_is() {
        let mut rig = Rig::start(two_programs()).await;
        let mut plan = two_programs();
        plan.programs.reverse();
        plan.programs[1].program_number = 9;
        rig.replace(plan.clone()).await;
        let sender = rig.bus.sender_for("a", 0x100, 0x1B);
        rig.bus.remove_if_same(&("a".to_string(), 0x100), &sender);
        drop(sender);
        let mut closed = Vec::new();
        for _ in 0..100 {
            closed = rig.raised("pid_bus_slot_source_closed");
            if !closed.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            closed,
            [serde_json::json!({
                "error_code": "pid_bus_slot_source_closed",
                "program_number": 9,
                "out_pid": 0x200,
                "source_input_id": "a",
                "source_pid": 0x100,
            })]
        );
        // The next swap reads it again.
        plan.programs.reverse();
        rig.replace(plan).await;
        rig.send("a", es_pes(0x100, 0x1B, 90_000));
        assert_eq!(rig.sent_pids(1).await, [0x200]);
    }

    /// What [`swaps_leave_what_they_do_not_edit`] knows of a program on air.
    #[derive(Clone, Debug)]
    struct OnAirProgram {
        /// Its slots: one of the five [`KINDS`].
        kind: usize,
        number: u16,
        pmt_pid: u16,
        /// With a slot more.
        extra: bool,
        /// The input it plays, the one a pending splice takes it to, and
        /// whether it waits for that input's PCR.
        on: &'static str,
        to: Option<&'static str>,
        held: bool,
        /// Whether its next PCR is due a discontinuity.
        marked: bool,
    }

    /// The inputs of a kind's slots (more than one: Switch slots, the first
    /// initial) and the source PID of the first slot, its PCR's, the rest
    /// following. e and f are on one clock, and their slots spliced.
    const KINDS: [(&[&str], u16); 5] = [
        (&["a", "b"], 0x100),
        (&["e", "f"], 0x100),
        (&["c"], 0x100),
        (&["a", "b"], 0x110),
        (&["d"], 0x100),
    ];

    impl OnAirProgram {
        fn fresh(kind: usize, number: u16) -> Self {
            let on = KINDS[kind].0[0];
            let pmt_pid = 0x1000 + kind as u16;
            Self { kind, number, pmt_pid, extra: false, on, to: None, held: false, marked: true }
        }

        /// The source and out PIDs of its slots.
        fn pids(&self) -> Vec<(u16, u16)> {
            let slots = 2 + u16::from(self.extra);
            let out = 0x200 + 0x10 * self.kind as u16;
            (0..slots).map(|i| (KINDS[self.kind].1 + i, out + i)).collect()
        }

        fn planned(&self) -> ProgramPlan {
            let (legs, pcr_pid) = KINDS[self.kind];
            let slots = self.pids().into_iter().map(|(src, out)| {
                let mut slot = switch_slot(legs, src, out, 0x0F);
                if legs.len() == 1 {
                    slot.switch_legs = None;
                }
                if legs[0] == "e" {
                    slot.splice_mode = crate::config::models::SpliceMode::PesAligned;
                    slot.splice_budget_ms = Some(600_000);
                }
                slot
            });
            ProgramPlan {
                program_number: self.number,
                pmt_pid: self.pmt_pid,
                pcr_source: (legs[0].to_string(), pcr_pid),
                slots: slots.collect(),
            }
        }
    }

    /// The plan of `programs`, and every input but e and f on its own clock.
    fn plan_on_air(programs: &[OnAirProgram]) -> AssemblyPlan {
        let ids = ["a", "b", "c", "d", "e"].iter();
        let mut map: HashMap<String, ClockIdentity> =
            ids.map(|id| (id.to_string(), source_pcr(id))).collect();
        map.insert("f".into(), source_pcr("e"));
        AssemblyPlan { programs: programs.iter().map(OnAirProgram::planned).collect(), clocks: map }
    }

    /// Takes, PCRs and plan swaps at random, with a model beside: a swap moves
    /// what it does not edit nowhere. A program's leg, its hold, its splice, the
    /// discontinuity due on its PCR and the continuity counters of its PIDs are
    /// what they were, and its PMT's version too unless the swap edits or
    /// renumbers it, whatever programs the swap drops, adds or moves around it.
    #[tokio::test]
    async fn swaps_leave_what_they_do_not_edit() {
        let mut runs = tokio::task::JoinSet::new();
        for seed in 1..=16u64 {
            runs.spawn(swap_at_random(seed));
        }
        let mut kept = 0;
        while let Some(run) = runs.join_next().await {
            kept += run.expect("the model and the assembler agree");
        }
        assert!(kept > 100, "programs a swap left alone: {kept}");
    }

    /// One run of [`swaps_leave_what_they_do_not_edit`]: how many times a swap
    /// left a program that was taken, held or spliced as it was.
    async fn swap_at_random(seed: u64) -> usize {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut below = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 33) as usize % n
        };
        let mut on_air: Vec<OnAirProgram> = [0, 1, 2]
            .map(|kind| OnAirProgram { marked: false, ..OnAirProgram::fresh(kind, kind as u16 + 1) })
            .to_vec();
        let mut next_number = 10;
        let mut rig = Rig::start(plan_on_air(&on_air)).await;
        // The last version of each PMT, by PID and by number, and the last CC.
        let mut by_pid: HashMap<u16, u8> = HashMap::new();
        let mut by_number: HashMap<u16, u8> = HashMap::new();
        let mut ccs: HashMap<u16, u8> = HashMap::new();
        let mut kept = 0;
        let mut log: Vec<String> = Vec::new();
        for step in 0..40 {
            // The programs whose PMT changes.
            let mut edited: Vec<u16> = Vec::new();
            let op = if step == 0 { 0 } else { 1 + below(8) };
            match op {
                0 => {}
                1 | 2 => {
                    let take = ["a", "b", "e", "f"][below(4)];
                    log.push(format!("take {take}"));
                    rig.take(take, None).await;
                    for p in on_air.iter_mut().filter(|p| KINDS[p.kind].0.contains(&take)) {
                        p.to = None;
                        if KINDS[p.kind].0.len() == 1 || p.on == take {
                            continue;
                        }
                        if p.kind == 1 {
                            p.to = Some(take);
                            continue;
                        }
                        (p.on, p.held, p.marked) = (take, true, true);
                        edited.push(p.number);
                    }
                }
                3 => {
                    let from = ["a", "b", "c", "d"][below(4)];
                    log.push(format!("PCR of {from}"));
                    let mut expected = Vec::new();
                    let pids: HashSet<u16> = on_air
                        .iter()
                        .filter(|p| KINDS[p.kind].0.contains(&from))
                        .map(|p| KINDS[p.kind].1)
                        .collect();
                    pids.iter().for_each(|pid| rig.send(from, es_pcr(*pid, 0x0F)));
                    for p in on_air.iter_mut().filter(|p| p.on == from) {
                        expected.push((p.pids()[0].1, p.marked));
                        (p.held, p.marked) = (false, false);
                    }
                    let sent = rig.sent(expected.len()).await;
                    let mut got: Vec<_> = sent.iter().map(|(pid, p)| (*pid, p[5] & 0x80 != 0)).collect();
                    got.sort_unstable();
                    expected.sort_unstable();
                    assert_eq!(got, expected, "seed {seed}: {log:#?}\n{on_air:#?}");
                }
                4 if on_air.iter().any(|p| p.to.is_some()) => {
                    // The splice lands.
                    let p = on_air.iter_mut().find(|p| p.to.is_some()).unwrap();
                    let to = p.to.take().unwrap();
                    log.push(format!("the splice to {to} lands"));
                    for (src, _) in p.pids() {
                        rig.send(to, es_pes(src, 0x0F, 99_000));
                    }
                    p.on = to;
                    edited.push(p.number);
                    assert_eq!(rig.sent_pids(2).await, [0x210, 0x211], "seed {seed}: {log:#?}");
                }
                _ => {
                    let before = on_air.clone();
                    // Drop some, add some, edit or renumber some.
                    on_air.retain(|_| below(8) != 0);
                    let left: HashSet<usize> = before.iter().map(|p| p.kind).collect();
                    for kind in (0..KINDS.len()).filter(|kind| !left.contains(kind)) {
                        if below(3) == 0 || (on_air.is_empty() && kind == 4) {
                            next_number += 1;
                            on_air.push(OnAirProgram::fresh(kind, next_number));
                            edited.push(next_number);
                        }
                    }
                    if on_air.is_empty() || (before.len() == 1 && on_air.len() == 1) {
                        on_air = before.clone();
                        edited.clear();
                    }
                    for i in 0..on_air.len() {
                        let p = &mut on_air[i];
                        if edited.contains(&p.number) {
                            continue;
                        }
                        match below(12) {
                            0 if p.kind != 1 => {
                                p.extra = !p.extra;
                                edited.push(p.number);
                            }
                            1 => {
                                p.pmt_pid ^= 0x80;
                                edited.push(p.number);
                            }
                            2 => {
                                next_number += 1;
                                p.number = next_number;
                                edited.push(p.number);
                            }
                            3 if i > 0 => {
                                let other = std::mem::replace(&mut on_air[0].number, 0);
                                on_air[0].number = std::mem::replace(&mut on_air[i].number, other);
                                edited.extend([on_air[0].number, on_air[i].number]);
                            }
                            _ => {}
                        }
                    }
                    for i in (1..on_air.len()).rev() {
                        on_air.swap(i, below(i + 1));
                    }
                    let alone = |p: &&OnAirProgram| {
                        !edited.contains(&p.number)
                            && (p.held || p.to.is_some() || p.on != KINDS[p.kind].0[0])
                    };
                    kept += on_air.iter().filter(alone).count();
                    log.push(format!("swap to {on_air:?}"));
                    rig.replace(plan_on_air(&on_air)).await;
                }
            }

            let context = || format!("seed {seed}: {log:#?}\n{on_air:#?}");
            let pmt_pids: Vec<u16> = on_air.iter().map(|p| p.pmt_pid).collect();
            for (p, version) in on_air.iter().zip(rig.versions(&pmt_pids).await) {
                let last = [by_pid.insert(p.pmt_pid, version), by_number.insert(p.number, version)];
                for last in last.into_iter().flatten() {
                    // 17 where the number's last and the PID's are opposite.
                    let ahead = version.wrapping_sub(last) & 0x1F;
                    let right = if edited.contains(&p.number) { 1..18 } else { 0..1 };
                    assert!(right.contains(&ahead), "version {last} to {version} of {p:?}, {}", context());
                }
            }
            // A packet of every input of every slot: one on the leg a splice
            // is to is behind what it waits for.
            let mut expected = Vec::new();
            let mut probes: HashMap<(&str, u16), u64> = HashMap::new();
            for p in &on_air {
                for (src, out) in p.pids() {
                    for (i, input) in KINDS[p.kind].0.iter().enumerate() {
                        let plays = *input == p.on;
                        let pts = if plays { 90_000 } else { 10 } + i as u64;
                        // Programs on the same inputs read PIDs of their own.
                        probes.insert((input, src), pts);
                        if plays && !p.held && p.to.is_none() {
                            expected.push((out, pts));
                        }
                    }
                }
            }
            for ((input, src), pts) in probes {
                rig.send(input, es_pes(src, 0x0F, pts));
            }
            let sent = rig.sent(expected.len()).await;
            let mut got = stamps(&sent);
            got.sort_unstable();
            expected.sort_unstable();
            assert_eq!(got, expected, "{}", context());
            for (pid, p) in &sent {
                let cc = p[3] & 0x0F;
                if let Some(last) = ccs.insert(*pid, cc) {
                    // A PCR or the splice's own between two probes.
                    let ahead = cc.wrapping_sub(last) & 0x0F;
                    assert!((1..=2).contains(&ahead), "CC {last} to {cc} on 0x{pid:X}, {}", context());
                }
            }
        }
        kept
    }
}
