// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Release manifest schema + URL whitelist.
//!
//! The manifest binds `(version, channel)` to a list of `(arch, variant,
//! url, sha256)` artefacts. It is signed once by the release workflow run
//! that produced it (Sigstore keyless — see `verify.rs` and `trust.rs`),
//! so a compromised manager cannot point edges at attacker-controlled
//! bytes: even if a malicious manager forged a manifest URL, the
//! signature verification in `verify::verify_manifest_bundle` would
//! reject it.
//!
//! ### URL host whitelist
//!
//! Manifest + tarball URLs are constructed deterministically by the edge
//! from `(version, channel)` and validated against [`ALLOWED_URL_HOSTS`]
//! before any TCP connection. A manifest carrying `artefacts[].url` with
//! a host outside the whitelist is rejected with
//! `error_code = "upgrade_url_invalid"`. This is defence-in-depth on top
//! of the signature: even a Sigstore-signed manifest cannot redirect the
//! download outside `github.com`.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::models::UpgradeConfig;

/// Hosts allowed to serve manifests / tarballs.
///
/// In practice this is exactly `github.com` — release assets land at
/// `github.com/Bilbycast/<repo>/releases/download/v<version>/<asset>`.
/// We keep the list explicit (no globs) so a typo in the manifest can't
/// silently widen the trust boundary.
pub const ALLOWED_URL_HOSTS: &[&str] = &["github.com", "objects.githubusercontent.com"];

/// Repos the edge is willing to fetch upgrades from. Pairs with the
/// per-binary `ALLOWED_SIGNERS` allowlist in `trust.rs` — mismatch on
/// either layer rejects the upgrade.
pub const ALLOWED_REPOS: &[&str] = &["Bilbycast/bilbycast-edge"];

/// Top-level release manifest. Canonical JSON written by
/// `scripts/build-manifest.sh` in the release pipeline; verified
/// byte-for-byte against the Sigstore bundle before parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub device_type: String,
    pub channel: String,
    pub released_at: String,
    /// Monotonic counter incremented per release per `device_type`.
    /// Defends against rollback / replay even when the semver comparison
    /// would otherwise allow it.
    pub sequence: u64,
    pub artefacts: Vec<ManifestArtefact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestArtefact {
    pub arch: String,
    pub variant: String,
    pub url: String,
    pub sha256: String,
}

impl Manifest {
    /// Cross-check the manifest against the operator's request and the
    /// edge's compiled-in expectations.
    pub fn validate_request(
        &self,
        requested_version: &str,
        requested_channel: &str,
        expected_device_type: &str,
    ) -> Result<()> {
        if self.version != requested_version {
            bail!(
                "manifest version mismatch: requested {requested_version}, got {}",
                self.version
            );
        }
        if self.channel != requested_channel {
            bail!(
                "manifest channel mismatch: requested {requested_channel}, got {}",
                self.channel
            );
        }
        if self.device_type != expected_device_type {
            bail!(
                "manifest device_type mismatch: expected {expected_device_type}, got {}",
                self.device_type
            );
        }
        if self.artefacts.is_empty() {
            bail!("manifest has no artefacts");
        }
        for a in &self.artefacts {
            validate_url_host(&a.url)?;
            if a.sha256.len() != 64 || !a.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!(
                    "manifest artefact for arch={} has malformed sha256 {:?}",
                    a.arch, a.sha256
                );
            }
            if a.arch.is_empty() || a.arch.len() > 64 {
                bail!("manifest artefact arch is empty or too long");
            }
            if a.variant.is_empty() || a.variant.len() > 32 {
                bail!("manifest artefact variant is empty or too long");
            }
        }
        Ok(())
    }

    /// Pick the artefact matching `(arch, variant)`. Returns `None` if no
    /// row matches — the dispatcher surfaces `upgrade_arch_mismatch`.
    pub fn pick_artefact(&self, arch: &str, variant: &str) -> Option<&ManifestArtefact> {
        self.artefacts
            .iter()
            .find(|a| a.arch == arch && a.variant == variant)
    }
}

/// Reject any URL whose host is not in [`ALLOWED_URL_HOSTS`]. Also
/// rejects schemes other than `https://` because the entire trust path
/// assumes TLS (system roots) underneath the Sigstore signature check.
pub fn validate_url_host(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| anyhow!("URL {url:?} is not a valid URL: {e}"))?;
    if parsed.scheme() != "https" {
        bail!("URL {url:?} must use https://");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL {url:?} has no host"))?;
    if !ALLOWED_URL_HOSTS.iter().any(|h| host == *h) {
        bail!(
            "URL {url:?} host {host:?} is not in the upgrade host whitelist {:?}",
            ALLOWED_URL_HOSTS
        );
    }
    if url.len() > 2048 {
        bail!("URL {url:?} exceeds 2048 chars");
    }
    Ok(())
}

