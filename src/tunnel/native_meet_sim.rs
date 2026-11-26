// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Two native-UDP edges and two relays in simulated milliseconds: whether, how
//! soon and for how long the edges meet, whatever happens to the paths between
//! them and the relays and to the relays themselves, and whatever their timers
//! happen to be at.
//!
//! What a new edge decides is decided by the code itself ([`NativeMeet`] with
//! [`NativeTiming::default`]). What carries it out (the supervisor, its
//! sockets, the carrier that reads them all) is acted out here, and so are the
//! relays, as bilbycast-relay after v0.20.0 runs them: a side is heard from a
//! Register or from its media, `live_ms` restarts after a gap of either side,
//! one Ack answers each Register, and a side unheard for 30 s is cleared. A
//! relay can be restarted (refusing everything meanwhile), can come back
//! refusing binds until it is authorized again, or can be an older one
//! (`ready` = both latched once, no liveness, nothing cleared).
//!
//! [`OldEdge`] is the rule every release up to v0.117.0 runs: start on relay
//! 0, rotate after 25 s without an Ack or at once on any refused datagram,
//! never go back. It reproduces the splits measured on the bench before any
//! bound below is trusted.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, VecDeque};
use std::time::Duration;

use tokio::time::Instant;

use super::native_meet::{Act, AckView, NativeMeet, NativeTiming, RELAY_PEER_LIVE_MS, Why};

/// Milliseconds.
type Ms = u64;

const TICK: Ms = 50;
/// The supervisor's tick.
const STEP: Ms = 500;
/// A relay clears a side unheard this long, at its reaper's next run.
const IDLE: Ms = 30_000;
const REAP: Ms = 10_000;
/// An older edge's dead-relay window and retry delay.
const OLD_DEAD: Ms = 25_000;
const OLD_RETRY: Ms = 1_000;
const KEEPALIVE: Ms = 5_000;

/// The edge that sends the media (registers `egress`), and the one that
/// receives it (and sends SRT's ACKs back when the flow is two-way).
const A: usize = 0;
const B: usize = 1;
const PRIMARY: usize = 0;
const BACKUP: usize = 1;

/// The same numbers on every run.
struct Numbers(u64);

impl Numbers {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n.max(1)
    }

    fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
}

#[derive(Debug, Clone)]
struct Answer {
    ready: bool,
    peer_live: Option<bool>,
    live_ms: Option<u64>,
    relay_id: Option<u64>,
    seq: Option<u32>,
}

#[derive(Debug, Clone)]
enum Msg {
    Register { address: usize, edge: usize, sock: u64, seq: Option<u32>, standby: bool },
    Ack { address: usize, edge: usize, sock: u64, answer: Answer },
    Refused { address: usize, edge: usize, sock: u64 },
}

struct Due {
    at: Ms,
    order: u64,
    msg: Msg,
}

impl PartialEq for Due {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.order) == (other.at, other.order)
    }
}
impl Eq for Due {}
impl PartialOrd for Due {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Due {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // The heap is a max-heap: the earliest first.
        (other.at, other.order).cmp(&(self.at, self.order))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    addr: Option<u64>,
    seen: Ms,
    media: Ms,
    standby: bool,
}

fn slot_heard(slot: &Slot, now: Ms) -> bool {
    let m = slot.seen.max(slot.media);
    slot.addr.is_some() && m != 0 && now.saturating_sub(m) < RELAY_PEER_LIVE_MS
}

/// One relay process, with the one tunnel's session.
struct Relay {
    up: bool,
    id: u64,
    legacy: bool,
    /// After a restart it refuses binds (no Ack, nothing latched) until this.
    refuses_until: Option<Ms>,
    /// `require_bind_auth`: a restart refuses binds until authorized again.
    bind_auth: bool,
    slots: [Slot; 2],
    ever_both: bool,
    live_since: Ms,
    next_reap: Ms,
}

impl Relay {
    fn new(id: u64, legacy: bool) -> Self {
        Self {
            up: true,
            id,
            legacy,
            refuses_until: None,
            bind_auth: false,
            slots: [Slot::default(); 2],
            ever_both: false,
            live_since: 0,
            next_reap: REAP,
        }
    }

    fn heard(&self, side: usize, now: Ms) -> bool {
        slot_heard(&self.slots[side], now)
    }

    fn register(&mut self, now: Ms, side: usize, addr: u64, standby: bool, seq: Option<u32>) -> Option<Answer> {
        if self.refuses_until.is_some_and(|until| now < until) {
            return None;
        }
        let gap = !self.heard(side, now);
        let slot = &mut self.slots[side];
        slot.addr = Some(addr);
        slot.seen = now;
        slot.standby = standby;
        let both = self.slots.iter().all(|s| s.addr.is_some());
        self.ever_both |= both;
        if self.legacy {
            return Some(Answer { ready: self.ever_both, peer_live: None, live_ms: None, relay_id: None, seq: None });
        }
        let other = self.heard(1 - side, now);
        if !other {
            self.live_since = 0;
        } else if self.live_since == 0 || gap {
            self.live_since = now;
        }
        Some(Answer {
            ready: both,
            peer_live: Some(other),
            live_ms: Some(if other { now - self.live_since } else { 0 }),
            relay_id: Some(self.id),
            seq,
        })
    }

