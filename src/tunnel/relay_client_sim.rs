// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Two edges and two relays in simulated time: whether, how soon and for how
//! long the edges meet, whatever happens to the paths between them and the
//! relays, and whatever their timers happen to be at.
//!
//! What an edge decides is decided by the code itself ([`Walk`], [`OtherRelay`]).
//! What carries it out (`connect_with_retry`, `meet`, `try_other` and
//! `run_relay_tunnel`) is acted out here, and so are the relays.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use super::carrier::DRAIN;
use super::relay_client::RelayTunnelState::{self, Connecting, Ready, Waiting};
use super::relay_client::{OtherRelay, Timing, Walk};

/// Milliseconds.
type Ms = u64;

const TICK: Ms = 50;
/// A QUIC connection that hears nothing ends after this (`quic.rs`).
const IDLE: Ms = 25_000;
/// `run_relay_tunnel` waits this long before it connects anew.
const RECONNECT: Ms = 1_000;

const A: usize = 0;
const B: usize = 1;
const PRIMARY: usize = 0;
const BACKUP: usize = 1;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Said {
    Ready,
    Waiting,
    PeerGone,
    Lost,
}

/// One connection of an edge to a relay. What is sent on it arrives in order,
/// and late if the path was dark.
struct Conn {
    edge: usize,
    relay: usize,
    /// Since when nothing gets through, and whether either end is done with it.
    dark: Option<Ms>,
    ended: bool,
    /// `true` is a bind, `false` an unbind after which the edge is gone.
    to_relay: VecDeque<(Ms, bool)>,
    to_edge: VecDeque<(Ms, Said)>,
}

/// A connection being made.
struct Dial {
    relay: usize,
    until: Ms,
    up_at: Option<Ms>,
}

struct Second {
    conn: Option<usize>,
    dial: Option<Dial>,
    both_since: Option<Ms>,
}

enum Phase {
    Off,
    Idle(Ms),
    Dialling(Dial),
    On(usize),
}

struct Edge {
    on: bool,
    phase: Phase,
    walk: Walk,
    idx: usize,
    state: RelayTunnelState,
    /// Has a forwarder.
    up: bool,
    rule: OtherRelay,
    second: Option<Second>,
    leaving: Vec<(Ms, usize)>,
    /// Whether it finds the primary near enough to go back to.
    near: bool,
    dials: [u32; 2],
    moves: u32,
}

struct World {
    base: Instant,
    now: Ms,
    /// By edge and relay.
    paths: [[bool; 2]; 2],
    latency: [[Ms; 2]; 2],
    conns: Vec<Conn>,
    /// By relay and edge: the connection that holds the side.
    holders: [[Option<usize>; 2]; 2],
    edges: [Edge; 2],
    timing: Timing,
}

impl World {
    fn new(timing: Timing) -> Self {
        let base = Instant::now();
        let edge = || Edge {
            on: false,
            phase: Phase::Off,
            walk: Walk::new(2, PRIMARY),
            idx: PRIMARY,
            state: Connecting,
            up: false,
            rule: OtherRelay::new(false, &timing, base),
            second: None,
            leaving: Vec::new(),
            near: true,
            dials: [0; 2],
            moves: 0,
        };
        let edges = [edge(), edge()];
        Self {
            base,
            now: 0,
            paths: [[true; 2]; 2],
            latency: [[10; 2]; 2],
            conns: Vec::new(),
            holders: [[None; 2]; 2],
            edges,
            timing,
        }
    }

    /// Both edges up on the primary.
    fn up() -> Self {
        let mut world = Self::new(Timing::default());
        world.start(A);
        world.start(B);
        world.run(5_000);
        assert!(world.met_on(PRIMARY));
        world
    }

    /// A is up on the backup and B on the primary, each waiting for the other.
    /// B was told `skew` later than A.
    fn parted(skew: Ms) -> Self {
        let mut world = Self::new(Timing::default());
        for (edge, relay) in [(A, BACKUP), (B, PRIMARY)] {
            world.conns.push(Conn {
                edge,
                relay,
                dark: None,
                ended: false,
                to_relay: VecDeque::new(),
                to_edge: VecDeque::from([(if edge == B { skew } else { 0 }, Said::PeerGone)]),
            });
            let conn = world.conns.len() - 1;
            world.holders[relay][edge] = Some(conn);
            let e = &mut world.edges[edge];
            (e.on, e.up, e.state, e.idx) = (true, true, Ready, relay);
            e.rule = OtherRelay::new(relay != PRIMARY, &world.timing, world.base);
            e.phase = Phase::On(conn);
        }
        world
    }

