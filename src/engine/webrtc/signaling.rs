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
/// best-effort and must not hold up the retry behind it (the WHIP output and
/// the WHEP input bound it tighter still). Used by the WHIP output and the
/// WHEP input to end every session they open.
///
/// `Ok` when the resource is gone: deleted (2xx), or not there to delete —
/// 404 or 410, which is what a server answers for a session it has already
/// ended (bilbycast-relay, for one, once the stream it served has closed),
/// so a release that finds nothing to release is a debug line, not a
/// warning. Any other status is an error, which the caller logs with the
/// resource. Every status used to be `Ok`, after a warning here that named
/// neither the resource nor the caller — a 404 included.
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
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    if matches!(status, reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE) {
        tracing::debug!("WHIP/WHEP DELETE {resource_url}: HTTP {}, the session is gone already", status.as_u16());
        return Ok(());
    }
    bail!("HTTP {}", status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::resolve_resource_url;
    use std::sync::{Arc, Mutex};

    /// Formatted `tracing` output, collected for a test to read.
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A DELETE answered 404 or 410 — a server that has already ended the
    /// session, as bilbycast-relay has once the stream it served closed —
    /// releases it as surely as a 200: `Ok`, and a debug line, not a
    /// warning. Any other refusal is an error the caller logs with the
    /// resource. Every status was `Ok`, after a warning naming neither the
    /// resource nor the caller — a 404 included.
    #[tokio::test]
    async fn a_session_already_gone_is_released() {
        use axum::http::StatusCode;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let app = axum::Router::new()
            .route("/whep/ok", axum::routing::delete(|| async { StatusCode::OK }))
            .route("/whep/gone", axum::routing::delete(|| async { StatusCode::NOT_FOUND }))
            .route("/whep/gone-for-good", axum::routing::delete(|| async { StatusCode::GONE }))
            .route("/whep/broken", axum::routing::delete(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let tls = crate::util::tls::TlsTrust { accept_self_signed: false, fingerprint: None };
        let delete = |path: &'static str| {
            let tls = tls.clone();
            async move { super::delete_session(&format!("http://{addr}/whep/{path}"), None, &tls).await }
        };

        // Logged at any level, read back; a first pass registers the
        // callsites against a subscriber (see the RTMP server's stream-key
        // log test: one first hit with none installed can cache "never").
        let captured = Capture(Arc::new(Mutex::new(Vec::new())));
        let subscriber = || {
            let writer = captured.clone();
            tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(move || writer.clone())
                    .with_max_level(tracing::Level::TRACE)
                    .with_ansi(false)
                    .finish(),
            )
        };
        drop({
            let warm = subscriber();
            for path in ["ok", "gone", "broken"] {
                let _ = delete(path).await;
            }
            warm
        });
        let _guard = subscriber();
        let mut logged = Vec::new();
        for path in ["ok", "gone", "gone-for-good", "broken"] {
            captured.0.lock().unwrap().clear();
            let result = delete(path).await;
            let text = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
            let ours: Vec<String> = text.lines().filter(|l| l.contains("WHIP/WHEP DELETE")).map(String::from).collect();
            logged.push((path, result.map_err(|e| e.to_string()), ours));
        }
        server.abort();

        assert!(matches!(&logged[0], (_, Ok(()), l) if l.is_empty()), "{:?}", logged[0]);
        for gone in &logged[1..3] {
            let (_, result, lines) = gone;
            assert_eq!(result, &Ok(()), "{gone:?}");
            assert!(lines.len() == 1 && lines[0].contains("DEBUG") && lines[0].contains("gone already"), "{gone:?}");
        }
        assert!(matches!(&logged[3], (_, Err(e), l) if e == "HTTP 500" && l.is_empty()), "{:?}", logged[3]);
    }

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
