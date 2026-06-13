#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Bring up a USB cellular modem (managed by ModemManager) as an independent
# egress path for bilbycast-edge multi-path bonding, then hold it up.
#
# This is for a modem the EDGE HOST owns directly over USB (e.g. a Teltonika
# TRM500 / Quectel RG520N 5G stick) — NOT a self-contained router like the
# Teltonika OTD500. A router presents a normal Ethernet interface with its
# own DHCP/NAT (see packaging notes and the website "Bonding Network Setup"
# page); a USB modem presents a raw-IP, point-to-point WWAN interface
# (`wwpXsYuZ`/`wwanN`) whose IPv4 lease comes from the carrier via
# ModemManager — there is no router doing DHCP for you.
#
# Why this script lives outside the edge (same rationale as setup-etf-qdisc.sh):
#
#   - Establishing a data bearer and writing `ip addr`/`ip route`/`ip rule`
#     needs `CAP_NET_ADMIN`, deliberately NOT granted to the edge process.
#     Keeping modem bring-up out of the edge keeps the edge's blast radius
#     small and avoids a privileged daemon inside the media path.
#   - Which modem, which APN, and how the path is routed are deployment
#     decisions. This is an opinionated default that works for a single USB
#     cellular stick used as one bond leg.
#
# ROUTING MODEL — source-based policy routing (deliberate, differs from the
# router-per-modem design):
#
#   The carrier hands a USB modem a CGNAT address (often /28 point-to-point)
#   and the link is METERED. We put the modem's default route in its OWN
#   table (TABLE) gated by an `ip rule` on the modem's source address, instead
#   of a high-metric default in the main table. Result: ONLY traffic the edge
#   explicitly pins to this modem (a bond leg via SO_BINDTODEVICE, which selects
#   the modem's source address) ever uses it. The host default route is left
#   untouched, and — critically for a metered SIM — background traffic can
#   NEVER silently fail over onto cellular if the primary link flaps.
#
#   (Fixed router uplinks don't have the metering risk and can use the simpler
#   high-metric main-table default documented in "Bonding Network Setup".)
#
# Prerequisites:
#
#   - ModemManager running (`mmcli --version`), modem enumerated (`mmcli -L`)
#   - An ACTIVATED SIM with a known data APN
#   - iproute2 (`ip`)
#
# Usage:
#
#   sudo APN=<your-apn> bash packaging/setup-cellular-modem.sh
#   sudo APN=connect      bash packaging/setup-cellular-modem.sh      # one-shot
#   sudo APN=connect      bash packaging/setup-cellular-modem.sh --watch   # daemon
#
# Env knobs (all optional except APN):
#
#   APN            data APN (REQUIRED) — e.g. connect, truphone.com
#   MODEM_INDEX    ModemManager modem index (default: first modem from `mmcli -L`)
#   MODEM_IFACE    WWAN net interface (default: auto from the modem/bearer)
#   IP_TYPE        ipv4 | ipv6 | ipv4v6 (default: ipv4)
#   TABLE          routing table id for this path (default: 70)
#   RULE_PREF      ip-rule priority (default: 1070)
#   WATCH_INTERVAL seconds between health checks in --watch mode (default: 30)
#
# Persistence across reboot + automatic reconnect on drop is operator policy.
# Install the opt-in keep-alive daemon with packaging/install-cellular-modem.sh
# (it runs THIS script with --watch under bilbycast-cellular-modem.service).
#
# Verify:
#
#   ping  -I <MODEM_IFACE> -c3 8.8.8.8
#   curl  --interface <MODEM_IFACE> -s https://api.ipify.org   # carrier public IP
#
set -euo pipefail

APN="${APN:-}"
MODEM_INDEX="${MODEM_INDEX:-}"
MODEM_IFACE="${MODEM_IFACE:-}"
IP_TYPE="${IP_TYPE:-ipv4}"
TABLE="${TABLE:-70}"
RULE_PREF="${RULE_PREF:-1070}"
WATCH_INTERVAL="${WATCH_INTERVAL:-30}"

WATCH=0
[[ "${1:-}" == "--watch" ]] && WATCH=1

