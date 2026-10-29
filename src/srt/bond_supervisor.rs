// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Member supervision for native libsrt SRT socket-group bonding.
//!
//! libsrt removes a broken group member and leaves bringing it back to the
//! application. Until this module the edge never did, so a bonded SRT caller
//! that lost one link ran on the rest with no redundancy, silently, until
//! every link failed and the whole group was redialled (issue #203).
//!
//! - [`plan_bond_members`] turns a `bonding` block into one
//!   [`BondMemberPlan`] per endpoint: its token (the config index), target,
//!   source and libsrt weight.
//! - [`resolve_member_source`] decides each member's source address on
//!   every connect and re-add, so a NIC named by `interface_binding` is
//!   re-resolved and a changed DHCP / cellular address is followed (#184).
//! - [`BondSupervisor`] runs on the caller group's 1 s stats tick: it
//!   re-adds a member that is missing or Broken, with backoff, while at
//!   least one other member is Idle or Running, and raises
//!   `srt_bond_member_down` / `srt_bond_member_restored`. With **no** member
//!   live it does nothing: that is the whole-group teardown and redial the
//!   caller loops already run (a member re-added then would keep libsrt from
//!   ever declaring the group failed — see `SrtGroup::add_member`).
//! - [`BondHealthTracker`] is the listener-side observer: a listener cannot
//!   re-add a member (only the remote caller can), but it reports running
//!   below the most members the session has had.
//!
//! Configured weights keep the edge's documented contract, **lower is
//! preferred**, and are mapped to libsrt's (greater is preferred) by an exact
//! dense rank; stats keep showing the configured value.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use srt_protocol::config::{MemberStatus, SocketStatus};
use srt_protocol::error::SrtError;
use srt_transport::{GroupEndpoint, GroupMemberStats, SrtGroup};
use tokio::task::JoinHandle;

use crate::config::models::{InterfaceBinding, SrtBondingConfig, SrtBondingMode};
use crate::manager::events::{Event, EventSender, EventSeverity, category};

/// First re-add backoff for a down member; doubles per attempt.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// Re-add backoff cap.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Floor for how long a member must be down before the group is reported
/// as running below its configured member count.
const MIN_DEGRADE_AFTER: Duration = Duration::from_secs(5);
/// libsrt's own connect timeout when `connect_timeout_secs` is unset.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 3;

/// How long a member may stay down before `srt_bond_member_down` fires: one
/// connect timeout plus 2 s of slack (a member that is still handshaking at
/// start is not "down"), never less than 5 s.
pub(crate) fn degrade_after(connect_timeout_secs: Option<u64>) -> Duration {
    let grace = Duration::from_secs(connect_timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS) + 2);
    grace.max(MIN_DEGRADE_AFTER)
}

// ── Plans ────────────────────────────────────────────────────────────────

/// One bonding endpoint, ready to be (re)connected as a group member.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BondMemberPlan {
    /// libsrt member token: the endpoint's index in `bonding.endpoints`.
    pub(crate) token: i32,
    pub(crate) addr: SocketAddr,
    /// `local_addr` from the config. Wins over `interface_binding`.
    pub(crate) explicit_local: Option<SocketAddr>,
    pub(crate) binding: Option<InterfaceBinding>,
    /// The weight handed to libsrt (greater is preferred).
    pub(crate) srt_weight: u16,
    /// The configured weight (lower is preferred), shown in stats. 0 in
    /// broadcast mode, which has no priority.
    pub(crate) configured_weight: u16,
}

/// Plan every endpoint of a caller-side bonding block. Validation already
/// guarantees both addresses parse; an error here means an unvalidated
/// config reached the engine.
pub(crate) fn plan_bond_members(bond: &SrtBondingConfig) -> Result<Vec<BondMemberPlan>> {
    let configured: Vec<u16> = bond
        .endpoints
        .iter()
        .map(|ep| match bond.mode {
            SrtBondingMode::Backup => ep.weight.unwrap_or(0),
            SrtBondingMode::Broadcast => 0,
        })
        .collect();
    let srt_weights = srt_weights_for(&configured);
    bond.endpoints
        .iter()
        .enumerate()
        .map(|(i, ep)| {
            let addr: SocketAddr = ep
                .addr
                .parse()
                .with_context(|| format!("parse bonding endpoint '{}'", ep.addr))?;
            let explicit_local = ep
                .local_addr
                .as_deref()
                .map(|la| {
                    la.parse::<SocketAddr>()
                        .with_context(|| format!("parse bonding endpoint local_addr '{la}'"))
                })
                .transpose()?;
            Ok(BondMemberPlan {
                token: i as i32,
                addr,
                explicit_local,
                binding: ep.interface_binding.clone(),
                srt_weight: srt_weights[i],
                configured_weight: configured[i],
            })
        })
        .collect()
}

/// Map configured weights (lower is preferred) to libsrt weights (greater is
/// preferred) by dense rank: the lowest configured value gets the highest
/// libsrt weight, equal values stay equal, all-equal is all 0 (libsrt
/// picks). Exact for every `u16` input, and never above `k - 1` for `k`
/// distinct values — far inside libsrt's 0..=32767.
pub(crate) fn srt_weights_for(configured: &[u16]) -> Vec<u16> {
    let mut distinct = configured.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let top = distinct.len().saturating_sub(1);
    configured
        .iter()
        .map(|c| {
            let rank = distinct.binary_search(c).unwrap_or(0);
            ((top - rank) as u16).min(srt_transport::MAX_GROUP_MEMBER_WEIGHT)
        })
        .collect()
}

// ── Sources ──────────────────────────────────────────────────────────────

/// What a member's source address resolves to for one connect attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberSource {
    /// Bind this source address.
    Bound(SocketAddr),
    /// Leave the member unbound. `Some(reason)` when a configured source
    /// could not be applied and the member connects unbound anyway.
    Unbound(Option<String>),
    /// The member cannot be prepared right now (its NIC is missing): skip
    /// it this attempt and try again on the next.
    Unavailable(String),
}

impl MemberSource {
    fn describe(&self) -> String {
        match self {
            MemberSource::Bound(a) => a.to_string(),
            MemberSource::Unbound(_) => "unbound".to_string(),
            MemberSource::Unavailable(e) => format!("unavailable: {e}"),
        }
    }
}

/// Decide a member's source, re-resolving `interface_binding` each time.
///
/// An explicit `local_addr` wins (as for every SRT surface — see
/// `srt_local_addr_from_binding`); `0.0.0.0:0` / `[::]:0` means unbound.
/// A source of the other address family than the peer would make libsrt
/// refuse the whole group connect, so it is dropped (unbound, with the
/// reason) rather than applied.
pub(crate) fn resolve_member_source(plan: &BondMemberPlan) -> MemberSource {
    if let Some(local) = plan.explicit_local {
        if local.ip().is_unspecified() && local.port() == 0 {
            return MemberSource::Unbound(None);
        }
        if local.is_ipv4() != plan.addr.is_ipv4() {
            return MemberSource::Unbound(Some(format!(
                "local_addr {local} is not the same address family as the peer {}",
                plan.addr
            )));
        }
        return MemberSource::Bound(local);
    }
    let Some(binding) = plan.binding.as_ref() else {
        return MemberSource::Unbound(None);
    };
    match crate::util::socket::srt_local_addr_from_binding(Some(binding), None) {
        Err(e) => MemberSource::Unavailable(format!("{e:#}")),
        Ok(None) => MemberSource::Unbound(Some(format!(
            "interface '{}' has no usable IPv4 address",
            binding.name
        ))),
        Ok(Some(s)) => match s.parse::<SocketAddr>() {
            Ok(sa) if sa.is_ipv4() == plan.addr.is_ipv4() => MemberSource::Bound(sa),
            Ok(sa) => MemberSource::Unbound(Some(format!(
                "interface '{}' resolved to {}, not the address family of the peer {}",
                binding.name,
                sa.ip(),
                plan.addr
            ))),
            Err(_) => MemberSource::Unbound(Some(format!(
                "interface '{}' resolved to an unusable address '{s}'",
                binding.name
            ))),
        },
    }
}

