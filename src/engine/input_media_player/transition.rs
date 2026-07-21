// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Media-player transition state machine (Phase 3 of the redesign).
//!
//! This is the **pure-logic core** of the controller: it owns the state
//! transitions, the playback generation counter, and the operator-`Next`
//! idempotency contract, with **no I/O, no demuxer, no scheduler**. The
//! Phase 3 controller drives it; every rule here is unit-testable in
//! isolation. It is deliberately not yet wired into the live `run()` loop —
//! Phase 3b does that behind a default-off flag, with the legacy sequential
//! loop retained (plan §16, Phase 3 "Rollback").
//!
//! It encodes three things the plan is exact about:
//!
//! * **State graph** (§9.1) — the legal states and edges, including the
//!   `HoldingForNext` state added in the plan revision (§9.3) for when
//!   `Next` is pressed near EOS and the next source is not ready.
//! * **Generation semantics** (§9.4) — the generation counter increments
//!   exactly once, at `CommitBoundary`. A `Next` ACK reports the
//!   *pre-commit* generation, and a stale request is rejected with a
//!   conflict rather than skipping a second item.
//! * **Commit is the only mutation of active source/generation** — nothing
//!   else may change which source is on air.

// This module is the tested pure-logic core; the Phase 3b controller (which
// wires it into the live run() loop behind a default-off flag) is the
// consumer. Until that lands, the public surface is exercised only by the
// unit tests, so silence dead-code warnings for the module rather than the
// whole crate. Remove this attribute once the controller uses it.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// The player lifecycle states (plan §9.1). `PreparingNext` is modelled as a
/// boolean sub-state on top of `Playing` rather than its own variant, because
/// preparation runs *in parallel* with playout and must not be able to change
/// the active source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerState {
    Idle,
    PreparingCurrent,
    PrerollingCurrent,
    Playing,
    /// A transition has been requested/accepted and the machine is committed
    /// to crossing at the next safe boundary.
    TransitionArmed,
    /// Natural completion: the current source is draining its already-scheduled
    /// media to the cut boundary.
    DrainingCurrent,
    /// Operator `Next`: the current source is being cut at the next safe
    /// scheduler boundary, unscheduled units dropped.
    CuttingCurrent,
    /// Current source exhausted with a transition armed but next not ready —
    /// emitting hold filler rather than dead air (§9.3).
    HoldingForNext,
    /// The single instant at which the active source + generation flip.
    CommitBoundary,
    PlayingNext,
    /// Non-looping playlist reached its tail.
    Exhausted,
    /// Committed/armed transition missed its bounded progress deadline, or a
    /// source stalled.
    Stalled,
    /// Terminal-ish failure states.
    FailedCurrent,
    FailedNext,
}

/// What triggered a transition. All three converge on the same commit
/// operation (plan §9.2 / §9.3) — that convergence is the whole point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionTrigger {
    /// The current source reached natural end-of-file.
    NaturalEos,
    /// Loop wrap (last → first) on a looping playlist.
    Loop,
    /// Operator-issued `Next` — a cut, not a seek.
    OperatorNext,
}

/// A `Next` command from the manager (plan §9.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextRequest {
    /// The generation the caller believes is active. Makes retries and
    /// delayed double-clicks safe.
    pub expected_generation: u64,
    /// Idempotency key; a retry carries the same id.
    pub request_id: String,
}

/// The reply to a `Next` (plan §9.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum NextOutcome {
    /// Accepted and armed. Reports the *pre-commit* generation (the one still
    /// active at acceptance = the request's `expected_generation`). It is
    /// deliberately not the post-commit value; that appears only in the
    /// completion event/stats.
    Accepted {
        previous_index: usize,
        target_index: usize,
        accepted_generation: u64,
        transition_id: String,
    },
    /// A transition is already armed; the existing one stands. A retried
    /// duplicate click is indistinguishable from this.
    AlreadyPending { transition_id: String },
    /// The request's generation is behind the active one — the first click
    /// already committed and moved the generation on, so this is refused
    /// rather than skipping a second item.
    GenerationConflict { current_generation: u64 },
    /// Not in a state that can accept a `Next` (e.g. still preparing the
    /// first source, or already exhausted).
    Rejected { code: &'static str },
}