    fn start(&mut self, edge: usize) {
        let e = &mut self.edges[edge];
        e.on = true;
        e.walk = Walk::new(2, PRIMARY);
        e.idx = PRIMARY;
        e.phase = Phase::Idle(self.now);
    }

    /// Switched off, without a word to anyone.
    fn stop(&mut self, edge: usize) {
        for conn in self.conns.iter_mut().filter(|c| c.edge == edge && !c.ended) {
            conn.dark.get_or_insert(self.now);
            conn.to_relay.clear();
        }
        let e = &mut self.edges[edge];
        e.on = false;
        e.phase = Phase::Off;
        e.second = None;
        e.leaving.clear();
        (e.up, e.state) = (false, Connecting);
    }

    fn instant(&self, at: Ms) -> Instant {
        self.base + Duration::from_millis(at)
    }

    /// The tunnel carries what both edges send.
    fn met_on(&self, relay: usize) -> bool {
        [A, B].into_iter().all(|edge| {
            let e = &self.edges[edge];
            e.up && e.state == Ready && e.idx == relay && self.paths[edge][relay]
        })
    }

    fn met(&self) -> bool {
        self.met_on(PRIMARY) || self.met_on(BACKUP)
    }

    fn run(&mut self, time: Ms) {
        let end = self.now + time;
        while self.now < end {
            self.tick();
        }
    }

    /// How long until the edges have met.
    fn until_met(&mut self, at_most: Ms) -> Ms {
        let start = self.now;
        while !self.met() {
            self.tick();
            assert!(self.now - start <= at_most, "they have not met: {}", self.describe());
        }
        self.now - start
    }

    /// The edges stay met, and the one thing that may happen is a failback.
    fn stays_met(&mut self, time: Ms) {
        let moves = self.edges[A].moves + self.edges[B].moves;
        assert_eq!(self.until_met_for_good(time), 0, "they have parted: {}", self.describe());
        let moved = self.edges[A].moves + self.edges[B].moves - moves;
        assert!(moved <= 2, "they keep moving: {moved}");
        assert!(moved == 0 || self.met_on(PRIMARY), "{}", self.describe());
    }

    /// How long it takes the edges to meet and not part again in this time. That
    /// one hears before the other of what both then do is not parting: nothing
    /// is lost by it.
    fn until_met_for_good(&mut self, time: Ms) -> Ms {
        let start = self.now;
        let (mut last_apart, mut apart) = (start, 0);
        while self.now < start + time {
            self.tick();
            apart = if self.met() { 0 } else { apart + TICK };
            if apart > 700 {
                last_apart = self.now;
            }
        }
        last_apart - start
    }

    fn describe(&self) -> String {
        let edges: Vec<String> = self
            .edges
            .iter()
            .map(|e| format!("{} on {} up {} moves {} dials {:?}", e.state, e.idx, e.up, e.moves, e.dials))
            .collect();
        format!("{} ms {edges:?} paths {:?} latency {:?}", self.now, self.paths, self.latency)
    }

    fn tick(&mut self) {
        self.now += TICK;
        for id in 0..self.conns.len() {
            self.carry(id);
        }
        for edge in [A, B] {
            if self.edges[edge].on {
                self.act(edge);
            }
        }
    }

    /// What arrives on a connection, and its end.
    fn carry(&mut self, id: usize) {
        let now = self.now;
        let conn = &mut self.conns[id];
        if conn.ended {
            return;
        }
        let (edge, relay) = (conn.edge, conn.relay);
        if !self.paths[edge][relay] || !self.edges[edge].on {
            let dark = *conn.dark.get_or_insert(now);
            if now - dark >= IDLE {
                conn.ended = true;
                conn.to_edge.clear();
                conn.to_edge.push_back((now, Said::Lost));
                self.unbind(id);
            }
            return;
        }
        conn.dark = None;
        while let Some(&(at, bind)) = self.conns[id].to_relay.front() {
            if at > now {
                break;
            }
            self.conns[id].to_relay.pop_front();
            if bind {
                self.holders[relay][edge] = Some(id);
                match self.holders[relay][1 - edge] {
                    Some(peer) => {
                        self.say(id, Said::Ready);
                        self.say(peer, Said::Ready);
                    }
                    None => self.say(id, Said::Waiting),
                }
            } else {
                self.conns[id].ended = true;
                self.unbind(id);
            }
        }
    }

