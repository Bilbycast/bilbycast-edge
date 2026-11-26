// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Where a native-UDP relay tunnel, or a relayed bond leg, meets its peer: the
//! rule, with no I/O and the time passed in, like [`super::relay_client::OtherRelay`]
//! and [`super::relay_client::Walk`].
//!
//! A tunnel with two relays may hold a socket on each (relay `0` the primary,
//! `1` the backup). It sends its media on one, the *active* relay, and reads
//! both. Each socket registers on its relay, and a relay after v0.20.0 answers
//! every Register with whether the other side of the tunnel has been heard
//! there in the last 16 s ([`RELAY_PEER_LIVE_MS`]) and for how long both have
//! been, without a break. From that the rule:
//!
//! - **follows the peer** (R1): when the active relay is in trouble — the peer
//!   is not heard there, or the relay itself is not — for [`NativeTiming::settle`],
//!   and both sides are heard on the other relay, it moves there;
//! - **fails back with it** (R2): on the backup it stands by on the primary,
//!   and once both sides have been heard there for [`NativeTiming::steady`], and
//!   the primary is not more than the gate slower, it moves back. Its peer sees
//!   the same relay say the same thing and moves too, and the crossing loses
//!   nothing, because each edge still reads the socket it moved from;
//! - opens the other socket only while it has reason to (R3), closes it once it
//!   has none (R4), and keeps one socket on a relay that turns out to be the
//!   same relay at a second address (R5) for as long as the address it keeps
//!   answers;
//! - keeps today's rules with an older relay (R6, R7): it moves only once its
//!   relay has been silent for [`RELAY_DEAD_TIMEOUT_MS`] and never fails back,
//!   nor follows the peer. So an outage of that long or more on one side's
//!   path leaves the pair on two relays for good about half the time: the
//!   side rotates while unheard and stays where it is when its path is back,
//!   and a path back within the last keepalive before a rotation is due races
//!   the Register due at the same moment (`NativeMeet::dead_active`).
//!
//! A refused datagram (the relay process is down, so its host answers ICMP)
//! moves nothing: it only speeds up the Registers, so a side whose media or
//! Register was refused while the relay was down finds it again within a
//! second of its return. A side that was refused nothing (no media, its
//! keepalive stepping over the restart) finds it at its next keepalive, up to
//! 5 s after; the media is back within the restart and 6 s either way
//! (`native_meet_sim`). An answer that is a Register and its fast retry late
//! is asked for again every [`NativeTiming::fast`], so one or two lost
//! Registers or Acks never let a relay count as silent.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use super::relay_client::RelayTunnelState;
use super::udp_relay_client::{KEEPALIVE_INTERVAL, RELAY_DEAD_TIMEOUT_MS, SLOT_TAKEOVER_GRACE_MS};

/// How long a relay after v0.20.0 counts a side as heard: the edge half of
/// bilbycast-relay `udp_relay::PEER_LIVE_MS`, which carries the same value.
pub(crate) const RELAY_PEER_LIVE_MS: u64 = 16_000;

const KEEPALIVE_MS: u64 = 5_000;
const FAST_MS: u64 = 1_000;
const FAST_FOR_MS: u64 = 60_000;
const SILENT_MS: u64 = 10_000;
const WAITING_MS: u64 = 10_000;
const SETTLE_MS: u64 = 8_000;
const STEADY_MS: u64 = 60_000;
const LINGER_MS: u64 = 80_000;
const MOVE_QUIET_MS: u64 = 20_000;
const GATE_RETRY_MS: u64 = 60_000;
const LEGACY_RECHECK_MS: u64 = 600_000;
const BACKOFF_MIN_MS: u64 = 1_000;
/// 30 s, not the minute QUIC's other relay waits at most: a relay that comes
/// back within reach is found again within half a minute of it, so two edges
/// that can meet there are together within 45 s (`native_meet_sim`).
const BACKOFF_MAX_MS: u64 = 30_000;
const BACKOFF_RESET_MS: u64 = 60_000;
/// How often the supervisor asks the rule what to do, between the notes that
/// also make it ask.
const TICK_MS: u64 = 500;

/// The shortest R1 waits for a silent active relay to answer once the other
/// relay has gone live (see [`NativeMeet::step`]).
const SILENT_CONFIRM_MIN: Duration = Duration::from_millis(300);

/// How many Register → Ack round trips are kept per relay, and how many each
/// relay needs before the gate is applied at all.
const RTT_SAMPLES: usize = 5;
const GATE_MIN_SAMPLES: usize = 3;

const _: () = {
    assert!(KEEPALIVE_INTERVAL.as_secs() * 1_000 == KEEPALIVE_MS);
    // The relay counts a side gone after three missed keepalives, not two, and
    // well before it clears the side (its 30 s idle timeout).
    assert!(3 * KEEPALIVE_MS <= RELAY_PEER_LIVE_MS && RELAY_PEER_LIVE_MS < 30_000);
    // A move waits out one Register and its fast retry.
    assert!(SETTLE_MS > KEEPALIVE_MS + FAST_MS);
    assert!(SILENT_MS >= 2 * KEEPALIVE_MS);
    assert!(RELAY_DEAD_TIMEOUT_MS > SILENT_MS);
    // The relay moved from stays open until the peer, which fails back up to a
    // few keepalives later, has moved too.
    assert!(LINGER_MS >= STEADY_MS + 4 * KEEPALIVE_MS);
    assert!(SLOT_TAKEOVER_GRACE_MS < RELAY_DEAD_TIMEOUT_MS);
};

/// The waits of the rule. [`Default`] is what a tunnel runs with; tests shorten
/// them, keeping their order.
#[derive(Debug, Clone)]
pub(crate) struct NativeTiming {
    /// A socket's Register cadence ([`KEEPALIVE_INTERVAL`]).
    pub keepalive: Duration,
    /// The cadence while there is something to find out quickly: a socket's
    /// first [`Self::keepalive`], a refusal, an answer that is late, a peer
    /// that is not heard.
    pub fast: Duration,
    /// How long a refusal, or one absence of the peer, keeps the fast cadence.
    pub fast_for: Duration,
    /// A relay not answered for this long is not heard.
    pub silent: Duration,
    /// How long the peer may be absent from the active relay before the
    /// other relay is opened to look for it there.
    pub waiting: Duration,
    /// How long the active relay has to be in trouble before a move.
    pub settle: Duration,
    /// A relay not answered for this long is given up: a new socket, or with
    /// an older relay the other relay ([`RELAY_DEAD_TIMEOUT_MS`]).
    pub dead: Duration,
    /// How long both sides have to be heard on the primary before a failback.
    pub steady: Duration,
    /// How long both sides have to be heard on the primary before the backup
    /// socket is closed.
    pub linger: Duration,
    /// How long after a move the socket moved from stays open at least.
    pub move_quiet: Duration,
    /// How long a primary that failed the RTT gate stays closed.
    pub gate_retry: Duration,
    /// How long an older relay on the other side is left alone before it is
    /// tried again.
    pub legacy_recheck: Duration,
    /// The wait before reopening a socket that came to nothing, doubling up to
    /// the max, and how long a socket has to be heard to start it over.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub backoff_reset: Duration,
    /// How often the supervisor steps the rule between notes.
    pub tick: Duration,
}

