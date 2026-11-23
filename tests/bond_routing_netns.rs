// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! `engine::bond_routing` against a real kernel.
//!
//! The module's unit tests cover only the pure slot allocator; this drives the
//! netlink half — the startup GC sweep in `BondRouteManager::global()`,
//! `program`, `is_intact`, `teardown`, `teardown_all` and `program_dish_route` —
//! and reads back what the kernel holds through a netlink connection of its own.
//!
//! It needs `CAP_NET_ADMIN` in a network namespace it may wreck, so both tests
//! are `#[ignore]`d and also skip unless `BILBYCAST_NETNS_TEST` asks for them,
//! and they refuse a namespace holding any link but `lo`.
//!
//! **CI** runs them on every pull request and push to `main`: the `check` job's
//! "bond_routing against a real kernel" step runs this binary as root under
//! `sudo unshare --net`, once per set of bases that step lists, each in a
//! fresh namespace (`.github/workflows/ci.yml`). Locally, a container gives one
//! without root on the host. The binary is glibc-linked, so on a musl image
//! run it through the host's loader:
//!
//! ```text
//! BIN=$(cargo test --test bond_routing_netns --no-run 2>&1 | sed -n 's/.*Executable .*(\(.*\))/\1/p')
//! docker run --rm --cap-add NET_ADMIN --network none -e BILBYCAST_NETNS_TEST=1 \
//!   -v /usr/lib/x86_64-linux-gnu:/hostlib:ro -v "$PWD/$BIN:/t/bond_routing_netns:ro" \
//!   alpine:latest /hostlib/ld-linux-x86-64.so.2 --library-path /hostlib \
//!   /t/bond_routing_netns --ignored --test-threads=1 --nocapture
//! ```
//!
//! `BILBYCAST_NETNS_TEST=1` runs `gateway_routing_against_a_real_kernel`. Add
//! `-e BILBYCAST_BOND_RT_TABLE_BASE=… -e BILBYCAST_BOND_RT_PRIO_BASE=…` to run
//! it against moved ranges (a table base from 256 to 4294967040, a priority
//! base from 1 to 744 or from 1001 to 32510). A value that is not a whole
//! number (`0xBD00`) takes its default, so it runs against the default range.
//! Everything it checks runs in one test: the manager is a process-wide
//! `OnceCell` and sweeps only on its first use.
//!
//! A refused base therefore needs a process of its own:
//! `-e BILBYCAST_NETNS_TEST=unsafe-bases` with at least one unsafe base runs
//! `refused_bases_touch_nothing` instead, which checks the manager sweeps and
//! programs nothing. Run it once per kind of refusal, e.g. with
//! `-e BILBYCAST_BOND_RT_TABLE_BASE=200 -e BILBYCAST_BOND_RT_PRIO_BASE=32600`
//! (tables 253–255, the kernel's `lookup main` / `lookup default` rules),
//! `BILBYCAST_BOND_RT_PRIO_BASE=900` (the VRF `l3mdev` rule at 1000),
//! `BILBYCAST_BOND_RT_TABLE_BASE=4294967200` (past `u32::MAX`),
//! `BILBYCAST_BOND_RT_TABLE_BASE=4294967296` (a whole number past `u32`) and
//! `BILBYCAST_BOND_RT_PRIO_BASE=40000` (after `lookup main`).
//!
//! Some assertions here are guards rather than regression checks: they hold
//! on every release, and say so where they stand.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::RangeInclusive;

