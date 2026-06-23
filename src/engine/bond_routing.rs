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
//! (host-level OS tuning). At startup [`BondRouteManager::global`]
//! runs a **GC sweep** that deletes any rule/route left in those
//! ranges by a prior crash before any flow starts. Per-path teardown
//! removes that path's rule + flushes its table. Addresses the edge
//! adds are left in place on teardown (cheap, and avoids tearing down
//! an address another leg or the operator may rely on).
//!
//! Runtime note: the netlink calls themselves require a live kernel +
//! `CAP_NET_ADMIN` and are not exercised by unit tests (only the pure
//! [`SlotAllocator`] is). See `docs/bonding-gateway-routing.md`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, anyhow};
use futures_util::TryStreamExt;
use tokio::sync::{Mutex, OnceCell};

/// Default base for our reserved routing tables (`0xBC00`). Override
/// with `BILBYCAST_BOND_RT_TABLE_BASE` if it collides with existing
/// host routing tables.
const DEFAULT_TABLE_BASE: u32 = 48128;
/// Default base for our reserved `ip rule` priorities. Override with
/// `BILBYCAST_BOND_RT_PRIO_BASE`.
const DEFAULT_PRIO_BASE: u32 = 10000;
/// Maximum concurrent gateway-mode legs per host.
const MAX_SLOTS: u32 = 256;

