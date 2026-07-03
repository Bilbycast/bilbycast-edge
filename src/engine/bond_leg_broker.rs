//! Shared-leg capacity broker — host-level, cross-flow fair-share arbiter
//! for multiple bonded flows that share the same physical uplink(s).
//!
//! ## Why
//!
//! Each bonded output builds its own [`CapacityAwareScheduler`] with private
//! per-leg congestion controllers. When N flows send over the SAME physical
//! NIC (e.g. several bonds all pinned to `eno4`), those N controllers each
//! discover the leg's capacity independently and each try to use it — the
//! leg is over-subscribed and divided by the (MIMD) control law's startup
//! ratio, not fairly. See `../bilbycast-bonding/docs/shared-leg-capacity-broker.md`.
//!
//! This broker replaces "N independent per-flow-per-leg controllers" with
//! "one auto-discovered capacity per *physical leg* + **priority-tiered**
//! reservation": it reads each flow's discovered demand on a leg (the
//! scheduler's `capacity_pub` estimate), hands the leg's capacity out tier by
//! tier — all `Critical`, then all `Normal`, then `BestEffort` — and writes
//! back each flow's **ceiling** (the scheduler's `ceiling_sub`), which the
//! token bucket then enforces. Within a tier it's weighted max-min fair.
//! Guaranteed flows reserve their smoothed VBR *peak* so the reservation holds
//! through a variable-bitrate stream's troughs. No capacity number is required
//! — an aggregate controller self-discovers each leg's rate; `bond_uplinks` is
//! only an optional hard cap for a metered link.
//!
//! ## Non-blocking contract
//!
//! ALL arithmetic runs on a single 100 ms timer task, never on a packet path.
//! The only data-path interaction is the scheduler's one relaxed `AtomicU64`
//! load of its ceiling per refill (already in `capacity_scheduler::refill_all`).
//! The registry is a [`DashMap`]; a leg's `Mutex` is held only during the tick
//! computation or a flow start/stop — never contended by the hot path.
//!
//! **On by default.** A lone flow on a leg (nothing to share with) is left
//! unconstrained (`ceiling = u64::MAX`) = byte-identical to pre-broker
//! behaviour, so the broker only bites once ≥ 2 flows genuinely share a leg.
//! `shared_leg_broker: false` disables it entirely.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dashmap::DashMap;

use crate::config::models::{BondPriority, BondUplinkConfig};
use crate::manager::events::{EventSender, EventSeverity};

/// Broker re-evaluation cadence. Matches the ~5-10 Hz keepalive/feedback
/// cadence of the per-flow controllers, so allocations track demand within a
/// tick or two without burning CPU.
const TICK: Duration = Duration::from_millis(100);

/// Default per-flow minimum-viable rate on a shared leg, bits/sec — used for
/// the admission-pressure signal (P3) when `Σ min_viable > capacity`.
pub const MIN_VIABLE_DEFAULT_BPS: u64 = 200_000;

/// `u64::MAX` sentinel written to a leg's ceiling atomic = "no broker
/// constraint" (the scheduler reads it as `INFINITY`).
const NO_CONSTRAINT: u64 = u64::MAX;

/// Ticks (× 100 ms) a member's demand keeps its `min_viable` floor after it
/// last moved real data — its activity grace window. A member that has been
/// idle/dead for this long releases its share so it can no longer hold a
/// ceiling it never uses (§9 stale-demand mitigation; prevents the
/// probe-up-lockout collapse spiral). 30 ≈ 3 s.
const GRACE_TICKS: u32 = 30;

/// A member delivering at least this fraction of its current ceiling is
/// treated as *saturating* its allocation → it wants more, so its demand is
/// its delivered rate scaled by [`DEMAND_GROWTH`]. Below it, demand = its
/// delivered rate (it wants only what it uses → work-conserving).
const DEMAND_SAT_FRAC: f64 = 0.9;
/// Growth allowance applied to a saturating member's demand so the broker
/// raises its ceiling next tick (25 %/tick → reaches fair share in a few
/// hundred ms).
const DEMAND_GROWTH: f64 = 1.25;
/// Delivered rate (bits/sec) above which a member is "active" and refreshes
/// its grace window. Small so any real traffic counts.
const DEMAND_ACTIVE_BPS: f64 = 50_000.0;
/// Per-tick decay of a member's smoothed-peak demand — the VBR reservation
/// window. A guaranteed flow reserves its recent *peak* rate, not its
/// instantaneous rate, so its reservation holds through the troughs of a VBR
/// stream and only relaxes when the stream genuinely calms down. At the 10 Hz
/// tick, 0.985 halves a stale peak in ~46 ticks (~4.6 s): long enough to ride
/// out a GOP-scale VBR dip, short enough that a real rate drop frees capacity
/// for other flows within a few seconds.
const PEAK_DECAY: f64 = 0.985;

// ─────────────────────────── pure allocator ───────────────────────────

/// One member's inputs to the allocator.
#[derive(Clone, Copy, Debug)]
pub struct MemberDemand {
    /// Relative fair-share weight (from `weight_hint`, ≥ 1). Only breaks ties
    /// *within* a priority tier — strict priority governs *between* tiers.
    pub weight: f64,
    /// The flow's demand on this leg, bits/sec. For a guaranteed flow this is
    /// its smoothed-peak (VBR-tracking) rate so the reservation holds through
    /// dips; for best-effort it's the same peak, capping how much spare it
    /// takes.
    pub demand_bps: f64,
    /// Operator hard cap (`max_bitrate_bps`), or `f64::INFINITY`.
    pub hard_cap_bps: f64,
    /// QoS tier. Capacity is handed out tier-by-tier — all `Critical` demand
    /// is satisfied (up to capacity) before any `Normal`, and all `Normal`
    /// before any `BestEffort`.
    pub priority: BondPriority,
}

/// Distribute `total` bits/sec among members by `weight`, each capped at
/// `cap[i]`, using progressive (water-filling) weighted max-min: a member
/// that saturates its cap below its weighted share releases the surplus to
/// the others. Returns allocations in input order. `Σ alloc == min(total,
/// Σ cap)` (within float epsilon). O(N²) worst case — N ≤ a handful of flows
/// per leg, so trivial.
fn weighted_maxmin(weights: &[f64], caps: &[f64], total: f64) -> Vec<f64> {
    let n = weights.len();
    let mut alloc = vec![0.0f64; n];
    if n == 0 || total <= 0.0 {
        return alloc;
    }
    let mut active: Vec<usize> = (0..n)
        .filter(|&i| weights[i] > 0.0 && caps[i] > 0.0)
        .collect();
    let mut remaining = total;
    // Each round either freezes ≥1 saturating member (and loops) or hands
    // everyone their weighted share (and exits) → ≤ N rounds.
    while remaining > 1e-6 && !active.is_empty() {
        let wsum: f64 = active.iter().map(|&i| weights[i]).sum();
        if wsum <= 0.0 {
            break;
        }
        let rate = remaining / wsum; // bps per unit weight this round
        let mut saturated_any = false;
        let mut next: Vec<usize> = Vec::with_capacity(active.len());
        for &i in &active {
            let room = caps[i] - alloc[i];
            if room <= weights[i] * rate + 1e-9 {
                // Saturates: give it exactly its remaining room and freeze.
                remaining -= room;
                alloc[i] = caps[i];
                saturated_any = true;
            } else {
                next.push(i);
            }
        }
        if saturated_any {
            active = next;
            continue;
        }
        // Nobody saturates → everyone takes their weighted share; done.
        for &i in &active {
            alloc[i] += weights[i] * rate;
        }
        break;
    }
    alloc
}

