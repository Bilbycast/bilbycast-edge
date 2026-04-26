// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Bracket-aware URL parser for the hand-rolled HTTP PUT client used by
//! `output_hls`. The previous implementation split host/port on `:` without
//! bracket-awareness, which mis-parsed IPv6 literals (`http://[::1]:8080/...`)
//! into `host = "[:"` plus a broken port. Delegating to the `url` crate gets
//! us correct handling of bracketed IPv6 hosts, default-port resolution, and
//! query-string preservation, while still returning a `connect_target` that
//! re-brackets IPv6 literals so `TcpStream::connect` accepts it directly.
//!
//! `output_hls` is the only consumer today. The RTMP client uses `url::Url`
//! inline because there's no second caller for an `rtmp://`-shaped helper.

use anyhow::{Context, Result};
use url::Url;

/// Parsed target for an HTTP/HTTPS PUT request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTarget {
    /// Lowercased scheme (`"http"` or `"https"`).
    pub scheme: String,
    /// Host as a string, *without* brackets for IPv6 literals (suitable for the `Host:` header).
    pub host: String,
    /// Resolved port (explicit if present, else 80 / 443).
    pub port: u16,
    /// Path + query (e.g. `/segment.ts` or `/segment.ts?token=abc`); always begins with `/`.
    pub path_and_query: String,
    /// `host:port` string suitable for `TcpStream::connect`, with brackets re-added for IPv6.
    pub connect_target: String,
}

/// Parse an `http://` or `https://` URL for the minimal HTTP PUT client.
///
/// Accepts IPv6 literals in the standard bracketed form (`http://[::1]:8080/...`).
/// Returns an `HttpTarget` containing the host without brackets (for the `Host:`
/// header) and a `connect_target` string with brackets re-added when the host
/// is an IPv6 literal (so `TcpStream::connect` accepts it as-is).
pub fn parse_http_target(url: &str) -> Result<HttpTarget> {
    let parsed = Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        anyhow::bail!("unsupported URL scheme '{scheme}' (expected http or https) in: {url}");
    }
    let (host, is_ipv6) = match parsed
        .host()
        .ok_or_else(|| anyhow::anyhow!("URL has no host: {url}"))?
    {
        url::Host::Domain(d) => (d.to_string(), false),
        url::Host::Ipv4(ip) => (ip.to_string(), false),
        url::Host::Ipv6(ip) => (ip.to_string(), true),
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("URL has no resolvable port: {url}"))?;
    let path = if parsed.path().is_empty() {
        "/".to_string()
    } else {
        parsed.path().to_string()
    };
    let path_and_query = match parsed.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let connect_target = if is_ipv6 {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Ok(HttpTarget {
        scheme,
        host,
        port,
        path_and_query,
        connect_target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_default_port() {
        let t = parse_http_target("http://example.com/foo").unwrap();
        assert_eq!(t.scheme, "http");
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 80);
        assert_eq!(t.path_and_query, "/foo");
        assert_eq!(t.connect_target, "example.com:80");
    }

    #[test]
    fn https_default_port() {
        let t = parse_http_target("https://example.com/foo").unwrap();
        assert_eq!(t.port, 443);
        assert_eq!(t.connect_target, "example.com:443");
    }

    #[test]
    fn http_explicit_port() {
        let t = parse_http_target("http://1.2.3.4:8080/foo").unwrap();
        assert_eq!(t.host, "1.2.3.4");
        assert_eq!(t.port, 8080);
        assert_eq!(t.connect_target, "1.2.3.4:8080");
    }

    #[test]
    fn ipv6_loopback_with_port() {
        let t = parse_http_target("http://[::1]:8080/foo").unwrap();
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 8080);
        assert_eq!(t.path_and_query, "/foo");
        assert_eq!(t.connect_target, "[::1]:8080");
    }

    #[test]
    fn ipv6_global_default_port() {
        let t = parse_http_target("http://[2001:db8::1]/foo").unwrap();
        assert_eq!(t.host, "2001:db8::1");
        assert_eq!(t.port, 80);
        assert_eq!(t.connect_target, "[2001:db8::1]:80");
    }

    #[test]
    fn ipv6_no_path_default_port() {
        let t = parse_http_target("http://[::1]/").unwrap();
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 80);
        assert_eq!(t.path_and_query, "/");
        assert_eq!(t.connect_target, "[::1]:80");
    }

    #[test]
    fn query_string_preserved() {
        let t = parse_http_target("http://example.com/seg.ts?token=abc&n=1").unwrap();
        assert_eq!(t.path_and_query, "/seg.ts?token=abc&n=1");
    }

    #[test]
    fn rejects_unbracketed_ipv6() {
        // url::Url::parse rejects an unbracketed v6 host because the colons
        // confuse port parsing. Surface the error rather than silently mis-parsing.
        assert!(parse_http_target("http://::1/foo").is_err());
    }

    #[test]
    fn rejects_malformed_bracket() {
        assert!(parse_http_target("http://[::1/foo").is_err());
    }

    #[test]
    fn rejects_unsupported_scheme() {
        let err = parse_http_target("ftp://example.com/foo")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported URL scheme"), "unexpected: {err}");
    }
}
