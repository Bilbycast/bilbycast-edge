// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Zero-impact pre-flight validation for a single bond leg.
//!
//! Answers the operator's question *"is this leg usable and correctly
//! wired?"* **without putting any media on the wire** and **without
//! touching a live `BondSocket`, scheduler, token bucket, or routing
//! state**. It is the deliberate, operator-triggered counterpart to the
//! always-on adaptive capacity discovery the running bond already does
//! (which is passive + demand-bound and therefore can't answer the
//! pre-flight / commissioning question).
//!
//! ## Why this is zero-impact by construction
//!
//! Phase 1 emits **zero packets**. It introspects the host (sysfs NIC
//! enumeration, the kernel neighbour table) and uses the one OS trick
//! that reveals routing without sending anything: **`connect()` on a UDP
//! socket performs the kernel route lookup but transmits no datagram**,
//! so `getsockname()` afterwards reveals the source IP / interface the
//! leg *would actually* egress on. Binding an ephemeral socket — even on
//! the same interface a live leg uses — puts nothing on the wire and
//! never aliases the production socket.
//!
//! The active throughput probe (Phase 2) — which *does* generate bounded
//! synthetic traffic — is idle-gated and lives elsewhere; see
//! `bilbycast-bonding/docs/leg-bandwidth-test.md`.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use serde::Serialize;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::config::models::BondPathTransportConfig;
use crate::util::network_interfaces::{self, NetworkInterfaceInfo};

/// Outcome of a single check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Verified good.
    Pass,
    /// Non-fatal — leg will work but something is sub-optimal or
    /// unverifiable (e.g. missing privilege, no recent first-hop traffic).
    Warn,
    /// Leg is mis-wired / unusable as configured.
    Fail,
    /// Not applicable to this leg shape (e.g. egress on a receiver-role
    /// listener, or first-hop on an interface-mode leg).
    Skipped,
}

/// One named check in the report.
#[derive(Debug, Clone, Serialize)]
pub struct LegCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

impl LegCheck {
    fn new(name: &'static str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self { name, status, detail: detail.into() }
    }
}

/// Full pre-flight report for one leg.
#[derive(Debug, Clone, Serialize)]
pub struct LegTestReport {
    /// True iff no check is `Fail` (warnings don't fail the leg).
    pub ok: bool,
    /// `"udp"` / `"quic"` / `"rist"`.
    pub transport: String,
    /// `"interface"` (NIC-pinned), `"gateway"` (policy-routed),
    /// `"default_route"` (no pin), or `"listener"` (receiver/server role).
    pub mode: String,
    pub interface: Option<String>,
    pub source_ip: Option<String>,
    pub remote: Option<String>,
    /// The interface the kernel would actually egress this leg through,
    /// discovered via the zero-packet `connect()`/`getsockname()` probe.
    pub chosen_egress_interface: Option<String>,
    pub mtu: Option<u32>,
    pub link_speed_mbps: Option<u64>,
    /// Whether a running flow currently uses this leg (caller-supplied
    /// hint). Phase 1 is zero-impact regardless; this gates the Phase 2
    /// active probe.
    pub leg_live: bool,
    pub checks: Vec<LegCheck>,
    pub note: String,
}

/// Normalised view of a leg's network identity, independent of transport.
/// Shared with the active capacity probe (`engine::bond_leg_probe`).
pub(crate) struct LegNet {
    pub(crate) transport: &'static str,
    /// NIC pin (`SO_BINDTODEVICE` / `IP_UNICAST_IF`).
    pub(crate) interface: Option<String>,
    /// Source address the leg binds to (gateway-mode `source`, or a
    /// UDP/QUIC `bind`). May carry a `/prefix`.
    pub(crate) source: Option<String>,
    /// Gateway-mode next hop.
    pub(crate) gateway: Option<String>,
    /// Destination `host:port` (sender-role legs only).
    pub(crate) remote: Option<String>,
}

pub(crate) fn normalize(t: &BondPathTransportConfig) -> LegNet {
    match t {
        BondPathTransportConfig::Udp { bind, remote, interface, gateway, source } => LegNet {
            transport: "udp",
            interface: interface.clone(),
            source: source.clone().or_else(|| bind.clone()),
            gateway: gateway.clone(),
            remote: remote.clone(),
        },
        BondPathTransportConfig::Rist { remote, local_bind, .. } => LegNet {
            transport: "rist",
            interface: None,
            source: local_bind.clone(),
            gateway: None,
            remote: remote.clone(),
        },
        BondPathTransportConfig::Quic { role, addr, bind, interface, .. } => {
            // Client role dials `addr`; server role *binds* it (listener).
            let is_client = matches!(role, crate::config::models::BondQuicRole::Client);
            LegNet {
                transport: "quic",
                interface: interface.clone(),
                source: bind.clone(),
                gateway: None,
                remote: if is_client { Some(addr.clone()) } else { None },
            }
        }
    }
}