impl Default for NativeTiming {
    fn default() -> Self {
        let ms = Duration::from_millis;
        Self {
            keepalive: KEEPALIVE_INTERVAL,
            fast: ms(FAST_MS),
            fast_for: ms(FAST_FOR_MS),
            silent: ms(SILENT_MS),
            waiting: ms(WAITING_MS),
            settle: ms(SETTLE_MS),
            dead: ms(RELAY_DEAD_TIMEOUT_MS),
            steady: ms(STEADY_MS),
            linger: ms(LINGER_MS),
            move_quiet: ms(MOVE_QUIET_MS),
            gate_retry: ms(GATE_RETRY_MS),
            legacy_recheck: ms(LEGACY_RECHECK_MS),
            backoff_min: ms(BACKOFF_MIN_MS),
            backoff_max: ms(BACKOFF_MAX_MS),
            backoff_reset: ms(BACKOFF_RESET_MS),
            tick: ms(TICK_MS),
        }
    }
}

/// What a relay is, by what its Acks carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// No Ack yet. Counted as [`Kind::New`].
    Unknown,
    /// After v0.20.0: its Acks say whether the peer is heard.
    New,
    /// Up to v0.20.0: its `ready` says only that both sides were latched once.
    Legacy,
}

/// The other side of the tunnel, as the relay last said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Peer {
    Unknown,
    /// Not heard there since `since` (when the edge was first told).
    Absent { since: Instant },
    /// Both sides heard there without a break since `since`.
    Live { since: Instant },
}

/// Why the tunnel moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// The relay it was on stopped answering.
    RelaySilent,
    /// The relay answers, but the peer is heard on the other one.
    PeerElsewhere,
    /// Back to the primary, with the peer.
    Failback,
}

impl Why {
    /// The `reason` of the move's event.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::RelaySilent => "relay_silent",
            Self::PeerElsewhere => "peer_elsewhere",
            Self::Failback => "failback",
        }
    }
}

/// What the supervisor is to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Act {
    /// Open a socket on relay `i`. [`NativeMeet::opened`] or
    /// [`NativeMeet::open_failed`] says how it went.
    Open(usize),
    Close(usize),
    /// Open a new socket on relay `i`, resolving its name again and trying the
    /// next address, and drop the one it has once the new one is open.
    Redial(usize),
    /// Send a Register on relay `i`'s socket now.
    RegisterNow(usize),
    /// Send the media on relay `to` from now on.
    Move { from: usize, to: usize, why: Why },
    State(RelayTunnelState),
}

/// What an Ack said, once the supervisor has checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AckView {
    pub ready: bool,
    pub peer_live: Option<bool>,
    pub live_ms: Option<u64>,
    pub relay_id: Option<String>,
    /// From the Register whose `seq` it echoes, when it echoes one.
    pub rtt: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
struct Sock {
    generation: u64,
    opened_at: Instant,
}

#[derive(Debug)]
struct Relay {
    sock: Option<Sock>,
    /// A socket is on the way ([`Act::Open`] or [`Act::Redial`]).
    opening: bool,
    /// The last Ack on the socket it has now.
    heard_at: Option<Instant>,
    peer: Peer,
    kind: Kind,
    relay_id: Option<String>,
    /// An Ack on this socket said `ready`.
    ready_seen: bool,
    rtts: VecDeque<Duration>,
    /// The median round trip of the last socket on it that had any: how long
    /// to give a new socket's first answer, before it has any of its own.
    last_rtt: Option<Duration>,
    /// Not reopened, or redialled, before this.
    retry_at: Option<Instant>,
    backoff: Duration,
    gate_closed_until: Option<Instant>,
    legacy_until: Option<Instant>,
    refused_at: Option<Instant>,
    /// Since when it has been [`NativeMeet::live`] without a break.
    live_run: Option<Instant>,
    /// Register on it at the next step.
    ask: bool,
}

impl Relay {
    fn new(timing: &NativeTiming) -> Self {
        Self {
            sock: None,
            opening: false,
            heard_at: None,
            peer: Peer::Unknown,
            kind: Kind::Unknown,
            relay_id: None,
            ready_seen: false,
            rtts: VecDeque::with_capacity(RTT_SAMPLES),
            last_rtt: None,
            retry_at: None,
            backoff: timing.backoff_min,
            gate_closed_until: None,
            legacy_until: None,
            refused_at: None,
            live_run: None,
            ask: false,
        }
    }

    /// Forget what the socket it no longer has told it. What it knows of the
    /// relay itself (its kind and id) stays.
    fn forget_socket(&mut self) {
        self.heard_at = None;
        self.peer = Peer::Unknown;
        self.ready_seen = false;
        self.last_rtt = median(&self.rtts).or(self.last_rtt);
        self.rtts.clear();
        self.refused_at = None;
        self.live_run = None;
    }

    /// When it was last answered, or opened if never: what its silence counts from.
    fn quiet_since(&self) -> Option<Instant> {
        let sock = self.sock?;
        Some(self.heard_at.map_or(sock.opened_at, |at| at.max(sock.opened_at)))
    }

    fn retry_due(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| at <= now)
    }

    fn back_off(&mut self, now: Instant, timing: &NativeTiming) {
        self.retry_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(timing.backoff_max);
    }
}

/// How much of the rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// One relay, or one relay at two addresses: nowhere else to go.
    Single,
    /// An older relay is involved: move only off a relay silent for
    /// [`NativeTiming::dead`], and never back.
    Legacy,
    Full,
}

/// R1's check that the active relay's trouble is real: once everything else
/// says move, the active relay is asked again, and the move waits for it not
/// to answer otherwise. A peer that has just come back to it, and an uplink of
/// this edge's that has just come back, show there within that.
#[derive(Debug, Clone, Copy)]
struct Confirm {
    /// When R1 first held, this time.
    from: Instant,
    /// How many Registers it has asked for.
    asked: u8,
}

/// The rule of one tunnel. See the module docs.
#[derive(Debug)]
pub(crate) struct NativeMeet {
    timing: NativeTiming,
    gate: Duration,
    relays: Vec<Relay>,
    active: usize,
    last_move: Option<Instant>,
    /// The two relay addresses answered with one relay id.
    same_relay: bool,
    state: RelayTunnelState,
    confirm: Option<Confirm>,
    /// The earliest instant there is, for a `live_ms` longer than this process.
    born: Instant,
}

impl NativeMeet {
    /// `relays` is how many relay addresses the tunnel has: one or two. A
    /// third is never looked at (validation allows two).
    pub(crate) fn new(relays: usize, timing: NativeTiming, gate_ms: u32, now: Instant) -> Self {
        let relays = relays.clamp(1, 2);
        Self {
            relays: (0..relays).map(|_| Relay::new(&timing)).collect(),
            timing,
            gate: Duration::from_millis(gate_ms.into()),
            active: 0,
            last_move: None,
            same_relay: false,
            state: RelayTunnelState::Connecting,
            confirm: None,
            born: now,
        }
    }

