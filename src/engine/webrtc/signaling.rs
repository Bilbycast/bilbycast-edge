// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! WHIP/WHEP HTTP signaling client.
//!
//! Implements the HTTP-based SDP exchange for WHIP (RFC 9725) and WHEP
//! (draft-ietf-wish-whep). Used when bilbycast-edge acts as a WHIP client
//! (output push) or WHEP client (input pull).
//!
//! TLS-trust policy is supplied by the caller via [`TlsTrust`]:
//! standard CA validation, self-signed acceptance (for testing), or
//! cert pinning (for production with a known leaf cert).

use anyhow::{Result, bail};

use crate::util::tls::{build_reqwest_client, TlsTrust};

/// Perform WHIP client signaling: POST SDP offer, receive SDP answer.
///
/// Per RFC 9725:
/// - POST to `whip_url` with Content-Type: application/sdp
/// - Include Authorization: Bearer header if token is provided
/// - Expect 201 Created with SDP answer body and Location header
///
/// Returns `(sdp_answer, resource_url)` where `resource_url` is the
/// session's resource for teardown (DELETE): the Location header resolved
/// against `url` ([`resolve_resource_url`]), `None` when the response
/// named none it could resolve.
pub async fn whip_post(
    url: &str,
    offer_sdp: &str,
    bearer_token: Option<&str>,
    tls: &TlsTrust,
) -> Result<(String, Option<String>)> {
    let client = build_reqwest_client(tls).map_err(|e| anyhow::anyhow!(e))?;

    let mut request = client
        .post(url)
        .header("Content-Type", "application/sdp")
        .body(offer_sdp.to_string());

    if let Some(token) = bearer_token {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    let response = request.send().await?;
    let status = response.status();

    if status != reqwest::StatusCode::CREATED {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "WHIP signaling failed: HTTP {} — {}",
            status.as_u16(),
            body
        );
    }

    let resource_url = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .and_then(|location| resolve_resource_url(url, location));

    let answer_sdp = response.text().await?;
    Ok((answer_sdp, resource_url))
}

/// The resource a 201's `Location` names, resolved against the endpoint
/// `url` the offer was POSTed to: `Location` is a URI reference (RFC 9110
/// §10.2.2), and the edge's own endpoints answer a relative one
/// (`/api/v1/flows/{id}/whip/{session}`). It used to be kept as it came,
/// which a DELETE cannot be sent to, and the endpoint URL itself stood in
/// when there was none — a DELETE of the endpoint, not of the session.
fn resolve_resource_url(url: &str, location: &str) -> Option<String> {
    reqwest::Url::parse(url).ok()?.join(location).ok().map(String::from)
}

/// Perform WHEP client signaling: POST SDP offer, receive SDP answer.
///
/// Identical to WHIP signaling (both use the same HTTP exchange pattern).
pub async fn whep_post(
    url: &str,
    offer_sdp: &str,
    bearer_token: Option<&str>,
    tls: &TlsTrust,
) -> Result<(String, Option<String>)> {
    // WHEP uses the same POST SDP offer → 201 SDP answer pattern as WHIP
    whip_post(url, offer_sdp, bearer_token, tls).await
}

/// Delete a WHIP/WHEP session resource.
///
/// Per RFC 9725, sending DELETE to the resource URL tears down the session.
/// Bounded to 10 s as a whole, not only its connect: a teardown is
/// best-effort and must not hold up the retry behind it. Used by the WHIP
/// output to drop a session it will not publish on (an answer without
/// H.264).
pub async fn delete_session(
    resource_url: &str,
    bearer_token: Option<&str>,
    tls: &TlsTrust,
) -> Result<()> {
    let client = build_reqwest_client(tls).map_err(|e| anyhow::anyhow!(e))?;

    let mut request = client
        .delete(resource_url)
        .timeout(std::time::Duration::from_secs(10));

    if let Some(token) = bearer_token {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    let response = request.send().await?;
    if !response.status().is_success() {
        tracing::warn!(
            "WHIP/WHEP DELETE returned HTTP {}",
            response.status().as_u16()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_resource_url;

    /// A relative `Location` — what the edge's own WHIP endpoint answers —
    /// resolves against the endpoint it was POSTed to; an absolute one is
    /// kept; one that cannot resolve is none.
    #[test]
    fn the_resource_url_resolves_against_the_endpoint() {
        let endpoint = "https://edge.example:8443/api/v1/flows/f1/whip";
        assert_eq!(
            resolve_resource_url(endpoint, "/api/v1/flows/f1/whip/s-1").as_deref(),
            Some("https://edge.example:8443/api/v1/flows/f1/whip/s-1"),
        );
        assert_eq!(
            resolve_resource_url(endpoint, "whip/s-2").as_deref(),
            Some("https://edge.example:8443/api/v1/flows/f1/whip/s-2"),
        );
        assert_eq!(
            resolve_resource_url(endpoint, "https://other.example/res/9").as_deref(),
            Some("https://other.example/res/9"),
        );
        assert_eq!(resolve_resource_url("not a url", "/res/1"), None);
    }
}
