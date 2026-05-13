#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# install-edge.sh — single-shot installer for bilbycast-edge.
#
# Operator usage on a fresh node:
#
#   curl -fsSL https://github.com/Bilbycast/bilbycast-edge/releases/latest/download/install-edge.sh \\
#     | sudo bash -s -- \\
#         --manager wss://manager.example.com:8443 \\
#         --registration-token <token-from-manager-ui> \\
#         [--channel stable|nightly|beta] \\
#         [--variant default|full]
#
# What the script does:
#   1. Detects the host arch (x86_64-linux / aarch64-linux).
#   2. Downloads `manifest.json` + `manifest.sig.bundle` from the
#      configured channel's GitHub release.
#   3. Verifies the Sigstore signature with cosign (installs cosign if
#      missing, with its own checksum verified against the upstream
#      release page). The verify pins the same identity allowlist the
#      edge enforces — `Bilbycast/bilbycast-edge` repo, the
#      nightly-release workflow path, and the `refs/tags/v*` ref pattern.
#   4. Reads the matching artefact's SHA-256 from the verified manifest,
#      downloads the tarball, verifies the hash.
#   5. Creates the `bilbycast` system user/group via systemd-sysusers
#      or useradd.
#   6. Lays out `/opt/bilbycast/edge/{versions/<v>/, current → versions/<v>,
#      state.json, secrets.json}`.
#   7. Writes the initial `config.json` with the manager URL +
#      registration token.
#   8. Installs the systemd unit, runs `systemctl daemon-reload` and
#      `systemctl enable --now`.
#   9. Polls the local edge HTTP health endpoint for ~30 s waiting for
#      successful manager registration.

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────
RELEASE_REPO="${RELEASE_REPO:-Bilbycast/bilbycast-edge}"
INSTALL_ROOT="${INSTALL_ROOT:-/opt/bilbycast/edge}"
DATA_ROOT="${DATA_ROOT:-/var/lib/bilbycast/edge}"
CONFIG_DIR="${CONFIG_DIR:-/etc/bilbycast}"
SYSTEMD_UNIT_DIR="${SYSTEMD_UNIT_DIR:-/etc/systemd/system}"
COSIGN_VERSION="${COSIGN_VERSION:-v2.4.1}"

CHANNEL="stable"
VARIANT=""    # auto-detect from arch unless overridden
MANAGER_URL=""
REGISTRATION_TOKEN=""
UPGRADE_INSTALLER=0

# ── Argument parsing ──────────────────────────────────────────────────
usage() {
    cat <<EOF
Usage: $0 --manager <wss://...> --registration-token <token> [options]

Options:
  --manager <url>              Manager WebSocket URL (must be wss://)
  --registration-token <tok>   One-shot registration token from manager UI
  --channel <name>             Release channel (stable | nightly | beta), default stable
  --variant <name>             Binary variant (default | full), defaults to "full" on Linux
  --upgrade-installer          Refresh service unit + install script,
                               leave config and versions/ untouched
  -h, --help                   Show this message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --manager) MANAGER_URL="$2"; shift 2;;
        --registration-token) REGISTRATION_TOKEN="$2"; shift 2;;
        --channel) CHANNEL="$2"; shift 2;;
        --variant) VARIANT="$2"; shift 2;;
        --upgrade-installer) UPGRADE_INSTALLER=1; shift;;
        -h|--help) usage; exit 0;;
        *) echo "Unknown argument: $1" >&2; usage; exit 1;;
    esac
done

# ── Pre-flight checks ─────────────────────────────────────────────────
if [[ "$(id -u)" -ne 0 ]]; then
    echo "install-edge.sh must run as root (sudo)." >&2
    exit 1
fi

if [[ "${UPGRADE_INSTALLER}" -eq 0 ]]; then
    if [[ -z "${MANAGER_URL}" || -z "${REGISTRATION_TOKEN}" ]]; then
        echo "--manager and --registration-token are required for fresh installs." >&2
        usage
        exit 1
    fi
    if [[ "${MANAGER_URL}" != wss://* ]]; then
        echo "--manager URL must use wss:// (TLS required); got: ${MANAGER_URL}" >&2
        exit 1
    fi
fi

if [[ ! "${CHANNEL}" =~ ^(stable|nightly|beta)$ ]]; then
    echo "Channel must be stable | nightly | beta; got: ${CHANNEL}" >&2
    exit 1
fi

# Detect arch.
case "$(uname -m)-$(uname -s)" in
    x86_64-Linux)   ARCH="x86_64-linux";;
    aarch64-Linux)  ARCH="aarch64-linux";;
    *)
        echo "Unsupported host: $(uname -m) on $(uname -s)" >&2
        echo "bilbycast-edge releases are published for x86_64-linux and aarch64-linux." >&2
        exit 1
        ;;