    /// The relay the media goes to.
    pub(crate) fn active(&self) -> usize {
        self.active
    }

    /// The active relay answers.
    pub(crate) fn active_heard(&self, now: Instant) -> bool {
        self.heard(self.active, now)
    }

    /// The two relay addresses have answered with one relay id.
    #[cfg(test)]
    pub(crate) fn same_relay_seen(&self) -> bool {
        self.same_relay
    }

    /// The median of relay `i`'s last Register → Ack round trips.
    pub(crate) fn median_rtt(&self, i: usize) -> Option<Duration> {
        median(&self.relays.get(i)?.rtts)
    }

    /// Relay `i` answered on socket `generation`.
    pub(crate) fn note_ack(&mut self, i: usize, generation: u64, at: Instant, ack: AckView) {
        let born = self.born;
        let is_active = i == self.active;
        let silent = self.timing.silent;
        let Some(r) = self.relays.get_mut(i) else { return };
        if r.sock.is_none_or(|s| s.generation != generation) {
            return;
        }
        // The first answer after a silence: whatever kept this relay from
        // answering (an uplink, say) may have kept the other from it too, and
        // that one is to be asked at once rather than at its next keepalive.
        let was_silent = r.heard_at.is_none_or(|h| at.saturating_duration_since(h) >= silent);
        r.heard_at = Some(at);
        // R8: what the Ack carries says what the relay is, every time, so a
        // relay upgraded under a running tunnel brings the full rules with it.
        r.kind = match ack.peer_live {
            Some(_) => Kind::New,
            None => Kind::Legacy,
        };
        if let Some(id) = ack.relay_id {
            // R5: a restarted relay is another relay until it says otherwise.
            if is_active && r.relay_id.as_deref().is_some_and(|known| known != id) {
                self.same_relay = false;
            }
            r.relay_id = Some(id);
        }
        r.ready_seen |= ack.ready;
        if let Some(rtt) = ack.rtt {
            if r.rtts.len() == RTT_SAMPLES {
                r.rtts.pop_front();
            }
            r.rtts.push_back(rtt);
        }
        r.peer = match ack.peer_live {
            Some(true) => {
                let live = Duration::from_millis(ack.live_ms.unwrap_or(0));
                Peer::Live { since: at.checked_sub(live).unwrap_or(born).max(born) }
            }
            Some(false) => match r.peer {
                absent @ Peer::Absent { .. } => absent,
                _ => Peer::Absent { since: at },
            },
            None => Peer::Unknown,
        };
        let live = r.kind == Kind::New && matches!(r.peer, Peer::Live { .. });
        r.live_run = if live { r.live_run.or(Some(at)) } else { None };
        if was_silent {
            for (j, other) in self.relays.iter_mut().enumerate() {
                other.ask |= j != i;
            }
        }
    }

    /// A datagram to relay `i`, on socket `generation`, was refused: its host has
    /// nothing listening there. That moves nothing; it only speeds up the
    /// Registers, so a relay that restarts is found again within a
    /// [`NativeTiming::fast`] of its return rather than at the next keepalive.
    pub(crate) fn note_refused(&mut self, i: usize, generation: u64, at: Instant) {
        if let Some(r) = self.relays.get_mut(i)
            && r.sock.is_some_and(|s| s.generation == generation)
        {
            r.refused_at = Some(at);
        }
    }

    /// The socket asked for on relay `i` is open, as `generation`. `false` if it is
    /// no longer wanted, and is to be dropped.
    pub(crate) fn opened(&mut self, i: usize, generation: u64, at: Instant) -> bool {
        let Some(r) = self.relays.get_mut(i) else { return false };
        if !std::mem::take(&mut r.opening) {
            return false;
        }
        r.sock = Some(Sock { generation, opened_at: at });
        r.forget_socket();
        true
    }

    /// The socket asked for on relay `i` could not be opened.
    pub(crate) fn open_failed(&mut self, i: usize, at: Instant) {
        let timing = self.timing.clone();
        if let Some(r) = self.relays.get_mut(i) {
            r.opening = false;
            r.back_off(at, &timing);
        }
    }

    /// How long after a Register on relay `i` the next one is due, as the rule
    /// stands at `now`. The supervisor asks again at every step, and sends at
    /// once when the last Register is older than this: a refusal, or an answer
    /// that is late, brings the next one forward.
    pub(crate) fn register_interval(&self, i: usize, now: Instant) -> Duration {
        let t = &self.timing;
        let Some(r) = self.relays.get(i) else { return t.keepalive };
        let Some(sock) = r.sock else { return t.keepalive };
        let young = now.saturating_duration_since(sock.opened_at) < t.keepalive;
        let refused = r.refused_at.is_some_and(|at| now.saturating_duration_since(at) < t.fast_for);
        // A relay that answered and has not answered the Register since, a
        // `fast` after it was due to: asked again every `fast` until it counts
        // as not heard, so one or two lost Registers or Acks in a row never
        // let it lapse. A relay that goes on not answering is asked at the
        // keepalive from then on, like one that never answered.
        let late = r.heard_at.is_some_and(|at| {
            let age = now.saturating_duration_since(at);
            t.keepalive + t.fast <= age && age < t.silent
        });
        // At most `fast_for` per absence: an episode that goes on is checked
        // at the keepalive, like a relay that does not answer at all.
        let absent = self.heard(i, now)
            && matches!(r.peer, Peer::Absent { since } if now.saturating_duration_since(since) < t.fast_for);
        if young || refused || late || absent { t.fast } else { t.keepalive }
    }

    /// Relay `i` has answered the socket it has within [`NativeTiming::silent`].
    fn heard(&self, i: usize, now: Instant) -> bool {
        let r = &self.relays[i];
        r.sock.is_some()
            && r.heard_at.is_some_and(|at| now.saturating_duration_since(at) < self.timing.silent)
    }

    /// Both sides are heard on relay `i`, and it is a relay that can say so.
    fn live(&self, i: usize, now: Instant) -> bool {
        let r = &self.relays[i];
        self.heard(i, now) && r.kind == Kind::New && matches!(r.peer, Peer::Live { .. })
    }

    /// Both sides have been heard on relay `i` for [`NativeTiming::steady`].
    fn steady(&self, i: usize, now: Instant) -> bool {
        self.live(i, now)
            && matches!(self.relays[i].peer,
                Peer::Live { since } if now.saturating_duration_since(since) >= self.timing.steady)
    }

    /// Since when relay `i` has been in trouble: its peer is not heard there,
    /// or it has not answered for a Register and its fast retry.
    fn trouble_since(&self, i: usize, now: Instant) -> Option<Instant> {
        let r = &self.relays[i];
        if let Peer::Absent { since } = r.peer {
            return Some(since);
        }
        let quiet = r.quiet_since()?;
        (now.saturating_duration_since(quiet) >= self.timing.keepalive + self.timing.fast).then_some(quiet)
    }

