// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Port binding error helpers.
//!
//! Provides utilities to detect and annotate "address already in use" errors
//! so that bind failures produce clear, actionable messages for operators.

use std::io::ErrorKind;

/// Returns `true` when the IO error is `EADDRINUSE` / `AddrInUse`.
pub fn is_addr_in_use(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::AddrInUse
}

/// Walks an [`anyhow::Error`] chain looking for a `std::io::Error` with
/// `AddrInUse`.  Useful when the bind call returns `anyhow::Error` rather
/// than a raw `std::io::Error` (e.g. the SRT library).
pub fn anyhow_is_addr_in_use(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io_err| io_err.kind() == ErrorKind::AddrInUse)
            .unwrap_or(false)
    })
}

/// Wraps a bind `std::io::Error` with a human-readable message.
///
/// For `EADDRINUSE` the message explicitly says "address already in use" and
/// suggests checking for other services on the port.  For other IO errors the
/// original error is wrapped with the component name and address.
pub fn annotate_bind_error(
    e: std::io::Error,
    addr: impl std::fmt::Display,
    component: &str,
) -> anyhow::Error {
    if is_addr_in_use(&e) {
        anyhow::anyhow!(
            "Port conflict: {component} could not bind to {addr} \
             — address already in use. \
             Check whether another service or edge component is using this port."
        )
    } else {
        anyhow::anyhow!("{component} failed to bind to {addr}: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_addr_in_use() {
        let e = std::io::Error::from(ErrorKind::AddrInUse);
        assert!(is_addr_in_use(&e));
    }

    #[test]
    fn rejects_other_errors() {
        let e = std::io::Error::from(ErrorKind::ConnectionRefused);
        assert!(!is_addr_in_use(&e));
    }

    #[test]
    fn annotate_produces_port_conflict_message() {
        let e = std::io::Error::from(ErrorKind::AddrInUse);
        let annotated = annotate_bind_error(e, "0.0.0.0:8080", "TCP tunnel egress");
        let msg = annotated.to_string();
        assert!(msg.contains("Port conflict"));
        assert!(msg.contains("TCP tunnel egress"));
        assert!(msg.contains("0.0.0.0:8080"));
    }

    #[test]
    fn annotate_preserves_other_errors() {
        let e = std::io::Error::from(ErrorKind::PermissionDenied);
        let annotated = annotate_bind_error(e, "0.0.0.0:80", "Edge API server");
        let msg = annotated.to_string();
        assert!(!msg.contains("Port conflict"));
        assert!(msg.contains("Edge API server"));
        assert!(msg.contains("permission denied"));
    }

    #[test]
    fn anyhow_chain_walker_finds_addr_in_use() {
        let io_err = std::io::Error::from(ErrorKind::AddrInUse);
        let anyhow_err: anyhow::Error =
            anyhow::anyhow!(io_err).context("SRT listener bind failed");
        assert!(anyhow_is_addr_in_use(&anyhow_err));
    }

    #[test]
    fn anyhow_chain_walker_rejects_other() {
        let io_err = std::io::Error::from(ErrorKind::ConnectionRefused);
        let anyhow_err: anyhow::Error =
            anyhow::anyhow!(io_err).context("connection failed");
        assert!(!anyhow_is_addr_in_use(&anyhow_err));
    }
}