/// The libsrt member for `plan` with `source`, or `None` when the member
/// cannot be prepared this attempt.
pub(crate) fn group_endpoint(plan: &BondMemberPlan, source: &MemberSource) -> Option<GroupEndpoint> {
    let local_addr = match source {
        MemberSource::Bound(a) => Some(*a),
        MemberSource::Unbound(_) => None,
        MemberSource::Unavailable(_) => return None,
    };
    Some(GroupEndpoint {
        addr: plan.addr,
        local_addr,
        weight: plan.srt_weight,
        token: plan.token,
    })
}

/// The members for one whole-group connect attempt. Members whose source
/// is unavailable are skipped (the supervisor re-adds them once the group
/// is up); if none can be prepared the attempt fails, through the caller's
/// usual observer and backoff.
pub(crate) fn bond_attempt_endpoints(plans: &[BondMemberPlan]) -> Result<Vec<GroupEndpoint>> {
    let mut endpoints = Vec::with_capacity(plans.len());
    let mut skipped = Vec::new();
    for plan in plans {
        let source = resolve_member_source(plan);
        match group_endpoint(plan, &source) {
            Some(ep) => endpoints.push(ep),
            None => skipped.push(format!(
                "endpoint[{}] {}: {}",
                plan.token,
                plan.addr,
                source.describe()
            )),
        }
    }
    if endpoints.is_empty() {
        bail!("no bonding member could be prepared: {}", skipped.join("; "));
    }
    for s in &skipped {
        tracing::warn!("SRT bonding: member skipped for this connect attempt — {s}");
    }
    Ok(endpoints)
}

/// Each member's index, target and source as of now, for event details.
pub(crate) fn member_sources_json(plans: &[BondMemberPlan]) -> serde_json::Value {
    serde_json::Value::Array(
        plans
            .iter()
            .map(|p| {
                serde_json::json!({
                    "index": p.token,
                    "addr": p.addr.to_string(),
                    "source": resolve_member_source(p).describe(),
                })
            })
            .collect(),
    )
}

/// A libsrt error from connecting members bound to `sources`, worded for
/// the operator. libsrt reports a source it cannot open or bind as
/// `SocketFail` ("unable to create/configure SRT socket"), naming neither
/// the step nor the address. An address this host does not have (a typo, a
/// static IP the DHCP server or carrier has since changed) is the usual
/// cause once `local_addr` is really bound; a fixed source port already in
/// use, or the process out of file descriptors, reads the same.
///
/// A whole-group connect fails only when libsrt spawned no member, and the
/// bridge returns the first member error, so there `sources` is every bound
/// member of the attempt: each one failed, though not necessarily for this
/// reason.
pub(crate) fn describe_connect_error(e: &SrtError, sources: &[SocketAddr]) -> String {
    if *e != SrtError::SocketFail || sources.is_empty() {
        return e.to_string();
    }
    let list: Vec<String> = sources.iter().map(|a| a.to_string()).collect();
    let port = if sources.iter().any(|a| a.port() != 0) { ", with its port free" } else { "" };
    format!(
        "could not open or bind a socket on source {} — is {} assigned to this host{port}? ({e})",
        list.join(", "),
        if sources.len() == 1 { "that address" } else { "each address" }
    )
}

/// Why a member handed to libsrt never went live: its handshake went
/// unanswered or the peer rejected it. The group listing carries no reject
/// reason, so the two read alike. Unanswered from a bound source is usually
/// routing (the kernel sends by its table, not by the source address); a
/// rejection is the peer's settings (passphrase, stream id).
fn handshake_failed_reason(source: &str) -> String {
    if source.parse::<SocketAddr>().is_ok() {
        format!(
            "the handshake did not complete from source {source} (no answer, or the peer \
             rejected it): check that the peer is reachable from that source (without source \
             policy routing the kernel may send it out of another interface) and that it \
             accepts this caller (passphrase, stream id)"
        )
    } else {
        "the handshake did not complete (no answer, or the peer rejected it): check that the \
         peer is reachable on this path and that it accepts this caller (passphrase, stream id)"
            .to_string()
    }
}

/// Spawn-time check of every bonding endpoint's `interface_binding`: strict
/// mode is refused and a missing NIC aborts the input/output, exactly as
/// before. The resolved address is **not** written into the config any
/// more — it is resolved again on every connect and re-add. An explicit
/// `local_addr` still wins, so a missing NIC alongside one is not an error.
pub(crate) fn check_bond_endpoint_bindings(bond: &SrtBondingConfig) -> Result<()> {
    for ep in &bond.endpoints {
        crate::util::socket::srt_local_addr_from_binding(
            ep.interface_binding.as_ref(),
            ep.local_addr.as_deref(),
        )?;
    }
    Ok(())
}

// ── Events ───────────────────────────────────────────────────────────────

/// Who a bonding event belongs to.
#[derive(Debug, Clone)]
pub(crate) enum BondEventScope {
    Input { flow_id: String, input_id: String },
    Output { output_id: String },
}

/// Emits the bonding member events for one bonded input or output.
#[derive(Clone)]
pub(crate) struct BondEvents {
    scope: BondEventScope,
    sender: EventSender,
    bonding_mode: SrtBondingMode,
    /// `"caller-bonded"` or `"listener-bonded"`, as on the connect events.
    mode: &'static str,
}

impl BondEvents {
    pub(crate) fn new(
        scope: BondEventScope,
        sender: EventSender,
        bonding_mode: SrtBondingMode,
        mode: &'static str,
    ) -> Self {
        Self { scope, sender, bonding_mode, mode }
    }

    fn subject(&self) -> String {
        match &self.scope {
            BondEventScope::Input { input_id, .. } => format!("SRT bonded input '{input_id}'"),
            BondEventScope::Output { output_id } => format!("SRT bonded output '{output_id}'"),
        }
    }

    fn emit(&self, severity: EventSeverity, message: String, mut details: serde_json::Value) {
        if let Some(map) = details.as_object_mut() {
            map.insert("mode".into(), serde_json::json!(self.mode));
            map.insert("bonding_mode".into(), serde_json::json!(format!("{:?}", self.bonding_mode)));
        }
        match &self.scope {
            BondEventScope::Input { flow_id, input_id } => {
                if let Some(map) = details.as_object_mut() {
                    map.insert("input_id".into(), serde_json::json!(input_id));
                }
                self.sender.send(Event {
                    severity,
                    category: category::SRT.to_string(),
                    message,
                    details: Some(details),
                    flow_id: Some(flow_id.clone()),
                    input_id: Some(input_id.clone()),
                    output_id: None,
                });
            }
            BondEventScope::Output { output_id } => {
                if let Some(map) = details.as_object_mut() {
                    map.insert("output_id".into(), serde_json::json!(output_id));
                }
                self.sender.emit_output_with_details(
                    severity,
                    category::SRT,
                    message,
                    output_id,
                    details,
                );
            }
        }
    }
}

// ── Caller-side supervisor ───────────────────────────────────────────────

/// What one stats tick saw of one listed group member.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MemberObs {
    pub(crate) token: i32,
    pub(crate) member: MemberStatus,
    pub(crate) socket: SocketStatus,
}

/// The supervisor's view of a group listing.
pub(crate) fn observe_members(members: &[GroupMemberStats]) -> Vec<MemberObs> {
    members
        .iter()
        .map(|m| MemberObs { token: m.token, member: m.member_status, socket: m.socket_status })
        .collect()
}

fn socket_terminal(s: SocketStatus) -> bool {
    matches!(
        s,
        SocketStatus::Broken | SocketStatus::Closing | SocketStatus::Closed | SocketStatus::NonExist
    )
}

fn obs_healthy(o: &MemberObs) -> bool {
    matches!(o.member, MemberStatus::Idle | MemberStatus::Running) && !socket_terminal(o.socket)
}

/// Still handshaking: never re-added (one handshake per endpoint). Safe to
/// wait on because it ends: libsrt fails a member whose handshake outlives
/// the connect timeout, and a member libsrt leaves Pending *after* its
/// handshake finished (a race in libsrt's own group connect) is replaced by
/// `srt-transport` within about a second, with the same endpoint. When it
/// was the group's only member up, it is closed only once its replacement
/// is up, so the listing briefly shows two members with this token, and
/// `decide` reads them together.
fn obs_pending(o: &MemberObs) -> bool {
    o.member == MemberStatus::Pending && !socket_terminal(o.socket)
}

