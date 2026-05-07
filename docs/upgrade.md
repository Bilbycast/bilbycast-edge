# Remote upgrade

bilbycast-edge supports manager-driven remote upgrades. A customer's manager can push a new version to any opted-in edge node, and the edge will:

1. Validate the request against operator-controlled policy (channel, version floor, rollback grace).
2. Fetch a Sigstore-signed manifest from the hardcoded GitHub release URL and verify the keyless signature against a compiled-in identity allowlist.
3. Download the matching tarball with streaming SHA-256 verification.
4. Atomically swap the `current` symlink under `/opt/bilbycast/edge/`.
5. Drain in-flight flows and exit for systemd respawn.
6. If the new binary fails to authenticate to the manager within the configured health window, automatically roll the symlink back to the previous version.

The trust model is described in [docs/security.md](security.md). This document is both the architecture reference and the operator runbook — start with [How it works](#how-it-works) for the design overview, then jump to [Configuration](#configuration) and [Triggering an upgrade](#triggering-an-upgrade) for day-to-day operations.

## How it works

### Three actors, three trust levels

| Actor | What it can do | What it can't do |
|---|---|---|
| **Bilbycast (publisher)** | Mint releases by running the GitHub Actions workflow on `Bilbycast/bilbycast-edge`. Holds **no long-lived signing key**. | Sign anything outside a workflow run on the pinned repo + workflow path. |
| **Customer's manager** | Schedule an upgrade by naming a `(version, channel)` tuple. | Influence URL, hash, or bytes. Forge a Sigstore signature. Reach inside the trust roots compiled into the edge. |
| **The edge** | Decide whether to install. Verify Sigstore signature against compiled-in trust roots. Atomically swap symlink. Roll back on failed boot. | Install anything that isn't a real Bilbycast release matching the compiled-in identity allowlist. |

### Three trust roots compiled into the edge

| Root | Source | Rotation |
|---|---|---|
| **Sigstore Fulcio CA root cert** | `sigstore` Rust crate | Routine `cargo update` — rotates rarely (years). Sigstore announces in advance. |
| **Rekor public key** | `sigstore` Rust crate | Same. |
| **`ALLOWED_SIGNERS` identity allowlist** | [`src/upgrade/trust.rs`](../src/upgrade/trust.rs) | Only when the release workflow path itself changes (essentially never). |

The allowlist pins four claims that **all** must match for a Sigstore-signed manifest to be accepted:

```rust
AllowedSigner {
    issuer: "https://token.actions.githubusercontent.com",
    repo:   "https://github.com/Bilbycast/bilbycast-edge",
    ref_pattern: "refs/tags/v*",
    workflow:    "https://github.com/Bilbycast/bilbycast-edge/.github/workflows/nightly-release.yml",
}
```

### Release-time flow (what happens when CI tags a release)

```text
GitHub Actions workflow run
   │
   ├── build per-(arch, variant) tarballs + .sha256 sidecars
   ├── scripts/build-manifest.sh emits canonical manifest.json
   │     {
   │       "version": "0.45.4", "device_type": "edge", "channel": "stable",
   │       "released_at": "...", "sequence": 142,
   │       "artefacts": [
   │         {"arch": "x86_64-linux", "variant": "full", "url": "...", "sha256": "..."},
   │         ...
   │       ]
   │     }
   │
   ├── cosign sign-blob --yes --bundle manifest.sig.bundle manifest.json
   │     1. workflow run requests an OIDC token from GitHub Actions
   │     2. Fulcio mints a 10-minute X.509 cert against that identity
   │     3. ephemeral keypair signs manifest.json bytes
   │     4. signing event uploaded to Rekor (public log)
   │     5. ephemeral keypair thrown away
   │
   ├── cosign verify-blob (paranoid local re-check against the production
   │     identity allowlist — catches a typo here, not in production)
   │
   └── publish manifest.json + manifest.sig.bundle alongside the tarballs
         on the GitHub release.
```

**No long-lived signing key exists.** Nothing to lose, steal, or rotate. Every signature is permanently recorded in Rekor against the specific commit and tag that produced it.

### Upgrade-time flow (what happens when an operator clicks Upgrade)

```text
Manager UI → POST /api/v1/nodes/{id}/upgrade { version, channel }
                      │
                      ▼
Manager forwards via existing WS:  command { type: "upgrade_binary", version, channel }
                      │
                      ▼
Edge UpgradeCoordinator (src/upgrade/mod.rs)
    │
    ├── 1. Single-flight guard. Second concurrent request → upgrade_in_progress.
    │
    ├── 2. Policy gate against local UpgradeConfig:
    │        - upgrades.enabled == true
    │        - channel ∈ allowed_channels
    │        - version ≥ min_version
    │        - target.minor ≥ current.minor − rollback_grace
    │        - same major version (no auto-bump across major boundaries)
    │
    ├── 3. Construct release URL deterministically:
    │        https://github.com/Bilbycast/bilbycast-edge/releases/download/v<version>/...
    │      Manager has zero input on URL or hash. URL host validated against
    │      ALLOWED_URL_HOSTS = ["github.com", "objects.githubusercontent.com"].
    │
    ├── 4. Fetch manifest.json + manifest.sig.bundle (capped at 1 MiB each,
    │      reqwest streaming + system rustls roots, retries with backoff).
    │
    ├── 5. Verify Sigstore bundle (src/upgrade/verify.rs):
    │        a. parse cosign bundle JSON
    │        b. cert chains to compiled-in Fulcio root
    │        c. cert SAN identity claims (issuer, repo, ref, workflow)
    │           match an entry in ALLOWED_SIGNERS
    │        d. Rekor inclusion proof valid against compiled-in Rekor pubkey;
    │           integratedTime within cert validity window
    │        e. ECDSA-P256 signature over manifest bytes verifies under
    │           the cert's pubkey
    │      Failure ⇒ upgrade_signature_invalid / upgrade_identity_not_allowed /
    │      upgrade_rekor_invalid Critical event, abort.
    │
    ├── 6. Cross-check verified manifest:
    │        - manifest.version == requested version
    │        - manifest.channel == requested channel
    │        - manifest.device_type == "edge"
    │        - manifest.sequence > state.last_sequence  (replay defence)
    │        - artefact for (host arch, variant) exists
    │
    ├── 7. Stream-download tarball (capped at 512 MiB), accumulate
    │      streaming SHA-256, must match value inside the verified
    │      manifest. Mismatch ⇒ upgrade_checksum_mismatch.
    │
    ├── 8. Extract to versions/<new>.partial/  (so a crash leaves an
    │      obvious orphan that the GC sweep deletes on next boot).
    │      Tar entries with absolute paths or `..` segments rejected.
    │
    ├── 9. Hoist binary to versions/<new>.partial/<binary_name>,
    │      chmod 0755, fsync the directory.
    │
    ├── 10. Atomic step A: rename(2) versions/<new>.partial/ → versions/<new>/
    │
    ├── 11. Atomic step B: rotate old `current` symlink target into `previous`,
    │       then ln -s versions/<new>/ current.tmp; rename(2) current.tmp → current.
    │       Kernel guarantees rename(2) atomicity on the same filesystem.
    │
    ├── 12. Persist state.json under flock(2):
    │         {
    │           "current_version": "<new>", "previous_version": "<old>",
    │           "channel": "...", "variant": "...", "arch": "...",
    │           "status": "pending_health", "boot_attempts": 0,
    │           "staged_at": "...", "last_sequence": <seq>
    │         }
    │       Tempfile + fsync + atomic rename, parent dir fsync'd after.
    │
    └── 13. Emit upgrade_staged event (info), drain flows for 5 s, exit(0).
            systemd respawns into versions/<new>/bilbycast-edge via the
            current symlink.
```

### Boot watchdog (runs inside the new binary, before any other init)

```text
read state.json
   │
   ├── status == stable          → return Continue (normal boot path)
   ├── status == staged_manual   → return Continue (operator drives swap via SIGUSR1)
   │
   ├── status == rolled_back     → emit upgrade_rolled_back Critical
   │                                set status = stable
   │                                return RolledBack(from, to)
   │                                continue boot on previous version
   │
   └── status == pending_health  → bump boot_attempts (atomic write under flock)
                                    │
                                    ├── boot_attempts ≤ max_boot_attempts (3)
                                    │     return PendingHealth { attempt }
                                    │     continue boot on new version
                                    │
                                    └── boot_attempts > max_boot_attempts
                                          revert symlink to `previous`
                                          set status = rolled_back
                                          emit upgrade_rolled_back Critical
                                          exit(1) → systemd respawns into old binary
```

A periodic watchdog task (every 5 s, see `src/upgrade/watchdog.rs::run_watchdog_periodic`) promotes `pending_health → stable` once:

* the new binary has been alive for `boot_health_window_secs` seconds since `staged_at`, **and**
* the most recent manager auth (`record_healthy_beat` is called from the WS auth handler) is < 60 s ago.

When both conditions hold the watchdog flips the status to `stable` and emits `upgrade_completed` (info).

### Why each failure mode can't break the node

| Concern | Defence |
|---|---|
| Compromised manager force-installs malicious code | Edge constructs URL deterministically from `(version, channel)`. Manager has no input on URL or hash. Manifest must verify against compiled-in trust roots before any tarball is fetched. Worst a bad manager can do: install a real Bilbycast release. |
| Compromised manager forces vulnerable old release | `min_version` + `rollback_grace` + `manifest.sequence > state.last_sequence` reject it. |
| Compromised manager forces wrong channel / variant | `allowed_channels` config; manifest carries `channel`; per-(arch, variant) artefact lookup must succeed. |
| MITM on download | TLS to GitHub via system rustls roots **plus** the Sigstore signature check — both layers must break. |
| Tampered tarball at rest on GitHub | SHA-256 inside the signed manifest must match the streamed digest. Tampering invalidates the signature. |
| Stolen GitHub PAT / rogue org admin overwrites a release | Bytes can be uploaded but no valid Sigstore signature can be produced without running a workflow on `Bilbycast/bilbycast-edge`. If that happens, the signing event is permanently logged in Rekor against a specific commit + tag — forensically detectable, never silent. |
| Workflow run hijack (e.g. malicious PR triggering the release workflow) | Workflow gated on `on: push: tags: ['v*']` plus `workflow_dispatch` restricted to maintainers via GitHub `environments`. PR-triggered runs cannot reach the signing step. |
| Sigstore Fulcio root or Rekor pubkey compromised / rotated | Edge ships with constants from the `sigstore` crate; routine `cargo update` picks up rotations. Standard rotation path; Sigstore announces well in advance. |
| Crash-loop after upgrade | Boot watchdog reverts after `max_boot_attempts` (default 3) failed boots. |
| New binary boots but never authenticates to manager | Periodic watchdog times out at `boot_health_window_secs` (default 120 s); state stays `pending_health`, manager UI surfaces it, operator can manually roll back. |
| Power loss mid-stage | Atomic `rename(2)` for partial → final and current.tmp → current. fsync'd state.json + parent dir. Partial extractions live under `versions/<new>.partial/` and are GC'd on next boot. |
| Two `upgrade_binary` commands in flight | In-process `Mutex<bool>` single-flight guard; on-disk `flock(2)` on state.json. Second command returns `upgrade_in_progress`. |
| Operator wants high-value site to refuse all auto-upgrades | `upgrades.enabled = false` (rejects every command) **or** `upgrades.manual_only = true` (manager can stage; operator must `kill -USR1` to apply). |
| Detect unauthorised releases after the fact | Optional Rekor monitor (deferred): scheduled job polls Rekor for any signatures by Bilbycast workflow identities, cross-references against authorised tags. Public log makes this a one-shot cron. |

### Manifest schema (canonical JSON)

```json
{
  "version": "0.45.4",
  "device_type": "edge",
  "channel": "stable",
  "released_at": "2026-05-06T12:00:00Z",
  "sequence": 142,
  "artefacts": [
    {
      "arch": "x86_64-linux",
      "variant": "default",
      "url": "https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/bilbycast-edge-x86_64-linux.tar.gz",
      "sha256": "abc..."
    },
    {
      "arch": "x86_64-linux",
      "variant": "full",
      "url": "https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/bilbycast-edge-x86_64-linux-full.tar.gz",
      "sha256": "def..."
    }
  ]
}
```

`sequence` is monotonic per `device_type` (today the GitHub Actions `run_number`). It defends against rollback / replay even when semver alone would otherwise allow a downgrade. The Sigstore bundle binds these manifest bytes to the workflow run that produced them; identity comes from the Fulcio cert, not from any field inside the manifest.

### Module layout (where the code lives)

| File | Responsibility |
|---|---|
| `src/upgrade/mod.rs` | `UpgradeCoordinator` lifecycle, single-flight guard, `error_codes` constants, global handle for the manager dispatch arm. |
| `src/upgrade/manifest.rs` | Manifest schema, URL host whitelist, `derive_release_base_url`, `check_version_window`. |
| `src/upgrade/verify.rs` | Sigstore bundle verify — Fulcio cert chain + identity allowlist + Rekor inclusion proof + ECDSA-P256 signature over manifest. |
| `src/upgrade/trust.rs` | Compile-time `ALLOWED_SIGNERS` allowlist + `ref_matches` glob helper. |
| `src/upgrade/download.rs` | `reqwest`-based fetch with strict TLS + `https_only`, streaming SHA-256, retry with backoff (3 attempts, 250 ms / 500 ms / 1 s). Manifest cap 1 MiB, tarball cap 512 MiB. |
| `src/upgrade/apply.rs` | Tarball extraction (with `..` segment + absolute-path rejection), atomic symlink swap, partial-dir GC. |
| `src/upgrade/state.rs` | `state.json` schema + read/write under `flock(2)`, `bump_boot_attempts`, atomic temp-file + rename + fsync. |
| `src/upgrade/watchdog.rs` | Boot watchdog (called before any other init in `main.rs`), periodic `pending_health → stable` finalizer, manual-apply hook. |

The single-flight guard is process-wide via `std::sync::OnceLock<UpgradeCoordinator>` installed in `main.rs` after config load. The manager dispatch arm in `manager/client.rs` reads it via `crate::upgrade::global_coordinator()`.

## On-disk layout

The upgrade machinery owns `/opt/bilbycast/edge/` (configurable via `upgrades.install_root`):

```text
/opt/bilbycast/edge/
├── current      → versions/0.45.4/        # atomic-swap target — systemd ExecStart
├── previous     → versions/0.45.3/        # populated after first upgrade
├── versions/
│   ├── 0.45.3/bilbycast-edge
│   └── 0.45.4/bilbycast-edge
├── state.json                             # status, boot_attempts, last_health_at, last_sequence
├── config.json                            # operational config (manager-managed)
└── secrets.json                           # 0600, machine-key encrypted
```

The systemd unit's `ExecStart` resolves through the `current` symlink, so a successful staged upgrade lands the moment the old binary exits.

## Configuration

Add an `upgrades` block to `config.json`:

```json
{
  "upgrades": {
    "enabled": true,
    "allowed_channels": ["stable"],
    "min_version": "0.40.0",
    "rollback_grace": 1,
    "install_root": "/opt/bilbycast/edge",
    "boot_health_window_secs": 120,
    "max_boot_attempts": 3,
    "manual_only": false
  }
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. Off by default — operators opt in per node. |
| `allowed_channels` | `[string]` | `["stable"]` | Channels this node will install. `nightly` and `beta` are valid testbed channels. |
| `min_version` | `string?` | unset | Optional version floor. Manager pushes below this get rejected with `upgrade_version_too_old`. |
| `rollback_grace` | u32 | `1` | How many minor versions back the edge will roll. `1` lets a stable→stable patch roll-forward but blocks an attacker forcing a known-vulnerable old release. |
| `install_root` | path | `/opt/bilbycast/edge` | Where `versions/`, `current`, `previous`, and `state.json` live. Must be absolute. |
| `boot_health_window_secs` | u32 | `120` | After respawning into the new binary, the edge has this long to authenticate to the manager. Failure rolls the symlink back. |
| `max_boot_attempts` | u32 | `3` | Maximum boot loops on the new binary before automatic rollback. |
| `manual_only` | bool | `false` | Stage-without-apply mode for high-value sites. Manager can stage; the actual symlink swap waits for a local `kill -USR1 <pid>` on the running edge. |

Validation lives in `src/config/validation.rs::validate_upgrade_config`.

## Triggering an upgrade

From the manager UI — **Node detail → Upgrade** button (Phase 2). Manager-side surface gated on the `"upgrade"` capability bit, so older edges never see the control.

From a manager API call:

```
POST /api/v1/nodes/{id}/upgrade
{ "version": "0.45.4", "channel": "stable" }
```

The manager sends `command { type: "upgrade_binary", version, channel }` to the edge over WS, and forwards `command_ack` back to the caller. The ack carries one of:

* `success: true, data: { status: "staged", from_version, to_version, channel, variant, arch }` — staging completed, the edge is about to exit. The new binary will respawn within seconds.
* `success: false, error_code: <code>` — see the [event reference](events-and-alarms.md#remote-upgrade-upgrade) for the full list of failure modes.

Subsequent state changes flow through manager events:

| Event | Severity | Meaning |
|---|---|---|
| `upgrade_started` | info | Edge accepted the command, staging begins |
| `upgrade_downloaded` | info | Manifest verified, tarball SHA-256 matched |
| `upgrade_staged` | info | Symlink swapped — edge is draining for respawn |
| `upgrade_completed` | info | New binary stable for `boot_health_window_secs` of healthy beats |
| `upgrade_rolled_back` | critical | Boot watchdog reverted to previous version |

## Manual rollback

If the boot watchdog didn't fire (e.g. operator misconfiguration that the edge accepts but the *flows* don't), an operator can roll back manually:

```bash
sudo systemctl stop bilbycast-edge
sudo ln -sfn /opt/bilbycast/edge/versions/<old-version>/ /opt/bilbycast/edge/current.tmp
sudo mv -Tf /opt/bilbycast/edge/current.tmp /opt/bilbycast/edge/current
# Update state.json so the watchdog doesn't fight us next boot:
sudo jq '.current_version = "<old-version>" | .status = "stable" | .boot_attempts = 0' \
    /opt/bilbycast/edge/state.json > /tmp/state.json && \
    sudo mv /tmp/state.json /opt/bilbycast/edge/state.json
sudo systemctl start bilbycast-edge
```

## Disabling upgrades on a high-value site

Two options:

* **Soft disable**: `upgrades.enabled = false` in `config.json`. The edge rejects every `upgrade_binary` with `error_code: upgrade_disabled`.
* **Stage-but-don't-apply**: `upgrades.manual_only = true`. The manager can stage the new version onto disk; an operator must `kill -USR1 <pid>` on the running edge to actually swap the symlink and exit.

Either way, the edge's compiled-in `ALLOWED_SIGNERS` allowlist still gates which signed releases it would accept — a compromised manager cannot install attacker-controlled bytes regardless of these flags.

## Verifying a release manually

A third-party security auditor can independently verify any bilbycast-edge release using only public information:

```bash
# Fetch manifest + bundle for the tag you want to audit.
curl -fsSL -O https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/manifest.json
curl -fsSL -O https://github.com/Bilbycast/bilbycast-edge/releases/download/v0.45.4/manifest.sig.bundle

# Verify with the same identity policy the edge enforces.
cosign verify-blob \
    --bundle manifest.sig.bundle \
    --certificate-identity-regexp 'https://github\.com/Bilbycast/bilbycast-edge/\.github/workflows/nightly-release\.yml@refs/tags/v.*' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    manifest.json

# Cross-check the tarball you have on disk against the verified manifest.
jq -r --arg arch "x86_64-linux" --arg variant "full" \
    '.artefacts[] | select(.arch == $arch and .variant == $variant) | .sha256' \
    manifest.json
sha256sum bilbycast-edge-x86_64-linux-full.tar.gz
```

## Troubleshooting

* **`journalctl -u bilbycast-edge`** — service-level logs.
* **`/opt/bilbycast/edge/state.json`** — current upgrade state (status, boot_attempts, last_sequence).
* **Manager events page** — full lifecycle including the `error_code` for any failed staging attempt.
* **`cat /opt/bilbycast/edge/current` (resolved via `readlink -f`)** — confirms which version `current` points at.

If `state.json.status == "pending_health"` for longer than `boot_health_window_secs`, the edge is up but isn't authenticating to the manager. Check the manager URL, registration token / node secret, and TLS configuration.
