#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Bring a media-egress NIC up to broadcast-grade wire-pacing config:
# linuxptp (ptp4l + phc2sys), ETF qdisc, file-capability grant on the
# edge binary, optional static ARP for known peers. All persistent
# (systemd / xattrs) so it survives reboots.
#
# This script is OPTIONAL for soft-receiver use cases (VLC, ffplay,
# OBS, web players, most cloud) — a default `install-edge.sh` install
# already runs at tier 4 (clock_nanosleep on SCHED_FIFO, ~50–500 µs
# jitter) without it. It IS REQUIRED for:
#   - Tier 1 / 2 PCR_AC (sub-µs to ~10 µs) — needs ETF qdisc + PTP
#     + CAP_NET_ADMIN.
#   - ST 2110 essence flows — needs PTP grandmaster sync.
#   - Multi-edge 2022-7 hitless across legs — needs cross-edge PTP.
#
# Does NOT install bilbycast-edge — use packaging/install-edge.sh for
# the binary, or run this on a host that already has it. Idempotent —
# safe to re-run after every upgrade or on every fresh box.
#
# Usage:
#   sudo MEDIA_IFACE=eno4 \
#        PEERS="10.0.0.5=00:0e:c6:4a:53:06 10.0.0.10=00:11:22:33:44:55" \
#        bash provision-edge-node.sh
#
# Optional env vars:
#   BILBYCAST_BIN=/path/to/bilbycast-edge
#                — explicit path to the edge binary to setcap. Default
#                  is `/opt/bilbycast/edge/current/bilbycast-edge`
#                  (where install-edge.sh lays it down). For dev /
#                  testbed runs, point this at your `target/release/`
#                  or `target-full/release/` build. The script auto-
#                  follows symlinks so the cap lands on the real
#                  versioned binary, not the `current` symlink.
#
# Flags (set to 1 to skip):
#   PTP_ONLY=1   — install + enable only ptp4l/phc2sys; no ETF, no
#                  ARP, no setcap. Safe to run on a NIC you also use
#                  for ssh/management.
#   SKIP_SETCAP=1
#                — don't apply file capabilities to the binary. Use
#                  when the binary already has them (e.g. you rely on
#                  systemd `AmbientCapabilities=CAP_NET_ADMIN` from
#                  the shipped unit), or when the install path isn't
#                  yet present and you'll re-run later.
#   NO_GM=1      — no external PTP grandmaster on the network.
#                  Auto-detected when unset (see step 1b probe); set
#                  explicitly only to override (NO_GM=0 forces tier-1
#                  even if the probe sees no GM, NO_GM=1 skips the
#                  probe and goes straight to tier-2). When active:
#                  skips enabling ptp4l@ (it would sit forever in
#                  LISTENING). Switches phc2sys@ to standalone manual
#                  mode (CLOCK_REALTIME→PHC with +37 s TAI offset) so
#                  the NIC PHC tracks CLOCK_TAI from the local system
#                  clock instead of an external GM. Switches ETF
#                  qdisc to software-only (no NIC HW offload) since
#                  offload demands a GM-disciplined PHC. Yields tier-2
#                  precision (~1–10 µs jitter) — fine for compressed-
#                  TS broadcast, not for ST 2110-21 narrow-profile.

set -euo pipefail

[[ $EUID -eq 0 ]] || { echo "must run as root" >&2; exit 1; }

: "${MEDIA_IFACE:?set MEDIA_IFACE (e.g. eno4)}"
: "${PEERS:=}"
: "${BILBYCAST_BIN:=/opt/bilbycast/edge/current/bilbycast-edge}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
step() { printf '\n=== %s ===\n' "$1"; }

ip link show "$MEDIA_IFACE" &>/dev/null || { echo "iface $MEDIA_IFACE not found" >&2; exit 1; }

# Portable replacement for `install -m MODE /dev/stdin DEST <<EOF ... EOF`.
# Some coreutils + sudo combos refuse the install + /dev/stdin pairing with
# a misleading `install: No such file or directory`; `cat > DEST` is
# universally supported.
write_file() {
    local mode="$1"; local dest="$2"
    mkdir -p "$(dirname "$dest")"
    cat > "$dest"
    chmod "$mode" "$dest"
}

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

