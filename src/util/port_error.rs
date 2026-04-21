// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Port binding error helpers.
//!
//! Provides utilities to detect and annotate "address already in use" errors
//! so that bind failures produce clear, actionable messages for operators.

use std::io::ErrorKind;

/// Marker error injected into an `anyhow::Error` chain by callers that
/// detected `EADDRINUSE` from a non-`io::Error` source (e.g. the libsrt FFI
/// returns string errors). The chain walker [`anyhow_is_addr_in_use`] picks
/// this up the same way it picks up a raw `io::Error::AddrInUse`, so every
/// runtime bind site can ask one question to decide whether to emit a
/// `port_conflict` event.
#[derive(Debug)]
pub struct PortInUse;

impl std::fmt::Display for PortInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("address already in use")
    }
}

impl std::error::Error for PortInUse {}

/// Returns `true` when the IO error is `EADDRINUSE` / `AddrInUse`.
pub fn is_addr_in_use(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::AddrInUse
}

/// Walks an [`anyhow::Error`] chain looking for either a `std::io::Error`
/// with `AddrInUse` or a [`PortInUse`] marker injected by FFI-style callers.
/// Useful when the bind call returns `anyhow::Error` rather than a raw
/// `std::io::Error` (e.g. the SRT library).
pub fn anyhow_is_addr_in_use(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        if cause.downcast_ref::<PortInUse>().is_some() {
            return true;
        }
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io_err| io_err.kind() == ErrorKind::AddrInUse)
            .unwrap_or(false)
    })
}

/// Wraps a bind `std::io::Error` with a human-readable message.
///
/// For `EADDRINUSE` the message explicitly says "address already in use" and
/// suggests checking for other services on the port. For other IO errors the
/// original error is wrapped with the component name and address.
///
/// The original `std::io::Error` is preserved as the underlying cause via
/// `anyhow::Error::new` so [`anyhow_is_addr_in_use`] can still detect the
/// AddrInUse kind by walking the error chain — required for higher-level
/// callers that want to emit a structured `port_conflict` event instead of
/// the generic bind-failure path.
pub fn annotate_bind_error(
    e: std::io::Error,
    addr: impl std::fmt::Display,
    component: &str,
) -> anyhow::Error {
    if is_addr_in_use(&e) {
        // Preserve the original io::Error in the chain so
        // `anyhow_is_addr_in_use()` works for higher-level callers, but keep
        // the human-readable top-line message stable so existing log strings
        // and tests don't drift.
        anyhow::Error::new(e).context(format!(
            "Port conflict: {component} could not bind to {addr} \
             — address already in use. \
             Check whether another service or edge component is using this port."
        ))
    } else {
        let inline = format!("{component} failed to bind to {addr}: {e}");
        anyhow::Error::new(e).context(inline)
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

    /// Regression: `annotate_bind_error` must preserve the underlying
    /// `io::Error` in the chain so higher-level callers can still detect
    /// `EADDRINUSE` after the annotation — required by the runtime
    /// `port_conflict` event path which walks the chain instead of parsing
    /// the message string.
    #[test]
    fn annotate_preserves_addr_in_use_in_chain() {
        let e = std::io::Error::from(ErrorKind::AddrInUse);
        let annotated = annotate_bind_error(e, "0.0.0.0:9527", "SRT listener");
        assert!(anyhow_is_addr_in_use(&annotated));
        assert!(annotated.to_string().contains("Port conflict"));
    }
}
