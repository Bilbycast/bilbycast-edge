# PTP operator mode

A bilbycast edge can be a PTP grandmaster, a PTP slave, or run with PTP
turned off. Picking the right one is two clicks in the manager UI — no
SSH, no `sudo`, no `systemctl`.

This page covers what the operator sees, what the edge does behind the
scenes, and how the pieces fit together. For the underlying clock model
(why PTP matters, what `master_clock.kind` does to a flow), see
[`clocking.md`](clocking.md).

## The four modes

| Mode | Use when | Behind the scenes |
|---|---|---|
| **Auto** | Mixed sites — you don't know in advance whether a grandmaster is on the LAN | Listen for a PTP Announce for `scan_timeout` seconds. If heard, become a slave; otherwise become the grandmaster. The plug-and-play default. |
| **Grandmaster** | You control the LAN and want this node to provide time | `priority1=128`, `masterOnly=1`, `clockClass=248` |
| **Slave only** | The customer requires that we never be the time source | `priority1=255`, `slaveOnly=1`, `clockClass=255`. Refuses to ever become master under BMCA. |
| **Off** | You're not using ST 2110 / MXL and don't want PTP traffic on the wire | No `ptp4l` / `phc2sys` running. TS-class flows run on system wallclock; ST 2110 / MXL flows refuse to start. |

**Default on a fresh install is `off`.** PTP packets at a customer site
without their knowledge would be noisy — the operator opts in
explicitly.

## How to change it

### Manager UI

`/nodes/{id}/time` — accessed from the **Time (PTP)** button on the node
detail page. Pick a mode, optionally set the interface / domain /
priority1, click **Apply**. The change takes effect within ~1 second.

### Direct REST against the edge

```bash
# Read current settings
curl https://edge:8443/api/v1/ptp \
     -H "Authorization: Bearer $TOKEN"

# Switch to slave-only on eno4
curl -X PUT https://edge:8443/api/v1/ptp \
     -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"mode":"slave-only","iface":"eno4","domain":127}'
```

### Hand-editing the config file

`/var/lib/bilbycast/ptp.conf` is a plain KEY=VALUE file. Edit it with
your favourite editor and the helper picks the change up within 1
second:

```ini
mode         = auto
iface        = eno4
domain       = 127
priority1    =
scan_timeout = 5
```

Unknown keys are tolerated — safe for forward-compat with future
fields. Comments (`#`) and blank lines are skipped.

## How it works under the hood

```
┌────────────────────────────────────────────────────────────────┐
│  Manager UI                                                    │
│  ─ /nodes/{id}/time picks mode → PUT /api/v1/nodes/{id}/ptp/mode│
└─────────────────────────┬──────────────────────────────────────┘
                          │ HTTPS
┌─────────────────────────▼──────────────────────────────────────┐
│  Manager                                                       │
│  ─ proxy_set_ptp_mode forwards over WS as set_ptp_mode         │
└─────────────────────────┬──────────────────────────────────────┘
                          │ WSS
┌─────────────────────────▼──────────────────────────────────────┐
│  Edge (bilbycast-edge process)                                 │
│  ─ manager/client.rs "set_ptp_mode" arm                        │
│  ─ atomic write+rename → /var/lib/bilbycast/ptp.conf           │
│  ─ returns echo of the applied (normalised) settings           │
└─────────────────────────┬──────────────────────────────────────┘
                          │ filesystem mtime
┌─────────────────────────▼──────────────────────────────────────┐
│  bilbycast-ptp-helper (separate process, separate systemd unit)│
│  ─ 1 Hz mtime poll                                             │
│  ─ on change: read config, exec bilbycast-ptp-gm.sh            │
│  ─ AmbientCapabilities=CAP_NET_RAW CAP_NET_ADMIN CAP_SYS_TIME  │
└─────────────────────────┬──────────────────────────────────────┘
                          │ exec
┌─────────────────────────▼──────────────────────────────────────┐
│  bilbycast-ptp-gm.sh                                           │
│  ─ stage_conf renders the right ptp4l options                  │
│  ─ systemctl stop/start ptp4l@<iface>.service + phc2sys        │
└────────────────────────────────────────────────────────────────┘
```

### Why a separate helper?

The PTP daemons (`ptp4l`, `phc2sys`) need elevated privileges:

- `CAP_NET_RAW` — `ptp4l` opens raw sockets for IEEE 1588 frames.
- `CAP_SYS_TIME` — `phc2sys` adjusts the system clock from the PHC.
- `CAP_NET_ADMIN` — adjusting PHC settings, hardware timestamping flags.

Keeping the helper separate means the main `bilbycast-edge` binary
itself runs with **no extra capabilities**. Only this tiny ~200-line
helper holds the ambient caps, and it does nothing on the data path —
it watches one file and execs one script.

### Why a file the helper polls, not RPC?

- **No new IPC surface** — manager writes a file, edge reads it back, helper polls it. Everything is over channels the platform already trusts.
- **Atomic** — manager writes `.tmp` then `rename(2)`s. The helper's `read_to_string` can never see a torn write.
- **Hand-editable** — operator on the box can drop into `/var/lib/bilbycast/ptp.conf` with vi and the same 1 Hz poll applies the change. No `systemctl reload`, no manager round-trip required.
- **No dbus / polkit dependency** — works in minimal container images and stripped-down distros where dbus isn't installed.

## Files + binaries reference

| Path | Role |
|---|---|
| `/var/lib/bilbycast/ptp.conf` | Operator-managed mode + iface + domain config. Watched at 1 Hz by the helper. |
| `/opt/bilbycast/bin/bilbycast-ptp-helper` | Tiny Rust binary: `scan`, `apply`, `run` subcommands. Runs under systemd unit `bilbycast-ptp.service`. |
| `/opt/bilbycast/bin/bilbycast-ptp-gm.sh` | The script the helper execs. Handles `start` / `stop` / `restart` / `scan` and the per-mode `ptp4l` config rendering. |
| `/etc/systemd/system/bilbycast-ptp.service` | systemd unit for the helper. `User=bilbycast`, ambient caps, `Before=bilbycast-edge.service`. |
| `/etc/default/bilbycast-ptp` | Optional shell-style overrides for the script's environment defaults. |

## Verifying it's working

```bash
# Helper status
systemctl status bilbycast-ptp.service

# Effective ptp4l + phc2sys
systemctl list-units 'ptp4l*' 'phc2sys*'

# Live PTP state surface from the edge
curl https://edge:8443/api/v1/ptp -H "Authorization: Bearer $TOKEN"

# Equivalent in the manager UI: /nodes/{id}/time → "Live status"
```

A successful slave shows `lock_state: locked`, a non-empty
`grandmaster_id`, and `offset_ns` settling in the low-nanoseconds range
within a few seconds. Grandmaster mode reports `lock_state: locked`
with `offset_ns: 0` (the node is its own reference).

## Operator UX rule of thumb

- Don't know if there's a GM? → **Auto**. Right answer 95% of the time.
- You run the LAN and want a known time source? → **Grandmaster**.
- The customer says "we provide PTP, you slave"? → **Slave only**.
- Not using ST 2110 / MXL at this site? → **Off** (the install default).

## See also

- [`clocking.md`](clocking.md) — flow-level master clock + PTP-disciplined wallclock
- [`st2110.md`](st2110.md) — ST 2110 essences require PTP to be locked
- [`installation.md`](installation.md) — the helper + script are installed by `install-edge.sh`
