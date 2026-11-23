// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! NMOS IS-04 mDNS-SD service registration.
//!
//! Registers `_nmos-node._tcp` on the local link via the pure-Rust
//! [`mdns_sd`] crate so AMWA NMOS controllers discover this edge
//! automatically.
//!
//! ## Best-effort
//!
//! When the daemon cannot be created (`mdns-sd` can bind neither an IPv4 nor
//! an IPv6 socket on UDP 5353) or the registration fails, this module logs a
//! warning and continues — flow startup is never blocked. Returns an
//! [`Option`] handle: the record is announced for as long as the handle
//! lives, and [`NmosMdnsHandle::shutdown`] withdraws it.
//!
//! ## Host
//!
//! The record's host is `<name>-bilbycast.local.` (`record_host`), where
//! `<name>` is the node's host name, which the IS-04 node resource carries
//! as its `label` and `hostname` (`api::nmos::node_hostname`). It is not
//! `<name>.local.`: on a host whose own mDNS responder runs (avahi-daemon,
//! or systemd-resolved with MulticastDNS on), that responder already
//! answers for `<name>.local.` with the addresses of its interfaces, not
//! all of which the record names (it never names an IPv6 link-local one),
//! so the record would lose its conflict probe to it at every start and go
//! out as `<name>-2.local.`. The record can still lose one to another record
//! for the same host that names other addresses — a second edge on this
//! host listening elsewhere, or an edge on another host of the same name —
//! and the daemon then renames its host (`<name>-bilbycast-2.local.`), or
//! its instance name; each new name is logged once (`log_renames`).
//!
//! ## Addresses
//!
//! The record names the addresses the API server listens on, so a controller
//! that follows it reaches the API (`announced_addrs`). A `0.0.0.0` bind
//! names every IPv4 address of every interface that is up, a `[::]` bind
//! every IPv6 one but a link-local (`fe80::/10`) one (the edge binds `[::]`
//! IPv6-only), and any other bind address names itself, loopback included.
//! The daemon looks them up on the host's interfaces at registration and
//! follows them as they come and go (`enable_addr_auto`, narrowed with
//! `set_interfaces`); an address on a point-to-point link is never named.
//!
//! The record keeps one set of addresses for every interface. The daemon
//! announces it, and answers for it, only on an interface holding a named
//! address, with the named addresses in that interface's subnets: it tells
//! which addresses belong on an interface by prefix alone. That is why a
//! `[::]` bind leaves link-local addresses out. Every interface's link-local
//! address is in `fe80::/64`, so each interface would announce every other
//! interface's too, each unreachable off its own link. An interface whose
//! only IPv6 address is link-local therefore announces no IPv6 address: with
//! an IPv4 bind it announces the record with its IPv4 addresses alone, and
//! without one not at all. A `0.0.0.0` bind does name IPv4 link-local
//! addresses (`169.254.0.0/16`), so where more than one interface holds one,
//! each of those interfaces announces them all. The default bind
//! (`127.0.0.1` and `[::1]`) keeps the record on loopback: only this host
//! can reach the API, and only this host sees the record.
//!
//! ## Backward compatibility
//!
//! mDNS-SD registration is purely additive. Existing deployments that
//! manually register the node URL with an NMOS registry continue to work
//! unchanged; the mDNS announcement is supplementary.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// How long [`NmosMdnsHandle::shutdown`] waits for the daemon to send its
/// goodbye packets and stop. The daemon does that as soon as it reads the
/// command, so this bounds a stalled daemon, not a normal shutdown.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

/// The DNS-SD service type IS-04 gives a Node API.
const SERVICE_TYPE: &str = "_nmos-node._tcp.local.";

/// What the record's host adds to the node's host name, so that the two
/// differ (module docs, Host).
const HOST_SUFFIX: &str = "-bilbycast";

/// The host the record names: `<name>-bilbycast.local.`, `<name>` the
/// node's host name, a DNS label (as `api::nmos::node_hostname` gives),
/// cut where the suffix would take the label past the 63 bytes it holds,
/// with no hyphen left at the cut.
fn record_host(hostname: &str) -> String {
    let base = &hostname[..hostname.floor_char_boundary(63 - HOST_SUFFIX.len())];
    format!("{}{HOST_SUFFIX}.local.", base.trim_end_matches('-'))
}

