# Security model

This document is the trust model for bilbycast-edge as a whole, with particular focus on the remote-upgrade pipeline that lets a customer's manager install new versions of the edge binary on a node.

The operator-facing runbook for the upgrade pipeline lives in [`docs/upgrade.md`](upgrade.md). This document explains *why* the design is safe.

## Trust model in one paragraph

Each customer runs their own manager. The manager has authority over **scheduling** an upgrade (which node, when, which version), but never over **what code runs**. Bilbycast (Softside Tech) is the sole publisher of edge / sidecar binaries. The manager only ever names a `version` + `channel`. Every release is **Sigstore-keyless-signed** by the GitHub Actions workflow run that produced it: an ephemeral keypair signs `manifest.json`; an X.509 certificate from Sigstore's Fulcio CA binds that signature to the specific GitHub workflow run (`Bilbycast/<repo>` on a release tag); the signing event is logged in the public Rekor transparency log. Edges fetch the manifest + signature bundle, verify the certificate identity against a hardcoded allowlist, verify the Rekor entry, then trust the SHA-256 inside before downloading the tarball. **No long-lived signing key exists** — there is nothing to lose, steal, or rotate. Customers hold no key. A compromised customer manager can only tell edges to install a real, Sigstore-signed Bilbycast release; arbitrary code is not reachable. Any unauthorised release would leave a permanent, public, timestamped record in Rekor tied to the workflow run that produced it.

## Trust roots

The edge embeds three pieces of trust at compile time:

1. **Sigstore Fulcio CA root certificate** — provided by the `sigstore` Rust crate as a constant. Refreshed via routine `cargo update`. Rotates rarely (years).
2. **Rekor public key** — same provenance, same crate.
3. **`ALLOWED_SIGNERS` identity allowlist** — Bilbycast-specific. Lives in [`src/upgrade/trust.rs`](../src/upgrade/trust.rs). Pins the OIDC issuer (`token.actions.githubusercontent.com`), the source repo (`Bilbycast/bilbycast-edge`), the workflow path (`.github/workflows/nightly-release.yml`), and the ref pattern (`refs/tags/v*`).

Updates to the identity allowlist land only when the release workflow path itself changes — essentially never.

## Threat model

| Threat | Defence |
|---|---|
| Compromised manager force-installs malicious code | Edge constructs URL deterministically; only `github.com/Bilbycast/*` accepted. Manager has no input on URL or hash. Manifest must verify under Sigstore (Fulcio cert + Rekor inclusion + identity allowlist) before any tarball is fetched. |
| Compromised manager force-installs known-vulnerable old release | Version monotonicity (`current - rollback_grace`, `min_version`) **and** manifest `sequence` must exceed the currently-installed one. |
| Compromised manager force-installs wrong channel | `allowed_channels` config; manifest carries `channel` and edge cross-checks. |
| MITM on download | TLS to github.com with system roots (rustls + webpki-roots). Even with TLS broken, manifest signature would still need to verify against Sigstore. |
| Tampered tarball at rest on GitHub | SHA-256 inside the signed manifest must match the downloaded tarball. Tampering invalidates the signature. |
| Stolen GitHub release PAT / rogue org admin overwrites a release | Attacker can upload bytes but cannot produce a valid Sigstore signature without running a workflow on the Bilbycast repo. If they manage that, the workflow run + signing event is publicly logged in Rekor and tied to a specific commit / tag — forensically detectable, not silent. |
| Workflow run hijack (e.g. malicious PR triggering the release workflow) | Release workflow gated on tag pushes (`on: push: tags: ['v*']`) plus `workflow_dispatch` restricted to maintainers via GitHub `environments`. PR-triggered runs cannot mint releases. |
| Build pipeline poisoned (legitimate signing of evil bytes via tampered source) | Out of scope at the upgrade-protocol layer — addressed by branch protection on `main`, required reviews, signed commits, restricted release-trigger workflows. The Rekor entry still records *which commit* was built, so post-incident attribution is exact. |
| Sigstore Fulcio root or Rekor pubkey compromised / rotated | Edge ships with constants for current Fulcio + Rekor roots (provided by the `sigstore` crate, updated via routine dependency bumps). New edge release carries new constants. Standard rotation path; Sigstore announces well in advance. |
| Crash-loop after upgrade | Boot watchdog flips symlink back to `previous` after `max_boot_attempts` failed boots. |
| Power loss mid-stage | Atomic symlink rename + state.json fsync; partial extraction lives under `versions/<new>.partial/` and is GC'd on next boot. |
| Operator wants high-value site to refuse all auto-upgrades | `upgrades.enabled = false` or `upgrades.manual_only = true` (manager can stage but local operator must `kill -USR1` to apply). |
| Detect unauthorised releases after the fact | Optional Rekor monitor: a small scheduled workflow polls Rekor for any signatures by Bilbycast workflow identities, cross-references against authorised tags, and pages on mismatch. Public log makes this a one-shot cron. |

## Verifying a release manually

A third-party security auditor can independently verify any bilbycast-edge release using only public information — no insider knowledge required:

```bash
curl -fsSL -O https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/manifest.json
curl -fsSL -O https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/manifest.sig.bundle

cosign verify-blob \
    --bundle manifest.sig.bundle \
    --certificate-identity-regexp 'https://github\.com/Bilbycast/bilbycast-edge/\.github/workflows/nightly-release\.yml@refs/tags/v.*' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    manifest.json
```

A successful verification proves:
* The manifest bytes were signed by a workflow run on the `Bilbycast/bilbycast-edge` repo.
* The signing workflow was the `nightly-release.yml` workflow.
* The signing was triggered by a release tag (`refs/tags/v*`).
* The signing event is recorded in Rekor — there is a permanent, timestamped public record of when the manifest was minted and which commit produced it.

This is the same verification the edge runs internally before installing any release.
