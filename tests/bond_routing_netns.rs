// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `engine::bond_routing` against a real kernel.
//!
//! The module's unit tests cover only the pure slot allocator; this drives the
//! netlink half — the startup GC sweep in `BondRouteManager::global()`,
//! `program`, `is_intact`, `teardown`, `teardown_all` and `program_dish_route` —
//! and reads back what the kernel holds through a netlink connection of its own.
//!
//! It needs `CAP_NET_ADMIN` in a network namespace it may wreck, so it is
//! `#[ignore]`d and also skips unless `BILBYCAST_NETNS_TEST=1`, and it refuses
//! a namespace holding any link but `lo`. A container gives it one without
//! root on the host. The binary is glibc-linked, so on a musl image run it
//! through the host's loader:
//!
//! ```text
//! BIN=$(cargo test --test bond_routing_netns --no-run 2>&1 | sed -n 's/.*Executable .*(\(.*\))/\1/p')
//! docker run --rm --cap-add NET_ADMIN --network none -e BILBYCAST_NETNS_TEST=1 \
//!   -v /usr/lib/x86_64-linux-gnu:/hostlib:ro -v "$PWD/$BIN:/t/bond_routing_netns:ro" \
//!   alpine:latest /hostlib/ld-linux-x86-64.so.2 --library-path /hostlib \
//!   /t/bond_routing_netns --ignored --test-threads=1 --nocapture
//! ```
//!
//! Add `-e BILBYCAST_BOND_RT_TABLE_BASE=… -e BILBYCAST_BOND_RT_PRIO_BASE=…` to
//! run it against moved ranges (each base at least 1, clear of 253–255 and of
//! priorities 32766–32767). Everything runs in one test: the manager is a
//! process-wide `OnceCell` and sweeps only on its first use.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::RangeInclusive;

use bilbycast_edge::engine::bond_routing::{BondRouteManager, SlotAllocator, SourceNet};
use futures_util::TryStreamExt;
use netlink_packet_route::address::AddressAttribute;
use netlink_packet_route::link::LinkAttribute;
use netlink_packet_route::route::{RouteAddress, RouteAttribute, RouteMessage};
use netlink_packet_route::rule::{RuleAction, RuleAttribute, RuleMessage};
use rtnetlink::{Handle, IpVersion, LinkUnspec, LinkVeth, RouteMessageBuilder};

const IFACE: &str = "bvtest0";
const PEER: &str = "bvtest1";
const FLOW: &str = "netns-flow";
const MAIN_TABLE: u32 = 254;