/// Priority-tiered fair-share allocation of a physical leg's `capacity_bps`
/// among its members. Capacity is handed out **tier by tier** in strict
/// priority order — `Critical`, then `Normal`, then `BestEffort` — and each
/// tier is split by demand-limited weighted max-min (each member capped at
/// `min(demand, hard_cap)`). So every guaranteed flow reserves its demand (its
/// smoothed VBR peak) ahead of any lower-priority flow, and best-effort flows
/// share only whatever the guaranteed tiers leave. Within a tier a light/idle
/// flow's surplus flows to flows that want more (work-conserving). Growth
/// headroom is NOT added here — the caller's demand already carries the VBR
/// peak, so adding headroom would double-count it and hand capacity to idle
/// members.
///
/// Guarantees: `Σ alloc = min(capacity, Σ min(demand, hard_cap)) ≤ capacity`
/// (even worst-case simultaneous bursting stays within leg capacity — no
/// over-subscription), each `alloc[i] ≤ hard_cap[i]`, and no member of a lower
/// tier is allocated anything a higher tier could have used.
pub fn allocate(members: &[MemberDemand], capacity_bps: f64) -> Vec<f64> {
    let n = members.len();
    // Reject non-finite / non-positive capacity (defense-in-depth: a NaN/INF
    // would propagate through the water-filling and silently un-clamp members).
    if n == 0 || !capacity_bps.is_finite() || capacity_bps <= 0.0 {
        return vec![0.0; n];
    }
    let mut alloc = vec![0.0f64; n];
    let mut remaining = capacity_bps;
    // Strict priority BETWEEN tiers; weighted max-min WITHIN a tier.
    for tier in [
        BondPriority::Critical,
        BondPriority::Normal,
        BondPriority::BestEffort,
    ] {
        let idxs: Vec<usize> = (0..n).filter(|&i| members[i].priority == tier).collect();
        if idxs.is_empty() {
            continue;
        }
        let weights: Vec<f64> = idxs.iter().map(|&i| members[i].weight.max(1e-9)).collect();
        let caps: Vec<f64> = idxs
            .iter()
            .map(|&i| members[i].demand_bps.min(members[i].hard_cap_bps).max(0.0))
            .collect();
        let tier_alloc = weighted_maxmin(&weights, &caps, remaining);
        for (k, &i) in idxs.iter().enumerate() {
            alloc[i] = tier_alloc[k];
            remaining -= tier_alloc[k];
        }
        if remaining <= 1e-6 {
            break;
        }
    }
    alloc
}

// ─────────────────────────── registry ───────────────────────────

/// One flow's participation on one physical leg.
struct Member {
    flow_id: u32,
    path_id: u8,
    weight: f64,
    hard_cap_bps: f64,
    /// QoS tier for this flow on this leg (from the bonded output's `priority`).
    /// Guaranteed tiers (`Critical`, `Normal`) reserve ahead of `BestEffort`.
    priority: BondPriority,
    /// Smoothed recent-peak demand, bits/sec — `max(current_demand, peak *
    /// PEAK_DECAY)` each tick. This is what the allocator reserves, so a
    /// guaranteed VBR flow holds its peak-rate reservation through the troughs
    /// instead of surrendering capacity every time its instantaneous rate dips.
    peak_demand_bps: f64,
    /// Broker READS: the scheduler's discovered per-leg capacity (demand).
    demand: Arc<AtomicU64>,
    /// Broker WRITES: this flow's fair-share ceiling for the leg.
    ceiling: Arc<AtomicU64>,
    /// P2 (adaptive C_L) — the flow's live sender-side PathStats
    /// (`throughput_bps` delivered rate + `loss_ppm`) on this leg.
    path_stats: Option<Arc<bonding_protocol::stats::PathStats>>,
    /// Remaining ticks of `min_viable` demand floor since the member last
    /// moved data (activity grace; see [`GRACE_TICKS`]). Decrements when idle
    /// so a long-idle/dead member releases its share.
    grace_ticks: u32,
    /// The broker's own (delivered-based) demand for this member at the last
    /// tick, bits/sec — reported as `demand` in the contention snapshot so the
    /// UI shows what the broker actually divides by, not the raw scheduler
    /// estimate (which is a fallback and can transiently spike).
    last_demand_bps: u64,
    /// This member's allocation (ceiling) at the last tick, bits/sec — reported
    /// in the contention snapshot so the UI can flag a flow as *protected*
    /// (alloc ≈ demand) or *degraded* (alloc < demand).
    last_alloc_bps: u64,
    /// Registration epoch. The [`LegRegGuard`] carries the same value so a
    /// stale guard (old flow task torn down *after* a restart re-registered)
    /// deregisters only its own instance, never the fresh one.
    generation: u64,
}

/// Per-physical-leg state.
struct LegState {
    members: Vec<Member>,
    cfg: Option<BondUplinkConfig>,
    /// Adaptive aggregate capacity estimate (P2), bits/sec. Seeded from the
    /// configured capacity; refined by the aggregate controller.
    est_bps: f64,
    /// Whether the last tick could not honour every member's min-viable rate.
    oversubscribed: bool,
    /// `oversubscribed` at the previous tick — edge-triggers the event so it
    /// fires once per transition, not every 100 ms.
    was_oversubscribed: bool,
    /// Aggregate unmet demand at the last tick (bits/sec) — telemetry across
    /// all tiers (best-effort shortfall is expected and does NOT alarm).
    unmet_bps: u64,
    /// Unmet demand of the GUARANTEED tiers (Critical + Normal) at the last
    /// tick, bits/sec. Non-zero means a must-have flow was starved — this is
    /// what drives the over-subscription alarm (`oversubscribed`), not the
    /// best-effort shortfall.
    guaranteed_unmet_bps: u64,
}

impl LegState {
    fn configured_capacity(&self) -> Option<f64> {
        self.cfg.as_ref().map(|c| c.capacity_bps as f64)
    }
    fn min_viable(&self) -> u64 {
        self.cfg
            .as_ref()
            .and_then(|c| c.min_viable_bps)
            .unwrap_or(MIN_VIABLE_DEFAULT_BPS)
    }
}

/// One member's registration inputs.
pub struct MemberReg {
    pub leg_key: String,
    pub flow_id: u32,
    pub path_id: u8,
    pub weight_hint: u32,
    pub max_bitrate_bps: Option<u64>,
    /// QoS tier for this bonded flow (from the output's `priority`, default
    /// `BestEffort`). Governs how the shared-leg broker reserves capacity.
    pub priority: BondPriority,
    pub demand: Arc<AtomicU64>,
    pub ceiling: Arc<AtomicU64>,
    /// The leg's live sender-side `PathStats` for the P2 aggregate capacity
    /// controller. `None` → adaptive discovery off for this member (P1
    /// configured-capacity division still applies).
    pub path_stats: Option<Arc<bonding_protocol::stats::PathStats>>,
}

