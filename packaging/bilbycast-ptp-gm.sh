#!/usr/bin/env bash
# bilbycast-ptp-gm.sh — local PTP grandmaster / slave / auto for bilbycast.
#
# Runs ptp4l (and phc2sys on HW-PTP interfaces) in the operator-selected
# role so bilbycast's ST 2110, MXL, and PCR-locked TS paths share a
# single, stable clock.
#
# Modes (--mode):
#   grandmaster  - This node IS the master clock (priority1=128).
#                  Other devices on the same domain slave to us.
#                  Default if --mode unset (back-compat with first
#                  shipped behaviour).
#   slave-only   - This node MUST slave to an external grandmaster
#                  (priority1=255, slaveOnly=1). Refuses to ever
#                  become master under BMCA. Use this when a
#                  customer audit trail says "this device may never
#                  be the time source".
#   auto         - Listen for PTP Announce on the chosen interface
#                  for --scan-timeout seconds (default 5). If a peer
#                  is heard with a better BMCA score, start in
#                  slave-only mode. Otherwise start as grandmaster.
#                  Operator-friendly default for plug-and-play
#                  deployments.
#
# Tiers picked automatically based on the interface:
#   HW PHC NIC (eno1/eno2/eno4 here)  -> hardware timestamping. phc2sys
#                                        direction depends on the role:
#                                        grandmaster = PHC <- system
#                                        clock (chrony/NTP stays the
#                                        sole CLOCK_REALTIME owner);
#                                        slave-only = CLOCK_REALTIME <-
#                                        fabric PHC (NTP daemons must
#                                        be disabled — see the
#                                        "System-clock ownership" note
#                                        in docs/ptp.md). Sub-µs floor;
#                                        right for ST 2110-21 narrow VRX
#                                        and tier-1 PCR_AC measurement.
#   Software-only NIC (eno3 here)     -> kernel software timestamping.
#                                        ~tens-of-microseconds jitter;
#                                        fine for functional testing.
#
# How bilbycast picks it up:
#   - bilbycast-edge's engine::st2110::ptp reads /var/run/ptp4l (default
#     ptp4l management socket); locks all ST 2110 / MXL flows to it.
#   - For TS flows, master_clock.kind = "wallclock" (default). When
#     HW mode is active, CLOCK_REALTIME == NIC PHC, so wire_emit's
#     CLOCK_TAI pacing rides the same clock and PCR generation inherits
#     the NIC stability.
#   - For source-PCR-locked TS, master_clock.kind = "source_pcr_pll" or
#     "contribution".
#
# Usage:
#   sudo ./bilbycast-ptp-gm.sh start [iface] [--mode MODE]
#                                    [--domain N] [--priority1 N]
#                                    [--scan-timeout SECONDS]
#   sudo ./bilbycast-ptp-gm.sh stop
#        ./bilbycast-ptp-gm.sh status
#   sudo ./bilbycast-ptp-gm.sh restart [iface] [flags]
#        ./bilbycast-ptp-gm.sh logs
#        ./bilbycast-ptp-gm.sh scan [iface] [--domain N] [--scan-timeout SECONDS]
#
# Without [iface], auto-picks:
#   HW-PTP + carrier-up  >  HW-PTP + admin-up  >  software UP
#
# Env overrides (set on the command line OR /etc/default/bilbycast-ptp):
#   BILBYCAST_PTP_IFACE        pin to this interface (overrides auto-pick)
#   BILBYCAST_PTP_FORCE_SW     set =1 to force software timestamping
#   BILBYCAST_PTP_MODE         grandmaster | slave-only | auto (--mode wins)
#   BILBYCAST_PTP_DOMAIN       PTP domain number (--domain wins; config default 127)
#   BILBYCAST_PTP_PRIORITY1    BMCA priority1 (--priority1 wins; default 128 for GM, 255 for slave)
#   BILBYCAST_PTP_SCAN_TIMEOUT seconds the auto-mode listener waits for Announce (default 5)
#   BILBYCAST_PTP_RUN_DIR      PID dir (default /var/run/bilbycast-ptp)
#   BILBYCAST_PTP_LOG_DIR      log dir (default /var/log/bilbycast-ptp)
#   BILBYCAST_PTP_CONF         ptp4l config TEMPLATE path (default ./bilbycast-ptp-gm.conf)