    fn unbind(&mut self, id: usize) {
        let (edge, relay) = (self.conns[id].edge, self.conns[id].relay);
        if self.holders[relay][edge] == Some(id) {
            self.holders[relay][edge] = None;
            if let Some(peer) = self.holders[relay][1 - edge] {
                self.say(peer, Said::PeerGone);
            }
        }
    }

    fn say(&mut self, id: usize, what: Said) {
        let conn = &self.conns[id];
        let at = self.now + self.latency[conn.edge][conn.relay];
        self.conns[id].to_edge.push_back((at, what));
    }

    /// What the edge is told on a connection, if anything.
    fn told(&mut self, id: usize) -> Option<Said> {
        let conn = &mut self.conns[id];
        let dark = conn.dark.is_some() && !conn.ended;
        match conn.to_edge.front() {
            Some(&(at, said)) if at <= self.now && !dark => {
                conn.to_edge.pop_front();
                Some(said)
            }
            _ => None,
        }
    }

    fn dial(&mut self, edge: usize, relay: usize) -> Dial {
        self.edges[edge].dials[relay] += 1;
        let until = self.now + self.timing.connect.as_millis() as Ms;
        Dial { relay, until, up_at: None }
    }

    /// `None` while it is not settled. A connection made is bound on.
    fn dialled(&mut self, edge: usize, dial: &mut Dial) -> Option<Option<usize>> {
        if self.paths[edge][dial.relay] {
            let round_trip = 2 * self.latency[edge][dial.relay];
            let up_at = *dial.up_at.get_or_insert(self.now + round_trip);
            if self.now >= up_at {
                let at = self.now + self.latency[edge][dial.relay];
                self.conns.push(Conn {
                    edge,
                    relay: dial.relay,
                    dark: None,
                    ended: false,
                    to_relay: VecDeque::from([(at, true)]),
                    to_edge: VecDeque::new(),
                });
                return Some(Some(self.conns.len() - 1));
            }
        } else {
            dial.up_at = None;
        }
        (self.now >= dial.until).then_some(None)
    }

    fn leave(&mut self, id: usize) {
        let conn = &mut self.conns[id];
        let at = self.now + self.latency[conn.edge][conn.relay];
        conn.to_relay.push_back((at, false));
    }

    fn give_up_second(&mut self, edge: usize) {
        if let Some(Second { conn: Some(conn), .. }) = self.edges[edge].second.take() {
            self.leave(conn);
        }
    }

    fn act(&mut self, edge: usize) {
        let now = self.now;
        let due: Vec<usize> = {
            let leaving = &mut self.edges[edge].leaving;
            let (due, later) = leaving.drain(..).partition(|(at, _)| *at <= now);
            *leaving = later;
            due.into_iter().map(|(_, conn): (Ms, usize)| conn).collect()
        };
        for conn in due {
            self.leave(conn);
        }
        match std::mem::replace(&mut self.edges[edge].phase, Phase::Off) {
            Phase::Idle(until) if now >= until => {
                let dial = self.dial(edge, self.edges[edge].walk.at);
                self.edges[edge].phase = Phase::Dialling(dial);
            }
            Phase::Dialling(mut dial) => match self.dialled(edge, &mut dial) {
                None => self.edges[edge].phase = Phase::Dialling(dial),
                Some(None) => self.walk_on(edge),
                Some(Some(conn)) => {
                    let at = self.instant(now);
                    let e = &mut self.edges[edge];
                    e.idx = dial.relay;
                    e.state = Connecting;
                    e.rule = OtherRelay::new(e.idx != PRIMARY, &self.timing, at);
                    e.phase = Phase::On(conn);
                }
            },
            Phase::On(conn) => {
                self.edges[edge].phase = Phase::On(conn);
                self.act_on(edge, conn);
            }
            other => self.edges[edge].phase = other,
        }
    }

    fn walk_on(&mut self, edge: usize) {
        let e = &mut self.edges[edge];
        let pause = e.walk.step().unwrap_or_default();
        e.phase = Phase::Idle(self.now + pause.as_millis() as Ms);
    }