    fn reap(&mut self, now: Ms) {
        if now < self.next_reap {
            return;
        }
        self.next_reap = now + REAP;
        let stale = |s: &Slot| s.addr.is_some() && now.saturating_sub(s.seen.max(s.media)) >= IDLE;
        if self.legacy {
            // Only whole sessions go, and only once both sides are quiet.
            if self.slots.iter().all(|s| s.addr.is_none() || stale(s)) {
                self.slots = [Slot::default(); 2];
                self.ever_both = false;
            }
            return;
        }
        for side in 0..2 {
            if stale(&self.slots[side]) {
                self.slots[side] = Slot::default();
                self.live_since = 0;
            }
        }
    }

    fn restart(&mut self, now: Ms) {
        self.up = true;
        self.id += 100;
        self.slots = [Slot::default(); 2];
        self.ever_both = false;
        self.live_since = 0;
        self.next_reap = now + REAP;
    }
}

struct ESock {
    id: u64,
    generation: u64,
    seq: u32,
    sent: VecDeque<(u32, Ms)>,
    next_reg: Ms,
    /// The last refusal noted (the carrier notes one a second).
    refused_noted: Ms,
}

/// An edge after v0.117.0: [`NativeMeet`], acted out.
struct NewEdge {
    rule: NativeMeet,
    socks: [Option<ESock>; 2],
    opening: [Option<Ms>; 2],
    generation: u64,
    next_step: Ms,
    moves: Vec<(Ms, usize, usize, Why)>,
}

/// An edge up to v0.117.0: one socket, on one relay at a time.
struct OldEdge {
    idx: usize,
    sock: Option<u64>,
    opening: Option<Ms>,
    last_ack: Ms,
    next_reg: Ms,
    rotate_at: Option<Ms>,
    moves: Vec<(Ms, usize, usize)>,
}

enum Edge {
    Off,
    New(Box<NewEdge>),
    Old(OldEdge),
}

/// What a scenario is made of.
#[derive(Clone, Copy)]
struct Setup {
    /// Relay addresses the tunnel has.
    addresses: usize,
    /// Both addresses are one relay.
    same_relay: bool,
    /// Which relays are older ones.
    legacy: [bool; 2],
    /// Which edges are older ones.
    old: [bool; 2],
    /// B sends back (SRT's ACKs), or only A sends (raw UDP).
    two_way: bool,
    gate_ms: u32,
}

impl Setup {
    /// Two new relays, two new edges, SRT both ways. The RTT gate is set
    /// wide open (the most validation allows), as the random paths here are
    /// up to 110 ms apart in round trip; `a_far_primary_is_not_returned_to`
    /// runs it closed.
    fn new() -> Self {
        Self { addresses: 2, same_relay: false, legacy: [false; 2], old: [false; 2], two_way: true, gate_ms: 5_000 }
    }
}

struct World {
    base: Instant,
    now: Ms,
    numbers: Numbers,
    setup: Setup,
    relays: Vec<Relay>,
    /// By edge and relay address.
    paths: [[bool; 2]; 2],
    latency: [[Ms; 2]; 2],
    edges: [Edge; 2],
    /// When each edge starts, after it is started.
    starts: [Option<Ms>; 2],
    queue: BinaryHeap<Due>,
    order: u64,
    next_sock: u64,
    /// Relay restarts: when each comes back.
    back_at: [Option<Ms>; 2],
    /// Registers lost on purpose: each with this chance in a thousand.
    register_loss: u64,
    /// The run of Registers lost in a row, per edge and relay address.
    lost_run: [[u32; 2]; 2],
    max_lost_run: u32,
    /// Per tick and direction (A → B, B → A): was the datagram sent delivered.
    flow: [Vec<bool>; 2],
    /// Media datagrams not delivered, per direction, since the counter was reset.
    undelivered: [u64; 2],
}

impl World {
    fn new(seed: u64, setup: Setup) -> Self {
        let mut numbers = Numbers(seed.wrapping_mul(2654435761).wrapping_add(17));
        let mut latency = [[0; 2]; 2];
        for row in &mut latency {
            for l in row.iter_mut() {
                *l = numbers.between(5, 60);
            }
        }
        let relays = (0..if setup.same_relay { 1 } else { setup.addresses })
            .map(|i| Relay::new(1 + i as u64, setup.legacy[i]))
            .collect();
        let mut world = Self {
            base: Instant::now(),
            now: 1_000,
            numbers,
            setup,
            relays,
            paths: [[true; 2]; 2],
            latency,
            edges: [Edge::Off, Edge::Off],
            starts: [None; 2],
            queue: BinaryHeap::new(),
            order: 0,
            next_sock: 1,
            back_at: [None; 2],
            register_loss: 0,
            lost_run: [[0; 2]; 2],
            max_lost_run: 0,
            flow: [Vec::new(), Vec::new()],
            undelivered: [0; 2],
        };
        for edge in [A, B] {
            let phase = world.numbers.below(KEEPALIVE);
            world.start_in(edge, phase);
        }
        world
    }

    /// Both edges up and together on the primary.
    fn up(seed: u64, setup: Setup) -> Self {
        let mut world = Self::new(seed, setup);
        world.run(150_000);
        assert!(world.met_on(PRIMARY), "{}", world.describe());
        world
    }

    fn instant(&self, at: Ms) -> Instant {
        self.base + Duration::from_millis(at)
    }

    fn start_in(&mut self, edge: usize, after: Ms) {
        self.starts[edge] = Some(self.now + after);
    }

    fn stop(&mut self, edge: usize) {
        self.edges[edge] = Edge::Off;
    }

    fn restart_relay(&mut self, relay: usize, down_for: Ms, refuse_binds_for: Option<Ms>) {
        self.relays[relay].up = false;
        self.back_at[relay] = Some(self.now + down_for);
        self.relays[relay].bind_auth = refuse_binds_for.is_some();
        self.relays[relay].refuses_until = refuse_binds_for.map(|t| self.now + down_for + t);
    }