set -u
set -o pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PTP4L_BIN="${PTP4L_BIN:-/usr/sbin/ptp4l}"
PHC2SYS_BIN="${PHC2SYS_BIN:-/usr/sbin/phc2sys}"
PMC_BIN="${PMC_BIN:-/usr/sbin/pmc}"
CONF_TEMPLATE="${BILBYCAST_PTP_CONF:-$HERE/bilbycast-ptp-gm.conf}"
RUN_DIR="${BILBYCAST_PTP_RUN_DIR:-/var/run/bilbycast-ptp}"
LOG_DIR="${BILBYCAST_PTP_LOG_DIR:-/var/log/bilbycast-ptp}"
# The conf we actually hand to ptp4l. Generated per-start from
# CONF_TEMPLATE + per-mode overrides. Lives in /etc/linuxptp so the
# AppArmor profile on /usr/sbin/ptp4l (which only allows reads from
# @{etc_ro}/linuxptp/**) can open it.
STAGED_CONF="${BILBYCAST_PTP_STAGED_CONF:-/etc/linuxptp/bilbycast-ptp-gm.conf}"
PTP4L_PID="$RUN_DIR/ptp4l.pid"
PHC2SYS_PID="$RUN_DIR/phc2sys.pid"
PTP4L_LOG="$LOG_DIR/ptp4l.log"
PHC2SYS_LOG="$LOG_DIR/phc2sys.log"
IFACE_FILE="$RUN_DIR/iface"
MODE_FILE="$RUN_DIR/mode"
ROLE_FILE="$RUN_DIR/role"
DOMAIN_FILE="$RUN_DIR/domain"
NTP_MARKER="$RUN_DIR/ntp-units-stopped"

# Pull defaults from /etc/default/bilbycast-ptp when the systemd unit
# wraps us. Operator-facing override of every env var.
if [ -r /etc/default/bilbycast-ptp ]; then
    # shellcheck disable=SC1091
    . /etc/default/bilbycast-ptp
fi

log()  { echo "[bilbycast-ptp-gm] $*" >&2; }
fail() { log "ERROR: $*"; exit 1; }

need_root() {
    [ "$(id -u)" -eq 0 ] || fail "this subcommand needs root (re-run with sudo) — runtime privileges are dropped under the systemd unit (CAP_NET_RAW + CAP_NET_ADMIN ambient caps)"
}

ensure_dirs() {
    install -d -m 0755 "$RUN_DIR" "$LOG_DIR"
    install -d -m 0755 "$(dirname "$STAGED_CONF")"
}