/// What the record names for one part of the API server's bind list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnnouncedAddr {
    /// Every IPv4 address of every interface that is up: a `0.0.0.0` bind.
    AllV4,
    /// Every IPv6 address of every interface that is up but a link-local
    /// one: a `[::]` bind.
    AllV6,
    /// That address alone: a bind to one address.
    One(IpAddr),
}

impl AnnouncedAddr {
    /// The `mdns-sd` interface kind whose addresses the record takes:
    /// `IPv4` matches every IPv4 address, the predicate every IPv6 one
    /// [`wildcard_v6_names`] admits, and `Addr` exactly that address.
    fn if_kind(self) -> mdns_sd::IfKind {
        match self {
            Self::AllV4 => mdns_sd::IfKind::IPv4,
            Self::AllV6 => mdns_sd::IfKind::Predicate(mdns_sd::IfPredicate::new(|intf| {
                wildcard_v6_names(intf.ip())
            })),
            Self::One(ip) => mdns_sd::IfKind::Addr(ip),
        }
    }
}

/// Whether a `[::]` bind names this interface address: any IPv6 address but
/// a unicast link-local one (`fe80::/10`). `mdns-sd` matches the record's
/// addresses to an interface by prefix, and every interface's link-local
/// address shares `fe80::/64`, so a link-local address named would go out
/// on every IPv6 interface, where only its own link can reach it.
fn wildcard_v6_names(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if !v6.is_unicast_link_local())
}

impl fmt::Display for AnnouncedAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllV4 => f.write_str("every IPv4 address"),
            Self::AllV6 => f.write_str("every IPv6 address but link-local"),
            Self::One(ip) => write!(f, "{ip}"),
        }
    }
}

/// The addresses the record names for the API server's bind list. A
/// family's unspecified address stands for every address of that family,
/// and so takes in any other address of it the list holds; every other
/// address stands for itself.
fn announced_addrs(bind_ips: &[IpAddr]) -> Vec<AnnouncedAddr> {
    let all_v4 = bind_ips.contains(&IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let all_v6 = bind_ips.contains(&IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    let mut out = Vec::with_capacity(bind_ips.len());
    if all_v4 {
        out.push(AnnouncedAddr::AllV4);
    }
    if all_v6 {
        out.push(AnnouncedAddr::AllV6);
    }
    out.extend(
        bind_ips
            .iter()
            .filter(|ip| match ip {
                IpAddr::V4(_) => !all_v4,
                IpAddr::V6(_) => !all_v6,
            })
            .map(|ip| AnnouncedAddr::One(*ip)),
    );
    out
}

/// The record for this node, unregistered, its host made from `hostname`
/// by [`record_host`]. Names no address yet: the daemon fills in the
/// host's addresses that `announced` admits when it is registered.
fn service_info(
    node_uuid: &str,
    hostname: &str,
    port: u16,
    https: bool,
    announced: &[AnnouncedAddr],
) -> Result<mdns_sd::ServiceInfo, mdns_sd::Error> {
    // Instance name must be unique. Truncate to keep below the DNS-SD label
    // limit (63 chars).
    let mut instance = format!("bilbycast-edge-{}", &node_uuid[..node_uuid.len().min(8)]);
    if instance.len() > 60 {
        instance.truncate(60);
    }
    let host = record_host(hostname);

    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("api_proto".into(), if https { "https".into() } else { "http".into() });
    props.insert("api_ver".into(), "v1.3".into());
    props.insert("pri".into(), "100".into());

    let mut info =
        mdns_sd::ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), port, props)?.enable_addr_auto();
    info.set_interfaces(announced.iter().map(|a| a.if_kind()).collect());
    Ok(info)
}

