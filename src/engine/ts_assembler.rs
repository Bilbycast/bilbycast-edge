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
//!   into a single `mpsc::Sender<(slot_idx, EsPacket)>`. Slow consumers
//!   see `broadcast::error::RecvError::Lagged(_)` and drop; nothing
//!   blocks the demuxer.
//! - Per-packet work: two header-byte edits + one CC increment. No PES
//!   reconstruction, no copies beyond `Bytes::copy_from_slice` of the
//!   188-byte payload into the bundle buffer.
//! - One allocation per emitted bundle (`BytesMut::with_capacity(1316)`
//!   swapped in after each flush) — bundles ship at ~(video packet rate
//!   / 7), not per-TS-packet.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use super::master_clock::ClockIdentity;
use super::packet::RtpPacket;
use super::ts_es_bus::{EsPacket, NodeEsBus};
use super::ts_parse::{
    extract_pcr, mpeg2_crc32, set_discontinuity_indicator, NULL_PID, PAT_PID, TS_PACKET_SIZE,
    TS_SYNC_BYTE,
};

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
    /// Concrete `(input_id, source_pid)` on the bus. For Switch slots
    /// this carries the **currently-active** leg — it mutates over time
    /// when the operator drives `ActivateInput`. For Pid / Essence /
    /// Hitless slots the value is fixed across the slot's lifetime.
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
    ///   supports neither fall through to the PmtBump path and emit
    ///   `pes_splice_degraded`.
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
}

/// Structured description of an assembly plan whose slots in one
/// program reference inputs with incompatible master clocks.
///
/// Built by [`check_assembly_clock_compatibility`]; consumed by
/// `flow.rs`'s `finalize_spts_assembler` to populate the structured
/// `details` block on the `pid_bus_master_clock_mismatch` event so
/// the manager UI can highlight the offending slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterClockMismatch {
    /// MPEG-TS program number containing the conflict (matches `ProgramPlan.program_number`).
    pub program_number: u16,
    /// `out_pid` of the slot that breaks the program's clock identity.
    pub slot_out_pid: u16,
    /// `input_id` referenced by the offending slot.
    pub slot_input_id: String,
    /// Human-readable label of the offending slot's clock
    /// (e.g. `"source_pcr:input=in-srt-a"`, `"ptp:domain=127"`).
    pub slot_clock: String,
    /// `input_id` of the slot whose clock the program adopted (the
    /// first slot walked in the program, used as the reference).
    pub reference_input_id: String,
    /// Human-readable label of the reference slot's clock.
    pub reference_clock: String,
}