# ── AppArmor local overrides for linuxptp ─────────────────────────────
#
# Ubuntu's stock linuxptp profiles (enforce mode) are too tight for a
# working ptp4l + phc2sys pair:
#
#   - usr.sbin.phc2sys only grants the READ-ONLY management socket
#     (@{run}/ptp4lro). phc2sys -a autoconfiguration needs SUBSCRIBE on
#     the read-write @{run}/ptp4l — in enforce it loops
#     "Waiting for ptp4l..." forever (observed: 38 h / 724k log lines
#     on ms02 before this was root-caused, 2026-06-12).
#   - usr.sbin.ptp4l cannot write to management clients' filesystem
#     sockets (@{run}/pmc.*, @{run}/phc2sys.*), so replies are dropped
#     and `pmc` times out. (bilbycast-edge itself is unaffected — it
#     uses abstract sockets, which bypass path-based mediation.)
#
# The distro profiles `include if exists <local/...>` for exactly this
# purpose, so we install idempotent local overrides and reload the
# profiles — which stay in ENFORCE. This replaces the old aa-complain
# boot-time workaround, which reverted on every reboot / profile reload
# and failed silently.
#
# Upstream quirk: Ubuntu's usr.sbin.phc2sys includes
# <local/usr.sbin.ptp4l> (copy-paste typo, not its own local file), so
# the union of rules is written to BOTH local files to be correct
# whether or not the typo is ever fixed.
ensure_apparmor_overrides() {
    local apparmor_dir="/etc/apparmor.d"
    [ -f "$apparmor_dir/usr.sbin.ptp4l" ] || return 0
    local marker="# managed by bilbycast-ptp-gm.sh (v1)"
    local rules
    rules=$(cat <<'EOF'
  # Management clients bind datagram sockets under @{run}; ptp4l must
  # reply to them, and phc2sys must reach the read-write management
  # socket (SUBSCRIBE for -a autoconfiguration — the stock profile only
  # grants the read-only @{run}/ptp4lro).
  @{run}/ptp4l rw,
  @{run}/ptp4lro rw,
  @{run}/pmc.[0-9]* rw,
  @{run}/phc2sys.[0-9]* rw,
EOF
)
    local changed=0 f target
    for f in usr.sbin.ptp4l usr.sbin.phc2sys; do
        target="$apparmor_dir/local/$f"
        if ! grep -qF "$marker" "$target" 2>/dev/null; then
            if printf '%s\n%s\n' "$marker" "$rules" > "$target" 2>/dev/null; then
                log "AppArmor: installed local override $target"
                changed=1
            else
                log "WARNING: could not write $target (not root?). In enforce mode"
                log "         phc2sys cannot reach ptp4l's management socket and"
                log "         pmc gets no replies. Install the overrides manually."
                return 0
            fi
        fi
    done
    if [ "$changed" = "1" ]; then
        if apparmor_parser -r "$apparmor_dir/usr.sbin.ptp4l" "$apparmor_dir/usr.sbin.phc2sys" 2>/dev/null; then
            log "AppArmor: reloaded linuxptp profiles (enforce + local overrides)"
        else
            log "WARNING: apparmor_parser reload failed — apply manually:"
            log "    sudo apparmor_parser -r $apparmor_dir/usr.sbin.ptp4l $apparmor_dir/usr.sbin.phc2sys"
        fi
    fi
}

# ── NTP-daemon handoff for slave mode ─────────────────────────────────
#
# In slave-only mode the fabric grandmaster owns time: phc2sys (HW) or
# ptp4l itself (SW timestamping) disciplines CLOCK_REALTIME. A stock
# NTP daemon (chrony / chronyd / systemd-timesyncd) also disciplining
# the clock produces alternating max-rate (~1000 ppm) slews that wreck
# every wallclock-anchored media path. So on entering slave mode we
# stop active NTP units (recording which, so leaving slave mode
# restores exactly what we stopped). Opt out with
# BILBYCAST_PTP_KEEP_NTP=1 if site policy forbids touching NTP — but
# then YOU must guarantee single clock ownership some other way
# (timemaster(8), or chrony configured not to control the clock).
#
# Under the production systemd unit the script runs unprivileged
# (User=bilbycast) and `systemctl stop` will fail — we then fall back
# to a loud warning. PTP-slaved production sites should disable NTP
# clock control in provisioning.
stop_ntp_for_slave() {
    if [ "${BILBYCAST_PTP_KEEP_NTP:-0}" = "1" ]; then
        log "BILBYCAST_PTP_KEEP_NTP=1 — leaving NTP daemons running."
        log "WARNING: ensure only ONE servo adjusts CLOCK_REALTIME (timemaster(8)"
        log "         or chrony without clock control) or pacing WILL degrade."
        return 0
    fi
    local stopped=""
    for unit in chrony chronyd systemd-timesyncd; do
        if systemctl is-active --quiet "$unit" 2>/dev/null; then
            if systemctl stop "$unit" 2>/dev/null; then
                log "slave mode: stopped $unit (PTP fabric owns CLOCK_REALTIME now)"
                stopped="$stopped$unit\n"
            else
                log "WARNING: $unit is active but could not be stopped (not root?)."
                log "         Two servos on one clock WILL fight. Run:"
                log "             sudo systemctl disable --now $unit"
            fi
        fi
    done
    if [ -n "$stopped" ]; then
        printf '%b' "$stopped" > "$NTP_MARKER"
        log "slave mode: will auto-restore stopped NTP unit(s) on role change / stop"
    fi
}

