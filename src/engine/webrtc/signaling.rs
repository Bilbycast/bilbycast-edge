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
/// Location header value for session management (DELETE to teardown).
pub async fn whip_post(
    url: &str,
    offer_sdp: &str,
    bearer_token: Option<&str>,
    tls: &TlsTrust,
) -> Result<(String, String)> {
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
        .unwrap_or(url)
        .to_string();

    let answer_sdp = response.text().await?;
    Ok((answer_sdp, resource_url))
}

/// Perform WHEP client signaling: POST SDP offer, receive SDP answer.
///
/// Identical to WHIP signaling (both use the same HTTP exchange pattern).
pub async fn whep_post(
    url: &str,
    offer_sdp: &str,
    bearer_token: Option<&str>,
    tls: &TlsTrust,
) -> Result<(String, String)> {
    // WHEP uses the same POST SDP offer → 201 SDP answer pattern as WHIP
    whip_post(url, offer_sdp, bearer_token, tls).await
}

/// Delete a WHIP/WHEP session resource.
///
/// Per RFC 9725, sending DELETE to the resource URL tears down the session.
/// Retained: spec-correct session teardown per RFC 9725, to be wired into session lifecycle.
#[allow(dead_code)]
pub async fn delete_session(
    resource_url: &str,
    bearer_token: Option<&str>,
    tls: &TlsTrust,
) -> Result<()> {
    let client = build_reqwest_client(tls).map_err(|e| anyhow::anyhow!(e))?;

    let mut request = client.delete(resource_url);

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
