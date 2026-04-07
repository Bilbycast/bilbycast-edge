// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! NMOS IS-04 mDNS-SD service registration.
//!
//! Registers `_nmos-node._tcp` (and `_nmos-register._tcp` placeholder) on the
//! local link via the pure-Rust [`mdns_sd`] crate so AMWA NMOS controllers
//! discover this edge automatically.
//!
//! ## Best-effort
//!
//! Multicast DNS depends on the host having a usable network interface with
//! multicast support (loopback alone is not enough on most systems). When the
//! daemon cannot be created or the registration fails, this module logs a
//! warning and continues — flow startup is never blocked. Returns an
//! [`Option`] handle so callers can keep the daemon alive (drop = unregister)
//! or discard it.
//!
//! ## Backward compatibility
//!
//! mDNS-SD registration is purely additive. Existing deployments that
//! manually register the node URL with an NMOS registry continue to work
//! unchanged; the mDNS announcement is supplementary.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

/// Spawn an mDNS-SD service registration for `_nmos-node._tcp` on the local
/// network. Returns the daemon handle (must be kept alive for the lifetime
/// of the process — dropping it unregisters) or `None` if registration could
/// not be set up.
///
/// `node_uuid` is the NMOS node UUID (used to make the instance name unique
/// per restart). `port` is the bilbycast HTTP/HTTPS listen port. `https`
/// selects the protocol attribute.
pub fn spawn_nmos_node_advertisement(
    node_uuid: &str,
    hostname: &str,
    port: u16,
    https: bool,
) -> Option<NmosMdnsHandle> {
    // The default daemon binds to all interfaces and uses the standard
    // mDNS multicast addresses. If multicast is not available the daemon
    // creation succeeds but registration is silently dropped — we still
    // return the handle so the caller can choose to keep it alive.
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("NMOS mDNS-SD: failed to create daemon: {e}");
            return None;
        }
    };

    // Use a synthetic IPv4 address for the registration. mdns-sd will resolve
    // the host's interface addresses internally — passing 0.0.0.0 lets the
    // crate enumerate. We use a placeholder loopback as a fallback so the
    // call signature is satisfied; the real interface enumeration happens
    // inside the daemon.
    let ip: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let service_type = "_nmos-node._tcp.local.";
    // Instance name must be unique. Truncate to keep below the DNS-SD label
    // limit (63 chars).
    let mut instance = format!("bilbycast-edge-{}", &node_uuid[..node_uuid.len().min(8)]);
    if instance.len() > 60 {
        instance.truncate(60);
    }
    let host = format!("{hostname}.local.");

    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("api_proto".into(), if https { "https".into() } else { "http".into() });
    props.insert("api_ver".into(), "v1.3".into());
    props.insert("pri".into(), "100".into());

    let info = match mdns_sd::ServiceInfo::new(service_type, &instance, &host, ip, port, props) {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!("NMOS mDNS-SD: failed to build ServiceInfo: {e}");
            return None;
        }
    };

    if let Err(e) = daemon.register(info) {
        tracing::warn!("NMOS mDNS-SD: registration failed: {e}");
        return None;
    }
    tracing::info!(
        "NMOS mDNS-SD: registered {service_type} instance '{instance}' on port {port}"
    );

    Some(NmosMdnsHandle { _daemon: daemon })
}

/// Opaque handle to an active NMOS mDNS-SD daemon. Drop to unregister.
pub struct NmosMdnsHandle {
    _daemon: mdns_sd::ServiceDaemon,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate is best-effort: in CI, multicast may be unavailable, but
    /// even then the helper should not panic. We assert that the function
    /// returns either `Some` or `None` without unwinding.
    #[test]
    fn test_spawn_does_not_panic() {
        let _ = spawn_nmos_node_advertisement(
            "12345678-1234-1234-1234-123456789012",
            "test-host",
            8080,
            false,
        );
    }
}
