#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Bring a media-egress NIC up to broadcast-grade wire-pacing config:
# linuxptp (ptp4l + phc2sys), ETF qdisc, static ARP for known peers.
# All persistent (systemd) so it survives reboots.
#
# This script is OPTIONAL. It is only needed for:
#   - Tier 1 / 2 PCR_AC (sub-µs to ~10 µs) — needs ETF qdisc + PTP.
#   - ST 2110 essence flows — needs PTP grandmaster sync.
#   - Multi-edge 2022-7 hitless across legs — needs cross-edge PTP.
# A default `install-edge.sh` install runs at tier 4 (clock_nanosleep
# on SCHED_FIFO, ~50–500 µs jitter) which is fine for VLC, ffplay,
# OBS, web players, and most cloud receivers.
#
# Does NOT install bilbycast-edge — use packaging/install-edge.sh for
# the binary, or run this on a host that already has it. Idempotent.
#
# Usage:
#   sudo MEDIA_IFACE=eno4 \
#        PEERS="10.0.0.5=00:0e:c6:4a:53:06 10.0.0.10=00:11:22:33:44:55" \
#        bash provision-edge-node.sh
#
# Flags (set to 1 to skip):
#   PTP_ONLY=1   — install + enable only ptp4l/phc2sys; no ETF, no ARP.
#                  Safe to run on a NIC you also use for ssh/management.

set -euo pipefail

[[ $EUID -eq 0 ]] || { echo "must run as root" >&2; exit 1; }

: "${MEDIA_IFACE:?set MEDIA_IFACE (e.g. eno4)}"
: "${PEERS:=}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
step() { printf '\n=== %s ===\n' "$1"; }

ip link show "$MEDIA_IFACE" &>/dev/null || { echo "iface $MEDIA_IFACE not found" >&2; exit 1; }

# ─── 0. Prerequisite checks ──────────────────────────────────────────
step "0. Prerequisite checks"