    fn mode(&self, now: Instant) -> Mode {
        if self.relays.len() == 1 || self.same_relay {
            return Mode::Single;
        }
        let (a, o) = (&self.relays[self.active], &self.relays[1 - self.active]);
        let other_legacy = o.kind == Kind::Legacy && o.legacy_until.is_some_and(|at| at > now);
        if a.kind == Kind::Legacy || other_legacy { Mode::Legacy } else { Mode::Full }
    }

    /// The primary is not more than the gate slower than the backup, or there
    /// is too little to tell.
    fn gate_passes(&self) -> bool {
        let (p, b) = (&self.relays[0].rtts, &self.relays[1].rtts);
        if p.len() < GATE_MIN_SAMPLES || b.len() < GATE_MIN_SAMPLES {
            return true;
        }
        match (median(p), median(b)) {
            (Some(p), Some(b)) => p <= b + self.gate,
            _ => true,
        }
    }

    fn close(&mut self, i: usize, acts: &mut Vec<Act>) {
        let r = &mut self.relays[i];
        r.sock = None;
        r.opening = false;
        r.forget_socket();
        acts.push(Act::Close(i));
    }

    fn open(&mut self, i: usize, acts: &mut Vec<Act>) {
        self.relays[i].opening = true;
        acts.push(Act::Open(i));
    }

    fn move_to(&mut self, to: usize, why: Why, now: Instant, acts: &mut Vec<Act>) {
        let from = self.active;
        self.active = to;
        self.last_move = Some(now);
        self.confirm = None;
        acts.push(Act::Move { from, to, why });
        // Both relays learn at once which side stands by.
        for i in 0..self.relays.len() {
            if self.relays[i].sock.is_some() {
                acts.push(Act::RegisterNow(i));
            }
        }
    }

    /// What to do now.
    pub(crate) fn step(&mut self, now: Instant) -> Vec<Act> {
        let mut acts = Vec::new();
        let t = self.timing.clone();
        for i in 0..self.relays.len() {
            if !self.live(i, now) {
                self.relays[i].live_run = None;
            }
            let r = &mut self.relays[i];
            if let (Some(sock), Some(heard)) = (r.sock, r.heard_at)
                && now.saturating_duration_since(heard) < t.silent
                && now.saturating_duration_since(sock.opened_at) >= t.backoff_reset
            {
                r.backoff = t.backoff_min;
            }
        }
        for (i, r) in self.relays.iter_mut().enumerate() {
            if std::mem::take(&mut r.ask) && r.sock.is_some() {
                acts.push(Act::RegisterNow(i));
            }
        }
        let a = self.active;
        // The active relay always has a socket, or one on the way.
        if self.relays[a].sock.is_none() && !self.relays[a].opening && self.relays[a].retry_due(now) {
            self.open(a, &mut acts);
        }
        if self.relays.len() == 2 {
            self.two_relays(now, &mut acts);
        }
        self.dead_active(now, &mut acts);
        let state = self.state_now(now);
        if state != self.state {
            self.state = state;
            acts.push(Act::State(state));
        }
        acts
    }

    /// R1 to R6.
    fn two_relays(&mut self, now: Instant, acts: &mut Vec<Act>) {
        let t = self.timing.clone();
        let a = self.active;
        let o = 1 - a;

        // What R5 learned holds only while the address it kept answers: once
        // that has been silent for `silent`, the other address may be the
        // only way left to the relay, and the full rules look for it there.
        if self.same_relay
            && self.relays[a].quiet_since().is_some_and(|q| now.saturating_duration_since(q) >= t.silent)
        {
            tracing::info!("native-UDP relay: the relay address kept has gone silent; trying its other address");
            self.same_relay = false;
        }

        // R5: the other relay is this one at a second address. Its socket
        // took our side away from this one: let go of it, and take it back —
        // while this one answers. One that does not is left for R1, which
        // moves to the address that does.
        if self.heard(a, now)
            && self.relays[o].sock.is_some()
            && self.relays[o].relay_id.is_some()
            && self.relays[o].relay_id == self.relays[a].relay_id
            && self.relays[o].heard_at.is_some()
        {
            tracing::info!("native-UDP relay: both relay addresses are one relay; keeping one socket");
            self.close(o, acts);
            self.same_relay = true;
            acts.push(Act::RegisterNow(a));
        }

        // R6: an older relay on the other side cannot say where the peer is,
        // so it is not stood by on, and is looked at again later.
        if self.relays[o].sock.is_some() && self.relays[o].kind == Kind::Legacy && self.relays[o].heard_at.is_some() {
            self.close(o, acts);
            self.relays[o].legacy_until = Some(now + t.legacy_recheck);
        }

        if self.mode(now) == Mode::Full {
            // R1: follow the peer, or leave a relay that has gone silent —
            // once the active relay has been asked once more and has not
            // answered otherwise (see [`Confirm`]).
            let settled = self.relays[o].sock.is_some()
                && self.live(o, now)
                && self.trouble_since(a, now).is_some_and(|since| since + t.settle <= now);
            if !settled {
                self.confirm = None;
            } else {
                let heard = self.heard(a, now);
                // A relay that answers is asked once more a `fast` later, by
                // when a peer that has just come back to it has registered
                // there. One that does not is given a few round trips to
                // answer — its own, as it answered before it fell silent (on
                // the socket it had then, if it has been given a new one), or
                // the other relay's if those are longer — which is all an
                // uplink that has just come back needs: at least
                // [`SILENT_CONFIRM_MIN`], and at most a `fast`, or two of its
                // own round trips if those are longer still.
                let wait = match heard {
                    true => t.fast,
                    false => {
                        let own = self.median_rtt(a).or(self.relays[a].last_rtt);
                        match own.max(self.relays[o].rtts.back().copied()) {
                            Some(rtt) => {
                                let most = t.fast.max(own.unwrap_or_default() * 2);
                                (rtt * 3).max(SILENT_CONFIRM_MIN).min(most)
                            }
                            None => t.fast,
                        }
                    }
                };
                let c = match self.confirm {
                    Some(c) => c,
                    None => {
                        acts.push(Act::RegisterNow(a));
                        *self.confirm.insert(Confirm { from: now, asked: 1 })
                    }
                };
                let confirm_at = c.from + wait;
                let confirmed = match heard {
                    true => self.relays[a].heard_at.is_some_and(|at| at >= confirm_at),
                    false => now >= confirm_at,
                };
                if confirmed {
                    let why = if heard { Why::PeerElsewhere } else { Why::RelaySilent };
                    self.move_to(o, why, now, acts);
                    return;
                }
                if heard && c.asked == 1 && now >= confirm_at {
                    acts.push(Act::RegisterNow(a));
                    self.confirm = Some(Confirm { asked: 2, ..c });
                }
            }

            // R2: back to the primary, with the peer.
            if a == 1 && self.live(1, now) && self.steady(0, now) && self.gate_passes() {
                self.move_to(0, Why::Failback, now, acts);
                return;
            }

            // R3: open the other relay while there is a reason to.
            let other = &self.relays[o];
            if other.sock.is_none() && !other.opening && other.retry_due(now) {
                let gated = other.gate_closed_until.is_some_and(|at| at > now) && self.live(1, now);
                let stand_by = a == 1 && !gated;
                let peer_away = matches!(self.relays[a].peer,
                    Peer::Absent { since } if now.saturating_duration_since(since) >= t.waiting);
                let unheard = self.relays[a]
                    .quiet_since()
                    .is_some_and(|q| now.saturating_duration_since(q) >= t.silent);
                if stand_by || peer_away || unheard {
                    self.open(o, acts);
                }
            }
        }

        // R4: close the other relay once there is no reason to keep it.
        if self.relays[o].sock.is_some() {
            let linger = a == 0
                && self.live(0, now)
                && matches!(self.relays[0].peer,
                    Peer::Live { since } if now.saturating_duration_since(since) >= t.linger)
                && self.last_move.is_none_or(|at| now.saturating_duration_since(at) >= t.move_quiet);
            let dead = self.relays[o]
                .quiet_since()
                .is_some_and(|q| now.saturating_duration_since(q) >= t.dead);
            let too_far = a == 1
                && self.live(1, now)
                && self.relays[0].rtts.len() >= GATE_MIN_SAMPLES
                && self.relays[1].rtts.len() >= GATE_MIN_SAMPLES
                && !self.gate_passes();
            if linger {
                self.close(o, acts);
            } else if dead {
                self.close(o, acts);
                self.relays[o].back_off(now, &t);
            } else if too_far {
                tracing::info!(
                    "native-UDP relay: the primary is more than {:?} slower than the backup; \
                     leaving it for {:?}",
                    self.gate,
                    t.gate_retry
                );
                self.close(0, acts);
                self.relays[0].gate_closed_until = Some(now + t.gate_retry);
            }
        }
    }