/// RAII guard: deregisters this flow's members from the broker on drop
/// (bonded output task teardown / cancellation). Each entry carries the
/// registration `generation` so a stale guard never removes a member a later
/// re-registration installed for the same `(flow_id, path_id)`.
pub struct LegRegGuard {
    entries: Vec<(String, u32, u8, u64)>,
}

impl Drop for LegRegGuard {
    fn drop(&mut self) {
        broker().deregister(&self.entries);
    }
}

/// Host-level singleton.
pub struct LegBroker {
    enabled: AtomicBool,
    legs: DashMap<String, Mutex<LegState>>,
    /// Configured physical-leg capacities, keyed by leg key (from
    /// `bond_uplinks`).
    configs: DashMap<String, BondUplinkConfig>,
    started: AtomicBool,
    /// Event sink for admission-pressure events (P3). Set once at startup;
    /// read only on the tick timer, never on the hot path.
    events: Mutex<Option<EventSender>>,
    /// Monotonic registration epoch (see [`Member::generation`]).
    next_gen: AtomicU64,
}

static BROKER: OnceLock<LegBroker> = OnceLock::new();

/// The process-global shared-leg capacity broker.
pub fn broker() -> &'static LegBroker {
    BROKER.get_or_init(LegBroker::new)
}

impl LegBroker {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            legs: DashMap::new(),
            configs: DashMap::new(),
            started: AtomicBool::new(false),
            events: Mutex::new(None),
            next_gen: AtomicU64::new(1),
        }
    }

    /// Install the event sink (called once at startup after the event channel
    /// exists). Used to surface admission-pressure events.
    pub fn set_event_sender(&self, tx: EventSender) {
        if let Ok(mut g) = self.events.lock() {
            *g = Some(tx);
        }
    }

    /// P4 (advisory): would admitting a new flow's `regs` push any shared leg
    /// below the
    /// point where every co-resident (plus the newcomer) can get its
    /// min-viable rate? Returns the first offending leg key. Advisory unless
    /// [`Self::set_strict_admission`] is on (the caller decides).
    pub fn admission_conflict(&self, regs: &[MemberReg]) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        for r in regs {
            let Some(cap) = self
                .configs
                .get(&r.leg_key)
                .map(|c| c.capacity_bps)
                .or_else(|| {
                    self.legs.get(&r.leg_key).and_then(|l| {
                        l.lock().ok().map(|s| s.est_bps.max(0.0) as u64)
                    })
                })
            else {
                continue;
            };
            let existing = self
                .legs
                .get(&r.leg_key)
                .and_then(|l| l.lock().ok().map(|s| s.members.len()))
                .unwrap_or(0);
            let min_viable = self
                .configs
                .get(&r.leg_key)
                .and_then(|c| c.min_viable_bps)
                .unwrap_or(MIN_VIABLE_DEFAULT_BPS);
            if cap > 0 && (existing as u64 + 1) * min_viable > cap {
                return Some(r.leg_key.clone());
            }
        }
        None
    }

    /// Whether the broker is active (any uplinks configured / explicitly on).
    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Install host-level uplink capacities and enable/disable the broker.
    /// Called once at startup from the loaded [`AppConfig`]. **Enabled by
    /// default** (`shared_leg_broker` unset) — capacity is auto-discovered per
    /// leg and a lone flow on an undeclared leg is left unconstrained, so the
    /// broker is a no-op until two flows actually share a leg. `bond_uplinks`
    /// are then just an optional hard-cap/hint. Operators disable it explicitly
    /// with `shared_leg_broker: false`.
    pub fn configure(&self, uplinks: &[BondUplinkConfig], enabled_override: Option<bool>) {
        self.configs.clear();
        for u in uplinks {
            self.configs.insert(leg_key_for_interface(&u.interface), u.clone());
        }
        let enabled = enabled_override.unwrap_or(true);
        self.enabled.store(enabled, Ordering::Relaxed);
        // Refresh any already-registered legs' configs (config reload).
        if enabled {
            for leg in self.legs.iter() {
                let key = leg.key().clone();
                if let Ok(mut st) = leg.value().lock() {
                    st.cfg = self.configs.get(&key).map(|c| c.clone());
                    if let Some(cap) = st.configured_capacity() {
                        if st.est_bps <= 0.0 {
                            st.est_bps = cap;
                        }
                    }
                }
            }
        } else {
            // Disable: revert every brokered leg to unconstrained so no flow is
            // left frozen at a stale throttle (broker off == pre-broker).
            for leg in self.legs.iter() {
                if let Ok(mut st) = leg.value().lock() {
                    for m in &st.members {
                        m.ceiling.store(NO_CONSTRAINT, Ordering::Relaxed);
                    }
                    st.cfg = None;
                    st.est_bps = 0.0;
                    st.oversubscribed = false;
                    st.was_oversubscribed = false;
                }
            }
        }
    }

    /// Register a flow's legs. Returns a guard that deregisters on drop.
    /// A no-op (empty guard) when the broker is disabled. Takes `&'static
    /// self` because it lazily spawns the timer task (which captures the
    /// broker); the process-global [`broker()`] handle satisfies this.
    pub fn register(&'static self, members: Vec<MemberReg>) -> LegRegGuard {
        if !self.enabled() || members.is_empty() {
            return LegRegGuard { entries: Vec::new() };
        }
        self.ensure_started();
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let mut entries = Vec::with_capacity(members.len());
        for m in members {
            entries.push((m.leg_key.clone(), m.flow_id, m.path_id, generation));
            let cfg = self.configs.get(&m.leg_key).map(|c| c.clone());
            let seed = cfg.as_ref().map(|c| c.capacity_bps as f64).unwrap_or(0.0);
            let mut entry = self.legs.entry(m.leg_key.clone()).or_insert_with(|| {
                Mutex::new(LegState {
                    members: Vec::new(),
                    cfg: cfg.clone(),
                    est_bps: seed,
                    oversubscribed: false,
                    was_oversubscribed: false,
                    unmet_bps: 0,
                    guaranteed_unmet_bps: 0,
                })
            });
            if let Ok(mut st) = entry.value_mut().lock() {
                // Replace any stale registration for the same (flow, path).
                st.members
                    .retain(|x| !(x.flow_id == m.flow_id && x.path_id == m.path_id));
                st.members.push(Member {
                    flow_id: m.flow_id,
                    path_id: m.path_id,
                    weight: (m.weight_hint.max(1)) as f64,
                    hard_cap_bps: m
                        .max_bitrate_bps
                        .map(|c| c as f64)
                        .unwrap_or(f64::INFINITY),
                    priority: m.priority,
                    peak_demand_bps: 0.0,
                    demand: m.demand,
                    ceiling: m.ceiling,
                    path_stats: m.path_stats,
                    grace_ticks: GRACE_TICKS,
                    last_demand_bps: 0,
                    last_alloc_bps: 0,
                    generation,
                });
            }
        }
        LegRegGuard { entries }
    }

    fn deregister(&self, entries: &[(String, u32, u8, u64)]) {
        for (key, flow_id, path_id, generation) in entries {
            if let Some(leg) = self.legs.get(key) {
                if let Ok(mut st) = leg.value().lock() {
                    // Match on generation too: never remove a member that a
                    // later re-registration installed for the same (flow, path).
                    st.members.retain(|x| {
                        !(x.flow_id == *flow_id
                            && x.path_id == *path_id
                            && x.generation == *generation)
                    });
                }
            }
        }
        // Drop now-empty legs so the map doesn't grow unbounded.
        self.legs
            .retain(|_, v| v.lock().map(|s| !s.members.is_empty()).unwrap_or(false));
    }

    /// Spawn the single re-evaluation timer once, lazily, on first register
    /// (we're in an async/tokio context there).
    fn ensure_started(&'static self) {
        if self
            .started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(TICK);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    iv.tick().await;
                    self.tick();
                }
            });
        }
    }

    /// One re-evaluation pass: per leg, read demands, water-fill the leg's
    /// capacity by weight, write each member's fair-share ceiling. Runs off
    /// the hot path; each leg's `Mutex` is held only for this leg's tiny
    /// computation.
    fn tick(&self) {
        if !self.enabled() {
            return;
        }
        for leg in self.legs.iter() {
            let key = leg.key().clone();
            let mut st = match leg.value().lock() {
                Ok(g) => g,
                Err(_) => continue,
            };
            self.tick_leg(&key, &mut st);
        }
    }

    /// Reconcile the over-subscription alarm and emit the edge-triggered
    /// event. Called from EVERY tick path (incl. the early returns) so the
    /// alarm always clears when contention ends — never latches.
    fn emit_oversub(&self, key: &str, st: &mut LegState, capacity: f64) {
        if st.oversubscribed == st.was_oversubscribed {
            return;
        }
        if let Ok(g) = self.events.lock() {
            if let Some(ev) = g.as_ref() {
                if st.oversubscribed {
                    ev.emit_with_details(
                        EventSeverity::Warning,
                        "bonding",
                        format!(
                            "shared uplink '{key}' can't honour its priority flows: {:.1} Mbps short of the reserved demand of its Critical/Normal flow(s) (capacity {:.1} Mbps, {} bonded flow(s))",
                            st.guaranteed_unmet_bps as f64 / 1_000_000.0,
                            capacity / 1_000_000.0,
                            st.members.len(),
                        ),
                        None,
                        serde_json::json!({
                            "error_code": "bond_leg_oversubscribed",
                            "leg": key,
                            "capacity_bps": capacity as u64,
                            "member_count": st.members.len(),
                            "unmet_bps": st.unmet_bps,
                            "guaranteed_unmet_bps": st.guaranteed_unmet_bps,
                            "min_viable_bps": st.min_viable(),
                        }),
                    );
                } else {
                    ev.emit_with_details(
                        EventSeverity::Info,
                        "bonding",
                        format!("shared uplink '{key}' no longer over-subscribed"),
                        None,
                        serde_json::json!({
                            "error_code": "bond_leg_oversubscribed_cleared",
                            "leg": key,
                        }),
                    );
                }
            }
        }
        st.was_oversubscribed = st.oversubscribed;
    }

    fn tick_leg(&self, key: &str, st: &mut LegState) {
        if st.members.is_empty() {
            return;
        }
        // Coordinate a leg only when it is genuinely contended (≥ 2 flows) or
        // has a hard configured capacity. A lone flow on an undeclared leg is
        // left unconstrained — there is nothing to be fair about and no reason
        // to throttle it below what it could discover on its own.
        if st.cfg.is_none() && st.members.len() < 2 {
            for m in &st.members {
                m.ceiling.store(NO_CONSTRAINT, Ordering::Relaxed);
            }
            st.oversubscribed = false;
            st.unmet_bps = 0;
            st.est_bps = 0.0;
            self.emit_oversub(key, st, 0.0);
            return;
        }
        // Effective capacity: adaptive estimate (P2) when we have one, else
        // the configured value. No capacity at all → leave every member
        // unconstrained (pre-broker behaviour) and bail.
        let capacity = self.effective_capacity(st);
        let Some(capacity) = capacity else {
            for m in &st.members {
                m.ceiling.store(NO_CONSTRAINT, Ordering::Relaxed);
            }
            st.oversubscribed = false;
            st.unmet_bps = 0;
            self.emit_oversub(key, st, 0.0);
            return;
        };

        let min_viable = st.min_viable() as f64;
        // Per-uplink activity-grace threshold (config override, else default).
        let demand_active = st
            .cfg
            .as_ref()
            .and_then(|c| c.demand_active_bps)
            .map(|v| v as f64)
            .unwrap_or(DEMAND_ACTIVE_BPS);
        let demands: Vec<MemberDemand> = st
            .members
            .iter_mut()
            .map(|m| {
                // Prefer the leg's measured delivered rate (always fresh: a
                // dead/idle leg decays to ~0) over the discovery estimate
                // (which freezes high on an idle/dead leg and latches at ~2×
                // the ceiling when throttled — both would let a non-delivering
                // member hoard allocation and block the aggregate probe-up).
                let delivered = m
                    .path_stats
                    .as_ref()
                    .map(|ps| ps.throughput_bps.load(Ordering::Relaxed))
                    .unwrap_or_else(|| m.demand.load(Ordering::Relaxed))
                    as f64;
                // Refresh / decay the activity grace window.
                if delivered >= demand_active {
                    m.grace_ticks = GRACE_TICKS;
                } else if m.grace_ticks > 0 {
                    m.grace_ticks -= 1;
                }
                let cur = m.ceiling.load(Ordering::Relaxed);
                let cur_ceiling = if cur == NO_CONSTRAINT {
                    f64::INFINITY
                } else {
                    cur as f64
                };
                // Saturating its allocation → wants more; else wants what it
                // uses (work-conserving); no allocation yet → bootstrap floor.
                let mut d = if cur_ceiling.is_finite()
                    && delivered >= DEMAND_SAT_FRAC * cur_ceiling
                {
                    delivered * DEMAND_GROWTH
                } else if cur_ceiling.is_infinite() {
                    delivered.max(min_viable)
                } else {
                    delivered
                };
                // Hold the min-viable floor only while (recently) active or in
                // the bootstrap grace — a long-idle/dead member releases its
                // share (§9 stale-demand mitigation), unblocking the probe-up.
                if m.grace_ticks > 0 {
                    d = d.max(min_viable);
                }
                // VBR reservation: reserve the recent *peak* rate, not the
                // instantaneous one, so a guaranteed flow holds its ceiling
                // through the troughs of a variable-bitrate stream. The peak
                // decays slowly (`PEAK_DECAY`) so a genuine rate drop still
                // frees the capacity for other flows within a few seconds.
                //
                // Peak-hold applies only while the member is (recently) active
                // — the same activity-grace window used above. A member that
                // has been idle/dead long enough to drain its grace releases
                // its peak immediately (drops to its current demand), so a dead
                // flow can't hoard a stale reservation and block probe-up.
                m.peak_demand_bps = if m.grace_ticks > 0 {
                    d.max(m.peak_demand_bps * PEAK_DECAY)
                } else {
                    d
                };
                let want = m.peak_demand_bps;
                m.last_demand_bps = want.max(0.0) as u64;
                MemberDemand {
                    weight: m.weight,
                    demand_bps: want,
                    hard_cap_bps: m.hard_cap_bps,
                    priority: m.priority,
                }
            })
            .collect();
        let alloc = allocate(&demands, capacity);

        // Telemetry: total unmet demand across all tiers (best-effort shortfall
        // is expected and does NOT alarm), plus the GUARANTEED shortfall — the
        // part that broke a Critical/Normal reservation, which drives the alarm.
        let total_demand: f64 = demands.iter().map(|d| d.demand_bps.min(d.hard_cap_bps)).sum();
        let total_alloc: f64 = alloc.iter().sum();
        st.unmet_bps = (total_demand - total_alloc).max(0.0) as u64;
        let guaranteed_unmet: f64 = demands
            .iter()
            .zip(alloc.iter())
            .filter(|(d, _)| matches!(d.priority, BondPriority::Critical | BondPriority::Normal))
            .map(|(d, a)| (d.demand_bps.min(d.hard_cap_bps) - a).max(0.0))
            .sum();
        st.guaranteed_unmet_bps = guaranteed_unmet as u64;

        // Alarm = a GUARANTEED (Critical/Normal) flow was starved below its
        // reserved demand. Best-effort shortfall is by-design and stays quiet.
        // Hysteresis on the shortfall (enter > 2 % of capacity, leave < 0.5 %)
        // so a VBR flow grazing its reservation edge doesn't flap the alarm.
        st.oversubscribed = if st.was_oversubscribed {
            guaranteed_unmet > capacity * 0.005
        } else {
            guaranteed_unmet > capacity * 0.02
        };

        for (m, a) in st.members.iter_mut().zip(alloc.iter()) {
            // Never publish below 1 bps; clamp to u64.
            let v = a.max(0.0).min(u64::MAX as f64) as u64;
            m.ceiling.store(v.max(1), Ordering::Relaxed);
            m.last_alloc_bps = v;
        }

        self.emit_oversub(key, st, capacity);
    }

    /// The capacity to divide for this leg. P1: the configured value. P2
    /// refines it adaptively (see `adapt_capacity`). `None` → no capacity
    /// known, leave members unconstrained.
    fn effective_capacity(&self, st: &mut LegState) -> Option<f64> {
        // P2 aggregate-capacity discovery runs here; for now it seeds/holds
        // `est_bps` at the configured value.
        self.adapt_capacity(st);
        if st.est_bps > 0.0 {
            Some(st.est_bps)
        } else {
            st.configured_capacity()
        }
    }

    /// P2 — **one** aggregate loss+delivered-rate congestion controller per
    /// physical leg (replacing N per-flow probes on the shared bottleneck).
    /// Discovers the leg's real usable capacity from the *aggregate* behaviour
    /// of all flows on it: probe up (slew-limited) while the aggregate is
    /// clean and capacity-limited, back off toward the aggregate delivered
    /// rate the moment aggregate loss appears. A configured `capacity_bps` is
    /// an upper bound the estimate never probes past (and the seed); with no
    /// config it self-discovers from delivered evidence.
    fn adapt_capacity(&self, st: &mut LegState) {
        // Aggregate delivered rate + delivery-weighted loss across members.
        let mut agg_delivered = 0.0f64;
        let mut weighted_loss = 0.0f64;
        let mut loss_weight = 0.0f64;
        let mut have_feedback = false;
        for m in &st.members {
            if let Some(ps) = &m.path_stats {
                have_feedback = true;
                let d = ps.throughput_bps.load(Ordering::Relaxed) as f64;
                let l = ps.loss_ppm.load(Ordering::Relaxed) as f64 / 1_000_000.0;
                agg_delivered += d;
                let w = d.max(1.0);
                weighted_loss += l * w;
                loss_weight += w;
            }
        }
        let agg_loss = if loss_weight > 0.0 { weighted_loss / loss_weight } else { 0.0 };

        let configured = st.configured_capacity();

        // Seed the estimate: configured value, else the current aggregate
        // delivered rate. Still zero (no config, no delivery) → leave 0 so
        // the tick leaves members unconstrained (pre-broker behaviour).
        if st.est_bps <= 0.0 {
            st.est_bps = configured.unwrap_or(agg_delivered).max(0.0);
            if st.est_bps <= 0.0 {
                return;
            }
        }

        if have_feedback {
            // Loss thresholds + gains mirror the per-leg controller so the two
            // agree; applied to the AGGREGATE so N flows share one probe.
            const LOSS_LOW: f64 = 0.005;
            const LOSS_HIGH: f64 = 0.03;
            const PROBE_UP: f64 = 1.05; // per 100 ms tick — gentle slew
            const BACKOFF: f64 = 0.9;
            const CLEAN_FRAC: f64 = 0.85;
            if agg_loss > LOSS_HIGH {
                st.est_bps = (agg_delivered * BACKOFF).max(st.min_viable() as f64);
            } else if agg_loss < LOSS_LOW && agg_delivered >= CLEAN_FRAC * st.est_bps {
                let up = st.est_bps * PROBE_UP;
                st.est_bps = match configured {
                    Some(c) => up.min(c),
                    None => up,
                };
            }
            // else hold.
        }

        // Never below a nominal floor; never above the configured safe bound.
        st.est_bps = st.est_bps.max(1000.0);
        if let Some(c) = configured {
            st.est_bps = st.est_bps.min(c);
        }
    }

    /// Snapshot per-leg contention for telemetry (P3).
    pub fn contention_snapshot(&self) -> Vec<LegContention> {
        let mut out = Vec::new();
        for leg in self.legs.iter() {
            if let Ok(st) = leg.value().lock() {
                if st.members.is_empty() {
                    continue;
                }
                out.push(LegContention {
                    leg_key: leg.key().clone(),
                    capacity_bps: st.est_bps.max(0.0) as u64,
                    member_count: st.members.len(),
                    oversubscribed: st.oversubscribed,
                    unmet_bps: st.unmet_bps,
                    guaranteed_unmet_bps: st.guaranteed_unmet_bps,
                    members: st
                        .members
                        .iter()
                        .map(|m| {
                            let ceil = m.ceiling.load(Ordering::Relaxed);
                            // An unconstrained (lone-flow) leg reports the flow
                            // getting exactly what it wants, not the u64::MAX
                            // sentinel.
                            let alloc = if ceil == NO_CONSTRAINT {
                                m.last_demand_bps
                            } else {
                                ceil
                            };
                            // Protected = the flow got (approximately) its full
                            // demand. A guaranteed flow marked unprotected is a
                            // broken reservation; a best-effort one is just
                            // sharing the spare.
                            let protected = alloc as f64 + 1.0 >= 0.95 * m.last_demand_bps as f64;
                            LegMemberContention {
                                flow_id: m.flow_id,
                                path_id: m.path_id,
                                weight: m.weight as u32,
                                priority: bond_priority_str(m.priority).to_string(),
                                // The broker's own (delivered-based) demand, not
                                // the raw scheduler estimate — see
                                // Member::last_demand_bps.
                                demand_bps: m.last_demand_bps,
                                allocation_bps: alloc,
                                protected,
                            }
                        })
                        .collect(),
                });
            }
        }
        out
    }
}

