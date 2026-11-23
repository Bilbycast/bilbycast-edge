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
//! must sit between the kernel's `lookup local` rule (0) and its
//! `lookup main` rule (32766). A value outside them, or one that does
//! not parse, disables gateway routing for the process — every
//! [`BondRouteManager::program`] fails naming the variable — rather than
//! falling back to the default range, which may be the very range the
//! operator moved away from. On its first use [`BondRouteManager::global`]
//! runs a **GC sweep** that deletes any rule/route left in those
//! ranges by a prior crash before any leg is programmed, and runs it
//! only on ranges that passed. Per-path teardown removes that path's rule +
//! flushes its table. Addresses the edge adds are left in place on
//! teardown (cheap, and avoids tearing down an address another leg or
//! the operator may rely on).
//!
//! Each `program` / `teardown` runs whole under the manager's one lock,
//! so two calls for one `(flow_id, path_id)` never interleave.
//!
//! The netlink half needs a live kernel + `CAP_NET_ADMIN`, so only the
//! pure [`SlotAllocator`] is unit-tested here; `tests/bond_routing_netns.rs`
//! (`#[ignore]`d, run in a throwaway network namespace) drives the rest.
//! See `docs/bonding-gateway-routing.md`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::RangeInclusive;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use tokio::sync::{Mutex, OnceCell};

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
/// Priority bases whose `MAX_SLOTS` rules sit after the kernel's
/// `lookup local` rule (priority 0) and before its `lookup main` (32766)
/// and `lookup default` (32767) rules: the sweep would delete a kernel
/// rule in range, and a leg's rule after `lookup main` is never consulted
/// for a source the main table routes.
const PRIO_BASES: RangeInclusive<u32> = 1..=32766 - MAX_SLOTS;

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
    /// rule priorities from `prio_base`, or why either base is refused
    /// (see [`TABLE_BASES`] and [`PRIO_BASES`]).
    pub fn new(table_base: u32, prio_base: u32) -> Result<Self> {
        let last = MAX_SLOTS - 1;
        if !TABLE_BASES.contains(&table_base) {
            bail!(
                "{TABLE_BASE_VAR}={table_base} is refused: its {MAX_SLOTS} tables must clear the \
                 kernel's tables 0 and 253-255 and end at or below {}. Use a base from {} to {}",
                u32::MAX,
                TABLE_BASES.start(),
                TABLE_BASES.end()
            );
        }
        if !PRIO_BASES.contains(&prio_base) {
            bail!(
                "{PRIO_BASE_VAR}={prio_base} is refused: its {MAX_SLOTS} rule priorities must sit \
                 between the kernel's `lookup local` rule (0) and its `lookup main` rule (32766). \
                 Use a base from {} to {} (priorities up to {})",
                PRIO_BASES.start(),
                PRIO_BASES.end(),
                PRIO_BASES.end() + last
            );
        }
        Ok(Self {
            table_base,
            prio_base,
            used: vec![false; MAX_SLOTS as usize],
        })
    }

    /// The allocator for the bases this host sets: each variable unset
    /// takes its default, and a value that does not parse is refused like
    /// an out-of-range one — never replaced by the default.
    pub fn from_settings(table_base: Option<&str>, prio_base: Option<&str>) -> Result<Self> {
        let base = |var: &str, raw: Option<&str>, default: u32| match raw {
            None => Ok(default),
            Some(raw) => raw
                .trim()
                .parse::<u32>()
                .map_err(|_| anyhow!("{var}={raw:?} is refused: it is not a whole number")),
        };
        Self::new(
            base(TABLE_BASE_VAR, table_base, DEFAULT_TABLE_BASE)?,
            base(PRIO_BASE_VAR, prio_base, DEFAULT_PRIO_BASE)?,
        )
    }

    /// [`SlotAllocator::from_settings`] over this process's environment.
    pub fn from_env() -> Result<Self> {
        let var = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        Self::from_settings(var(TABLE_BASE_VAR).as_deref(), var(PRIO_BASE_VAR).as_deref())
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
                let routes = match SlotAllocator::from_env() {
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
                    Err(e) => {
                        tracing::error!(
                            "bond gateway routing disabled, and no orphan sweep run: {e:#}"
                        );
                        Err(format!("{e:#}"))
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
    /// `(flow_id, path_id)`; on failure nothing of the leg is left, in
    /// the kernel or the allocator.
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
    /// kernel rule (0, 32766, 32767), or run past `u32::MAX`, is refused —
    /// the startup sweep deletes whatever its ranges hold — and the bases
    /// either side of each boundary are told apart.
    #[test]
    fn unsafe_bases_are_refused() {
        for table in [0, 1, 200, 252, 253, 255, u32::MAX - 254, u32::MAX] {
            let e = SlotAllocator::new(table, 10000).expect_err("table base accepted");
            assert!(e.to_string().starts_with(&format!("{TABLE_BASE_VAR}={table} ")), "{e}");
        }
        for prio in [0, 32511, 32600, 32766, 32767, 32768, u32::MAX - 255, u32::MAX] {
            let e = SlotAllocator::new(48128, prio).expect_err("priority base accepted");
            assert!(e.to_string().starts_with(&format!("{PRIO_BASE_VAR}={prio} ")), "{e}");
        }
        for (table, prio) in [(256, 1), (u32::MAX - 255, 32510), (48128, 10000)] {
            assert!(SlotAllocator::new(table, prio).is_ok(), "({table}, {prio}) refused");
        }
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

    /// Unset takes the default; a value that does not parse is refused,
    /// never replaced by the default range it may have been set to avoid.
    #[test]
    fn settings_default_when_unset_and_refuse_garbage() {
        let a = SlotAllocator::from_settings(None, None).unwrap();
        assert_eq!((a.table_range().0, a.prio_range().0), (48128, 10000));
        let a = SlotAllocator::from_settings(Some(" 7000 "), Some("20000")).unwrap();
        assert_eq!((a.table_range().0, a.prio_range().0), (7000, 20000));
        for (table, prio, var) in [
            (Some("0xBD00"), None, TABLE_BASE_VAR),
            (Some(""), None, TABLE_BASE_VAR),
            (None, Some("-1"), PRIO_BASE_VAR),
            (Some("4294967296"), None, TABLE_BASE_VAR),
            (Some("200"), None, TABLE_BASE_VAR),
            (None, Some("32600"), PRIO_BASE_VAR),
        ] {
            let e = SlotAllocator::from_settings(table, prio).expect_err("accepted");
            assert!(e.to_string().starts_with(&format!("{var}=")), "{e}");
        }
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