    fn act_on(&mut self, edge: usize, conn: usize) {
        let at = self.instant(self.now);
        while let Some(said) = self.told(conn) {
            let e = &mut self.edges[edge];
            match said {
                Said::Ready => (e.state, e.up) = (Ready, true),
                Said::Waiting => e.state = Waiting,
                // `meet` does not mind it.
                Said::PeerGone if e.up => e.state = Waiting,
                Said::PeerGone => {}
                Said::Lost => {
                    let was_up = std::mem::take(&mut e.up);
                    e.state = Connecting;
                    self.give_up_second(edge);
                    if was_up {
                        let e = &mut self.edges[edge];
                        e.walk = Walk::new(2, e.idx);
                        e.phase = Phase::Idle(self.now + RECONNECT);
                    } else {
                        self.walk_on(edge);
                    }
                    return;
                }
            }
        }
        let e = &mut self.edges[edge];
        e.rule.set(e.state, at);
        self.look_elsewhere(edge, conn);
    }

    fn look_elsewhere(&mut self, edge: usize, active: usize) {
        let (now, at) = (self.now, self.instant(self.now));
        let other = 1 - self.edges[edge].idx;
        let Some(mut second) = self.edges[edge].second.take() else {
            if self.edges[edge].rule.due().is_some_and(|due| due <= at) {
                let dial = self.dial(edge, other);
                let second = Second { conn: None, dial: Some(dial), both_since: None };
                self.edges[edge].second = Some(second);
            }
            return;
        };
        let near = self.edges[edge].near;
        if let Some(mut dial) = second.dial.take() {
            match self.dialled(edge, &mut dial) {
                None => {
                    second.dial = Some(dial);
                    self.edges[edge].second = Some(second);
                }
                Some(Some(conn)) if self.edges[edge].rule.wanted(at, || near) => {
                    second.conn = Some(conn);
                    self.edges[edge].second = Some(second);
                }
                Some(conn) => {
                    // Closed before the bind has left.
                    if let Some(conn) = conn {
                        self.conns[conn].to_relay.clear();
                        self.conns[conn].ended = true;
                    }
                    self.edges[edge].rule.failed(at);
                }
            }
            return;
        }
        let conn = second.conn.expect("a second connection is being made or is there");
        while let Some(said) = self.told(conn) {
            match said {
                Said::Ready => {
                    second.both_since.get_or_insert(now);
                }
                Said::Waiting | Said::PeerGone => second.both_since = None,
                Said::Lost => {
                    self.edges[edge].rule.failed(at);
                    return;
                }
            }
        }
        let rule = &mut self.edges[edge].rule;
        if !rule.wanted(at, || near) {
            rule.failed(at);
            self.leave(conn);
            return;
        }
        let settle = rule.settle().as_millis() as Ms;
        if second.both_since.is_none_or(|since| now < since + settle) {
            self.edges[edge].second = Some(second);
            return;
        }
        let e = &mut self.edges[edge];
        let drain = if e.up && e.state == Ready { DRAIN.as_millis() as Ms } else { 0 };
        e.leaving.push((now + drain, active));
        e.moves += 1;
        (e.idx, e.state, e.up) = (other, Ready, true);
        e.phase = Phase::On(conn);
        e.rule = OtherRelay::new(other != PRIMARY, &self.timing, at);
        e.rule.set(Ready, at);
    }
}

/// The same numbers on every run.
struct Numbers(u64);

impl Numbers {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }

    fn either(&mut self) -> bool {
        self.below(2) == 1
    }
}

fn paths(bits: u64) -> [[bool; 2]; 2] {
    let bit = |n: u64| bits >> n & 1 == 1;
    [[bit(0), bit(1)], [bit(2), bit(3)]]
}

/// A relay that both edges can reach.
fn can_meet(paths: &[[bool; 2]; 2]) -> bool {
    [PRIMARY, BACKUP].into_iter().any(|relay| paths[A][relay] && paths[B][relay])
}

/// The longest it may take two edges to meet once they can: a minute until one
/// tries again, what it tries first in vain, and 10 s until it tries the next.
const SOON: Ms = 80_000;

/// The same when a connection may just have died, which is not noticed at once.
const LATER: Ms = 100_000;