# ─── 1b. Probe link for a PTP grandmaster ────────────────────────────
#
# Tier-1 setup (NIC-offload ETF qdisc + phc2sys auto-mode + ptp4l) only
# works when a grandmaster is announcing on the link. Without one, the
# PHC drifts freely and every paced packet lands outside the NIC's
# launch horizon and is silently dropped at the qdisc — wire goes
# completely silent. Probe the link briefly; if no GM Announce arrives
# within ~6 s (>2× the default 1 Hz Announce interval), auto-flip to
# NO_GM=1 (tier-2 software ETF). Override with NO_GM=0 (force tier-1
# regardless) or NO_GM=1 (skip the probe, go straight to tier-2).
#
# Only `port 1: ... to SLAVE` indicates an external GM. `to MASTER`
# means BMCA elected this host (no external clock saw us as worse),
# which still leaves us GM-less.
if [[ -z "${NO_GM:-}" ]]; then
    step "1b. Probing $MEDIA_IFACE for a PTP grandmaster"
    PROBE_LOG=$(mktemp)
    # -2 matches the L2 transport written into the real ptp4l config
    # below. If your GM uses UDP transport instead, set NO_GM=0 to
    # force tier-1 and skip this probe.
    timeout 6 ptp4l -i "$MEDIA_IFACE" -m -2 >"$PROBE_LOG" 2>&1 || true
    if grep -qE "port 1: .* to SLAVE" "$PROBE_LOG"; then
        echo "  ✓ PTP grandmaster detected on $MEDIA_IFACE — tier-1 mode"
        NO_GM=0
    else
        echo "  ! no PTP grandmaster on $MEDIA_IFACE — falling back to tier-2 (software ETF)"
        echo "    Override: NO_GM=0 forces tier-1 anyway. NO_GM=1 skips the probe entirely."
        NO_GM=1
    fi
    rm -f "$PROBE_LOG"
fi

# ─── 2. ptp4l@${MEDIA_IFACE} ──────────────────────────────────────────
step "2. ptp4l on $MEDIA_IFACE"
write_file 0644 "/etc/linuxptp/ptp4l-${MEDIA_IFACE}.conf" <<EOF
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
write_file 0644 "/etc/systemd/system/ptp4l@${MEDIA_IFACE}.service.d/override.conf" <<EOF
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
# DO NOT pass `-O <n>` (any value, not just 0). Modern linuxptp's `-a` mode
# rejects manual `-O` outright ("autoconfiguration cannot be mixed with manual
# config options") — the daemon exits 255 in a tight crash-loop, the PHC is
# never disciplined, and with `offload on` on the ETF qdisc the wire goes
# silent. Older versions silently skewed CLOCK_REALTIME by the TAI-UTC offset
# or left CLOCK_TAI N seconds ahead of the grandmaster, breaking multi-edge
# coherence either way. See: linuxptp `phc2sys(8)`, "AUTOMATIC PTP UTC OFFSET".
step "3. phc2sys"
if [[ "${NO_GM:-0}" == "1" ]]; then
    # Explicit no-PTP mode: ptp4l isn't running at all (see step 6),
    # so phc2sys runs in pure manual mode. Bridges CLOCK_REALTIME → PHC
    # with +37 s TAI offset (PHC ends up in TAI domain), and sets the
    # kernel TAI offset = 37 via adjtimex so CLOCK_TAI = CLOCK_REALTIME
    # + 37 system-wide.
    #
    # Direction is critical: `-c %I -s CLOCK_REALTIME` means **target =
    # PHC, source = CLOCK_REALTIME** — phc2sys READS from the NTP-
    # disciplined CLOCK_REALTIME (managed by chrony/timesyncd) and
    # WRITES the PHC. CLOCK_REALTIME is untouched, so chrony and
    # phc2sys do not fight. If you accidentally reverse this to
    # `-c CLOCK_REALTIME -s %I`, phc2sys will pull CLOCK_REALTIME
    # toward the free-running PHC while chrony pulls it back — the
    # system clock jitters by ~ms over short timeframes, CLOCK_TAI
    # follows, every paced packet's SO_TXTIME goes stale by the time
    # it reaches the ETF qdisc, and the qdisc drops everything within
    # seconds.
    write_file 0644 "/etc/systemd/system/phc2sys@.service" <<'EOF'
