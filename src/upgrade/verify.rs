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
//! * `rekorBundle` — the Rekor log entry's payload (`body`,
//!   `integratedTime`, `logIndex`, `logID`) and its Signed Entry
//!   Timestamp, Rekor's signature over that payload, binding the signing
//!   event to a specific public-log entry. It carries no Merkle inclusion
//!   proof, and none is checked. Rekor is the public transparency log that
//!   makes any unauthorised release forensically detectable after the fact.
//!
//! Verification steps (all must succeed):
//!
//! 1. Parse the bundle JSON and extract the cert + signature + Rekor
//!    payload.
//! 2. Verify the leaf cert chains to (is signed by) a Fulcio CA from the
//!    TUF-verified Sigstore trust root. This is the load-bearing
//!    anti-forgery check.
//! 3. Verify the cert's SAN URI matches one of the entries in
//!    [`super::trust::ALLOWED_SIGNERS`].
//! 4. Verify the Rekor entry: (a) the entry body binds this manifest's
//!    SHA-256 and this exact certificate; (b) the Signed Entry Timestamp
//!    verifies under a trusted Rekor public key; (c) the entry's
//!    `integratedTime` falls within the cert's validity window.
//! 5. Verify the signature is valid over the manifest bytes using the
//!    pubkey embedded in the cert.
//!
//! Implementation note: sigstore-rs 0.15 keeps its chain / Rekor
//! verifiers `pub(crate)`, so the checks above are performed in-tree
//! against the crate's public primitives (`SigstoreTrustRoot::{fulcio_certs,
//! rekor_keys}`, `CosignVerificationKey`). `verify_manifest_bundle` is the
//! single entry point. Any failure — including an unreachable/malformed
//! trust root — is fatal (fail-closed), and every one carries a `Refused`
//! naming the kind of check that failed (`Check`), from which the
//! coordinator picks the `error_code` the manager UI renders.

use std::borrow::Cow;
use std::future::Future;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sigstore::trust::sigstore::SigstoreTrustRoot;

use super::trust::{ref_matches, AllowedSigner};

/// The kind of check that refused a bundle. The upgrade coordinator turns it
/// into the `error_code` (`super::map_verify_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    /// Sigstore's trust root could not be loaded: its TUF repository was
    /// unreachable, or what it served did not verify or held no Fulcio CA.
    /// Nothing about the release itself has been checked yet.
    TrustRoot,
    /// The bundle did not parse, its certificate does not chain to a trusted
    /// Fulcio CA, or the manifest signature does not verify under it.
    Signature,
    /// The signer's identity claims do not match the allowlist (or there is
    /// no allowlist to match).
    Identity,
    /// The Rekor entry is missing, was logged outside the certificate's
    /// validity window, does not bind this manifest and certificate, or
    /// carries a Signed Entry Timestamp no trusted Rekor key verifies.
    Rekor,
}

/// The context every failing step of the verification attaches to its error.
/// It displays as `reason`, and the coordinator finds it again by type —
/// `anyhow::Error::downcast_ref` walks the whole context chain — so the
/// `error_code` never depends on how a message is worded.
#[derive(Debug)]
pub(crate) struct Refused {
    pub check: Check,
    reason: Cow<'static, str>,
}