esac

# Default to "full" on Linux — operators picking a non-transcoding
# install can pass `--variant default` explicitly. Full is what most
# broadcast workflows want.
if [[ -z "${VARIANT}" ]]; then
    VARIANT="full"
fi

if [[ "${VARIANT}" != "default" && "${VARIANT}" != "full" ]]; then
    echo "Variant must be default | full; got: ${VARIANT}" >&2
    exit 1
fi

echo "── bilbycast-edge installer ──"
echo "  Repo       : ${RELEASE_REPO}"
echo "  Channel    : ${CHANNEL}"
echo "  Arch       : ${ARCH}"
echo "  Variant    : ${VARIANT}"
echo "  Install at : ${INSTALL_ROOT}"
echo

# ── Idempotency guard ─────────────────────────────────────────────────
if [[ -e "${INSTALL_ROOT}/current" && "${UPGRADE_INSTALLER}" -eq 0 ]]; then
    echo "Already installed at ${INSTALL_ROOT}/current → $(readlink -f "${INSTALL_ROOT}/current")."
    echo "Use --upgrade-installer to refresh the service unit + install script,"
    echo "or trigger an upgrade from the manager UI to advance the binary."
    exit 0
fi

# ── Tooling: jq + curl + cosign ───────────────────────────────────────
need_pkg() {
    local pkg="$1"
    if ! command -v "${pkg}" > /dev/null 2>&1; then
        echo "${pkg} is required but not installed. Install via your package manager." >&2
        exit 1
    fi
}
need_pkg curl
need_pkg jq
need_pkg sha256sum

ensure_cosign() {
    if command -v cosign > /dev/null 2>&1; then
        echo "Using existing cosign: $(command -v cosign)"
        return
    fi
    echo "Installing cosign ${COSIGN_VERSION} into /usr/local/bin/cosign…"
    local cosign_arch
    case "${ARCH}" in
        x86_64-linux)  cosign_arch="amd64";;
        aarch64-linux) cosign_arch="arm64";;
    esac
    local url="https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/cosign-linux-${cosign_arch}"
    local checksum_url="${url}.sha256"
    curl -fsSL -o /tmp/cosign "${url}"
    # The .sha256 file format is `<hex>  <filename>` — pull just the hex.
    local expected
    expected="$(curl -fsSL "${checksum_url}" | awk '{print $1}')"
    if [[ -z "${expected}" ]]; then
        echo "Could not fetch cosign checksum from ${checksum_url}" >&2
        exit 1
    fi
    local got
    got="$(sha256sum /tmp/cosign | awk '{print $1}')"
    if [[ "${got}" != "${expected}" ]]; then
        echo "cosign checksum mismatch: expected ${expected}, got ${got}" >&2
        exit 1
    fi
    install -m 0755 /tmp/cosign /usr/local/bin/cosign
    rm /tmp/cosign
    echo "cosign installed."
}
ensure_cosign

# ── Resolve the latest release for the chosen channel ─────────────────
RELEASE_BASE="https://github.com/${RELEASE_REPO}/releases/latest/download"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT
cd "${WORK_DIR}"

echo "Downloading manifest.json + manifest.sig.bundle from ${RELEASE_BASE}…"
curl -fsSL -o manifest.json        "${RELEASE_BASE}/manifest.json"
curl -fsSL -o manifest.sig.bundle  "${RELEASE_BASE}/manifest.sig.bundle"

echo "Verifying Sigstore signature against ALLOWED_SIGNERS allowlist…"
COSIGN_EXPERIMENTAL=1 cosign verify-blob \
    --bundle manifest.sig.bundle \
    --certificate-identity-regexp "https://github\\.com/${RELEASE_REPO//\//\\/}/\\.github/workflows/nightly-release\\.yml@refs/tags/v.*" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    manifest.json

VERSION="$(jq -r '.version' manifest.json)"
CHANNEL_IN_MANIFEST="$(jq -r '.channel' manifest.json)"

