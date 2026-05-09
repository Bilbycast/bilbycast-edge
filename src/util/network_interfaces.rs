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
use std::time::Instant;

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
    /// RX bandwidth in bits per second, derived from the delta between
    /// the previous and current `rx_bytes` sysfs counter on Linux.
    /// `None` on the first sample after start (no prior counter to diff
    /// against), on non-Linux hosts, or when the sysfs read failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_bps: Option<u64>,
    /// Cumulative kernel counters since boot for diagnostic surfacing.
    /// Operators care about absolute counts here, not deltas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_errors: Option<u64>,
}

/// Per-tick state needed to derive bandwidth rates from the kernel's
/// monotonic byte counters. Owned by the manager-client task and called
/// once per health tick — never on the data path.
#[derive(Default)]
pub struct NetworkSampler {
    last_samples: HashMap<String, LastSample>,
}

struct LastSample {
    rx_bytes: u64,
    tx_bytes: u64,
    at: Instant,
}

impl NetworkSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enumerate interfaces and, on Linux, populate rate / drop / error
    /// fields from `/sys/class/net/<iface>/statistics/`. The first call
    /// for any given interface returns `None` for `rx_bps` / `tx_bps`
    /// because there's no prior counter to diff against — the second
    /// tick onwards has valid rates.
    pub fn sample(&mut self) -> Vec<NetworkInterfaceInfo> {
        let mut out = enumerate();
        #[cfg(target_os = "linux")]
        {
            let now = Instant::now();
            let mut seen: std::collections::HashSet<String> =
                std::collections::HashSet::with_capacity(out.len());
            for entry in out.iter_mut() {
                seen.insert(entry.name.clone());
                let base = format!("/sys/class/net/{}/statistics", entry.name);
                let rx_bytes = read_sysfs_u64(&format!("{base}/rx_bytes"));
                let tx_bytes = read_sysfs_u64(&format!("{base}/tx_bytes"));
                entry.rx_dropped = read_sysfs_u64(&format!("{base}/rx_dropped"));
                entry.tx_dropped = read_sysfs_u64(&format!("{base}/tx_dropped"));
                entry.rx_errors = read_sysfs_u64(&format!("{base}/rx_errors"));
                entry.tx_errors = read_sysfs_u64(&format!("{base}/tx_errors"));

                if let (Some(rx), Some(tx)) = (rx_bytes, tx_bytes) {
                    if let Some(prev) = self.last_samples.get(&entry.name) {
                        let elapsed = now.saturating_duration_since(prev.at).as_secs_f64();
                        if elapsed > 0.0 {
                            let drx = rx.saturating_sub(prev.rx_bytes);
                            let dtx = tx.saturating_sub(prev.tx_bytes);
                            entry.rx_bps = Some(((drx as f64 / elapsed) * 8.0) as u64);
                            entry.tx_bps = Some(((dtx as f64 / elapsed) * 8.0) as u64);
                        }
                    }
                    self.last_samples.insert(
                        entry.name.clone(),
                        LastSample { rx_bytes: rx, tx_bytes: tx, at: now },
                    );
                }
            }
            // Forget interfaces that have disappeared so the map doesn't grow
            // unbounded across hot-add/remove cycles (USB NICs, container veth).
            self.last_samples.retain(|name, _| seen.contains(name));
        }
        out
    }
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
                rx_bps: None,
                tx_bps: None,
                rx_dropped: None,
                tx_dropped: None,
                rx_errors: None,
                tx_errors: None,
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

#[cfg(target_os = "linux")]
fn read_sysfs_u64(path: &str) -> Option<u64> {
    read_sysfs_string(path).and_then(|s| s.parse().ok())
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
            rx_bps: Some(124_300_000),
            tx_bps: Some(1_200_000),
            rx_dropped: Some(0),
            tx_dropped: Some(0),
            rx_errors: Some(0),
            tx_errors: Some(0),
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
        assert_eq!(v["rx_bps"], 124_300_000);
        assert_eq!(v["tx_bps"], 1_200_000);
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
            rx_bps: None,
            tx_bps: None,
            rx_dropped: None,
            tx_dropped: None,
            rx_errors: None,
            tx_errors: None,
        };
        let v = serde_json::to_value(&info).unwrap();
        assert!(v.get("mac").is_none(), "mac should be omitted when None");
        assert!(v.get("mtu").is_none(), "mtu should be omitted when None");
        assert!(v.get("link_speed_mbps").is_none(), "link_speed_mbps should be omitted when None");
        assert!(v.get("is_up").is_none(), "is_up should be omitted when None");
        assert!(v.get("rx_bps").is_none());
        assert!(v.get("tx_bps").is_none());
        assert!(v.get("rx_dropped").is_none());
        assert!(v.get("rx_errors").is_none());
    }

    #[test]
    fn sampler_first_call_has_no_rate() {
        let mut sampler = NetworkSampler::new();
        let snap = sampler.sample();
        // First call: no prior counters → rx_bps / tx_bps must be None
        // for every interface, regardless of OS.
        for entry in &snap {
            assert!(entry.rx_bps.is_none(), "first sample should have no rx_bps for {}", entry.name);
            assert!(entry.tx_bps.is_none(), "first sample should have no tx_bps for {}", entry.name);
        }
    }
}