[Unit]
Description=PTP hardware-to-system clock sync on %I (NO_GM forced)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/sbin/phc2sys -c %I -s CLOCK_REALTIME -O 37 -m
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF
else
    # GM-confirmed branch (step 1b proved a grandmaster is on the link).
    # ptp4l elects SLAVE via BMCA; phc2sys with `-a` reads port state +
    # parent dataset via the ptp4l management socket and:
    #   - port SLAVE  → sync system FROM PHC (PHC disciplined by GM,
    #                   TAI-UTC offset learned from PTP Announce TLVs)
    #   - port MASTER → sync PHC FROM system (CLOCK_REALTIME as ref,
    #                   via -rr) — only triggers if the GM disappears
    write_file 0644 "/etc/systemd/system/phc2sys@.service" <<'EOF'
[Unit]
Description=PTP hardware-to-system clock sync on %I
After=ptp4l@%i.service
Requires=ptp4l@%i.service

[Service]
Type=simple
# -a:   automatic mode. Queries ptp4l for port states AND learns the
#       TAI-UTC offset from PTP Announce TLVs via the management API.
#       Do NOT add `-O <n>` here — modern linuxptp rejects -a + -O as
#       "autoconfiguration cannot be mixed with manual config options",
#       phc2sys exits 255, the PHC is never disciplined, and combined
#       with `offload on` on the ETF qdisc the wire goes silent.
# -rr:  permit CLOCK_REALTIME as a fallback reference if the SLAVE port
#       drops out briefly (> 8 s). The step 1b probe guarantees a GM
#       was present when this branch was selected, so the steady state
#       is SLAVE; -rr only covers transient GM dropouts.
ExecStart=/usr/sbin/phc2sys -a -rr -m
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF
fi

if [[ "${PTP_ONLY:-0}" != "1" ]]; then
# ─── 4. ETF qdisc persistence ────────────────────────────────────────
#
# The shipped setup-etf-qdisc.sh installs with `skip_sock_check` — this
# is non-negotiable because the priomap routes default-priority traffic
# (ARP, ssh, every default UDP socket the kernel issues) into the etf-
# bearing TC. Without `skip_sock_check`, etf drops every non-SO_TXTIME
# packet at the qdisc — including ARP, which then never resolves the
# next-hop MAC and every sendmsg to that interface returns ENETUNREACH.
# Always copy the latest setup script over the persistent path so a
# reboot reapplies the corrected qdisc, even if the previous install
# laid down an old (pre-skip_sock_check) version.
step "4. ETF qdisc on $MEDIA_IFACE (persistent)"
install -m 0755 "$SCRIPT_DIR/setup-etf-qdisc.sh" /usr/local/sbin/bilbycast-setup-etf-qdisc.sh
# When NO_GM=1, force software ETF (no NIC offload). The kernel-offload
# path needs a GM-disciplined PHC; without one, every paced packet
# lands outside the NIC's launch horizon and is silently dropped.
# Software ETF on CLOCK_TAI still delivers ~1–10 µs jitter, which is
# tier-2 — broadcast-quality for compressed TS.
if [[ "${NO_GM:-0}" == "1" ]]; then
    ETF_AFTER="network-online.target"
    ETF_ENV="Environment=BILBYCAST_ETF_OFFLOAD=0"
else
    ETF_AFTER="network-online.target ptp4l@%i.service"
    ETF_ENV=""
fi
write_file 0644 "/etc/systemd/system/bilbycast-etf@.service" <<EOF
[Unit]
Description=Apply bilbycast ETF qdisc on %I
After=${ETF_AFTER}
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
${ETF_ENV}
ExecStart=/usr/local/sbin/bilbycast-setup-etf-qdisc.sh %I
ExecStop=/usr/sbin/tc qdisc del dev %I root

[Install]
WantedBy=multi-user.target
EOF

fi   # PTP_ONLY guard ends; ETF section above is skipped when PTP_ONLY=1