#[tokio::test]
#[ignore = "needs CAP_NET_ADMIN in a throwaway network namespace; see the file header"]
async fn gateway_routing_against_a_real_kernel() {
    if std::env::var("BILBYCAST_NETNS_TEST").as_deref() != Ok("1") {
        eprintln!(
            "skipped: set BILBYCAST_NETNS_TEST=1 and run in a throwaway network namespace \
             (see the header of tests/bond_routing_netns.rs)"
        );
        return;
    }
    let slots = SlotAllocator::new(
        base("BILBYCAST_BOND_RT_TABLE_BASE", 48128),
        base("BILBYCAST_BOND_RT_PRIO_BASE", 10000),
    );
    let (t_lo, t_hi) = slots.table_range();
    let (p_lo, p_hi) = slots.prio_range();
    let ranges = (p_lo..=p_hi, t_lo..=t_hi);

    let (conn, h, _) = rtnetlink::new_connection().expect("netlink socket");
    tokio::spawn(conn);
    let links: Vec<String> = h
        .link()
        .get()
        .execute()
        .map_ok(|l| {
            l.attributes
                .into_iter()
                .find_map(|a| match a {
                    LinkAttribute::IfName(n) => Some(n),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .try_collect()
        .await
        .expect("link dump");
    assert_eq!(links, ["lo"], "refusing to run outside a fresh network namespace");

    // A NIC with an operator address in each family, so gateways resolve.
    h.link()
        .add(LinkVeth::new(IFACE, PEER).build())
        .execute()
        .await
        .expect("create veth pair");
    for name in [IFACE, PEER] {
        h.link()
            .set(LinkUnspec::new_with_name(name).up().build())
            .execute()
            .await
            .expect("link up");
    }
    let oif = h
        .link()
        .get()
        .match_name(IFACE)
        .execute()
        .try_next()
        .await
        .expect("link lookup")
        .expect("veth exists")
        .header
        .index;
    let gw4: IpAddr = "192.0.2.1".parse().unwrap();
    let gw6: IpAddr = "2001:db8:bc::1".parse().unwrap();
    for (addr, prefix) in [("192.0.2.10", 24), ("2001:db8:bc::10", 64)] {
        h.address()
            .add(oif, addr.parse().unwrap(), prefix)
            .execute()
            .await
            .expect("operator address");
    }

    // ── Startup GC: seed a crashed instance's leftovers in the reserved
    // ranges (lowest-but-five and topmost slot), and operator state one step
    // outside each end. Seeded before the first `global()`, which sweeps.
    seed(&h, p_lo + 5, "192.0.2.99".parse().unwrap(), t_lo + 5, gw4, oif).await;
    seed(&h, p_hi, "2001:db8:bc::99".parse().unwrap(), t_hi, gw6, oif).await;
    seed(&h, p_lo - 1, "192.0.2.98".parse().unwrap(), t_lo - 1, gw4, oif).await;
    seed(&h, p_hi + 1, "2001:db8:bc::98".parse().unwrap(), t_hi + 1, gw6, oif).await;
    let mgr = BondRouteManager::global().await.expect("global() opens netlink");
    assert_eq!(reserved(&h, &ranges).await, (vec![], vec![]), "GC left orphans");
    let (all_rules, all_routes) = (rules(&h).await, routes(&h).await);
    for kept in [
        rule(0, "all", 255),
        rule(32766, "all", MAIN_TABLE),
        rule(32767, "all", 253),
        rule(p_lo - 1, "192.0.2.98/32", t_lo - 1),
        rule(p_hi + 1, "2001:db8:bc::98/128", t_hi + 1),
    ] {
        assert!(has(&all_rules, &kept), "GC removed `{kept}`: {all_rules:#?}");
    }
    for kept in [route(t_lo - 1, gw4, oif), route(t_hi + 1, gw6, oif)] {
        assert!(has(&all_routes, &kept), "GC removed `{kept}`: {all_routes:#?}");
    }

    // ── program: one leg per family takes slots 0 and 1.
    let src4 = SourceNet::parse("192.0.2.2/24").unwrap();
    let src6 = SourceNet::parse("2001:db8:bc::2/64").unwrap();
    mgr.program(FLOW, 0, IFACE, src4, gw4).await.expect("program v4 leg");
    mgr.program(FLOW, 1, IFACE, src6, gw6).await.expect("program v6 leg");
    let leg4 = (rule(p_lo, "192.0.2.2/32", t_lo), route(t_lo, gw4, oif));
    let leg6 = (rule(p_lo + 1, "2001:db8:bc::2/128", t_lo + 1), route(t_lo + 1, gw6, oif));
    let both = (
        sorted([leg4.0.clone(), leg6.0.clone()]),
        sorted([leg4.1.clone(), leg6.1.clone()]),
    );
    assert_eq!(reserved(&h, &ranges).await, both);
    let addrs = ["192.0.2.10/24", "192.0.2.2/24", "2001:db8:bc::10/64", "2001:db8:bc::2/64"];
    assert_eq!(addresses(&h, oif).await, addrs, "program did not add the source address");

    // ── is_intact: the kernel empties a leg's table on link-down; do that
    // behind the manager's back, then re-program as the 5 s re-assert does.
    assert!(mgr.is_intact(FLOW, 0).await && mgr.is_intact(FLOW, 1).await);
    flush_table(&h, t_lo).await;
    assert!(!mgr.is_intact(FLOW, 0).await, "emptied table still reported intact");
    assert!(mgr.is_intact(FLOW, 1).await, "the other leg lost its standing");
    mgr.program(FLOW, 0, IFACE, src4, gw4).await.expect("re-program v4 leg");
    assert!(mgr.is_intact(FLOW, 0).await);
    assert_eq!(reserved(&h, &ranges).await, both, "re-program moved or doubled the leg");
    assert_eq!(addresses(&h, oif).await, addrs, "re-program changed the addresses");

    // ── teardown removes the rule and flushes the table, keeps the address.
    mgr.teardown(FLOW, 0).await;
    assert!(!mgr.is_intact(FLOW, 0).await);
    assert_eq!(reserved(&h, &ranges).await, (vec![leg6.0], vec![leg6.1]));
    mgr.teardown_all().await;
    assert_eq!(reserved(&h, &ranges).await, (vec![], vec![]), "teardown_all left state");
    assert_eq!(addresses(&h, oif).await, addrs, "teardown removed an address");

    // ── program_dish_route: a scoped route in main, replaced, never doubled.
    let dish = Ipv4Addr::new(192, 168, 100, 0);
    for _ in 0..2 {
        mgr.program_dish_route(dish, 24, Ipv4Addr::new(192, 0, 2, 1), IFACE)
            .await
            .expect("program dish route");
    }
    let main = within(&routes(&h).await, &(MAIN_TABLE..=MAIN_TABLE));
    let dish_routes: Vec<&String> = main.iter().filter(|r| r.contains("192.168.100.0")).collect();
    assert_eq!(dish_routes, [&format!("table 254 192.168.100.0/24 via 192.0.2.1 oif {oif}")]);
}

/// A reserved-range base, read the way `engine::bond_routing` reads it.
fn base(var: &str, default: u32) -> u32 {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Install a leg-shaped rule + default route, as a crashed edge or an
/// operator would leave them.
async fn seed(h: &Handle, priority: u32, from: IpAddr, table: u32, via: IpAddr, oif: u32) {
    let add_rule = h.rule().add().table_id(table).priority(priority).action(RuleAction::ToTable);
    let default_route = match (from, via) {
        (IpAddr::V4(src), IpAddr::V4(gw)) => {
            add_rule.v4().source_prefix(src, 32).execute().await.expect("seed rule");
            RouteMessageBuilder::<Ipv4Addr>::new()
                .destination_prefix(Ipv4Addr::UNSPECIFIED, 0)
                .gateway(gw)
                .output_interface(oif)
                .table_id(table)
                .build()
        }
        (IpAddr::V6(src), IpAddr::V6(gw)) => {
            add_rule.v6().source_prefix(src, 128).execute().await.expect("seed rule");
            RouteMessageBuilder::<Ipv6Addr>::new()
                .destination_prefix(Ipv6Addr::UNSPECIFIED, 0)
                .gateway(gw)
                .output_interface(oif)
                .table_id(table)
                .build()
        }
        _ => unreachable!("one family per seed"),
    };
    h.route().add(default_route).execute().await.expect("seed route");
}

/// Delete every route in `table`, as a link-down does.
async fn flush_table(h: &Handle, table: u32) {
    for r in dump_routes(h).await.into_iter().filter(|r| route_key(r).0 == table) {
        h.route().del(r).execute().await.expect("flush route");
    }
}

/// The rules and routes inside the reserved `(priority, table)` ranges, sorted.
async fn reserved(
    h: &Handle,
    (prios, tables): &(RangeInclusive<u32>, RangeInclusive<u32>),
) -> (Vec<String>, Vec<String>) {
    (within(&rules(h).await, prios), within(&routes(h).await, tables))
}

fn within(lines: &[(u32, String)], range: &RangeInclusive<u32>) -> Vec<String> {
    sorted(lines.iter().filter(|(k, _)| range.contains(k)).map(|(_, l)| l.clone()))
}

fn has(lines: &[(u32, String)], line: &str) -> bool {
    lines.iter().any(|(_, l)| l == line)
}

fn sorted(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = lines.into_iter().collect();
    v.sort();
    v
}

fn rule(priority: u32, from: &str, table: u32) -> String {
    format!("{priority}: from {from} lookup {table}")
}

fn route(table: u32, via: IpAddr, oif: u32) -> String {
    format!("table {table} default via {via} oif {oif}")
}

/// Every rule of both families, keyed by priority.
async fn rules(h: &Handle) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for v in [IpVersion::V4, IpVersion::V6] {
        let dump: Vec<RuleMessage> =
            h.rule().get(v).execute().try_collect().await.expect("rule dump");
        out.extend(dump.iter().map(describe_rule));
    }
    out
}

fn describe_rule(r: &RuleMessage) -> (u32, String) {
    let (mut priority, mut table, mut from) = (0, u32::from(r.header.table), "all".to_string());
    for a in &r.attributes {
        match a {
            RuleAttribute::Priority(p) => priority = *p,
            RuleAttribute::Table(t) => table = *t,
            RuleAttribute::Source(ip) => from = format!("{ip}/{}", r.header.src_len),
            _ => {}
        }
    }
    (priority, rule(priority, &from, table))
}

/// Every route of both families, keyed by table.
async fn routes(h: &Handle) -> Vec<(u32, String)> {
    dump_routes(h).await.iter().map(route_key).collect()
}

async fn dump_routes(h: &Handle) -> Vec<RouteMessage> {
    let mut out = Vec::new();
    for filter in [
        RouteMessageBuilder::<Ipv4Addr>::new().build(),
        RouteMessageBuilder::<Ipv6Addr>::new().build(),
    ] {
        let dump: Vec<RouteMessage> =
            h.route().get(filter).execute().try_collect().await.expect("route dump");
        out.extend(dump);
    }
    out
}

fn route_key(r: &RouteMessage) -> (u32, String) {
    let ip = |a: &RouteAddress| match a {
        RouteAddress::Inet(v4) => v4.to_string(),
        RouteAddress::Inet6(v6) => v6.to_string(),
        other => format!("{other:?}"),
    };
    let (mut table, mut dst) = (u32::from(r.header.table), "default".to_string());
    let (mut via, mut oif) = (String::new(), String::new());
    for a in &r.attributes {
        match a {
            RouteAttribute::Table(t) => table = *t,
            RouteAttribute::Destination(d) => {
                dst = format!("{}/{}", ip(d), r.header.destination_prefix_length)
            }
            RouteAttribute::Gateway(g) => via = format!(" via {}", ip(g)),
            RouteAttribute::Oif(i) => oif = format!(" oif {i}"),
            _ => {}
        }
    }
    (table, format!("table {table} {dst}{via}{oif}"))
}

/// The addresses on link `oif`, as `addr/prefix`, sorted — less the IPv6
/// link-local the kernel adds itself on link-up.
async fn addresses(h: &Handle, oif: u32) -> Vec<String> {
    let dump: Vec<_> = h
        .address()
        .get()
        .set_link_index_filter(oif)
        .execute()
        .try_collect()
        .await
        .expect("address dump");
    sorted(dump.iter().filter_map(|m| {
        m.attributes.iter().find_map(|a| match a {
            AddressAttribute::Address(IpAddr::V6(v6)) if v6.is_unicast_link_local() => None,
            AddressAttribute::Address(ip) => Some(format!("{ip}/{}", m.header.prefix_len)),
            _ => None,
        })
    }))
}
