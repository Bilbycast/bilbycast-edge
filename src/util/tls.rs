// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared TLS-trust helpers for outbound HTTPS / WSS clients.
//!
//! Three modes:
//! 1. **Standard** — webpki-roots system CAs, full chain validation.
//! 2. **Pinned** — webpki-roots + an additional SHA-256 fingerprint check
//!    on the leaf certificate (defense against compromised CAs).
//! 3. **Insecure** — accepts any certificate (self-signed development /
//!    testing only). The decision to use this mode is the caller's; this
//!    module only provides the verifier and never gates on env vars.
//!
//! Used by the manager WebSocket client and by the WHIP/WHEP signaling
//! HTTP client, both of which need the same TLS-trust policy shape.

use std::sync::Arc;

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
///
/// Returns colon-separated hex, lowercase — e.g. `"ab:cd:ef:01:23:..."`.
/// 32 bytes → 95 chars (32 * 2 hex digits + 31 colons).
pub fn compute_cert_fingerprint(cert_der: &[u8]) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(cert_der);
    hash.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Validate that a string looks like a SHA-256 cert fingerprint.
///
/// Accepts both colon-separated (`"ab:cd:..."`) and bare hex
/// (`"abcdef01..."`) on input. Returns the **canonicalised**
/// colon-separated lowercase form. Errors if the input is not 32 bytes
/// of hex.
pub fn canonicalise_fingerprint(s: &str) -> Result<String, String> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace() && *c != ':').collect();
    if cleaned.len() != 64 {
        return Err(format!(
            "expected 32-byte SHA-256 fingerprint (64 hex chars, got {})",
            cleaned.len()
        ));
    }
    if !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("fingerprint must be hex (0-9, a-f, A-F)".to_string());
    }
    let lower = cleaned.to_ascii_lowercase();
    let mut out = String::with_capacity(95);
    for (i, ch) in lower.chars().enumerate() {
        if i > 0 && i % 2 == 0 {
            out.push(':');
        }
        out.push(ch);
    }
    Ok(out)
}

/// Certificate verifier that accepts any certificate (for self-signed cert support).
///
/// **WARNING:** disables MITM protection. Use only when explicitly opted in.
#[derive(Debug)]
pub struct InsecureCertVerifier;

impl rustls::client::danger::ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Certificate verifier with fingerprint pinning.
///
/// Performs standard TLS certificate validation (CA chain, hostname),
/// then additionally checks that the server certificate's SHA-256
/// fingerprint matches the configured pin. Defends against compromised
/// CAs.
#[derive(Debug)]
pub struct PinnedCertVerifier {
    /// Expected SHA-256 fingerprint (colon-separated lowercase hex).
    pub expected_fingerprint: String,
    /// Standard webpki-based verifier for CA chain validation.
    pub inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // First, perform standard CA chain validation.
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;

        // Then check the fingerprint pin. Comparison is over canonical
        // lowercase hex on both sides — `canonicalise_fingerprint`
        // should have run on the configured value at config-load time.
        let actual = compute_cert_fingerprint(end_entity.as_ref());
        let expected = self.expected_fingerprint.to_ascii_lowercase();
        if actual != expected {
            tracing::error!(
                "Certificate fingerprint mismatch! Expected: {}, got: {}. \
                 This could indicate a man-in-the-middle attack or a legitimate \
                 certificate rotation. Update cert_fingerprint in the config \
                 if the certificate was intentionally changed.",
                expected,
                actual
            );
            return Err(rustls::Error::General(format!(
                "Certificate fingerprint mismatch: expected {expected}, got {actual}"
            )));
        }

        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// TLS trust policy for outbound HTTPS / WSS clients.
///
/// Precedence (highest first):
/// 1. `fingerprint` set → cert pinning (CA chain still validated;
///    leaf must match).
/// 2. `accept_self_signed` true → no validation at all.
/// 3. Neither → standard CA validation (webpki-roots).
#[derive(Debug, Clone, Default)]
pub struct TlsTrust {
    pub accept_self_signed: bool,
    pub fingerprint: Option<String>,
}

/// Build a `rustls::ClientConfig` honoring the trust policy. Returns
/// `None` for the "standard" path (caller should use the default
/// reqwest client).
pub fn build_rustls_config(trust: &TlsTrust) -> Result<Option<rustls::ClientConfig>, String> {
    if let Some(ref fp) = trust.fingerprint {
        let canonical = canonicalise_fingerprint(fp).map_err(|e| format!("cert_fingerprint: {e}"))?;
        let root_store = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(root_store))
            .build()
            .map_err(|e| format!("Failed to build certificate verifier: {e}"))?;
        let verifier = PinnedCertVerifier {
            expected_fingerprint: canonical,
            inner,
        };
        let cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        return Ok(Some(cfg));
    }
    if trust.accept_self_signed {
        let cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier))
            .with_no_client_auth();
        return Ok(Some(cfg));
    }
    Ok(None)
}

/// Build a `reqwest::Client` that honors the TLS trust policy.
///
/// Standard paths use reqwest's default config; pinned and insecure
/// paths use a custom rustls config via `use_preconfigured_tls`.
pub fn build_reqwest_client(trust: &TlsTrust) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder();
    let builder = match build_rustls_config(trust)? {
        Some(cfg) => builder.use_preconfigured_tls(cfg),
        None => builder,
    };
    builder.build().map_err(|e| format!("reqwest client build failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_format_round_trip() {
        let raw = "AB:cd:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab";
        let canon = canonicalise_fingerprint(raw).unwrap();
        assert_eq!(canon.len(), 95);
        assert!(canon.chars().all(|c| c.is_ascii_hexdigit() || c == ':'));
        assert_eq!(canon, canon.to_ascii_lowercase());
        // Bare hex should canonicalise the same way.
        let raw2 = "abcd012345678 9abcdef0123456789abcdef0123456789abcdef0123456789ab";
        let canon2 = canonicalise_fingerprint(raw2);
        assert!(canon2.is_ok(), "got error: {:?}", canon2.err());
    }

    #[test]
    fn fingerprint_format_rejects_invalid() {
        assert!(canonicalise_fingerprint("").is_err());
        assert!(canonicalise_fingerprint("not-hex").is_err());
        assert!(canonicalise_fingerprint("ab:cd").is_err()); // too short
        // 65 hex chars (one too many)
        assert!(canonicalise_fingerprint(&"a".repeat(65)).is_err());
    }

    #[test]
    fn trust_standard_is_default() {
        let t = TlsTrust::default();
        assert!(!t.accept_self_signed);
        assert!(t.fingerprint.is_none());
    }
}
