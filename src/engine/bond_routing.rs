// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gateway-mode path routing for bonded outputs.
//!
//! Bonded "gateway mode" lets each leg of a bonded output egress
//! through a specific router/next-hop while sharing one NIC on a dumb
//! switch. There is no per-socket "use this gateway" syscall on Linux,
//! so the edge programs a dedicated **policy route** per leg via
//! netlink (using `CAP_NET_ADMIN`, which the edge already holds):
//!
//! ```text
//! ip addr  add  <source>/<prefix> dev <iface>          # ensure addr
//! ip route add  default via <gateway> dev <iface> table <T>
//! ip rule  add  from <source-ip> lookup <T> priority <P>
//! ```
//!
//! The bonded path socket then binds to `<source-ip>`, so its packets
//! match the `from` rule and follow `<gateway>`. Several legs on one
//! NIC each get their own `(table, priority)` and therefore their own
//! router.
//!
//! ## Ownership model
//!
//! The edge owns a **reserved, namespaced slice** of routing policy —
//! it never touches `main` / `local` / `default`. Tables come from
//! `[TABLE_BASE, TABLE_BASE+MAX_SLOTS)`, rule priorities from
//! `[PRIO_BASE, PRIO_BASE+MAX_SLOTS)`. Bases are env-overridable
//! (host-level OS tuning), within bounds [`SlotAllocator::new`] enforces:
//! the tables must clear the kernel's 0 and 253–255, and the priorities
//! must hold none of the kernel's own rules ([`KERNEL_RULE_PRIORITIES`])
//! and end before its `lookup main` rule (32766). A whole number outside
//! them disables gateway routing for the process — every
//! [`BondRouteManager::program`] fails naming each refused variable, its
//! value and its bounds — rather than falling back to the default range,
//! which may be the very range the operator moved away from. A value that
//! is not a whole number is discarded for its default, as it always was,
//! and an empty one is unset. Both are reported at boot
//! ([`Bases::report`]). On its first use [`BondRouteManager::global`]
//! runs a **GC sweep** that deletes any rule/route left in those
//! ranges by a prior crash before any leg is programmed, and runs it
//! only on ranges that passed. Per-path teardown removes that path's rule +
//! flushes its table. Addresses the edge adds are left in place on
//! teardown (cheap, and avoids tearing down an address another leg or
//! the operator may rely on).
//!
//! Each `program` / `teardown` runs whole under the manager's one lock,
//! so two calls for one `(flow_id, path_id)` never interleave. That
//! orders calls already made; a `program` made after a `teardown`
//! installs the leg again, so a leg's owner stops whatever re-programs it
//! before it tears it down (`output_bonded`'s re-assert task).
//!
//! The netlink half needs a live kernel + `CAP_NET_ADMIN`, so only the
//! pure [`SlotAllocator`] and [`Bases`] are unit-tested here;
//! `tests/bond_routing_netns.rs` (`#[ignore]`d, run in a throwaway network
//! namespace, and by CI under `unshare --net`) drives the rest.
//! [`LegRoutes`] is the seam the callers' own tests put a fake behind.
//! See `docs/bonding-gateway-routing.md`.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::RangeInclusive;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use tokio::sync::{Mutex, OnceCell};

use crate::manager::events::{EventSender, EventSeverity, category};

/// Moves the reserved routing tables, if they collide with existing host
/// routing tables.
pub const TABLE_BASE_VAR: &str = "BILBYCAST_BOND_RT_TABLE_BASE";
/// Moves the reserved `ip rule` priorities.
pub const PRIO_BASE_VAR: &str = "BILBYCAST_BOND_RT_PRIO_BASE";
/// Default base for our reserved routing tables (`0xBC00`).
const DEFAULT_TABLE_BASE: u32 = 48128;
/// Default base for our reserved `ip rule` priorities.
const DEFAULT_PRIO_BASE: u32 = 10000;
/// Maximum concurrent gateway-mode legs per host.
pub const MAX_SLOTS: u32 = 256;
/// Table bases whose `MAX_SLOTS` tables clear the kernel's own — 0
/// (unspec), 253 (default), 254 (main), 255 (local) — and end at or below
/// the last table id. No base under 256 can: its range would reach
/// 253–255, and the startup sweep would empty those tables.
const TABLE_BASES: RangeInclusive<u32> = 256..=u32::MAX - (MAX_SLOTS - 1);
/// The rule priorities the kernel installs by itself, which the sweep and
/// a leg's teardown (both delete every rule at a priority they own,
/// whatever it does) must never own:
///
/// - 0 `lookup local` and 32766 `lookup main`, in every namespace, for
///   IPv4 and IPv6 (`fib_default_rule_add` in `net/ipv4/fib_rules.c` and
///   `net/ipv6/fib6_rules.c`);
/// - 32767 `lookup default`, for IPv4 (`net/ipv4/fib_rules.c`), and the one
///   rule of the IPMR / IP6MR families (`net/ipv4/ipmr.c`,
///   `net/ipv6/ip6mr.c`);
/// - 1000, the `l3mdev` rule `drivers/net/vrf.c` adds for each family
///   (`FIB_RULE_PREF`) when a namespace's first VRF device is created.
///
/// Nothing else in the kernel adds a rule at a fixed priority: those four
/// files are the only callers of `fib_default_rule_add`, and `vrf.c` the
/// only driver that sets `FRA_PRIORITY`. A rule added with no priority
/// takes one just below the second rule in the list
/// (`fib_default_rule_pref`, `net/core/fib_rules.c`); that is a rule some
/// program asked for, not the kernel's.
pub const KERNEL_RULE_PRIORITIES: [u32; 4] = [0, 1000, 32766, 32767];
/// The kernel's `lookup main` rule. A leg's rule after it is never
/// consulted for a source the main table routes, which with a default
/// route there is every source.
const LOOKUP_MAIN_PRIORITY: u32 = 32766;

/// Whether `base`'s `MAX_SLOTS` rule priorities hold none of
/// [`KERNEL_RULE_PRIORITIES`] and end before `lookup main`.
fn prio_base_ok(base: u32) -> bool {
    base.checked_add(MAX_SLOTS - 1).is_some_and(|last| {
        last < LOOKUP_MAIN_PRIORITY && !KERNEL_RULE_PRIORITIES.iter().any(|k| (base..=last).contains(k))
    })
}

/// The bases [`prio_base_ok`] accepts, as an error states them: the runs
/// either side of the VRF rule at 1000 (a unit test holds the two equal).
const PRIO_BASES: [RangeInclusive<u32>; 2] = [1..=1000 - MAX_SLOTS, 1001..=LOOKUP_MAIN_PRIORITY - MAX_SLOTS];