/// Rejection codes surfaced to the manager. Stable strings.
pub mod reject_code {
    pub const NOT_PLAYING: &str = "media_player_not_playing";
    pub const GENERATION_CONFLICT: &str = "media_player_generation_conflict";
}

/// The transition machine. Holds only the small amount of state needed to
/// enforce the plan's rules; the controller holds the heavy runtime objects.
#[derive(Debug, Clone)]
pub struct TransitionMachine {
    state: PlayerState,
    /// Increments exactly once per commit. Starts at 0; the first source that
    /// begins playing is generation 0.
    generation: u64,
    /// Index into the (possibly shuffled) play order of the on-air source.
    active_index: usize,
    /// The armed transition's target index and id, when a transition is armed.
    armed: Option<ArmedTransition>,
    /// Number of sources in the playlist (for wrap math and exhaustion).
    playlist_len: usize,
    loop_playback: bool,
    /// Whether the next source has reached readiness (set by the controller
    /// as preparation completes). Gates commit vs hold.
    next_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArmedTransition {
    trigger: TransitionTrigger,
    target_index: usize,
    transition_id: String,
}

impl TransitionMachine {
    pub fn new(playlist_len: usize, loop_playback: bool) -> Self {
        TransitionMachine {
            state: PlayerState::Idle,
            generation: 0,
            active_index: 0,
            armed: None,
            playlist_len,
            loop_playback,
            next_ready: false,
        }
    }