fn table_base() -> u32 {
    std::env::var("BILBYCAST_BOND_RT_TABLE_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TABLE_BASE)
}

fn prio_base() -> u32 {
    std::env::var("BILBYCAST_BOND_RT_PRIO_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PRIO_BASE)
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
    pub fn new(table_base: u32, prio_base: u32) -> Self {
        Self {
            table_base,
            prio_base,
            used: vec![false; MAX_SLOTS as usize],
        }
    }

    /// Allocate the lowest free slot, or `None` if the reserved range
    /// is exhausted.
    pub fn alloc(&mut self) -> Option<Slot> {
        let idx = self.used.iter().position(|u| !u)? as u32;
        self.used[idx as usize] = true;
        Some(Slot {
            index: idx,
            table: self.table_base + idx,
            priority: self.prio_base + idx,
        })
    }

    pub fn free(&mut self, slot: Slot) {
        if (slot.index as usize) < self.used.len() {
            self.used[slot.index as usize] = false;
        }
    }

    /// Inclusive `(low, high)` of the reserved table range.
    pub fn table_range(&self) -> (u32, u32) {
        (self.table_base, self.table_base + MAX_SLOTS - 1)
    }
    /// Inclusive `(low, high)` of the reserved priority range.
    pub fn prio_range(&self) -> (u32, u32) {
        (self.prio_base, self.prio_base + MAX_SLOTS - 1)
    }
}

// ── Programmed-path bookkeeping ──────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PathRoute {
    slot: Slot,
}

/// Process-global manager owning the netlink handle, slot allocator,
/// and the active per-`(flow_id, path_id)` route table.
pub struct BondRouteManager {
    handle: rtnetlink::Handle,
    alloc: Mutex<SlotAllocator>,
    active: Mutex<HashMap<(String, u8), PathRoute>>,
}

static GLOBAL: OnceCell<BondRouteManager> = OnceCell::const_new();

impl BondRouteManager {
    /// Lazily build the process-global manager, opening the netlink
    /// connection and running the startup GC sweep on first use.
    pub async fn global() -> Result<&'static BondRouteManager> {
        GLOBAL
            .get_or_try_init(|| async {
                let (conn, handle, _) = rtnetlink::new_connection()
                    .context("open netlink connection for bond gateway routing")?;
                tokio::spawn(conn);
                let mgr = BondRouteManager {
                    handle,
                    alloc: Mutex::new(SlotAllocator::new(table_base(), prio_base())),
                    active: Mutex::new(HashMap::new()),
                };
                // Sweep orphans from a prior crash before any flow runs.
                if let Err(e) = mgr.gc_reserved().await {
                    tracing::warn!("bond gateway routing: startup GC sweep failed: {e}");
                }
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

    /// Tear down EVERY active gateway-mode route. Called from the edge's
    /// graceful shutdown so a clean stop removes the policy routes it
    /// installed; the startup GC remains the crash safety-net.
    pub async fn teardown_all(&self) {
        let keys: Vec<(String, u8)> = {
            let active = self.active.lock().await;
            active.keys().cloned().collect()
        };
        if keys.is_empty() {
            return;
        }
        tracing::info!(
            "bond gateway routing: tearing down {} active route(s) on shutdown",
            keys.len()
        );
        for (flow_id, path_id) in keys {
            self.teardown(&flow_id, path_id).await;
        }
    }

    /// Program a gateway-mode leg: ensure the source address is on the
    /// NIC, install `default via <gateway>` in a fresh table, and a
    /// `from <source>` rule pointing at it. Idempotent per
    /// `(flow_id, path_id)`.
    pub async fn program(
        &self,
        flow_id: &str,
        path_id: u8,
        interface: &str,
        source: SourceNet,
        gateway: IpAddr,
    ) -> Result<()> {
        let key = (flow_id.to_string(), path_id);
        // Re-program cleanly if this key was already active.
        self.teardown(flow_id, path_id).await;

        let slot = {
            let mut a = self.alloc.lock().await;
            a.alloc()
                .ok_or_else(|| anyhow!("bond gateway routing: reserved slot range exhausted (>{MAX_SLOTS} legs)"))?
        };

        let if_index = if_index(interface)?;

        // 1. ensure the source address is present on the NIC (idempotent).
        if let Err(e) = self
            .handle
            .address()
            .add(if_index, source.addr, source.prefix)
            .execute()
            .await
        {
            if !is_exists(&e) {
                self.alloc.lock().await.free(slot);
                return Err(anyhow!(
                    "bond gateway routing: add address {}/{} dev {interface}: {e}",
                    source.addr,
                    source.prefix
                ));
            }
        }

        // 2. default route via the gateway in our private table.
        if let Err(e) = self
            .add_default_route(gateway, slot.table, if_index)
            .await
        {
            self.alloc.lock().await.free(slot);
            return Err(anyhow!(
                "bond gateway routing: add default via {gateway} table {}: {e}",
                slot.table
            ));
        }

        // 3. policy rule: from <source-ip> lookup <table>.
        if let Err(e) = self
            .add_rule(source.addr, slot.table, slot.priority)
            .await
        {
            let _ = self.del_routes_in_table(slot.table).await;
            self.alloc.lock().await.free(slot);
            return Err(anyhow!(
                "bond gateway routing: add rule from {} table {}: {e}",
                source.addr,
                slot.table
            ));
        }

        self.active
            .lock()
            .await
            .insert(key, PathRoute { slot });
        tracing::info!(
            "bond gateway routing: flow={flow_id} path={path_id} → via {gateway} (table {}, prio {}, src {})",
            slot.table,
            slot.priority,
            source.addr
        );
        Ok(())
    }

    /// Whether a leg's programmed routing is still in effect: the
    /// `(flow_id, path_id)` key is active AND its private table still
    /// holds at least one route. The kernel silently empties the table
    /// on link-down / device-destroy (USB modem re-plug) without any
    /// event the socket watcher can see — the `from <source>` rule then
    /// falls through to the main default route and the leg collapses
    /// onto another leg's router while sends keep succeeding. `false`
    /// also covers a `program()` that never succeeded (key not active).
    pub async fn is_intact(&self, flow_id: &str, path_id: u8) -> bool {
        let key = (flow_id.to_string(), path_id);
        let table = match self.active.lock().await.get(&key) {
            Some(route) => route.slot.table,
            None => return false,
        };
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
        let key = (flow_id.to_string(), path_id);
        let entry = self.active.lock().await.remove(&key);
        if let Some(route) = entry {
            let _ = self.del_rules_with_priority(route.slot.priority).await;
            let _ = self.del_routes_in_table(route.slot.table).await;
            self.alloc.lock().await.free(route.slot);
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

    /// Startup sweep: delete any rule/route left in our reserved
    /// ranges by a prior crashed instance.
    async fn gc_reserved(&self) -> Result<()> {
        use rtnetlink::RouteMessageBuilder;
        let (table_lo, table_hi, prio_lo, prio_hi) = {
            let a = self.alloc.lock().await;
            let tr = a.table_range();
            let pr = a.prio_range();
            (tr.0, tr.1, pr.0, pr.1)
        };
        let mut removed_rules = 0u32;
        let mut removed_routes = 0u32;
        for ipv in [rtnetlink::IpVersion::V4, rtnetlink::IpVersion::V6] {
            let mut rules = self.handle.rule().get(ipv).execute();
            while let Some(rule) = rules.try_next().await? {
                if matches!(rule_priority(&rule), Some(p) if p >= prio_lo && p <= prio_hi)
                    && self.handle.rule().del(rule).execute().await.is_ok()
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
            let mut routes = self.handle.route().get(filter).execute();
            while let Some(route) = routes.try_next().await? {
                let t = route_table(&route);
                if t >= table_lo
                    && t <= table_hi
                    && self.handle.route().del(route).execute().await.is_ok()
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
}

/// Extract the `Priority` attribute from a rule message.
fn rule_priority(rule: &netlink_packet_route::rule::RuleMessage) -> Option<u32> {
    rule.attributes.iter().find_map(|a| match a {
        netlink_packet_route::rule::RuleAttribute::Priority(p) => Some(*p),
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

    #[test]
    fn slot_alloc_hands_out_distinct_table_and_prio() {
        let mut a = SlotAllocator::new(48128, 10000);
        let s0 = a.alloc().unwrap();
        let s1 = a.alloc().unwrap();
        assert_eq!((s0.table, s0.priority), (48128, 10000));
        assert_eq!((s1.table, s1.priority), (48129, 10001));
        assert_ne!(s0.index, s1.index);
    }

    #[test]
    fn freed_slot_is_reused_lowest_first() {
        let mut a = SlotAllocator::new(100, 200);
        let s0 = a.alloc().unwrap();
        let _s1 = a.alloc().unwrap();
        a.free(s0);
        let s2 = a.alloc().unwrap();
        assert_eq!(s2.index, s0.index, "lowest free slot reused");
    }

    #[test]
    fn allocator_exhausts_at_max_slots() {
        let mut a = SlotAllocator::new(1000, 2000);
        for _ in 0..MAX_SLOTS {
            assert!(a.alloc().is_some());
        }
        assert!(a.alloc().is_none(), "range exhausted");
    }

    #[test]
    fn ranges_are_inclusive_and_sized() {
        let a = SlotAllocator::new(48128, 10000);
        assert_eq!(a.table_range(), (48128, 48128 + MAX_SLOTS - 1));
        assert_eq!(a.prio_range(), (10000, 10000 + MAX_SLOTS - 1));
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