/// [`TABLE_BASES`] in words: "from 256 to 4294967040".
fn table_bounds() -> String {
    format!("from {} to {}", TABLE_BASES.start(), TABLE_BASES.end())
}

/// [`PRIO_BASES`] in words: "from 1 to 744 or from 1001 to 32510".
fn prio_bounds() -> String {
    let [low, high] = &PRIO_BASES;
    format!(
        "from {} to {} or from {} to {}",
        low.start(),
        low.end(),
        high.start(),
        high.end()
    )
}

/// Why `base` cannot be the table base, or `None` when it can. `None` for
/// `base` is a whole number past `u32`; `shown` is the value as written.
fn table_refusal(base: Option<u32>, shown: &str) -> Option<String> {
    match base {
        Some(b) if TABLE_BASES.contains(&b) => None,
        Some(_) => Some(format!(
            "{TABLE_BASE_VAR}={shown} is refused: its {MAX_SLOTS} tables must clear the kernel's \
             tables 0 and 253-255 and end at or below {}. Use a base {}",
            u32::MAX,
            table_bounds()
        )),
        None => Some(format!(
            "{TABLE_BASE_VAR}={shown} is refused: it is out of range. Use a base {}",
            table_bounds()
        )),
    }
}

/// Why `base` cannot be the priority base, or `None` when it can, as
/// [`table_refusal`].
fn prio_refusal(base: Option<u32>, shown: &str) -> Option<String> {
    match base {
        Some(b) if prio_base_ok(b) => None,
        Some(_) => Some(format!(
            "{PRIO_BASE_VAR}={shown} is refused: its {MAX_SLOTS} rule priorities must hold none \
             of the kernel's own rules (0 `lookup local`, 1000 the VRF `l3mdev` rule, 32766 \
             `lookup main`, 32767 `lookup default`) and end before `lookup main`. Use a base {}",
            prio_bounds()
        )),
        None => Some(format!(
            "{PRIO_BASE_VAR}={shown} is refused: it is out of range. Use a base {}",
            prio_bounds()
        )),
    }
}

/// A parsed source address with prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceNet {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl SourceNet {
    /// Parse `"192.168.10.2/24"` (or a bare IP → /32 or /128).
    pub fn parse(s: &str) -> Result<Self> {
        let (ip_part, prefix) = match s.split_once('/') {
            Some((ip, pfx)) => (
                ip,
                pfx.parse::<u8>()
                    .with_context(|| format!("invalid prefix in source '{s}'"))?,
            ),
            None => (s, 0u8), // resolved below to max-length
        };
        let addr: IpAddr = ip_part
            .trim()
            .parse()
            .with_context(|| format!("invalid source IP '{s}'"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = if s.contains('/') { prefix } else { max };
        if prefix > max {
            return Err(anyhow!("source '{s}' prefix /{prefix} exceeds /{max}"));
        }
        Ok(Self { addr, prefix })
    }
}

// ── Pure slot allocation (unit-tested) ───────────────────────────────────────

/// Hands out a unique `(table, priority)` per gateway-mode leg from
/// the reserved ranges. Pure + synchronous so the allocation policy
/// is testable without a kernel.
///
/// Only [`SlotAllocator::new`] builds one, and it refuses a base whose
/// range would run past `u32::MAX`, so `base + index` cannot overflow.
#[derive(Debug)]
pub struct SlotAllocator {
    table_base: u32,
    prio_base: u32,
    used: Vec<bool>,
}

/// A reserved routing slot: a table id + the rule priority that points
/// at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub index: u32,
    pub table: u32,
    pub priority: u32,
}

impl SlotAllocator {
    /// An allocator over `MAX_SLOTS` tables from `table_base` and as many
    /// rule priorities from `prio_base`, or why the bases are refused
    /// (see [`TABLE_BASES`] and [`PRIO_BASES`]) — both, when both are.
    pub fn new(table_base: u32, prio_base: u32) -> Result<Self> {
        let refusals: Vec<String> = [
            table_refusal(Some(table_base), &table_base.to_string()),
            prio_refusal(Some(prio_base), &prio_base.to_string()),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !refusals.is_empty() {
            return Err(anyhow!("{}", refusals.join("; ")));
        }
        Ok(Self {
            table_base,
            prio_base,
            used: vec![false; MAX_SLOTS as usize],
        })
    }


    /// Allocate the lowest free slot, or `None` if the reserved range
    /// is exhausted.
    pub fn alloc(&mut self) -> Option<Slot> {
        let idx = self.used.iter().position(|u| !u)?;
        self.used[idx] = true;
        let index = idx as u32; // < MAX_SLOTS
        Some(Slot {
            index,
            table: self.table_base + index,
            priority: self.prio_base + index,
        })
    }

    pub fn free(&mut self, slot: Slot) {
        if (slot.index as usize) < self.used.len() {
            self.used[slot.index as usize] = false;
        }
    }

    /// Inclusive `(low, high)` of the reserved table range.
    pub fn table_range(&self) -> (u32, u32) {
        (self.table_base, self.table_base + (MAX_SLOTS - 1))
    }
    /// Inclusive `(low, high)` of the reserved priority range.
    pub fn prio_range(&self) -> (u32, u32) {
        (self.prio_base, self.prio_base + (MAX_SLOTS - 1))
    }
}

// ── The two base variables ───────────────────────────────────────────────────

/// How one base variable reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseRead<'a> {
    /// Unset, empty or only whitespace — what a systemd `Environment=VAR=`
    /// line or an empty `EnvironmentFile` entry gives: the default.
    Unset,
    /// A whole number that fits a `u32`.
    Value(u32),
    /// A whole number past `u32` either way (`4294967296`, `-1`), as
    /// written: out of range, so refused like any other unsafe base.
    OutOfRange(&'a str),
    /// Not a whole number at all (`0xBD00`, `7000x`), as written.
    Unparseable(&'a str),
}

fn read_base(raw: Option<&str>) -> BaseRead<'_> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return BaseRead::Unset;
    };
    if let Ok(v) = raw.parse::<u32>() {
        return BaseRead::Value(v);
    }
    let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        BaseRead::OutOfRange(raw)
    } else {
        BaseRead::Unparseable(raw)
    }
}

/// A base variable set to something that is not a whole number. Its value
/// is discarded for the default, which is what every release did; this
/// release says so ([`Bases::report`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnparseableBase {
    pub var: &'static str,
    /// The value as the host sets it, trimmed.
    pub value: String,
    /// The default used in its place.
    pub default: u32,
    /// The bases that variable accepts, in words ("from 256 to
    /// 4294967040").
    pub accepted: String,
}