    pub fn state(&self) -> PlayerState {
        self.state
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn active_index(&self) -> usize {
        self.active_index
    }
    pub fn armed_target(&self) -> Option<usize> {
        self.armed.as_ref().map(|a| a.target_index)
    }
    pub fn is_transition_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// The controller reports the first source has begun emitting.
    pub fn begin_playing(&mut self) {
        self.state = PlayerState::Playing;
    }

    /// The controller updates next-source readiness as preparation proceeds.
    pub fn set_next_ready(&mut self, ready: bool) {
        self.next_ready = ready;
    }

    /// Index the machine would advance to on a natural/loop transition:
    /// `active + 1`, wrapping to 0 under loop. `None` when non-looping and
    /// already at the tail (→ exhaustion).
    fn natural_next_index(&self) -> Option<usize> {
        if self.playlist_len == 0 {
            return None;
        }
        let next = self.active_index + 1;
        if next < self.playlist_len {
            Some(next)
        } else if self.loop_playback {
            Some(0)
        } else {
            None
        }
    }

    /// Arm a transition triggered by natural EOS or loop wrap. Idempotent: if
    /// one is already armed, keeps it. Returns the target index, or `None`
    /// when the playlist is exhausted (non-looping tail) — the controller
    /// then moves to `Exhausted`.
    pub fn arm_natural(&mut self, trigger: TransitionTrigger, new_id: impl FnOnce() -> String) -> Option<usize> {
        if let Some(a) = &self.armed {
            return Some(a.target_index);
        }
        match self.natural_next_index() {
            Some(target) => {
                self.armed = Some(ArmedTransition { trigger, target_index: target, transition_id: new_id() });
                self.state = PlayerState::TransitionArmed;
                Some(target)
            }
            None => {
                self.state = PlayerState::Exhausted;
                None
            }
        }
    }

    /// Handle an operator `Next` (plan §9.4). Pure decision — the controller
    /// performs the I/O the outcome implies.
    pub fn request_next(&mut self, req: &NextRequest, new_id: impl FnOnce() -> String) -> NextOutcome {
        // Only meaningful while playing or already transitioning. Refuse in
        // Idle/Preparing/Exhausted/Failed. `CuttingCurrent` and `PlayingNext`
        // are included so a duplicate/retry click that lands after the first
        // Next has begun cutting (or just after a commit) still resolves to
        // AlreadyPending / a clean re-arm rather than a spurious rejection.
        if !matches!(
            self.state,
            PlayerState::Playing
                | PlayerState::PlayingNext
                | PlayerState::TransitionArmed
                | PlayerState::DrainingCurrent
                | PlayerState::CuttingCurrent
                | PlayerState::HoldingForNext
        ) {
            return NextOutcome::Rejected { code: reject_code::NOT_PLAYING };
        }

        // Stale generation → conflict. This is the delayed-double-click guard:
        // the first click committed and moved the generation on.
        if req.expected_generation < self.generation {
            return NextOutcome::GenerationConflict { current_generation: self.generation };
        }
        // A generation *ahead* of ours is nonsense (client bug / replay) —
        // treat as conflict rather than trust it.
        if req.expected_generation > self.generation {
            return NextOutcome::GenerationConflict { current_generation: self.generation };
        }

        // Already armed → the existing transition stands; a retry maps here.
        if let Some(a) = &self.armed {
            return NextOutcome::AlreadyPending { transition_id: a.transition_id.clone() };
        }

        // Fresh Next: arm a cut to the natural-next index. `Next` on a
        // non-looping tail still advances toward exhaustion — there is simply
        // nothing to cut to, so reject.
        let target = match self.natural_next_index() {
            Some(t) => t,
            None => return NextOutcome::Rejected { code: reject_code::NOT_PLAYING },
        };
        let transition_id = new_id();
        self.armed = Some(ArmedTransition {
            trigger: TransitionTrigger::OperatorNext,
            target_index: target,
            transition_id: transition_id.clone(),
        });
        // A Next is a cut: move toward CuttingCurrent. If the current source is
        // still producing media the controller drains to the next safe
        // boundary; readiness of `next` decides commit vs hold at that point.
        self.state = PlayerState::CuttingCurrent;
        NextOutcome::Accepted {
            previous_index: self.active_index,
            target_index: target,
            accepted_generation: self.generation,
            transition_id,
        }
    }

    /// The current source has run out of schedulable media while a transition
    /// is armed. If next is ready we can commit; otherwise we enter
    /// `HoldingForNext` (plan §9.3) — never dead air. Returns `true` when the
    /// caller should proceed to [`Self::commit`], `false` when it entered a
    /// hold and must keep the filler running.
    pub fn on_current_exhausted(&mut self) -> bool {
        if self.armed.is_none() {
            // Nothing armed and current ended: natural exhaustion path is via
            // arm_natural; if we got here with nothing armed, go Exhausted.
            self.state = PlayerState::Exhausted;
            return false;
        }
        if self.next_ready {
            true
        } else {
            self.state = PlayerState::HoldingForNext;
            false
        }
    }

    /// The next source became ready (e.g. while holding). Returns `true` when
    /// a commit should now happen (a transition is armed) — the controller
    /// calls [`Self::commit`] next.
    pub fn on_next_ready(&mut self) -> bool {
        self.next_ready = true;
        self.armed.is_some()
            && matches!(
                self.state,
                PlayerState::HoldingForNext
                    | PlayerState::CuttingCurrent
                    | PlayerState::DrainingCurrent
                    | PlayerState::TransitionArmed
            )
    }

    /// The hold deadline expired without next becoming ready (plan §9.3, step
    /// 4). Abandon the armed transition and fall back to normal progression:
    /// the controller emits `media_player_next_prepare_failed` and resumes.
    pub fn on_hold_deadline_expired(&mut self) {
        self.armed = None;
        self.next_ready = false;
        // Return to Playing if the current source can continue; the controller
        // decides. We model the machine side: no transition is armed anymore.
        self.state = PlayerState::Playing;
    }

    /// The single mutation point for active source + generation (plan §9.1:
    /// "Only `CommitBoundary` may change the active generation/source").
    /// Consumes the armed transition; increments the generation exactly once;
    /// moves the active index to the target. Returns the completed transition
    /// summary for the telemetry/event. Panics only on a controller bug
    /// (commit with nothing armed) — that is a logic error, not runtime input.
    pub fn commit(&mut self) -> CommitResult {
        let armed = self
            .armed
            .take()
            .expect("commit() called with no armed transition — controller bug");
        let previous_index = self.active_index;
        let previous_generation = self.generation;
        self.active_index = armed.target_index;
        self.generation += 1;
        self.next_ready = false;
        self.state = PlayerState::PlayingNext;
        CommitResult {
            trigger: armed.trigger,
            previous_index,
            new_index: self.active_index,
            previous_generation,
            new_generation: self.generation,
            transition_id: armed.transition_id,
        }
    }

    /// After the new source is emitting steadily the controller settles the
    /// machine back to the plain `Playing` state so the next transition can
    /// arm.
    pub fn settle_playing(&mut self) {
        if self.state == PlayerState::PlayingNext {
            self.state = PlayerState::Playing;
        }
    }
}

/// What a commit changed — fed into `media_player_transition_completed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitResult {
    pub trigger: TransitionTrigger,
    pub previous_index: usize,
    pub new_index: usize,
    pub previous_generation: u64,
    pub new_generation: u64,
    pub transition_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_gen() -> impl FnMut() -> String {
        let mut n = 0u64;
        move || {
            n += 1;
            format!("t{n}")
        }
    }