/// State that outlives one connected group: the caller loop owns it, so a
/// whole-group reconnect neither repeats a warning nor forgets that an
/// `srt_bond_member_down` still needs its `srt_bond_member_restored`.
#[derive(Debug, Default)]
pub(crate) struct BondLatch {
    degraded: AtomicBool,
    /// One bit per member token: its `srt_bond_source_not_applied` fired.
    source_warned: AtomicU64,
}

impl BondLatch {
    /// `true` the first time for `token`. Tokens past 63 (validation caps
    /// bonding at 8 endpoints) are never latched.
    fn first_source_warning(&self, token: i32) -> bool {
        match u32::try_from(token).ok().and_then(|t| 1u64.checked_shl(t)) {
            Some(bit) => self.source_warned.fetch_or(bit, Ordering::Relaxed) & bit == 0,
            None => true,
        }
    }
}

#[derive(Debug, Clone)]
struct MemberTrack {
    down_since: Option<Instant>,
    next_attempt: Instant,
    backoff: Duration,
    /// A connect or re-add of this member was handed to libsrt and its
    /// handshake has not been seen to finish either way.
    awaiting_handshake: bool,
    /// Seen Idle or Running at least once in this group.
    was_live: bool,
    last_error: Option<String>,
    /// The source the member last tried to connect from.
    source: String,
}

/// One member that is not Idle or Running, as reported in
/// `srt_bond_member_down`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DownMember {
    pub(crate) token: i32,
    pub(crate) down_for: Duration,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BondAction {
    /// Connect this member token into the live group again.
    Readd(i32),
    /// The group has run below its configured member count for at least
    /// `degrade_after`, with at least one member still live.
    Degraded { connected: usize, configured: usize, down: Vec<DownMember> },
    /// Every configured member is live again after a `Degraded`.
    Restored { configured: usize },
}

/// Re-adds missing or broken members of one connected caller group and
/// reports redundancy loss. Lives as long as the group: the caller loop
/// builds a new one per connect, sharing one [`BondLatch`].
pub(crate) struct BondSupervisor {
    plans: Vec<BondMemberPlan>,
    tracks: Vec<MemberTrack>,
    latch: Arc<BondLatch>,
    events: BondEvents,
    degrade_after: Duration,
    sources_reported: bool,
}

impl BondSupervisor {
    /// `started` is when the group connect returned: every member was just
    /// handed to libsrt, and the first re-add of one that is already gone
    /// waits [`FIRST_BACKOFF`] from then.
    pub(crate) fn new(
        plans: Vec<BondMemberPlan>,
        events: BondEvents,
        latch: Arc<BondLatch>,
        degrade_after: Duration,
        started: Instant,
    ) -> Self {
        let tracks = plans
            .iter()
            .map(|plan| {
                // As the connect that built this group resolved it a moment
                // ago, so a group that fails before the first tick still
                // reports it. A member whose NIC was missing was left out of
                // that connect: it is not handshaking, and the NIC is its
                // reason (as `note_source` records on the first tick).
                let source = resolve_member_source(plan);
                let unavailable = match &source {
                    MemberSource::Unavailable(e) => Some(e.clone()),
                    _ => None,
                };
                MemberTrack {
                    down_since: None,
                    next_attempt: started + FIRST_BACKOFF,
                    backoff: FIRST_BACKOFF,
                    awaiting_handshake: unavailable.is_none(),
                    was_live: false,
                    last_error: unavailable,
                    source: source.describe(),
                }
            })
            .collect();
        Self { plans, tracks, latch, events, degrade_after, sources_reported: false }
    }

    pub(crate) fn plans(&self) -> &[BondMemberPlan] {
        &self.plans
    }

    /// The pure half of a tick: from what the group lists, which members to
    /// re-add and which redundancy event to raise.
    ///
    /// A token is **live** if any member carrying it is Idle or Running on a
    /// non-terminal socket, **pending** if one is still handshaking. A
    /// pending token is never re-added, so there is at most one handshake
    /// per endpoint in flight. Everything else — absent, or listed Broken
    /// while libsrt drains it — is down, and is re-added once its backoff
    /// has run out **and at least one token is live**. With none live the
    /// group is failing as a whole, which the caller loop's redial handles.
    pub(crate) fn decide(&mut self, view: &[MemberObs], now: Instant) -> Vec<BondAction> {
        let state: Vec<(bool, bool)> = self
            .plans
            .iter()
            .map(|plan| {
                let mine = || view.iter().filter(|o| o.token == plan.token);
                let live = mine().any(obs_healthy);
                (live, !live && mine().any(obs_pending))
            })
            .collect();
        let configured = self.plans.len();
        let connected = state.iter().filter(|(live, _)| *live).count();

        let mut actions = Vec::new();
        for ((plan, track), (live, pending)) in self.plans.iter().zip(&mut self.tracks).zip(state) {
            if live {
                // Reset: the next loss is re-added as soon as it is seen.
                track.down_since = None;
                track.next_attempt = now;
                track.backoff = FIRST_BACKOFF;
                track.awaiting_handshake = false;
                track.was_live = true;
                track.last_error = None;
                continue;
            }
            track.down_since.get_or_insert(now);
            if pending {
                continue;
            }
            if track.awaiting_handshake {
                // Handed to libsrt, never seen live, and now gone.
                track.awaiting_handshake = false;
                track.last_error = Some(handshake_failed_reason(&track.source));
            }
            if connected > 0 && now >= track.next_attempt {
                actions.push(BondAction::Readd(plan.token));
                track.next_attempt = now + track.backoff;
                track.backoff = (track.backoff * 2).min(MAX_BACKOFF);
            }
        }

        if connected > 0 && connected < configured {
            let longest = self
                .tracks
                .iter()
                .filter_map(|t| t.down_since)
                .map(|since| now.saturating_duration_since(since))
                .max()
                .unwrap_or_default();
            if longest >= self.degrade_after && !self.latch.degraded.swap(true, Ordering::Relaxed) {
                let down = self
                    .plans
                    .iter()
                    .zip(&self.tracks)
                    .filter_map(|(plan, t)| {
                        t.down_since.map(|since| DownMember {
                            token: plan.token,
                            down_for: now.saturating_duration_since(since),
                            last_error: t.last_error.clone(),
                        })
                    })
                    .collect();
                actions.push(BondAction::Degraded { connected, configured, down });
            }
        } else if connected == configured && self.latch.degraded.swap(false, Ordering::Relaxed) {
            actions.push(BondAction::Restored { configured });
        }
        actions
    }

    /// Record the source a member is about to (re)connect from, raising the
    /// one-shot `srt_bond_source_not_applied` when a configured one cannot
    /// be applied.
    fn note_source(&mut self, idx: usize, source: &MemberSource) {
        let plan = &self.plans[idx];
        self.tracks[idx].source = source.describe();
        match source {
            MemberSource::Unbound(Some(why)) if self.latch.first_source_warning(plan.token) => {
                tracing::warn!(
                    "{}: bonding member {} ({}) connects unbound — {why}",
                    self.events.subject(),
                    plan.token,
                    plan.addr
                );
                self.events.emit(
                    EventSeverity::Warning,
                    format!(
                        "{}: bonding member {} ({}) connects without its configured source — {why}",
                        self.events.subject(),
                        plan.token,
                        plan.addr
                    ),
                    serde_json::json!({
                        "error_code": "srt_bond_source_not_applied",
                        "index": plan.token,
                        "addr": plan.addr.to_string(),
                        "local_addr": plan.explicit_local.map(|a| a.to_string()),
                        "interface": plan.binding.as_ref().map(|b| b.name.clone()),
                        "reason": why,
                    }),
                );
            }
            MemberSource::Unavailable(e) => {
                self.tracks[idx].awaiting_handshake = false;
                self.tracks[idx].last_error = Some(e.clone());
            }
            _ => {}
        }
    }