if [[ "${CHANNEL_IN_MANIFEST}" != "${CHANNEL}" ]]; then
    echo "Manifest channel mismatch: requested ${CHANNEL}, got ${CHANNEL_IN_MANIFEST}." >&2
    echo "(Today every release targets the 'stable' channel; nightly / beta require dedicated workflows.)" >&2
    exit 1
fi

ARTEFACT_URL="$(jq -r --arg arch "${ARCH}" --arg variant "${VARIANT}" \
    '.artefacts[] | select(.arch == $arch and .variant == $variant) | .url' manifest.json)"
ARTEFACT_SHA256="$(jq -r --arg arch "${ARCH}" --arg variant "${VARIANT}" \
    '.artefacts[] | select(.arch == $arch and .variant == $variant) | .sha256' manifest.json)"

if [[ -z "${ARTEFACT_URL}" || "${ARTEFACT_URL}" == "null" ]]; then
    echo "No artefact for arch=${ARCH} variant=${VARIANT} in manifest." >&2
    echo "Available combinations:" >&2
    jq -r '.artefacts[] | "  \(.arch) / \(.variant)"' manifest.json >&2
    exit 1
fi

echo "Downloading ${ARTEFACT_URL}…"
curl -fsSL -o release.tar.gz "${ARTEFACT_URL}"
got_sha="$(sha256sum release.tar.gz | awk '{print $1}')"
if [[ "${got_sha}" != "${ARTEFACT_SHA256}" ]]; then
    echo "Tarball checksum mismatch: expected ${ARTEFACT_SHA256}, got ${got_sha}" >&2
    exit 1
fi
echo "Tarball SHA-256 matches manifest."

# ── Lay out /opt/bilbycast/edge/ ──────────────────────────────────────
mkdir -p "${INSTALL_ROOT}/versions"
mkdir -p "${DATA_ROOT}"
mkdir -p "${CONFIG_DIR}"