    fn playing(len: usize, looped: bool) -> TransitionMachine {
        let mut m = TransitionMachine::new(len, looped);
        m.begin_playing();
        m
    }

    #[test]
    fn generation_starts_zero_and_only_commit_increments_it() {
        let mut m = playing(3, true);
        assert_eq!(m.generation(), 0);
        let mut idg = id_gen();
        m.arm_natural(TransitionTrigger::NaturalEos, || idg());
        // Arming does NOT change generation.
        assert_eq!(m.generation(), 0);
        m.set_next_ready(true);
        assert!(m.on_current_exhausted());
        let r = m.commit();
        assert_eq!(r.previous_generation, 0);
        assert_eq!(r.new_generation, 1);
        assert_eq!(m.generation(), 1);
    }

    #[test]
    fn natural_advance_and_loop_wrap() {
        let mut m = playing(3, true);
        let mut idg = id_gen();
        assert_eq!(m.active_index(), 0);
        // 0 -> 1
        assert_eq!(m.arm_natural(TransitionTrigger::NaturalEos, || idg()), Some(1));
        m.set_next_ready(true);
        m.on_current_exhausted();
        m.commit();
        assert_eq!(m.active_index(), 1);
        m.settle_playing();
        // fast-forward to tail 2, then wrap to 0
        assert_eq!(m.arm_natural(TransitionTrigger::NaturalEos, || idg()), Some(2));
        m.set_next_ready(true);
        m.on_current_exhausted();
        m.commit();
        m.settle_playing();
        assert_eq!(m.active_index(), 2);
        assert_eq!(m.arm_natural(TransitionTrigger::Loop, || idg()), Some(0), "wrap to head");
    }

    #[test]
    fn non_looping_tail_exhausts() {
        let mut m = playing(2, false);
        let mut idg = id_gen();
        m.arm_natural(TransitionTrigger::NaturalEos, || idg());
        m.set_next_ready(true);
        m.on_current_exhausted();
        m.commit();
        m.settle_playing();
        assert_eq!(m.active_index(), 1);
        // At the tail, no more to advance to.
        assert_eq!(m.arm_natural(TransitionTrigger::NaturalEos, || idg()), None);
        assert_eq!(m.state(), PlayerState::Exhausted);
    }