/// Per-leg contention telemetry (P3 — surfaced on `/api/v1/stats` and to the
/// manager UI).
#[derive(Clone, Debug, serde::Serialize)]
pub struct LegContention {
    pub leg_key: String,
    pub capacity_bps: u64,
    pub member_count: usize,
    /// A guaranteed (Critical/Normal) flow on this leg is starved below its
    /// reserved demand. Best-effort shortfall does NOT set this.
    pub oversubscribed: bool,
    pub unmet_bps: u64,
    /// Shortfall against the guaranteed tiers only (bits/sec) — the part of
    /// `unmet_bps` that broke a Critical/Normal reservation.
    pub guaranteed_unmet_bps: u64,
    pub members: Vec<LegMemberContention>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct LegMemberContention {
    pub flow_id: u32,
    pub path_id: u8,
    pub weight: u32,
    /// QoS tier: `"critical"` / `"normal"` / `"best_effort"`.
    pub priority: String,
    pub demand_bps: u64,
    pub allocation_bps: u64,
    /// The flow got (approximately) its full demand this tick.
    pub protected: bool,
}

/// Wire string for a [`BondPriority`] — matches the `#[serde(rename_all =
/// "snake_case")]` form so the manager UI can compare against the config value.
fn bond_priority_str(p: BondPriority) -> &'static str {
    match p {
        BondPriority::Critical => "critical",
        BondPriority::Normal => "normal",
        BondPriority::BestEffort => "best_effort",
    }
}