# Kernel ≥ 4.19 required for SO_TXTIME + ETF qdisc.
KERNEL_VERSION=$(uname -r | cut -d. -f1,2)
KERNEL_MAJOR=${KERNEL_VERSION%.*}
KERNEL_MINOR=${KERNEL_VERSION#*.}
if (( KERNEL_MAJOR < 4 )) || { (( KERNEL_MAJOR == 4 )) && (( KERNEL_MINOR < 19 )); }; then
    echo "WARNING: kernel $(uname -r) is older than 4.19 — SO_TXTIME / ETF qdisc unavailable." >&2
    echo "         The edge will fall back to clock_nanosleep (tier 4)." >&2
fi

# HW PTP timestamping probe — informational only. Without it, you fall
# back to software ETF (tier 2) which is still ~1–10 µs jitter.
if command -v ethtool &>/dev/null; then
    if ethtool -T "$MEDIA_IFACE" 2>/dev/null | grep -q "PTP Hardware Clock: none"; then
        echo "WARNING: $MEDIA_IFACE has no PTP hardware clock." >&2
        echo "         Tier-1 (sub-µs) PCR_AC requires a HW-PTP NIC (Intel I225/I226/E810, Mellanox CX-6/7)." >&2
        echo "         You will get tier 2 (~1–10 µs) at best on this NIC." >&2
    fi
else
    echo "NOTE: ethtool not found; skipping NIC HW-PTP capability probe." >&2
fi

# ─── 1. linuxptp ──────────────────────────────────────────────────────
step "1. Installing linuxptp"
if ! command -v ptp4l &>/dev/null; then
    apt-get update -qq
    apt-get install -y -qq linuxptp
fi

# ─── 2. ptp4l@${MEDIA_IFACE} ──────────────────────────────────────────
step "2. ptp4l on $MEDIA_IFACE"
mkdir -p /etc/linuxptp
install -D -m 0644 /dev/stdin "/etc/linuxptp/ptp4l-${MEDIA_IFACE}.conf" <<EOF
[global]
logging_level             5
use_syslog                1
network_transport         L2
delay_mechanism           E2E
tx_timestamp_timeout      50
hwts_filter               full
socket_priority           4
[$MEDIA_IFACE]
EOF

# Override drop-in points the shipped ptp4l@.service at our config file.
mkdir -p "/etc/systemd/system/ptp4l@${MEDIA_IFACE}.service.d"
install -m 0644 /dev/stdin "/etc/systemd/system/ptp4l@${MEDIA_IFACE}.service.d/override.conf" <<EOF
[Service]
ExecStart=
ExecStart=/usr/sbin/ptp4l -f /etc/linuxptp/ptp4l-${MEDIA_IFACE}.conf -i ${MEDIA_IFACE}
EOF

# ─── 3. phc2sys (slave system clock to media-NIC PHC) ────────────────
#
# `-s %I`  — source clock is the interface's PHC (already PTP-synced by ptp4l).
# `-w`     — wait for ptp4l to be in sync, then auto-apply the TAI-UTC offset
#            advertised in PTP announce TLVs. This is the modern recipe — it
#            keeps CLOCK_REALTIME on UTC AND the kernel's TAI offset correct,
#            so CLOCK_TAI = grandmaster TAI (which is what bilbycast's
#            engine::wire_emit and engine::st2110 read).
# `-m`     — log to stdout for journald.
#
# DO NOT pass `-O 0`: it overrides the auto-detected TAI-UTC offset and either
# skews CLOCK_REALTIME by 37 s (UTC vs TAI) or leaves CLOCK_TAI 37 s ahead of
# the grandmaster, depending on phc2sys version. Either way, multi-edge
# coherence breaks. See: linuxptp `phc2sys(8)`, "AUTOMATIC PTP UTC OFFSET".
step "3. phc2sys"
install -m 0644 /dev/stdin "/etc/systemd/system/phc2sys@.service" <<'EOF'
[Unit]
Description=PTP hardware-to-system clock sync on %I
After=ptp4l@%i.service
Requires=ptp4l@%i.service

[Service]
Type=simple
ExecStart=/usr/sbin/phc2sys -s %I -w -m
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF

if [[ "${PTP_ONLY:-0}" != "1" ]]; then
# ─── 4. ETF qdisc persistence ────────────────────────────────────────
step "4. ETF qdisc on $MEDIA_IFACE (persistent)"
install -m 0755 "$SCRIPT_DIR/setup-etf-qdisc.sh" /usr/local/sbin/bilbycast-setup-etf-qdisc.sh
install -m 0644 /dev/stdin "/etc/systemd/system/bilbycast-etf@.service" <<'EOF'
[Unit]
Description=Apply bilbycast ETF qdisc on %I
After=network-online.target ptp4l@%i.service
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/bilbycast-setup-etf-qdisc.sh %I
ExecStop=/usr/sbin/tc qdisc del dev %I root

[Install]
WantedBy=multi-user.target
EOF

fi   # PTP_ONLY guard ends; ETF section above is skipped when PTP_ONLY=1

# ─── 5. Static ARP for peers ─────────────────────────────────────────
if [[ "${PTP_ONLY:-0}" != "1" && -n "$PEERS" ]]; then
    step "5. Static ARP for $PEERS"
    install -m 0755 /dev/stdin /usr/local/sbin/bilbycast-static-arp.sh <<'EOF'
#!/usr/bin/env bash
set -e
IFACE="$1"; shift
for entry in "$@"; do
    ip="${entry%%=*}"; mac="${entry##*=}"
    /sbin/ip neigh replace "$ip" lladdr "$mac" dev "$IFACE" nud permanent
done
EOF
    install -m 0644 /dev/stdin "/etc/systemd/system/bilbycast-arp@.service" <<EOF
[Unit]
Description=Static ARP for bilbycast peers on %I
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/bilbycast-static-arp.sh %I ${PEERS}

[Install]
WantedBy=multi-user.target
EOF
fi

# ─── 6. Enable + start ───────────────────────────────────────────────
step "6. Enabling services"
systemctl daemon-reload
systemctl enable --now "ptp4l@${MEDIA_IFACE}.service"
systemctl enable --now "phc2sys@${MEDIA_IFACE}.service"
if [[ "${PTP_ONLY:-0}" != "1" ]]; then
    systemctl enable --now "bilbycast-etf@${MEDIA_IFACE}.service"
    [[ -n "$PEERS" ]] && systemctl enable --now "bilbycast-arp@${MEDIA_IFACE}.service"
fi

# ─── 7. Verify ───────────────────────────────────────────────────────
step "7. Verification (give it ~5 s to settle)"
sleep 5
for svc in "ptp4l@${MEDIA_IFACE}" "phc2sys@${MEDIA_IFACE}" "bilbycast-etf@${MEDIA_IFACE}"; do
    printf '  %-32s %s\n' "$svc" "$(systemctl is-active "${svc}.service" || true)"
done
echo
echo "-- ptp4l recent log --"
journalctl -u "ptp4l@${MEDIA_IFACE}.service" -n 5 --no-pager || true
echo
echo "-- phc2sys recent log --"
journalctl -u "phc2sys@${MEDIA_IFACE}.service" -n 5 --no-pager || true
echo
echo "-- ETF qdisc --"
tc -s qdisc show dev "$MEDIA_IFACE" | head -16

cat <<EOF

Done. Healthy signs:
  - ptp4l: 'port 1: ... to MASTER' or 'to SLAVE' (not stuck in LISTENING)
  - phc2sys: offset converging toward 0, state 's2'
  - etf: 'offload on', drops counter near 0 once edge is sending
EOF
