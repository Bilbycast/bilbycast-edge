// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Network interface enumeration for `HealthPayload.network_interfaces`.
//!
//! Cross-platform fields (name, IPv4/IPv6, MAC, MTU, loopback) come from
//! [`if_addrs::get_if_addrs`]. Linux-only fields (link speed, operstate)
//! come from `/sys/class/net/<name>/{speed,operstate}` and fall back to
//! `None` on macOS / Windows / containerised environments where sysfs
//! isn't readable.
//!
//! Called once per health tick (~15 s) — not on the data path.
//!
//! Wire shape mirrors `manager_core::models::ws_protocol::NetworkInterfaceInfo`
//! exactly. The edge crate is standalone (no manager dep) so the struct
//! is duplicated here; field names + serde defaults must stay in sync
//! with the manager-side definition.

use serde::Serialize;
use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize)]
pub struct NetworkInterfaceInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_speed_mbps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_up: Option<bool>,
    pub is_loopback: bool,
}

/// Enumerate every network interface on the host, grouping multiple
/// addresses on the same interface into a single entry.
///
/// Returns an empty `Vec` if `if_addrs::get_if_addrs()` fails (rare —
/// containerised environments without `/proc/net/dev` access). The
/// caller wraps this in `Option<Vec<_>>` so an empty result is sent
/// as `None` on the wire.
pub fn enumerate() -> Vec<NetworkInterfaceInfo> {
    let raw = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "if_addrs::get_if_addrs failed; reporting no network interfaces");
            return Vec::new();
        }
    };

    let mut by_name: HashMap<String, NetworkInterfaceInfo> = HashMap::new();
    for iface in raw {
        let entry = by_name
            .entry(iface.name.clone())
            .or_insert_with(|| NetworkInterfaceInfo {
                name: iface.name.clone(),
                mac: None,
                ipv4: Vec::new(),
                ipv6: Vec::new(),
                mtu: None,
                link_speed_mbps: None,
                is_up: None,
                is_loopback: iface.is_loopback(),
            });
        match iface.ip() {
            IpAddr::V4(v4) => entry.ipv4.push(v4.to_string()),
            IpAddr::V6(v6) => entry.ipv6.push(v6.to_string()),
        }
    }

    #[cfg(target_os = "linux")]
    for entry in by_name.values_mut() {
        let base = format!("/sys/class/net/{}", entry.name);
        entry.mac = read_sysfs_string(&format!("{base}/address"))
            .filter(|s| s != "00:00:00:00:00:00");
        entry.mtu = read_sysfs_string(&format!("{base}/mtu")).and_then(|s| s.parse().ok());
        entry.link_speed_mbps = read_sysfs_string(&format!("{base}/speed"))
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .map(|n| n as u64);
        entry.is_up = read_sysfs_string(&format!("{base}/operstate")).map(|s| s == "up");
    }

    let mut out: Vec<NetworkInterfaceInfo> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(target_os = "linux")]
fn read_sysfs_string(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_returns_at_least_loopback() {
        let ifaces = enumerate();
        assert!(!ifaces.is_empty(), "every host has at least a loopback interface");
        assert!(ifaces.iter().any(|i| i.is_loopback), "loopback should be present");
    }

    #[test]
    fn loopback_has_127_0_0_1() {
        let ifaces = enumerate();
        let lo = ifaces.iter().find(|i| i.is_loopback).expect("loopback present");
        assert!(
            lo.ipv4.iter().any(|a| a == "127.0.0.1"),
            "loopback should have 127.0.0.1, got {:?}",
            lo.ipv4
        );
    }

    #[test]
    fn entries_are_sorted_by_name() {
        let ifaces = enumerate();
        let names: Vec<_> = ifaces.iter().map(|i| i.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn json_shape_matches_protocol() {
        let info = NetworkInterfaceInfo {
            name: "eth0".into(),
            mac: Some("aa:bb:cc:dd:ee:ff".into()),
            ipv4: vec!["192.168.1.10".into()],
            ipv6: vec!["fe80::1".into()],
            mtu: Some(1500),
            link_speed_mbps: Some(1000),
            is_up: Some(true),
            is_loopback: false,
        };
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["name"], "eth0");
        assert_eq!(v["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(v["ipv4"][0], "192.168.1.10");
        assert_eq!(v["ipv6"][0], "fe80::1");
        assert_eq!(v["mtu"], 1500);
        assert_eq!(v["link_speed_mbps"], 1000);
        assert_eq!(v["is_up"], true);
        assert_eq!(v["is_loopback"], false);
    }

    #[test]
    fn omits_optional_fields_when_none() {
        let info = NetworkInterfaceInfo {
            name: "lo".into(),
            mac: None,
            ipv4: vec!["127.0.0.1".into()],
            ipv6: vec![],
            mtu: None,
            link_speed_mbps: None,
            is_up: None,
            is_loopback: true,
        };
        let v = serde_json::to_value(&info).unwrap();
        assert!(v.get("mac").is_none(), "mac should be omitted when None");
        assert!(v.get("mtu").is_none(), "mtu should be omitted when None");
        assert!(v.get("link_speed_mbps").is_none(), "link_speed_mbps should be omitted when None");
        assert!(v.get("is_up").is_none(), "is_up should be omitted when None");
    }
}