/// Canonical physical-leg key from an interface name. Kept in one place so
/// the config side (`bond_uplinks[].interface`) and the registration side
/// (`bond_leg_probe::LegKey`) agree.
pub fn leg_key_for_interface(interface: &str) -> String {
    format!("if:{interface}")
}

/// Canonical key from a `bond_leg_probe::LegKey` (interface wins; else source;
/// else gateway). `None` when the leg has no physical pin — it rides the
/// default route and can't be identified, so it isn't brokered (unconstrained,
/// pre-broker behaviour).
pub fn leg_key_from_probe(k: &crate::engine::bond_leg_probe::LegKey) -> Option<String> {
    if let Some(i) = &k.interface {
        Some(format!("if:{i}"))
    } else if let Some(s) = &k.source_ip {
        Some(format!("src:{s}"))
    } else {
        k.gateway_ip.as_ref().map(|g| format!("gw:{g}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(weight: f64, demand: f64, cap: f64) -> MemberDemand {
        MemberDemand {
            weight,
            demand_bps: demand,
            hard_cap_bps: cap,
            priority: BondPriority::BestEffort,
        }
    }

    /// Same as [`m`] but with an explicit priority tier.
    fn mp(weight: f64, demand: f64, cap: f64, priority: BondPriority) -> MemberDemand {
        MemberDemand { weight, demand_bps: demand, hard_cap_bps: cap, priority }
    }

    const INF: f64 = f64::INFINITY;

    #[test]
    fn single_member_gets_its_demand() {
        // A lone flow is capped at what it wants (demand); its demand carries
        // its own per-tick growth so it ramps rather than being pinned.
        let a = allocate(&[m(1.0, 500_000.0, INF)], 2_000_000.0);
        assert!((a[0] - 500_000.0).abs() < 1.0, "single flow gets its demand: {}", a[0]);
        // When it wants more than the leg, it's capped at the leg capacity.
        let a2 = allocate(&[m(1.0, 9_000_000.0, INF)], 2_000_000.0);
        assert!((a2[0] - 2_000_000.0).abs() < 1.0, "capped at leg capacity: {}", a2[0]);
    }

    #[test]
    fn two_equal_flows_split_evenly_under_contention() {
        // Both want more than half → 50/50.
        let a = allocate(&[m(1.0, 3_000_000.0, INF), m(1.0, 3_000_000.0, INF)], 2_000_000.0);
        assert!((a[0] - 1_000_000.0).abs() < 1.0, "{a:?}");
        assert!((a[1] - 1_000_000.0).abs() < 1.0, "{a:?}");
    }

    #[test]
    fn weighting_is_proportional() {
        // weights 3:1, both demand-saturating → 75/25.
        let a = allocate(&[m(3.0, 9_000_000.0, INF), m(1.0, 9_000_000.0, INF)], 4_000_000.0);
        assert!((a[0] - 3_000_000.0).abs() < 1.0, "{a:?}");
        assert!((a[1] - 1_000_000.0).abs() < 1.0, "{a:?}");
    }

    #[test]
    fn critical_reserves_ahead_of_best_effort() {
        // 10 Mbps leg. Critical wants 6, best-effort wants 8. Critical is fully
        // reserved first; best-effort gets only the 4 Mbps left.
        let a = allocate(
            &[
                mp(1.0, 6_000_000.0, INF, BondPriority::Critical),
                mp(1.0, 8_000_000.0, INF, BondPriority::BestEffort),
            ],
            10_000_000.0,
        );
        assert!((a[0] - 6_000_000.0).abs() < 1.0, "critical fully reserved: {a:?}");
        assert!((a[1] - 4_000_000.0).abs() < 1.0, "best-effort gets remainder: {a:?}");
    }

    #[test]
    fn critical_starves_best_effort_when_leg_full() {
        // Critical alone wants the whole leg → best-effort gets nothing.
        let a = allocate(
            &[
                mp(1.0, 10_000_000.0, INF, BondPriority::Critical),
                mp(1.0, 5_000_000.0, INF, BondPriority::BestEffort),
            ],
            10_000_000.0,
        );
        assert!((a[0] - 10_000_000.0).abs() < 1.0, "critical takes all: {a:?}");
        assert!(a[1] < 1.0, "best-effort starved: {a:?}");
    }

    #[test]
    fn strict_priority_across_three_tiers() {
        // 10 Mbps. Critical 4, Normal 4, BestEffort 4 — total 12 > 10.
        // Critical + Normal fully reserved (8), best-effort gets the last 2.
        let a = allocate(
            &[
                mp(1.0, 4_000_000.0, INF, BondPriority::Critical),
                mp(1.0, 4_000_000.0, INF, BondPriority::Normal),
                mp(1.0, 4_000_000.0, INF, BondPriority::BestEffort),
            ],
            10_000_000.0,
        );
        assert!((a[0] - 4_000_000.0).abs() < 1.0, "critical: {a:?}");
        assert!((a[1] - 4_000_000.0).abs() < 1.0, "normal: {a:?}");
        assert!((a[2] - 2_000_000.0).abs() < 1.0, "best-effort remainder: {a:?}");
    }

    #[test]
    fn normal_yields_to_critical_but_beats_best_effort() {
        // 6 Mbps leg. Critical 4, Normal 4, BestEffort 4. Critical reserved
        // first (4), Normal gets the remaining 2, best-effort gets nothing.
        let a = allocate(
            &[
                mp(1.0, 4_000_000.0, INF, BondPriority::Critical),
                mp(1.0, 4_000_000.0, INF, BondPriority::Normal),
                mp(1.0, 4_000_000.0, INF, BondPriority::BestEffort),
            ],
            6_000_000.0,
        );
        assert!((a[0] - 4_000_000.0).abs() < 1.0, "critical full: {a:?}");
        assert!((a[1] - 2_000_000.0).abs() < 1.0, "normal gets remainder: {a:?}");
        assert!(a[2] < 1.0, "best-effort starved: {a:?}");
    }

    #[test]
    fn within_tier_is_weighted_fair() {
        // Two Critical flows, weights 3:1, both saturating, on a 4 Mbps leg with
        // no lower tiers → 75/25 (tiering doesn't change intra-tier fairness).
        let a = allocate(
            &[
                mp(3.0, 9_000_000.0, INF, BondPriority::Critical),
                mp(1.0, 9_000_000.0, INF, BondPriority::Critical),
            ],
            4_000_000.0,
        );
        assert!((a[0] - 3_000_000.0).abs() < 1.0, "{a:?}");
        assert!((a[1] - 1_000_000.0).abs() < 1.0, "{a:?}");
    }

    #[test]
    fn guaranteed_flows_fit_leaves_best_effort_the_rest() {
        // Under-committed: Critical 2 + Normal 2 = 4 on a 10 Mbps leg. Both
        // guaranteed satisfied; best-effort (wants 8) takes the 6 Mbps spare.
        let a = allocate(
            &[
                mp(1.0, 2_000_000.0, INF, BondPriority::Critical),
                mp(1.0, 2_000_000.0, INF, BondPriority::Normal),
                mp(1.0, 8_000_000.0, INF, BondPriority::BestEffort),
            ],
            10_000_000.0,
        );
        assert!((a[0] - 2_000_000.0).abs() < 1.0, "critical: {a:?}");
        assert!((a[1] - 2_000_000.0).abs() < 1.0, "normal: {a:?}");
        assert!((a[2] - 6_000_000.0).abs() < 1.0, "best-effort spare: {a:?}");
    }

    #[test]
    fn light_flow_surplus_goes_to_heavy_flow() {
        // Light flow wants 200k; heavy flow wants everything. Work-conserving:
        // heavy gets capacity - 200k, NOT just its 50% share.
        let a = allocate(&[m(1.0, 200_000.0, INF), m(1.0, 9_000_000.0, INF)], 2_000_000.0);
        assert!((a[0] - 200_000.0).abs() < 1.0, "light gets exactly its demand: {a:?}");
        assert!((a[1] - 1_800_000.0).abs() < 1.0, "heavy reclaims the surplus: {a:?}");
    }

    #[test]
    fn hard_cap_bounds_a_flow_and_spills() {
        // Flow 0 capped at 500k even though weight/demand would give more.
        let a = allocate(&[m(1.0, 9_000_000.0, 500_000.0), m(1.0, 9_000_000.0, INF)], 2_000_000.0);
        assert!((a[0] - 500_000.0).abs() < 1.0, "capped: {a:?}");
        assert!((a[1] - 1_500_000.0).abs() < 1.0, "spill to sibling: {a:?}");
    }

    #[test]
    fn never_exceeds_capacity() {
        // Ten hungry flows on a small leg: Σ ≤ capacity, always.
        let members: Vec<MemberDemand> = (0..10).map(|_| m(1.0, 5_000_000.0, INF)).collect();
        let cap = 2_500_000.0;
        let a = allocate(&members, cap);
        let sum: f64 = a.iter().sum();
        assert!(sum <= cap + 1.0, "Σ {sum} must not exceed capacity {cap}");
        // Equal weights → equal shares.
        for x in &a {
            assert!((x - 250_000.0).abs() < 1.0, "{a:?}");
        }
    }

    #[test]
    fn undersubscribed_gets_exactly_demand() {
        // Both flows light → each gets exactly its demand (no headroom handed
        // to idle members), Σ ≤ capacity.
        let a = allocate(&[m(1.0, 300_000.0, INF), m(1.0, 500_000.0, INF)], 4_000_000.0);
        assert!((a[0] - 300_000.0).abs() < 1.0 && (a[1] - 500_000.0).abs() < 1.0, "{a:?}");
        let sum: f64 = a.iter().sum();
        assert!(sum <= 4_000_000.0 + 1.0, "Σ {sum} ≤ capacity");
    }

    #[test]
    fn zero_capacity_allocates_nothing() {
        let a = allocate(&[m(1.0, 1_000_000.0, INF)], 0.0);
        assert_eq!(a, vec![0.0]);
    }

    fn member(flow_id: u32, ps: &Arc<bonding_protocol::stats::PathStats>) -> Member {
        member_pri(flow_id, ps, BondPriority::BestEffort)
    }

    fn member_pri(
        flow_id: u32,
        ps: &Arc<bonding_protocol::stats::PathStats>,
        priority: BondPriority,
    ) -> Member {
        Member {
            flow_id,
            path_id: 0,
            weight: 1.0,
            hard_cap_bps: f64::INFINITY,
            priority,
            peak_demand_bps: 0.0,
            demand: Arc::new(AtomicU64::new(5_000_000)),
            ceiling: Arc::new(AtomicU64::new(NO_CONSTRAINT)),
            path_stats: Some(ps.clone()),
            grace_ticks: GRACE_TICKS,
            last_demand_bps: 0,
            last_alloc_bps: 0,
            generation: 1,
        }
    }

    /// P2: one aggregate controller per leg seeds from aggregate delivered,
    /// probes up while clean + capacity-limited, and backs off on aggregate
    /// loss toward the delivered rate.
    #[test]
    fn aggregate_controller_seeds_probes_and_backs_off() {
        let b = LegBroker::new();
        let ps0 = bonding_protocol::stats::PathStats::new();
        let ps1 = bonding_protocol::stats::PathStats::new();
        // Two flows share the leg, no configured cap → fully adaptive.
        let mut st = LegState {
            members: vec![member(0, &ps0), member(1, &ps1)],
            cfg: None,
            est_bps: 0.0,
            oversubscribed: false,
            was_oversubscribed: false,
            unmet_bps: 0,
            guaranteed_unmet_bps: 0,
        };
        // Both deliver 1 Mbps cleanly → aggregate 2 Mbps.
        ps0.throughput_bps.store(1_000_000, Ordering::Relaxed);
        ps1.throughput_bps.store(1_000_000, Ordering::Relaxed);

        b.adapt_capacity(&mut st);
        let seeded = st.est_bps;
        assert!(seeded >= 2_000_000.0, "seeds from aggregate delivered: {seeded}");

        // Clean + capacity-limited (delivered ≈ est) → probes up.
        for _ in 0..5 {
            b.adapt_capacity(&mut st);
        }
        assert!(st.est_bps > seeded, "clean aggregate probes up: {seeded} -> {}", st.est_bps);

        // Inject 10 % loss on both legs → back off toward aggregate delivered.
        let high = st.est_bps;
        ps0.loss_ppm.store(100_000, Ordering::Relaxed);
        ps1.loss_ppm.store(100_000, Ordering::Relaxed);
        b.adapt_capacity(&mut st);
        assert!(st.est_bps < high, "aggregate loss backs off: {high} -> {}", st.est_bps);
        assert!(
            st.est_bps <= 2_000_000.0 * 0.9 + 1.0,
            "backs off toward delivered ×0.9: {}",
            st.est_bps
        );
    }

    /// A configured capacity is a hard upper bound the adaptive estimate
    /// never probes past.
    #[test]
    fn configured_capacity_caps_the_estimate() {
        let b = LegBroker::new();
        let ps = bonding_protocol::stats::PathStats::new();
        let mut st = LegState {
            members: vec![member(0, &ps), member(1, &ps)],
            cfg: Some(BondUplinkConfig {
                interface: "eno4".into(),
                capacity_bps: 2_500_000,
                min_viable_bps: None,
                demand_active_bps: None,
            }),
            est_bps: 0.0,
            oversubscribed: false,
            was_oversubscribed: false,
            unmet_bps: 0,
            guaranteed_unmet_bps: 0,
        };
        // Deliver near the cap, clean → tries to probe up, but is bounded.
        ps.throughput_bps.store(2_400_000, Ordering::Relaxed);
        for _ in 0..50 {
            b.adapt_capacity(&mut st);
        }
        assert!(
            st.est_bps <= 2_500_000.0 + 1.0,
            "estimate never exceeds configured capacity: {}",
            st.est_bps
        );
    }

    /// Stale-demand grace fix: an idle member keeps its min-viable floor
    /// briefly (bootstrap/active grace) then releases its share, so it can't
    /// hold a ceiling it never uses (which would block the aggregate probe-up
    /// = the collapse-spiral the review flagged).
    #[test]
    fn idle_member_releases_share_after_grace() {
        let b = LegBroker::new();
        let ps_hot = bonding_protocol::stats::PathStats::new();
        let ps_idle = bonding_protocol::stats::PathStats::new();
        let mut st = LegState {
            members: vec![member(0, &ps_hot), member(1, &ps_idle)],
            cfg: Some(BondUplinkConfig {
                interface: "eno4".into(),
                capacity_bps: 2_000_000,
                min_viable_bps: Some(200_000),
                demand_active_bps: None,
            }),
            est_bps: 2_000_000.0,
            oversubscribed: false,
            was_oversubscribed: false,
            unmet_bps: 0,
            guaranteed_unmet_bps: 0,
        };
        ps_hot.throughput_bps.store(1_000_000, Ordering::Relaxed);
        ps_idle.throughput_bps.store(0, Ordering::Relaxed);

        // While in the grace window, the idle member keeps ~min_viable.
        b.tick_leg("if:eno4", &mut st);
        let idle_grace = st.members[1].ceiling.load(Ordering::Relaxed);
        assert!(idle_grace >= 190_000, "idle member floored while in grace: {idle_grace}");

        // After the grace window drains, it releases its share (~0).
        for _ in 0..(GRACE_TICKS + 2) {
            b.tick_leg("if:eno4", &mut st);
        }
        let idle_released = st.members[1].ceiling.load(Ordering::Relaxed);
        assert!(idle_released < 20_000, "long-idle member releases its share: {idle_released}");
    }

    /// End-to-end tick path: a Critical flow reserves ahead of a best-effort
    /// flow on a full shared leg, and starving the guaranteed flow raises the
    /// alarm while best-effort shortfall stays quiet.
    #[test]
    fn tick_reserves_for_critical_and_alarms_on_guaranteed_breach() {
        let b = LegBroker::new();
        let ps_crit = bonding_protocol::stats::PathStats::new();
        let ps_be = bonding_protocol::stats::PathStats::new();
        let mut st = LegState {
            members: vec![
                member_pri(0, &ps_crit, BondPriority::Critical),
                member_pri(1, &ps_be, BondPriority::BestEffort),
            ],
            cfg: Some(BondUplinkConfig {
                interface: "eno4".into(),
                capacity_bps: 2_000_000,
                min_viable_bps: Some(200_000),
                demand_active_bps: None,
            }),
            est_bps: 2_000_000.0,
            oversubscribed: false,
            was_oversubscribed: false,
            unmet_bps: 0,
            guaranteed_unmet_bps: 0,
        };
        // Both flows want more than the whole leg.
        ps_crit.throughput_bps.store(3_000_000, Ordering::Relaxed);
        ps_be.throughput_bps.store(3_000_000, Ordering::Relaxed);
        for _ in 0..5 {
            b.tick_leg("if:eno4", &mut st);
        }
        let crit = st.members[0].ceiling.load(Ordering::Relaxed);
        let be = st.members[1].ceiling.load(Ordering::Relaxed);
        // Critical is served first; best-effort is squeezed out.
        assert!(crit > 1_500_000, "critical reserved most of the leg: {crit}");
        assert!(be < crit, "best-effort yields to critical: be={be} crit={crit}");
        // A guaranteed flow can't get its demand → alarm + guaranteed shortfall.
        assert!(st.oversubscribed, "guaranteed breach raises the alarm");
        assert!(st.guaranteed_unmet_bps > 0, "guaranteed shortfall recorded");
    }

    /// Generation token: a stale guard (old flow task torn down AFTER a
    /// restart re-registered the same flow/path) must not evict the fresh
    /// member. Exercises the real `deregister`.
    #[test]
    fn stale_guard_does_not_evict_reregistered_member() {
        let b = LegBroker::new();
        let mut mem = member(7, &bonding_protocol::stats::PathStats::new());
        mem.generation = 9; // the current live registration
        b.legs.insert(
            "if:eno4".to_string(),
            Mutex::new(LegState {
                members: vec![mem],
                cfg: None,
                est_bps: 0.0,
                oversubscribed: false,
                was_oversubscribed: false,
                unmet_bps: 0,
                guaranteed_unmet_bps: 0,
            }),
        );
        // A stale guard for the OLD generation (5) must remove nothing.
        b.deregister(&[("if:eno4".to_string(), 7, 0, 5)]);
        assert_eq!(
            b.legs.get("if:eno4").map(|l| l.lock().unwrap().members.len()),
            Some(1),
            "gen-9 member survives a stale gen-5 deregister"
        );
        // The correct-generation deregister removes it (and prunes the leg).
        b.deregister(&[("if:eno4".to_string(), 7, 0, 9)]);
        assert!(
            b.legs.get("if:eno4").is_none(),
            "gen-9 deregister removes it and prunes the empty leg"
        );
    }
}