# ─── 4b. CAP_NET_ADMIN on the edge binary ────────────────────────────
#
# Mainline kernel ≥ 6.x (and every recent Ubuntu / RHEL backport) gates
# setsockopt(SO_TXTIME) on any non-CLOCK_MONOTONIC clockid behind
# CAP_NET_ADMIN. Wire pacing always uses CLOCK_TAI (the only clockid
# the etf qdisc on ice/igc/igb/mlx5 accepts), so without this cap the
# SO_TXTIME probe returns EPERM, wire-emit silently falls back to the
# clock_nanosleep tier, and on a host with the etf qdisc above those
# fallback packets are then ALSO dropped at the qdisc — the wire goes
# completely silent. setcap on the binary file persists across reboots
# via filesystem xattrs.
#
# Production (install-edge.sh + shipped systemd unit) gets the same
# privileges via `AmbientCapabilities=CAP_NET_ADMIN`. Apply the file
# capability here anyway — it's harmless when systemd is also granting
# it, and it covers ad-hoc invocations (`systemctl start` with an
# override, direct `./bilbycast-edge` runs, debug sessions).
#
# Re-run this step after every `cargo build` — each rebuild produces a
# fresh binary file with empty xattrs.
if [[ "${PTP_ONLY:-0}" != "1" && "${SKIP_SETCAP:-0}" != "1" ]]; then
    step "4b. File capabilities on $BILBYCAST_BIN"
    if [[ -L "$BILBYCAST_BIN" ]]; then
        # setcap on a symlink writes to the link target — but resolve
        # it ourselves so a stale symlink (e.g. pointing into an old
        # `versions/` dir that was GC'd) fails loud here instead of
        # silently leaving the real binary uncapped.
        TARGET=$(readlink -f "$BILBYCAST_BIN")
        if [[ -z "$TARGET" || ! -x "$TARGET" ]]; then
            echo "  ✗ symlink target $BILBYCAST_BIN -> $TARGET is missing or not executable" >&2
            echo "  → fix the install (re-run install-edge.sh) and re-run this script." >&2
            exit 1
        fi
        BILBYCAST_BIN="$TARGET"
    fi
    if [[ ! -x "$BILBYCAST_BIN" ]]; then
        echo "  ! binary $BILBYCAST_BIN not found / not executable" >&2
        echo "    skipping setcap. Either:" >&2
        echo "      - point BILBYCAST_BIN at your build (e.g.  BILBYCAST_BIN=\$PWD/target/release/bilbycast-edge)" >&2
        echo "      - or rerun this script after install-edge.sh lands the binary" >&2
        echo "      - or set SKIP_SETCAP=1 if you rely on the systemd unit's AmbientCapabilities" >&2
    else
        # cap_net_admin: SO_TXTIME(CLOCK_TAI). cap_sys_nice: SCHED_FIFO
        # priority change (LimitRTPRIO=50 covers it under systemd; setcap
        # covers it for ad-hoc invocations).
        setcap cap_net_admin,cap_sys_nice+ep "$BILBYCAST_BIN"
        echo "  ✓ setcap cap_net_admin,cap_sys_nice+ep $BILBYCAST_BIN"
        getcap "$BILBYCAST_BIN"
    fi
fi

# ─── 5. Static ARP for peers ─────────────────────────────────────────
if [[ "${PTP_ONLY:-0}" != "1" && -n "$PEERS" ]]; then
    step "5. Static ARP for $PEERS"
    write_file 0755 /usr/local/sbin/bilbycast-static-arp.sh <<'EOF'
#!/usr/bin/env bash
set -e
IFACE="$1"; shift
for entry in "$@"; do
    ip="${entry%%=*}"; mac="${entry##*=}"
    /sbin/ip neigh replace "$ip" lladdr "$mac" dev "$IFACE" nud permanent
done
EOF
    write_file 0644 "/etc/systemd/system/bilbycast-arp@.service" <<EOF
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
#
# `enable --now` starts a stopped service but does NOT restart one
# that's already running — so on a re-run with updated unit content
# (e.g. fixing a stale `phc2sys -O 0` ExecStart, or swapping in the
# new setup-etf-qdisc.sh with skip_sock_check), the running daemon
# would keep the old args until the next reboot. Explicit restart
# guarantees re-runs of this script actually apply.
step "6. Enabling services"
systemctl daemon-reload