VERSION_DIR="${INSTALL_ROOT}/versions/${VERSION}"
if [[ ! -e "${VERSION_DIR}/bilbycast-edge" || "${UPGRADE_INSTALLER}" -eq 0 ]]; then
    rm -rf "${VERSION_DIR}.partial"
    mkdir "${VERSION_DIR}.partial"
    tar -xzf release.tar.gz -C "${VERSION_DIR}.partial"
    # Hoist the binary if it's in a nested dir (the release packaging
    # uses `bilbycast-edge-<version>-<arch>-<variant>/bilbycast-edge`).
    nested="$(find "${VERSION_DIR}.partial" -maxdepth 2 -name bilbycast-edge -type f | head -1)"
    if [[ -z "${nested}" ]]; then
        echo "Tarball did not contain a bilbycast-edge binary." >&2
        exit 1
    fi
    if [[ "${nested}" != "${VERSION_DIR}.partial/bilbycast-edge" ]]; then
        # Move binary + sibling files (LICENSE, NOTICE, etc.) to the
        # top of the version dir.
        mv "${nested%/*}"/* "${VERSION_DIR}.partial/"
        rmdir "${nested%/*}" 2>/dev/null || true
    fi
    chmod 0755 "${VERSION_DIR}.partial/bilbycast-edge"
    rm -rf "${VERSION_DIR}"
    mv "${VERSION_DIR}.partial" "${VERSION_DIR}"
fi

# Atomic symlink swap.
ln -sfn "${VERSION_DIR}" "${INSTALL_ROOT}/current.tmp"
mv -Tf "${INSTALL_ROOT}/current.tmp" "${INSTALL_ROOT}/current"

# ── Create system user + group ─────────────────────────────────────────
if ! id -u bilbycast > /dev/null 2>&1; then
    if command -v systemd-sysusers > /dev/null 2>&1; then
        # /etc/sysusers.d/ doesn't exist on minimal Ubuntu / Debian images
        # by default, even when systemd-sysusers is present. Pre-create it.
        mkdir -p /etc/sysusers.d
        cat > /etc/sysusers.d/bilbycast.conf <<'EOF'
u bilbycast - "bilbycast-edge service account" /var/lib/bilbycast/edge /usr/sbin/nologin
m bilbycast video
m bilbycast render
m bilbycast audio
EOF
        systemd-sysusers
    else
        useradd --system --home /var/lib/bilbycast/edge --shell /usr/sbin/nologin bilbycast
        for grp in video render audio; do
            getent group "${grp}" > /dev/null && usermod -aG "${grp}" bilbycast || true
        done
    fi
fi

chown -R bilbycast:bilbycast "${INSTALL_ROOT}" "${DATA_ROOT}"

# ── Initial config + secrets ──────────────────────────────────────────
CONFIG_FILE="${INSTALL_ROOT}/config.json"
SECRETS_FILE="${INSTALL_ROOT}/secrets.json"

if [[ "${UPGRADE_INSTALLER}" -eq 0 || ! -f "${CONFIG_FILE}" ]]; then
    cat > "${CONFIG_FILE}.tmp" <<EOF
{
  "version": 2,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "manager": {
    "enabled": true,
    "urls": ["${MANAGER_URL}"],
    "registration_token": "${REGISTRATION_TOKEN}"
  },
  "upgrades": {
    "enabled": true,
    "allowed_channels": ["stable"],
    "install_root": "${INSTALL_ROOT}"
  }
}
EOF
    mv -f "${CONFIG_FILE}.tmp" "${CONFIG_FILE}"
    chown bilbycast:bilbycast "${CONFIG_FILE}"
    chmod 0640 "${CONFIG_FILE}"
fi

if [[ ! -f "${SECRETS_FILE}" ]]; then
    : > "${SECRETS_FILE}"
    chown bilbycast:bilbycast "${SECRETS_FILE}"
    chmod 0600 "${SECRETS_FILE}"
fi

# ── Install systemd unit ──────────────────────────────────────────────
UNIT_DEST="${SYSTEMD_UNIT_DIR}/bilbycast-edge.service"
# Prefer the unit shipped inside the tarball; fall back to a curl from
# the release if the tarball didn't include it (older releases).
if [[ -f "${VERSION_DIR}/packaging/bilbycast-edge.service" ]]; then
    install -m 0644 "${VERSION_DIR}/packaging/bilbycast-edge.service" "${UNIT_DEST}"
else
    curl -fsSL -o "${UNIT_DEST}" \
        "https://github.com/${RELEASE_REPO}/releases/latest/download/bilbycast-edge.service"
fi

# Default env file (RUST_LOG etc.). Only seed it if missing — operators
# may have customised it.
ENV_FILE="${CONFIG_DIR}/edge.env"
if [[ ! -f "${ENV_FILE}" ]]; then
    cat > "${ENV_FILE}" <<'EOF'
# bilbycast-edge runtime environment.
# Tunable via systemctl restart bilbycast-edge (no daemon-reload needed).
RUST_LOG=info
# BILBYCAST_REPLAY_DIR=/var/lib/bilbycast/edge/replay
# BILBYCAST_MEDIA_DIR=/var/lib/bilbycast/edge/media
EOF
    chmod 0640 "${ENV_FILE}"
fi

systemctl daemon-reload
systemctl enable --now bilbycast-edge

# ── Optional: per-interface strict binding ─────────────────────────────
# Strict mode (`SO_BINDTODEVICE` on each input/output socket) requires
# `CAP_NET_RAW`. The default unit drops every capability, so strict is
# opt-in via a documented drop-in. Print the install one-liner so
# operators discover the toggle without grepping the docs. Loose
# source-IP binding works on the default install with zero extra caps.
echo
echo "Optional — strict per-interface binding (kernel-enforced NIC pinning):"
echo "  sudo install -m 0644 ${VERSION_DIR}/packaging/strict-binding.conf \\"
echo "      /etc/systemd/system/bilbycast-edge.service.d/strict-binding.conf"
echo "  sudo systemctl daemon-reload && sudo systemctl restart bilbycast-edge"

# ── Wait for first manager registration ────────────────────────────────
echo
echo "Waiting up to 60 s for bilbycast-edge to come up + register with the manager…"
for _ in $(seq 1 30); do
    if systemctl is-active --quiet bilbycast-edge; then
        if curl -fsS http://127.0.0.1:8080/health > /dev/null 2>&1; then
            echo "bilbycast-edge is up. Verify in the manager UI under /admin/nodes."
            exit 0
        fi
    fi
    sleep 2
done

echo
echo "bilbycast-edge service didn't reach a healthy state in 60 s."
echo "Inspect:"
echo "  journalctl -u bilbycast-edge -e"
echo "  systemctl status bilbycast-edge"
exit 1
