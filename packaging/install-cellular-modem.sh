#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# OPTIONAL add-on installer: set up the bilbycast cellular-modem bond-leg
# daemon. Use this ONLY if a USB cellular modem (e.g. Teltonika TRM500 /
# Quectel RG520N), managed by ModemManager, is one of your bonding uplinks.
#
# It does NOT install or touch bilbycast-edge itself — run install-edge.sh for
# that first (or alongside). This script just adds the cellular keep-alive
# daemon: the bring-up script, the systemd unit, and its env file.
#
# What it does:
#   1. Installs setup-cellular-modem.sh into the edge packaging dir
#      (/opt/bilbycast/edge/current/packaging — tracks edge upgrades).
#   2. Installs bilbycast-cellular-modem.service into /etc/systemd/system.
#   3. Installs the env file to /etc/default/bilbycast-cellular-modem
#      (only if absent — never clobbers your APN).
#   4. `systemctl daemon-reload`.
#   5. Leaves the unit DISABLED unless you pass --enable (opt-in by design).
#
# Usage:
#   sudo packaging/install-cellular-modem.sh            # install, don't enable
#   sudo APN=connect packaging/install-cellular-modem.sh --enable
#
# The daemon needs an APN. With --enable, set it first via APN=<apn> on this
# command (written into the env file) or edit /etc/default/bilbycast-cellular-modem.
#
set -euo pipefail

SYSTEMD_UNIT_DIR="${SYSTEMD_UNIT_DIR:-/etc/systemd/system}"
EDGE_PKG_DIR="${EDGE_PKG_DIR:-/opt/bilbycast/edge/current/packaging}"
ENV_DEST="${ENV_DEST:-/etc/default/bilbycast-cellular-modem}"
APN="${APN:-}"

ENABLE=0
[[ "${1:-}" == "--enable" ]] && ENABLE=1

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { echo "install-cellular-modem: $*"; }
die() { echo "install-cellular-modem: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "must run as root"
command -v systemctl >/dev/null || die "systemctl not found — this installer targets systemd hosts"
[[ -f "$SRC_DIR/setup-cellular-modem.sh" ]]        || die "setup-cellular-modem.sh not found next to this installer"
[[ -f "$SRC_DIR/bilbycast-cellular-modem.service" ]] || die "bilbycast-cellular-modem.service not found next to this installer"

# 1. runtime script into the edge packaging dir (so edge upgrades carry it).
if [[ ! -d "$EDGE_PKG_DIR" ]]; then
    log "edge packaging dir $EDGE_PKG_DIR missing — creating it (run install-edge.sh for a full edge install)"
    mkdir -p "$EDGE_PKG_DIR"
fi
install -m 0755 "$SRC_DIR/setup-cellular-modem.sh" "$EDGE_PKG_DIR/setup-cellular-modem.sh"
log "installed $EDGE_PKG_DIR/setup-cellular-modem.sh"

# 2. systemd unit.
install -m 0644 "$SRC_DIR/bilbycast-cellular-modem.service" "$SYSTEMD_UNIT_DIR/bilbycast-cellular-modem.service"
log "installed $SYSTEMD_UNIT_DIR/bilbycast-cellular-modem.service"

# 3. env file — never overwrite an existing one (operator's APN lives there).
if [[ -f "$ENV_DEST" ]]; then
    log "env file $ENV_DEST already exists — leaving it untouched"
else
    install -m 0644 "$SRC_DIR/bilbycast-cellular-modem.default" "$ENV_DEST"
    log "installed $ENV_DEST"
fi
# If APN was passed on this command, write it into the env file.
if [[ -n "$APN" ]]; then
    if grep -q '^APN=' "$ENV_DEST"; then
        sed -i "s|^APN=.*|APN=$APN|" "$ENV_DEST"
    else
        echo "APN=$APN" >> "$ENV_DEST"
    fi
    log "set APN=$APN in $ENV_DEST"
fi

# 4. reload.
systemctl daemon-reload

# 4b. Seed the request/execute IPC the edge + keeper share. The edge writes the
#     REQUEST file in place (it runs unprivileged and can't write the root-owned
#     dir), so it must be owned by the edge's service account. The STATUS file is
#     created by THIS daemon (root) at runtime — deliberately not seeded, so its
#     absence keeps the edge from advertising `cellular-control` until the keeper
#     is actually running. Mirrors the PTP helper's ptp.conf seeding.
WAKE_DIR="/var/lib/bilbycast"
WAKE_REQ="$WAKE_DIR/cellular-wake.req"
mkdir -p "$WAKE_DIR"
[[ -e "$WAKE_REQ" ]] || : >"$WAKE_REQ"
chmod 0664 "$WAKE_REQ" 2>/dev/null || true
if id bilbycast >/dev/null 2>&1; then
    chown bilbycast:bilbycast "$WAKE_REQ" 2>/dev/null || chown bilbycast "$WAKE_REQ" 2>/dev/null || true
    log "seeded $WAKE_REQ (writable by the bilbycast edge account)"
else
    log "note: 'bilbycast' user not found — $WAKE_REQ left root-owned. Run install-edge.sh"
    log "      first so the edge can write wake requests (else the Wake button can't post)."
fi

# 5. opt-in enable.
if [[ "$ENABLE" -eq 1 ]]; then
    if ! grep -qE '^APN=.+' "$ENV_DEST"; then
        die "--enable requested but no APN set. Re-run with APN=<apn> --enable, or edit $ENV_DEST then: sudo systemctl enable --now bilbycast-cellular-modem.service"
    fi
    systemctl enable --now bilbycast-cellular-modem.service
    log "enabled + started bilbycast-cellular-modem.service"
    log "status:  systemctl status bilbycast-cellular-modem.service"
else
    echo
    log "installed (DISABLED). To turn it on:"
    log "  1. set your APN:  sudo \$EDITOR $ENV_DEST   (set APN=...)"
    log "  2. enable:        sudo systemctl enable --now bilbycast-cellular-modem.service"
    log "  3. verify:        systemctl status bilbycast-cellular-modem.service"
fi

echo
log "docs: https://docs.bilbycast.com/edge/bonding-cellular-modem/"