use bilbycast_edge::engine::bond_routing::{Bases, BondRouteManager, MAX_SLOTS, SourceNet};
use futures_util::TryStreamExt;
use netlink_packet_route::address::AddressAttribute;
use netlink_packet_route::link::LinkAttribute;
use netlink_packet_route::route::{RouteAddress, RouteAttribute, RouteMessage, RouteProtocol};
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
    let slots = Bases::from_env().slots
        .expect("the bases are refused; run them with BILBYCAST_NETNS_TEST=unsafe-bases");
    let (t_lo, t_hi) = slots.table_range();
    let (p_lo, p_hi) = slots.prio_range();
    let ranges = (p_lo..=p_hi, t_lo..=t_hi);

    let (h, oif) = fresh_namespace().await;
    let gw4: IpAddr = "192.0.2.1".parse().unwrap();
    let gw6: IpAddr = "2001:db8:bc::1".parse().unwrap();

    // ── Startup GC: seed a crashed instance's leftovers in the reserved
    // ranges (lowest-but-five and topmost slot), and operator state one step
    // outside each end — none above a range ending at the last table id.
    // Seeded before the first `global()`, which sweeps.
    seed(&h, p_lo + 5, "192.0.2.99".parse().unwrap(), t_lo + 5, gw4, oif).await;
    seed(&h, p_hi, "2001:db8:bc::99".parse().unwrap(), t_hi, gw6, oif).await;
    seed(&h, p_lo - 1, "192.0.2.98".parse().unwrap(), t_lo - 1, gw4, oif).await;
    let above = t_hi.checked_add(1);
    if let Some(t) = above {
        seed(&h, p_hi + 1, "2001:db8:bc::98".parse().unwrap(), t, gw6, oif).await;
    }
    // The rule the kernel's VRF driver adds at 1000, one per family.
    seed_l3mdev(&h).await;
    let mgr = BondRouteManager::global().await.expect("global() opens netlink");
    assert_eq!(reserved(&h, &ranges).await, (vec![], vec![]), "GC left orphans");
    let (all_rules, all_routes) = (rules(&h).await, routes(&h).await);
    let rules_above = above.map(|t| rule(p_hi + 1, "2001:db8:bc::98/128", t));
    for kept in [
        rule(0, "all", 255),
        rule(32766, "all", MAIN_TABLE),
        rule(32767, "all", 253),
        rule(p_lo - 1, "192.0.2.98/32", t_lo - 1),
    ]
    .into_iter()
    .chain(rules_above)
    {
        assert!(has(&all_rules, &kept), "GC removed `{kept}`: {all_rules:#?}");
    }
    let l3mdev = all_rules.iter().filter(|(_, l)| *l == l3mdev_rule()).count();
    assert_eq!(l3mdev, 2, "GC removed the VRF `{}`: {all_rules:#?}", l3mdev_rule());
    let routes_above = above.map(|t| route(t, gw6, oif));
    for kept in [route(t_lo - 1, gw4, oif)].into_iter().chain(routes_above) {
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

    // ── is_intact checks the leg's rule too: an `ip rule flush`, or a network
    // manager removing rules it did not create, leaves the table alone. The
    // rule is deleted, or replaced by one that differs in one respect.
    let src: IpAddr = src4.addr;
    for (why, stand_in) in [
        ("deleted", None),
        ("at another priority", Some((p_lo + 7, src, 32, t_lo))),
        ("looking up another table", Some((p_lo, src, 32, t_lo + 7))),
        ("from another source", Some((p_lo, "192.0.2.3".parse().unwrap(), 32, t_lo))),
        ("from a wider prefix", Some((p_lo, src, 31, t_lo))),
    ] {
        del_rule(&h, &leg4.0).await;
        let stand_in = match stand_in {
            Some((priority, from, len, table)) => {
                Some(add_rule(&h, priority, from, len, table).await)
            }
            None => None,
        };
        assert!(!mgr.is_intact(FLOW, 0).await, "rule {why}: still reported intact");
        assert!(mgr.is_intact(FLOW, 1).await, "rule {why}: the other leg lost its standing");
        if let Some(line) = stand_in {
            del_rule(&h, &line).await;
        }
        mgr.program(FLOW, 0, IFACE, src4, gw4).await.expect("re-program after the rule went");
        assert!(mgr.is_intact(FLOW, 0).await, "rule {why}: not intact after re-program");
        assert_eq!(reserved(&h, &ranges).await, both, "rule {why}: re-program left the wrong state");
    }

    // ── teardown removes the rule and flushes the table, keeps the address.
    mgr.teardown(FLOW, 0).await;
    assert!(!mgr.is_intact(FLOW, 0).await);
    assert_eq!(reserved(&h, &ranges).await, (vec![leg6.0.clone()], vec![leg6.1.clone()]));
    mgr.teardown_all().await;
    assert_eq!(reserved(&h, &ranges).await, (vec![], vec![]), "teardown_all left state");
    assert_eq!(addresses(&h, oif).await, addrs, "teardown removed an address");

    // ── A failed program() leaves nothing behind, in the kernel or the
    // allocator: one whose rule is already there (the rule add fails after
    // the route went in), one against an unreachable gateway (the route add
    // fails), and MAX_SLOTS against a missing interface (a modem unplugged
    // through 21 min of 5 s re-asserts). Another leg then still gets slot 0.
    add_rule(&h, p_lo, src, 32, t_lo).await;
    let e = mgr.program(FLOW, 5, IFACE, src4, gw4).await.expect_err("rule already present");
    assert!(e.to_string().contains("add rule"), "{e}");
    // Guard: no release reported a failed program() intact.
    assert!(!mgr.is_intact(FLOW, 5).await, "a failed program() is reported intact");
    let nowhere: IpAddr = "198.51.100.1".parse().unwrap();
    mgr.program(FLOW, 5, IFACE, src4, nowhere).await.expect_err("unreachable gateway");
    for _ in 0..MAX_SLOTS {
        let e = mgr.program(FLOW, 6, "bvgone0", src4, gw4).await.expect_err("missing interface");
        assert!(e.to_string().contains("'bvgone0' not found"), "{e}");
    }
    assert_eq!(reserved(&h, &ranges).await, (vec![], vec![]), "a failed program() left state");
    let after = mgr.program(FLOW, 2, IFACE, src4, gw4).await;
    assert!(after.is_ok(), "the failed program()s leaked slots: {after:?}");
    let one = (vec![leg4.0.clone()], vec![leg4.1.clone()]);
    assert_eq!(reserved(&h, &ranges).await, one, "the failed program()s leaked slots");

    // ── program / teardown run whole per key: two re-programs of one leg
    // racing each other leave one leg in its slot, and a teardown made
    // while a program() of its leg holds the lock waits for it and returns
    // with the leg gone (`join!` polls the program() first, so it takes the
    // lock first). A program() made AFTER a teardown installs the leg
    // again, as it should; that a stopped bonded output's re-assert task
    // makes none is the output's job, tested in `output_bonded` with a
    // fake route manager.
    let (a, b) = tokio::join!(
        mgr.program(FLOW, 2, IFACE, src4, gw4),
        mgr.program(FLOW, 2, IFACE, src4, gw4)
    );
    a.and(b).expect("racing re-programs");
    assert_eq!(reserved(&h, &ranges).await, one, "racing re-programs left two slots");
    let (c, ()) = tokio::join!(mgr.program(FLOW, 3, IFACE, src6, gw6), mgr.teardown(FLOW, 3));
    c.expect("program racing a teardown");
    assert_eq!(reserved(&h, &ranges).await, one, "teardown returned before the program() it raced");
    // Guard: holds whenever the kernel check above does.
    assert!(!mgr.is_intact(FLOW, 3).await, "the raced leg is still active after its teardown");
    mgr.teardown_all().await;
    assert_eq!(reserved(&h, &ranges).await, (vec![], vec![]), "teardown_all left state");
    assert_eq!(addresses(&h, oif).await, addrs, "the failures changed the addresses");

    // ── program_dish_route: a scoped route in main, replaced, never doubled.
    // Unchanged behaviour, checked as a guard.
    assert_eq!(dish_routes(&h, mgr).await, [dish_route(oif)]);
}