    /// R7: the active relay has not answered for [`NativeTiming::dead`].
    fn dead_active(&mut self, now: Instant, acts: &mut Vec<Act>) {
        let t = self.timing.clone();
        let a = self.active;
        let r = &self.relays[a];
        let dead = !r.opening
            && r.retry_due(now)
            && r.quiet_since().is_some_and(|q| now.saturating_duration_since(q) >= t.dead);
        if !dead {
            return;
        }
        if self.mode(now) == Mode::Legacy && self.relays.len() == 2 {
            // Today's rotation, without the forwarder rebuild.
            let o = 1 - a;
            self.move_to(o, Why::RelaySilent, now, acts);
            if self.relays[o].sock.is_none() && !self.relays[o].opening {
                self.open(o, acts);
            }
            return;
        }
        let r = &mut self.relays[a];
        r.opening = true;
        r.back_off(now, &t);
        acts.push(Act::Redial(a));
    }

    fn state_now(&self, now: Instant) -> RelayTunnelState {
        let a = &self.relays[self.active];
        if a.sock.is_none() || a.heard_at.is_none() {
            return RelayTunnelState::Connecting;
        }
        match a.kind {
            Kind::Unknown => RelayTunnelState::Connecting,
            Kind::New if self.live(self.active, now) => RelayTunnelState::Ready,
            Kind::New => RelayTunnelState::Waiting,
            // An older relay never says the peer is gone: today's rule.
            Kind::Legacy if a.ready_seen => RelayTunnelState::Ready,
            Kind::Legacy => RelayTunnelState::Waiting,
        }
    }
}