restore_ntp_if_stopped() {
    [ -f "$NTP_MARKER" ] || return 0
    while IFS= read -r unit; do
        [ -n "$unit" ] || continue
        if systemctl start "$unit" 2>/dev/null; then
            log "restored NTP unit: $unit"
        else
            log "WARNING: failed to restart NTP unit $unit — restart it manually"
        fi
    done < "$NTP_MARKER"
    rm -f "$NTP_MARKER"
}

iface_has_hw_ptp() {
    # Detect a usable PTP hardware clock across ethtool output formats:
    #   ethtool < 6.x:  "PTP Hardware Clock: 1"                ("none" when absent)
    #   ethtool >= 6.x: "Hardware timestamp provider index: 1" ("-1"  when absent)
    # Older releases only printed the first form; ethtool 6.x renamed it.
    # Matching just the legacy string silently demotes modern E810/CX NICs to
    # software timestamping, whose TX-timestamp path faults ("timed out while
    # polling for tx timestamp") and flaps the grandmaster MASTER<->FAULTY.
    local out
    out="$(ethtool -T "$1" 2>/dev/null)" || return 1
    printf '%s\n' "$out" | grep -qE 'PTP Hardware Clock: [0-9]+' && return 0
    printf '%s\n' "$out" | grep -qE 'Hardware timestamp provider index: [0-9]+' && return 0
    return 1
}

iface_link_up() {
    [ -d "/sys/class/net/$1" ] || return 1
    [ "$(cat "/sys/class/net/$1/carrier" 2>/dev/null || echo 0)" = "1" ]
}

iface_admin_up() {
    [ -d "/sys/class/net/$1" ] || return 1
    local op
    op=$(cat "/sys/class/net/$1/operstate" 2>/dev/null || echo down)
    [ "$op" != "down" ]
}

pick_iface() {
    if [ -n "${BILBYCAST_PTP_IFACE:-}" ]; then
        echo "${BILBYCAST_PTP_IFACE}"
        return
    fi
    local best="" tier=0 t
    for iface in $(ls /sys/class/net/ 2>/dev/null); do
        case "$iface" in
            lo|docker*|br-*|veth*|tun*|tap*) continue ;;
        esac
        t=0
        iface_has_hw_ptp "$iface" && t=$((t + 10))
        iface_link_up    "$iface" && t=$((t + 5))
        iface_admin_up   "$iface" && t=$((t + 1))
        if [ "$t" -gt "$tier" ]; then
            best="$iface"; tier="$t"
        fi
    done
    [ -n "$best" ] || fail "no usable interface found"
    echo "$best"
}

is_running() {
    local pidfile="$1" pid
    [ -f "$pidfile" ] || return 1
    pid=$(cat "$pidfile" 2>/dev/null || true)
    [ -n "$pid" ] || return 1
    kill -0 "$pid" 2>/dev/null
}

stop_pid() {
    local pidfile="$1" name="$2" pid
    if is_running "$pidfile"; then
        pid=$(cat "$pidfile")
        log "stopping $name (pid $pid)"
        kill "$pid" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.2
        done
        kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$pidfile"
}

# Generate a per-mode ptp4l config from the template at $CONF_TEMPLATE.
# Writes to $STAGED_CONF (/etc/linuxptp/bilbycast-ptp-gm.conf so AppArmor
# is happy on Ubuntu). Replaces the [global] role section per the
# selected mode; keeps every other setting from the template (SMPTE
# profile timings, transport, logging).
stage_conf() {
    local role="$1" priority1="$2"
    local clock_class free_running master_only slave_only
    case "$role" in
        grandmaster)
            clock_class=248; free_running=1; master_only=1; slave_only=0 ;;
        slave-only)
            clock_class=255; free_running=0; master_only=0; slave_only=1 ;;
        *) fail "stage_conf: unknown role '$role'" ;;
    esac

    [ -r "$CONF_TEMPLATE" ] || fail "config template not found: $CONF_TEMPLATE"

    # Strip the existing [global] role lines (priority1, clockClass,
    # free_running, masterOnly, slaveOnly) — case-insensitive match on
    # the option name. Append our per-mode block at the end so it wins.
    grep -viE '^\s*(priority1|clockClass|free_running|masterOnly|slaveOnly)\b' \
        "$CONF_TEMPLATE" > "$STAGED_CONF"
    cat >> "$STAGED_CONF" <<EOF