/// One edge loses its path to the primary, when the other does what it does. It
/// comes to the backup, and the other follows, 10 s after it has been told. Both
/// go back together when the path is back, and not before.
#[test]
fn an_edge_that_loses_its_path_is_followed() {
    for edge in [A, B] {
        for latency in [1, 60, 300] {
            let mut world = World::up();
            world.latency[edge] = [latency; 2];
            world.paths[edge][PRIMARY] = false;
            let out = world.until_met(60_000);
            assert!((IDLE + 10_000..IDLE + 15_000).contains(&out), "{out}");
            assert!(world.met_on(BACKUP));
            let dials = world.edges[1 - edge].dials;
            world.stays_met(3_600_000);
            assert!(world.met_on(BACKUP));
            // It waits on the primary, where it has been once since.
            assert_eq!(world.edges[1 - edge].dials, [dials[PRIMARY] + 1, dials[BACKUP]]);
            assert_eq!(world.edges[1 - edge].moves, 1);

            world.paths[edge][PRIMARY] = true;
            world.stays_met(100_000);
            assert!(world.met_on(PRIMARY));
        }
    }
}

/// An edge that finds the primary too far keeps the other on the backup, and
/// nothing is interrupted for it.
#[test]
fn an_edge_that_does_not_go_back_is_not_left() {
    let mut world = World::up();
    world.edges[A].near = false;
    world.paths[A][PRIMARY] = false;
    world.until_met(60_000);
    world.paths[A][PRIMARY] = true;
    world.stays_met(3_600_000);
    assert!(world.met_on(BACKUP));
    world.edges[A].near = true;
    world.stays_met(100_000);
    assert!(world.met_on(PRIMARY));
}

/// Two edges that start at any time of each other: the later one finds the
/// earlier one where it looks first.
#[test]
fn edges_meet_whenever_they_start() {
    for apart in (0..200_000).step_by(250) {
        let mut world = World::new(Timing::default());
        world.start(A);
        world.run(apart + TICK);
        world.start(B);
        assert!(world.until_met(1_000) <= 500);
        assert!(world.met_on(PRIMARY));
        world.stays_met(20_000);
    }
}

/// The peer is off for hours. The edge that is left waits, and hardly tries
/// anything, and they meet as soon as the peer is back.
#[test]
fn an_edge_alone_is_quiet_and_is_found() {
    for (dark, most) in [(None, [0, 1]), (Some(BACKUP), [0, 200]), (Some(PRIMARY), [200, 1])] {
        for back in (0..70_000).step_by(1_750) {
            let mut world = World::up();
            if let Some(relay) = dark {
                world.paths[A][relay] = false;
            }
            world.stop(B);
            world.run(60_000);
            let dials = world.edges[A].dials;
            world.run(3 * 3_600_000 + back);
            assert!(world.edges[A].dials[PRIMARY] <= dials[PRIMARY] + most[PRIMARY], "{}", world.describe());
            assert!(world.edges[A].dials[BACKUP] <= dials[BACKUP] + most[BACKUP], "{}", world.describe());
            world.start(B);
            // On the backup it is looked for after 10 s.
            let out = world.until_met(60_000);
            assert!(out <= if dark == Some(PRIMARY) { 12_000 } else { 500 }, "{out}: {}", world.describe());
            world.stays_met(20_000);
        }
    }
}

/// Each edge has a path to the other's relay only, and then the paths are back,
/// at any point of the edges' timers. They do not go past each other.
#[test]
fn edges_do_not_go_past_each_other() {
    for latency in [[1, 1], [20, 20], [200, 200], [10, 250]] {
        for ms in (0..130_000).step_by(650) {
            let mut world = World::up();
            world.latency = latency.map(|ms| [ms; 2]);
            world.paths = [[false, true], [true, false]];
            world.run(120_000 + ms);
            assert!(!world.met());
            assert_eq!([world.edges[A].idx, world.edges[B].idx], [BACKUP, PRIMARY]);
            let moves = world.edges[A].moves + world.edges[B].moves;
            world.paths = [[true; 2]; 2];
            let met = world.until_met_for_good(200_000);
            assert!(met <= SOON, "{met}: {}", world.describe());
            let moved = world.edges[A].moves + world.edges[B].moves - moves;
            assert!(moved <= 3, "{moved}: {}", world.describe());
        }
    }
}