fn median(samples: &VecDeque<Duration>) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<Duration> = samples.iter().copied().collect();
    sorted.sort();
    Some(sorted[sorted.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use RelayTunnelState::{Connecting, Ready, Waiting};

    const ID0: &str = "00000000000000000000000000000000";
    const ID1: &str = "11111111111111111111111111111111";

    /// One tunnel's rule, driven by hand in seconds from its start. Every
    /// socket it asks for opens at once.
    struct Rule {
        rule: NativeMeet,
        t0: Instant,
        now: Instant,
        generation: u64,
        gens: [u64; 2],
        /// Every act since the start.
        log: Vec<Act>,
    }

    impl Rule {
        fn new(relays: usize) -> Self {
            let t0 = Instant::now();
            let mut rule = Self {
                rule: NativeMeet::new(relays, NativeTiming::default(), 50, t0),
                t0,
                now: t0,
                generation: 0,
                gens: [0; 2],
                log: Vec::new(),
            };
            rule.at(0.0);
            rule
        }

        fn time(&self, secs: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(secs)
        }

        /// Step the rule at `secs`, opening what it asks for. What it did.
        fn at(&mut self, secs: f64) -> Vec<Act> {
            self.now = self.time(secs);
            let acts = self.rule.step(self.now);
            for act in &acts {
                if let Act::Open(i) | Act::Redial(i) = *act {
                    self.generation += 1;
                    self.gens[i] = self.generation;
                    assert!(self.rule.opened(i, self.generation, self.now));
                }
            }
            self.log.extend(acts.iter().cloned());
            acts
        }

        /// Step every half second from now to `secs`, as the supervisor's
        /// tick does, acking relay `i` as `ack` says each keepalive.
        fn run(&mut self, until: f64, mut ack: impl FnMut(&mut Self, f64)) {
            let mut secs = (self.now - self.t0).as_secs_f64();
            while secs < until - 1e-9 {
                secs = (((secs * 2.0 + 1e-9).floor() + 1.0) / 2.0).min(until);
                ack(self, secs);
                self.at(secs);
            }
        }

        fn ack(&mut self, i: usize, secs: f64, view: AckView) {
            self.now = self.time(secs);
            self.rule.note_ack(i, self.gens[i], self.now, view);
        }

        /// A relay after v0.20.0's answer.
        fn new_ack(&mut self, i: usize, secs: f64, peer_live: bool, live_ms: u64) {
            let id = if i == 0 { ID0 } else { ID1 };
            self.ack(i, secs, view(peer_live, live_ms, id, 20));
        }

        fn moves(&self) -> Vec<(usize, usize, Why)> {
            self.log
                .iter()
                .filter_map(|a| match *a {
                    Act::Move { from, to, why } => Some((from, to, why)),
                    _ => None,
                })
                .collect()
        }

        fn opened(&self, i: usize) -> usize {
            self.log.iter().filter(|a| **a == Act::Open(i)).count()
        }

        fn closed(&self, i: usize) -> usize {
            self.log.iter().filter(|a| **a == Act::Close(i)).count()
        }

        fn is_open(&self, i: usize) -> bool {
            self.rule.relays[i].sock.is_some()
        }

        fn last_state(&self) -> Option<RelayTunnelState> {
            self.log.iter().rev().find_map(|a| match a {
                Act::State(s) => Some(*s),
                _ => None,
            })
        }

        /// On relay 0, both sides heard there since the start.
        fn up_on_primary() -> Self {
            let mut r = Self::new(2);
            r.new_ack(0, 0.05, true, 0);
            r.at(0.1);
            r
        }

        /// Moved to the backup, the peer with it, 0 standing by and silent.
        fn up_on_backup() -> Self {
            let mut r = Self::up_on_primary();
            r.run(200.0, |r, s| {
                if s < 100.0 && every(s, 5.0) {
                    r.new_ack(0, s, true, (s * 1000.0) as u64);
                }
                if s >= 100.0 && every(s, 1.0) {
                    r.new_ack(1, s, true, ((s - 100.0) * 1000.0) as u64);
                }
            });
            // The primary went silent at 100 s: the backup is opened, the
            // peer comes there, the tunnel moves.
            assert_eq!(r.moves(), [(0, 1, Why::RelaySilent)], "{:?}", r.log);
            r
        }
    }

    fn view(peer_live: bool, live_ms: u64, id: &str, rtt_ms: u64) -> AckView {
        AckView {
            ready: true,
            peer_live: Some(peer_live),
            live_ms: Some(if peer_live { live_ms } else { 0 }),
            relay_id: Some(id.into()),
            rtt: Some(Duration::from_millis(rtt_ms)),
        }
    }

    fn legacy(ready: bool) -> AckView {
        AckView { ready, peer_live: None, live_ms: None, relay_id: None, rtt: None }
    }

    fn every(secs: f64, period: f64) -> bool {
        let n = (secs / period).round();
        (secs - n * period).abs() < 1e-6
    }

    #[test]
    fn the_relay_peer_live_window_is_16_s() {
        // bilbycast-relay `udp_relay::the_peer_live_window_is_16_s` pins its
        // side of the same number.
        assert_eq!(RELAY_PEER_LIVE_MS, 16_000);
    }

    #[test]
    fn waits_before_opening_the_other() {
        let mut r = Rule::new(2);
        assert_eq!(r.log, [Act::Open(0)]);
        // Answered at once, but the peer is not there.
        r.run(10.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        assert_eq!(r.opened(1), 0, "the backup is opened only once the peer has been away 10 s");
        r.run(11.5, |r, s| r.new_ack(0, s, false, 0));
        assert_eq!(r.opened(1), 1);
        assert_eq!(r.last_state(), Some(Waiting));

        // A relay that does not answer at all: the other is opened after 10 s
        // of silence.
        let mut r = Rule::new(2);
        r.run(9.5, |_, _| {});
        assert_eq!(r.opened(1), 0);
        r.run(10.5, |_, _| {});
        assert_eq!(r.opened(1), 1);
        assert_eq!(r.last_state(), None, "still Connecting: no Ack on the active relay yet");
    }

    #[test]
    fn moves_only_after_settle_while_the_other_is_live() {
        let mut r = Rule::up_on_primary();
        // The peer leaves the primary at 20 s (the relay says so at 36 s).
        r.run(36.0, |r, s| {
            if every(s, 5.0) {
                r.new_ack(0, s, true, (s * 1000.0) as u64);
            }
        });
        r.run(46.5, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        assert_eq!(r.opened(1), 0);
        r.run(47.5, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        assert_eq!(r.opened(1), 1, "opened 10 s into the absence");
        // The backup has nobody either: nothing moves however long.
        r.run(80.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
                r.new_ack(1, s, false, 0);
            }
        });
        assert!(r.moves().is_empty());
        // The peer turns up on the backup at 80 s: the move waits for one
        // more answer from the primary that still has no peer.
        r.new_ack(1, 80.2, true, 0);
        let acts = r.at(80.3);
        assert!(acts.contains(&Act::RegisterNow(0)), "the primary is asked again: {acts:?}");
        assert!(r.moves().is_empty());
        r.new_ack(0, 80.4, false, 0);
        r.at(80.5);
        assert!(r.moves().is_empty(), "that answer predates the peer's arrival by less than `fast`");
        r.new_ack(1, 81.2, true, 1000);
        r.new_ack(0, 81.3, false, 0);
        r.at(81.5);
        assert_eq!(r.moves(), [(0, 1, Why::PeerElsewhere)]);
        assert_eq!(r.rule.active(), 1);
        assert_eq!(r.last_state(), Some(Ready));
    }

    #[test]
    fn a_peer_back_on_the_active_relay_in_time_stops_the_move() {
        let mut r = Rule::up_on_primary();
        r.run(20.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        r.run(30.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
                r.new_ack(1, s, false, 0);
            }
        });
        // Back on both at once, as after an uplink outage of the peer's.
        r.run(60.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(1, s, true, 0);
                r.new_ack(0, s + 0.05, true, 0);
            }
        });
        assert!(r.moves().is_empty(), "{:?}", r.log);
    }

    #[test]
    fn never_leaves_a_live_active() {
        let mut r = Rule::up_on_primary();
        // Both relays say both sides are heard, for an hour.
        r.rule.relays[1].opening = true;
        r.generation += 1;
        r.gens[1] = r.generation;
        assert!(r.rule.opened(1, r.generation, r.now));
        r.run(3600.0, |r, s| {
            if every(s, 5.0) {
                r.new_ack(0, s, true, (s * 1000.0) as u64);
                r.new_ack(1, s, true, (s * 1000.0) as u64);
            }
        });
        assert!(r.moves().is_empty());
        assert_eq!(r.closed(1), 1, "the backup is let go of once it is not needed");
    }

    #[test]
    fn fails_back_only_after_60_s_steady_by_live_ms() {
        let mut r = Rule::up_on_backup();
        assert!(r.is_open(0), "the primary is stood by on for the whole stay on the backup");
        // The primary answers again from 200 s, with the peer standing by on
        // it too; live_ms counts from 205 s.
        let start = (r.now - r.t0).as_secs_f64();
        r.run(start + 64.0, |r, s| {
            if every(s, 5.0) {
                r.new_ack(1, s, true, 1_000_000);
                let live = ((s - 205.0).max(0.0) * 1000.0) as u64;
                r.new_ack(0, s, s >= 205.0, live);
            }
        });
        assert_eq!(r.moves().len(), 1, "not before 60 s of live_ms: {:?}", r.moves());
        r.run(start + 70.0, |r, s| {
            if every(s, 5.0) {
                r.new_ack(1, s, true, 1_000_000);
                r.new_ack(0, s, true, ((s - 205.0) * 1000.0) as u64);
            }
        });
        assert_eq!(r.moves()[1], (1, 0, Why::Failback));
        assert!(r.is_open(1), "the backup stays open for the peer still on it");
    }

    #[test]
    fn keeps_the_relay_left_until_linger() {
        let mut r = Rule::up_on_backup();
        let start = (r.now - r.t0).as_secs_f64();
        r.run(start + 70.0, |r, s| {
            if every(s, 5.0) {
                r.new_ack(1, s, true, 1_000_000);
                r.new_ack(0, s, true, ((s - start) * 1000.0) as u64);
            }
        });
        let back = r.moves().len();
        assert_eq!(r.moves()[back - 1].2, Why::Failback);
        let failback = (r.now - r.t0).as_secs_f64();
        // The primary has been live since `start`: the backup is kept until
        // 80 s of that, and 20 s after the move.
        r.run(start + 79.5, |r, s| {
            if every(s, 5.0) {
                r.new_ack(1, s, true, 1_000_000);
                r.new_ack(0, s, true, ((s - start) * 1000.0) as u64);
            }
        });
        assert!(r.is_open(1));
        r.run((start + 82.0).max(failback + 21.0), |r, s| {
            if every(s, 5.0) {
                r.new_ack(1, s, true, 1_000_000);
                r.new_ack(0, s, true, ((s - start) * 1000.0) as u64);
            }
        });
        assert!(!r.is_open(1), "let go of after linger and move_quiet");
    }

    #[test]
    fn the_gate_closes_the_primary_only_while_up_on_the_backup_and_reopens() {
        let mut r = Rule::up_on_backup();
        let start = (r.now - r.t0).as_secs_f64();
        let far = |r: &mut Rule, s: f64, backup_live: bool| {
            if every(s, 5.0) {
                r.ack(1, s, view(backup_live, 1_000_000, ID1, 10));
                r.ack(0, s, view(true, ((s - start) * 1000.0) as u64, ID0, 200));
            }
        };
        r.run(start + 25.0, |r, s| far(r, s, true));
        assert!(!r.is_open(0), "a primary 190 ms slower than the backup is let go of");
        let closes = r.closed(0);
        // Not reopened while up on the backup...
        r.run(start + 50.0, |r, s| far(r, s, true));
        assert_eq!(r.closed(0), closes);
        assert!(!r.is_open(0));
        // ...but at once when the backup loses the peer, so that a pair that
        // has to meet on a far primary can.
        r.run(start + 75.0, |r, s| {
            if every(s, 1.0) {
                r.ack(1, s, view(false, 0, ID1, 10));
            }
        });
        assert!(r.is_open(0), "{:?}", r.log);
        assert!(r.moves().iter().all(|m| m.2 != Why::Failback));

        // And after gate_retry while up on the backup.
        let mut r = Rule::up_on_backup();
        let start = (r.now - r.t0).as_secs_f64();
        r.run(start + 25.0, |r, s| far(r, s, true));
        assert!(!r.is_open(0));
        let opens = r.opened(0);
        r.run(start + 75.0, |r, s| far(r, s, true));
        assert_eq!(r.opened(0), opens, "not before gate_retry");
        r.run(start + 90.0, |r, s| far(r, s, true));
        assert_eq!(r.opened(0), opens + 1, "tried again after gate_retry: {:?}", r.log);
    }

    #[test]
    fn equal_relay_ids_close_the_other_and_relatch() {
        let mut r = Rule::new(2);
        // Relay 0 does not answer: the other address is opened at 10 s.
        r.run(10.5, |_, _| {});
        assert!(r.is_open(1));
        // It is the same relay: both addresses answer with one id.
        r.ack(1, 10.6, view(false, 0, ID0, 10));
        r.ack(0, 10.7, view(false, 0, ID0, 10));
        let acts = r.at(11.0);
        assert!(acts.contains(&Act::Close(1)) && acts.contains(&Act::RegisterNow(0)), "{acts:?}");
        // Nothing reopens it, however long the peer is away.
        r.run(100.0, |r, s| {
            if every(s, 1.0) {
                r.ack(0, s, view(false, 0, ID0, 10));
            }
        });
        assert_eq!(r.opened(1), 1);
        // A restarted relay is looked at again.
        r.ack(0, 100.1, view(false, 0, ID1, 10));
        r.run(102.0, |r, s| {
            if every(s, 1.0) {
                r.ack(0, s, view(false, 0, ID1, 10));
            }
        });
        assert_eq!(r.opened(1), 2);
    }

    /// One relay at two addresses, the path to the one in use cut: the tunnel
    /// moves to the address that answers, whether or not it had learned the
    /// two were one relay. It used to keep the silent one, as the one socket
    /// it keeps on one relay, for as long as the cut lasted.
    #[test]
    fn one_relay_at_two_addresses_follows_the_address_that_answers() {
        // Not learned yet: the cut is what opens the second address.
        let mut r = Rule::up_on_primary();
        r.run(20.0, |r, s| {
            if every(s, 5.0) {
                r.ack(0, s, view(true, (s * 1000.0) as u64, ID0, 20));
            }
        });
        r.run(40.0, |r, s| {
            if r.is_open(1) && every(s, 1.0) {
                r.ack(1, s, view(true, 40_000, ID0, 20));
            }
        });
        assert_eq!(r.moves(), [(0, 1, Why::RelaySilent)], "{:?}", r.log);
        assert!(!r.is_open(0), "the silent address is let go of once the other is known to be the same relay");
        assert!(r.rule.same_relay_seen());
        assert_eq!(r.last_state(), Some(Ready));

        // Learned while the peer was away, then the cut.
        let mut r = Rule::up_on_primary();
        r.run(40.0, |r, s| {
            if every(s, 1.0) {
                r.ack(0, s, view(s < 5.0, 1000, ID0, 20));
                if r.is_open(1) {
                    r.ack(1, s, view(false, 0, ID0, 20));
                }
            }
        });
        assert!(r.rule.same_relay_seen() && !r.is_open(1), "{:?}", r.log);
        r.run(60.0, |r, s| {
            if every(s, 5.0) {
                r.ack(0, s, view(true, ((s - 40.0) * 1000.0) as u64, ID0, 20));
            }
        });
        // The path to the first address goes at 60 s.
        r.run(90.0, |r, s| {
            if r.is_open(1) && every(s, 1.0) {
                r.ack(1, s, view(true, 40_000, ID0, 20));
            }
        });
        assert_eq!(r.moves(), [(0, 1, Why::RelaySilent)], "{:?}", r.log);
        let moved = r.log.iter().position(|a| matches!(a, Act::Move { .. })).unwrap();
        assert!(
            !r.log[..moved].contains(&Act::Redial(0)),
            "moved before the silent address was given a new socket: {:?}",
            r.log
        );
        assert_eq!(r.last_state(), Some(Ready));
    }

    /// A silent active relay with a long round trip, back just after a backup
    /// with a short one has gone live: the move waits for the active relay's
    /// own round trips, and its answer stops it. It used to wait three of the
    /// backup's.
    #[test]
    fn a_silent_relay_is_given_its_own_round_trips_to_answer() {
        let mut r = Rule::new(2);
        r.run(100.0, |r, s| {
            if every(s, 5.0) {
                r.ack(0, s, view(true, (s * 1000.0) as u64, ID0, 320));
            }
        });
        // This edge's uplink goes at 100 s; the backup is opened while it is
        // out, and the peer, which no longer hears it on the primary, comes
        // there.
        r.run(140.0, |_, _| {});
        assert!(r.is_open(1));
        // Back at 140 s: the backup answers in 15 ms, the primary in 320.
        r.ack(1, 140.015, view(true, 5_000, ID1, 15));
        r.at(140.02);
        r.at(140.35);
        r.at(140.4);
        r.ack(0, 140.42, view(true, 140_000, ID0, 320));
        r.at(140.5);
        r.run(160.0, |r, s| {
            if every(s, 1.0) {
                r.ack(0, s, view(true, (s * 1000.0) as u64, ID0, 320));
                r.ack(1, s, view(true, 5_000, ID1, 15));
            }
        });
        assert!(r.moves().is_empty(), "{:?}", r.log);
    }

    #[test]
    fn a_legacy_other_is_never_followed_and_is_rechecked() {
        let mut r = Rule::up_on_primary();
        r.run(20.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        assert!(r.is_open(1));
        // The backup is an older relay. Its `ready` says nothing of where the
        // peer is now, so it is let go of rather than followed.
        r.ack(1, 20.1, legacy(true));
        r.at(20.5);
        assert!(!r.is_open(1));
        r.run(600.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        assert!(r.moves().is_empty());
        assert_eq!(r.opened(1), 1, "not tried again for 10 minutes");
        r.run(625.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        assert_eq!(r.opened(1), 2, "{:?}", r.log);
    }

    #[test]
    fn an_upgraded_active_relay_switches_to_full_rules() {
        let mut r = Rule::new(2);
        r.ack(0, 0.1, legacy(true));
        r.run(100.0, |r, s| {
            if every(s, 5.0) {
                r.ack(0, s, legacy(true));
            }
        });
        assert_eq!(r.last_state(), Some(Ready), "an older relay's ready, as today");
        assert_eq!(r.opened(1), 0, "an older relay never says the peer is away");
        // Upgraded under the tunnel: its next Acks say the peer is not there.
        r.run(115.0, |r, s| {
            if every(s, 1.0) {
                r.new_ack(0, s, false, 0);
            }
        });
        assert_eq!(r.last_state(), Some(Waiting));
        assert_eq!(r.opened(1), 1, "the full rules look for the peer on the backup");
    }

    #[test]
    fn a_legacy_relay_is_left_only_after_25_s_of_silence() {
        let mut r = Rule::new(2);
        r.ack(0, 0.1, legacy(true));
        r.run(30.0, |r, s| {
            if every(s, 5.0) {
                r.ack(0, s, legacy(true));
            }
        });
        // Silent from 30 s, refusing on top.
        r.run(54.5, |r, s| r.rule.note_refused(0, r.gens[0], r.time(s)));
        assert!(r.moves().is_empty(), "a refusal moves nothing");
        r.run(56.0, |_, _| {});
        assert_eq!(r.moves(), [(0, 1, Why::RelaySilent)]);
        assert!(r.is_open(1));
        // And never back.
        r.run(1000.0, |r, s| {
            if every(s, 5.0) {
                r.ack(1, s, legacy(true));
                r.ack(0, s, legacy(true));
            }
        });
        assert_eq!(r.moves().len(), 1);
    }

    #[test]
    fn a_dead_relay_is_dialled_again() {
        let mut r = Rule::new(1);
        r.new_ack(0, 0.1, true, 0);
        r.run(25.0, |_, _| {});
        assert!(!r.log.contains(&Act::Redial(0)));
        r.run(26.0, |_, _| {});
        assert_eq!(r.log.iter().filter(|a| **a == Act::Redial(0)).count(), 1);
        assert_eq!(r.last_state(), Some(Connecting));
        // Then about every 25 s, waiting longer each time.
        r.run(300.0, |_, _| {});
        let redials = r.log.iter().filter(|a| **a == Act::Redial(0)).count();
        assert!((8..=11).contains(&redials), "{redials}");
    }

    #[test]
    fn a_5_s_absent_blip_after_a_relay_restart_moves_nothing() {
        // On the primary, the backup closed.
        let mut r = Rule::up_on_primary();
        r.run(200.0, |r, s| {
            if every(s, 5.0) {
                r.new_ack(0, s, true, (s * 1000.0) as u64);
            }
        });
        // The relay restarts: refusals for 2 s, then 5 s with the peer not yet
        // registered again.
        r.run(202.0, |r, s| r.rule.note_refused(0, r.gens[0], r.time(s)));
        r.run(207.0, |r, s| {
            if every(s, 1.0) {
                r.ack(0, s, view(false, 0, ID1, 20));
            }
        });
        r.run(400.0, |r, s| {
            if every(s, 1.0) {
                r.ack(0, s, view(true, ((s - 207.0) * 1000.0) as u64, ID1, 20));
            }
        });
        assert!(r.moves().is_empty());
        assert_eq!(r.opened(1), 0, "a blip shorter than `waiting` opens nothing");

        // On the backup with the primary standing by: a blip on the backup.
        let mut r = Rule::up_on_backup();
        let start = (r.now - r.t0).as_secs_f64();
        r.run(start + 5.0, |r, s| {
            if every(s, 1.0) {
                r.ack(1, s, view(false, 0, ID0, 20));
            }
        });
        r.run(start + 30.0, |r, s| {
            if every(s, 5.0) {
                r.ack(1, s, view(true, 1000, ID0, 20));
                r.ack(0, s, view(true, 1000, ID1, 20));
            }
        });
        assert_eq!(r.moves().len(), 1);
    }

    #[test]
    fn a_refusal_moves_nothing() {
        let mut r = Rule::up_on_primary();
        r.run(60.0, |r, s| {
            if every(s, 5.0) {
                r.new_ack(0, s, true, (s * 1000.0) as u64);
            }
            r.rule.note_refused(0, r.gens[0], r.time(s));
        });
        assert!(r.moves().is_empty());
        assert_eq!(r.opened(1), 0);
        assert_eq!(r.last_state(), Some(Ready));
    }

    #[test]
    fn register_cadence_rules() {
        let t = NativeTiming::default();
        let mut r = Rule::new(2);
        let gap = |r: &Rule, i: usize, s: f64| r.rule.register_interval(i, r.time(s));
        assert_eq!(gap(&r, 0, 0.5), t.fast, "a young socket finds out quickly");
        // A socket the relay does not answer gets no faster cadence past its
        // first 5 s: a relay refusing a held slot sees no more than today.
        r.run(6.0, |_, _| {});
        assert_eq!(gap(&r, 0, 6.0), t.keepalive);
        // A refusal: fast for a minute.
        r.rule.note_refused(0, r.gens[0], r.time(6.0));
        assert_eq!(gap(&r, 0, 6.5), t.fast);
        assert_eq!(gap(&r, 0, 66.5), t.keepalive);
        // Heard, with the peer away: fast for at most a minute of the absence.
        r.new_ack(0, 70.0, false, 0);
        r.at(70.0);
        assert_eq!(gap(&r, 0, 70.5), t.fast);
        r.run(129.0, |r, s| r.new_ack(0, s, false, 0));
        assert_eq!(gap(&r, 0, 129.0), t.fast);
        r.run(131.0, |r, s| r.new_ack(0, s, false, 0));
        assert_eq!(gap(&r, 0, 131.0), t.keepalive);
        // Heard with the peer there: the keepalive.
        r.new_ack(0, 132.0, true, 0);
        assert_eq!(gap(&r, 0, 132.0), t.keepalive);
        // An answer a Register and its fast retry late: asked for every
        // `fast` until the relay counts as not heard, then at the keepalive.
        assert_eq!(gap(&r, 0, 137.9), t.keepalive);
        assert_eq!(gap(&r, 0, 138.0), t.fast);
        assert_eq!(gap(&r, 0, 141.9), t.fast);
        assert_eq!(gap(&r, 0, 142.0), t.keepalive);
    }
}