if [[ "${NO_GM:-0}" == "1" ]]; then
    # No external GM: stop+disable ptp4l (it would block in LISTENING
    # forever and waste a socket). phc2sys runs in standalone mode
    # against the system clock — no ptp4l dependency.
    systemctl disable --now "ptp4l@${MEDIA_IFACE}.service" 2>/dev/null || true
    systemctl enable "phc2sys@${MEDIA_IFACE}.service"
    systemctl restart "phc2sys@${MEDIA_IFACE}.service"
else
    # GM-aware mode: ptp4l elects to MASTER or SLAVE via BMCA against
    # the network; phc2sys (with -w) waits for ptp4l to declare sync,
    # then drives system clock from PHC.
    for svc in "ptp4l@${MEDIA_IFACE}.service" "phc2sys@${MEDIA_IFACE}.service"; do
        systemctl enable "$svc"
        systemctl restart "$svc"
    done
fi

if [[ "${PTP_ONLY:-0}" != "1" ]]; then
    systemctl enable "bilbycast-etf@${MEDIA_IFACE}.service"
    systemctl restart "bilbycast-etf@${MEDIA_IFACE}.service"
    if [[ -n "$PEERS" ]]; then
        systemctl enable "bilbycast-arp@${MEDIA_IFACE}.service"
        systemctl restart "bilbycast-arp@${MEDIA_IFACE}.service"
    fi
fi

# ─── 7. Verify ───────────────────────────────────────────────────────
step "7. Verification (give it ~5 s to settle)"
sleep 5
if [[ "${NO_GM:-0}" == "1" ]]; then
    VERIFY_UNITS=("phc2sys@${MEDIA_IFACE}" "bilbycast-etf@${MEDIA_IFACE}")
else
    VERIFY_UNITS=("ptp4l@${MEDIA_IFACE}" "phc2sys@${MEDIA_IFACE}" "bilbycast-etf@${MEDIA_IFACE}")
fi
for svc in "${VERIFY_UNITS[@]}"; do
    printf '  %-32s %s\n' "$svc" "$(systemctl is-active "${svc}.service" || true)"
done
if [[ "${NO_GM:-0}" != "1" ]]; then
    echo
    echo "-- ptp4l recent log (look for 'to MASTER' or 'to SLAVE') --"
    journalctl -t ptp4l -n 5 --no-pager 2>/dev/null \
        || journalctl -u "ptp4l@${MEDIA_IFACE}.service" -n 5 --no-pager \
        || true
fi
echo
echo "-- phc2sys recent log --"
journalctl -u "phc2sys@${MEDIA_IFACE}.service" -n 5 --no-pager || true
echo
echo "-- ETF qdisc --"
tc -s qdisc show dev "$MEDIA_IFACE" | head -16

echo
if [[ "${NO_GM:-0}" == "1" ]]; then
    cat <<EOF
Done. Mode: NO_GM (no external PTP grandmaster). Healthy signs:
  - phc2sys: 'sys offset … s2' (PHC tracking CLOCK_TAI via system clock)
  - etf qdisc: 'clockid TAI delta 200000 skip_sock_check on' (software ETF, no 'offload on')
  - Drops counter near 0 once the edge is sending

Tier-2 wire pacing (~1–10 µs jitter) — broadcast-quality for compressed TS.
For tier-1 sub-µs PCR_AC, re-run this script without NO_GM=1 once a GM
is on the network.
EOF
else
    cat <<EOF
Done. Mode: GM-aware (external PTP grandmaster expected). Healthy signs:
  - ptp4l: 'port 1: ... to MASTER' or 'to SLAVE' (NOT stuck in LISTENING).
    If stuck: there's no GM on this link — re-run with NO_GM=1.
  - phc2sys: offset converging toward 0, state 's2'
  - etf qdisc: 'clockid TAI delta 200000 offload on skip_sock_check on'
  - Drops counter near 0 once the edge is sending
EOF
fi