/// Bases the manager refuses: nothing is swept, nothing is programmed, and
/// the dish route — which needs no reserved range — still works.
#[tokio::test]
#[ignore = "needs CAP_NET_ADMIN in a throwaway network namespace; see the file header"]
async fn refused_bases_touch_nothing() {
    if std::env::var("BILBYCAST_NETNS_TEST").as_deref() != Ok("unsafe-bases") {
        eprintln!(
            "skipped: set BILBYCAST_NETNS_TEST=unsafe-bases and an unsafe base, in a throwaway \
             network namespace (see the header of tests/bond_routing_netns.rs)"
        );
        return;
    }
    let refused = Bases::from_env().slots
        .expect_err("these bases are accepted; set an unsafe one (see the file header)");
    eprintln!("refused: {refused}");

    let (h, oif) = fresh_namespace().await;
    let gw4: IpAddr = "192.0.2.1".parse().unwrap();
    // A leg-shaped rule + route in the default ranges, as a crashed edge or
    // an operator who moved the bases away from them would leave them.
    seed(&h, 10005, "192.0.2.99".parse().unwrap(), 48133, gw4, oif).await;
    seed_l3mdev(&h).await;
    let before = everything(&h, oif).await;
    for kernel in [rule(32766, "all", MAIN_TABLE), rule(32767, "all", 253), l3mdev_rule()] {
        assert!(has(&before.0, &kernel), "no `{kernel}` to watch: {:#?}", before.0);
    }

    let mgr = BondRouteManager::global().await.expect("global() opens netlink");
    assert_eq!(everything(&h, oif).await, before, "the sweep ran on a refused range");
    let src4 = SourceNet::parse("192.0.2.2/24").unwrap();
    let e = mgr.program(FLOW, 0, IFACE, src4, gw4).await.expect_err("program() with refused bases");
    assert!(e.to_string().contains(&refused), "{e}");
    // Guard: a failed program() was never reported intact.
    assert!(!mgr.is_intact(FLOW, 0).await);
    mgr.teardown(FLOW, 0).await;
    mgr.teardown_all().await;
    assert_eq!(everything(&h, oif).await, before, "refused bases touched the kernel");

    // The dish route needs no reserved range, so refused bases must not
    // take it down with them (a guard on this design: `global()` succeeds).
    assert_eq!(dish_routes(&h, mgr).await, [dish_route(oif)]);
}