/// Strip a `:port` and/or `/prefix` off an address string and parse the
/// bare IP.
pub(crate) fn ip_only(s: &str) -> Option<IpAddr> {
    let s = s.trim();
    if let Some((head, _)) = s.split_once('/') {
        return head.trim().parse().ok();
    }
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Some(sa.ip());
    }
    s.parse().ok()
}

/// Parse an IPv4/IPv6 string (possibly `ip/prefix`) from a NIC's address
/// list into a bare `IpAddr`.
pub(crate) fn nic_ip(s: &str) -> Option<IpAddr> {
    s.split('/').next().and_then(|h| h.trim().parse().ok())
}

pub(crate) fn find_nic<'a>(
    nics: &'a [NetworkInterfaceInfo],
    name: &str,
) -> Option<&'a NetworkInterfaceInfo> {
    nics.iter().find(|n| n.name == name)
}

/// Which NIC owns `ip` (matched against each interface's address list)?
pub(crate) fn nic_for_ip(nics: &[NetworkInterfaceInfo], ip: IpAddr) -> Option<String> {
    nics.iter()
        .find(|n| {
            n.ipv4.iter().chain(n.ipv6.iter()).any(|a| nic_ip(a) == Some(ip))
        })
        .map(|n| n.name.clone())
}

struct EgressProbe {
    chosen_interface: Option<String>,
    local_ip: IpAddr,
    /// Whether the strict `SO_BINDTODEVICE` pin was applied; carries the
    /// error string when it could not be (e.g. missing `CAP_NET_RAW`).
    strict_error: Option<String>,
}

/// Build an ephemeral UDP socket bound exactly as the leg will bind —
/// strict `SO_BINDTODEVICE` for an interface-mode pin (Linux; needs
/// `CAP_NET_RAW`), falling back to the NIC's primary source IP (the
/// unprivileged `IP_UNICAST_IF`-equivalent path) when the strict pin
/// can't be applied; or the explicit gateway-mode `source` / UDP `bind`.
/// Returns the bound (but not connected) socket and any strict-pin error.
/// **Binding sends nothing on the wire.** Shared by the Phase 1 egress
/// probe and the Phase 2 capacity probe.
pub(crate) fn bind_leg_socket(
    net: &LegNet,
    nics: &[NetworkInterfaceInfo],
    want_ipv4: bool,
) -> std::io::Result<(Socket, Option<String>)> {
    let domain = if want_ipv4 { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true).ok();

    let mut strict_error: Option<String> = None;
    if let Some(ifn) = &net.interface {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
        {
            if let Err(e) = socket.bind_device(Some(ifn.as_bytes())) {
                strict_error = Some(e.to_string());
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "fuchsia")))]
        {
            strict_error = Some("SO_BINDTODEVICE is Linux-only".to_string());
        }
    }

    let bind_ip: Option<IpAddr> = net.source.as_deref().and_then(ip_only).or_else(|| {
        if strict_error.is_some() {
            net.interface.as_deref().and_then(|ifn| {
                find_nic(nics, ifn).and_then(|n| {
                    n.ipv4
                        .iter()
                        .chain(n.ipv6.iter())
                        .filter_map(|a| nic_ip(a))
                        .find(|ip| ip.is_ipv4() == want_ipv4)
                })
            })
        } else {
            None
        }
    });
    let bind_sa: SocketAddr = match bind_ip {
        Some(ip) => SocketAddr::new(ip, 0),
        None if want_ipv4 => SocketAddr::new(IpAddr::from([0u8, 0, 0, 0]), 0),
        None => SocketAddr::new(IpAddr::from([0u16; 8]), 0),
    };
    socket.bind(&SockAddr::from(bind_sa))?;
    Ok((socket, strict_error))
}