# ── role injected by bilbycast-ptp-gm.sh (role=$role) ──
priority1                   $priority1
priority2                   128
clockClass                  $clock_class
free_running                $free_running
masterOnly                  $master_only
slaveOnly                   $slave_only
EOF
    chmod 0644 "$STAGED_CONF" 2>/dev/null || true
    log "staged config for role=$role priority1=$priority1 -> $STAGED_CONF"
}

# Listen for PTP Announce messages on the chosen iface + domain for
# $timeout seconds. Returns 0 if a peer master was heard (we should
# slave), non-zero if no peer was heard (we should become GM).
#
# Uses ptp4l itself in slave-only listen mode and parses its log for
# "received Announce" lines. Quick + accurate because ptp4l's BMCA is
# the authoritative implementation; running it briefly tells us
# definitively whether another GM is on the wire.
cmd_scan() {
    need_root
    ensure_dirs

    local iface="${1:-$(pick_iface)}"; shift || true
    parse_flags "$@"
    local domain="${SCAN_DOMAIN:-${BILBYCAST_PTP_DOMAIN:-127}}"
    local timeout="${SCAN_TIMEOUT:-${BILBYCAST_PTP_SCAN_TIMEOUT:-5}}"

    if ! iface_admin_up "$iface"; then
        log "scan: bringing $iface admin-up"
        ip link set "$iface" up || fail "scan: failed to bring $iface up"
    fi

    # Stage a transient slave-only config for the listener.
    local scan_conf="$RUN_DIR/scan.conf"
    stage_conf slave-only 255
    cp "$STAGED_CONF" "$scan_conf"

    local mode="sw"
    if [ "${BILBYCAST_PTP_FORCE_SW:-0}" != "1" ] && iface_has_hw_ptp "$iface"; then
        mode="hw"
    fi

    log "scan: listening on $iface (domain $domain, $timeout s, ${mode} timestamping)"
    local scan_log="$RUN_DIR/scan.log"
    : > "$scan_log"

    local args=(-f "$scan_conf" -i "$iface" -m --domainNumber "$domain")
    [ "$mode" = "sw" ] && args+=(-S)

    # Run ptp4l for the timeout window, capturing log. We don't need a
    # successful lock; we just want the Announce line.
    timeout "$timeout" "$PTP4L_BIN" "${args[@]}" >>"$scan_log" 2>&1 &
    local scan_pid=$!
    wait "$scan_pid" 2>/dev/null || true
    rm -f "$scan_conf"

    # ptp4l logs "received Announce ..." when it sees a peer with valid
    # BMCA. The "selected ..." line confirms the BMCA picked an external
    # master. Either signal = there's a GM out there → we slave.
    if grep -qE 'selected best master clock|received Announce' "$scan_log"; then
        local peer
        peer=$(grep -m1 -oE 'selected best master clock [0-9a-f.:]+' "$scan_log" | tail -1)
        log "scan: detected peer GM ($peer); recommend slave-only mode"
        echo "slave-only"
        return 0
    fi
    log "scan: no peer master heard in ${timeout}s; recommend grandmaster mode"
    echo "grandmaster"
    return 1
}

# Parse --mode / --domain / --priority1 / --scan-timeout flags.
# Assigns globals MODE_FLAG, DOMAIN_FLAG, PRIORITY1_FLAG, SCAN_TIMEOUT.
parse_flags() {
    MODE_FLAG=""
    DOMAIN_FLAG=""
    PRIORITY1_FLAG=""
    SCAN_TIMEOUT=""
    SCAN_DOMAIN=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --mode)          shift; MODE_FLAG="$1" ;;
            --mode=*)        MODE_FLAG="${1#*=}" ;;
            --domain)        shift; DOMAIN_FLAG="$1"; SCAN_DOMAIN="$1" ;;
            --domain=*)      DOMAIN_FLAG="${1#*=}"; SCAN_DOMAIN="${1#*=}" ;;
            --priority1)     shift; PRIORITY1_FLAG="$1" ;;
            --priority1=*)   PRIORITY1_FLAG="${1#*=}" ;;
            --scan-timeout)  shift; SCAN_TIMEOUT="$1" ;;
            --scan-timeout=*) SCAN_TIMEOUT="${1#*=}" ;;
            --) shift; break ;;
            -*) fail "unknown flag: $1" ;;
            *) break ;;
        esac
        shift
    done
}