/// Construct the deterministic GitHub release base URL for `(version,
/// channel)`. Today every release carries the same `manifest.json` +
/// `manifest.sig.bundle` per tag — channel is encoded inside the
/// manifest, not the URL — so this is the only host the edge ever talks
/// to during staging.
pub fn derive_release_base_url(version: &str) -> Result<String> {
    let v = semver::Version::parse(version).map_err(|e| {
        anyhow!("requested version {version:?} is not valid semver: {e}")
    })?;
    let url = format!(
        "https://github.com/{repo}/releases/download/v{v}",
        repo = ALLOWED_REPOS[0],
        v = v
    );
    validate_url_host(&url)?;
    Ok(url)
}

/// Enforce `target_v` is acceptable given the operator's policy.
///
/// Rules:
/// * `target_v >= cfg.min_version` (when set).
/// * `target_v.minor + cfg.rollback_grace >= current_v.minor` — i.e. the
///   edge will move *forward* freely but only roll back at most
///   `rollback_grace` minor versions. This blocks a compromised manager
///   from forcing a known-vulnerable old release.
/// * `target_v` cannot be the exact current version (no-op upgrade is a
///   misconfiguration we'd rather surface than silently complete).
pub fn check_version_window(
    target_v: &semver::Version,
    current_v: &semver::Version,
    cfg: &UpgradeConfig,
) -> Result<()> {
    if let Some(ref min_str) = cfg.min_version {
        let min = semver::Version::parse(min_str)
            .map_err(|e| anyhow!("min_version {min_str:?} is not valid semver: {e}"))?;
        if *target_v < min {
            bail!(
                "requested {target_v} < min_version {min} configured on this node"
            );
        }
    }
    if target_v == current_v {
        bail!("requested {target_v} matches the currently installed version");
    }
    // Major-bump policy: refuse silently — operators upgrade major
    // versions explicitly via a fresh install. Forward minor / patch
    // bumps are unconditionally allowed (subject to min_version).
    if target_v.major != current_v.major {
        bail!(
            "requested {target_v} crosses a major version boundary from {current_v} \
             — major upgrades require a fresh install, not the auto-upgrader"
        );
    }
    // Backwards step: bound by rollback_grace.
    if target_v.minor + u64::from(cfg.rollback_grace) < current_v.minor {
        bail!(
            "requested {target_v} is more than {} minor versions behind current {current_v} \
             (rollback_grace = {})",
            cfg.rollback_grace, cfg.rollback_grace
        );
    }
    if target_v.minor == current_v.minor && target_v.patch < current_v.patch {
        // Patch-level rollback is allowed — patches are bug-fix only and
        // we already checked min_version above. Operators wanting to
        // forbid downgrades within a minor set min_version explicitly.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> UpgradeConfig {
        UpgradeConfig::default()
    }

    #[test]
    fn url_host_whitelist_accepts_github() {
        validate_url_host(
            "https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/foo.tar.gz",
        )
        .unwrap();
    }

    #[test]
    fn url_host_whitelist_rejects_other_hosts() {
        assert!(validate_url_host("https://evil.com/foo.tar.gz").is_err());
        assert!(validate_url_host("http://github.com/foo").is_err());
        assert!(validate_url_host("file:///etc/passwd").is_err());
    }

    #[test]
    fn version_window_blocks_rollback_beyond_grace() {
        let mut c = cfg();
        c.rollback_grace = 1;
        let target = semver::Version::parse("0.42.0").unwrap();
        let current = semver::Version::parse("0.45.0").unwrap();
        let err = check_version_window(&target, &current, &c).unwrap_err();
        assert!(err.to_string().contains("rollback_grace"));
    }

    #[test]
    fn version_window_allows_one_minor_back_with_grace_1() {
        let c = cfg();
        let target = semver::Version::parse("0.44.0").unwrap();
        let current = semver::Version::parse("0.45.0").unwrap();
        check_version_window(&target, &current, &c).unwrap();
    }

    #[test]
    fn version_window_blocks_major_bump() {
        let c = cfg();
        let target = semver::Version::parse("1.0.0").unwrap();
        let current = semver::Version::parse("0.45.0").unwrap();
        let err = check_version_window(&target, &current, &c).unwrap_err();
        assert!(err.to_string().contains("major"));
    }

    #[test]
    fn version_window_blocks_min_version() {
        let mut c = cfg();
        c.min_version = Some("0.45.0".to_string());
        let target = semver::Version::parse("0.44.0").unwrap();
        let current = semver::Version::parse("0.45.0").unwrap();
        let err = check_version_window(&target, &current, &c).unwrap_err();
        assert!(err.to_string().contains("min_version"));
    }

    #[test]
    fn manifest_request_mismatch_rejected() {
        let m = Manifest {
            version: "0.45.4".into(),
            device_type: "edge".into(),
            channel: "stable".into(),
            released_at: "2026-05-06T12:00:00Z".into(),
            sequence: 1,
            artefacts: vec![ManifestArtefact {
                arch: "x86_64-linux".into(),
                variant: "default".into(),
                url: "https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/foo.tar.gz".into(),
                sha256: "0".repeat(64),
            }],
        };
        assert!(m.validate_request("0.45.5", "stable", "edge").is_err());
        assert!(m.validate_request("0.45.4", "nightly", "edge").is_err());
        assert!(m.validate_request("0.45.4", "stable", "relay").is_err());
        m.validate_request("0.45.4", "stable", "edge").unwrap();
    }
}
