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
#
# PTP is NOT required for the default software ETF mode. Without PTP,
# the system TAI clock is wall time + leap-second offset — good enough
# for software-ETF pacing (~1–10 µs jitter). PTP is only needed for:
#   - HW offload (`BILBYCAST_ETF_OFFLOAD=1`) — sub-µs jitter
#   - ST 2110-21 narrow profile receiver-side VRX bound compliance
#
# For HW offload (`BILBYCAST_ETF_OFFLOAD=1`): requires a PTP-disciplined
# NIC with `ptp4l` + `phc2sys` running in TAI domain. Without PHC sync,
# HW offload silently drops every packet. Tested NIC families:
#       * Mellanox CX-6, CX-7 (mlx5_core)
#       * Intel E810 (ice driver)
#       * Intel i210 (igb driver)
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
#    traffic, classes 1+2 take everything else.
#
#    The priority map sends ONLY socket-priority 5 to the etf class
#    (TC0). Priority 5 is unreachable via DSCP marking — the kernel's
#    `ip_tos2prio` table only ever derives priorities 0/2/4/6 — so the
#    etf class is hit exclusively by sockets that opt in with an
#    explicit SO_PRIORITY=5 (the edge's SO_TXTIME outputs, via
#    BILBYCAST_ETF_SO_PRIORITY=5). Default-priority traffic — ARP,
#    ICMP, IGMP joins, TCP, mDNS, any unmarked socket — rides the
#    fq_codel classes and the NIC behaves like a normal interface.
#
#    The previous map (`0 0 0 0 1 1 1 1 2 2 2 2 0 0 0 0`) routed
#    priority 0 onto the etf class, which SILENTLY BLACKHOLED all
#    unstamped egress on the NIC (measured 2026-06-06: ARP resolution
#    FAILED, ping 100 % loss, IGMP membership reports eaten — the etf
#    class showed `Sent 0` with a continuously climbing drop counter).
#    See the skip_sock_check note below for why the old map's safety
#    assumption did not hold.
#
#    REQUIRED COMPANION: run the edge with BILBYCAST_ETF_SO_PRIORITY=5
#    when BILBYCAST_ENABLE_TXTIME=1, otherwise SO_TXTIME outputs land
#    on fq_codel (priority 0 default) and lose etf launch-time pacing.
echo "$0: installing mqprio root qdisc on $IF (etf class reachable only via SO_PRIORITY=5)"
tc qdisc replace dev "$IF" root handle 100: mqprio \
    num_tc 3 \
    map 1 1 1 1 1 0 1 1 2 2 2 2 1 1 1 1 \
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
#
#    `skip_sock_check` only skips the per-SOCKET validation (the
#    SOCK_TXTIME flag / clockid / deadline_mode match). It does NOT
#    exempt a packet from the per-packet txtime validity check: a
#    packet with no timestamp (skb->tstamp == 0 — ARP, ICMP, TCP,
#    IGMP, any unmarked socket) has a "launch time" of 1970, which is
#    in the past, and etf DROPS it on enqueue. The earlier belief that
#    such packets "fall through to FIFO release and do leave the box"
#    was measured FALSE on kernel 7.x (2026-06-06: etf class Sent 0,
#    drop counter climbing; ARP FAILED; ping 100 % loss). That is why
#    the priomap above keeps all default-priority traffic OFF the etf
#    class entirely — `skip_sock_check` is retained only so legitimate
#    SO_TXTIME senders with mismatched socket flags aren't rejected.
# NIC-offload mode programs the NIC's hardware launch register in PHC
# time. The kernel translates the per-packet CLOCK_TAI tx-time into PHC
# time using its known TAI↔PHC offset, which is only correct when
# phc2sys is actively keeping PHC ≈ CLOCK_TAI (ptp4l locked to a GM, or
# phc2sys in standalone mode bridging CLOCK_REALTIME→PHC). When PHC
# freewheels (no PTP daemon yet, or daemon stuck waiting for a GM
# announce), the launch register sees timestamps tens of seconds
# outside its ~1 s horizon and the NIC silently rejects every packet.
#
# Default: software ETF (`BILBYCAST_ETF_OFFLOAD=0`). Safe everywhere —
# no PTP, no PHC sync, no silent packet drops. Gives ~1–10 µs jitter
# (broadcast-quality for compressed TS and ST 2110 wide profile).
#
# Operators with a PTP grandmaster + HW-PTP NIC + confirmed PHC sync
# (`phc2sys` in TAI domain) set `BILBYCAST_ETF_OFFLOAD=1` for sub-µs
# jitter (tier 1). Without PHC sync, HW offload silently drops every
# packet — the NIC rejects launch timestamps outside its ~1 s horizon.
USE_OFFLOAD="${BILBYCAST_ETF_OFFLOAD:-0}"
if [[ "$USE_OFFLOAD" == "1" ]]; then
    echo "$0: installing etf qdisc on $IF class 100:1 (clockid CLOCK_TAI, offload, skip_sock_check)"
    tc qdisc replace dev "$IF" parent 100:1 etf \
        clockid CLOCK_TAI \
        delta 200000 \
        offload \
        skip_sock_check || {
            echo "$0: ETF install with offload failed; retrying without offload" >&2
            tc qdisc replace dev "$IF" parent 100:1 etf \
                clockid CLOCK_TAI \
                delta 200000 \
                skip_sock_check \
                || {
                    echo "$0: ETF qdisc install failed entirely; check kernel version (≥ 4.19) and iproute2 version" >&2
                    exit 1
                }
        }