impl Refused {
    fn new(check: Check, reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            check,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Top-level entry point. Returns `Ok(())` only when **every** check
/// passes; on any failure the error chain names the specific reason and
/// carries a `Refused` the caller maps to an `error_code`.
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
    // Sigstore's trust root: TUF metadata fetched from Sigstore's TUF
    // repository (starting from the TUF root embedded in the `sigstore`
    // crate) vouches for `trusted_root.json`, the Fulcio CA certs + Rekor
    // public keys every check below runs against. Fetched on every call and
    // never stored, so Fulcio and Rekor key rotations land without a code
    // change or a rebuild. No network, no trust root: the call fails closed.
    verify_against(
        manifest_bytes,
        bundle_bytes,
        allowed_signers,
        SigstoreTrustRoot::new(None),
    )
    .await
}

/// [`verify_manifest_bundle`] against the trust root `trust_root` resolves
/// to. It is awaited only once the bundle has parsed, so a malformed bundle
/// never reaches the network; the tests hand it a fixed root or a failure.
async fn verify_against(
    manifest_bytes: &[u8],
    bundle_bytes: &[u8],
    allowed_signers: &[AllowedSigner],
    trust_root: impl Future<Output = sigstore::errors::Result<SigstoreTrustRoot>>,
) -> Result<()> {
    if allowed_signers.is_empty() {
        bail!(Refused::new(
            Check::Identity,
            "identity allowlist is empty — refusing to verify any signature",
        ));
    }

    let bundle: CosignBundle = serde_json::from_slice(bundle_bytes).context(Refused::new(
        Check::Signature,
        "failed to parse cosign Sigstore bundle JSON",
    ))?;

    // 1. Parse cert + check chain to Fulcio root.
    let cert_pem = bundle.cert.as_deref().context(Refused::new(
        Check::Signature,
        "bundle missing 'cert' (X.509 PEM)",
    ))?;
    let signature_b64 = bundle.base64_signature.as_deref().context(Refused::new(
        Check::Signature,
        "bundle missing 'base64Signature'",
    ))?;

    let trust_root = trust_root.await.context(Refused::new(
        Check::TrustRoot,
        "failed to load Sigstore trust root (Fulcio + Rekor)",
    ))?;

    // 2. Chain the cert to a trusted Fulcio CA, check its validity window
    //    against the Rekor integrated time, and bind the Rekor entry to THIS
    //    manifest + certificate (plus verify the Rekor SET signature).
    sigstore_inner::verify_cert_and_rekor(
        cert_pem,
        manifest_bytes,
        bundle.rekor_bundle.as_ref(),
        &trust_root,
    )?;

    // 3. SAN identity match against `allowed_signers`.
    let identity = sigstore_inner::extract_identity_claims(cert_pem).context(Refused::new(
        Check::Identity,
        "could not extract identity claims from Fulcio cert",
    ))?;
    if !identity_matches_allowlist(&identity, allowed_signers) {
        bail!(Refused::new(
            Check::Identity,
            format!(
                "Sigstore cert identity {identity:?} is not in the upgrade ALLOWED_SIGNERS \
                 allowlist — refusing to install (the binary may have been signed by an \
                 unauthorised workflow)"
            ),
        ));
    }

    // 4. Verify signature over the raw manifest bytes.
    sigstore_inner::verify_blob_signature(cert_pem, signature_b64, manifest_bytes).context(
        Refused::new(
            Check::Signature,
            "manifest signature does not verify under the cert's pubkey",
        ),
    )?;

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

/// The Rekor entry from cosign's bundle: the log entry's `Payload` and its
/// `SignedEntryTimestamp`, Rekor's signature over that payload, which
/// asserts that the entry was logged in the public transparency log at
/// `Payload.integratedTime`. It carries no Merkle inclusion proof.
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
                // The SAN is `<workflow>@<ref>`, so the workflow path must end
                // where the `@` begins: `nightly-release.yml.evil.yml@…` is a
                // different workflow file that merely starts the same way.
                iss == s.issuer
                    && repo == s.repo
                    && wf
                        .strip_prefix(s.workflow)
                        .is_some_and(|rest| rest.starts_with('@'))
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
/// * `sigstore::trust::sigstore::SigstoreTrustRoot` for the Fulcio CA
///   certificates and Rekor public keys.
/// * X.509 parsing via `x509-cert`, our own direct dependency — sigstore-rs
///   does NOT re-export it, so this is not a redundant dep to "simplify"
///   away. It moves in lockstep with sigstore's own requirement (0.3 for
///   sigstore 0.15); see the lockstep note in Cargo.toml.
/// * sigstore's `CosignVerificationKey` for every signature check, on its
///   aws-lc-rs backend (the `rustls-tls` feature).
///
/// Two ECDSA schemes are in play. Fulcio's intermediate CA is P-384 and
/// signs leaf certificates ecdsa-with-SHA384 (the chain check maps the
/// leaf's signature-algorithm OID to the scheme); the ephemeral leaf key
/// that signs the blob and Rekor's log key are both P-256, checked as
/// ECDSA-P256-SHA256. Anything else (Ed25519 long-term keys, another curve)
/// is refused with an error rather than silently accepting an unverified
/// blob.
pub(crate) mod sigstore_inner {
    use anyhow::{anyhow, bail, Context, Result};
    use base64::Engine;
    use sigstore::trust::sigstore::SigstoreTrustRoot;

    use super::{CertIdentity, Check, Refused};

    /// Verify the cert chain to a trusted Fulcio CA **and** the Rekor entry
    /// — its binding to this manifest and certificate, and its Signed Entry
    /// Timestamp under a trusted Rekor public key — both key sets taken from
    /// `trust_root`.
    pub fn verify_cert_and_rekor(
        cert_pem: &str,
        manifest_bytes: &[u8],
        rekor_bundle: Option<&super::RekorBundle>,
        trust_root: &SigstoreTrustRoot,
    ) -> Result<()> {
        use sigstore::trust::TrustRoot;

        // The roots of trust: the Fulcio CA certs + Rekor public keys of the
        // TUF-verified `trusted_root.json`. Verification below is entirely
        // against them.
        let fulcio_certs = trust_root.fulcio_certs().context(Refused::new(
            Check::TrustRoot,
            "Fulcio cert bundle unavailable",
        ))?;
        let rekor_keys = trust_root.rekor_keys().context(Refused::new(
            Check::TrustRoot,
            "Rekor public key unavailable",
        ))?;

        // Rekor inclusion is mandatory — a bundle without it cannot be
        // transparency-verified, and its absence must never pass.
        let rekor = rekor_bundle.context(Refused::new(
            Check::Rekor,
            "rekor bundle missing — every release must be logged in Rekor",
        ))?;

        let leaf_der = pem_to_der(cert_pem).context(Refused::new(
            Check::Signature,
            "bundle 'cert' does not decode to a certificate",
        ))?;

        // (a) Chain: the leaf MUST be signed by a trusted Fulcio CA. This is
        //     the load-bearing anti-forgery check — without it an attacker can
        //     self-sign a certificate carrying the allowed workflow identity
        //     and pass the SAN allowlist below.
        verify_cert_chain(&leaf_der, &fulcio_certs).context(Refused::new(
            Check::Signature,
            "Fulcio certificate-chain verification failed",
        ))?;

        // (b) The signing cert's validity window must contain the Rekor
        //     integrated time (the cosign/Fulcio short-lived-cert model).
        verify_validity_window(&leaf_der, rekor.payload.integrated_time).context(Refused::new(
            Check::Rekor,
            "certificate validity window does not contain the Rekor integrated time",
        ))?;

        // (c) The Rekor entry body must bind THIS manifest hash AND THIS
        //     certificate — otherwise a valid entry for some other artifact
        //     could be replayed alongside a forged manifest.
        verify_rekor_body_binding(&rekor.payload.body, manifest_bytes, &leaf_der).context(
            Refused::new(
                Check::Rekor,
                "Rekor entry body does not bind this manifest and certificate",
            ),
        )?;

        // (d) The Signed Entry Timestamp must verify under a trusted Rekor
        //     public key (proves Rekor actually logged this entry).
        verify_rekor_set(rekor, &rekor_keys).context(Refused::new(
            Check::Rekor,
            "Rekor SignedEntryTimestamp verification failed",
        ))?;

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
        use x509_cert::der::{Any, Decode, Encode};
        use x509_cert::Certificate;

        let leaf = Certificate::from_der(leaf_der)
            .map_err(|e| anyhow!("leaf X.509 DER parse error: {e}"))?;
        // The exact bytes Fulcio signed: the leaf's TBSCertificate DER *as
        // received* — the first element of the outer Certificate SEQUENCE.
        // Never re-encode the parsed TBSCertificate: x509-cert 0.3 rewrites a
        // pre-2050 GeneralizedTime validity as UTCTime on encode, so for such
        // a certificate the re-encoding is not what the CA signed and a
        // genuine release would fail closed. The parse above is strict DER
        // (one encoding per value), which is what ties the fields the later
        // checks read to these signed bytes.
        let leaf_fields = Vec::<Any>::from_der(leaf_der)
            .map_err(|e| anyhow!("leaf X.509 outer SEQUENCE: {e}"))?;
        let tbs_der = leaf_fields
            .first()
            .ok_or_else(|| anyhow!("leaf has no TBSCertificate"))?
            .to_der()
            .map_err(|e| anyhow!("leaf TBSCertificate bytes: {e}"))?;
        let sig = leaf
            .signature()
            .as_bytes()
            .ok_or_else(|| anyhow!("leaf signature is not octet-aligned"))?;
        let scheme = scheme_for_oid(&leaf.signature_algorithm().oid.to_string())?;

        for ca_der in fulcio_certs {
            let Ok(ca) = Certificate::from_der(ca_der.as_ref()) else {
                continue;
            };
            let Ok(spki_der) = ca.tbs_certificate().subject_public_key_info().to_der() else {
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
            .tbs_certificate()
            .validity()
            .not_before
            .to_unix_duration()
            .as_secs() as i64;
        let na = leaf
            .tbs_certificate()
            .validity()
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
                && key
                    .verify_signature(Signature::Raw(&set_sig), canonical.as_bytes())
                    .is_ok()
                {
                    return Ok(());
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
        let spki = cert.tbs_certificate().subject_public_key_info();
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
        // Fulcio writes each claim twice: the current extensions hold a
        // DER-encoded UTF8String, the deprecated ones (.1.1 to .1.6) the raw
        // bytes. Read the current one and fall back to the deprecated one,
        // so a cert missing either still yields the claim.
        //
        // The repository is not the same string in the two. The current
        // `source-repository-uri` (.1.12) is `https://github.com/<owner>/<repo>`,
        // the form the allowlist holds; the deprecated `GitHub Workflow
        // Repository` (.1.5) is bare `<owner>/<repo>`. Comparing .1.5 against
        // the allowlist rejected every genuine release on v0.105.0 to v0.114.0
        // (earlier edges failed before this check, parsing cosign's
        // base64-of-PEM cert).
        const OID_ISSUER: &str = "1.3.6.1.4.1.57264.1.8";
        const OID_ISSUER_V1: &str = "1.3.6.1.4.1.57264.1.1";
        const OID_REPO_URI: &str = "1.3.6.1.4.1.57264.1.12";
        const OID_REPO_V1: &str = "1.3.6.1.4.1.57264.1.5";
        const OID_REF: &str = "1.3.6.1.4.1.57264.1.14";
        const OID_REF_V1: &str = "1.3.6.1.4.1.57264.1.6";
        // SAN URI carries the workflow + ref combined; we parse it below.

        let (mut issuer_v1, mut repo_v1, mut ref_v1) = (None, None, None);
        if let Some(exts) = cert.tbs_certificate().extensions() {
            for e in exts {
                let oid = e.extn_id.to_string();
                let bytes = e.extn_value.as_bytes();
                match oid.as_str() {
                    OID_ISSUER => id.issuer = der_utf8(bytes),
                    OID_ISSUER_V1 => issuer_v1 = raw_utf8(bytes),
                    OID_REPO_URI => id.repo = der_utf8(bytes),
                    OID_REPO_V1 => repo_v1 = raw_utf8(bytes),
                    OID_REF => id.ref_claim = der_utf8(bytes),
                    OID_REF_V1 => ref_v1 = raw_utf8(bytes),
                    _ => {}
                }
                // SAN is OID 2.5.29.17. Cosign's keyless flow puts the
                // workflow URI in the URI form of the SAN.
                if oid == "2.5.29.17"
                    && let Some(s) = first_san_uri(bytes) {
                        id.workflow_uri = Some(s);
                    }
            }
        }

        id.issuer = id.issuer.or(issuer_v1);
        // The issuer must be GitHub Actions to match at all, so a bare
        // `<owner>/<repo>` is a github.com repository.
        id.repo = id
            .repo
            .or_else(|| repo_v1.map(|r| format!("https://github.com/{r}")));
        id.ref_claim = id.ref_claim.or(ref_v1);

        Ok(id)
    }

    /// A current Fulcio extension: the `OCTET STRING` holds a DER `UTF8String`.
    fn der_utf8(bytes: &[u8]) -> Option<String> {
        use x509_cert::der::{asn1::Utf8StringRef, Decode};
        let s = Utf8StringRef::from_der(bytes).ok()?;
        Some(s.as_str().to_owned()).filter(|s| !s.is_empty())
    }

    /// A deprecated Fulcio extension: the `OCTET STRING` holds the raw bytes.
    fn raw_utf8(bytes: &[u8]) -> Option<String> {
        std::str::from_utf8(bytes)
            .ok()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
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

    /// Decode the bundle's `cert` field to DER.
    ///
    /// **Two encodings are accepted, and that is not defensive slop.**
    /// `cosign sign-blob --bundle` writes `LocalSignedPayload.Cert` as
    /// **base64-of-PEM**, not raw PEM — see the bundle fixture in
    /// `sigstore-0.14.0/src/cosign/bundle.rs`, whose `cert` begins
    /// `LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0t` (base64 of
    /// `-----BEGIN CERTIFICATE-----`). The Rekor body's
    /// `spec.signature.publicKey.content` uses the same encoding. Accepting
    /// only raw PEM therefore rejects every genuine release bundle at the
    /// first parse, before any signature is checked — an upgrade that fails
    /// closed, but fails.
    ///
    /// Raw PEM is still accepted because the chain tests below build their
    /// fixtures that way, and because a hand-assembled bundle may use it.
    ///
    /// A *present but malformed* PEM block is an error rather than a silent
    /// retry as base64 — hence `Option<Result<_>>` from [`try_pem_block`].
    fn pem_to_der(cert_field: &str) -> Result<Vec<u8>> {
        let trimmed = cert_field.trim();
        if let Some(der) = try_pem_block(trimmed) {
            return der;
        }
        let inner = base64::engine::general_purpose::STANDARD
            .decode(trimmed.as_bytes())
            .context("cert field is neither a PEM CERTIFICATE block nor base64")?;
        let inner = std::str::from_utf8(&inner)
            .context("base64-decoded cert field is not UTF-8 PEM")?;
        try_pem_block(inner.trim())
            .ok_or_else(|| anyhow!("cert field is not a single PEM CERTIFICATE block"))?
    }

    /// `Some(..)` when `s` looks like a PEM CERTIFICATE block — carrying the
    /// decode result so a corrupt body surfaces as an error rather than
    /// falling through to the base64 branch. `None` when it is not PEM at all.
    fn try_pem_block(s: &str) -> Option<Result<Vec<u8>>> {
        let body = s
            .strip_prefix("-----BEGIN CERTIFICATE-----")?
            .strip_suffix("-----END CERTIFICATE-----")?;
        let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        Some(
            base64::engine::general_purpose::STANDARD
                .decode(cleaned)
                .context("PEM body is not valid base64"),
        )
    }

    #[cfg(test)]
    mod chain_tests {
        use super::*;

        /// A real `cosign sign-blob --bundle` writes the `cert` field as
        /// **base64-of-PEM**. This is the exact encoding from the sigstore
        /// crate's own bundle fixture (`sigstore-0.14.0/src/cosign/bundle.rs`,
        /// generated by running cosign), reduced to just the cert.
        ///
        /// Before the dual-encoding decode, this input failed at the first
        /// parse with "cert is not a single PEM CERTIFICATE block" — so every
        /// genuine release bundle was rejected before a single signature was
        /// checked. Both encodings must decode to identical DER.
        #[test]
        fn cert_field_accepts_both_raw_pem_and_cosigns_base64_of_pem() {
            use base64::Engine;
            // `LEAF_PEM` is already a complete PEM block.
            let from_raw = pem_to_der(LEAF_PEM).expect("raw PEM must still decode");

            let b64_of_pem =
                base64::engine::general_purpose::STANDARD.encode(LEAF_PEM.as_bytes());
            let from_b64 =
                pem_to_der(&b64_of_pem).expect("cosign writes base64-of-PEM; it must decode");

            assert_eq!(from_raw, from_b64, "both encodings must yield the same DER");
            assert!(!from_raw.is_empty());
        }

        /// A present-but-corrupt PEM block must be an error, not a silent
        /// retry down the base64 branch that then reports a confusing message.
        #[test]
        fn corrupt_pem_block_is_an_error_not_a_base64_retry() {
            let bad = "-----BEGIN CERTIFICATE-----\n!!!not base64!!!\n-----END CERTIFICATE-----";
            let err = pem_to_der(bad).expect_err("corrupt PEM body must error");
            assert!(
                format!("{err:#}").contains("PEM body is not valid base64"),
                "expected the PEM-body error, got: {err:#}"
            );
        }

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

        /// A real Fulcio leaf against the real CA that issued it. The synthetic
        /// fixtures above are P-256 / ecdsa-with-SHA256; real releases are
        /// signed ecdsa-with-SHA384 by Fulcio's P-384 intermediate, a path no
        /// other offline test runs.
        ///
        /// `testdata/sigstore-intermediate.pem` is `O=sigstore.dev,
        /// CN=sigstore-intermediate` (serial
        /// 00b9d58957e75346eb25ab264641eb9ff5277da4, valid 2022-04-13 to
        /// 2031-10-05), PEM-wrapped from
        /// `certificateAuthorities[1].certChain.certificates[0].rawBytes` of the
        /// Sigstore public-good `trusted_root.json` that `sigstore-0.15.1`
        /// embeds (`trust_root/prod/trusted_root.json`). The leaf is the
        /// bilbycast-edge v0.114.0 release certificate.
        #[test]
        fn real_fulcio_leaf_chains_to_the_sigstore_intermediate() {
            let leaf = pem_to_der(include_str!("testdata/fulcio-edge-v0.114.0.pem")).unwrap();
            let intermediate =
                pem_to_der(include_str!("testdata/sigstore-intermediate.pem")).unwrap();
            assert!(verify_cert_chain(&leaf, std::slice::from_ref(&intermediate)).is_ok());

            // One letter of the repository name changed inside the signed
            // TBSCertificate: still a well-formed certificate, no longer the
            // one Fulcio signed.
            let mut tampered = leaf.clone();
            let at = tampered
                .windows(b"bilbycast-edge".len())
                .position(|w| w == b"bilbycast-edge")
                .unwrap();
            tampered[at] = b'B';
            let err = verify_cert_chain(&tampered, &[intermediate]).unwrap_err();
            assert!(
                format!("{err:#}").contains("does not chain"),
                "expected the signature check to refuse it, got: {err:#}"
            );
        }

        // A leaf whose validity is GeneralizedTime before 2050 — RFC 5280 says
        // UTCTime, some CAs write GeneralizedTime anyway. Built offline:
        // GT_CA_PEM with `openssl req -x509` (P-256); the leaf's TBSCertificate
        // assembled by hand, because OpenSSL's `-not_before` always writes
        // UTCTime for 1950-2049, then signed ecdsa-with-SHA256 with GT_CA's key
        // (`openssl dgst -sha256 -sign`). `openssl verify` accepts the pair.
        const GT_CA_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBmDCCAT+gAwIBAgIUKcNYqTNKMpbXd2mfbdJtpa5PGSAwCgYIKoZIzj0EAwIw
IjEgMB4GA1UEAwwXVGVzdCBHZW5lcmFsaXplZFRpbWUgQ0EwHhcNMjYxMTIzMDAw
NjQ1WhcNMzYxMTIwMDAwNjQ1WjAiMSAwHgYDVQQDDBdUZXN0IEdlbmVyYWxpemVk
VGltZSBDQTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABKlS9hq3dxouZ3Oy6GA+
2E+9C120rXBjsVISdJgx2RKb2qLhkp3SaTh739HYgMWXjgHI7Gcm5QlSJLWvnZbN
D7ajUzBRMB0GA1UdDgQWBBSrCY4cnz5M9XoBHE1Dwjs9o+zVBzAfBgNVHSMEGDAW
gBSrCY4cnz5M9XoBHE1Dwjs9o+zVBzAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49
BAMCA0cAMEQCID6ikufpoJDbbWmbTPAxCLO2PJnjV8YMz5qUlaPBVAuWAiAPY6Mj
PugDFTldQPQHGE+H3m1Ad854X7jmCxkW4qlmUQ==
-----END CERTIFICATE-----"#;

        const GT_LEAF_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBJjCBzKADAgECAgJqHDAKBggqhkjOPQQDAjAiMSAwHgYDVQQDDBdUZXN0IEdl
bmVyYWxpemVkVGltZSBDQTAiGA8yMDI2MDEwMTAwMDAwMFoYDzIwMjcwMTAxMDAw
MDAwWjASMRAwDgYDVQQDDAdndC1sZWFmMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcD
QgAEWqPq65qgufcUDlQMlN55hqHTptD4kwgdJQLTY1v/Vj9acQbnFpb3QGPKmGIy
jPrI3sWpIXQBx7W1gvTn7/GISzAKBggqhkjOPQQDAgNJADBGAiEA9BwKUEpyzhHu
bpXwon4K5RU/sBryQuC9JUiExTB6hPACIQD+fW2UJzROfOeAydcCXTzKwkgKC0kg
DBlyousmSRzpyg==
-----END CERTIFICATE-----"#;

        /// x509-cert 0.3 rewrites such a validity as UTCTime when it encodes, so
        /// a chain check that hashed a re-encoded TBSCertificate would refuse
        /// this genuine leaf. The chain check must use the bytes as received.
        #[test]
        fn leaf_with_generalized_time_validity_chains() {
            use x509_cert::der::{Any, Decode, Encode};
            let leaf = pem_to_der(GT_LEAF_PEM).unwrap();
            let ca = pem_to_der(GT_CA_PEM).unwrap();

            // The fixture must be one x509-cert does not re-encode byte for
            // byte, or this test proves nothing.
            let as_received = Vec::<Any>::from_der(&leaf).unwrap()[0].to_der().unwrap();
            let reencoded = x509_cert::Certificate::from_der(&leaf)
                .unwrap()
                .tbs_certificate()
                .to_der()
                .unwrap();
            assert_ne!(as_received, reencoded);

            assert!(verify_cert_chain(&leaf, &[ca]).is_ok());
            // GT_LEAF validity: 2026-01-01 .. 2027-01-01 (unix 1_767_225_600 ..
            // 1_798_761_600).
            assert!(verify_validity_window(&leaf, 1_780_000_000).is_ok());
            assert!(verify_validity_window(&leaf, 1_800_000_000).is_err());
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

        let longer_workflow = CertIdentity {
            workflow_uri: Some(
                "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/nightly-release.yml.evil.yml@refs/tags/v0.45.4"
                    .to_string(),
            ),
            ..id.clone()
        };
        assert!(!identity_matches_allowlist(&longer_workflow, &allow));
    }

    // Real Fulcio certificates, decoded from published releases'
    // `manifest.sig.bundle` (which carries them base64-of-PEM). They expired
    // ten minutes after issue; claim extraction never reads the validity
    // window, so that does not matter here.
    const EDGE_RELEASE_CERT: &str = include_str!("testdata/fulcio-edge-v0.114.0.pem");
    const RELAY_RELEASE_CERT: &str = include_str!("testdata/fulcio-relay-v0.16.0.pem");
    // Self-signed with OpenSSL, the edge's identity in each: only the deprecated
    // extensions (.1.1 / .1.5 / .1.6, raw bytes), only the current ones (.1.8 /
    // .1.12 / .1.14, DER UTF8String), and both with every claim different in
    // the current set (`https://issuer.example.com`, `attacker/bilbycast-edge`,
    // `refs/heads/main`) — once with the deprecated extensions first, once with
    // the current ones first, so neither first-wins nor last-wins passes.
    const V1_ONLY_CERT: &str = include_str!("testdata/fulcio-synthetic-v1-only.pem");
    const V2_ONLY_CERT: &str = include_str!("testdata/fulcio-synthetic-v2-only.pem");
    const CONFLICT_V1_FIRST_CERT: &str =
        include_str!("testdata/fulcio-synthetic-conflict-v1-first.pem");
    const CONFLICT_V2_FIRST_CERT: &str =
        include_str!("testdata/fulcio-synthetic-conflict-v2-first.pem");

    /// The test that would have caught the defect: every earlier test built a
    /// `CertIdentity` by hand, already holding the URL form, so none of them
    /// ever read a certificate Fulcio had issued. Fed this one, the old reader
    /// took the repository from .1.5 (`Bilbycast/bilbycast-edge`) and the
    /// allowlist refused every genuine release.
    #[test]
    fn a_real_release_certificate_matches_the_compiled_in_allowlist() {
        let id = sigstore_inner::extract_identity_claims(EDGE_RELEASE_CERT).unwrap();
        assert_eq!(
            id.issuer.as_deref(),
            Some("https://token.actions.githubusercontent.com")
        );
        assert_eq!(
            id.repo.as_deref(),
            Some("https://github.com/Bilbycast/bilbycast-edge")
        );
        assert_eq!(id.ref_claim.as_deref(), Some("refs/tags/v0.114.0"));
        assert_eq!(
            id.workflow_uri.as_deref(),
            Some(
                "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/nightly-release.yml@refs/tags/v0.114.0"
            )
        );
        assert!(identity_matches_allowlist(
            &id,
            crate::upgrade::trust::ALLOWED_SIGNERS
        ));
    }

    #[test]
    fn another_repositorys_release_certificate_is_refused() {
        let id = sigstore_inner::extract_identity_claims(RELAY_RELEASE_CERT).unwrap();
        assert_eq!(
            id.repo.as_deref(),
            Some("https://github.com/Bilbycast/bilbycast-relay")
        );
        assert!(!identity_matches_allowlist(
            &id,
            crate::upgrade::trust::ALLOWED_SIGNERS
        ));
    }

    #[test]
    fn a_certificate_with_only_the_deprecated_extensions_still_matches() {
        let id = sigstore_inner::extract_identity_claims(V1_ONLY_CERT).unwrap();
        assert_eq!(
            id.issuer.as_deref(),
            Some("https://token.actions.githubusercontent.com")
        );
        assert_eq!(
            id.repo.as_deref(),
            Some("https://github.com/Bilbycast/bilbycast-edge")
        );
        assert_eq!(id.ref_claim.as_deref(), Some("refs/tags/v0.45.4"));
        assert!(identity_matches_allowlist(
            &id,
            crate::upgrade::trust::ALLOWED_SIGNERS
        ));
    }

    #[test]
    fn a_certificate_with_only_the_current_extensions_matches() {
        let id = sigstore_inner::extract_identity_claims(V2_ONLY_CERT).unwrap();
        assert_eq!(
            id.issuer.as_deref(),
            Some("https://token.actions.githubusercontent.com")
        );
        assert_eq!(
            id.repo.as_deref(),
            Some("https://github.com/Bilbycast/bilbycast-edge")
        );
        assert_eq!(id.ref_claim.as_deref(), Some("refs/tags/v0.45.4"));
        assert!(identity_matches_allowlist(
            &id,
            crate::upgrade::trust::ALLOWED_SIGNERS
        ));
    }

    /// Each current extension is read in preference to its deprecated one, not
    /// alongside it, whichever comes first in the certificate: a deprecated value
    /// that would match cannot rescue a current one that does not.
    #[test]
    fn the_current_extensions_win_over_the_deprecated_ones() {
        for cert in [CONFLICT_V1_FIRST_CERT, CONFLICT_V2_FIRST_CERT] {
            let id = sigstore_inner::extract_identity_claims(cert).unwrap();
            assert_eq!(id.issuer.as_deref(), Some("https://issuer.example.com"));
            assert_eq!(
                id.repo.as_deref(),
                Some("https://github.com/attacker/bilbycast-edge")
            );
            assert_eq!(id.ref_claim.as_deref(), Some("refs/heads/main"));
            assert!(!identity_matches_allowlist(
                &id,
                crate::upgrade::trust::ALLOWED_SIGNERS
            ));
        }
    }
}

/// Which `error_code` each way of refusing a bundle reports. Run through the
/// whole pipeline against a fixed trust root, so no test reaches the network:
/// a genuine release and the trust root it chains to, then that release with
/// one thing wrong at a time, each through `super::map_verify_error`.
#[cfg(test)]
mod refusal_tests {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use serde_json::Value;
    use sigstore::errors::SigstoreError;

    use super::*;
    use crate::upgrade::{error_codes, map_verify_error, UpgradeError};

    /// `manifest.json` and `manifest.sig.bundle` as published with
    /// bilbycast-edge v0.117.0, byte for byte: the Rekor entry binds the
    /// manifest's SHA-256 (`96d1fed0…`), and the certificate was issued to the
    /// edge's `nightly-release.yml` at `refs/tags/v0.117.0`.
    const RELEASE_MANIFEST: &[u8] = include_bytes!("testdata/release-edge-v0.117.0.manifest.json");
    const RELEASE_BUNDLE: &[u8] =
        include_bytes!("testdata/release-edge-v0.117.0.manifest.sig.bundle");

    /// `trust_root/prod/trusted_root.json` as `sigstore-0.15.1` embeds it
    /// (SHA-256 `f44a1b88…`): Sigstore's public-good Fulcio CAs and Rekor key.
    const TRUSTED_ROOT: &[u8] = include_bytes!("testdata/sigstore-trusted-root.json");

    /// The allowlist the release above was signed under: this binary's own.
    const RELEASE_SIGNERS: &[AllowedSigner] = crate::upgrade::trust::ALLOWED_SIGNERS;

    /// Another binary's allowlist, which the release above must not satisfy.
    const OTHER_SIGNERS: &[AllowedSigner] = &[AllowedSigner {
        issuer: "https://token.actions.githubusercontent.com",
        repo: "https://github.com/Bilbycast/bilbycast-appear-x-api-gateway",
        ref_pattern: "refs/tags/v*",
        workflow: "https://github.com/Bilbycast/bilbycast-appear-x-api-gateway/.github/workflows/nightly-release.yml",
    }];

    async fn offline_root() -> sigstore::errors::Result<SigstoreTrustRoot> {
        SigstoreTrustRoot::from_trusted_root_json_unchecked(TRUSTED_ROOT)
    }

    async fn never_loaded() -> sigstore::errors::Result<SigstoreTrustRoot> {
        panic!("the trust root is loaded only for a bundle that parses")
    }

    /// Verify, expecting a refusal; return the check it names and what the
    /// coordinator reports for it.
    async fn refuse(
        manifest: &[u8],
        bundle: &[u8],
        signers: &[AllowedSigner],
        trust_root: impl Future<Output = sigstore::errors::Result<SigstoreTrustRoot>>,
    ) -> (Check, UpgradeError) {
        let err = verify_against(manifest, bundle, signers, trust_root)
            .await
            .expect_err("the bundle must be refused");
        let check = err
            .downcast_ref::<Refused>()
            .expect("every refusal names the check that failed")
            .check;
        (check, map_verify_error(err))
    }

    /// The release bundle with `edit` applied to its JSON.
    fn edited_bundle(edit: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut bundle: Value = serde_json::from_slice(RELEASE_BUNDLE).unwrap();
        edit(&mut bundle);
        serde_json::to_vec(&bundle).unwrap()
    }

    /// Flip the last byte of a base64 field's decoded value.
    fn flip_last_byte(field: &mut Value) {
        let mut bytes = B64.decode(field.as_str().unwrap()).unwrap();
        *bytes.last_mut().unwrap() ^= 0x01;
        *field = Value::String(B64.encode(bytes));
    }

    #[tokio::test]
    async fn a_genuine_release_verifies_against_the_trust_root_it_chains_to() {
        verify_against(
            RELEASE_MANIFEST,
            RELEASE_BUNDLE,
            RELEASE_SIGNERS,
            offline_root(),
        )
        .await
        .unwrap_or_else(|e| panic!("genuine release refused: {e:#}"));
    }

    /// With no route to Sigstore's TUF repository `SigstoreTrustRoot::new`
    /// fails with "root metadata is expired": tough takes a next root it
    /// cannot fetch for the end of the chain, and the embedded TUF root has
    /// expired. That error wraps a `tough` error, and `tough` is not a
    /// dependency here, so a `TufMetadataError` with the same text stands in
    /// for it. The release was never checked: a network error, not a bad
    /// signature — and the message carries the cause, not only the outermost
    /// context.
    #[tokio::test]
    async fn a_trust_root_that_does_not_load_is_a_network_error() {
        let unreachable = async {
            Err(SigstoreError::TufMetadataError(
                "root metadata is expired".into(),
            ))
        };
        let (check, e) = refuse(
            RELEASE_MANIFEST,
            RELEASE_BUNDLE,
            RELEASE_SIGNERS,
            unreachable,
        )
        .await;
        assert_eq!(check, Check::TrustRoot);
        assert_eq!(e.code, error_codes::UPGRADE_NETWORK_ERROR);
        assert_eq!(
            e.message,
            "failed to load Sigstore trust root (Fulcio + Rekor): root metadata is expired"
        );
    }

    #[tokio::test]
    async fn a_trust_root_without_a_fulcio_ca_is_a_network_error() {
        let mut root: Value = serde_json::from_slice(TRUSTED_ROOT).unwrap();
        root["certificateAuthorities"] = Value::Array(Vec::new());
        let root = serde_json::to_vec(&root).unwrap();
        let no_ca = async move { SigstoreTrustRoot::from_trusted_root_json_unchecked(&root) };
        let (check, e) = refuse(RELEASE_MANIFEST, RELEASE_BUNDLE, RELEASE_SIGNERS, no_ca).await;
        assert_eq!(check, Check::TrustRoot);
        assert_eq!(e.code, error_codes::UPGRADE_NETWORK_ERROR);
        assert!(
            e.message.starts_with("Fulcio cert bundle unavailable"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn a_release_with_no_rekor_entry_is_rekor_invalid() {
        let bundle = edited_bundle(|b| {
            b.as_object_mut().unwrap().remove("rekorBundle");
        });
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, offline_root()).await;
        assert_eq!(check, Check::Rekor);
        assert_eq!(e.code, error_codes::UPGRADE_REKOR_INVALID);
        assert!(
            e.message.starts_with("rekor bundle missing"),
            "{}",
            e.message
        );
    }

    /// One byte of the manifest changed: the Rekor entry no longer binds it.
    #[tokio::test]
    async fn a_manifest_the_rekor_entry_does_not_bind_is_rekor_invalid() {
        let mut manifest = RELEASE_MANIFEST.to_vec();
        let at = manifest
            .windows(9)
            .position(|w| w == b"\"0.117.0\"")
            .unwrap();
        manifest[at + 7] = b'1';
        let (check, e) = refuse(&manifest, RELEASE_BUNDLE, RELEASE_SIGNERS, offline_root()).await;
        assert_eq!(check, Check::Rekor);
        assert_eq!(e.code, error_codes::UPGRADE_REKOR_INVALID);
        assert!(
            e.message
                .starts_with("Rekor entry body does not bind this manifest and certificate"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn a_signed_entry_timestamp_no_rekor_key_verifies_is_rekor_invalid() {
        let bundle =
            edited_bundle(|b| flip_last_byte(&mut b["rekorBundle"]["SignedEntryTimestamp"]));
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, offline_root()).await;
        assert_eq!(check, Check::Rekor);
        assert_eq!(e.code, error_codes::UPGRADE_REKOR_INVALID);
        assert!(
            e.message
                .starts_with("Rekor SignedEntryTimestamp verification failed"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn a_rekor_entry_logged_outside_the_certificate_validity_is_rekor_invalid() {
        let bundle = edited_bundle(|b| {
            let t = &mut b["rekorBundle"]["Payload"]["integratedTime"];
            *t = Value::from(t.as_i64().unwrap() + 86_400);
        });
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, offline_root()).await;
        assert_eq!(check, Check::Rekor);
        assert_eq!(e.code, error_codes::UPGRADE_REKOR_INVALID);
        assert!(
            e.message
                .starts_with("certificate validity window does not contain"),
            "{}",
            e.message
        );
    }

    /// One letter of the repository name changed inside the certificate's
    /// signed TBSCertificate: well-formed, but not what Fulcio signed.
    #[tokio::test]
    async fn a_certificate_no_fulcio_ca_signed_is_signature_invalid() {
        let bundle = edited_bundle(|b| {
            let pem = String::from_utf8(B64.decode(b["cert"].as_str().unwrap()).unwrap()).unwrap();
            let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
            let mut der = B64.decode(body).unwrap();
            let at = der
                .windows(b"bilbycast-edge".len())
                .position(|w| w == b"bilbycast-edge")
                .unwrap();
            der[at] = b'B';
            let pem = format!(
                "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                B64.encode(der)
            );
            b["cert"] = Value::String(B64.encode(pem));
        });
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, offline_root()).await;
        assert_eq!(check, Check::Signature);
        assert_eq!(e.code, error_codes::UPGRADE_SIGNATURE_INVALID);
        assert!(e.message.contains("does not chain"), "{}", e.message);
    }

    /// Every check before it passes — the chain, the Rekor entry, the
    /// identity — so only the signature over the manifest refuses it. The
    /// Rekor entry's `hashedrekord` body does carry the signature
    /// (`spec.signature.content`) beside the manifest's SHA-256 and the
    /// signing certificate, but the Rekor check matches only the hash and
    /// the certificate, never the logged signature against
    /// `base64Signature`; and the Signed Entry Timestamp signs that body,
    /// which this edit leaves alone.
    #[tokio::test]
    async fn a_signature_that_does_not_cover_the_manifest_is_signature_invalid() {
        let bundle = edited_bundle(|b| flip_last_byte(&mut b["base64Signature"]));
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, offline_root()).await;
        assert_eq!(check, Check::Signature);
        assert_eq!(e.code, error_codes::UPGRADE_SIGNATURE_INVALID);
        assert!(
            e.message.starts_with("manifest signature does not verify"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn a_genuine_release_under_another_binarys_allowlist_is_identity_not_allowed() {
        let (check, e) = refuse(
            RELEASE_MANIFEST,
            RELEASE_BUNDLE,
            OTHER_SIGNERS,
            offline_root(),
        )
        .await;
        assert_eq!(check, Check::Identity);
        assert_eq!(e.code, error_codes::UPGRADE_IDENTITY_NOT_ALLOWED);
        assert!(e.message.contains("bilbycast-edge"), "{}", e.message);
    }

    #[tokio::test]
    async fn an_empty_allowlist_is_identity_not_allowed_before_any_fetch() {
        let (check, e) = refuse(RELEASE_MANIFEST, RELEASE_BUNDLE, &[], never_loaded()).await;
        assert_eq!(check, Check::Identity);
        assert_eq!(e.code, error_codes::UPGRADE_IDENTITY_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn a_bundle_that_does_not_parse_is_signature_invalid_before_any_fetch() {
        let (check, e) = refuse(
            RELEASE_MANIFEST,
            b"not-json",
            RELEASE_SIGNERS,
            never_loaded(),
        )
        .await;
        assert_eq!(check, Check::Signature);
        assert_eq!(e.code, error_codes::UPGRADE_SIGNATURE_INVALID);
    }

    #[tokio::test]
    async fn a_bundle_with_no_certificate_is_signature_invalid_before_any_fetch() {
        let bundle = edited_bundle(|b| {
            b.as_object_mut().unwrap().remove("cert");
        });
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, never_loaded()).await;
        assert_eq!(check, Check::Signature);
        assert_eq!(e.code, error_codes::UPGRADE_SIGNATURE_INVALID);
        assert!(
            e.message.starts_with("bundle missing 'cert'"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn a_bundle_with_no_signature_is_signature_invalid_before_any_fetch() {
        let bundle = edited_bundle(|b| {
            b.as_object_mut().unwrap().remove("base64Signature");
        });
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, never_loaded()).await;
        assert_eq!(check, Check::Signature);
        assert_eq!(e.code, error_codes::UPGRADE_SIGNATURE_INVALID);
        assert!(
            e.message.starts_with("bundle missing 'base64Signature'"),
            "{}",
            e.message
        );
    }

    /// The `cert` field still has cosign's shape, base64 of a PEM block, but
    /// the block's body is not base64. It is decoded only once the trust root
    /// has loaded, so this one needs the trust root.
    #[tokio::test]
    async fn a_certificate_that_does_not_decode_is_signature_invalid() {
        let bundle = edited_bundle(|b| {
            let pem = "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n";
            b["cert"] = Value::String(B64.encode(pem));
        });
        let (check, e) = refuse(RELEASE_MANIFEST, &bundle, RELEASE_SIGNERS, offline_root()).await;
        assert_eq!(check, Check::Signature);
        assert_eq!(e.code, error_codes::UPGRADE_SIGNATURE_INVALID);
        assert!(
            e.message.starts_with(
                "bundle 'cert' does not decode to a certificate: PEM body is not valid base64"
            ),
            "{}",
            e.message
        );
    }

    /// No step fails without naming its check, but an error that did would
    /// still be a refusal, and must fail closed as one.
    #[test]
    fn an_unclassified_failure_is_still_signature_invalid() {
        let e = map_verify_error(anyhow::anyhow!("unclassified"));
        assert_eq!(e.code, error_codes::UPGRADE_SIGNATURE_INVALID);
    }
}