    /// Record how a re-add from `source` went: `Ok` = handed to libsrt, now
    /// handshaking.
    ///
    /// `ConnectionLost` is the bridge refusing a re-add into a group with no
    /// Idle or Running member left (or one it has seen break), or libsrt
    /// naming no member error. It says nothing about this member, so a
    /// reason the member already has is kept: if the group is failing
    /// whole, that reason is what the disconnect event's report should show.
    fn note_readd(&mut self, idx: usize, result: Result<(), SrtError>, source: Option<SocketAddr>) {
        let Err(e) = result else {
            self.tracks[idx].awaiting_handshake = true;
            return;
        };
        let message = format!(
            "re-adding the member failed: {}",
            describe_connect_error(&e, source.as_slice())
        );
        tracing::warn!(
            "{}: bonding member {} ({}): {message}",
            self.events.subject(),
            self.plans[idx].token,
            self.plans[idx].addr
        );
        let track = &mut self.tracks[idx];
        track.awaiting_handshake = false;
        if e != SrtError::ConnectionLost || track.last_error.is_none() {
            track.last_error = Some(message);
        }
    }

    /// One stats tick: re-add what [`Self::decide`] asks for and raise its
    /// events. `view` is the listing the tick already read.
    pub(crate) async fn tick(&mut self, group: &SrtGroup, view: &[MemberObs], now: Instant) {
        if !self.sources_reported {
            // The connect that built this group resolved the same way a
            // moment ago.
            self.sources_reported = true;
            for idx in 0..self.plans.len() {
                let source = resolve_member_source(&self.plans[idx]);
                self.note_source(idx, &source);
            }
        }
        for action in self.decide(view, now) {
            match action {
                BondAction::Readd(token) => {
                    let Some(idx) = self.plans.iter().position(|p| p.token == token) else {
                        continue;
                    };
                    let source = resolve_member_source(&self.plans[idx]);
                    self.note_source(idx, &source);
                    let Some(ep) = group_endpoint(&self.plans[idx], &source) else {
                        continue;
                    };
                    let result = group.add_member(ep).await;
                    self.note_readd(idx, result, ep.local_addr);
                }
                BondAction::Degraded { connected, configured, down } => {
                    self.emit_degraded(connected, configured, &down);
                }
                BondAction::Restored { configured } => {
                    tracing::info!(
                        "{}: all {configured} bonding members connected again",
                        self.events.subject()
                    );
                    self.events.emit(
                        EventSeverity::Info,
                        format!(
                            "{}: all {configured} bonding members connected again",
                            self.events.subject()
                        ),
                        serde_json::json!({
                            "error_code": "srt_bond_member_restored",
                            "configured": configured,
                            "connected": configured,
                        }),
                    );
                }
            }
        }
    }

    fn emit_degraded(&self, connected: usize, configured: usize, down: &[DownMember]) {
        let list: Vec<serde_json::Value> = down
            .iter()
            .map(|d| {
                let idx = self.plans.iter().position(|p| p.token == d.token);
                serde_json::json!({
                    "index": d.token,
                    "addr": idx.map(|i| self.plans[i].addr.to_string()),
                    "source": idx.map(|i| self.tracks[i].source.clone()),
                    "down_secs": d.down_for.as_secs(),
                    "last_error": d.last_error,
                })
            })
            .collect();
        let names: Vec<String> = down.iter().map(|d| d.token.to_string()).collect();
        let message = format!(
            "{}: running on {connected} of {configured} bonding members (redundancy reduced) — \
             member {} down, re-adding",
            self.events.subject(),
            names.join(", ")
        );
        tracing::warn!("{message}");
        self.events.emit(
            EventSeverity::Warning,
            message,
            serde_json::json!({
                "error_code": "srt_bond_member_down",
                "configured": configured,
                "connected": connected,
                "down": list,
            }),
        );
    }

    /// Every member as this group ended, for the caller loop's whole-group
    /// disconnect event: the source it last tried, whether it was ever live
    /// in this group, and why not.
    ///
    /// That event is the only report of a bond whose members open but never
    /// complete a handshake — every member pinned to a source the kernel
    /// routes out of the wrong interface, say. Its connect succeeds (libsrt
    /// returns once the members are spawned), so no `connect_failed` fires,
    /// and with no member ever live neither does `srt_bond_member_down`.
    pub(crate) fn final_report(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.plans
                .iter()
                .zip(&self.tracks)
                .map(|(plan, t)| {
                    // A connect or re-add still handshaking when the group
                    // failed never completed.
                    let last_error = t.last_error.clone().or_else(|| {
                        t.awaiting_handshake.then(|| handshake_failed_reason(&t.source))
                    });
                    serde_json::json!({
                        "index": plan.token,
                        "addr": plan.addr.to_string(),
                        "source": t.source,
                        "was_live": t.was_live,
                        "last_error": last_error,
                    })
                })
                .collect(),
        )
    }
}

/// Wait for a caller group's stats poller to stop (its token already
/// cancelled) and take its supervisor's [`BondSupervisor::final_report`] —
/// `null` with no supervisor, or if the poller has not stopped within 2 s
/// (it stops between ticks, and a tick is a few srt-io round trips).
pub(crate) async fn final_member_report(
    poller: JoinHandle<Option<BondSupervisor>>,
) -> serde_json::Value {
    match tokio::time::timeout(Duration::from_secs(2), poller).await {
        Ok(Ok(Some(supervisor))) => supervisor.final_report(),
        _ => serde_json::Value::Null,
    }
}

// ── Listener-side observer ───────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TrackerAction {
    Degraded { connected: usize, peak: usize, missing: Vec<String> },
    Restored { connected: usize },
}

/// Listener-side twin of [`BondSupervisor`] for one accepted bonded
/// session. The edge cannot re-add a member here — only the remote caller
/// can — so this only reports: running below the most live members the
/// session has had, for at least `degrade_after`, with at least one still
/// live (a session with none is the disconnect the listener loop already
/// reports). One tracker per accepted session: a session that drops and is
/// re-accepted starts afresh.
pub(crate) struct BondHealthTracker {
    events: BondEvents,
    degrade_after: Duration,
    peak: usize,
    /// The peers live at the last tick the session was at its peak: what a
    /// later loss is measured against. Replaced, never added to — a remote
    /// caller re-adds a member from a new source port, and the old port
    /// must not stay listed as missing.
    peak_peers: Vec<String>,
    down_since: Option<Instant>,
    degraded: bool,
}

impl BondHealthTracker {
    pub(crate) fn new(events: BondEvents, degrade_after: Duration) -> Self {
        Self {
            events,
            degrade_after,
            peak: 0,
            peak_peers: Vec::new(),
            down_since: None,
            degraded: false,
        }
    }

    /// The pure half: `live` is the peer of every Idle / Running member.
    pub(crate) fn decide(&mut self, live: &[String], now: Instant) -> Option<TrackerAction> {
        if live.len() >= self.peak {
            self.peak = live.len();
            self.peak_peers = live.to_vec();
        }
        if live.is_empty() {
            return None;
        }
        if live.len() < self.peak {
            let since = *self.down_since.get_or_insert(now);
            if !self.degraded && now.saturating_duration_since(since) >= self.degrade_after {
                self.degraded = true;
                let missing = self.peak_peers.iter().filter(|p| !live.contains(p)).cloned().collect();
                return Some(TrackerAction::Degraded { connected: live.len(), peak: self.peak, missing });
            }
            return None;
        }
        self.down_since = None;
        if self.degraded {
            self.degraded = false;
            return Some(TrackerAction::Restored { connected: live.len() });
        }
        None
    }

