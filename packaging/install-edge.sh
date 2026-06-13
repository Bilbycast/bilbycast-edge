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
OUTPUT_NICS=""  # comma-separated NIC list for ETF qdisc + SO_TXTIME

# ── Argument parsing ──────────────────────────────────────────────────
usage() {
    cat <<EOF
Usage: $0 --manager <wss://...> --registration-token <token> [options]

Options:
  --manager <url>              Manager WebSocket URL (must be wss://)
  --registration-token <tok>   One-shot registration token from manager UI
  --channel <name>             Release channel (stable | nightly | beta), default stable
  --variant <name>             Binary variant (default | full), defaults to "full" on Linux
  --output-nics <nic1,nic2>    Comma-separated NIC names for ETF qdisc + SO_TXTIME
                               wire pacing. Each NIC gets a boot-persistent systemd
                               unit and BILBYCAST_ENABLE_TXTIME=1 is set in the env
                               file. Requires ≥ 3 HW tx queues per NIC (validated).
                               Omit to stay on the default clock_nanosleep tier.
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
        --output-nics) OUTPUT_NICS="$2"; shift 2;;
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

# linuxptp ships ptp4l + phc2sys + pmc — required by the
# bilbycast-ptp.service unit when an operator enables PTP via the
# manager UI. Install via apt rather than need_pkg so a missing
# linuxptp on a fresh node doesn't block install of bilbycast-edge
# itself (the operator may choose `mode = off` and never need
# linuxptp). The bilbycast-ptp.service will refuse to start with a
# clear error if the binaries are missing later — and apt will be
# tried again automatically here on next install/upgrade.
if ! command -v ptp4l > /dev/null 2>&1; then
    if command -v apt-get > /dev/null 2>&1; then
        echo "Installing linuxptp (provides ptp4l, phc2sys, pmc)…"
        DEBIAN_FRONTEND=noninteractive apt-get update -qq || true
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq linuxptp || true
    else
        echo "WARNING: ptp4l not found and apt-get unavailable. The"
        echo "         bilbycast-ptp.service unit will fail until linuxptp"
        echo "         is installed manually. ST 2110 / MXL flows that"
        echo "         require PTP will not start."
    fi
fi

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
    fi
fi

# Reconcile supplementary groups on every run (idempotent) — not just on
# user creation. Covers upgrades from installer versions that granted
# fewer groups, and hosts where the user pre-existed. Without `audio`
# the display output renders video but ALSA opens fail (the alsa-lib
# symptom is a misleading "Cannot get card index for N").
for grp in video render audio; do
    getent group "${grp}" > /dev/null && usermod -aG "${grp}" bilbycast || true
done

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

# ── Install ETF qdisc systemd unit template ──────────────────────────
# The bilbycast-etf-qdisc@.service template installs the ETF qdisc on
# a named NIC at boot, before bilbycast-edge starts. Always laid down
# so it's discoverable; instances are enabled only when the operator
# passes --output-nics at install time.
ETF_UNIT_DEST="${SYSTEMD_UNIT_DIR}/bilbycast-etf-qdisc@.service"
if [[ -f "${VERSION_DIR}/packaging/bilbycast-etf-qdisc@.service" ]]; then
    install -m 0644 "${VERSION_DIR}/packaging/bilbycast-etf-qdisc@.service" "${ETF_UNIT_DEST}"
fi

# ── Kernel tuning (sysctl.d) ──────────────────────────────────────────
# net.core.rmem_max: SO_RCVBUF ceiling. The ST 2110-20 ingest requests
# 64 MB per leg (the distro-default 4 MB cap holds ~8 ms at 2160p50 —
# one scheduler hiccup overflows it into kernel drops). Installed as a
# persistent sysctl.d drop-in and applied immediately; harmless for
# non-2110 deployments (a ceiling, not an allocation).
SYSCTL_DEST="/etc/sysctl.d/90-bilbycast-edge.conf"
if [[ -f "${VERSION_DIR}/packaging/90-bilbycast-edge.conf" ]]; then
    install -m 0644 "${VERSION_DIR}/packaging/90-bilbycast-edge.conf" "${SYSCTL_DEST}"
    sysctl -p "${SYSCTL_DEST}" >/dev/null 2>&1 || true
fi

# ── Enable ETF qdisc per NIC (when --output-nics is provided) ────────
ETF_ENABLED_COUNT=0
if [[ -n "${OUTPUT_NICS}" ]]; then
    IFS=',' read -ra NICS <<< "${OUTPUT_NICS}"
    for nic in "${NICS[@]}"; do
        nic="$(echo "$nic" | xargs)"  # trim whitespace
        if ! ip link show "$nic" &>/dev/null; then
            echo "WARNING: NIC '$nic' not found — skipping ETF setup" >&2
            continue
        fi
        tx_queues="$(ls -d /sys/class/net/"$nic"/queues/tx-* 2>/dev/null | wc -l)"
        if [[ "$tx_queues" -lt 3 ]]; then
            echo "WARNING: NIC '$nic' has $tx_queues tx queue(s) (need ≥ 3 for mqprio) — skipping ETF setup" >&2
            continue
        fi
        echo "  Enabling ETF qdisc boot service on $nic ($tx_queues tx queues)"
        systemctl enable "bilbycast-etf-qdisc@${nic}.service" 2>/dev/null || {
            echo "WARNING: failed to enable bilbycast-etf-qdisc@${nic}.service" >&2
            continue
        }
        systemctl start "bilbycast-etf-qdisc@${nic}.service" 2>/dev/null || {
            echo "WARNING: ETF qdisc install failed on $nic — the boot service is enabled"
            echo "         and will retry on next reboot. Check: systemctl status bilbycast-etf-qdisc@${nic}" >&2
        }
        ((ETF_ENABLED_COUNT++)) || true
    done
fi

# ── Install bilbycast PTP supervisor unit + helper + script ──────────
# The helper (`bilbycast-ptp-helper`) ships alongside `bilbycast-edge`
# in the release tarball. It runs under `bilbycast-ptp.service` with
# ambient capabilities (CAP_NET_RAW, CAP_NET_ADMIN, CAP_SYS_TIME) so
# the operator can change PTP modes via the manager UI without sudo
# at runtime — the UI writes `/var/lib/bilbycast/ptp.conf` (writable
# by the bilbycast user) and the helper's 1 Hz mtime poll picks it up.
#
# Default install ships `mode = off` so a freshly-imaged node sits
# idle until the operator opts in via the manager. ST 2110 / MXL
# flows refuse to start until the operator picks a non-off mode.
PTP_SCRIPT_SRC="${VERSION_DIR}/packaging/bilbycast-ptp-gm.sh"
PTP_SCRIPT_DEST="/opt/bilbycast/bin/bilbycast-ptp-gm.sh"
PTP_HELPER_SRC="${VERSION_DIR}/bilbycast-ptp-helper"
PTP_HELPER_DEST="${VERSION_DIR}/bilbycast-ptp-helper"   # already in place
PTP_UNIT_SRC="${VERSION_DIR}/packaging/bilbycast-ptp.service"
PTP_UNIT_DEST="${SYSTEMD_UNIT_DIR}/bilbycast-ptp.service"
PTP_CONF_FILE="/var/lib/bilbycast/ptp.conf"
PTP_CONF_DEFAULT_SRC="${VERSION_DIR}/packaging/bilbycast-ptp.default"

mkdir -p /opt/bilbycast/bin
if [[ -f "${PTP_SCRIPT_SRC}" ]]; then
    install -m 0755 "${PTP_SCRIPT_SRC}" "${PTP_SCRIPT_DEST}"
    # The script reads its config template from the same dir by
    # default; ship the template alongside.
    if [[ -f "${VERSION_DIR}/packaging/bilbycast-ptp-gm.conf" ]]; then
        install -m 0644 "${VERSION_DIR}/packaging/bilbycast-ptp-gm.conf" \
            /opt/bilbycast/bin/bilbycast-ptp-gm.conf
    fi
fi

if [[ -f "${PTP_UNIT_SRC}" ]]; then
    install -m 0644 "${PTP_UNIT_SRC}" "${PTP_UNIT_DEST}"
fi

# Seed the default config — `mode = off` until operator opts in.
mkdir -p /var/lib/bilbycast
if [[ ! -f "${PTP_CONF_FILE}" ]]; then
    if [[ -f "${PTP_CONF_DEFAULT_SRC}" ]]; then
        install -m 0644 "${PTP_CONF_DEFAULT_SRC}" "${PTP_CONF_FILE}"
    else
        cat > "${PTP_CONF_FILE}" <<'EOF'
# bilbycast-ptp-helper config — manager UI rewrites this file when
# the operator changes PTP mode. mode = off keeps PTP disabled.
mode = off
iface =
domain = 127
priority1 =
scan_timeout = 5
EOF
    fi
    chown bilbycast:bilbycast "${PTP_CONF_FILE}"
    chmod 0664 "${PTP_CONF_FILE}"
fi

# Runtime dirs the helper + script write to. Owned by bilbycast so
# the service unit (running as `bilbycast` user) can manage PID
# files + logs without escalating.
install -d -o bilbycast -g bilbycast -m 0755 /var/run/bilbycast-ptp
install -d -o bilbycast -g bilbycast -m 0755 /var/log/bilbycast-ptp
# /etc/linuxptp holds the staged ptp4l config the script generates.
# Stock AppArmor profile on /usr/sbin/ptp4l allows reads from
# @{etc_ro}/linuxptp/** so the staged path lives there.
install -d -m 0755 /etc/linuxptp

# AppArmor local override — lets ptp4l reply to bilbycast / pmc
# client sockets under /tmp/. Belt-and-braces alongside the F4
# abstract-socket fix in edge 0.89.0+.
APPARMOR_LOCAL_SRC="${VERSION_DIR}/packaging/apparmor-local-ptp4l"
APPARMOR_LOCAL_DEST="/etc/apparmor.d/local/usr.sbin.ptp4l"
if [[ -d /etc/apparmor.d/local && -f "${APPARMOR_LOCAL_SRC}" ]]; then
    if [[ ! -f "${APPARMOR_LOCAL_DEST}" ]] \
       || ! cmp -s "${APPARMOR_LOCAL_SRC}" "${APPARMOR_LOCAL_DEST}"; then
        install -m 0644 "${APPARMOR_LOCAL_SRC}" "${APPARMOR_LOCAL_DEST}"
        if command -v apparmor_parser > /dev/null 2>&1; then
            apparmor_parser -r /etc/apparmor.d/usr.sbin.ptp4l 2>/dev/null || true
        fi
    fi
fi

# Enable the PTP unit. The default `mode = off` config means the
# helper starts, idles, and never spawns ptp4l until the operator
# changes the config. This is intentional — the service is always
# running so a manager-UI write to ptp.conf is immediately picked
# up via the 1 Hz mtime poll (no systemctl restart needed).
systemctl enable --now bilbycast-ptp.service 2>/dev/null || true

# Default env file (RUST_LOG etc.). Only seed it if missing — operators
# may have customised it.
ENV_FILE="${CONFIG_DIR}/edge.env"
if [[ ! -f "${ENV_FILE}" ]]; then
    if [[ "${ETF_ENABLED_COUNT}" -gt 0 ]]; then
        TXTIME_LINE="BILBYCAST_ENABLE_TXTIME=1"
    else
        TXTIME_LINE="# BILBYCAST_ENABLE_TXTIME=1"
    fi
    cat > "${ENV_FILE}" <<EOF
# bilbycast-edge runtime environment.
# Tunable via systemctl restart bilbycast-edge (no daemon-reload needed).
RUST_LOG=info
# BILBYCAST_REPLAY_DIR=/var/lib/bilbycast/edge/replay
# BILBYCAST_MEDIA_DIR=/var/lib/bilbycast/edge/media

# Lock all current + future memory pages into RAM at startup via
# \`mlockall(MCL_CURRENT | MCL_FUTURE)\`. Eliminates major-page-fault
# stalls on the data-plane hot path (encoder buffers, wire-emit
# threads, broadcast channel slots). Paired with LimitMEMLOCK=infinity
# in the systemd unit so the call always succeeds on this install.
# Recommended on production deployments; safe to leave on.
BILBYCAST_MLOCKALL=1

# CPU pinning for wire-emit threads. Each output socket spawns its
# own wire-emit thread; with pinning, multiple threads round-robin
# across the listed CPUs. Recommended pairing: \`isolcpus=N-M\` on the
# kernel command line + this set referencing the same isolated cores.
# Example for a 4-core box with cores 2 + 3 isolated:
# BILBYCAST_WIRE_EMIT_CPUS=2,3

# SO_TXTIME wire-pacing release tier (kernel-paced via the ETF qdisc;
# tier 1 / 2 in OutputStats.wire_pacing_tier). Requires the ETF qdisc
# on each egress NIC — installed at boot by the per-NIC
# bilbycast-etf-qdisc@<NIC>.service units. For sub-us jitter (tier 1),
# also needs a HW-PTP NIC with ptp4l + phc2sys disciplined to a PTP
# grandmaster. Full setup: docs.bilbycast.com/edge/wire-pacing/
${TXTIME_LINE}
EOF
    chmod 0640 "${ENV_FILE}"
elif [[ "${ETF_ENABLED_COUNT}" -gt 0 ]]; then
    if grep -q '^# *BILBYCAST_ENABLE_TXTIME=' "${ENV_FILE}"; then
        sed -i 's/^# *BILBYCAST_ENABLE_TXTIME=.*/BILBYCAST_ENABLE_TXTIME=1/' "${ENV_FILE}"
        echo "  Enabled BILBYCAST_ENABLE_TXTIME=1 in ${ENV_FILE}"
    elif ! grep -q '^BILBYCAST_ENABLE_TXTIME=' "${ENV_FILE}"; then
        echo "BILBYCAST_ENABLE_TXTIME=1" >> "${ENV_FILE}"
        echo "  Added BILBYCAST_ENABLE_TXTIME=1 to ${ENV_FILE}"
    fi
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

# ── Optional: USB cellular modem as a bond leg ─────────────────────────
# Only relevant if a USB cellular modem (ModemManager-managed) is one of
# this host's bonding uplinks. Opt-in keep-alive daemon — not enabled by
# default. Print the pointer so operators discover it during install.
echo
echo "Optional — USB cellular modem as a bonding path (keep-alive daemon):"
echo "  sudo APN=<your-apn> ${VERSION_DIR}/packaging/install-cellular-modem.sh --enable"
echo "  https://docs.bilbycast.com/edge/bonding-cellular-modem/"

# ── SO_TXTIME wire-pacing status ──────────────────────────────────────
if [[ "${ETF_ENABLED_COUNT}" -gt 0 ]]; then
    echo
    echo "Wire pacing: SO_TXTIME enabled on ${ETF_ENABLED_COUNT} NIC(s) via ETF qdisc."
    echo "  Verify: tc -s qdisc show dev <NIC>"
    echo "  Disable per-NIC: sudo systemctl disable --now bilbycast-etf-qdisc@<NIC>"
    echo "  For sub-us jitter (tier 1), also configure PTP:"
    echo "    https://docs.bilbycast.com/edge/wire-pacing/"
else
    echo
    echo "Optional — kernel-paced wire emission (SO_TXTIME, sub-µs PCR_AC):"
    echo "  Rerun this installer with --output-nics <nic1,nic2,...> to enable, or manually:"
    echo "  sudo bash ${VERSION_DIR}/packaging/setup-etf-qdisc.sh <NIC>"
    echo "  sudo systemctl enable --now bilbycast-etf-qdisc@<NIC>"
    echo "  # then uncomment BILBYCAST_ENABLE_TXTIME=1 in ${ENV_FILE} and restart:"
    echo "  sudo systemctl restart bilbycast-edge"
fi

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