/// A netlink handle on a namespace holding only `lo`, given a veth pair
/// whose `IFACE` end (returned by index) carries an operator address in
/// each family, so gateways resolve.
async fn fresh_namespace() -> (Handle, u32) {
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
    for (addr, prefix) in [("192.0.2.10", 24), ("2001:db8:bc::10", 64)] {
        h.address()
            .add(oif, addr.parse().unwrap(), prefix)
            .execute()
            .await
            .expect("operator address");
    }
    (h, oif)
}

/// Program the Starlink dish route twice and return the main table's routes
/// to it: one, replaced rather than doubled.
async fn dish_routes(h: &Handle, mgr: &BondRouteManager) -> Vec<String> {
    let dish = Ipv4Addr::new(192, 168, 100, 0);
    for _ in 0..2 {
        mgr.program_dish_route(dish, 24, Ipv4Addr::new(192, 0, 2, 1), IFACE)
            .await
            .expect("program dish route");
    }
    let main = within(&routes(h).await, &(MAIN_TABLE..=MAIN_TABLE));
    main.into_iter().filter(|r| r.contains("192.168.100.0")).collect()
}

fn dish_route(oif: u32) -> String {
    format!("table 254 192.168.100.0/24 via 192.0.2.1 oif {oif}")
}

/// Every rule and route, and the addresses on `oif`.
async fn everything(h: &Handle, oif: u32) -> (Vec<(u32, String)>, Vec<(u32, String)>, Vec<String>) {
    (rules(h).await, routes(h).await, addresses(h, oif).await)
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

/// Add the rule `drivers/net/vrf.c` adds, in each family, when a
/// namespace's first VRF device is created: `1000: from all lookup
/// [l3mdev-table]`. Added directly, as no VRF module need be loaded.
async fn seed_l3mdev(h: &Handle) {
    fn l3mdev(m: &mut RuleMessage) {
        m.header.table = 0;
        m.attributes.push(RuleAttribute::L3MDev(true));
        m.attributes.push(RuleAttribute::Protocol(RouteProtocol::Kernel));
    }
    let add = || h.rule().add().priority(1000).action(RuleAction::ToTable);
    let why = "seed the VRF l3mdev rule (needs CONFIG_NET_L3_MASTER_DEV)";
    let mut v4 = add().v4();
    l3mdev(v4.message_mut());
    v4.execute().await.expect(why);
    let mut v6 = add().v6();
    l3mdev(v6.message_mut());
    v6.execute().await.expect(why);
}

/// The VRF rule as [`describe_rule`] writes it.
fn l3mdev_rule() -> String {
    "1000: from all lookup [l3mdev-table]".to_string()
}

/// Add `priority: from <from>/<len> lookup <table>`, as an operator would,
/// and return it as [`rule`] writes it.
async fn add_rule(h: &Handle, priority: u32, from: IpAddr, len: u8, table: u32) -> String {
    let add = h.rule().add().table_id(table).priority(priority).action(RuleAction::ToTable);
    match from {
        IpAddr::V4(v4) => add.v4().source_prefix(v4, len).execute().await,
        IpAddr::V6(v6) => add.v6().source_prefix(v6, len).execute().await,
    }
    .expect("add rule");
    rule(priority, &format!("{from}/{len}"), table)
}

/// Delete every rule [`rule`] would write as `line`, as `ip rule del` does.
async fn del_rule(h: &Handle, line: &str) {
    let mut found = false;
    for v in [IpVersion::V4, IpVersion::V6] {
        let dump: Vec<RuleMessage> =
            h.rule().get(v).execute().try_collect().await.expect("rule dump");
        for r in dump.into_iter().filter(|r| describe_rule(r).1 == line) {
            h.rule().del(r).execute().await.expect("delete rule");
            found = true;
        }
    }
    assert!(found, "no `{line}` to delete");
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
    let mut l3mdev = false;
    for a in &r.attributes {
        match a {
            RuleAttribute::Priority(p) => priority = *p,
            RuleAttribute::Table(t) => table = *t,
            RuleAttribute::Source(ip) => from = format!("{ip}/{}", r.header.src_len),
            RuleAttribute::L3MDev(on) => l3mdev = *on,
            _ => {}
        }
    }
    if l3mdev {
        return (priority, format!("{priority}: from {from} lookup [l3mdev-table]"));
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