    pub(crate) fn tick(&mut self, members: &[GroupMemberStats], now: Instant) {
        let live: Vec<String> = members
            .iter()
            .filter(|m| {
                matches!(m.member_status, MemberStatus::Idle | MemberStatus::Running)
                    && !socket_terminal(m.socket_status)
            })
            .map(|m| m.peer_addr.map_or_else(|| format!("member {}", m.id), |a| a.to_string()))
            .collect();
        match self.decide(&live, now) {
            Some(TrackerAction::Degraded { connected, peak, missing }) => {
                let message = format!(
                    "{}: running on {connected} of {peak} bonding members (redundancy reduced) — \
                     the remote caller has not re-added {}",
                    self.events.subject(),
                    missing.join(", ")
                );
                tracing::warn!("{message}");
                self.events.emit(
                    EventSeverity::Warning,
                    message,
                    serde_json::json!({
                        "error_code": "srt_bond_member_down",
                        "configured": peak,
                        "connected": connected,
                        "missing": missing,
                    }),
                );
            }
            Some(TrackerAction::Restored { connected }) => {
                let message = format!(
                    "{}: back to {connected} bonding members",
                    self.events.subject()
                );
                tracing::info!("{message}");
                self.events.emit(
                    EventSeverity::Info,
                    message,
                    serde_json::json!({
                        "error_code": "srt_bond_member_restored",
                        "configured": connected,
                        "connected": connected,
                    }),
                );
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::SrtBondingEndpoint;
    use crate::manager::events::event_channel;
    use tokio::sync::mpsc;

    fn ep(addr: &str) -> SrtBondingEndpoint {
        SrtBondingEndpoint {
            addr: addr.to_string(),
            local_addr: None,
            weight: None,
            interface_binding: None,
        }
    }

    fn bond(mode: SrtBondingMode, endpoints: Vec<SrtBondingEndpoint>) -> SrtBondingConfig {
        SrtBondingConfig { mode, endpoints }
    }

    fn nic(name: &str) -> Option<InterfaceBinding> {
        Some(InterfaceBinding { name: name.to_string(), strict: false })
    }

    fn two_plans() -> Vec<BondMemberPlan> {
        plan_bond_members(&bond(
            SrtBondingMode::Broadcast,
            vec![ep("203.0.113.10:9000"), ep("203.0.113.11:9000")],
        ))
        .unwrap()
    }

    fn input_events(tx: EventSender) -> BondEvents {
        BondEvents::new(
            BondEventScope::Input { flow_id: "f1".into(), input_id: "in1".into() },
            tx,
            SrtBondingMode::Broadcast,
            "caller-bonded",
        )
    }

    fn supervisor(
        plans: Vec<BondMemberPlan>,
        latch: Arc<BondLatch>,
        t0: Instant,
    ) -> (BondSupervisor, mpsc::Receiver<Event>) {
        let (tx, rx) = event_channel();
        (BondSupervisor::new(plans, input_events(tx), latch, degrade_after(None), t0), rx)
    }

    fn obs(token: i32, member: MemberStatus, socket: SocketStatus) -> MemberObs {
        MemberObs { token, member, socket }
    }
    fn live(token: i32) -> MemberObs {
        obs(token, MemberStatus::Running, SocketStatus::Connected)
    }
    fn idle(token: i32) -> MemberObs {
        obs(token, MemberStatus::Idle, SocketStatus::Connected)
    }
    fn pending(token: i32) -> MemberObs {
        obs(token, MemberStatus::Pending, SocketStatus::Connecting)
    }
    fn broken(token: i32) -> MemberObs {
        obs(token, MemberStatus::Broken, SocketStatus::Broken)
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn readds(actions: &[BondAction]) -> Vec<i32> {
        actions
            .iter()
            .filter_map(|a| if let BondAction::Readd(t) = a { Some(*t) } else { None })
            .collect()
    }

    // ── Plans and weights ───────────────────────────────────────────────

    #[test]
    fn tokens_are_config_indices() {
        let plans = plan_bond_members(&bond(
            SrtBondingMode::Broadcast,
            vec![ep("10.0.0.1:9000"), ep("10.0.0.2:9000"), ep("10.0.0.3:9000")],
        ))
        .unwrap();
        assert_eq!(plans.iter().map(|p| p.token).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(plans[2].addr, "10.0.0.3:9000".parse().unwrap());
    }

    /// The edge's contract is "lower is preferred"; libsrt's is "greater is
    /// preferred". Before #184 the weight never reached libsrt at all.
    #[test]
    fn lower_configured_weight_maps_to_higher_libsrt_weight() {
        let mut eps = vec![ep("10.0.0.1:9000"), ep("10.0.0.2:9000"), ep("10.0.0.3:9000")];
        eps[0].weight = Some(10);
        eps[1].weight = Some(0);
        eps[2].weight = Some(5);
        let plans = plan_bond_members(&bond(SrtBondingMode::Backup, eps)).unwrap();
        assert_eq!(plans.iter().map(|p| p.srt_weight).collect::<Vec<_>>(), [0, 2, 1]);
        assert_eq!(plans.iter().map(|p| p.configured_weight).collect::<Vec<_>>(), [10, 0, 5]);
    }

    #[test]
    fn equal_weights_stay_equal() {
        assert_eq!(srt_weights_for(&[3, 3, 3]), [0, 0, 0]);
        assert_eq!(srt_weights_for(&[0, 7, 0, 7]), [1, 0, 1, 0]);
        // Unset is 0, the most preferred.
        let mut eps = vec![ep("10.0.0.1:9000"), ep("10.0.0.2:9000")];
        eps[1].weight = Some(1);
        let plans = plan_bond_members(&bond(SrtBondingMode::Backup, eps)).unwrap();
        assert_eq!(plans.iter().map(|p| p.srt_weight).collect::<Vec<_>>(), [1, 0]);
    }

    #[test]
    fn broadcast_weights_are_zero() {
        let mut eps = vec![ep("10.0.0.1:9000"), ep("10.0.0.2:9000")];
        eps[0].weight = Some(0);
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, eps)).unwrap();
        assert!(plans.iter().all(|p| p.srt_weight == 0 && p.configured_weight == 0));
    }

    /// libsrt refuses the whole group connect for a weight above 32767; the
    /// manager allows up to 65535. A rank keeps the order with no clamping.
    #[test]
    fn u16_max_weights_keep_their_order() {
        let w = srt_weights_for(&[u16::MAX, 32768, 32767, 0]);
        assert_eq!(w, [0, 1, 2, 3]);
        assert!(w.iter().all(|w| *w <= srt_transport::MAX_GROUP_MEMBER_WEIGHT));
        assert!(srt_weights_for(&[]).is_empty());
    }

    // ── Sources ─────────────────────────────────────────────────────────

    /// #184(a): a configured `local_addr` reaches libsrt. Before, the group
    /// builder took only the target, so every member left from the
    /// default route's source.
    #[test]
    fn explicit_local_addr_is_carried_into_the_group_endpoint() {
        let mut e = ep("203.0.113.10:9000");
        e.local_addr = Some("192.168.1.2:0".into());
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![e, ep("203.0.113.11:9000")]))
            .unwrap();
        let source = resolve_member_source(&plans[0]);
        assert_eq!(source, MemberSource::Bound("192.168.1.2:0".parse().unwrap()));
        let member = group_endpoint(&plans[0], &source).unwrap();
        assert_eq!(member.local_addr, Some("192.168.1.2:0".parse().unwrap()));
        assert_eq!(member.token, 0);
        assert_eq!(resolve_member_source(&plans[1]), MemberSource::Unbound(None));
    }

    /// As on every SRT surface (`srt_local_addr_from_binding`), an explicit
    /// `local_addr` wins: its NIC is not even looked up.
    #[test]
    fn explicit_local_addr_wins_over_interface_binding() {
        let mut e = ep("203.0.113.10:9000");
        e.local_addr = Some("192.168.1.2:0".into());
        e.interface_binding = nic("no-such-nic0");
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![e])).unwrap();
        assert_eq!(
            resolve_member_source(&plans[0]),
            MemberSource::Bound("192.168.1.2:0".parse().unwrap())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interface_binding_lo_resolves_to_127_0_0_1_port_0() {
        let mut e = ep("127.0.0.1:9000");
        e.interface_binding = nic("lo");
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![e])).unwrap();
        assert_eq!(
            resolve_member_source(&plans[0]),
            MemberSource::Bound("127.0.0.1:0".parse().unwrap())
        );
    }

    /// The NIC path is IPv4-only; bound to an IPv6 peer it would make libsrt
    /// refuse the whole group connect. The member connects unbound and the
    /// reason is kept for the one-shot warning.
    #[cfg(target_os = "linux")]
    #[test]
    fn interface_binding_to_an_ipv6_peer_falls_back_unbound_with_a_reason() {
        let mut e = ep("[2001:db8::10]:9000");
        e.interface_binding = nic("lo");
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![e])).unwrap();
        match resolve_member_source(&plans[0]) {
            MemberSource::Unbound(Some(why)) => assert!(why.contains("lo"), "{why}"),
            other => panic!("expected unbound with a reason, got {other:?}"),
        }
    }

    #[test]
    fn a_family_mismatched_local_addr_falls_back_unbound_with_a_reason() {
        let mut e = ep("[2001:db8::10]:9000");
        e.local_addr = Some("192.168.1.2:0".into());
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![e])).unwrap();
        assert!(matches!(resolve_member_source(&plans[0]), MemberSource::Unbound(Some(_))));
    }

    #[test]
    fn a_missing_nic_is_unavailable_not_unbound() {
        let mut e = ep("203.0.113.10:9000");
        e.interface_binding = nic("no-such-nic0");
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![e])).unwrap();
        let source = resolve_member_source(&plans[0]);
        assert!(matches!(source, MemberSource::Unavailable(_)), "{source:?}");
        assert!(group_endpoint(&plans[0], &source).is_none());
    }

    #[test]
    fn unspecified_explicit_local_is_unbound_without_warning() {
        for la in ["0.0.0.0:0", "[::]:0"] {
            let mut e = ep("203.0.113.10:9000");
            e.local_addr = Some(la.into());
            let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![e])).unwrap();
            assert_eq!(resolve_member_source(&plans[0]), MemberSource::Unbound(None), "{la}");
        }
    }

    /// Whatever the config, every member the edge builds passes libsrt's
    /// whole-call checks — otherwise `connect` would fail deterministically
    /// and the caller would retry it forever.
    #[test]
    fn every_planned_endpoint_passes_libsrt_check() {
        let mut eps: Vec<SrtBondingEndpoint> = (0..8).map(|i| ep(&format!("10.0.0.{i}:9000"))).collect();
        for (e, w) in eps.iter_mut().zip([u16::MAX, 0, 7, u16::MAX, 3, 3, 65534, 1]) {
            e.weight = Some(w);
        }
        eps[1].local_addr = Some("192.168.1.2:4000".into());
        eps[2].local_addr = Some("0.0.0.0:0".into());
        eps[3].addr = "[2001:db8::3]:9000".into();
        eps[3].local_addr = Some("192.168.1.3:0".into());
        eps[4].addr = "[2001:db8::4]:9000".into();
        eps[4].local_addr = Some("[2001:db8::99]:0".into());
        eps[5].addr = "[2001:db8::5]:9000".into();
        eps[5].interface_binding = nic("lo");
        eps[6].interface_binding = nic("lo");
        eps[7].local_addr = Some("[::]:0".into());
        let plans = plan_bond_members(&bond(SrtBondingMode::Backup, eps)).unwrap();
        for plan in &plans {
            let source = resolve_member_source(plan);
            if let Some(member) = group_endpoint(plan, &source) {
                assert_eq!(member.check(), Ok(()), "{plan:?} -> {member:?}");
            }
        }
    }

    #[test]
    fn an_attempt_skips_members_whose_nic_is_missing() {
        let mut gone = ep("203.0.113.10:9000");
        gone.interface_binding = nic("no-such-nic0");
        let plans =
            plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![gone, ep("203.0.113.11:9000")]))
                .unwrap();
        let members = bond_attempt_endpoints(&plans).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].token, 1);
    }

    /// With nothing preparable the attempt fails through the caller's
    /// observer and backoff, rather than handing libsrt an empty group.
    #[test]
    fn an_attempt_with_no_preparable_member_fails() {
        let mut a = ep("203.0.113.10:9000");
        a.interface_binding = nic("no-such-nic0");
        let mut b = ep("203.0.113.11:9000");
        b.interface_binding = nic("no-such-nic1");
        let plans = plan_bond_members(&bond(SrtBondingMode::Broadcast, vec![a, b])).unwrap();
        let err = bond_attempt_endpoints(&plans).unwrap_err().to_string();
        assert!(err.contains("no bonding member could be prepared"), "{err}");
        assert!(err.contains("no-such-nic0") && err.contains("no-such-nic1"), "{err}");
    }

    // ── Spawn-time check ────────────────────────────────────────────────

    /// The spawn-time abort is exactly as before: an explicit `local_addr`
    /// wins, so its NIC is never looked up and a missing one is no error.
    #[test]
    fn explicit_local_addr_with_missing_nic_still_starts() {
        let mut e = ep("203.0.113.10:9000");
        e.local_addr = Some("192.168.1.2:0".into());
        e.interface_binding = nic("no-such-nic0");
        assert!(check_bond_endpoint_bindings(&bond(SrtBondingMode::Broadcast, vec![e])).is_ok());
    }

    #[test]
    fn a_missing_nic_without_local_addr_still_aborts() {
        let mut e = ep("203.0.113.10:9000");
        e.interface_binding = nic("no-such-nic0");
        assert!(check_bond_endpoint_bindings(&bond(SrtBondingMode::Broadcast, vec![e])).is_err());
    }

    #[test]
    fn a_strict_bonding_binding_still_errors() {
        let mut e = ep("203.0.113.10:9000");
        e.interface_binding = Some(InterfaceBinding { name: "lo".into(), strict: true });
        assert!(check_bond_endpoint_bindings(&bond(SrtBondingMode::Broadcast, vec![e])).is_err());
    }

    // ── Supervisor ──────────────────────────────────────────────────────

    /// #203: a member libsrt dropped is re-added, after the first backoff.
    #[test]
    fn a_missing_member_is_re_added_once_its_backoff_elapses() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        assert!(readds(&sup.decide(&[live(0)], at(t0, 500))).is_empty());
        assert_eq!(readds(&sup.decide(&[live(0)], at(t0, 1000))), [1]);
        assert!(readds(&sup.decide(&[live(0)], at(t0, 1500))).is_empty());
        assert_eq!(readds(&sup.decide(&[live(0)], at(t0, 2000))), [1]);
    }

    /// One handshake per endpoint at a time.
    #[test]
    fn a_pending_member_is_never_re_added() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        for s in 1..120 {
            assert!(readds(&sup.decide(&[live(0), pending(1)], at(t0, s * 1000))).is_empty());
        }
    }

    /// libsrt can keep a broken member listed while it drains: that is down.
    #[test]
    fn a_broken_member_listed_beside_nothing_else_counts_as_down() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        assert_eq!(readds(&sup.decide(&[live(0), broken(1)], at(t0, 1000))), [1]);
    }

    /// A re-added member beside its still-listed broken predecessor.
    #[test]
    fn a_token_with_one_broken_and_one_running_member_is_healthy() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        for s in 1..60 {
            assert!(sup.decide(&[idle(0), broken(1), live(1)], at(t0, s * 1000)).is_empty());
        }
    }

    #[test]
    fn re_add_backs_off_1_2_4_to_30s_and_resets_on_health() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        let mut times = Vec::new();
        for half in 1..=260u64 {
            if !readds(&sup.decide(&[live(0)], at(t0, half * 500))).is_empty() {
                times.push(half * 500);
            }
        }
        assert_eq!(times, [1000, 2000, 4000, 8000, 16000, 32000, 62000, 92000, 122000]);

        // Healthy again resets it: the next loss is re-added at once, then
        // after 1 s.
        assert!(readds(&sup.decide(&[live(0), live(1)], at(t0, 130_000))).is_empty());
        assert_eq!(readds(&sup.decide(&[live(0)], at(t0, 140_000))), [1]);
        assert!(readds(&sup.decide(&[live(0)], at(t0, 140_500))).is_empty());
        assert_eq!(readds(&sup.decide(&[live(0)], at(t0, 141_000))), [1]);
    }

    /// With no live member the group is failing as a whole. A member added
    /// now would count as healthy in libsrt's failure test and keep the dead
    /// group out of a terminal state, deferring the whole-group redial.
    #[test]
    fn no_re_add_while_no_member_is_healthy() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        let views: [&[MemberObs]; 4] =
            [&[], &[broken(0), broken(1)], &[pending(0), broken(1)], &[pending(0)]];
        for (i, view) in views.iter().cycle().take(120).enumerate() {
            let actions = sup.decide(view, at(t0, 1000 + i as u64 * 500));
            assert!(actions.is_empty(), "{view:?}: {actions:?}");
        }
    }

    #[test]
    fn re_add_resumes_once_a_member_is_healthy() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        assert!(sup.decide(&[], at(t0, 5000)).is_empty());
        assert!(sup.decide(&[broken(0)], at(t0, 6000)).is_empty());
        assert_eq!(readds(&sup.decide(&[live(0)], at(t0, 7000))), [1]);
    }

    /// Members still handshaking at start are not "down".
    #[test]
    fn startup_pending_inside_grace_raises_no_warning() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        for ms in (500..=4000).step_by(500) {
            assert!(sup.decide(&[live(0), pending(1)], at(t0, ms)).is_empty());
        }
        for ms in (4500..=20000).step_by(500) {
            assert!(sup.decide(&[live(0), live(1)], at(t0, ms)).is_empty());
        }
    }

    #[test]
    fn degraded_fires_once_per_episode_and_restored_once() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        let mut degraded = Vec::new();
        let mut restored = 0;
        let mut tick = |sup: &mut BondSupervisor, view: &[MemberObs], ms: u64| {
            for a in sup.decide(view, at(t0, ms)) {
                match a {
                    BondAction::Degraded { connected, configured, down } => {
                        degraded.push((ms, connected, configured, down))
                    }
                    BondAction::Restored { configured } => {
                        assert_eq!(configured, 2);
                        restored += 1;
                    }
                    BondAction::Readd(_) => {}
                }
            }
        };
        tick(&mut sup, &[live(0), live(1)], 1000);
        for ms in (2000..=20000).step_by(500) {
            tick(&mut sup, &[live(0)], ms);
        }
        for ms in (20500..=30000).step_by(500) {
            tick(&mut sup, &[live(0), live(1)], ms);
        }
        assert_eq!(degraded.len(), 1, "{degraded:?}");
        let (ms, connected, configured, down) = &degraded[0];
        assert_eq!(*ms, 2000 + degrade_after(None).as_millis() as u64);
        assert_eq!((*connected, *configured), (1, 2));
        assert_eq!(down.iter().map(|d| d.token).collect::<Vec<_>>(), [1]);
        assert_eq!(restored, 1);
    }

    /// Every member down is the whole-group disconnect the caller loop
    /// already reports; the member event stays out of it.
    #[test]
    fn no_degraded_event_while_no_member_is_live() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        sup.decide(&[live(0), live(1)], at(t0, 1000));
        for ms in (2000..=60000).step_by(500) {
            assert!(sup.decide(&[broken(0)], at(t0, ms)).is_empty());
        }
    }

    /// A whole-group reconnect builds a new supervisor; the episode it
    /// interrupted still gets its "restored".
    #[test]
    fn the_latch_survives_a_new_supervisor() {
        let t0 = Instant::now();
        let latch = Arc::new(BondLatch::default());
        let (mut first, _rx1) = supervisor(two_plans(), latch.clone(), t0);
        first.decide(&[live(0), live(1)], at(t0, 1000));
        let degraded = (2000..=20000)
            .step_by(500)
            .flat_map(|ms| first.decide(&[live(0)], at(t0, ms)))
            .filter(|a| matches!(a, BondAction::Degraded { .. }))
            .count();
        assert_eq!(degraded, 1);
        drop(first);

        let (mut second, _rx2) = supervisor(two_plans(), latch, at(t0, 30_000));
        assert_eq!(
            second.decide(&[live(0), live(1)], at(t0, 31_000)),
            [BondAction::Restored { configured: 2 }]
        );
        // And a fresh loss on the new group is a new episode.
        let again = (32_000..=50_000)
            .step_by(500)
            .flat_map(|ms| second.decide(&[live(1)], at(t0, ms)))
            .filter(|a| matches!(a, BondAction::Degraded { .. }))
            .count();
        assert_eq!(again, 1);
    }

    /// The usual failure of a source-bound member is asynchronous: libsrt
    /// accepts the member, it sits Pending, then disappears. The down event
    /// must still say why.
    #[test]
    fn a_handshake_that_never_completes_leaves_a_reason() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        sup.note_source(1, &MemberSource::Bound("192.168.2.2:0".parse().unwrap()));
        sup.decide(&[live(0), pending(1)], at(t0, 1000));
        let down = (3000..=20000)
            .step_by(500)
            .flat_map(|ms| sup.decide(&[live(0)], at(t0, ms)))
            .find_map(|a| if let BondAction::Degraded { down, .. } = a { Some(down) } else { None })
            .expect("no degraded event");
        let reason = down[0].last_error.as_deref().expect("no reason recorded");
        assert!(reason.contains("handshake did not complete"), "{reason}");
        assert!(reason.contains("192.168.2.2:0") && reason.contains("routing"), "{reason}");
        // The listing carries no reject reason, so a peer that refused the
        // handshake (wrong passphrase, stream id) reads the same: the text
        // must not blame routing alone.
        assert!(reason.contains("rejected") && reason.contains("passphrase"), "{reason}");
        let unbound = handshake_failed_reason("unbound");
        assert!(unbound.contains("rejected") && !unbound.contains("did not answer"), "{unbound}");
    }

    /// Every member pinned to a source the kernel cannot route to the peer:
    /// the group connects, no member ever goes live, and it fails whole.
    /// Neither `connect_failed` nor `srt_bond_member_down` can fire, so the
    /// disconnect event's report is what says why — and it must not blame
    /// a member that did carry.
    #[test]
    fn the_final_report_says_why_a_member_never_went_live() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        sup.note_source(0, &MemberSource::Bound("192.168.1.2:0".parse().unwrap()));
        sup.note_source(1, &MemberSource::Bound("192.168.2.2:0".parse().unwrap()));
        assert!(sup.decide(&[pending(0), pending(1)], at(t0, 1000)).is_empty());
        let report = sup.final_report();
        for (i, src) in [(0, "192.168.1.2:0"), (1, "192.168.2.2:0")] {
            assert_eq!(report[i]["index"], i);
            assert_eq!(report[i]["addr"], two_plans()[i].addr.to_string());
            assert_eq!(report[i]["source"], src);
            assert_eq!(report[i]["was_live"], false);
            let why = report[i]["last_error"].as_str().expect("no reason");
            assert!(why.contains("handshake did not complete") && why.contains(src), "{why}");
        }

        // A member that was live leaves no reason: the group lost it with
        // the rest, it did not fail to connect.
        sup.decide(&[live(0), pending(1)], at(t0, 2000));
        let report = sup.final_report();
        assert_eq!(report[0]["was_live"], true);
        assert!(report[0]["last_error"].is_null(), "{}", report[0]);
        assert_eq!(report[1]["was_live"], false);
        assert!(report[1]["last_error"].is_string());
    }

    /// A group that fails before the supervisor's first tick still reports
    /// each member's source, as the connect resolved it — and a member
    /// whose NIC was missing, left out of that connect, its NIC rather than
    /// a handshake it never had.
    #[test]
    fn the_final_report_has_sources_before_the_first_tick() {
        let mut first = ep("203.0.113.10:9000");
        first.local_addr = Some("192.0.2.10:0".into());
        let mut gone = ep("203.0.113.12:9000");
        gone.interface_binding = nic("no-such-nic0");
        let plans = plan_bond_members(&bond(
            SrtBondingMode::Broadcast,
            vec![first, ep("203.0.113.11:9000"), gone],
        ))
        .unwrap();
        let (sup, _rx) = supervisor(plans, Arc::default(), Instant::now());
        let report = sup.final_report();
        assert_eq!(report[0]["source"], "192.0.2.10:0");
        assert!(report[0]["last_error"].as_str().unwrap().contains("192.0.2.10:0"));
        assert_eq!(report[1]["source"], "unbound");
        let why = report[2]["last_error"].as_str().expect("no reason");
        assert!(why.contains("no-such-nic0") && !why.contains("handshake"), "{why}");
    }

    /// The bridge refuses a re-add with `ConnectionLost` once the group has
    /// no live member left. That is the group failing, not this member, so
    /// the member's own earlier reason survives for the disconnect report.
    #[test]
    fn a_refused_re_add_keeps_the_members_earlier_reason() {
        let t0 = Instant::now();
        let (mut sup, _rx) = supervisor(two_plans(), Arc::default(), t0);
        let src: SocketAddr = "192.168.2.2:0".parse().unwrap();
        sup.note_source(1, &MemberSource::Bound(src));
        sup.decide(&[live(0), pending(1)], at(t0, 1000));
        sup.decide(&[live(0)], at(t0, 2000));
        let earlier = sup.tracks[1].last_error.clone().expect("no handshake reason");

        sup.note_readd(1, Err(SrtError::ConnectionLost), Some(src));
        assert_eq!(sup.tracks[1].last_error.as_deref(), Some(earlier.as_str()));
        assert!(!sup.tracks[1].awaiting_handshake);

        // An error about the member itself replaces it.
        sup.note_readd(1, Err(SrtError::SocketFail), Some(src));
        let now = sup.tracks[1].last_error.clone().unwrap();
        assert!(now.contains("re-adding the member failed") && now.contains("192.168.2.2:0"), "{now}");

        // With nothing earlier, the refusal is the reason.
        sup.note_readd(0, Err(SrtError::ConnectionLost), None);
        assert!(sup.tracks[0].last_error.as_deref().unwrap().contains("re-adding"));

        sup.note_readd(1, Ok(()), Some(src));
        assert!(sup.tracks[1].awaiting_handshake);
    }

    /// libsrt reports a source it cannot bind as `SocketFail` ("unable to
    /// create/configure SRT socket"); the operator needs the address.
    #[test]
    fn an_unbindable_source_is_named_in_the_error() {
        let src: SocketAddr = "192.0.2.1:0".parse().unwrap();
        let msg = describe_connect_error(&SrtError::SocketFail, &[src]);
        assert!(msg.contains("192.0.2.1:0") && msg.contains("assigned to this host"), "{msg}");
        assert!(msg.contains("open or bind") && !msg.contains("port"), "{msg}");
        assert!(msg.contains(&SrtError::SocketFail.to_string()), "{msg}");
        // A fixed source port can be what is taken.
        let fixed: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        let msg = describe_connect_error(&SrtError::SocketFail, &[fixed]);
        assert!(msg.contains("192.0.2.1:5000") && msg.contains("port free"), "{msg}");
        let two = [src, "198.51.100.1:0".parse().unwrap()];
        let msg = describe_connect_error(&SrtError::SocketFail, &two);
        assert!(msg.contains("192.0.2.1:0, 198.51.100.1:0") && msg.contains("each address"), "{msg}");
        // Unbound, or any other error: libsrt's own text.
        assert_eq!(
            describe_connect_error(&SrtError::SocketFail, &[]),
            SrtError::SocketFail.to_string()
        );
        assert_eq!(
            describe_connect_error(&SrtError::ConnectionLost, &[src]),
            SrtError::ConnectionLost.to_string()
        );
    }

    #[test]
    fn source_not_applied_warns_once_per_member() {
        let t0 = Instant::now();
        let latch = Arc::new(BondLatch::default());
        let (mut sup, mut rx) = supervisor(two_plans(), latch.clone(), t0);
        let why = MemberSource::Unbound(Some("interface 'wwan0' has no usable IPv4 address".into()));
        sup.note_source(1, &why);
        sup.note_source(1, &why);
        sup.note_source(1, &MemberSource::Unbound(None));
        let event = rx.try_recv().expect("no warning");
        assert_eq!(event.severity, EventSeverity::Warning);
        let details = event.details.unwrap();
        assert_eq!(details["error_code"], "srt_bond_source_not_applied");
        assert_eq!(details["index"], 1);
        assert!(rx.try_recv().is_err(), "warned twice");

        // Still once across a whole-group reconnect.
        let (mut again, mut rx2) = supervisor(two_plans(), latch, t0);
        again.note_source(1, &why);
        assert!(rx2.try_recv().is_err(), "warned again after a reconnect");
    }

    #[test]
    fn degraded_event_names_each_down_member_with_its_source_and_reason() {
        let t0 = Instant::now();
        let (mut sup, mut rx) = supervisor(two_plans(), Arc::default(), t0);
        sup.note_source(1, &MemberSource::Bound("192.168.2.2:0".parse().unwrap()));
        sup.emit_degraded(
            1,
            2,
            &[DownMember {
                token: 1,
                down_for: Duration::from_secs(7),
                last_error: Some("boom".into()),
            }],
        );
        let event = rx.try_recv().expect("no event");
        assert_eq!(event.severity, EventSeverity::Warning);
        assert_eq!(event.category, category::SRT);
        assert_eq!(event.flow_id.as_deref(), Some("f1"));
        assert_eq!(event.input_id.as_deref(), Some("in1"));
        let d = event.details.unwrap();
        assert_eq!(d["error_code"], "srt_bond_member_down");
        assert_eq!(d["mode"], "caller-bonded");
        assert_eq!((d["configured"].as_u64(), d["connected"].as_u64()), (Some(2), Some(1)));
        assert_eq!(d["down"][0]["index"], 1);
        assert_eq!(d["down"][0]["addr"], "203.0.113.11:9000");
        assert_eq!(d["down"][0]["source"], "192.168.2.2:0");
        assert_eq!(d["down"][0]["last_error"], "boom");
        assert_eq!(d["down"][0]["down_secs"], 7);
    }

    // ── Listener-side tracker ───────────────────────────────────────────

    fn tracker() -> (BondHealthTracker, mpsc::Receiver<Event>) {
        let (tx, rx) = event_channel();
        let events = BondEvents::new(
            BondEventScope::Output { output_id: "out1".into() },
            tx,
            SrtBondingMode::Backup,
            "listener-bonded",
        );
        (BondHealthTracker::new(events, degrade_after(None)), rx)
    }

    #[test]
    fn listener_tracker_warns_below_peak_and_recovers() {
        let t0 = Instant::now();
        let (mut tr, _rx) = tracker();
        let a = "198.51.100.1:5000".to_string();
        let b = "198.51.100.2:5000".to_string();
        // Members join one at a time: no warning on the way up.
        assert_eq!(tr.decide(std::slice::from_ref(&a), at(t0, 0)), None);
        assert_eq!(tr.decide(&[a.clone(), b.clone()], at(t0, 1000)), None);
        let mut events = Vec::new();
        for ms in (2000..=20000).step_by(1000) {
            events.extend(tr.decide(std::slice::from_ref(&a), at(t0, ms)));
        }
        assert_eq!(
            events,
            [TrackerAction::Degraded { connected: 1, peak: 2, missing: vec![b.clone()] }]
        );
        assert_eq!(
            tr.decide(&[a.clone(), b], at(t0, 21_000)),
            Some(TrackerAction::Restored { connected: 2 })
        );
        assert_eq!(tr.decide(std::slice::from_ref(&a), at(t0, 22_000)), None);
    }

    /// A remote caller re-adds an unbound member from a new source port.
    /// The next loss must list only the member missing now — not the port
    /// the earlier episode already replaced.
    #[test]
    fn listener_tracker_lists_only_the_member_missing_now() {
        let t0 = Instant::now();
        let (mut tr, _rx) = tracker();
        let a = "198.51.100.1:5000".to_string();
        let b = "198.51.100.2:5000".to_string();
        let b_again = "198.51.100.2:5001".to_string();
        assert_eq!(tr.decide(&[a.clone(), b.clone()], t0), None);
        let first: Vec<_> = (1000..=20000)
            .step_by(1000)
            .filter_map(|ms| tr.decide(std::slice::from_ref(&a), at(t0, ms)))
            .collect();
        assert_eq!(first, [TrackerAction::Degraded { connected: 1, peak: 2, missing: vec![b] }]);
        assert_eq!(
            tr.decide(&[a.clone(), b_again.clone()], at(t0, 21_000)),
            Some(TrackerAction::Restored { connected: 2 })
        );
        let second: Vec<_> = (22_000..=40_000)
            .step_by(1000)
            .filter_map(|ms| tr.decide(std::slice::from_ref(&b_again), at(t0, ms)))
            .collect();
        assert_eq!(second, [TrackerAction::Degraded { connected: 1, peak: 2, missing: vec![a] }]);
    }

    #[test]
    fn listener_tracker_ignores_a_session_with_no_live_member() {
        let t0 = Instant::now();
        let (mut tr, _rx) = tracker();
        tr.decide(&["198.51.100.1:5000".into(), "198.51.100.2:5000".into()], t0);
        for ms in (1000..=30000).step_by(1000) {
            assert_eq!(tr.decide(&[], at(t0, ms)), None);
        }
    }
}