    fn relay_of(&self, address: usize) -> usize {
        if self.setup.same_relay { 0 } else { address }
    }

    fn rand(&mut self, n: u64) -> u64 {
        self.numbers.below(n)
    }

    fn post(&mut self, after: Ms, msg: Msg) {
        self.order += 1;
        self.queue.push(Due { at: self.now + after, order: self.order, msg });
    }

    fn run(&mut self, time: Ms) {
        let until = self.now + time;
        while self.now < until {
            self.tick();
        }
    }

    /// Run until `cond`, at most `at_most`; how long it took, or `None`.
    fn until(&mut self, at_most: Ms, mut cond: impl FnMut(&Self) -> bool) -> Option<Ms> {
        let from = self.now;
        while self.now - from <= at_most {
            if cond(self) {
                return Some(self.now - from);
            }
            self.tick();
        }
        None
    }

    fn active(&self, edge: usize) -> Option<usize> {
        match &self.edges[edge] {
            Edge::New(e) => Some(e.rule.active()),
            Edge::Old(e) => Some(e.idx),
            Edge::Off => None,
        }
    }

    /// Both edges send on one relay, and the media goes through both ways.
    fn met(&self) -> bool {
        let (Some(a), Some(b)) = (self.active(A), self.active(B)) else { return false };
        self.relay_of(a) == self.relay_of(b) && self.flowing()
    }

    fn met_on(&self, address: usize) -> bool {
        self.met() && self.active(A) == Some(address)
    }

    /// The last tick's media got through, both ways if the flow is two-way.
    fn flowing(&self) -> bool {
        let last = |d: usize| self.flow[d].last().copied().unwrap_or(false);
        last(0) && (!self.setup.two_way || last(1))
    }

    fn moves(&self, edge: usize) -> usize {
        match &self.edges[edge] {
            Edge::New(e) => e.moves.len(),
            Edge::Old(e) => e.moves.len(),
            Edge::Off => 0,
        }
    }

    fn new_moves(&self, edge: usize) -> &[(Ms, usize, usize, Why)] {
        match &self.edges[edge] {
            Edge::New(e) => &e.moves,
            _ => &[],
        }
    }

    fn describe(&self) -> String {
        let edge = |e: &Edge| match e {
            Edge::Off => "off".to_string(),
            Edge::New(e) => format!(
                "new on {} socks [{:?}, {:?}] moves {:?}\n  rule {:?}",
                e.rule.active(),
                e.socks[0].as_ref().map(|s| s.id),
                e.socks[1].as_ref().map(|s| s.id),
                e.moves,
                e.rule
            ),
            Edge::Old(e) => format!("old on {} moves {:?}", e.idx, e.moves),
        };
        let relays: Vec<String> = self
            .relays
            .iter()
            .map(|r| format!("up {} slots {:?} live_since {}", r.up, r.slots, r.live_since))
            .collect();
        format!(
            "at {} ms: paths {:?}, flowing {}\nA {}\nB {}\nrelays {:?}",
            self.now,
            self.paths,
            self.flowing(),
            edge(&self.edges[A]),
            edge(&self.edges[B]),
            relays,
        )
    }

    fn tick(&mut self) {
        self.now += TICK;
        let now = self.now;
        for r in 0..self.relays.len() {
            if self.back_at[r].is_some_and(|at| at <= now) {
                self.back_at[r] = None;
                self.relays[r].restart(now);
            }
            if self.relays[r].up {
                self.relays[r].reap(now);
            }
        }
        for edge in [A, B] {
            if self.starts[edge].is_some_and(|at| at <= now) {
                self.starts[edge] = None;
                self.edges[edge] = match self.setup.old[edge] {
                    true => Edge::Old(OldEdge {
                        idx: 0,
                        sock: None,
                        opening: Some(now + 20),
                        last_ack: now,
                        next_reg: now,
                        rotate_at: None,
                        moves: Vec::new(),
                    }),
                    false => {
                        let rule = NativeMeet::new(
                            self.setup.addresses,
                            NativeTiming::default(),
                            self.setup.gate_ms,
                            self.instant(now),
                        );
                        Edge::New(Box::new(NewEdge {
                            rule,
                            socks: [None, None],
                            opening: [None; 2],
                            generation: 0,
                            next_step: now,
                            moves: Vec::new(),
                        }))
                    }
                };
            }
        }
        while self.queue.peek().is_some_and(|d| d.at <= now) {
            let Due { msg, .. } = self.queue.pop().expect("peeked");
            self.deliver(msg);
        }
        for edge in [A, B] {
            match self.edges[edge] {
                Edge::New(_) => self.new_edge_tick(edge),
                Edge::Old(_) => self.old_edge_tick(edge),
                Edge::Off => {}
            }
        }
        let delivered = self.media(A);
        self.flow[0].push(delivered);
        if self.setup.two_way {
            let delivered = self.media(B);
            self.flow[1].push(delivered);
        }
    }