cmd_start() {
    need_root
    ensure_dirs

    [ -x "$PTP4L_BIN" ]   || fail "$PTP4L_BIN not found (install linuxptp)"
    [ -x "$PHC2SYS_BIN" ] || fail "$PHC2SYS_BIN not found (install linuxptp)"

    local iface=""
    # Allow either "start eno4 --mode auto" or "start --mode auto" (iface auto-picked)
    if [ $# -gt 0 ] && [[ "$1" != --* ]]; then
        iface="$1"; shift
    fi
    parse_flags "$@"
    [ -z "$iface" ] && iface="$(pick_iface)"

    local role="${MODE_FLAG:-${BILBYCAST_PTP_MODE:-grandmaster}}"
    case "$role" in
        auto)
            log "auto mode: scanning for existing GM first"
            role=$(cmd_scan "$iface" --domain "${DOMAIN_FLAG:-${BILBYCAST_PTP_DOMAIN:-127}}" \
                --scan-timeout "${SCAN_TIMEOUT:-${BILBYCAST_PTP_SCAN_TIMEOUT:-5}}" \
                | tail -1)
            log "auto mode: resolved role=$role"
            ;;
        grandmaster|slave-only)
            ;;
        off)
            log "mode=off — not starting ptp4l. (Operator chose to disable PTP entirely.)"
            cmd_stop || true
            echo "off" > "$ROLE_FILE"
            return 0
            ;;
        *) fail "unknown --mode '$role' (expected: auto, grandmaster, slave-only, off)" ;;
    esac

    # Default priority1 per role; --priority1 overrides.
    local default_priority1=128
    [ "$role" = "slave-only" ] && default_priority1=255
    local priority1="${PRIORITY1_FLAG:-${BILBYCAST_PTP_PRIORITY1:-$default_priority1}}"
    local domain="${DOMAIN_FLAG:-${BILBYCAST_PTP_DOMAIN:-127}}"

    stage_conf "$role" "$priority1"

    log "selected interface: $iface"

    if ! iface_admin_up "$iface"; then
        log "bringing $iface admin-up"
        ip link set "$iface" up || fail "failed to bring $iface up"
    fi
    if ! iface_link_up "$iface"; then
        log "WARNING: $iface has no carrier (no cable / link partner)."
        log "         ptp4l will still run and bilbycast can lock via"
        log "         /var/run/ptp4l, but no remote slave can reach it."
    fi

    local mode="sw"
    if [ "${BILBYCAST_PTP_FORCE_SW:-0}" != "1" ] && iface_has_hw_ptp "$iface"; then
        mode="hw"
    fi
    log "timestamping mode: $mode  role: $role  priority1: $priority1  domain: $domain"

    # Make the distro AppArmor profiles workable BEFORE the daemons
    # start (idempotent — a no-op once the local overrides exist).
    ensure_apparmor_overrides

    # One clock owner per role (see the phc2sys block below): entering
    # slave mode hands CLOCK_REALTIME to the PTP fabric (stop NTP);
    # entering grandmaster mode hands it back to NTP.
    case "$role" in
        slave-only)  stop_ntp_for_slave ;;
        grandmaster) restore_ntp_if_stopped ;;
    esac

    if is_running "$PTP4L_PID"; then
        log "ptp4l already running (pid $(cat "$PTP4L_PID")); restarting"
        stop_pid "$PTP4L_PID" ptp4l
    fi
    if is_running "$PHC2SYS_PID"; then
        log "phc2sys already running (pid $(cat "$PHC2SYS_PID")); restarting"
        stop_pid "$PHC2SYS_PID" phc2sys
    fi

    local ptp4l_args=(-f "$STAGED_CONF" -i "$iface" -m --domainNumber "$domain")
    [ "$mode" = "sw" ] && ptp4l_args+=(-S)
    log "starting ptp4l: $PTP4L_BIN ${ptp4l_args[*]}"
    nohup "$PTP4L_BIN" "${ptp4l_args[@]}" >>"$PTP4L_LOG" 2>&1 &
    echo $! > "$PTP4L_PID"
    sleep 0.5
    is_running "$PTP4L_PID" || fail "ptp4l failed to start; see $PTP4L_LOG"

    # linuxptp creates /var/run/ptp4l mode 0660 root:root — that locks
    # out non-root edge processes pre-F4 (edge 0.89.0). Post-F4 bilbycast
    # uses abstract Unix sockets on Linux that bypass path-based
    # AppArmor mediation, so this chmod is belt-and-braces. Re-applied
    # on every start because ptp4l recreates the socket each launch.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        if [ -S /var/run/ptp4l ]; then
            chmod 0666 /var/run/ptp4l 2>/dev/null && break
        fi
        sleep 0.2
    done

    # ── System-clock ownership rule: exactly ONE servo may adjust
    # CLOCK_REALTIME. Two servos (e.g. chrony + phc2sys both writing
    # adjtimex) trade max-rate (~1000 ppm) slews back and forth, which
    # wrecks every wallclock-anchored media path on the box (PCR
    # accuracy, A/V regulation on the display output, wire pacing) —
    # not just ST 2110. Direction therefore depends on the role:
    #
    #   grandmaster — WE are the time source. chrony (NTP) stays the
    #     CLOCK_REALTIME owner; phc2sys runs in the opposite direction
    #     (NIC PHC <- system clock) so the fabric is served
    #     NTP-coherent time and nothing fights chrony. This is the
    #     standard linuxptp/RHEL "serve system time over PTP" recipe.
    #     Safe even when the port has no carrier — the system clock is
    #     never touched.
    #
    #   slave-only — the fabric grandmaster is the time source.
    #     phc2sys disciplines CLOCK_REALTIME from the PHC (-a -r); NTP
    #     daemons MUST NOT also control the clock. We warn loudly; the
    #     operator disables chrony or integrates both under one servo
    #     with timemaster(8).
    if [ "$mode" = "hw" ]; then
        case "$role" in
            grandmaster)
                # -n <domain> is REQUIRED: phc2sys's management client
                # defaults to domain 0 and ptp4l silently drops
                # mismatched-domain management messages — without it
                # phc2sys loops "Waiting for ptp4l..." forever on our
                # SMPTE domain 127 (same failure shape as the AppArmor
                # denial above; both were present here).
                log "starting phc2sys (NIC PHC <- system clock; chrony keeps owning CLOCK_REALTIME):"
                log "  $PHC2SYS_BIN -c $iface -s CLOCK_REALTIME -w -n $domain -S 1.0 -m"
                nohup "$PHC2SYS_BIN" -c "$iface" -s CLOCK_REALTIME -w -n "$domain" -S 1.0 -m >>"$PHC2SYS_LOG" 2>&1 &
                ;;
            slave-only)
                # NTP daemons were stopped above (stop_ntp_for_slave) —
                # phc2sys is the sole CLOCK_REALTIME owner from here.
                # -n <domain>: see the grandmaster arm.
                log "starting phc2sys (system clock <- fabric PHC): $PHC2SYS_BIN -a -r -n $domain -S 1.0 -m"
                nohup "$PHC2SYS_BIN" -a -r -n "$domain" -S 1.0 -m >>"$PHC2SYS_LOG" 2>&1 &
                ;;
            *) fail "phc2sys start: unknown role '$role'" ;;
        esac
        echo $! > "$PHC2SYS_PID"
        sleep 0.5
        is_running "$PHC2SYS_PID" || fail "phc2sys failed to start; see $PHC2SYS_LOG"
    fi

    echo "$iface"  > "$IFACE_FILE"
    echo "$mode"   > "$MODE_FILE"
    echo "$role"   > "$ROLE_FILE"
    echo "$domain" > "$DOMAIN_FILE"

    log "started OK (role=$role)"
    echo
    cmd_status
    echo
    log "next steps:"
    log "  - bilbycast-edge ST 2110 / MXL inputs+outputs lock automatically"
    log "    via /var/run/ptp4l; check FlowStats.ptp_state in the manager."
    log "  - For TS flows you want anchored to the NIC clock, leave"
    log "    master_clock.kind = \"wallclock\" (default) in HW mode."
    log "  - Watch live: $HERE/bilbycast-ptp-gm.sh logs"
}