/// A base variable refused: a whole number whose range would reach the
/// kernel's own tables or rules, or past `u32`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedBase {
    pub var: &'static str,
    /// The value as the host sets it, trimmed.
    pub value: String,
    /// The sentence naming the variable, its value, why, and its bounds.
    pub reason: String,
    /// The bases that variable accepts, in words, as `reason` states them.
    pub accepted: String,
}

/// What this host's two base variables come to.
#[derive(Debug)]
pub struct Bases {
    /// The allocator over them, or — when any is refused — every refused
    /// variable's [`RefusedBase::reason`] in one message.
    pub slots: std::result::Result<SlotAllocator, String>,
    /// The refused variables (empty when `slots` is `Ok`).
    pub refused: Vec<RefusedBase>,
    /// The variables discarded for their defaults.
    pub unparseable: Vec<UnparseableBase>,
}

/// One line of the boot report: an event and its log line.
#[derive(Debug, Clone)]
pub struct BaseFinding {
    pub severity: EventSeverity,
    pub message: String,
    pub details: serde_json::Value,
}

impl Bases {
    /// What `table_base` and `prio_base`, as the host sets them (`None`
    /// unset), come to.
    pub fn from_settings(table_base: Option<&str>, prio_base: Option<&str>) -> Self {
        let mut refused = Vec::new();
        let mut unparseable = Vec::new();
        let mut resolve = |var: &'static str,
                           raw: Option<&str>,
                           default: u32,
                           refusal: fn(Option<u32>, &str) -> Option<String>,
                           accepted: String| {
            let (base, shown) = match read_base(raw) {
                BaseRead::Unset => return default,
                BaseRead::Unparseable(raw) => {
                    unparseable.push(UnparseableBase {
                        var,
                        value: raw.to_string(),
                        default,
                        accepted,
                    });
                    return default;
                }
                BaseRead::Value(v) => (Some(v), v.to_string()),
                BaseRead::OutOfRange(raw) => (None, raw.to_string()),
            };
            if let Some(reason) = refusal(base, &shown) {
                refused.push(RefusedBase {
                    var,
                    value: shown,
                    reason,
                    accepted,
                });
            }
            base.unwrap_or(default)
        };
        let table = resolve(
            TABLE_BASE_VAR,
            table_base,
            DEFAULT_TABLE_BASE,
            table_refusal,
            table_bounds(),
        );
        let prio = resolve(
            PRIO_BASE_VAR,
            prio_base,
            DEFAULT_PRIO_BASE,
            prio_refusal,
            prio_bounds(),
        );
        let slots = if refused.is_empty() {
            SlotAllocator::new(table, prio).map_err(|e| format!("{e:#}"))
        } else {
            Err(refused
                .iter()
                .map(|r| r.reason.as_str())
                .collect::<Vec<_>>()
                .join("; "))
        };
        Self {
            slots,
            refused,
            unparseable,
        }
    }

    /// [`Bases::from_settings`] over this process's environment. A value
    /// that is not UTF-8 reads as not a whole number.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        Self::from_settings(var(TABLE_BASE_VAR).as_deref(), var(PRIO_BASE_VAR).as_deref())
    }

    /// The boot report: a Warning per variable discarded for its default,
    /// and one Critical naming every refused variable. Empty when the host
    /// sets neither, or sets both to bases that pass.
    ///
    /// Not `deprecated_env_var`, though `config::env_compat` reports an
    /// unparseable value under that code: these variables are host-level
    /// and stay, and that event's message tells the operator to move the
    /// knob into the node's configuration and remove the variable. Removing
    /// a moved base puts the reserved range back on the default, which the
    /// next start then sweeps — the tables the operator moved it to protect.
    pub fn findings(&self) -> Vec<BaseFinding> {
        let mut out: Vec<BaseFinding> = self
            .unparseable
            .iter()
            .map(|u| BaseFinding {
                severity: EventSeverity::Warning,
                message: format!(
                    "{} is set to {:?}, which is not a whole number. It has been DISCARDED and \
                     the default base {} is in use, as if it were unset. To move the range, set \
                     a whole number {}; to keep the default, remove the variable.",
                    u.var, u.value, u.default, u.accepted
                ),
                details: serde_json::json!({
                    "error_code": "bond_rt_base_unparseable",
                    "env_var": u.var,
                    "value": u.value,
                    "status": "unparseable",
                    "default": u.default,
                    "accepted": u.accepted,
                }),
            })
            .collect();
        if let Err(why) = &self.slots
            && !self.refused.is_empty()
        {
            out.push(BaseFinding {
                severity: EventSeverity::Critical,
                message: format!(
                    "bond gateway routing is disabled for this process: {why}. No gateway-mode \
                     leg (bonded output, relay or direct tunnel, leg test) can be programmed, \
                     and no orphan sweep runs, until the value is fixed and the edge restarted."
                ),
                details: serde_json::json!({
                    "error_code": "bond_rt_base_refused",
                    "status": "refused",
                    "refused": self.refused.iter().map(|r| serde_json::json!({
                        "env_var": r.var,
                        "value": r.value,
                        "accepted": r.accepted,
                    })).collect::<Vec<_>>(),
                }),
            });
        }
        out
    }

    /// Log [`Bases::findings`] and raise each as a `config` event. Called
    /// once at boot, so the operator learns of a refused base before any
    /// gateway leg runs into it; the events queue until the manager
    /// connects.
    pub fn report(&self, events: &EventSender) {
        for f in self.findings() {
            if f.severity == EventSeverity::Critical {
                tracing::error!("{}", f.message);
            } else {
                tracing::warn!("{}", f.message);
            }
            events.emit_with_details(f.severity, category::CONFIG, f.message, None, f.details);
        }
    }
}

// ── Programmed-path bookkeeping ──────────────────────────────────────────────

/// One programmed leg: its slot, and the source its rule matches.
#[derive(Debug, Clone, Copy)]
struct PathRoute {
    slot: Slot,
    source: IpAddr,
}

/// The slot allocator and the active per-`(flow_id, path_id)` legs.
struct Routes {
    slots: SlotAllocator,
    active: HashMap<(String, u8), PathRoute>,
}