/// Build an ephemeral socket bound exactly as the leg would be, then
/// `connect()` to the remote to force a kernel route lookup — **no
/// datagram is sent** — and read back the chosen source via
/// `getsockname()`. Pure introspection.
fn probe_egress(net: &LegNet, nics: &[NetworkInterfaceInfo]) -> Result<EgressProbe, String> {
    let remote_str = net.remote.as_deref().ok_or("no remote")?;
    let remote: SocketAddr = remote_str
        .parse()
        .or_else(|_| {
            // Allow host:port — DNS resolution is a control-plane op, not
            // leg traffic.
            remote_str
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
                .ok_or(())
        })
        .map_err(|_| format!("cannot resolve remote '{remote_str}'"))?;

    let (socket, strict_error) =
        bind_leg_socket(net, nics, remote.is_ipv4()).map_err(|e| e.to_string())?;

    // Route lookup only — UDP connect() sends nothing on the wire.
    socket
        .connect(&SockAddr::from(remote))
        .map_err(|e| format!("connect({remote}): {e}"))?;

    let local = socket
        .local_addr()
        .ok()
        .and_then(|sa| sa.as_socket())
        .ok_or("getsockname() failed")?;

    Ok(EgressProbe {
        chosen_interface: nic_for_ip(nics, local.ip()),
        local_ip: local.ip(),
        strict_error,
    })
}