else
    echo "$0: installing etf qdisc on $IF class 100:1 (clockid CLOCK_TAI, software ETF, skip_sock_check)"
    echo "$0:   BILBYCAST_ETF_OFFLOAD=0 — skipping NIC HW offload (safe without PTP grandmaster)"
    tc qdisc replace dev "$IF" parent 100:1 etf \
        clockid CLOCK_TAI \
        delta 200000 \
        skip_sock_check || {
            echo "$0: ETF qdisc install failed; check kernel version (≥ 4.19) and iproute2 version" >&2
            exit 1
        }
fi

# 3. PTP coexistence — keep PTP traffic OUT of the ETF class.
#
#    The mqprio priomap above sends socket-priority 0 (the default for ALL
#    traffic, including ptp4l's Sync/Delay_Req — they carry no SO_TXTIME)
#    into the ETF class 100:1. On some kernels (verified broken on Ubuntu
#    7.0.0-15 / ice / E810, May 2026) ETF does NOT release those zero-txtime
#    packets even with skip_sock_check, so ptp4l's Sync never egresses and the
#    grandmaster flaps MASTER<->FAULTY with "timed out polling for tx
#    timestamp". That makes ETF and a PTP master/slave mutually exclusive on
#    the same NIC — which is fatal, because ST 2110 needs BOTH.
#
#    Fix: a clsact EGRESS filter that matches PTP event (UDP 319) + general
#    (UDP 320) and rewrites the skb priority to PTP_BYPASS_PRIO, which the
#    priomap routes to a NON-ETF traffic class (fq_codel). The clsact egress
#    hook runs in dev_queue_xmit BEFORE mqprio picks the queue, so the
#    priority rewrite actually changes the class. Media (other UDP ports)
#    stays priority 0 -> ETF and keeps its SO_TXTIME pacing; only PTP is
#    carved out. PTP then egresses normally and gets its TX timestamp.
#
#    PTP_BYPASS_PRIO defaults to 4 -> tc1 per the priomap "0 0 0 0 1 1 1 1..".
#    Set BILBYCAST_SKIP_PTP_BYPASS=1 to skip (e.g. NICs that never carry PTP).
PTP_BYPASS_PRIO="${PTP_BYPASS_PRIO:-4}"
if [[ "${BILBYCAST_SKIP_PTP_BYPASS:-0}" != "1" ]]; then
    echo "$0: installing clsact PTP-bypass filter (udp/319,320 -> skb-priority $PTP_BYPASS_PRIO, non-ETF tc)"
    tc qdisc del dev "$IF" clsact 2>/dev/null || true
    if tc qdisc add dev "$IF" clsact 2>/dev/null; then
        for port in 319 320; do
            if tc filter add dev "$IF" egress protocol ip prio 1 \
                    flower ip_proto udp dst_port "$port" \
                    action skbedit priority "$PTP_BYPASS_PRIO" 2>/dev/null; then
                echo "$0:   PTP udp/$port -> non-ETF tc OK"
            else
                echo "$0:   WARNING: failed to add bypass filter for udp/$port (cls_flower / act_skbedit missing?)" >&2
            fi
        done
    else
        echo "$0: WARNING: clsact unsupported on this kernel — PTP will flap if it shares this NIC with ETF" >&2
    fi
fi

echo
echo "$0: ETF qdisc installed on $IF. Current state:"
tc -s qdisc show dev "$IF"
echo
echo "$0: done. Restart bilbycast-edge so its outputs pick up the new qdisc."
echo "$0: persistent install (across reboots) is operator policy — wrap this"
echo "$0: script in a systemd unit, NetworkManager dispatch hook, or"
echo "$0: ifupdown post-up snippet, depending on your distro."
