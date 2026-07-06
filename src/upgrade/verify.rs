// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Sigstore keyless-signature verification for upgrade manifests.
//!
//! The release workflow signs `manifest.json` with `cosign sign-blob
//! --bundle manifest.sig.bundle`. The bundle JSON carries:
//! * `base64Signature` — Ed25519/ECDSA signature over the manifest bytes,
//!   produced by an ephemeral keypair generated inside the workflow run.
//! * `cert` — PEM-encoded X.509 certificate issued by Sigstore's Fulcio
//!   CA. The cert's SAN extensions bind the ephemeral pubkey to the
//!   specific GitHub workflow run that produced it (issuer, repo, ref,
//!   workflow path).
//! * `rekorBundle` — Rekor inclusion proof + signed entry timestamp,
//!   binding the signing event to a specific public-log entry. Rekor
//!   is the public transparency log that makes any unauthorised release
//!   forensically detectable after the fact.
//!
//! Verification steps (all must succeed):
//!
//! 1. Parse the bundle JSON and extract the cert + signature + Rekor
//!    payload.
//! 2. Verify the leaf cert chains to (is signed by) a compiled-in/TUF
//!    Fulcio CA. This is the load-bearing anti-forgery check.
//! 3. Verify the cert's SAN URI matches one of the entries in
//!    [`super::trust::ALLOWED_SIGNERS`].
//! 4. Verify the Rekor entry: (a) the entry body binds this manifest's
//!    SHA-256 and this exact certificate; (b) the Signed Entry Timestamp
//!    verifies under a trusted Rekor public key; (c) the entry's
//!    `integratedTime` falls within the cert's validity window.
//! 5. Verify the signature is valid over the manifest bytes using the
//!    pubkey embedded in the cert.
//!
//! Implementation note: sigstore-rs 0.14 keeps its chain / Rekor
//! verifiers `pub(crate)`, so the checks above are performed in-tree
//! against the crate's public primitives (`SigstoreTrustRoot::{fulcio_certs,
//! rekor_keys}`, `CosignVerificationKey`). `verify_manifest_bundle` is the
//! single entry point and returns clear `error_code`-friendly errors so the
//! manager UI can render targeted failures. Any failure — including an
//! unreachable/malformed trust root — is fatal (fail-closed).

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use super::trust::{ref_matches, AllowedSigner};

/// Top-level entry point. Returns `Ok(())` only when **every** check
/// passes; on any failure the message string contains the specific
/// reason so the caller can map it to an `error_code`.
///
/// `manifest_bytes` is the **raw, byte-for-byte** content of
/// `manifest.json` as fetched from the GitHub release. The bundle binds
/// to a specific byte sequence; canonicalising or pretty-printing the
/// JSON before passing it here would break verification.
pub async fn verify_manifest_bundle(
    manifest_bytes: &[u8],
    bundle_bytes: &[u8],
    allowed_signers: &[AllowedSigner],
) -> Result<()> {
    if allowed_signers.is_empty() {
        bail!("identity allowlist is empty — refusing to verify any signature");
    }

    let bundle: CosignBundle = serde_json::from_slice(bundle_bytes)
        .context("failed to parse cosign Sigstore bundle JSON")?;

    // 1. Parse cert + check chain to Fulcio root.
    let cert_pem = bundle
        .cert
        .as_deref()
        .ok_or_else(|| anyhow!("bundle missing 'cert' (X.509 PEM)"))?;
    let signature_b64 = bundle
        .base64_signature
        .as_deref()
        .ok_or_else(|| anyhow!("bundle missing 'base64Signature'"))?;

    // 2. Chain the cert to a trusted Fulcio CA, check its validity window
    //    against the Rekor integrated time, and bind the Rekor entry to THIS
    //    manifest + certificate (plus verify the Rekor SET signature).
    sigstore_inner::verify_cert_and_rekor(
        cert_pem,
        manifest_bytes,
        bundle.rekor_bundle.as_ref(),
    )
    .await
    .context("Sigstore Fulcio + Rekor verification failed")?;

    // 3. SAN identity match against `allowed_signers`.
    let identity = sigstore_inner::extract_identity_claims(cert_pem)
        .context("could not extract identity claims from Fulcio cert")?;
    if !identity_matches_allowlist(&identity, allowed_signers) {
        bail!(
            "Sigstore cert identity {identity:?} is not in the upgrade ALLOWED_SIGNERS allowlist \
             — refusing to install (the binary may have been signed by an unauthorised workflow)"
        );
    }

    // 4. Verify signature over the raw manifest bytes.
    sigstore_inner::verify_blob_signature(cert_pem, signature_b64, manifest_bytes)
        .context("manifest signature does not verify under the cert's pubkey")?;

    Ok(())
}