    #[test]
    fn next_is_accepted_and_reports_precommit_generation() {
        let mut m = playing(3, true);
        let mut idg = id_gen();
        let out = m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        match out {
            NextOutcome::Accepted { previous_index, target_index, accepted_generation, .. } => {
                assert_eq!(previous_index, 0);
                assert_eq!(target_index, 1);
                assert_eq!(accepted_generation, 0, "ACK reports pre-commit generation");
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
        // Generation has NOT moved yet — only commit moves it.
        assert_eq!(m.generation(), 0);
    }

    #[test]
    fn duplicate_next_is_already_pending_not_double_skip() {
        let mut m = playing(4, true);
        let mut idg = id_gen();
        let first = m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        let tid = match first {
            NextOutcome::Accepted { transition_id, .. } => transition_id,
            o => panic!("{o:?}"),
        };
        // A retry / second click at the same generation must not arm a second
        // transition — it maps to the existing one.
        let second = m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        match second {
            NextOutcome::AlreadyPending { transition_id } => assert_eq!(transition_id, tid),
            o => panic!("expected AlreadyPending, got {o:?}"),
        }
        assert_eq!(m.armed_target(), Some(1), "still just one transition, to index 1");
    }

    #[test]
    fn stale_generation_next_is_conflict() {
        let mut m = playing(4, true);
        let mut idg = id_gen();
        // Commit once so generation becomes 1.
        m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        m.set_next_ready(true);
        assert!(m.on_current_exhausted());
        m.commit();
        m.settle_playing();
        assert_eq!(m.generation(), 1);
        // A delayed double-click carrying the old generation 0 is refused.
        let out = m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        match out {
            NextOutcome::GenerationConflict { current_generation } => assert_eq!(current_generation, 1),
            o => panic!("expected GenerationConflict, got {o:?}"),
        }
    }

    #[test]
    fn next_near_eos_with_unready_next_holds_not_dead_air() {
        let mut m = playing(3, true);
        let mut idg = id_gen();
        m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        // next is NOT ready and the current source runs out.
        m.set_next_ready(false);
        let can_commit = m.on_current_exhausted();
        assert!(!can_commit, "must not commit to an unready source");
        assert_eq!(m.state(), PlayerState::HoldingForNext);
        // When next becomes ready, we can commit.
        assert!(m.on_next_ready());
        let r = m.commit();
        assert_eq!(r.new_index, 1);
        assert_eq!(r.new_generation, 1);
    }

    #[test]
    fn hold_deadline_abandons_transition_and_resumes() {
        let mut m = playing(3, true);
        let mut idg = id_gen();
        m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        m.set_next_ready(false);
        m.on_current_exhausted();
        assert_eq!(m.state(), PlayerState::HoldingForNext);
        m.on_hold_deadline_expired();
        assert!(!m.is_transition_armed(), "armed transition abandoned");
        assert_eq!(m.state(), PlayerState::Playing);
        assert_eq!(m.generation(), 0, "no commit happened");
        assert_eq!(m.active_index(), 0);
    }

    #[test]
    fn next_rejected_when_not_playing() {
        let mut m = TransitionMachine::new(3, true); // still Idle
        let mut idg = id_gen();
        let out = m.request_next(&NextRequest { expected_generation: 0, request_id: "r1".into() }, || idg());
        assert!(matches!(out, NextOutcome::Rejected { code } if code == reject_code::NOT_PLAYING));
    }

    #[test]
    fn only_one_transition_arms_at_a_time() {
        let mut m = playing(5, true);
        let mut idg = id_gen();
        m.arm_natural(TransitionTrigger::NaturalEos, || idg());
        assert_eq!(m.armed_target(), Some(1));
        // A subsequent arm_natural keeps the first, does not stack.
        m.arm_natural(TransitionTrigger::NaturalEos, || idg());
        assert_eq!(m.armed_target(), Some(1));
    }
}