/// Spawn an mDNS-SD service registration for `_nmos-node._tcp` on the local
/// network. Returns the handle that keeps the record announced (keep it for
/// the lifetime of the process and withdraw the record with
/// [`NmosMdnsHandle::shutdown`]) or `None` if registration could not be set
/// up.
///
/// `node_uuid` is the NMOS node UUID (used to make the instance name unique
/// per restart). `hostname` is the node's host name (`<name>`, a DNS
/// label), from which the record's host is made (see the module docs).
/// `port` is the bilbycast HTTP/HTTPS listen port and `bind_ips` the
/// addresses the API server listens on it, which decide the addresses the
/// record names. `https` selects the protocol attribute. Call it on the
/// Tokio runtime: it spawns the task that logs the record's renames.
pub fn spawn_nmos_node_advertisement(
    node_uuid: &str,
    hostname: &str,
    port: u16,
    https: bool,
    bind_ips: &[IpAddr],
) -> Option<NmosMdnsHandle> {
    let announced = announced_addrs(bind_ips);
    let info = match service_info(node_uuid, hostname, port, https, &announced) {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!("NMOS mDNS-SD: failed to build ServiceInfo: {e}");
            return None;
        }
    };

    // The default daemon binds to all interfaces and uses the standard
    // mDNS multicast addresses. If multicast is not available the daemon
    // creation succeeds but registration is silently dropped — we still
    // return the handle so the caller can choose to keep it alive. Since
    // mdns-sd 0.22 creation does fail (logged below) when neither the IPv4
    // nor the IPv6 socket on UDP 5353 can be bound at all.
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("NMOS mDNS-SD: failed to create daemon: {e}");
            return None;
        }
    };

    // Watched before the record is registered, so that a rename its
    // conflict probe makes is seen. The task ends when the daemon stops.
    match daemon.monitor() {
        Ok(events) => {
            tokio::spawn(log_renames(events));
        }
        Err(e) => tracing::warn!("NMOS mDNS-SD: cannot watch the record for renames: {e}"),
    }

    let fullname = info.get_fullname().to_string();
    let host = info.get_hostname().to_string();
    if let Err(e) = daemon.register(info) {
        tracing::warn!("NMOS mDNS-SD: registration failed: {e}");
        // Stop the daemon thread rather than leave it running with nothing
        // to announce.
        let _ = daemon.shutdown();
        return None;
    }
    let naming: Vec<String> = announced.iter().map(ToString::to_string).collect();
    tracing::info!(
        "NMOS mDNS-SD: registered '{fullname}' on host '{host}' port {port}, naming {}",
        naming.join(", ")
    );

    Some(NmosMdnsHandle { daemon: Some(daemon) })
}

/// Log the renames the daemon makes to settle a name conflict, each new
/// name once, until the daemon stops. A conflict probe that another
/// responder on a link wins renames the record's host (`<host>-2.local.`)
/// or its instance name (`<instance> (2)`), and the daemon reports that
/// for every record and interface the rename touches; the line names the
/// first. The daemon drops an event this task has not read when 100 are
/// waiting, so a rename amid a burst of others can go unlogged.
async fn log_renames(events: mdns_sd::Receiver<mdns_sd::DaemonEvent>) {
    let mut logged = HashSet::new();
    while let Ok(event) = events.recv_async().await {
        if let mdns_sd::DaemonEvent::NameChange(change) = event
            && logged.insert(change.new_name.clone())
        {
            tracing::warn!(
                "NMOS mDNS-SD: '{}' is taken on {}; the record now goes by '{}'",
                change.original,
                change.intf_name,
                change.new_name
            );
        }
    }
}

/// The NMOS mDNS-SD daemon and the record it announces.
///
/// The daemon runs on a thread of its own, which keeps a sender to its own
/// command queue, so dropping the last `ServiceDaemon` does not stop it:
/// stopping takes a shutdown command. [`Self::shutdown`] sends it and waits
/// for the daemon to say it has sent its goodbye packets (TTL 0, so browsers
/// drop the record) and stopped. Dropping the handle without it sends the
/// same command but cannot wait, so a process that exits straight after —
/// as the edge does, through `_exit` — may be gone before the goodbye is.
pub struct NmosMdnsHandle {
    /// `None` once [`Self::shutdown`] has taken it.
    daemon: Option<mdns_sd::ServiceDaemon>,
}