/// Process-global manager owning the netlink handle and the leg routes.
pub struct BondRouteManager {
    handle: rtnetlink::Handle,
    /// Held across each whole `program` / `teardown` — the
    /// teardown-allocate-install-record sequence must not interleave with
    /// another call for the same key, or two slots get programmed for it
    /// and one is never removed, and a teardown must not return while a
    /// `program` of its key is still installing. One lock for every key,
    /// not one per key: the kernel already takes its RTNL lock for every
    /// route, rule and address change, so per-key locks would buy no
    /// parallelism, and the allocator is shared by every key anyway. The
    /// cost is that one leg's netlink round trips (a handful, each a local
    /// kernel call) delay another leg's. No method takes it while holding
    /// it, so it cannot deadlock. `Err` holds why the operator's range
    /// bases were refused.
    routes: std::result::Result<Mutex<Routes>, String>,
}

static GLOBAL: OnceCell<BondRouteManager> = OnceCell::const_new();

impl BondRouteManager {
    /// Lazily build the process-global manager, opening the netlink
    /// connection and running the startup GC sweep on first use. Refused
    /// range bases do not fail this — [`Self::program_dish_route`] needs no
    /// reserved range — but they skip the sweep and fail every
    /// [`Self::program`].
    pub async fn global() -> Result<&'static BondRouteManager> {
        GLOBAL
            .get_or_try_init(|| async {
                let (conn, handle, _) = rtnetlink::new_connection()
                    .context("open netlink connection for bond gateway routing")?;
                tokio::spawn(conn);
                // Reported at boot (`Bases::report`); read again here.
                let routes = match Bases::from_env().slots {
                    Ok(slots) => {
                        // Sweep orphans from a prior crash before any flow runs.
                        if let Err(e) =
                            gc_reserved(&handle, slots.table_range(), slots.prio_range()).await
                        {
                            tracing::warn!("bond gateway routing: startup GC sweep failed: {e}");
                        }
                        Ok(Mutex::new(Routes {
                            slots,
                            active: HashMap::new(),
                        }))
                    }
                    Err(why) => {
                        tracing::error!(
                            "bond gateway routing disabled, and no orphan sweep run: {why}"
                        );
                        Err(why)
                    }
                };
                let mgr = BondRouteManager { handle, routes };
                Ok::<_, anyhow::Error>(mgr)
            })
            .await
    }

    /// The global manager **if it was already initialised** (i.e. a
    /// gateway-mode leg ran this process). Unlike [`Self::global`] this
    /// does NOT initialise — safe to call on shutdown so a process that
    /// never used gateway routing doesn't spin up netlink for nothing.
    pub fn try_global() -> Option<&'static BondRouteManager> {
        GLOBAL.get()
    }

    fn routes(&self) -> Result<&Mutex<Routes>> {
        self.routes
            .as_ref()
            .map_err(|why| anyhow!("bond gateway routing is disabled: {why}"))
    }

    /// Tear down EVERY active gateway-mode route. Called from the edge's
    /// graceful shutdown so a clean stop removes the policy routes it
    /// installed; the startup GC remains the crash safety-net.
    pub async fn teardown_all(&self) {
        let Ok(routes) = self.routes() else {
            return;
        };
        let mut routes = routes.lock().await;
        let keys: Vec<(String, u8)> = routes.active.keys().cloned().collect();
        if keys.is_empty() {
            return;
        }
        tracing::info!(
            "bond gateway routing: tearing down {} active route(s) on shutdown",
            keys.len()
        );
        for key in keys {
            self.release(&mut routes, &key).await;
        }
    }

    /// Program a gateway-mode leg: ensure the source address is on the
    /// NIC, install `default via <gateway>` in a fresh table, and a
    /// `from <source>` rule pointing at it. Idempotent per
    /// `(flow_id, path_id)`. On failure the leg's rule, route and slot
    /// are all gone again, and a source address step 1 added stays on the
    /// NIC, as it does on teardown.
    pub async fn program(
        &self,
        flow_id: &str,
        path_id: u8,
        interface: &str,
        source: SourceNet,
        gateway: IpAddr,
    ) -> Result<()> {
        let mut routes = self.routes()?.lock().await;
        let key = (flow_id.to_string(), path_id);
        // Re-program cleanly if this key was already active.
        self.release(&mut routes, &key).await;

        let if_index = if_index(interface)?;
        let slot = routes.slots.alloc().ok_or_else(|| {
            anyhow!("bond gateway routing: reserved slot range exhausted (>{MAX_SLOTS} legs)")
        })?;
        if let Err(e) = self
            .install(slot, interface, if_index, source, gateway)
            .await
        {
            // Whatever part of the leg went in comes out with its slot.
            let _ = self.del_rules_with_priority(slot.priority).await;
            let _ = self.del_routes_in_table(slot.table).await;
            routes.slots.free(slot);
            return Err(e);
        }
        routes.active.insert(
            key,
            PathRoute {
                slot,
                source: source.addr,
            },
        );
        tracing::info!(
            "bond gateway routing: flow={flow_id} path={path_id} → via {gateway} (table {}, prio {}, src {})",
            slot.table,
            slot.priority,
            source.addr
        );
        Ok(())
    }

    /// The three netlink steps of [`Self::program`] into `slot`.
    async fn install(
        &self,
        slot: Slot,
        interface: &str,
        if_index: u32,
        source: SourceNet,
        gateway: IpAddr,
    ) -> Result<()> {
        // 1. ensure the source address is present on the NIC (idempotent).
        if let Err(e) = self
            .handle
            .address()
            .add(if_index, source.addr, source.prefix)
            .execute()
            .await
            && !is_exists(&e)
        {
            bail!(
                "bond gateway routing: add address {}/{} dev {interface}: {e}",
                source.addr,
                source.prefix
            );
        }
        // 2. default route via the gateway in our private table.
        self.add_default_route(gateway, slot.table, if_index)
            .await
            .map_err(|e| {
                anyhow!(
                    "bond gateway routing: add default via {gateway} table {}: {e}",
                    slot.table
                )
            })?;
        // 3. policy rule: from <source-ip> lookup <table>.
        self.add_rule(source.addr, slot.table, slot.priority)
            .await
            .map_err(|e| {
                anyhow!(
                    "bond gateway routing: add rule from {} table {}: {e}",
                    source.addr,
                    slot.table
                )
            })
    }

    /// Whether a leg's programmed routing is still in effect: the
    /// `(flow_id, path_id)` key is active, its `from <source> lookup
    /// <table>` rule is still at its priority, AND its private table
    /// still holds at least one route. The kernel silently empties the
    /// table on link-down / device-destroy (USB modem re-plug) without
    /// any event the socket watcher can see, and an operator's
    /// `ip rule flush` or a network manager removing rules it did not
    /// create takes the rule — either way the leg falls through to the
    /// main default route and collapses onto another leg's router while
    /// sends keep succeeding. `false` also covers a `program()` that
    /// never succeeded (key not active).
    pub async fn is_intact(&self, flow_id: &str, path_id: u8) -> bool {
        let Ok(routes) = self.routes() else {
            return false;
        };
        let key = (flow_id.to_string(), path_id);
        let Some(route) = routes.lock().await.active.get(&key).copied() else {
            return false;
        };
        self.has_rule(route).await && self.table_has_route(route.slot.table).await
    }

    /// Whether the kernel holds `route`'s rule: its priority, its source
    /// as a host prefix, and its table.
    async fn has_rule(&self, route: PathRoute) -> bool {
        let (ipv, host_len) = match route.source {
            IpAddr::V4(_) => (rtnetlink::IpVersion::V4, 32),
            IpAddr::V6(_) => (rtnetlink::IpVersion::V6, 128),
        };
        let mut rules = self.handle.rule().get(ipv).execute();
        while let Ok(Some(rule)) = rules.try_next().await {
            if rule_priority(&rule) == Some(route.slot.priority)
                && rule_table(&rule) == route.slot.table
                && rule.header.src_len == host_len
                && rule_source(&rule) == Some(route.source)
            {
                return true;
            }
        }
        false
    }

    /// Whether `table` holds at least one route, of either family.
    async fn table_has_route(&self, table: u32) -> bool {
        use rtnetlink::RouteMessageBuilder;
        let filters = [
            RouteMessageBuilder::<Ipv4Addr>::new().build(),
            RouteMessageBuilder::<Ipv6Addr>::new().build(),
        ];
        for filter in filters {
            let mut routes = self.handle.route().get(filter).execute();
            while let Ok(Some(route)) = routes.try_next().await {
                if route_table(&route) == table {
                    return true;
                }
            }
        }
        false
    }

    /// Remove a leg's rule + flush its table. Idempotent; never errors
    /// (best-effort teardown). The source address is intentionally
    /// left in place.
    pub async fn teardown(&self, flow_id: &str, path_id: u8) {
        let Ok(routes) = self.routes() else {
            return;
        };
        let mut routes = routes.lock().await;
        self.release(&mut routes, &(flow_id.to_string(), path_id)).await;
    }

    /// [`Self::teardown`] under a lock the caller already holds.
    async fn release(&self, routes: &mut Routes, key: &(String, u8)) {
        if let Some(route) = routes.active.remove(key) {
            let _ = self.del_rules_with_priority(route.slot.priority).await;
            let _ = self.del_routes_in_table(route.slot.table).await;
            routes.slots.free(route.slot);
        }
    }

    // ── netlink helpers ──────────────────────────────────────────────────────

    async fn add_default_route(
        &self,
        gateway: IpAddr,
        table: u32,
        if_index: u32,
    ) -> Result<(), rtnetlink::Error> {
        use rtnetlink::RouteMessageBuilder;
        let msg = match gateway {
            IpAddr::V4(gw) => RouteMessageBuilder::<Ipv4Addr>::new()
                .destination_prefix(Ipv4Addr::UNSPECIFIED, 0)
                .gateway(gw)
                .output_interface(if_index)
                .table_id(table)
                .build(),
            IpAddr::V6(gw) => RouteMessageBuilder::<Ipv6Addr>::new()
                .destination_prefix(Ipv6Addr::UNSPECIFIED, 0)
                .gateway(gw)
                .output_interface(if_index)
                .table_id(table)
                .build(),
        };
        self.handle.route().add(msg).execute().await
    }

    /// Program (add-or-replace, idempotent) a scoped IPv4 route to a
    /// dish/management subnet via `gateway` out `if_index`, in the MAIN table.
    /// Keeps the Starlink dish telemetry reachable (e.g. `192.168.100.0/24`
    /// via the Wi-Fi gateway on `wlo5`) without the operator re-adding the
    /// route by hand every time the Wi-Fi link cycles. Scoped to a `/24`-ish
    /// subnet in `main` (never a default route + never a policy rule) so it
    /// can't shadow the operator's own default route.
    pub async fn program_dish_route(
        &self,
        dest: Ipv4Addr,
        prefix: u8,
        gateway: Ipv4Addr,
        interface: &str,
    ) -> Result<()> {
        let if_index = if_index(interface)?;
        use rtnetlink::RouteMessageBuilder;
        let msg = RouteMessageBuilder::<Ipv4Addr>::new()
            .destination_prefix(dest, prefix)
            .gateway(gateway)
            .output_interface(if_index)
            .build();
        self.handle
            .route()
            .add(msg)
            .replace()
            .execute()
            .await
            .with_context(|| format!("program dish route {dest}/{prefix} via {gateway}"))?;
        Ok(())
    }

    async fn add_rule(
        &self,
        source: IpAddr,
        table: u32,
        priority: u32,
    ) -> Result<(), rtnetlink::Error> {
        use netlink_packet_route::rule::RuleAction;
        match source {
            IpAddr::V4(v4) => {
                self.handle
                    .rule()
                    .add()
                    .v4()
                    .source_prefix(v4, 32)
                    .table_id(table)
                    .priority(priority)
                    .action(RuleAction::ToTable)
                    .execute()
                    .await
            }
            IpAddr::V6(v6) => {
                self.handle
                    .rule()
                    .add()
                    .v6()
                    .source_prefix(v6, 128)
                    .table_id(table)
                    .priority(priority)
                    .action(RuleAction::ToTable)
                    .execute()
                    .await
            }
        }
    }

    /// Delete every `ip rule` whose priority matches `priority`.
    async fn del_rules_with_priority(&self, priority: u32) -> Result<()> {
        for ipv in [rtnetlink::IpVersion::V4, rtnetlink::IpVersion::V6] {
            let mut rules = self.handle.rule().get(ipv).execute();
            while let Some(rule) = rules.try_next().await? {
                if rule_priority(&rule) == Some(priority) {
                    let _ = self.handle.rule().del(rule).execute().await;
                }
            }
        }
        Ok(())
    }

    /// Delete every route in `table` (flush our private table).
    async fn del_routes_in_table(&self, table: u32) -> Result<()> {
        use rtnetlink::RouteMessageBuilder;
        let filters = [
            RouteMessageBuilder::<Ipv4Addr>::new().build(),
            RouteMessageBuilder::<Ipv6Addr>::new().build(),
        ];
        for filter in filters {
            let mut routes = self.handle.route().get(filter).execute();
            while let Some(route) = routes.try_next().await? {
                if route_table(&route) == table {
                    let _ = self.handle.route().del(route).execute().await;
                }
            }
        }
        Ok(())
    }
}