/// Reject assembly plans that combine elementary streams from
/// inputs whose master clocks are not coherent.
///
/// **Why this exists.** When Phase 2 of the PES Switch redesign drops
/// the manager-side membership check (`assembly.input_ids ⊆
/// flow.input_ids`), assembly slots can pull from any input on the
/// node. Without this guard, an operator could route video from one
/// SRT encoder and audio from a *different* SRT encoder onto the
/// same output program. The two encoders run independent 27 MHz
/// oscillators — their PCRs drift apart at hundreds of ppm and the
/// downstream receiver loses lipsync within minutes. Mute beats
/// wrong-sync (per `feedback_no_pcr_restamp.md` + the master-clock
/// invariants in the PES Switch plan).
///
/// **What we check.** Each [`ProgramPlan`] in the resolved plan:
/// - The first slot's source `input_id` defines the program's
///   reference clock identity (looked up in `input_clocks`).
/// - Every subsequent slot's source — and **every leg** of a switch
///   slot's `switch_legs` — must report the same `ClockIdentity` from
///   the map. Mismatch → `Err(MasterClockMismatch { … })` describing
///   the first conflict found.
/// - Slots whose `input_id` is absent from `input_clocks` are
///   skipped (defensive). The caller should populate the map
///   eagerly from `ResolvedFlow::inputs`; missing entries indicate
///   a bug upstream in the resolve pipeline, not an operator error.
///
/// Cross-program clock differences are **allowed** — MPEG-TS
/// receivers re-acquire per program anyway, so an MPTS may carry
/// programs from different sources side-by-side.
///
/// This function is pure / does not allocate beyond the error path.
/// Today (with the membership check still in place) the check is a
/// no-op because every slot's input is in the flow's `input_ids`
/// and the single-flow runtime has a single master clock by
/// construction. Phase 2.1 makes it load-bearing.
pub fn check_assembly_clock_compatibility(
    plan: &AssemblyPlan,
    input_clocks: &HashMap<String, ClockIdentity>,
) -> Result<(), MasterClockMismatch> {
    for program in &plan.programs {
        // Collect (input_id, out_pid) for every source the program
        // references — the primary slot source plus every switch leg.
        // The walk preserves config order so the first slot's clock
        // becomes the program reference. This matches operator intent
        // because the first slot is conventionally the video PID, and
        // a video clock mismatch is the most user-visible failure
        // mode (audio drifts before video does, but video locks
        // first).
        let mut reference: Option<(String, ClockIdentity)> = None;
        for slot in &program.slots {
            // Direct source.
            check_one_source(
                program.program_number,
                slot.out_pid,
                &slot.source.0,
                input_clocks,
                &mut reference,
            )?;
            // Every switch leg.
            if let Some(legs) = &slot.switch_legs {
                for leg in legs {
                    check_one_source(
                        program.program_number,
                        slot.out_pid,
                        &leg.0,
                        input_clocks,
                        &mut reference,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn check_one_source(
    program_number: u16,
    slot_out_pid: u16,
    input_id: &str,
    input_clocks: &HashMap<String, ClockIdentity>,
    reference: &mut Option<(String, ClockIdentity)>,
) -> Result<(), MasterClockMismatch> {
    let Some(clock) = input_clocks.get(input_id) else {
        // Defensive: missing entries indicate the caller didn't
        // populate the map for this input. Treat as "unknown but
        // not provably incompatible" and skip — the upstream essence
        // resolver will have already failed on a truly unknown input.
        return Ok(());
    };
    match reference.as_ref() {
        None => {
            *reference = Some((input_id.to_string(), clock.clone()));
            Ok(())
        }
        Some((_ref_input, ref_clock)) if ref_clock == clock => Ok(()),
        Some((ref_input, ref_clock)) => Err(MasterClockMismatch {
            program_number,
            slot_out_pid,
            slot_input_id: input_id.to_string(),
            slot_clock: clock.label(),
            reference_input_id: ref_input.clone(),
            reference_clock: ref_clock.label(),
        }),
    }
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
// (empty in tests).
pub fn spawn_spts_assembler(
    plan: AssemblyPlan,
    bus: Arc<NodeEsBus>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
    event_sender: Option<crate::manager::events::EventSender>,
    flow_id: String,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
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
            run_assembler(plan, bus, broadcast_tx, cancel, plan_rx, event_sender, flow_id, av_sync_pacer).await;
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
/// `program_idx` is retained for per-PID switching so fan-in traffic
/// routes to the right program's PMT without re-searching.
#[derive(Debug, Clone)]
struct FlatSlot {
    program_idx: usize,
    source: (String, u16),
    out_pid: u16,
    /// `true` iff this FlatSlot is one leg of a [`AssemblySlot::switch_legs`]
    /// group. The main loop drops fan-in packets whose `source.0`
    /// (this leg's input_id) doesn't match
    /// `active_leg_input.get(&(program_idx, out_pid))`. Non-switch slots
    /// have this `false` and forward unconditionally.
    is_switch_leg: bool,
    /// MPEG-TS `stream_type` of the parent [`AssemblySlot`]. Carried on
    /// the flattened view so the PES Switch Phase 4 codec-param
    /// sentinel can short-circuit the AAC ADTS-only path without a
    /// PSI lookup on every fan-in packet.
    stream_type: u8,
}

async fn run_assembler(
    mut plan: AssemblyPlan,
    bus: Arc<NodeEsBus>,
    broadcast_tx: broadcast::Sender<RtpPacket>,
    cancel: CancellationToken,
    mut plan_rx: mpsc::Receiver<PlanCommand>,
    event_sender: Option<crate::manager::events::EventSender>,
    flow_id: String,
    av_sync_pacer: Option<Arc<crate::engine::av_sync_mux::AvSyncPacer>>,
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
    // PES Switch Phase 4 — per-switch-slot splice state, keyed by
    // `(program_idx, out_pid)` to match the `active_leg_input` map.
    // Default `Idle`; flipped to `Pending` on `SwitchActiveInput` with
    // splice_mode = PesAligned, reset to `Idle` on commit or timeout.
    let mut splice_state: std::collections::HashMap<(usize, u16), AudioSpliceState> =
        std::collections::HashMap::new();
    // Last PUSI=1 PTS observed on each (program_idx, out_pid) — needed
    // to arm the splice machine at SwitchActiveInput time. Updated on
    // every active-leg PUSI=1 packet; cheap (one HashMap insert per AU,
    // hot path stays clean).
    let mut last_a_pts: std::collections::HashMap<(usize, u16), u64> =
        std::collections::HashMap::new();
    // Most recent AAC AudioSpecificConfig observed on the active leg of
    // each (program_idx, out_pid) — feeds the codec-param sentinel at
    // arm time. Only populated for AAC ADTS (stream_type 0x0F) slots;
    // every other audio codec leaves this `None`.
    let mut last_a_aac_params: std::collections::HashMap<(usize, u16), AacAudioParams> =
        std::collections::HashMap::new();
    // PES Switch Phase 4 (edge 0.66.0) — per-switch-slot video splice
    // state, keyed by `(program_idx, out_pid)`. Disjoint from
    // `splice_state` by construction: each slot's `stream_type` is
    // either an audio codec (audio state map) or a supported video
    // codec (this map), never both.
    let mut video_splice_state: std::collections::HashMap<(usize, u16), VideoSpliceState> =
        std::collections::HashMap::new();
    // PES Switch Phase 4 Session B — most recent SPS-derived parameter
    // snapshot observed on the active leg of each `(program_idx,
    // out_pid)` for video slots. Refreshed on every active-leg PUSI=1
    // IDR PES that carries a parseable SPS. Feeds the codec-param
    // sentinel at splice arm time so the state machine can compare
    // A's snapshot against B's first IDR.
    let mut last_a_video_params: std::collections::HashMap<(usize, u16), VideoCodecParams> =
        std::collections::HashMap::new();
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

    // Per-out_pid one-shot DI arming. After every `ReplacePlan` the
    // map is populated for every out_pid in the new plan; after every
    // `SwitchActiveInput` only the affected switch slots' out_pids are
    // armed. The flag is consumed when the next PCR-bearing TS packet
    // flows through the ES rewrite path on that out_pid; the assembler
    // sets `discontinuity_indicator` (AF flags 0x80) so receivers (VLC,
    // ffplay, broadcast-grade hardware) re-anchor STC instead of treating
    // the post-swap PCR-epoch jump as a clock fault. Per-out_pid keying
    // means a switch on slot X doesn't false-trigger DI on a sibling
    // slot Y. Mirror of `TsContinuityFixer::pending_di_on_pcr` for the
    // PID-bus / Flow Assembly path.
    let mut pending_di_for_out_pid: std::collections::HashMap<u16, bool> =
        std::collections::HashMap::new();

    // Per-switch-slot active-leg pointer, keyed by `(program_idx, out_pid)`.
    // Populated on `install_plan` and `apply_plan_replacement` from each
    // switch slot's initial `source.0`; mutated by `SwitchActiveInput`.
    // Fan-in packets from non-active legs are dropped at the main-loop
    // edge (cheap string compare, no rewrite).
    let mut active_leg_input: std::collections::HashMap<(usize, u16), String> =
        std::collections::HashMap::new();

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
        &mut active_leg_input,
        &event_sender,
        &flow_id,
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

        pts_rewriter.as_mut(),
    );

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                flush(&mut buf, &broadcast_tx, &mut bundle_seq, pts_rewriter.as_mut());
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
                            &mut active_leg_input,
                            &event_sender,
                            &flow_id,
                        );
                        // Arm DI on every out_pid — the next PCR-bearing
                        // TS packet on each will carry
                        // `discontinuity_indicator = 1`. Without this,
                        // receivers see the source-change PCR epoch jump
                        // as a clock fault and freeze on their last
                        // decoded frame even though PMT version bumped.
                        pending_di_for_out_pid.clear();
                        for prog in &plan.programs {
                            for slot in &prog.slots {
                                pending_di_for_out_pid.insert(slot.out_pid, true);
                            }
                        }
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

                            pts_rewriter.as_mut(),
                        );
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
                                let key = (pidx, slot.out_pid);
                                let prev = active_leg_input.get(&key).cloned();
                                if prev.as_deref() == Some(new_input_id.as_str()) {
                                    continue; // already active
                                }

                                // Decide splice path. PesAligned needs:
                                //  (1) effective splice_mode == PesAligned,
                                //  (2) slot is a known audio OR video codec,
                                //  (3) we've seen at least one PUSI=1 PTS
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
                                    && is_supported_audio_stream_type(slot.stream_type);
                                let want_video = pes_aligned
                                    && is_supported_video_stream_type(slot.stream_type);
                                if want_audio {
                                    if let Some(&last_pts) = last_a_pts.get(&key) {
                                        let budget_ms = slot
                                            .splice_budget_ms
                                            .unwrap_or(DEFAULT_AUDIO_SPLICE_BUDGET_MS);
                                        let state = splice_state
                                            .entry(key)
                                            .or_insert_with(AudioSpliceState::new);
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
                                } else if want_video {
                                    if let Some(&last_pts) = last_a_pts.get(&key) {
                                        let budget_ms = slot
                                            .splice_budget_ms
                                            .unwrap_or(DEFAULT_VIDEO_SPLICE_BUDGET_MS);
                                        let state = video_splice_state
                                            .entry(key)
                                            .or_insert_with(VideoSpliceState::new);
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
                                }

                                // PES-aligned was requested (override or
                                // slot config) but we reached the PmtBump
                                // fall-through — emit a degrade Warning so
                                // the operator isn't left thinking the cut
                                // was glitchless. Reasons: the slot's codec
                                // is not a supported PES-splice type, or no
                                // active-leg PTS reference has been observed
                                // yet (first switch on a freshly-armed flow).
                                // Budget-exhaustion degrades emit
                                // `pes_splice_timeout` separately on the
                                // flush tick.
                                if pes_aligned {
                                    if let Some(es_) = &event_sender {
                                        let reason = if want_audio || want_video {
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
                                }

                                // PmtBump path (today's behaviour).
                                active_leg_input.insert(key, new_input_id.clone());
                                if let Some(v) = pmt_versions.get_mut(pidx) {
                                    *v = v.wrapping_add(1) & 0x1F;
                                }
                                pending_di_for_out_pid.insert(slot.out_pid, true);
                                any_flipped = true;
                            }
                        }
                        if any_flipped {
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
                                &mut pmt_cc,
                                &broadcast_tx,
                                &mut bundle_seq,

                                pts_rewriter.as_mut(),
                            );
                        }
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
                    let key = (slot.program_idx, slot.out_pid);
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
                            if matches!(slot.stream_type, 0x0F | 0x11) {
                                if let Some(p) = extract_aac_params_from_pes(
                                    &es.payload,
                                    slot.stream_type,
                                ) {
                                    last_a_aac_params.insert(key, p);
                                }
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
                            if is_supported_video_stream_type(slot.stream_type) {
                                if let Some(p) = extract_video_params_from_pes(
                                    &es.payload,
                                    slot.stream_type,
                                ) {
                                    last_a_video_params.insert(key, p);
                                }
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
                            {
                                if let Some(s) = splice_state.get_mut(&key) {
                                    s.record_a_audio_params(&es.payload);
                                }
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
                            if crate::engine::ts_parse::ts_pusi(&es.payload) {
                                if let Some(s) = video_splice_state.get_mut(&key) {
                                    s.record_a_video_params(&es.payload);
                                }
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
                                if let Some(v) = pmt_versions.get_mut(key.0) {
                                    *v = v.wrapping_add(1) & 0x1F;
                                }
                                pending_di_for_out_pid.insert(slot.out_pid, true);
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

                                    pts_rewriter.as_mut(),
                                );
                                if let Some(es_) = &event_sender {
                                    es_.emit_flow_with_details(
                                        crate::manager::events::EventSeverity::Info,
                                        crate::manager::events::category::FLOW,
                                        format!(
                                            "Flow '{flow_id}': PES-aligned video splice committed on \
                                             program {} out_pid 0x{:04X} → '{}' (IDR @ first_b_pts={first_b_pts})",
                                            plan.programs[key.0].program_number,
                                            key.1,
                                            slot.source.0,
                                        ),
                                        &flow_id,
                                        serde_json::json!({
                                            "error_code": "pes_splice_completed",
                                            "kind": "video",
                                            "program_number": plan.programs[key.0].program_number,
                                            "out_pid": key.1,
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
                                if let Some(v) = pmt_versions.get_mut(key.0) {
                                    *v = v.wrapping_add(1) & 0x1F;
                                }
                                pending_di_for_out_pid.insert(slot.out_pid, true);
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
                                            plan.programs[key.0].program_number,
                                            key.1,
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
                                            "program_number": plan.programs[key.0].program_number,
                                            "out_pid": key.1,
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
                                if let Some(v) = pmt_versions.get_mut(key.0) {
                                    *v = v.wrapping_add(1) & 0x1F;
                                }
                                pending_di_for_out_pid.insert(slot.out_pid, true);
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

                                    pts_rewriter.as_mut(),
                                );
                                if let Some(es_) = &event_sender {
                                    es_.emit_flow_with_details(
                                        crate::manager::events::EventSeverity::Info,
                                        crate::manager::events::category::FLOW,
                                        format!(
                                            "Flow '{flow_id}': PES-aligned splice committed on program \
                                             {} out_pid 0x{:04X} → '{}' (first_b_pts={first_b_pts})",
                                            plan.programs[key.0].program_number,
                                            key.1,
                                            slot.source.0,
                                        ),
                                        &flow_id,
                                        serde_json::json!({
                                            "error_code": "pes_splice_completed",
                                            "program_number": plan.programs[key.0].program_number,
                                            "out_pid": key.1,
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
                                if matches!(slot.stream_type, 0x0F | 0x11) {
                                    if let Some(p) = extract_aac_params_from_pes(
                                        &es.payload,
                                        slot.stream_type,
                                    ) {
                                        last_a_aac_params.insert(key, p);
                                    }
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
                                if let Some(v) = pmt_versions.get_mut(key.0) {
                                    *v = v.wrapping_add(1) & 0x1F;
                                }
                                pending_di_for_out_pid.insert(slot.out_pid, true);
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
                                            plan.programs[key.0].program_number,
                                            key.1,
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
                                            "program_number": plan.programs[key.0].program_number,
                                            "out_pid": key.1,
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
                // (key, to_input_id, kind) tuples. `kind` is folded
                // into the emitted event so the manager UI can tell
                // audio splice timeouts apart from video splice
                // timeouts (different operator action — video timeouts
                // usually mean the encoder's GoP exceeds the budget).
                let mut timeouts: Vec<((usize, u16), String, &'static str)> = Vec::new();
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
                    for (key, to_input_id, kind) in &timeouts {
                        let (pidx, out_pid) = *key;
                        active_leg_input.insert(*key, to_input_id.clone());
                        if let Some(v) = pmt_versions.get_mut(pidx) {
                            *v = v.wrapping_add(1) & 0x1F;
                        }
                        pending_di_for_out_pid.insert(out_pid, true);
                        any_flipped = true;
                        if let Some(es_) = &event_sender {
                            es_.emit_flow_with_details(
                                crate::manager::events::EventSeverity::Warning,
                                crate::manager::events::category::FLOW,
                                format!(
                                    "Flow '{flow_id}': PES-aligned {kind} splice timed out on program \
                                     {} out_pid 0x{:04X} → '{to_input_id}'; falling back to PMT-bump",
                                    plan.programs[pidx].program_number,
                                    out_pid,
                                ),
                                &flow_id,
                                serde_json::json!({
                                    "error_code": "pes_splice_timeout",
                                    "kind": kind,
                                    "program_number": plan.programs[pidx].program_number,
                                    "out_pid": out_pid,
                                    "to_input_id": to_input_id,
                                }),
                            );
                        }
                    }
                    if any_flipped {
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

                            pts_rewriter.as_mut(),
                        );
                    }
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

/// (Re-)flatten the program×slot matrix into `flat`, spawn fan-ins for
/// any slot that isn't already represented by a live task, and recompute
/// `pcr_out_pid_by_program`. Call once at startup; subsequent plan
/// changes go through [`apply_plan_replacement`] which handles diff +
/// fan-in reuse.
#[allow(clippy::too_many_arguments)]
fn install_plan(
    plan: &AssemblyPlan,
    bus: &Arc<NodeEsBus>,
    fanin_tx: &mpsc::Sender<(usize, EsPacket)>,
    cancel: &CancellationToken,
    flat: &mut Vec<FlatSlot>,
    slot_cancels: &mut Vec<CancellationToken>,
    slot_tasks: &mut Vec<JoinHandle<()>>,
    pcr_out_pid_by_program: &mut Vec<u16>,
    active_leg_input: &mut std::collections::HashMap<(usize, u16), String>,
    event_sender: &Option<crate::manager::events::EventSender>,
    flow_id: &str,
) {
    for (pidx, prog) in plan.programs.iter().enumerate() {
        for s in prog.slots.iter() {
            // Spawn one fan-in per bus subscription. Pid / Essence /
            // Hitless slots produce one. Switch slots produce one per
            // leg, all sharing `(pidx, out_pid)` so the main loop can
            // gate on a single (program_idx, out_pid) → active_input
            // lookup and drop non-active-leg packets at the edge.
            let leg_pairs: Vec<(String, u16)> = match &s.switch_legs {
                Some(legs) => legs.clone(),
                None => vec![s.source.clone()],
            };
            let is_switch = s.switch_legs.is_some();
            for src in leg_pairs {
                let slot = FlatSlot {
                    program_idx: pidx,
                    source: src.clone(),
                    out_pid: s.out_pid,
                    is_switch_leg: is_switch,
                    stream_type: s.stream_type,
                };
                let idx = flat.len();
                flat.push(slot.clone());
                let slot_cancel = cancel.child_token();
                let rx = bus.subscribe(&slot.source.0, slot.source.1);
                let tx = fanin_tx.clone();
                slot_cancels.push(slot_cancel.clone());
                let identity = SlotIdentity {
                    program_number: prog.program_number,
                    out_pid: slot.out_pid,
                    source_input_id: slot.source.0.clone(),
                    source_pid: slot.source.1,
                };
                slot_tasks.push(tokio::spawn(slot_fanin(
                    idx,
                    rx,
                    tx,
                    slot_cancel,
                    identity,
                    event_sender.clone(),
                    flow_id.to_string(),
                )));
            }
            if is_switch {
                // Initial active leg = AssemblySlot.source — populated
                // by build_assembly_plan from the config's
                // initial_input_id (or flow.active_input_id on restart).
                active_leg_input.insert((pidx, s.out_pid), s.source.0.clone());
            }
        }
    }
    *pcr_out_pid_by_program = plan
        .programs
        .iter()
        .map(|prog| {
            prog.slots
                .iter()
                .find(|s| {
                    s.source == prog.pcr_source
                        || s.switch_legs
                            .as_ref()
                            .is_some_and(|legs| legs.iter().any(|p| p == &prog.pcr_source))
                })
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
#[allow(clippy::too_many_arguments)]
fn apply_plan_replacement(
    plan: &mut AssemblyPlan,
    new_plan: AssemblyPlan,
    bus: &Arc<NodeEsBus>,
    fanin_tx: &mpsc::Sender<(usize, EsPacket)>,
    cancel: &CancellationToken,
    flat: &mut Vec<FlatSlot>,
    slot_cancels: &mut Vec<CancellationToken>,
    slot_tasks: &mut Vec<JoinHandle<()>>,
    pcr_out_pid_by_program: &mut Vec<u16>,
    pat_version: &mut u8,
    pmt_versions: &mut Vec<u8>,
    active_leg_input: &mut std::collections::HashMap<(usize, u16), String>,
    event_sender: &Option<crate::manager::events::EventSender>,
    flow_id: &str,
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

    // Refresh active-leg pointers on swap. Drop entries for switch
    // slots that no longer exist; preserve entries for slots that
    // survive (the operator's prior switch sticks across hot-swap).
    let mut new_active_leg_input: std::collections::HashMap<(usize, u16), String> =
        std::collections::HashMap::new();

    for new_prog in new_plan.programs.iter().enumerate().map(|(pidx, p)| (pidx, p)) {
        let (pidx, prog) = new_prog;
        for s in prog.slots.iter() {
            let leg_pairs: Vec<(String, u16)> = match &s.switch_legs {
                Some(legs) => legs.clone(),
                None => vec![s.source.clone()],
            };
            let is_switch = s.switch_legs.is_some();
            for src in &leg_pairs {
                // Look for a reusable fan-in in the current flat list
                // for this exact (source, ...) tuple. Skip cancelled
                // tombstones — see original comment.
                let match_idx = flat.iter().enumerate().find_map(|(i, f)| {
                    if !reused[i]
                        && f.source == *src
                        && slot_cancels.get(i).map_or(true, |c| !c.is_cancelled())
                    {
                        Some(i)
                    } else {
                        None
                    }
                });
                match match_idx {
                    Some(i) => {
                        reused[i] = true;
                        flat[i].out_pid = s.out_pid;
                        flat[i].program_idx = pidx;
                        flat[i].is_switch_leg = is_switch;
                        flat[i].stream_type = s.stream_type;
                    }
                    None => {
                        let slot = FlatSlot {
                            program_idx: pidx,
                            source: src.clone(),
                            out_pid: s.out_pid,
                            is_switch_leg: is_switch,
                            stream_type: s.stream_type,
                        };
                        let idx = flat.len();
                        flat.push(slot.clone());
                        reused.push(true);
                        let slot_cancel = cancel.child_token();
                        let rx = bus.subscribe(&slot.source.0, slot.source.1);
                        let tx = fanin_tx.clone();
                        slot_cancels.push(slot_cancel.clone());
                        let identity = SlotIdentity {
                            program_number: prog.program_number,
                            out_pid: slot.out_pid,
                            source_input_id: slot.source.0.clone(),
                            source_pid: slot.source.1,
                        };
                        slot_tasks.push(tokio::spawn(slot_fanin(
                            idx,
                            rx,
                            tx,
                            slot_cancel,
                            identity,
                            event_sender.clone(),
                            flow_id.to_string(),
                        )));
                    }
                }
            }
            if is_switch {
                // Carry the operator's prior choice if the new slot
                // still has it as a leg; otherwise fall back to the
                // new plan's `source.0` (= initial_input_id).
                let key = (pidx, s.out_pid);
                let prior = active_leg_input
                    .get(&key)
                    .filter(|prev| leg_pairs.iter().any(|(iid, _)| iid == prev.as_str()))
                    .cloned()
                    .unwrap_or_else(|| s.source.0.clone());
                new_active_leg_input.insert(key, prior);
            }
        }
    }

    *active_leg_input = new_active_leg_input;

    // Cancel any fan-in that wasn't reused. Leave its entry in `flat`
    // as a tombstone — the main loop's `flat.get(idx)` still returns
    // the old metadata, but since the fan-in is cancelled no further
    // packets arrive under that index.
    for (i, was_reused) in reused.iter().enumerate().take(slot_cancels.len()) {
        if !was_reused {
            slot_cancels[i].cancel();
        }
    }

    // Recompute PCR resolution for each program. Match either the
    // active-leg `source` (Pid/Essence/Hitless or current Switch) or
    // any leg in `switch_legs` — switch slots' active leg flips at
    // runtime but the PCR still rides the slot's fixed out_pid.
    *pcr_out_pid_by_program = new_plan
        .programs
        .iter()
        .map(|prog| {
            prog.slots
                .iter()
                .find(|s| {
                    s.source == prog.pcr_source
                        || s.switch_legs
                            .as_ref()
                            .is_some_and(|legs| legs.iter().any(|p| p == &prog.pcr_source))
                })
                .map(|s| s.out_pid)
                .unwrap_or(prog.pmt_pid)
        })
        .collect();

    *plan = new_plan;
    *pmt_versions = new_pmt_versions;

    // Silence unused-var warnings from the earlier parallel-map stubs.
    let _ = (&mut new_flat, &mut carry_tasks, &mut carry_cancels);
}

/// Identity carried into [`slot_fanin`] so it can emit a structured
/// `pid_bus_slot_source_closed` event when the upstream ES bus channel
/// closes while the parent assembler is still running. Cheap to clone
/// (one String + four scalars).
#[derive(Debug, Clone)]
struct SlotIdentity {
    program_number: u16,
    out_pid: u16,
    source_input_id: String,
    source_pid: u16,
}

async fn slot_fanin(
    slot_idx: usize,
    mut rx: broadcast::Receiver<EsPacket>,
    tx: mpsc::Sender<(usize, EsPacket)>,
    cancel: CancellationToken,
    identity: SlotIdentity,
    event_sender: Option<crate::manager::events::EventSender>,
    flow_id: String,
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
                Err(broadcast::error::RecvError::Closed) => {
                    // Upstream publisher dropped — the input task that
                    // owns this `(input_id, source_pid)` channel exited.
                    // Most commonly the owning flow was stopped while
                    // sibling flows still reference its inputs via
                    // assembly slots (the "input-host flow" pattern). The
                    // bus channel re-arms automatically on owner restart,
                    // but the operator needs to see why output went
                    // silent. The `cancel.cancelled()` arm above wins
                    // when the assembler is being torn down (parent flow
                    // stop / hot-swap / edge shutdown), so this emit only
                    // fires on truly unexpected closes.
                    if let Some(es) = &event_sender {
                        es.emit_flow_with_details(
                            crate::manager::events::EventSeverity::Warning,
                            crate::manager::events::category::FLOW,
                            format!(
                                "Flow '{flow_id}': assembly slot source disappeared — \
                                 input '{}' source_pid 0x{:04X} (program {} out_pid 0x{:04X}). \
                                 Most likely the owning flow stopped; restart it to recover.",
                                identity.source_input_id,
                                identity.source_pid,
                                identity.program_number,
                                identity.out_pid,
                            ),
                            &flow_id,
                            serde_json::json!({
                                "error_code": "pid_bus_slot_source_closed",
                                "program_number": identity.program_number,
                                "out_pid": identity.out_pid,
                                "source_input_id": identity.source_input_id,
                                "source_pid": identity.source_pid,
                            }),
                        );
                    }
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
    /// operator hasn't set it. Ignored when `seq_aware` is `true`.
    pub stall_ms: u64,
    /// When `true`, the merger uses true SMPTE 2022-7 seq-aware
    /// dedup + gap-fill (driven by `EsPacket.upstream_seq`) instead of
    /// stall-timer-based primary preference.
    pub seq_aware: bool,
    /// Reorder window for the seq-aware merger (multiple of 64 in
    /// 64..=4096). Ignored when `seq_aware` is `false`.
    pub reorder_window: u16,
    /// Path-differential / skew-accommodation buffer in milliseconds.
    /// `Some(ms)` enables the buffered SMPTE 2022-7 merger (industry
    /// standard — emits at constant `ms` latency, hitlessly fills loss
    /// within the window). `None` falls back to the stateless dedup-
    /// only path.
    pub path_differential_ms: Option<u32>,
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

    /// Build an AF-only source packet (PCR-bearing, no PES payload).
    /// AFC = `0b10`. Used to exercise the H.222.0 §2.4.3.3 rule that
    /// AF-only packets MUST NOT advance the continuity counter — they
    /// carry the previously-issued payload's CC value verbatim.
    fn ts_af_only(src_pid: u16) -> Bytes {
        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;
        pkt[1] = (((src_pid >> 8) as u8) & 0x1F);
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
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None).join;

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
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None).join;

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

    #[tokio::test]
    async fn assembler_mpts_emits_pat_with_two_programs_and_two_pmts() {
        let bus = Arc::new(NodeEsBus::new());
        let (tx, mut rx) = broadcast::channel::<RtpPacket>(32);
        let cancel = CancellationToken::new();
        let handle = spawn_spts_assembler(make_mpts_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None).join;

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
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None).join;
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
        let handle = spawn_spts_assembler(initial, bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None);

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
        let handle = spawn_spts_assembler(make_plan(), bus.clone(), tx.clone(), cancel.clone(), None, String::new(), None);

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

    #[test]
    fn clock_compat_walks_switch_legs() {
        // A Switch slot whose legs span multiple inputs must have
        // every leg on the same clock as the program's reference.
        let mut plan = make_plan();
        // Replace slot 1 with a switch slot whose backup leg lives on
        // a different SourcePcr — that's a sneaky failure mode the
        // resolver could miss because the active leg looks fine.
        plan.programs[0].slots[1] = AssemblySlot {
            source: ("in-a".to_string(), 0x200),
            out_pid: 0x201,
            stream_type: 0x0F,
            switch_legs: Some(vec![
                ("in-a".to_string(), 0x200),
                ("in-c".to_string(), 0x200),
            ]),
            splice_mode: Default::default(),
            splice_budget_ms: None,
        };
        let map = clocks(&[
            ("in-a", ClockIdentity::SourcePcr { input_id: "in-a".into() }),
            ("in-c", ClockIdentity::SourcePcr { input_id: "in-c".into() }),
        ]);
        let err = check_assembly_clock_compatibility(&plan, &map)
            .expect_err("switch leg on a different clock must fail");
        assert_eq!(err.slot_input_id, "in-c");
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
}