/// Read the kernel neighbour table (Linux) to see whether the gateway is
/// already resolved — a pure read, no probe packet. `(status, detail)`.
fn neighbor_reachable(gw: &str) -> (CheckStatus, String) {
    let gw_ip = match ip_only(gw) {
        Some(ip) => ip,
        None => return (CheckStatus::Warn, format!("unparseable gateway '{gw}'")),
    };
    #[cfg(target_os = "linux")]
    {
        // /proc/net/arp: "IP address  HW type  Flags  HW address  Mask  Device"
        // Flags bit 0x2 = ATF_COM (entry complete).
        match std::fs::read_to_string("/proc/net/arp") {
            Ok(table) => {
                for line in table.lines().skip(1) {
                    let mut cols = line.split_whitespace();
                    let ip = cols.next();
                    let _hw = cols.next();
                    let flags = cols.next();
                    if ip.and_then(|s| s.parse::<IpAddr>().ok()) == Some(gw_ip) {
                        let complete = flags
                            .and_then(|f| u32::from_str_radix(f.trim_start_matches("0x"), 16).ok())
                            .map(|f| f & 0x2 != 0)
                            .unwrap_or(false);
                        return if complete {
                            (CheckStatus::Pass, format!("gateway {gw_ip} resolved in neighbour table"))
                        } else {
                            (CheckStatus::Warn, format!("gateway {gw_ip} present but incomplete (unreachable / resolving)"))
                        };
                    }
                }
                (
                    CheckStatus::Warn,
                    format!("gateway {gw_ip} not in neighbour table — no recent first-hop traffic; reachability unverified"),
                )
            }
            Err(e) => (CheckStatus::Warn, format!("could not read neighbour table: {e}")),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = gw_ip;
        (CheckStatus::Skipped, "neighbour-table read is Linux-only".to_string())
    }
}

/// Run the zero-impact pre-flight on one leg. Synchronous (sysfs +
/// socket syscalls); call via `spawn_blocking` from async contexts.
pub fn test_leg(transport: &BondPathTransportConfig, leg_live: bool) -> LegTestReport {
    let net = normalize(transport);
    let nics = network_interfaces::enumerate();

    let is_listener = net.remote.is_none();
    let mode = if is_listener {
        "listener"
    } else if net.gateway.is_some() {
        "gateway"
    } else if net.interface.is_some() {
        "interface"
    } else {
        "default_route"
    };

    let mut checks: Vec<LegCheck> = Vec::new();
    let mut mtu: Option<u32> = None;
    let mut link_speed_mbps: Option<u64> = None;

    // ── interface_link ──────────────────────────────────────────────
    if let Some(ifn) = &net.interface {
        match find_nic(&nics, ifn) {
            Some(n) => {
                mtu = n.mtu;
                link_speed_mbps = n.link_speed_mbps;
                let speed = n
                    .link_speed_mbps
                    .map(|s| format!(", {s} Mbps"))
                    .unwrap_or_default();
                let m = n.mtu.map(|m| format!(", MTU {m}")).unwrap_or_default();
                match n.is_up {
                    Some(false) => checks.push(LegCheck::new(
                        "interface_link",
                        CheckStatus::Fail,
                        format!("NIC '{ifn}' is DOWN (no carrier){m}{speed}"),
                    )),
                    _ => checks.push(LegCheck::new(
                        "interface_link",
                        CheckStatus::Pass,
                        format!("NIC '{ifn}' up{m}{speed}"),
                    )),
                }
            }
            None => {
                let avail: Vec<&str> = nics.iter().map(|n| n.name.as_str()).collect();
                checks.push(LegCheck::new(
                    "interface_link",
                    CheckStatus::Fail,
                    format!("NIC '{ifn}' not found; available: {}", avail.join(", ")),
                ));
            }
        }
    } else {
        checks.push(LegCheck::new(
            "interface_link",
            CheckStatus::Skipped,
            "no NIC pin (interface/gateway unset) — leg follows the kernel default route",
        ));
    }

    // ── source_ip (gateway-mode / explicit source) ──────────────────
    if let Some(src) = &net.source {
        match ip_only(src) {
            Some(ip) => {
                let present = nics
                    .iter()
                    .any(|n| n.ipv4.iter().chain(n.ipv6.iter()).any(|a| nic_ip(a) == Some(ip)));
                if present {
                    let on = nic_for_ip(&nics, ip).unwrap_or_default();
                    checks.push(LegCheck::new(
                        "source_ip",
                        CheckStatus::Pass,
                        format!("source {ip} present on '{on}'"),
                    ));
                } else {
                    // For gateway-mode this is fatal (the `from <source>`
                    // policy rule can't match); for a plain UDP bind it's
                    // also fatal (bind would EADDRNOTAVAIL).
                    checks.push(LegCheck::new(
                        "source_ip",
                        CheckStatus::Fail,
                        format!("source {ip} is not present on any NIC — leg cannot bind it"),
                    ));
                }
            }
            None => checks.push(LegCheck::new(
                "source_ip",
                CheckStatus::Warn,
                format!("could not parse source '{src}'"),
            )),
        }
    }

    // ── bind + egress_route (zero-packet) ───────────────────────────
    let mut chosen_egress_interface: Option<String> = None;
    if is_listener {
        checks.push(LegCheck::new(
            "egress_route",
            CheckStatus::Skipped,
            "receiver/listener-role leg — no remote to egress to",
        ));
    } else if mode == "gateway" {
        // The per-leg policy route is only programmed at flow start, so a
        // route lookup while idle would follow the default route and
        // mislead. Usability for gateway-mode is proven by source_ip +
        // first_hop; live egress shows on the Traffic-Shaping card.
        checks.push(LegCheck::new(
            "egress_route",
            CheckStatus::Skipped,
            "gateway-mode policy route is programmed at flow start; verified live on the flow's Traffic-Shaping telemetry",
        ));
    } else {
        match probe_egress(&net, &nics) {
            Ok(probe) => {
                chosen_egress_interface = probe.chosen_interface.clone();
                if let Some(e) = &probe.strict_error {
                    checks.push(LegCheck::new(
                        "bind",
                        CheckStatus::Warn,
                        format!(
                            "strict NIC pin (SO_BINDTODEVICE) unavailable ({e}); leg will fall back to IP_UNICAST_IF source-bind"
                        ),
                    ));
                } else {
                    checks.push(LegCheck::new(
                        "bind",
                        CheckStatus::Pass,
                        "socket bound as the leg will bind".to_string(),
                    ));
                }

                match (&net.interface, &probe.chosen_interface) {
                    (Some(want), Some(got)) if want == got => checks.push(LegCheck::new(
                        "egress_route",
                        CheckStatus::Pass,
                        format!("egresses '{got}' (source {}) — no default-route collapse", probe.local_ip),
                    )),
                    (Some(want), Some(got)) => checks.push(LegCheck::new(
                        "egress_route",
                        CheckStatus::Fail,
                        format!(
                            "leg would egress '{got}' (source {}), NOT the pinned '{want}' — the leg collapses onto the default route (cosmetic bond)",
                            probe.local_ip
                        ),
                    )),
                    (Some(want), None) => checks.push(LegCheck::new(
                        "egress_route",
                        CheckStatus::Warn,
                        format!(
                            "pinned '{want}' but could not map the chosen source {} to a NIC",
                            probe.local_ip
                        ),
                    )),
                    (None, got) => checks.push(LegCheck::new(
                        "egress_route",
                        CheckStatus::Pass,
                        format!(
                            "default route egresses {} (source {})",
                            got.clone().unwrap_or_else(|| "?".into()),
                            probe.local_ip
                        ),
                    )),
                }
            }
            Err(e) => checks.push(LegCheck::new("bind", CheckStatus::Fail, format!("bind/route probe failed: {e}"))),
        }
    }

    // ── first_hop (gateway-mode) ────────────────────────────────────
    if let Some(gw) = &net.gateway {
        let (status, detail) = neighbor_reachable(gw);
        checks.push(LegCheck::new("first_hop", status, detail));
    }

    let ok = !checks.iter().any(|c| c.status == CheckStatus::Fail);
    let note = if leg_live {
        "Leg is currently carrying live traffic — usability checks are zero-impact; live capacity/loss is on the flow's Traffic-Shaping card. An active capacity probe is withheld while live to protect the feed.".to_string()
    } else {
        "Usability only (zero packets sent). To measure throughput, run the active capacity probe (Phase 2) on this idle leg.".to_string()
    };

    LegTestReport {
        ok,
        transport: net.transport.to_string(),
        mode: mode.to_string(),
        interface: net.interface.clone(),
        source_ip: net.source.as_deref().and_then(ip_only).map(|ip| ip.to_string()),
        remote: net.remote.clone(),
        chosen_egress_interface,
        mtu,
        link_speed_mbps,
        leg_live,
        checks,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::{BondPathTransportConfig, BondQuicRole, BondRistRole};

    #[test]
    fn ip_only_strips_port_and_prefix() {
        assert_eq!(ip_only("192.168.1.2:7400"), "192.168.1.2".parse().ok());
        assert_eq!(ip_only("192.168.1.2/24"), "192.168.1.2".parse().ok());
        assert_eq!(ip_only("10.0.0.1"), "10.0.0.1".parse().ok());
        assert_eq!(ip_only("not-an-ip"), None);
    }

    #[test]
    fn normalize_udp_gateway_mode() {
        let t = BondPathTransportConfig::Udp {
            bind: None,
            remote: Some("203.0.113.7:7400".into()),
            interface: Some("eno4".into()),
            gateway: Some("192.168.1.1".into()),
            source: Some("192.168.1.50/24".into()),
        };
        let n = normalize(&t);
        assert_eq!(n.transport, "udp");
        assert_eq!(n.interface.as_deref(), Some("eno4"));
        assert_eq!(n.gateway.as_deref(), Some("192.168.1.1"));
        assert_eq!(n.source.as_deref(), Some("192.168.1.50/24"));
        assert_eq!(n.remote.as_deref(), Some("203.0.113.7:7400"));
    }

    #[test]
    fn quic_server_role_is_listener() {
        let t = BondPathTransportConfig::Quic {
            role: BondQuicRole::Server,
            addr: "0.0.0.0:7400".into(),
            server_name: String::new(),
            tls: crate::config::models::BondQuicTls::SelfSigned,
            bind: None,
            interface: None,
        };
        let n = normalize(&t);
        assert!(n.remote.is_none(), "server role has no remote to dial");
    }

    #[test]
    fn rist_receiver_normalizes_to_listener_when_no_remote() {
        let t = BondPathTransportConfig::Rist {
            role: BondRistRole::Receiver,
            remote: None,
            local_bind: Some("0.0.0.0:7400".into()),
            buffer_ms: None,
        };
        let n = normalize(&t);
        assert_eq!(n.transport, "rist");
        assert!(n.remote.is_none());
    }

    #[test]
    fn missing_nic_fails_interface_link() {
        let t = BondPathTransportConfig::Udp {
            bind: None,
            remote: Some("203.0.113.7:7400".into()),
            interface: Some("definitely-not-a-real-nic-xyz".into()),
            gateway: None,
            source: None,
        };
        let report = test_leg(&t, false);
        let link = report
            .checks
            .iter()
            .find(|c| c.name == "interface_link")
            .expect("interface_link check present");
        assert_eq!(link.status, CheckStatus::Fail);
        assert!(!report.ok);
    }

    #[test]
    fn listener_leg_skips_egress() {
        let t = BondPathTransportConfig::Rist {
            role: BondRistRole::Receiver,
            remote: None,
            local_bind: Some("0.0.0.0:7400".into()),
            buffer_ms: None,
        };
        let report = test_leg(&t, false);
        assert_eq!(report.mode, "listener");
        let egress = report.checks.iter().find(|c| c.name == "egress_route").unwrap();
        assert_eq!(egress.status, CheckStatus::Skipped);
    }
}