/// What the owner of a gateway leg asks of the route manager. The bonded
/// output's re-assert task and the native-UDP tunnels take it rather than
/// [`BondRouteManager`] itself, so their tests can stand a fake in for the
/// kernel ([`fake::FakeRoutes`]).
pub(crate) trait LegRoutes: Sync {
    /// [`BondRouteManager::program`].
    fn program(
        &self,
        flow_id: &str,
        path_id: u8,
        interface: &str,
        source: SourceNet,
        gateway: IpAddr,
    ) -> impl Future<Output = Result<()>> + Send;
    /// [`BondRouteManager::is_intact`].
    fn is_intact(&self, flow_id: &str, path_id: u8) -> impl Future<Output = bool> + Send;
    /// [`BondRouteManager::teardown`].
    fn teardown(&self, flow_id: &str, path_id: u8) -> impl Future<Output = ()> + Send;
}

impl LegRoutes for BondRouteManager {
    fn program(
        &self,
        flow_id: &str,
        path_id: u8,
        interface: &str,
        source: SourceNet,
        gateway: IpAddr,
    ) -> impl Future<Output = Result<()>> + Send {
        BondRouteManager::program(self, flow_id, path_id, interface, source, gateway)
    }
    fn is_intact(&self, flow_id: &str, path_id: u8) -> impl Future<Output = bool> + Send {
        BondRouteManager::is_intact(self, flow_id, path_id)
    }
    fn teardown(&self, flow_id: &str, path_id: u8) -> impl Future<Output = ()> + Send {
        BondRouteManager::teardown(self, flow_id, path_id)
    }
}

