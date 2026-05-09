#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Install the ETF (Earliest TxTime First) qdisc on a chosen NIC so the
# bilbycast-edge ST 2110-20 / -23 outputs can use SO_TXTIME pacing.
#
# Why this script lives outside the edge:
#
#   - `tc qdisc` requires `CAP_NET_ADMIN`, deliberately not granted to
#     the edge process. Operator-side setup keeps the edge's blast
#     radius small and avoids needing a privileged daemon.
#   - The qdisc layout is a deployment decision (which NICs, which
#     traffic classes, which clock source). One-size-fits-all is wrong
#     for production; this script is an opinionated default that works
#     on commodity hardware with a single egress NIC. Operators with
#     bonded interfaces, SR-IOV, or per-tenant priority maps should
#     adapt this template.
#   - Persistence across reboot is also operator-policy: this script
#     applies the qdisc *now*; operators wanting boot-time persistence
#     should wrap the same `tc` calls in a systemd unit or a
#     post-up hook on the NIC's network config.
#
# Prerequisites:
#
#   - Linux kernel ≥ 4.19
#   - `iproute2` ≥ 4.20 (for `etf` qdisc support; check `tc -V`)
#   - PTP discipline running on the system clock — either
#     `ptp4l` + `phc2sys`, or a similar GM-aligned setup. The ETF qdisc
#     pulls its reference from `clockid` (`CLOCK_TAI` here); without
#     PTP the system TAI clock is just wall time + leap-second offset
#     and the receiver-side ST 2110-21 narrow profile bounds will fail.
#   - For HW offload (`offload` flag below): a NIC with PTP-disciplined
#     hardware tx timestamping. Tested NIC families:
#       * Mellanox CX-6, CX-7 (mlx5_core)
#       * Intel E810 (ice driver)
#       * Intel i210 (igb driver)
#     Other NICs may work but the kernel falls back to software ETF if
#     HW offload isn't available — still ~1–10 µs jitter, an order of
#     magnitude better than no pacing.
#
# Usage:
#
#   sudo bash packaging/setup-etf-qdisc.sh enp1s0
#
# To remove (revert to default `pfifo_fast`):
#
#   sudo tc qdisc del dev enp1s0 root
#
# After installation, verify:
#
#   tc -s qdisc show dev enp1s0
#
# expected to list `mqprio` at root and `etf` on the prioritized class.

set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "Usage: sudo $0 <interface>" >&2
    echo "  e.g. sudo $0 enp1s0" >&2
    exit 1
fi

IF="$1"

if [[ $EUID -ne 0 ]]; then
    echo "$0: must run as root (tc qdisc requires CAP_NET_ADMIN)" >&2
    exit 1
fi

if ! ip link show "$IF" &>/dev/null; then
    echo "$0: interface '$IF' not found" >&2
    exit 1
fi

if ! command -v tc &>/dev/null; then
    echo "$0: 'tc' not found — install iproute2" >&2
    exit 1
fi

# 1. Multiqueue priority qdisc at root: 3 traffic classes mapped to 3
#    HW queues (assumes the NIC has at least 3 tx queues — typical on
#    modern multi-Gbps cards). Class 0 carries the paced ST 2110
#    traffic, classes 1+2 are spare for non-paced bulk + control.
echo "$0: installing mqprio root qdisc on $IF"
tc qdisc replace dev "$IF" root handle 100: mqprio \
    num_tc 3 \
    map 0 0 0 0 1 1 1 1 2 2 2 2 0 0 0 0 \
    queues 1@0 1@1 1@2 \
    hw 0 || {
        echo "$0: mqprio install failed — NIC may not have ≥ 3 tx queues" >&2
        exit 1
    }

# 2. ETF (Earliest TxTime First) qdisc on the paced class. `clockid
#    CLOCK_TAI` aligns with the SO_TXTIME timestamps the edge sends.
#    `delta 200000` (200 µs) is the kernel's pre-emit lookahead — the
#    ETF qdisc wakes 200 µs before the target tx time to prep the
#    packet. `offload` enables HW-offload on supported NICs;
#    silently degrades to software ETF on unsupported NICs.
echo "$0: installing etf qdisc on $IF class 100:1 (clockid CLOCK_TAI, offload)"
tc qdisc replace dev "$IF" parent 100:1 etf \
    clockid CLOCK_TAI \
    delta 200000 \
    offload || {
        echo "$0: ETF install with offload failed; retrying without offload" >&2
        tc qdisc replace dev "$IF" parent 100:1 etf \
            clockid CLOCK_TAI \
            delta 200000 \
            || {
                echo "$0: ETF qdisc install failed entirely; check kernel version (≥ 4.19) and iproute2 version" >&2
                exit 1
            }
    }

echo
echo "$0: ETF qdisc installed on $IF. Current state:"
tc -s qdisc show dev "$IF"
echo
echo "$0: done. Restart bilbycast-edge so its outputs pick up the new qdisc."
echo "$0: persistent install (across reboots) is operator policy — wrap this"
echo "$0: script in a systemd unit, NetworkManager dispatch hook, or"
echo "$0: ifupdown post-up snippet, depending on your distro."