log() { echo "setup-cellular-modem: $*"; }
die() { echo "setup-cellular-modem: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "must run as root (modem connect + ip routing need CAP_NET_ADMIN)"
[[ -n "$APN" ]]   || die "APN is required — set APN=<your-apn> (e.g. APN=connect)"
command -v mmcli >/dev/null || die "'mmcli' not found — install ModemManager"
command -v ip    >/dev/null || die "'ip' not found — install iproute2"

# --- modem discovery ---------------------------------------------------------
detect_modem() {
    [[ -n "$MODEM_INDEX" ]] && { echo "$MODEM_INDEX"; return 0; }
    # First modem path from `mmcli -L`, e.g. /org/freedesktop/ModemManager1/Modem/1
    mmcli -L 2>/dev/null | sed -n 's#.*/Modem/\([0-9][0-9]*\).*#\1#p' | head -n1
}

# Echo the path of a connected bearer that carries an IPv4 address, else nothing.
pick_bearer() {
    local b kv
    for b in $(mmcli -m "$MODEM" -K 2>/dev/null \
                 | sed -n 's/.*bearers.value\[[0-9]*\][[:space:]]*:[[:space:]]*//p'); do
        [[ -n "$b" ]] || continue
        kv=$(mmcli -b "$b" -K 2>/dev/null) || continue
        grep -q '^bearer.status.connected[[:space:]]*:[[:space:]]*yes' <<<"$kv" || continue
        grep -q '^bearer.ipv4-config.address[[:space:]]*:[[:space:]]*[0-9]' <<<"$kv" || continue
        echo "$b"; return 0
    done
    return 1
}

# Bring up a data bearer if none is connected (simple-connect also enables +
# registers the modem as needed).
ensure_connected() {
    if ! pick_bearer >/dev/null; then
        log "no connected data bearer on Modem/$MODEM — connecting (apn=$APN, ip-type=$IP_TYPE)"
        mmcli -m "$MODEM" --simple-connect="apn=$APN,ip-type=$IP_TYPE" \
            || die "simple-connect failed — check registration: mmcli -m $MODEM"
        sleep 3
    fi
}

# True if the interface already carries $1 and the policy route + rule exist
# (lets --watch re-check cheaply without churning the interface every tick).
already_up() {
    ip -4 addr show dev "$MODEM_IFACE" 2>/dev/null | grep -q "inet $1/"   || return 1
    ip route show table "$TABLE" 2>/dev/null       | grep -q '^default '  || return 1
    ip rule list 2>/dev/null                       | grep -q "lookup $TABLE" || return 1
    return 0
}

bring_up() {
    MODEM="$(detect_modem)"
    [[ -n "$MODEM" ]] || die "no ModemManager modem found (mmcli -L) — is the stick plugged in?"

    ensure_connected
    local bearer kv addr prefix gw mtu
    bearer="$(pick_bearer)" || die "no connected IPv4 bearer on Modem/$MODEM after connect"
    kv="$(mmcli -b "$bearer" -K)"
    get() { sed -n "s/^bearer.ipv4-config.$1[[:space:]]*:[[:space:]]*//p" <<<"$kv"; }
    addr="$(get address)"; prefix="$(get prefix)"; gw="$(get gateway)"; mtu="$(get mtu)"
    mtu="${mtu:-1500}"
    [[ -n "$addr" && -n "$prefix" && -n "$gw" ]] || die "incomplete bearer IPv4 config"

    # Resolve the WWAN net interface: explicit env > bearer.interface > modem net port.
    if [[ -z "$MODEM_IFACE" ]]; then
        MODEM_IFACE="$(sed -n 's/^bearer.interface[[:space:]]*:[[:space:]]*//p' <<<"$kv")"
    fi
    if [[ -z "$MODEM_IFACE" ]]; then
        MODEM_IFACE="$(mmcli -m "$MODEM" -K 2>/dev/null \
            | sed -n 's/.*ports.value\[[0-9]*\][[:space:]]*:[[:space:]]*\([^ ]*\) (net).*/\1/p' | head -n1)"
    fi
    [[ -n "$MODEM_IFACE" ]] || die "could not resolve the WWAN interface — set MODEM_IFACE="

    if already_up "$addr"; then
        log "Modem/$MODEM already up: $addr/$prefix on $MODEM_IFACE (table $TABLE) — no change"
        return 0
    fi

    log "Modem/$MODEM bearer $bearer: $addr/$prefix gw $gw mtu $mtu dev $MODEM_IFACE"

    # interface
    ip link set "$MODEM_IFACE" up
    ip link set "$MODEM_IFACE" mtu "$mtu"
    ip addr flush dev "$MODEM_IFACE"
    ip addr add "$addr/$prefix" dev "$MODEM_IFACE"

    # source-gated policy route (does NOT touch the host default route)
    ip route flush table "$TABLE" 2>/dev/null || true
    ip route add default via "$gw" dev "$MODEM_IFACE" onlink table "$TABLE"
    while ip rule del priority "$RULE_PREF" 2>/dev/null; do :; done   # clear stale
    ip rule add from "$addr" lookup "$TABLE" priority "$RULE_PREF"

    # loose reverse-path filtering for the multi-homed return traffic
    sysctl -wq "net.ipv4.conf.$MODEM_IFACE.rp_filter=2" 2>/dev/null || true

    log "configured. host default route is untouched:"
    ip route show default | sed 's/^/setup-cellular-modem:   /'
}

if [[ "$WATCH" -eq 1 ]]; then
    log "watch mode — keeping the cellular bond leg up (every ${WATCH_INTERVAL}s)"
    while true; do
        # Subshell so a transient die() (modem briefly gone, bearer mid-reconnect)
        # only ends this cycle, not the daemon — we retry on the next tick.
        ( bring_up ) || log "bring_up failed this cycle — retrying in ${WATCH_INTERVAL}s"
        sleep "$WATCH_INTERVAL"
    done
else
    bring_up
    log "verifying egress (source-pinned)..."
    if ping -I "$MODEM_IFACE" -c 3 -W 3 8.8.8.8 >/dev/null 2>&1; then
        log "egress OK via $MODEM_IFACE"
    else
        log "WARNING: ping via $MODEM_IFACE failed — check signal / APN / CGNAT"
    fi
    log "done. For boot persistence + auto-reconnect, install the daemon:"
    log "  sudo packaging/install-cellular-modem.sh"
fi