/// A [`LegRoutes`] that records every call and touches no kernel.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    pub(crate) struct FakeRoutes {
        /// Every call, in order: `"program 3"`, `"is_intact 0"`,
        /// `"teardown 1"`. A `program` is recorded when it returns — when
        /// its leg is in place — the others when they are called.
        log: std::sync::Mutex<Vec<String>>,
        /// The call (`"is_intact"` / `"program"`) and path that waits for
        /// [`FakeRoutes::release`] before it answers.
        hold: Option<(&'static str, u8)>,
        gate: tokio::sync::Semaphore,
        /// What `program` fails with, if it does.
        program_err: Option<String>,
    }

    impl FakeRoutes {
        /// Every leg broken: `is_intact` answers `false`, `program` succeeds.
        pub(crate) fn new() -> Self {
            Self {
                log: std::sync::Mutex::default(),
                hold: None,
                gate: tokio::sync::Semaphore::new(0),
                program_err: None,
            }
        }
        /// As [`FakeRoutes::new`], with `call` (`"is_intact"` or
        /// `"program"`) of `path` waiting for [`FakeRoutes::release`]
        /// before it answers.
        pub(crate) fn holding(call: &'static str, path: u8) -> Self {
            Self { hold: Some((call, path)), ..Self::new() }
        }
        /// `program` fails with `why`.
        pub(crate) fn failing(why: &str) -> Self {
            Self { program_err: Some(why.to_string()), ..Self::new() }
        }
        /// Let the held `is_intact` answer.
        pub(crate) fn release(&self) {
            self.gate.add_permits(1);
        }
        pub(crate) fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn record(&self, call: &str, path_id: u8) {
            self.log.lock().unwrap().push(format!("{call} {path_id}"));
        }
        /// Wait if `call` of `path_id` is the held one.
        async fn wait_if_held(&self, call: &'static str, path_id: u8) {
            if self.hold == Some((call, path_id)) {
                let _ = self.gate.acquire().await;
            }
        }
    }

    impl LegRoutes for FakeRoutes {
        async fn program(
            &self,
            _flow_id: &str,
            path_id: u8,
            _interface: &str,
            _source: SourceNet,
            _gateway: IpAddr,
        ) -> Result<()> {
            self.wait_if_held("program", path_id).await;
            self.record("program", path_id);
            match &self.program_err {
                Some(why) => Err(anyhow!("{why}")),
                None => Ok(()),
            }
        }
        async fn is_intact(&self, _flow_id: &str, path_id: u8) -> bool {
            self.record("is_intact", path_id);
            self.wait_if_held("is_intact", path_id).await;
            false
        }
        async fn teardown(&self, _flow_id: &str, path_id: u8) {
            self.record("teardown", path_id);
        }
    }
}

/// Startup sweep: delete any rule/route left in the reserved ranges by a
/// prior crashed instance. Only ever run on ranges [`SlotAllocator::new`]
/// accepted.
async fn gc_reserved(
    handle: &rtnetlink::Handle,
    (table_lo, table_hi): (u32, u32),
    (prio_lo, prio_hi): (u32, u32),
) -> Result<()> {
    use rtnetlink::RouteMessageBuilder;
    let mut removed_rules = 0u32;
    let mut removed_routes = 0u32;
    for ipv in [rtnetlink::IpVersion::V4, rtnetlink::IpVersion::V6] {
        let mut rules = handle.rule().get(ipv).execute();
        while let Some(rule) = rules.try_next().await? {
            if matches!(rule_priority(&rule), Some(p) if p >= prio_lo && p <= prio_hi)
                && handle.rule().del(rule).execute().await.is_ok()
            {
                removed_rules += 1;
            }
        }
    }
    let filters = [
        RouteMessageBuilder::<Ipv4Addr>::new().build(),
        RouteMessageBuilder::<Ipv6Addr>::new().build(),
    ];
    for filter in filters {
        let mut routes = handle.route().get(filter).execute();
        while let Some(route) = routes.try_next().await? {
            let t = route_table(&route);
            if t >= table_lo
                && t <= table_hi
                && handle.route().del(route).execute().await.is_ok()
            {
                removed_routes += 1;
            }
        }
    }
    if removed_rules + removed_routes > 0 {
        tracing::info!(
            "bond gateway routing: GC removed {removed_rules} orphan rule(s), {removed_routes} orphan route(s)"
        );
    }
    Ok(())
}

/// Extract the `Priority` attribute from a rule message.
fn rule_priority(rule: &netlink_packet_route::rule::RuleMessage) -> Option<u32> {
    rule.attributes.iter().find_map(|a| match a {
        netlink_packet_route::rule::RuleAttribute::Priority(p) => Some(*p),
        _ => None,
    })
}

/// The table a rule looks up: the wide attribute if present, else the
/// legacy 8-bit header field.
fn rule_table(rule: &netlink_packet_route::rule::RuleMessage) -> u32 {
    rule.attributes
        .iter()
        .find_map(|a| match a {
            netlink_packet_route::rule::RuleAttribute::Table(t) => Some(*t),
            _ => None,
        })
        .unwrap_or(rule.header.table as u32)
}