    fn deliver(&mut self, msg: Msg) {
        let now = self.now;
        match msg {
            Msg::Register { address, edge, sock, seq, standby } => {
                if !self.paths[edge][address] {
                    return;
                }
                let r = self.relay_of(address);
                if !self.relays[r].up {
                    let back = self.latency[edge][address];
                    self.post(back, Msg::Refused { address, edge, sock });
                    return;
                }
                if let Some(answer) = self.relays[r].register(now, edge, sock, standby, seq) {
                    let back = self.latency[edge][address];
                    self.post(back, Msg::Ack { address, edge, sock, answer });
                }
            }
            Msg::Ack { address, edge, sock, answer } => {
                if self.paths[edge][address] {
                    self.edge_ack(edge, address, sock, answer);
                }
            }
            Msg::Refused { address, edge, sock } => {
                if self.paths[edge][address] {
                    self.edge_refused(edge, address, sock);
                }
            }
        }
    }

    /// Send one datagram of media from `edge` on the relay it sends on; whether
    /// the other edge got it.
    fn media(&mut self, edge: usize) -> bool {
        let now = self.now;
        let (address, sock) = match &mut self.edges[edge] {
            Edge::New(e) => {
                let a = e.rule.active();
                match e.socks[a].as_ref() {
                    Some(s) => (a, s.id),
                    None => {
                        self.undelivered[edge] += 1;
                        return false;
                    }
                }
            }
            Edge::Old(e) => match e.sock {
                Some(s) => (e.idx, s),
                None => {
                    self.undelivered[edge] += 1;
                    return false;
                }
            },
            Edge::Off => return false,
        };
        let delivered = self.forward(edge, address, sock, now);
        if !delivered {
            self.undelivered[edge] += 1;
        }
        delivered
    }

    fn forward(&mut self, edge: usize, address: usize, sock: u64, now: Ms) -> bool {
        if !self.paths[edge][address] {
            return false;
        }
        let r = self.relay_of(address);
        if !self.relays[r].up {
            // The carrier notes one refusal a second; an older edge ends its
            // forwarder on the first.
            let back = self.latency[edge][address];
            let noted = match &mut self.edges[edge] {
                Edge::New(e) => match e.socks[address].as_mut() {
                    Some(s) if now >= s.refused_noted + 1_000 => {
                        s.refused_noted = now;
                        true
                    }
                    _ => false,
                },
                _ => true,
            };
            if noted {
                self.post(back, Msg::Refused { address, edge, sock });
            }
            return false;
        }
        let relay = &mut self.relays[r];
        if relay.slots[edge].addr != Some(sock) {
            return false;
        }
        relay.slots[edge].media = now;
        let Some(target) = relay.slots[1 - edge].addr else { return false };
        let peer = 1 - edge;
        // Which address of the peer's holds that socket, if it still reads it.
        let held = match &self.edges[peer] {
            Edge::New(e) => (0..2).find(|&i| e.socks[i].as_ref().is_some_and(|s| s.id == target)),
            Edge::Old(e) => (e.sock == Some(target)).then_some(e.idx),
            Edge::Off => None,
        };
        held.is_some_and(|at| self.relay_of(at) == r && self.paths[peer][at])
    }

    fn send_register(&mut self, edge: usize, address: usize, sock: u64, seq: Option<u32>, standby: bool) {
        if self.register_loss > 0 && self.rand(1000) < self.register_loss && self.lost_run[edge][address] < 2 {
            self.lost_run[edge][address] += 1;
            self.max_lost_run = self.max_lost_run.max(self.lost_run[edge][address]);
            return;
        }
        self.lost_run[edge][address] = 0;
        if !self.paths[edge][address] {
            return;
        }
        let after = self.latency[edge][address];
        self.post(after, Msg::Register { address, edge, sock, seq, standby });
    }

    fn edge_ack(&mut self, edge: usize, address: usize, sock: u64, answer: Answer) {
        let at = self.instant(self.now);
        let now = self.now;
        match &mut self.edges[edge] {
            Edge::New(e) => {
                let Some(s) = e.socks[address].as_ref().filter(|s| s.id == sock) else { return };
                let rtt = match answer.seq {
                    Some(seq) => match s.sent.iter().find(|(q, _)| *q == seq) {
                        Some((_, sent)) => Some(Duration::from_millis(now - sent)),
                        None => return,
                    },
                    None => None,
                };
                let view = AckView {
                    ready: answer.ready,
                    peer_live: answer.peer_live,
                    live_ms: answer.live_ms,
                    relay_id: answer.relay_id.map(|id| format!("{id:032x}")),
                    rtt,
                };
                let generation = s.generation;
                e.rule.note_ack(address, generation, at, view);
            }
            Edge::Old(e) => {
                if e.sock == Some(sock) {
                    e.last_ack = now;
                }
                return;
            }
            Edge::Off => return,
        }
        self.new_edge_step(edge);
    }

    fn edge_refused(&mut self, edge: usize, address: usize, sock: u64) {
        let at = self.instant(self.now);
        let now = self.now;
        match &mut self.edges[edge] {
            Edge::New(e) => {
                let Some(generation) = e.socks[address].as_ref().filter(|s| s.id == sock).map(|s| s.generation)
                else {
                    return;
                };
                e.rule.note_refused(address, generation, at);
            }
            Edge::Old(e) => {
                // The forwarder's next receive fails, and the attempt ends.
                if e.sock == Some(sock) && e.rotate_at.is_none() {
                    e.sock = None;
                    e.rotate_at = Some(now + OLD_RETRY);
                }
                return;
            }
            Edge::Off => return,
        }
        self.new_edge_step(edge);
    }