/// The same when they were told that they are parted at about the same time, and
/// so look for each other at the same time, again and again: they may miss each
/// other once.
#[test]
fn edges_that_look_at_the_same_time_meet() {
    for latency in [1, 10, 40, 100, 250, 400] {
        for other_latency in [1, 25, 70, 160, 400] {
            for skew in (0..2_000).step_by(50) {
                for near in [true, false] {
                    let mut world = World::parted(skew);
                    world.latency = [[latency, other_latency], [other_latency, latency]];
                    world.edges[A].near = near;
                    let met = world.until_met_for_good(100_000);
                    assert!(met <= 26_000, "{met}: {}", world.describe());
                    let moved = world.edges[A].moves + world.edges[B].moves;
                    assert!(moved <= 4, "{moved}: {}", world.describe());
                }
            }
        }
    }
}

/// The same of edges that connect.
#[test]
fn edges_that_connect_at_the_same_time_meet() {
    for latency in (1..400).step_by(13) {
        for other_latency in (1..400).step_by(57) {
            for later in [0, TICK, 3 * TICK, 10 * TICK] {
                let mut world = World::new(Timing::default());
                world.latency = [[latency; 2], [other_latency; 2]];
                world.paths = [[false, true], [true, false]];
                // The one that has a path to its primary waits there at once.
                world.start(A);
                world.run(6_000 + later);
                world.start(B);
                world.run(10_000 - later + 2 * latency.max(other_latency));
                world.paths = [[true; 2]; 2];
                let met = world.until_met_for_good(100_000);
                assert!(met <= 15_000, "{met}: {}", world.describe());
                let moved = world.edges[A].moves + world.edges[B].moves;
                assert!(moved <= 3, "{moved}: {}", world.describe());
            }
        }
    }
}

/// Whatever the paths have been doing, two edges that can meet do, soon, and
/// stay. Two that cannot are quiet.
#[test]
fn edges_meet_after_anything() {
    let mut numbers = Numbers(1);
    let (mut longest, mut alone) = (0, 0);
    for _ in 0..600 {
        let mut world = World::new(Timing::default());
        for edge in [A, B] {
            world.edges[edge].near = numbers.below(4) > 0;
            for relay in [PRIMARY, BACKUP] {
                world.latency[edge][relay] = 1 + numbers.below(300);
            }
        }
        world.start(A);
        world.run(numbers.below(120_000));
        world.start(B);
        for _ in 0..numbers.below(12) {
            match numbers.below(5) {
                0 => {
                    let edge = numbers.below(2) as usize;
                    world.stop(edge);
                    world.run(numbers.below(40_000));
                    world.start(edge);
                }
                _ => world.paths = paths(numbers.below(16)),
            }
            let most = if numbers.either() { 20_000 } else { 200_000 };
            world.run(numbers.below(most));
        }
        world.paths = paths(numbers.below(16));
        if can_meet(&world.paths) {
            let met = world.until_met_for_good(300_000);
            assert!(met <= LATER, "{met}: {}", world.describe());
            longest = longest.max(met);
            world.stays_met(300_000);
        } else {
            alone += 1;
            world.run(LATER);
            let dials = [world.edges[A].dials, world.edges[B].dials];
            world.run(600_000);
            for (edge, dials) in dials.into_iter().enumerate() {
                for relay in [PRIMARY, BACKUP] {
                    let dialled = world.edges[edge].dials[relay] - dials[relay];
                    assert!(dialled <= 15, "{dialled}: {}", world.describe());
                }
            }
            assert!(!world.met());
        }
    }
    // It is not that nothing happened to them.
    assert!(longest > SOON / 2 && alone > 100, "{longest} {alone}");
}

/// A path that comes and goes: the edges follow it no faster than it is worth,
/// and have met soon after it stays.
#[test]
fn edges_do_not_chase_a_path_that_flaps() {
    for (up, down) in [(5, 5), (30, 30), (40, 20), (20, 40), (90, 30), (7, 60)] {
        for edge in [A, B] {
            let mut world = World::up();
            for _ in 0..40 {
                world.paths[edge][PRIMARY] = false;
                world.run(down * 1_000);
                world.paths[edge][PRIMARY] = true;
                world.run(up * 1_000);
            }
            // One failback in a minute, and one way back from it.
            let minutes = 40 * (up + down) / 60 + 1;
            for e in &world.edges {
                assert!(u64::from(e.moves) <= 2 * minutes, "{up} {down}: {}", world.describe());
            }
            let met = world.until_met_for_good(600_000);
            assert!(met <= LATER, "{met}: {}", world.describe());
            assert!(world.met_on(PRIMARY));
            world.stays_met(600_000);
        }
    }
}