impl NmosMdnsHandle {
    /// Withdraw the record and stop the daemon, waiting up to `SHUTDOWN_WAIT`
    /// (1 s) for it to confirm. Awaits the daemon's reply on its channel
    /// rather than blocking a runtime thread on it.
    pub async fn shutdown(mut self) {
        let Some(daemon) = self.daemon.take() else {
            return;
        };
        let reply = match daemon.shutdown() {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!("NMOS mDNS-SD: could not stop the daemon: {e}");
                return;
            }
        };
        match tokio::time::timeout(SHUTDOWN_WAIT, reply.recv_async()).await {
            Ok(Ok(mdns_sd::DaemonStatus::Shutdown)) => {
                tracing::info!("NMOS mDNS-SD: record withdrawn");
            }
            Ok(Ok(other)) => {
                tracing::warn!("NMOS mDNS-SD: unexpected daemon status on shutdown: {other:?}");
            }
            Ok(Err(e)) => {
                tracing::warn!("NMOS mDNS-SD: daemon stopped without confirming: {e}");
            }
            Err(_) => {
                tracing::warn!(
                    "NMOS mDNS-SD: daemon did not confirm its shutdown within {} ms",
                    SHUTDOWN_WAIT.as_millis()
                );
            }
        }
    }
}

impl Drop for NmosMdnsHandle {
    fn drop(&mut self) {
        // Queue the shutdown and return: nothing here waits, so a drop on a
        // runtime thread never blocks it. The daemon still sends the goodbye
        // if the process outlives it by a moment.
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::AnnouncedAddr::{AllV4, AllV6, One};

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    /// The bind list a fresh install gets, parsed the way `main` parses it.
    fn default_bind_ips() -> Vec<IpAddr> {
        let server = crate::config::models::ServerConfig::default();
        crate::config::validation::parse_listen_addrs(&server.effective_listen_addrs(), "server")
            .unwrap()
    }

    #[test]
    fn the_default_loopback_bind_keeps_the_record_on_loopback() {
        assert_eq!(
            announced_addrs(&default_bind_ips()),
            [One("127.0.0.1".parse().unwrap()), One("::1".parse().unwrap())]
        );
    }

    #[test]
    fn an_unspecified_bind_names_every_address_of_its_family() {
        assert_eq!(announced_addrs(&ips(&["0.0.0.0", "::"])), [AllV4, AllV6]);
        assert_eq!(announced_addrs(&ips(&["0.0.0.0"])), [AllV4]);
        assert_eq!(announced_addrs(&ips(&["::"])), [AllV6]);
    }

    #[test]
    fn a_specific_bind_names_exactly_its_addresses() {
        assert_eq!(
            announced_addrs(&ips(&["192.168.1.10", "fd00::5"])),
            [One("192.168.1.10".parse().unwrap()), One("fd00::5".parse().unwrap())]
        );
    }

    /// An unspecified address takes in the other addresses of its family,
    /// and leaves the other family's as they are.
    #[test]
    fn an_unspecified_bind_takes_in_its_own_family_only() {
        assert_eq!(announced_addrs(&ips(&["127.0.0.1", "0.0.0.0"])), [AllV4]);
        assert_eq!(
            announced_addrs(&ips(&["0.0.0.0", "fd00::5"])),
            [AllV4, One("fd00::5".parse().unwrap())]
        );
        assert_eq!(
            announced_addrs(&ips(&["10.0.0.1", "::1", "::"])),
            [AllV6, One("10.0.0.1".parse().unwrap())]
        );
    }

    #[test]
    fn the_operator_reads_what_the_record_names() {
        let naming: Vec<String> = announced_addrs(&ips(&["0.0.0.0", "fd00::5"]))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(naming, ["every IPv4 address", "fd00::5"]);
        assert_eq!(AllV6.to_string(), "every IPv6 address but link-local");
    }

    /// The record for `bind`, unregistered.
    fn record_for(bind: &[IpAddr]) -> mdns_sd::ServiceInfo {
        service_info(
            "12345678-1234-1234-1234-123456789012",
            "test-host",
            8443,
            true,
            &announced_addrs(bind),
        )
        .unwrap()
    }

    /// The interfaces a record is restricted to, as its `Debug` shows them:
    /// `mdns-sd` has no getter for them.
    fn interfaces_of(info: &mdns_sd::ServiceInfo) -> String {
        let debug = format!("{info:?}");
        let field = "supported_intfs: ";
        let start = debug
            .find(field)
            .expect("ServiceInfo's Debug shows supported_intfs")
            + field.len();
        let len = debug[start..].find(']').expect("a list") + 1;
        debug[start..start + len].to_owned()
    }

    /// The record takes its addresses only from the interface kinds its
    /// bind names, where `mdns-sd`'s own default is every interface: what
    /// keeps the default loopback bind's record — the live test's below
    /// included — off the LAN.
    #[test]
    fn the_record_takes_addresses_only_from_what_it_names() {
        let unrestricted = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            "bilbycast-edge-12345678",
            "test-host.local.",
            (),
            8443,
            HashMap::<String, String>::new(),
        )
        .unwrap();
        assert_eq!(interfaces_of(&unrestricted), "[All]");

        assert_eq!(
            interfaces_of(&record_for(&default_bind_ips())),
            "[Addr(127.0.0.1), Addr(::1)]"
        );
        assert_eq!(
            interfaces_of(&record_for(&ips(&["192.168.1.10", "fd00::5"]))),
            "[Addr(192.168.1.10), Addr(fd00::5)]"
        );
        assert_eq!(
            interfaces_of(&record_for(&ips(&["0.0.0.0", "::"]))),
            "[IPv4, Predicate(IfPredicate(...))]"
        );
    }