    fn new_edge_tick(&mut self, edge: usize) {
        let now = self.now;
        let at = self.instant(now);
        let mut opened_any = false;
        if let Edge::New(e) = &mut self.edges[edge] {
            for i in 0..2 {
                if e.opening[i].is_some_and(|t| t <= now) {
                    e.opening[i] = None;
                    e.generation += 1;
                    if e.rule.opened(i, e.generation, at) {
                        let id = self.next_sock;
                        self.next_sock += 1;
                        e.socks[i] = Some(ESock {
                            id,
                            generation: e.generation,
                            seq: self.numbers.below(u32::MAX as u64) as u32,
                            sent: VecDeque::with_capacity(4),
                            next_reg: now,
                            refused_noted: 0,
                        });
                        opened_any = true;
                    }
                }
            }
        }
        let due = matches!(&self.edges[edge], Edge::New(e) if e.next_step <= now);
        if due || opened_any {
            if let Edge::New(e) = &mut self.edges[edge]
                && due
            {
                e.next_step += STEP;
            }
            self.new_edge_step(edge);
        }
        // Registers that are due.
        let mut sends = Vec::new();
        if let Edge::New(e) = &mut self.edges[edge] {
            let active = e.rule.active();
            for i in 0..2 {
                let interval = e.rule.register_interval(i, at);
                if let Some(s) = e.socks[i].as_mut()
                    && s.next_reg <= now
                {
                    if s.sent.len() == 4 {
                        s.sent.pop_front();
                    }
                    s.sent.push_back((s.seq, now));
                    sends.push((i, s.id, s.seq, i != active));
                    s.seq = s.seq.wrapping_add(1);
                    s.next_reg = now + interval.as_millis() as u64;
                }
            }
        }
        for (i, id, seq, standby) in sends {
            self.send_register(edge, i, id, Some(seq), standby);
        }
    }

    fn new_edge_step(&mut self, edge: usize) {
        let now = self.now;
        let at = self.instant(now);
        let resolve = self.numbers.between(5, 200);
        let Edge::New(e) = &mut self.edges[edge] else { return };
        for act in e.rule.step(at) {
            match act {
                Act::Open(i) | Act::Redial(i) => e.opening[i] = Some(now + resolve),
                Act::Close(i) => e.socks[i] = None,
                Act::RegisterNow(i) => {
                    if let Some(s) = e.socks[i].as_mut() {
                        s.next_reg = now;
                    }
                }
                Act::Move { from, to, why } => e.moves.push((now, from, to, why)),
                Act::State(_) => {}
            }
        }
    }