/// The `from` address a rule matches, if it names one.
fn rule_source(rule: &netlink_packet_route::rule::RuleMessage) -> Option<IpAddr> {
    rule.attributes.iter().find_map(|a| match a {
        netlink_packet_route::rule::RuleAttribute::Source(ip) => Some(*ip),
        _ => None,
    })
}

/// Resolve an interface name to its kernel index.
fn if_index(iface: &str) -> Result<u32> {
    let cname = std::ffi::CString::new(iface)
        .map_err(|_| anyhow!("interface name '{iface}' contains NUL"))?;
    // SAFETY: cname is a valid NUL-terminated C string.
    let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if idx == 0 {
        return Err(anyhow!("interface '{iface}' not found"));
    }
    Ok(idx)
}

/// True if a netlink error is an "already exists" (EEXIST) response.
fn is_exists(e: &rtnetlink::Error) -> bool {
    matches!(
        e,
        rtnetlink::Error::NetlinkError(msg)
            if msg.code.map(|c| c.get() == -(libc::EEXIST)).unwrap_or(false)
    )
}

/// The effective routing-table id of a route message: the wide
/// attribute if present, else the legacy 8-bit header field.
fn route_table(route: &netlink_packet_route::route::RouteMessage) -> u32 {
    route
        .attributes
        .iter()
        .find_map(|a| match a {
            netlink_packet_route::route::RouteAttribute::Table(t) => Some(*t),
            _ => None,
        })
        .unwrap_or(route.header.table as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(table_base: u32, prio_base: u32) -> SlotAllocator {
        SlotAllocator::new(table_base, prio_base).expect("bases in range")
    }

    #[test]
    fn slot_alloc_hands_out_distinct_table_and_prio() {
        let mut a = slots(48128, 10000);
        let s0 = a.alloc().unwrap();
        let s1 = a.alloc().unwrap();
        assert_eq!((s0.table, s0.priority), (48128, 10000));
        assert_eq!((s1.table, s1.priority), (48129, 10001));
        assert_ne!(s0.index, s1.index);
    }

    #[test]
    fn freed_slot_is_reused_lowest_first() {
        let mut a = slots(1000, 200);
        let s0 = a.alloc().unwrap();
        let _s1 = a.alloc().unwrap();
        a.free(s0);
        let s2 = a.alloc().unwrap();
        assert_eq!(s2.index, s0.index, "lowest free slot reused");
    }

    #[test]
    fn allocator_exhausts_at_max_slots() {
        let mut a = slots(1000, 2000);
        for _ in 0..MAX_SLOTS {
            assert!(a.alloc().is_some());
        }
        assert!(a.alloc().is_none(), "range exhausted");
    }

    #[test]
    fn ranges_are_inclusive_and_sized() {
        let a = slots(48128, 10000);
        assert_eq!(a.table_range(), (48128, 48128 + MAX_SLOTS - 1));
        assert_eq!(a.prio_range(), (10000, 10000 + MAX_SLOTS - 1));
    }

    /// Every base whose range would reach a kernel table (0, 253-255) or a
    /// kernel rule (0, 1000, 32766, 32767), or run past `u32::MAX`, is refused —
    /// the startup sweep deletes whatever its ranges hold — and the bases
    /// either side of each boundary are told apart.
    #[test]
    fn unsafe_bases_are_refused() {
        for table in [0, 1, 200, 252, 253, 255, u32::MAX - 254, u32::MAX] {
            let e = SlotAllocator::new(table, 10000).expect_err("table base accepted");
            assert!(e.to_string().starts_with(&format!("{TABLE_BASE_VAR}={table} ")), "{e}");
            assert!(e.to_string().ends_with("Use a base from 256 to 4294967040"), "{e}");
        }
        // 745..=1000 reach the VRF `l3mdev` rule at 1000.
        for prio in [0, 745, 900, 1000, 32511, 32600, 32766, 32767, 32768, u32::MAX - 255, u32::MAX] {
            let e = SlotAllocator::new(48128, prio).expect_err("priority base accepted");
            assert!(e.to_string().starts_with(&format!("{PRIO_BASE_VAR}={prio} ")), "{e}");
            assert!(
                e.to_string().ends_with("Use a base from 1 to 744 or from 1001 to 32510"),
                "{e}"
            );
        }
        for (table, prio) in [(256, 1), (256, 744), (256, 1001), (u32::MAX - 255, 32510), (48128, 10000)] {
            assert!(SlotAllocator::new(table, prio).is_ok(), "({table}, {prio}) refused");
        }
    }

    /// The bounds an error states are exactly the bases accepted, and
    /// none of those reaches a kernel rule or `lookup main`.
    #[test]
    fn priority_bounds_state_exactly_the_bases_accepted() {
        let stated = |base: u32| PRIO_BASES.iter().any(|r| r.contains(&base));
        let extremes = [u32::MAX - 256, u32::MAX - 255, u32::MAX - 1, u32::MAX];
        for base in (0..=70_000).chain(extremes) {
            assert_eq!(prio_refusal(Some(base), "x").is_none(), stated(base), "base {base}");
            if stated(base) {
                let range = base..=base + (MAX_SLOTS - 1);
                assert!(KERNEL_RULE_PRIORITIES.iter().all(|k| !range.contains(k)), "{range:?}");
                assert!(*range.end() < 32766, "{range:?}");
            }
        }
        assert_eq!(prio_bounds(), "from 1 to 744 or from 1001 to 32510");
    }

    /// The highest table base hands out tables up to `u32::MAX` itself
    /// without overflowing (a debug build panicked computing its range).
    #[test]
    fn top_table_base_allocates_without_overflow() {
        let mut a = slots(u32::MAX - (MAX_SLOTS - 1), 32510);
        assert_eq!(a.table_range(), (u32::MAX - 255, u32::MAX));
        assert_eq!(a.prio_range(), (32510, 32765));
        let last = (0..MAX_SLOTS).map(|_| a.alloc().unwrap()).last().unwrap();
        assert_eq!((last.table, last.priority), (u32::MAX, 32765));
    }

    /// Both bases refused: one message names both, each with its bounds.
    #[test]
    fn both_refused_bases_are_named_at_once() {
        let e = SlotAllocator::new(200, 32600).expect_err("accepted").to_string();
        assert!(e.starts_with(&format!("{TABLE_BASE_VAR}=200 is refused")), "{e}");
        assert!(e.contains(&format!("; {PRIO_BASE_VAR}=32600 is refused")), "{e}");
        assert!(e.contains("Use a base from 256 to 4294967040;"), "{e}");
        assert!(e.ends_with("Use a base from 1 to 744 or from 1001 to 32510"), "{e}");
        let b = Bases::from_settings(Some("200"), Some("32600"));
        assert_eq!(b.slots.expect_err("accepted"), e);
        let vars: Vec<_> = b.refused.iter().map(|r| r.var).collect();
        assert_eq!(vars, [TABLE_BASE_VAR, PRIO_BASE_VAR]);
    }

    /// Unset, empty or blank takes the default silently; a value that is
    /// not a whole number takes the default and is reported; a whole
    /// number outside the bounds — past `u32` included — is refused,
    /// naming the bounds.
    #[test]
    fn settings_default_unset_and_garbage_and_refuse_unsafe_numbers() {
        let bases = |t: Option<&str>, p: Option<&str>| {
            let b = Bases::from_settings(t, p);
            let ranges = b
                .slots
                .as_ref()
                .map(|a| (a.table_range().0, a.prio_range().0))
                .map_err(String::clone);
            (ranges, b)
        };
        for blank in [None, Some(""), Some("   "), Some("\t")] {
            let (ranges, b) = bases(blank, blank);
            assert_eq!(ranges, Ok((48128, 10000)), "{blank:?}");
            assert!(b.unparseable.is_empty() && b.findings().is_empty(), "{blank:?}");
        }
        let (ranges, b) = bases(Some(" 7000 "), Some("+20000"));
        assert_eq!(ranges, Ok((7000, 20000)));
        assert!(b.findings().is_empty());

        let (ranges, b) = bases(Some("0xBD00"), Some(" 7000x "));
        assert_eq!(ranges, Ok((48128, 10000)), "unparseable values take the defaults");
        let got: Vec<_> = b.unparseable.iter().map(|u| (u.var, u.value.as_str(), u.default)).collect();
        assert_eq!(got, [(TABLE_BASE_VAR, "0xBD00", 48128), (PRIO_BASE_VAR, "7000x", 10000)]);
        assert!(b.refused.is_empty());

        for (table, prio, var, why) in [
            (Some("4294967296"), None, TABLE_BASE_VAR, "=4294967296 is refused: it is out of range. Use a base from 256 to 4294967040"),
            (Some("-1"), None, TABLE_BASE_VAR, "=-1 is refused: it is out of range. Use a base"),
            (None, Some("99999999999999999999999"), PRIO_BASE_VAR, "=99999999999999999999999 is refused: it is out of range. Use a base from 1 to 744"),
            (Some("200"), None, TABLE_BASE_VAR, "=200 is refused: its 256 tables"),
            (None, Some(" 900 "), PRIO_BASE_VAR, "=900 is refused: its 256 rule priorities"),
        ] {
            let (ranges, b) = bases(table, prio);
            let e = ranges.expect_err("accepted");
            assert!(e.starts_with(&format!("{var}{why}")), "{e}");
            assert!(b.unparseable.is_empty(), "{:?}", b.unparseable);
        }

        // One of each: the garbage is reported, the unsafe one refused.
        let (ranges, b) = bases(Some("0xBD00"), Some("32600"));
        assert!(ranges.expect_err("accepted").starts_with(&format!("{PRIO_BASE_VAR}=32600 ")));
        assert_eq!(b.unparseable.len(), 1);
    }

    /// The boot report: a Warning per discarded value naming its default
    /// and bounds, one Critical naming every refused base, raised as
    /// `config` events.
    #[test]
    fn boot_report_names_each_variable_its_value_and_its_bounds() {
        let (events, mut rx) = crate::manager::events::event_channel();
        Bases::from_settings(Some("0xBD00"), Some("900")).report(&events);
        let warn = rx.try_recv().expect("unparseable event");
        assert_eq!(warn.severity, EventSeverity::Warning);
        assert_eq!(warn.category, category::CONFIG);
        assert!(warn.message.starts_with(&format!("{TABLE_BASE_VAR} is set to \"0xBD00\"")), "{}", warn.message);
        assert!(warn.message.contains("the default base 48128 is in use"), "{}", warn.message);
        assert!(warn.message.contains("set a whole number from 256 to 4294967040;"), "{}", warn.message);
        let d = warn.details.expect("details");
        assert_eq!(d["error_code"], "bond_rt_base_unparseable");
        assert_eq!(d["env_var"], TABLE_BASE_VAR);
        assert_eq!(d["value"], "0xBD00");
        assert_eq!(d["default"], 48128);
        assert_eq!(d["accepted"], "from 256 to 4294967040");

        let crit = rx.try_recv().expect("refusal event");
        assert_eq!(crit.severity, EventSeverity::Critical);
        assert_eq!(crit.category, category::CONFIG);
        assert!(crit.message.contains(&format!("{PRIO_BASE_VAR}=900 is refused")), "{}", crit.message);
        assert!(crit.message.contains("Use a base from 1 to 744 or from 1001 to 32510"), "{}", crit.message);
        let d = crit.details.expect("details");
        assert_eq!(d["error_code"], "bond_rt_base_refused");
        assert_eq!(d["refused"][0]["env_var"], PRIO_BASE_VAR);
        assert_eq!(d["refused"][0]["value"], "900");
        assert!(rx.try_recv().is_err(), "one event per finding");

        // Both refused: one Critical naming both.
        Bases::from_settings(Some("200"), Some("32600")).report(&events);
        let crit = rx.try_recv().expect("refusal event");
        assert!(crit.message.contains(&format!("{TABLE_BASE_VAR}=200 is refused")), "{}", crit.message);
        assert!(crit.message.contains(&format!("{PRIO_BASE_VAR}=32600 is refused")), "{}", crit.message);
        assert_eq!(crit.details.expect("details")["refused"].as_array().map(Vec::len), Some(2));
        assert!(rx.try_recv().is_err());

        // Nothing set, nothing raised: a false alarm here would be on every
        // node of the fleet at every boot.
        Bases::from_settings(None, Some("")).report(&events);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn source_net_parse_cidr_and_bare() {
        let n = SourceNet::parse("192.168.10.2/24").unwrap();
        assert_eq!(n.addr, "192.168.10.2".parse::<IpAddr>().unwrap());
        assert_eq!(n.prefix, 24);
        let bare = SourceNet::parse("10.0.0.5").unwrap();
        assert_eq!(bare.prefix, 32);
        let v6 = SourceNet::parse("fd00::2/64").unwrap();
        assert_eq!(v6.prefix, 64);
        assert!(SourceNet::parse("192.168.1.1/40").is_err());
        assert!(SourceNet::parse("not-an-ip").is_err());
    }
}