    /// A `[::]` bind names no link-local IPv6 address (`fe80::/10`, to
    /// `febf::`), every other IPv6 address, and no IPv4 one.
    #[test]
    fn a_wildcard_v6_bind_names_every_ipv6_address_but_link_local() {
        for ip in ["fe80::1", "fe80::1c2b:3cff:fe4d:5e6f", "febf:ffff::1"] {
            assert!(!wildcard_v6_names(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["::1", "fd00::5", "2001:db8::1", "fec0::1", "fe7f::1"] {
            assert!(wildcard_v6_names(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["192.168.1.10", "169.254.1.1", "127.0.0.1"] {
            assert!(!wildcard_v6_names(ip.parse().unwrap()), "{ip}");
        }
    }

    /// The record's host is never the machine's own `<name>.local.`.
    #[test]
    fn the_record_host_is_the_node_name_and_a_suffix() {
        assert_eq!(record_host("edge01"), "edge01-bilbycast.local.");
        assert_eq!(
            record_host("bilbycast-edge"),
            "bilbycast-edge-bilbycast.local."
        );
    }

    /// The name is cut so the host stays one label of at most 63 bytes,
    /// with no hyphen doubled at the cut, and never inside a character.
    #[test]
    fn a_long_node_name_is_cut_to_keep_the_host_one_label() {
        let longest = "a".repeat(63);
        let label = record_host(&longest)
            .strip_suffix(".local.")
            .unwrap()
            .to_owned();
        assert_eq!(label, format!("{}-bilbycast", "a".repeat(53)));
        assert_eq!(label.len(), 63);
        let hyphen_at_cut = format!("{}-{}", "b".repeat(52), "c".repeat(10));
        assert_eq!(
            record_host(&hyphen_at_cut),
            format!("{}-bilbycast.local.", "b".repeat(52))
        );
        assert_eq!(
            record_host(&"é".repeat(30)),
            format!("{}-bilbycast.local.", "é".repeat(26))
        );
    }

    /// The port the loopback daemons below use, so that nothing they send
    /// reaches a responder on the mDNS port.
    const TEST_PORT: u16 = 53_531;

    /// The record `service_info` builds for `id` (an 8-byte name, its
    /// instance and host) and `announced`.
    fn record_of(id: &str, announced: &[AnnouncedAddr]) -> mdns_sd::ServiceInfo {
        service_info(
            &format!("{id}-0000-0000-0000-000000000000"),
            id,
            8443,
            false,
            announced,
        )
        .unwrap()
    }

    fn instance(id: &str) -> String {
        format!("bilbycast-edge-{id}.{SERVICE_TYPE}")
    }

    /// The instances of `records` that a daemon seeing only `loopback`
    /// (`IfKind::LoopbackV4` or `IfKind::LoopbackV6`) announces when they
    /// are registered, in order. The daemon fills a record's addresses in
    /// from that loopback address through the interface kinds the record
    /// carries, the predicate included, and announces only a record it gave
    /// one. Probing is off, so a record goes out as it is registered; a
    /// marker taking any address, registered last, shows every registration
    /// before it done. `None` when the daemon cannot be made, or the marker
    /// is not announced within 5 s: this host cannot announce on that
    /// loopback.
    fn announced_on_loopback(
        loopback: mdns_sd::IfKind,
        records: Vec<mdns_sd::ServiceInfo>,
    ) -> Option<Vec<String>> {
        let daemon = mdns_sd::ServiceDaemon::new_with_port(TEST_PORT).ok()?;
        let events = daemon.monitor().unwrap();
        daemon.disable_interface(mdns_sd::IfKind::All).unwrap();
        daemon.enable_interface(loopback).unwrap();
        let marker = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            "marker",
            "marker.local.",
            (),
            9,
            HashMap::<String, String>::new(),
        )
        .unwrap()
        .enable_addr_auto();
        let marker_name = marker.get_fullname().to_owned();
        for mut info in records.into_iter().chain([marker]) {
            info.set_requires_probe(false);
            daemon.register(info).unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut announced = Vec::new();
        let marked = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(left) {
                Ok(mdns_sd::DaemonEvent::Announce(name, _)) if name == marker_name => break true,
                Ok(mdns_sd::DaemonEvent::Announce(name, _)) => announced.push(name),
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        if let Ok(stopped) = daemon.shutdown() {
            let _ = stopped.recv_timeout(SHUTDOWN_WAIT);
        }
        marked.then_some(announced)
    }

    /// What the record names is what registration gives it: the daemon
    /// applies the interface kinds `service_info` set, the `[::]`
    /// predicate included. With only `127.0.0.1` to take, a `[::]` record
    /// gets no address and goes unannounced beside the `0.0.0.0` and
    /// `127.0.0.1` ones; with only `::1`, the `[::]` and `::1` ones go out
    /// and those two do not. Nothing is sent off loopback.
    #[test]
    fn registration_gives_the_record_the_addresses_its_bind_names() {
        let records = || {
            vec![
                record_of("all-ipv6", &[AllV6]),
                record_of("all-ipv4", &[AllV4]),
                record_of("one-ipv4", &[One("127.0.0.1".parse().unwrap())]),
                record_of("one-ipv6", &[One("::1".parse().unwrap())]),
            ]
        };
        let on_v4 = announced_on_loopback(mdns_sd::IfKind::LoopbackV4, records())
            .expect("an mDNS daemon announces on 127.0.0.1");
        assert_eq!(on_v4, [instance("all-ipv4"), instance("one-ipv4")]);
        match announced_on_loopback(mdns_sd::IfKind::LoopbackV6, records()) {
            Some(on_v6) => assert_eq!(on_v6, [instance("all-ipv6"), instance("one-ipv6")]),
            None => eprintln!("IPv6 half skipped: this host announces nothing on ::1"),
        }
    }

    /// The record before registration: the IS-04 TXT keys, the node's port
    /// and host, and no address of its own — the daemon fills those in.
    #[test]
    fn the_record_carries_the_node_api_and_no_fixed_address() {
        let info = record_for(&default_bind_ips());
        assert_eq!(info.get_fullname(), "bilbycast-edge-12345678._nmos-node._tcp.local.");
        assert_eq!(info.get_hostname(), "test-host-bilbycast.local.");
        assert_eq!(info.get_port(), 8443);
        assert_eq!(info.get_property_val_str("api_proto"), Some("https"));
        assert_eq!(info.get_property_val_str("api_ver"), Some("v1.3"));
        assert_eq!(info.get_property_val_str("pri"), Some("100"));
        assert!(info.is_addr_auto());
        assert!(info.get_addresses().is_empty());
    }

    /// The live daemon, on the default loopback bind so that the record it
    /// registers names loopback addresses alone and goes out on no other
    /// interface. Multicast may be unavailable (in CI, say), so `None` is
    /// accepted; neither registering nor shutting a handle down may panic or
    /// hang.
    #[tokio::test]
    async fn a_loopback_registration_does_not_panic_or_hang() {
        let handle = spawn_nmos_node_advertisement(
            "12345678-1234-1234-1234-123456789012",
            "test-host",
            8080,
            false,
            &default_bind_ips(),
        );
        if let Some(handle) = handle {
            tokio::time::timeout(SHUTDOWN_WAIT * 2, handle.shutdown())
                .await
                .expect("shutdown is bounded by SHUTDOWN_WAIT");
        }
    }
}