    fn old_edge_tick(&mut self, edge: usize) {
        let now = self.now;
        let n = self.setup.addresses;
        let mut send = None;
        if let Edge::Old(e) = &mut self.edges[edge] {
            if e.rotate_at.is_some_and(|t| t <= now) {
                e.rotate_at = None;
                let from = e.idx;
                e.idx = (e.idx + 1) % n;
                e.moves.push((now, from, e.idx));
                e.opening = Some(now + 20);
            }
            if e.opening.is_some_and(|t| t <= now) {
                e.opening = None;
                e.sock = Some(self.next_sock);
                self.next_sock += 1;
                e.last_ack = now;
                e.next_reg = now;
            }
            if let Some(sock) = e.sock
                && e.next_reg <= now
            {
                send = Some((e.idx, sock));
                e.next_reg = now + KEEPALIVE;
                // The watchdog runs on the Register tick.
                if now.saturating_sub(e.last_ack) > OLD_DEAD {
                    e.sock = None;
                    e.rotate_at = Some(now + OLD_RETRY);
                    send = None;
                }
            }
        }
        if let Some((address, sock)) = send {
            self.send_register(edge, address, sock, None, false);
        }
    }
}

fn setups_new() -> Setup {
    Setup::new()
}

/// One-way (raw UDP: only A sends) or two-way (SRT: B's ACKs come back).
fn both_ways() -> [bool; 2] {
    [false, true]
}

const SEEDS: u64 = 500;

/// A path to the primary is cut for 40 s on either side: both edges are on
/// one relay within 35 s, and once the path is back and has stayed up 60 s
/// they return together within 70 s, with nothing lost either way as they cross.
#[test]
fn a_cut_path_is_followed_and_returned_from() {
    for side in [A, B] {
        for seed in 0..SEEDS {
            let mut setup = setups_new();
            setup.two_way = seed % 2 == 0;
            let mut w = World::up(seed, setup);
            w.paths[side][PRIMARY] = false;
            let met = w.until(35_000, |w| w.met_on(BACKUP));
            assert!(met.is_some(), "seed {seed} side {side}: {}", w.describe());
            w.run(40_000 - met.unwrap_or(0));
            w.paths[side][PRIMARY] = true;
            w.undelivered = [0; 2];
            let back = w.until(70_000, |w| w.met_on(PRIMARY) && w.moves(A) == 2 && w.moves(B) == 2);
            assert!(back.is_some(), "seed {seed} side {side}: {}", w.describe());
            assert_eq!(w.undelivered, [0, 0], "seed {seed} side {side}: lost in the crossing");
            for e in [A, B] {
                let m = w.new_moves(e);
                assert!(m[1].0 - m[0].0 >= 60_000, "seed {seed}: failback within 60 s of a move: {m:?}");
                assert_eq!(m[1].3, Why::Failback);
            }
            w.run(200_000);
            assert!(w.met_on(PRIMARY) && w.moves(A) == 2 && w.moves(B) == 2, "{}", w.describe());
        }
    }
}

/// An edge's uplink is out for 30 to 75 s: nobody moves, and the media is
/// back within 6 s of the uplink.
#[test]
fn an_uplink_outage_moves_nothing() {
    for side in [A, B] {
        for outage in [30_000, 45_000, 60_000, 75_000] {
            for seed in 0..SEEDS {
                let mut setup = setups_new();
                setup.two_way = seed % 2 == 0;
                let mut w = World::up(seed * 7 + outage, setup);
                w.paths[side] = [false, false];
                w.run(outage);
                w.paths[side] = [true, true];
                let back = w.until(6_000, |w| w.met_on(PRIMARY));
                assert!(back.is_some(), "seed {seed} side {side} outage {outage}: {}", w.describe());
                w.run(300_000);
                assert_eq!((w.moves(A), w.moves(B)), (0, 0), "seed {seed} side {side}: {}", w.describe());
                assert!(w.met_on(PRIMARY));
            }
        }
    }
}

/// An edge restarts while the pair is on the backup. If the primary is out of
/// its reach the two are together within 12 s; if it is in reach, within
/// 30 s, with the media the restarted edge sends delivered at once.
///
/// 30 s: the peer, which stays on the backup, learns that the restarted edge
/// is gone from there only once the backup has not heard it for 16 s, at its
/// next Register (up to 5 s later), and then settles 8 s.
#[test]
fn an_edge_restarted_on_the_backup_finds_its_peer() {
    for side in [A, B] {
        for primary_reachable in [false, true] {
            for seed in 0..SEEDS {
                let mut w = World::up(seed * 13 + 5, setups_new());
                // Onto the backup: the restarting edge's path to the primary is cut.
                w.paths[side][PRIMARY] = false;
                assert!(w.until(35_000, |w| w.met_on(BACKUP)).is_some(), "{}", w.describe());
                w.run(5_000);
                w.stop(side);
                w.run(500);
                w.paths[side][PRIMARY] = primary_reachable;
                w.start_in(side, 0);
                w.run(TICK);
                let bound = if primary_reachable { 30_000 } else { 12_000 };
                let met = w.until(bound, |w| w.met());
                assert!(
                    met.is_some(),
                    "seed {seed} side {side} reachable {primary_reachable}: {}",
                    w.describe()
                );
                if primary_reachable {
                    // What the restarted edge sends gets through at once: by
                    // its standby on the primary, the peer reads it.
                    let dir = side;
                    let since = w.flow[dir].len() - (met.unwrap_or(0) / TICK) as usize - 1;
                    let first = w.flow[dir][since..].iter().position(|d| *d);
                    assert!(
                        first.is_some_and(|f| (f as u64) * TICK <= 2_000),
                        "seed {seed} side {side}: {first:?} {}",
                        w.describe()
                    );
                }
                w.run(300_000);
                assert!(w.met(), "seed {seed}: {}", w.describe());
            }
        }
    }
}

/// A relay restarts for 1 to 3 s: nobody moves, and the media is back within
/// the restart and 6 s.
#[test]
fn a_relay_restart_moves_nothing() {
    for two_way in both_ways() {
        for seed in 0..SEEDS {
            let mut setup = setups_new();
            setup.two_way = two_way;
            let mut w = World::up(seed * 3 + 1, setup);
            let down = 1_000 + (seed % 21) * 100;
            w.restart_relay(PRIMARY, down, None);
            w.run(down);
            let back = w.until(6_000, |w| w.met_on(PRIMARY));
            assert!(back.is_some(), "seed {seed} down {down}: {}", w.describe());
            w.run(300_000);
            assert_eq!((w.moves(A), w.moves(B)), (0, 0), "seed {seed}: {}", w.describe());
        }
    }
}

/// A relay that comes back refusing binds until it is authorized again: the
/// pair meets on the backup, and returns once the primary takes them.
#[test]
fn a_relay_that_refuses_binds_after_a_restart_is_returned_to() {
    for seed in 0..SEEDS {
        let mut w = World::up(seed * 11 + 3, setups_new());
        w.restart_relay(PRIMARY, 2_000, Some(120_000));
        let met = w.until(60_000, |w| w.met_on(BACKUP));
        assert!(met.is_some(), "seed {seed}: {}", w.describe());
        let back = w.until(260_000, |w| w.met_on(PRIMARY));
        assert!(back.is_some(), "seed {seed}: {}", w.describe());
        w.run(200_000);
        assert!(w.met_on(PRIMARY));
        assert_eq!((w.moves(A), w.moves(B)), (2, 2), "seed {seed}: {}", w.describe());
    }
}

/// Today's rule, against the same relay restart: the sender leaves on its
/// first refused datagram, the receiver of a one-way flow only if one of its
/// Registers fell in the restart, and neither comes back. About half the
/// one-way pairs split for good, as measured (6 of 12 on the bench).
#[test]
fn todays_edges_split_on_a_relay_restart_as_measured() {
    let mut splits = 0;
    let trials = SEEDS;
    for seed in 0..trials {
        let mut setup = setups_new();
        setup.old = [true, true];
        setup.two_way = false;
        let mut w = World::up(seed * 3 + 1, setup);
        let down = 1_000 + (seed % 21) * 100;
        w.restart_relay(PRIMARY, down, None);
        w.run(120_000);
        if !w.met() {
            splits += 1;
        }
    }
    assert!(
        (trials * 3 / 10..=trials * 8 / 10).contains(&splits),
        "{splits} of {trials} split: the model is not today's edge"
    );
    // And the new rule, the same trials: none.
    for seed in 0..trials {
        let mut setup = setups_new();
        setup.two_way = false;
        let mut w = World::up(seed * 3 + 1, setup);
        let down = 1_000 + (seed % 21) * 100;
        w.restart_relay(PRIMARY, down, None);
        w.run(120_000);
        assert!(w.met_on(PRIMARY), "seed {seed}: {}", w.describe());
    }
}

/// Today's rule against a 40 s path cut on the receiver's side: the receiver
/// leaves after 25 s of silence and the sender, still answered by the primary,
/// never follows.
#[test]
fn todays_edges_split_on_a_cut_path() {
    let mut splits = 0;
    for seed in 0..SEEDS {
        let mut setup = setups_new();
        setup.old = [true, true];
        let mut w = World::up(seed, setup);
        w.paths[B][PRIMARY] = false;
        w.run(40_000);
        w.paths[B][PRIMARY] = true;
        w.run(120_000);
        if !w.met() {
            splits += 1;
        }
    }
    assert_eq!(splits, SEEDS, "every such pair splits");
}

/// Five tunnels of one node all ride the node's path cut and come back.
#[test]
fn five_tunnels_on_one_node_all_meet() {
    for seed in 0..SEEDS / 5 {
        // Five tunnels each: 500 tunnels in all.
        let mut tunnels: Vec<World> = (0..5).map(|t| World::up(seed * 5 + t, setups_new())).collect();
        for w in &mut tunnels {
            w.paths[A][PRIMARY] = false;
        }
        for w in &mut tunnels {
            assert!(w.until(35_000, |w| w.met_on(BACKUP)).is_some(), "{}", w.describe());
        }
        for w in &mut tunnels {
            w.run(10_000);
            w.paths[A][PRIMARY] = true;
            assert!(w.until(140_000, |w| w.met_on(PRIMARY)).is_some(), "{}", w.describe());
        }
    }
}

/// A primary that comes and goes: the edges move no more than twice per
/// change, never fail back within 60 s of a move, and are together soon
/// after it stays.
#[test]
fn a_flapping_primary_is_not_chased() {
    for (up, down) in [(5, 5), (30, 30), (40, 20), (20, 40), (90, 30), (7, 60)] {
        for side in [A, B] {
            for seed in 0..SEEDS / 25 {
                let mut w = World::up(seed + up * 100 + down, setups_new());
                let mut changes = 0;
                for _ in 0..20 {
                    w.paths[side][PRIMARY] = false;
                    w.run(down * 1_000);
                    w.paths[side][PRIMARY] = true;
                    w.run(up * 1_000);
                    changes += 2;
                }
                for e in [A, B] {
                    assert!(w.moves(e) <= 2 * (changes + 1), "{up} {down}: {}", w.describe());
                    let m = w.new_moves(e);
                    for pair in m.windows(2) {
                        if pair[1].3 == Why::Failback {
                            assert!(pair[1].0 - pair[0].0 >= 60_000, "{up} {down}: {m:?}");
                        }
                    }
                }
                assert!(w.until(200_000, |w| w.met_on(PRIMARY)).is_some(), "{up} {down}: {}", w.describe());
                w.run(300_000);
                assert!(w.met_on(PRIMARY), "{up} {down}: {}", w.describe());
            }
        }
    }
}

/// One relay at two addresses: an edge that looks for its peer at the second
/// address finds the same relay there, lets go of it and takes its side back,
/// and the two keep one socket each.
#[test]
fn one_relay_at_two_addresses_is_one_relay() {
    for seed in 0..SEEDS {
        let mut setup = setups_new();
        setup.same_relay = true;
        let mut w = World::up(seed, setup);
        // B goes away for 40 s: A looks for it at the second address.
        w.stop(B);
        w.run(40_000);
        w.start_in(B, 0);
        assert!(w.until(40_000, |w| w.met()).is_some(), "seed {seed}: {}", w.describe());
        w.run(300_000);
        assert!(w.met(), "seed {seed}: {}", w.describe());
        let Edge::New(e) = &w.edges[A] else { unreachable!() };
        assert!(e.rule.same_relay_seen(), "seed {seed}: A never looked: {}", w.describe());
        for edge in [A, B] {
            let Edge::New(e) = &w.edges[edge] else { unreachable!() };
            assert_eq!(e.socks.iter().flatten().count(), 1, "seed {seed}: {}", w.describe());
            assert_eq!(w.moves(edge), 0);
        }
    }
}

/// A primary more than the gate slower than the backup: once the pair is on
/// the backup it stays there.
#[test]
fn a_far_primary_is_not_returned_to() {
    for seed in 0..SEEDS {
        let mut setup = setups_new();
        setup.gate_ms = 50;
        let mut w = World::new(seed, setup);
        w.latency[A] = [150, 10];
        w.latency[B] = [10, 10];
        w.run(150_000);
        assert!(w.met_on(PRIMARY), "{}", w.describe());
        w.paths[B][PRIMARY] = false;
        assert!(w.until(35_000, |w| w.met_on(BACKUP)).is_some(), "{}", w.describe());
        w.run(5_000);
        w.paths[B][PRIMARY] = true;
        w.run(600_000);
        assert!(w.met_on(BACKUP), "seed {seed}: {}", w.describe());
        assert!(w.new_moves(A).iter().all(|m| m.3 != Why::Failback), "{}", w.describe());
    }
}

/// A single relay: a relay restart and a path cut are ridden out.
#[test]
fn a_single_relay_is_ridden_through() {
    for seed in 0..SEEDS {
        let mut setup = setups_new();
        setup.addresses = 1;
        let mut w = World::up(seed, setup);
        w.restart_relay(PRIMARY, 2_000, None);
        w.run(2_000);
        assert!(w.until(6_000, |w| w.met()).is_some(), "{}", w.describe());
        w.paths[B][PRIMARY] = false;
        w.run(40_000);
        w.paths[B][PRIMARY] = true;
        assert!(w.until(40_000, |w| w.met()).is_some(), "{}", w.describe());
    }
}

/// A new edge paired with an older one, on new relays: wherever the older
/// one goes, the new one is with it within 35 s.
#[test]
fn a_new_edge_follows_an_old_one() {
    for old in [A, B] {
        for seed in 0..SEEDS {
            let mut setup = setups_new();
            setup.old[old] = true;
            let mut w = World::up(seed, setup);
            // The older edge's path to the primary goes: it rotates after 25 s.
            w.paths[old][PRIMARY] = false;
            assert!(w.until(25_000 + 35_000, |w| w.met_on(BACKUP)).is_some(), "{}", w.describe());
            w.paths[old][PRIMARY] = true;
            // It never returns, and the new one stays with it.
            w.run(300_000);
            assert!(w.met_on(BACKUP), "{}", w.describe());
            // Its backup path goes: it rotates back, and is followed.
            w.paths[old][BACKUP] = false;
            assert!(w.until(25_000 + 35_000, |w| w.met_on(PRIMARY)).is_some(), "{}", w.describe());
        }
    }
}

/// New edges on older relays: no move on a relay restart, a move only after
/// 25 s of silence, and never a failback.
#[test]
fn new_edges_keep_todays_rules_with_old_relays() {
    for seed in 0..SEEDS {
        let mut setup = setups_new();
        setup.legacy = [true, true];
        let mut w = World::up(seed, setup);
        w.restart_relay(PRIMARY, 1_000 + seed % 2_000, None);
        w.run(120_000);
        assert!(w.met_on(PRIMARY), "a restart moves nothing: {}", w.describe());
        assert_eq!((w.moves(A), w.moves(B)), (0, 0));
        // The primary goes for both: a move after 25 s of silence.
        w.paths = [[false, true], [false, true]];
        // The last Ack before the cut came up to a keepalive before it.
        assert!(w.until(19_000, |w| w.moves(A) + w.moves(B) > 0).is_none(), "{}", w.describe());
        assert!(w.until(40_000, |w| w.met_on(BACKUP)).is_some(), "{}", w.describe());
        w.paths = [[true; 2]; 2];
        w.run(600_000);
        assert!(w.met_on(BACKUP), "never a failback: {}", w.describe());
    }
    // One relay older than the other: a move only off a silent relay too.
    for seed in 0..SEEDS {
        let mut setup = setups_new();
        setup.legacy = [false, true];
        let mut w = World::up(seed, setup);
        w.restart_relay(PRIMARY, 2_000, None);
        w.run(120_000);
        assert!(w.met_on(PRIMARY), "{}", w.describe());
        assert_eq!((w.moves(A), w.moves(B)), (0, 0));
    }
}

/// Whatever the paths have been doing, two edges that can meet on a relay for
/// 90 s are together within 45 s and stay so, and move at most twice per
/// change of path.
///
/// Together is the media getting through both ways. A history can leave the
/// two crossed — each sending on a different relay and reading the other's
/// by its standby socket there — which delivers everything and is undone by
/// the failback, once both sides have been heard on the primary for a
/// minute: on one relay within 45 + 65 s.
#[test]
fn edges_meet_after_anything() {
    let mut numbers = Numbers(1);
    for seed in 0..2_000 {
        let mut w = World::up(seed, setups_new());
        let mut changes = 0u64;
        for _ in 0..numbers.between(1, 6) {
            let e = numbers.below(2) as usize;
            let r = numbers.below(2) as usize;
            w.paths[e][r] = !w.paths[e][r];
            changes += 1;
            w.run(numbers.between(1, 90) * 1_000);
        }
        // Settle on paths that leave a relay both can reach.
        let bits = numbers.below(16);
        let paths = [[bits & 1 == 1, bits & 2 == 2], [bits & 4 == 4, bits & 8 == 8]];
        let reachable = [PRIMARY, BACKUP].into_iter().any(|r| paths[A][r] && paths[B][r]);
        if !reachable {
            continue;
        }
        if paths != w.paths {
            changes += 1;
        }
        w.paths = paths;
        // For up to 16 s a relay still counts a side heard that can no longer
        // reach it, and a move made on that is undone within a settle: what
        // the two do in the first 30 s is decided partly by the old paths.
        w.run(30_000);
        let together = w.until(15_000, |w| w.flowing());
        assert!(together.is_some(), "seed {seed}: {}", w.describe());
        let one = w.until(80_000 - together.unwrap_or(0), |w| w.met());
        assert!(one.is_some(), "seed {seed}: {}", w.describe());
        // And stay so: what they send keeps getting through, a failback that
        // crosses the two included.
        let left = w.until(45_000, |w| !w.flowing());
        assert!(left.is_none(), "seed {seed}: {}", w.describe());
        for e in [A, B] {
            assert!(w.moves(e) as u64 <= 2 * (changes + 1), "seed {seed}: {}", w.describe());
        }
    }
}

/// An hour on the primary with Registers lost, at most two in a row: nobody
/// moves.
#[test]
fn lost_registers_move_nothing() {
    for seed in 0..SEEDS / 50 {
        let mut setup = setups_new();
        setup.two_way = seed % 2 == 0;
        let mut w = World::up(seed, setup);
        w.register_loss = 300;
        w.run(3_600_000);
        assert!(w.max_lost_run == 2, "the trial lost no two in a row");
        assert_eq!((w.moves(A), w.moves(B)), (0, 0), "seed {seed}: {}", w.describe());
    }
}