/// Cosign-bundle JSON envelope. Field names follow cosign's output
/// (`cosign sign-blob --bundle <path>`).
#[derive(Debug, Deserialize)]
struct CosignBundle {
    #[serde(rename = "base64Signature")]
    base64_signature: Option<String>,
    cert: Option<String>,
    #[serde(rename = "rekorBundle")]
    rekor_bundle: Option<RekorBundle>,
}

/// Rekor inclusion proof payload from cosign's bundle. The
/// `SignedEntryTimestamp` is a Rekor-signed assertion that the entry
/// was included in the public transparency log at `Payload.integratedTime`.
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct RekorBundle {
    #[serde(rename = "SignedEntryTimestamp")]
    pub signed_entry_timestamp: String,
    #[serde(rename = "Payload")]
    pub payload: RekorPayload,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct RekorPayload {
    pub body: String,
    #[serde(rename = "integratedTime")]
    pub integrated_time: i64,
    #[serde(rename = "logIndex")]
    pub log_index: u64,
    #[serde(rename = "logID")]
    pub log_id: String,
}

/// Decoded identity claims from a Fulcio cert.
#[derive(Debug, Clone)]
pub(crate) struct CertIdentity {
    pub issuer: Option<String>,
    pub repo: Option<String>,
    pub workflow_uri: Option<String>,
    pub ref_claim: Option<String>,
}

fn identity_matches_allowlist(claims: &CertIdentity, allowed: &[AllowedSigner]) -> bool {
    allowed.iter().any(|s| {
        match (
            claims.issuer.as_deref(),
            claims.repo.as_deref(),
            claims.workflow_uri.as_deref(),
            claims.ref_claim.as_deref(),
        ) {
            (Some(iss), Some(repo), Some(wf), Some(rf)) => {
                iss == s.issuer
                    && repo == s.repo
                    && wf.starts_with(s.workflow)
                    && ref_matches(s.ref_pattern, rf)
            }
            _ => false,
        }
    })
}

/// Wrapper around the heavyweight `sigstore` crate calls. Isolated in
/// its own submodule so the higher-level pipeline above stays readable
/// even when the sigstore-rs API surface evolves.
///
/// **Implementation note**: the sigstore-rs crate has churned across
/// 0.x releases. We use a small set of stable primitives here:
/// * `sigstore::trust::sigstore::SigstoreTrustRoot` for the Fulcio root.
/// * X.509 parsing via `x509-cert` (which sigstore-rs re-exports).
/// * RustCrypto's `p256` / `ed25519-dalek` for signature verification.
///
/// Because Sigstore Fulcio always issues ECDSA-P256 certs and cosign's
/// blob signatures are also P-256 by default, we only need ECDSA-P256
/// verification here. If the upstream defaults ever change (Ed25519
/// long-term keys, P-384), the verifier returns a clear error rather
/// than silently accepting an unverified blob.
pub(crate) mod sigstore_inner {
    use anyhow::{anyhow, bail, Context, Result};
    use base64::Engine;

    use super::CertIdentity;

    /// Verify the cert chain to the compiled-in Fulcio root **and** the
    /// Rekor inclusion proof against the compiled-in Rekor pubkey.
    ///
    /// Both pieces are delegated to the `sigstore` crate's
    /// `SigstoreTrustRoot` so that root rotations land via routine
    /// `cargo update` without code changes.
    pub async fn verify_cert_and_rekor(
        cert_pem: &str,
        manifest_bytes: &[u8],
        rekor_bundle: Option<&super::RekorBundle>,
    ) -> Result<()> {
        use sigstore::trust::TrustRoot;

        // Build the trust root — loads the Fulcio CA certs + Rekor public
        // keys from the sigstore crate's TUF data (embedded snapshot,
        // refreshed from the Sigstore TUF repo). These are the roots of
        // trust; verification below is entirely against them.
        let trust_root = sigstore::trust::sigstore::SigstoreTrustRoot::new(None)
            .await
            .context("failed to load Sigstore trust root (Fulcio + Rekor)")?;
        let fulcio_certs = trust_root
            .fulcio_certs()
            .map_err(|e| anyhow!("Fulcio cert bundle unavailable: {e}"))?;
        let rekor_keys = trust_root
            .rekor_keys()
            .map_err(|e| anyhow!("Rekor public key unavailable: {e}"))?;

        // Rekor inclusion is mandatory — a bundle without it cannot be
        // transparency-verified, and its absence must never pass.
        let rekor = rekor_bundle
            .ok_or_else(|| anyhow!("rekor bundle missing — every release must be logged in Rekor"))?;

        let leaf_der = pem_to_der(cert_pem)?;

        // (a) Chain: the leaf MUST be signed by a trusted Fulcio CA. This is
        //     the load-bearing anti-forgery check — without it an attacker can
        //     self-sign a certificate carrying the allowed workflow identity
        //     and pass the SAN allowlist below.
        verify_cert_chain(&leaf_der, &fulcio_certs)
            .context("Fulcio certificate-chain verification failed")?;

        // (b) The signing cert's validity window must contain the Rekor
        //     integrated time (the cosign/Fulcio short-lived-cert model).
        verify_validity_window(&leaf_der, rekor.payload.integrated_time)
            .context("certificate validity window does not contain the Rekor integrated time")?;

        // (c) The Rekor entry body must bind THIS manifest hash AND THIS
        //     certificate — otherwise a valid entry for some other artifact
        //     could be replayed alongside a forged manifest.
        verify_rekor_body_binding(&rekor.payload.body, manifest_bytes, &leaf_der)
            .context("Rekor entry body does not bind this manifest and certificate")?;

        // (d) The Signed Entry Timestamp must verify under a trusted Rekor
        //     public key (proves Rekor actually logged this entry).
        verify_rekor_set(rekor, &rekor_keys)
            .context("Rekor SignedEntryTimestamp verification failed")?;

        Ok(())
    }

    /// Map an X.509 signature-algorithm OID to a sigstore signing scheme.
    fn scheme_for_oid(oid: &str) -> Result<sigstore::crypto::SigningScheme> {
        use sigstore::crypto::SigningScheme;
        match oid {
            "1.2.840.10045.4.3.2" => Ok(SigningScheme::ECDSA_P256_SHA256_ASN1), // ecdsa-with-SHA256
            "1.2.840.10045.4.3.3" => Ok(SigningScheme::ECDSA_P384_SHA384_ASN1), // ecdsa-with-SHA384
            other => bail!("unsupported certificate signature algorithm OID {other}"),
        }
    }

    /// Verify that `leaf_der` is signed by (i.e. chains to) one of the trusted
    /// Fulcio CA certificates. Only the true issuer's public key verifies the
    /// leaf's signature over its TBSCertificate, so trying every CA and
    /// requiring one success is a sound chain check for Fulcio's short chain.
    fn verify_cert_chain<C: AsRef<[u8]>>(leaf_der: &[u8], fulcio_certs: &[C]) -> Result<()> {
        use sigstore::crypto::{CosignVerificationKey, Signature};
        use x509_cert::der::{Decode, Encode};
        use x509_cert::Certificate;

        let leaf = Certificate::from_der(leaf_der)
            .map_err(|e| anyhow!("leaf X.509 DER parse error: {e}"))?;
        // The exact bytes Fulcio signed: the leaf's TBSCertificate DER.
        let tbs_der = leaf
            .tbs_certificate
            .to_der()
            .map_err(|e| anyhow!("re-encode leaf TBSCertificate: {e}"))?;
        let sig = leaf
            .signature
            .as_bytes()
            .ok_or_else(|| anyhow!("leaf signature is not octet-aligned"))?;
        let scheme = scheme_for_oid(&leaf.signature_algorithm.oid.to_string())?;

        for ca_der in fulcio_certs {
            let Ok(ca) = Certificate::from_der(ca_der.as_ref()) else {
                continue;
            };
            let Ok(spki_der) = ca.tbs_certificate.subject_public_key_info.to_der() else {
                continue;
            };
            let Ok(key) = CosignVerificationKey::from_der(&spki_der, &scheme) else {
                continue;
            };
            if key.verify_signature(Signature::Raw(sig), &tbs_der).is_ok() {
                return Ok(());
            }
        }
        bail!(
            "leaf certificate does not chain to any trusted Fulcio CA \
             (possible forged or self-signed certificate)"
        )
    }

    /// Require the Rekor `integrated_time` to fall within the leaf cert's
    /// `[notBefore, notAfter]` validity window.
    fn verify_validity_window(leaf_der: &[u8], integrated_time: i64) -> Result<()> {
        use x509_cert::der::Decode;
        use x509_cert::Certificate;
        let leaf = Certificate::from_der(leaf_der)
            .map_err(|e| anyhow!("leaf X.509 DER parse error: {e}"))?;
        let nb = leaf
            .tbs_certificate
            .validity
            .not_before
            .to_unix_duration()
            .as_secs() as i64;
        let na = leaf
            .tbs_certificate
            .validity
            .not_after
            .to_unix_duration()
            .as_secs() as i64;
        if integrated_time < nb || integrated_time > na {
            bail!("Rekor integratedTime {integrated_time} is outside certificate validity [{nb}, {na}]");
        }
        Ok(())
    }

    /// Verify the Rekor `hashedrekord` entry body binds the exact manifest we
    /// fetched (by SHA-256) and the exact certificate we verified.
    fn verify_rekor_body_binding(
        body_b64: &str,
        manifest_bytes: &[u8],
        leaf_der: &[u8],
    ) -> Result<()> {
        let body_bytes = base64::engine::general_purpose::STANDARD
            .decode(body_b64)
            .context("rekor body is not valid base64")?;
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).context("rekor body is not valid JSON")?;

        // spec.data.hash.value == hex(sha256(manifest))
        let entry_hash = body
            .pointer("/spec/data/hash/value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("rekor body missing spec.data.hash.value"))?;
        let want_hash = sha256_hex(manifest_bytes);
        if !entry_hash.eq_ignore_ascii_case(&want_hash) {
            bail!("rekor entry data hash {entry_hash} != manifest sha256 {want_hash}");
        }

        // spec.signature.publicKey.content == base64(PEM of the signing cert)
        let pk_b64 = body
            .pointer("/spec/signature/publicKey/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("rekor body missing spec.signature.publicKey.content"))?;
        let pk_pem = base64::engine::general_purpose::STANDARD
            .decode(pk_b64)
            .context("rekor body publicKey.content is not valid base64")?;
        let pk_pem = std::str::from_utf8(&pk_pem)
            .context("rekor body publicKey is not UTF-8 PEM")?;
        let entry_cert_der = pem_to_der(pk_pem)
            .context("rekor body publicKey PEM did not parse")?;
        if entry_cert_der != leaf_der {
            bail!("rekor entry certificate does not match the bundle's signing certificate");
        }
        Ok(())
    }

    /// Verify the Rekor Signed Entry Timestamp (an ECDSA-P256 signature over
    /// the canonical JSON of the log entry) against a trusted Rekor key.
    fn verify_rekor_set(
        rekor: &super::RekorBundle,
        rekor_keys: &std::collections::BTreeMap<String, &[u8]>,
    ) -> Result<()> {
        use sigstore::crypto::{CosignVerificationKey, Signature, SigningScheme};

        let set_sig = base64::engine::general_purpose::STANDARD
            .decode(&rekor.signed_entry_timestamp)
            .context("SignedEntryTimestamp is not valid base64")?;

        // The canonical JSON Rekor signs: keys sorted, no whitespace. The
        // string fields (`body` base64, `logID` hex) contain no characters
        // needing JSON escaping, so this reproduces the signed bytes exactly.
        let canonical = format!(
            "{{\"body\":\"{}\",\"integratedTime\":{},\"logID\":\"{}\",\"logIndex\":{}}}",
            rekor.payload.body,
            rekor.payload.integrated_time,
            rekor.payload.log_id,
            rekor.payload.log_index
        );

        for spki in rekor_keys.values().copied() {
            if let Ok(key) =
                CosignVerificationKey::from_der(spki, &SigningScheme::ECDSA_P256_SHA256_ASN1)
            {
                if key
                    .verify_signature(Signature::Raw(&set_sig), canonical.as_bytes())
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }
        bail!("SignedEntryTimestamp did not verify against any trusted Rekor public key")
    }

    /// Lowercase hex of SHA-256(`data`).
    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write;
        let digest = Sha256::digest(data);
        let mut out = String::with_capacity(64);
        for b in digest {
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// Verify the cosign blob signature over `blob` using the pubkey
    /// embedded in the leaf certificate. Cosign defaults to ECDSA-P256
    /// over the SHA-256 of the blob.
    pub fn verify_blob_signature(cert_pem: &str, signature_b64: &str, blob: &[u8]) -> Result<()> {
        let signature = base64::engine::general_purpose::STANDARD
            .decode(signature_b64)
            .context("base64Signature is not valid base64")?;
        let cert_der = pem_to_der(cert_pem).context("cert is not valid PEM")?;

        // Use sigstore-rs's signing-key abstraction to verify. The
        // `CosignVerificationKey::from_der` constructor accepts the
        // SubjectPublicKeyInfo (SPKI) bytes; we extract them from the
        // X.509 cert via x509-cert.
        use x509_cert::der::Decode;
        let cert = x509_cert::Certificate::from_der(&cert_der)
            .map_err(|e| anyhow!("X.509 DER parse error: {e}"))?;
        let spki = &cert.tbs_certificate.subject_public_key_info;
        let spki_der = x509_cert::der::Encode::to_der(spki)
            .map_err(|e| anyhow!("SPKI encode error: {e}"))?;

        use sigstore::crypto::CosignVerificationKey;
        let key = CosignVerificationKey::from_der(
            &spki_der,
            &sigstore::crypto::SigningScheme::ECDSA_P256_SHA256_ASN1,
        )
        .context("could not load ECDSA-P256 verification key from cert SPKI")?;
        key.verify_signature(sigstore::crypto::Signature::Raw(&signature), blob)
            .context("signature does not verify under cert SPKI")?;
        Ok(())
    }

    /// Pull the `oidc-issuer`, `source-repository`, `workflow-uri`, and
    /// `ref` claims out of a Fulcio cert's extensions / SAN.
    pub fn extract_identity_claims(cert_pem: &str) -> Result<CertIdentity> {
        use x509_cert::der::Decode;
        let der = pem_to_der(cert_pem)?;
        let cert = x509_cert::Certificate::from_der(&der)
            .map_err(|e| anyhow!("X.509 DER parse error: {e}"))?;

        let mut id = CertIdentity {
            issuer: None,
            repo: None,
            workflow_uri: None,
            ref_claim: None,
        };

        // Sigstore Fulcio extension OIDs (registered under 1.3.6.1.4.1.57264.1.x).
        const OID_ISSUER: &str = "1.3.6.1.4.1.57264.1.1";
        const OID_REPO: &str = "1.3.6.1.4.1.57264.1.5";
        const OID_REF: &str = "1.3.6.1.4.1.57264.1.6";
        // SAN URI carries the workflow + ref combined; we parse it below.

        if let Some(exts) = &cert.tbs_certificate.extensions {
            for e in exts {
                let oid = e.extn_id.to_string();
                let bytes = e.extn_value.as_bytes();
                match oid.as_str() {
                    OID_ISSUER => id.issuer = utf8_or_none(bytes),
                    OID_REPO => id.repo = utf8_or_none(bytes),
                    OID_REF => id.ref_claim = utf8_or_none(bytes),
                    _ => {}
                }
                // SAN is OID 2.5.29.17. Cosign's keyless flow puts the
                // workflow URI in the URI form of the SAN.
                if oid == "2.5.29.17" {
                    if let Some(s) = first_san_uri(bytes) {
                        id.workflow_uri = Some(s);
                    }
                }
            }
        }

        Ok(id)
    }

    fn utf8_or_none(bytes: &[u8]) -> Option<String> {
        // Fulcio extensions are typed `OCTET STRING` whose value is
        // itself an `IA5String`/`UTF8String`. Skip any leading DER tag/
        // length bytes by scanning to the first printable run.
        let s = String::from_utf8_lossy(bytes).into_owned();
        if s.is_empty() {
            None
        } else {
            // Trim leading non-printable bytes from the DER prefix.
            Some(s.trim_start_matches(|c: char| !c.is_ascii_graphic()).to_string())
        }
    }

    fn first_san_uri(bytes: &[u8]) -> Option<String> {
        // Quick-and-dirty extractor. Real DER parsing would walk the
        // SubjectAltName SEQUENCE, but cosign always emits a single URI
        // SAN so a string scan is robust enough — we check for the
        // `https://` substring and run from there to the first non-URI
        // byte.
        let s = String::from_utf8_lossy(bytes);
        let idx = s.find("https://")?;
        let rest = &s[idx..];
        let end = rest
            .find(|c: char| {
                !(c.is_ascii_graphic()
                    && c != '"'
                    && c != ' ')
            })
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }

    fn pem_to_der(cert_pem: &str) -> Result<Vec<u8>> {
        let trimmed = cert_pem.trim();
        let stripped = trimmed
            .strip_prefix("-----BEGIN CERTIFICATE-----")
            .and_then(|s| s.strip_suffix("-----END CERTIFICATE-----"))
            .ok_or_else(|| anyhow!("cert is not a single PEM CERTIFICATE block"))?;
        let cleaned: String = stripped.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .context("PEM body is not valid base64")
    }

    #[cfg(test)]
    mod chain_tests {
        use super::*;

        // Fixtures generated offline with OpenSSL (P-256, ecdsa-with-SHA256):
        //  * CA_PEM        — a self-signed test CA (stands in for a Fulcio CA).
        //  * LEAF_PEM      — a leaf issued *by* that CA.
        //  * OTHER_CA_PEM  — an unrelated self-signed CA.
        //  * FORGED_PEM    — an attacker-self-signed cert with the same subject
        //                    as LEAF but NOT issued by CA (the forgery the
        //                    chain check exists to stop).
        const CA_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBhjCCAS2gAwIBAgIURxdwF6sp5xqzl7rx8RKgWkvhh4YwCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwOVGVzdCBGdWxjaW8gQ0EwHhcNMjYwNzA2MDI1ODMxWhcNMzYw
NzAzMDI1ODMxWjAZMRcwFQYDVQQDDA5UZXN0IEZ1bGNpbyBDQTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABAHNMvfuUdk4y/VA74k2YhXbUpAPOiwiMD9Xq+rYriSf
cPcaZwy36MG6fVVZYMWOhl20RZx13PcUk4Tlm9kubP+jUzBRMB0GA1UdDgQWBBR7
YpMmucviS/rKNfvh5nQnYeULvDAfBgNVHSMEGDAWgBR7YpMmucviS/rKNfvh5nQn
YeULvDAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIF8NNUK0b+Ho
CwOeL99LrNZJHKE1F6FXjK7juqRZhs7tAiB4WxB/BosQF2/U3W6Mlm0P6hy2K1Nj
3sbZxuywV3xVSw==
-----END CERTIFICATE-----"#;

        const LEAF_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBbDCCARKgAwIBAgIUack56LkOqBV3bH7TCvFYEhf+oHIwCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwOVGVzdCBGdWxjaW8gQ0EwHhcNMjYwNzA2MDI1ODMxWhcNMjcw
NzA2MDI1ODMxWjAPMQ0wCwYDVQQDDARsZWFmMFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEAXclNg9sf5hNuqiqxe4voL5sXVi8pccpprcDpueQIvp4j4KHv5wza7LT
ykGo4FRVpx6Zggk0rN/Sca7siAFkv6NCMEAwHQYDVR0OBBYEFNCUvLZDwM0sxQYH
toVAKni9r/t+MB8GA1UdIwQYMBaAFHtikya5y+JL+so1++HmdCdh5Qu8MAoGCCqG
SM49BAMCA0gAMEUCIGoE4eNGJB2BSBR6+IFNlakoKcEPB66LyXOnGWX/EDonAiEA
zXkAMcVKpLiuTmuWzC/vuzBmQpdkQ6UW+fVqx7oOkQQ=
-----END CERTIFICATE-----"#;

        const OTHER_CA_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBezCCASGgAwIBAgIUWhSyw1aTo3dEwELL17bV6tSKi0gwCgYIKoZIzj0EAwIw
EzERMA8GA1UEAwwIT3RoZXIgQ0EwHhcNMjYwNzA2MDI1ODMxWhcNMzYwNzAzMDI1
ODMxWjATMREwDwYDVQQDDAhPdGhlciBDQTBZMBMGByqGSM49AgEGCCqGSM49AwEH
A0IABMx7E1n0dB8spO0H+TQ7KKkOsqCGeHRqRE2grnYUUw/Yetakeq6seHTRYwQE
bH3Kb+Jw/z8Nd1CUHTIorBUDWBCjUzBRMB0GA1UdDgQWBBQ1ppALy2883CHbgdNc
U0UuzKwBbjAfBgNVHSMEGDAWgBQ1ppALy2883CHbgdNcU0UuzKwBbjAPBgNVHRMB
Af8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUCICLtGS9cN9LYNU0Wq9HM3iWSdFrd
ZHof+F2ASqbZYjMcAiEAhb4m/Swc5NRYK/Pp3eJs94IzNcKrwvqUrvvs+tSwDW4=
-----END CERTIFICATE-----"#;

        const FORGED_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBczCCARmgAwIBAgIUPwjMF8WEIi+PJhojP1mBQfN7CDgwCgYIKoZIzj0EAwIw
DzENMAsGA1UEAwwEbGVhZjAeFw0yNjA3MDYwMjU4MzFaFw0yNzA3MDYwMjU4MzFa
MA8xDTALBgNVBAMMBGxlYWYwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASc8rjQ
ovJh4PWjJhGATFHiFWCM5nSFxbTkZP/JEEoBwLvcX59tmRdtQr2N0jsg+Y5lE7nA
YBfM5tRdtUmhtz3+o1MwUTAdBgNVHQ4EFgQUhLYz0CN431vzfNhd2F8+sPDpXnIw
HwYDVR0jBBgwFoAUhLYz0CN431vzfNhd2F8+sPDpXnIwDwYDVR0TAQH/BAUwAwEB
/zAKBggqhkjOPQQDAgNIADBFAiEA7dRB7EGCkC7ulfYQE7EdhBsA0ym/LGvJAIbY
YWzC7nACIBXD/yRzFUPRFLc9i6BWdgKLnH0Q63jwgsuDIfcgcppk
-----END CERTIFICATE-----"#;

        #[test]
        fn leaf_chains_to_its_issuing_ca() {
            let leaf = pem_to_der(LEAF_PEM).unwrap();
            let ca = pem_to_der(CA_PEM).unwrap();
            assert!(verify_cert_chain(&leaf, &[ca]).is_ok());
        }

        #[test]
        fn forged_self_signed_cert_is_rejected() {
            // This is the RCE the whole fix exists to stop: an attacker
            // self-signs a cert with the allowed identity and hands it over.
            // Without the chain check it would pass the SAN allowlist; the
            // chain check rejects it because it isn't issued by the CA.
            let forged = pem_to_der(FORGED_PEM).unwrap();
            let ca = pem_to_der(CA_PEM).unwrap();
            assert!(verify_cert_chain(&forged, &[ca]).is_err());
        }

        #[test]
        fn leaf_rejected_against_unrelated_ca() {
            let leaf = pem_to_der(LEAF_PEM).unwrap();
            let other = pem_to_der(OTHER_CA_PEM).unwrap();
            assert!(verify_cert_chain(&leaf, &[other]).is_err());
        }

        #[test]
        fn validity_window_bounds_integrated_time() {
            // LEAF validity: 2026-07-06 .. 2027-07-06 (unix ~1.783e9 .. 1.815e9).
            let leaf = pem_to_der(LEAF_PEM).unwrap();
            assert!(verify_validity_window(&leaf, 1_790_000_000).is_ok());
            assert!(verify_validity_window(&leaf, 100).is_err()); // 1970, before notBefore
            assert!(verify_validity_window(&leaf, 1_900_000_000).is_err()); // after notAfter
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::trust::AllowedSigner;

    #[tokio::test]
    async fn empty_allowlist_is_rejected() {
        let res = verify_manifest_bundle(b"x", b"{}", &[]).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn malformed_bundle_json_is_rejected() {
        let allow = vec![AllowedSigner {
            issuer: "https://token.actions.githubusercontent.com",
            repo: "https://github.com/Bilbycast/bilbycast-edge",
            ref_pattern: "refs/tags/v*",
            workflow: "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/nightly-release.yml",
        }];
        let res = verify_manifest_bundle(b"manifest", b"not-json", &allow).await;
        assert!(res.is_err());
    }

    #[test]
    fn identity_matches_exact_workflow_prefix_only() {
        let allow = vec![AllowedSigner {
            issuer: "https://token.actions.githubusercontent.com",
            repo: "https://github.com/Bilbycast/bilbycast-edge",
            ref_pattern: "refs/tags/v*",
            workflow: "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/nightly-release.yml",
        }];
        let id = CertIdentity {
            issuer: Some("https://token.actions.githubusercontent.com".to_string()),
            repo: Some("https://github.com/Bilbycast/bilbycast-edge".to_string()),
            workflow_uri: Some(
                "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/nightly-release.yml@refs/tags/v0.45.4"
                    .to_string(),
            ),
            ref_claim: Some("refs/tags/v0.45.4".to_string()),
        };
        assert!(identity_matches_allowlist(&id, &allow));

        let bad_repo = CertIdentity {
            repo: Some("https://github.com/attacker/bilbycast-edge".to_string()),
            ..id.clone()
        };
        assert!(!identity_matches_allowlist(&bad_repo, &allow));

        let bad_ref = CertIdentity {
            ref_claim: Some("refs/heads/main".to_string()),
            ..id.clone()
        };
        assert!(!identity_matches_allowlist(&bad_ref, &allow));

        let bad_workflow = CertIdentity {
            workflow_uri: Some(
                "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/evil.yml@refs/tags/v0.45.4".to_string(),
            ),
            ..id.clone()
        };
        assert!(!identity_matches_allowlist(&bad_workflow, &allow));
    }
}
