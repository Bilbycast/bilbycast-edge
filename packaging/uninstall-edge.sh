#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# uninstall-edge.sh — counterpart to install-edge.sh.
#
# By default removes the binary tree and systemd unit but preserves
# `config.json`, `secrets.json`, and `/var/lib/bilbycast/edge` so the
# operator can reinstall and pick up where they left off. Pass
# `--purge` to wipe everything including config + state.

set -euo pipefail

INSTALL_ROOT="${INSTALL_ROOT:-/opt/bilbycast/edge}"
DATA_ROOT="${DATA_ROOT:-/var/lib/bilbycast/edge}"
CONFIG_DIR="${CONFIG_DIR:-/etc/bilbycast}"
SYSTEMD_UNIT_DIR="${SYSTEMD_UNIT_DIR:-/etc/systemd/system}"
PURGE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --purge) PURGE=1; shift;;
        -h|--help)
            cat <<EOF
Usage: $0 [--purge]
  --purge    Remove config + state (otherwise preserved for reinstall).
EOF
            exit 0
            ;;
        *) echo "Unknown argument: $1" >&2; exit 1;;
    esac
done

if [[ "$(id -u)" -ne 0 ]]; then
    echo "uninstall-edge.sh must run as root (sudo)." >&2
    exit 1
fi

echo "Stopping bilbycast-edge…"
systemctl disable --now bilbycast-edge 2>/dev/null || true
rm -f "${SYSTEMD_UNIT_DIR}/bilbycast-edge.service"
rm -f /etc/sysctl.d/90-bilbycast-edge.conf
systemctl daemon-reload

echo "Removing binaries from ${INSTALL_ROOT}…"
rm -rf "${INSTALL_ROOT}/versions" "${INSTALL_ROOT}/current" "${INSTALL_ROOT}/previous" \
       "${INSTALL_ROOT}/state.json" "${INSTALL_ROOT}/.state.lock"

if [[ "${PURGE}" -eq 1 ]]; then
    echo "--purge: removing config + data…"
    rm -rf "${INSTALL_ROOT}/config.json" "${INSTALL_ROOT}/secrets.json"
    rm -rf "${DATA_ROOT}"
    rm -f "${CONFIG_DIR}/edge.env"
    rm -f /etc/sysusers.d/bilbycast.conf
    if id -u bilbycast > /dev/null 2>&1; then
        userdel bilbycast 2>/dev/null || true
    fi
    rmdir "${INSTALL_ROOT}" 2>/dev/null || true
    rmdir "${CONFIG_DIR}" 2>/dev/null || true
fi

echo "Uninstall complete."