cmd_stop() {
    need_root
    stop_pid "$PHC2SYS_PID" phc2sys
    stop_pid "$PTP4L_PID"   ptp4l
    restore_ntp_if_stopped
    rm -f "$IFACE_FILE" "$MODE_FILE" "$ROLE_FILE" "$DOMAIN_FILE" "$STAGED_CONF"
    log "stopped"
}

cmd_status() {
    local iface mode role
    iface=$(cat "$IFACE_FILE" 2>/dev/null || echo "?")
    mode=$(cat "$MODE_FILE" 2>/dev/null || echo "?")
    role=$(cat "$ROLE_FILE" 2>/dev/null || echo "?")
    printf "  %-15s : %s\n" "iface" "$iface"
    printf "  %-15s : %s\n" "timestamping" "$mode"
    printf "  %-15s : %s\n" "role" "$role"
    if is_running "$PTP4L_PID"; then
        printf "  %-15s : running (pid %s)\n" "ptp4l" "$(cat "$PTP4L_PID")"
    else
        printf "  %-15s : stopped\n" "ptp4l"
    fi
    if is_running "$PHC2SYS_PID"; then
        printf "  %-15s : running (pid %s)\n" "phc2sys" "$(cat "$PHC2SYS_PID")"
    elif [ "$mode" = "hw" ]; then
        printf "  %-15s : STOPPED (expected running in HW mode)\n" "phc2sys"
    else
        printf "  %-15s : n/a (software mode)\n" "phc2sys"
    fi

    if is_running "$PTP4L_PID" && [ -x "$PMC_BIN" ]; then
        # pmc defaults to domain 0; ptp4l silently drops
        # mismatched-domain management messages, so -d must match the
        # running daemon or every query reads as "(no response)".
        local pmc_domain
        pmc_domain=$(cat "$DOMAIN_FILE" 2>/dev/null || echo 127)
        echo
        echo "  --- pmc PARENT_DATA_SET (live, domain $pmc_domain) ---"
        "$PMC_BIN" -u -b 1 -d "$pmc_domain" "GET PARENT_DATA_SET" 2>/dev/null \
            | grep -E "grandmasterIdentity|grandmasterPriority1|grandmasterClockClass|grandmasterClockQuality" \
            | sed 's/^/    /' \
            || echo "    (no response — ptp4l may still be initialising)"
        echo "  --- pmc TIME_PROPERTIES_DATA_SET ---"
        "$PMC_BIN" -u -b 1 -d "$pmc_domain" "GET TIME_PROPERTIES_DATA_SET" 2>/dev/null \
            | grep -E "currentUtcOffset|timeSource|ptpTimescale" \
            | sed 's/^/    /' \
            || true
        echo "  --- live offset (1 sample) ---"
        timeout 2 tail -n 1 "$PTP4L_LOG" 2>/dev/null | sed 's/^/    /' || true
    fi
}

cmd_restart() {
    cmd_stop || true
    cmd_start "$@"
}

cmd_logs() {
    local files=()
    [ -r "$PTP4L_LOG" ]   && files+=("$PTP4L_LOG")
    [ -r "$PHC2SYS_LOG" ] && files+=("$PHC2SYS_LOG")
    [ ${#files[@]} -gt 0 ] || fail "no log files yet ($LOG_DIR)"
    tail -F "${files[@]}"
}

cmd_help() {
    sed -n '2,/^$/{s/^# \{0,1\}//;p;}' "$0"
}

case "${1:-help}" in
    start)            shift; cmd_start "$@" ;;
    stop)             cmd_stop ;;
    status)           cmd_status ;;
    restart)          shift; cmd_restart "$@" ;;
    logs|tail)        cmd_logs ;;
    scan)             shift; cmd_scan "$@" ;;
    help|-h|--help)   cmd_help ;;
    *)                cmd_help; exit 2 ;;
esac
