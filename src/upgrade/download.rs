// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Manifest + tarball fetcher for the upgrade pipeline.
//!
//! Two entry points:
//!
//! * [`fetch_text`] — small, in-memory fetch for `manifest.json` and the
//!   Sigstore bundle. Hard-capped at [`MAX_MANIFEST_BYTES`] so a
//!   compromised origin cannot exhaust the edge's RAM.
//! * [`fetch_with_sha256`] — streams the response body through
//!   `sha2::Sha256`, accumulates into an in-memory `Vec<u8>` (capped at
//!   [`MAX_TARBALL_BYTES`]), and returns only when the final digest
//!   matches the expected hex string. A mismatch returns
//!   `error_code = upgrade_checksum_mismatch` upstream.
//!
//! Both honour the host whitelist via [`super::manifest::validate_url_host`]
//! before any network traffic. Both retry transient errors (DNS, TCP
//! reset, 5xx) with exponential backoff capped at three attempts.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use super::manifest::validate_url_host;

/// Hard cap for `manifest.json` + bundle. The real files are <2 KiB; the
/// 1 MiB ceiling is paranoid headroom against a compromised origin
/// returning a multi-GB body to exhaust RAM.
pub const MAX_MANIFEST_BYTES: usize = 1 * 1024 * 1024;
/// Hard cap for the release tarball. Today's largest variant
/// (`-x86_64-linux-full`) is ~120 MB; cap at 512 MiB so future
/// growth (more codec backends, debuginfo) is fine but a runaway
/// download is not.
pub const MAX_TARBALL_BYTES: usize = 512 * 1024 * 1024;

const MAX_ATTEMPTS: u32 = 3;

/// Fetch a small text/JSON resource, hard-capped at
/// [`MAX_MANIFEST_BYTES`]. Used for `manifest.json` and the
/// Sigstore bundle.
pub async fn fetch_text(url: &str, timeout: Duration) -> Result<String> {
    validate_url_host(url)?;
    let client = build_client(timeout)?;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    last_err = Some(anyhow!("HTTP {status} from {url}"));
                    if !is_retryable_status(status) {
                        break;
                    }
                } else {
                    let bytes = resp
                        .bytes()
                        .await
                        .with_context(|| format!("failed to read body from {url}"))?;
                    if bytes.len() > MAX_MANIFEST_BYTES {
                        bail!(
                            "{url} returned {} bytes; cap is {MAX_MANIFEST_BYTES}",
                            bytes.len()
                        );
                    }
                    return Ok(String::from_utf8(bytes.to_vec())
                        .with_context(|| format!("body from {url} is not valid UTF-8"))?);
                }
            }
            Err(e) => {
                last_err = Some(anyhow!("network error fetching {url}: {e}"));
            }
        }
        backoff(attempt).await;
    }
    Err(last_err.unwrap_or_else(|| anyhow!("fetch_text exhausted retries")))
}

/// Stream-download a binary resource, computing SHA-256 incrementally.
/// Returns the bytes only when the digest matches `expected_sha256_hex`.
pub async fn fetch_with_sha256(
    url: &str,
    expected_sha256_hex: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    validate_url_host(url)?;
    if expected_sha256_hex.len() != 64
        || !expected_sha256_hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!("expected SHA-256 {expected_sha256_hex:?} is not 64 hex chars");
    }

    let client = build_client(timeout)?;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..MAX_ATTEMPTS {
        let mut hasher = Sha256::new();
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024 * 1024);
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    last_err = Some(anyhow!("HTTP {status} from {url}"));
                    if !is_retryable_status(status) {
                        break;
                    }
                    backoff(attempt).await;
                    continue;
                }
                if let Some(len) = resp.content_length() {
                    if (len as usize) > MAX_TARBALL_BYTES {
                        bail!(
                            "{url} content-length {len} exceeds tarball cap {MAX_TARBALL_BYTES}"
                        );
                    }
                }
                let mut stream = resp.bytes_stream();
                use futures_util::StreamExt as _;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk
                        .with_context(|| format!("failed to read chunk from {url}"))?;
                    if buf.len() + chunk.len() > MAX_TARBALL_BYTES {
                        bail!(
                            "{url} body exceeds tarball cap {MAX_TARBALL_BYTES} mid-stream"
                        );
                    }
                    hasher.update(&chunk);
                    buf.extend_from_slice(&chunk);
                }
                let digest = hasher.finalize();
                let got_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                if !got_hex.eq_ignore_ascii_case(expected_sha256_hex) {
                    bail!(
                        "checksum mismatch for {url}: expected SHA-256 {expected_sha256_hex}, \
                         got {got_hex} (size {} bytes)",
                        buf.len()
                    );
                }
                return Ok(buf);
            }
            Err(e) => {
                last_err = Some(anyhow!("network error fetching {url}: {e}"));
                backoff(attempt).await;
                continue;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("fetch_with_sha256 exhausted retries")))
}

fn build_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        // We rely on the system rustls roots (already linked for the
        // manager WSS path). `https_only` rejects redirects that
        // accidentally drop to plaintext.
        .https_only(true)
        .user_agent(format!(
            "bilbycast-edge/{version} (upgrade-fetcher; +https://bilbycast.com)",
            version = env!("CARGO_PKG_VERSION")
        ))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            // GitHub's release CDN redirects to objects.githubusercontent.com;
            // both hosts are in the whitelist, so we accept up to 5 hops.
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else {
                let url = attempt.url();
                if url.scheme() != "https" {
                    return attempt.error("redirect must stay on https");
                }
                let host_ok = url
                    .host_str()
                    .map(|h| super::manifest::ALLOWED_URL_HOSTS.iter().any(|a| h == *a))
                    .unwrap_or(false);
                if host_ok {
                    attempt.follow()
                } else {
                    attempt.error("redirect to disallowed host")
                }
            }
        }))
        .build()
        .context("failed to build reqwest::Client for upgrade fetch")
}

fn is_retryable_status(s: reqwest::StatusCode) -> bool {
    s.is_server_error() || s.as_u16() == 429 || s.as_u16() == 408
}

async fn backoff(attempt: u32) {
    // 250ms, 500ms, 1s — keeps the worst case under 2s for three
    // attempts, well inside the manager's 10s command-ack budget.
    let ms = 250u64.checked_shl(attempt).unwrap_or(2000);
    tokio::time::sleep(Duration::from_millis(ms.min(2000))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_bad_host() {
        let err = fetch_text("https://evil.com/foo", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not in the upgrade host whitelist"));
    }

    #[tokio::test]
    async fn rejects_bad_sha_format() {
        let err = fetch_with_sha256(
            "https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.0.0/x.tar.gz",
            "not-hex",
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("64 hex chars"));
    }
}
