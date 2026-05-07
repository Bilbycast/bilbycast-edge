// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Compile-time trust roots for upgrade verification.
//!
//! Layered defence:
//!
//! 1. **Sigstore Fulcio CA root** — provided by the `sigstore` crate's
//!    `SigstoreTrustRoot`, refreshed via routine `cargo update`. The
//!    edge binary embeds whatever Fulcio root the crate version ships
//!    with at build time. Sigstore announces rotations well in
//!    advance; the bilbycast-edge release flow picks them up on its
//!    nightly cargo update.
//! 2. **Rekor public key** — same provenance, same crate.
//! 3. **Identity allowlist (this file)** — Bilbycast-specific. Pins
//!    which GitHub Actions workflow runs may produce a manifest that
//!    this binary will trust. The allowlist names the OIDC issuer
//!    (`token.actions.githubusercontent.com`), the source repo, the
//!    workflow path, and the ref pattern (release tags only). All
//!    four claims must match for a Sigstore-signed manifest to be
//!    accepted.
//!
//! Updates to this file land only when the release workflow path
//! itself changes — which is approximately never.

/// One row of the identity allowlist. Each field is matched against the
/// corresponding claim on the Fulcio-issued certificate's SAN extensions.
#[derive(Debug, Clone, Copy)]
pub struct AllowedSigner {
    /// Required `oidc-issuer` extension (OID 1.3.6.1.4.1.57264.1.1).
    pub issuer: &'static str,
    /// Required `source-repository-uri` ext (OID 1.3.6.1.4.1.57264.1.5).
    /// Matched as exact string equality.
    pub repo: &'static str,
    /// Required `source-repository-ref` ext (OID 1.3.6.1.4.1.57264.1.6).
    /// Glob-matched: `refs/tags/v*` accepts any release tag, `refs/tags/v0.45.*`
    /// would scope to the 0.45.x line. Today every entry is `refs/tags/v*`.
    pub ref_pattern: &'static str,
    /// Required `build-config-uri` / workflow path. Matched against the
    /// `Subject Alternative Name` URI claim
    /// `https://github.com/<repo>/<workflow>@<ref>`. Today this is the
    /// nightly-release workflow.
    pub workflow: &'static str,
}

/// The identity allowlist for the bilbycast-edge binary.
///
/// **Adding entries**: only when the release workflow path changes. This
/// list is the single source of truth for "who can mint a release that
/// this edge will install over the upgrade path." Treat changes as
/// security-critical — a typo here would silently widen the trust
/// boundary.
///
/// **Per-binary scoping**: each downstream binary (sidecars in Phase 3)
/// has its own copy of this list pinning its own repo + workflow path.
/// The shared `bilbycast-gateway-sdk::upgrade` machinery is parameterised
/// by `&'static [AllowedSigner]` so vendors can't accidentally accept
/// signatures from a sibling binary's release workflow.
pub const ALLOWED_SIGNERS: &[AllowedSigner] = &[
    AllowedSigner {
        issuer: "https://token.actions.githubusercontent.com",
        repo: "https://github.com/Bilbycast/bilbycast-edge",
        ref_pattern: "refs/tags/v*",
        workflow: "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/nightly-release.yml",
    },
];

/// Glob match used by the verifier when checking the cert's `ref` SAN
/// against the allowlist's `ref_pattern`. We deliberately do not pull in
/// a regex crate for this — `*` is the only wildcard the patterns use.
pub fn ref_matches(pattern: &str, actual: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        actual.starts_with(prefix)
    } else {
        pattern == actual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_matches_glob_suffix() {
        assert!(ref_matches("refs/tags/v*", "refs/tags/v0.45.4"));
        assert!(ref_matches("refs/tags/v*", "refs/tags/v1.0.0"));
        assert!(!ref_matches("refs/tags/v*", "refs/heads/main"));
        assert!(!ref_matches("refs/tags/v*", "refs/tags/foo"));
    }

    #[test]
    fn ref_matches_exact() {
        assert!(ref_matches("refs/tags/v0.45.4", "refs/tags/v0.45.4"));
        assert!(!ref_matches("refs/tags/v0.45.4", "refs/tags/v0.45.5"));
    }

    #[test]
    fn allowed_signers_constant_has_at_least_one_entry() {
        // Defensive: if someone empties the list it would silently allow
        // any signer (vacuous "no match required"). The verifier rejects
        // an empty allowlist, but pin the invariant here too.
        assert!(!ALLOWED_SIGNERS.is_empty());
    }

    #[test]
    fn allowed_signers_repo_is_https_github() {
        for s in ALLOWED_SIGNERS {
            assert!(s.repo.starts_with("https://github.com/Bilbycast/"));
            assert!(s.workflow.starts_with(s.repo));
            assert!(s.issuer.starts_with("https://token.actions.githubusercontent.com"));
        }
    }
}
