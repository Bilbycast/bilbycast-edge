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

## Security analysis

The PTP UX moves a previously root-only workflow (`sudo systemctl
restart ptp4l@…`) into a daemon driven by manager-UI input. The
threat model + mitigations:

### Trust boundaries

| Step | Who acts | Privilege held | What it can do |
|---|---|---|---|
| Operator → Manager | Authenticated user with `Operate` role on this node | Group-scoped session JWT + CSRF | Submit a `SetPtpModePayload` JSON to `PUT /api/v1/nodes/{id}/ptp/mode` |
| Manager → Edge | Manager process | Authenticated WS to the edge | Send the `set_ptp_mode` command — already gated by the existing manager-WS auth |
| Edge → Disk | `bilbycast-edge` user | File write to `/var/lib/bilbycast/ptp.conf` (mode 0644, owned by `bilbycast`) | Persist the operator's mode + iface + domain |
| Helper → Script | `bilbycast-ptp-helper` (separate process, `bilbycast` user) | `CAP_NET_RAW`, `CAP_NET_ADMIN`, `CAP_SYS_TIME` ambient caps | exec `/opt/bilbycast/bin/bilbycast-ptp-gm.sh` with up to ~6 argv entries |
| Script → ptp4l/phc2sys | The script | Inherits the helper's caps | `systemctl restart ptp4l@<iface>.service` + `phc2sys` equivalents |

### Defended attack vectors

**1. Config-file forging via `iface`.** A malicious operator with
`Operate` could try to defeat the KEY=VALUE on-disk format by sending
`iface = "eno4\nmode = grandmaster"` so that the file's last `mode`
line wins, overriding the chosen mode. Mitigation: `PtpSettings::validate`
rejects any `iface` containing characters outside `[A-Za-z0-9._-]` or
longer than 15 bytes (Linux `IFNAMSIZ`). Same rule is enforced both
on the manager (front-stops with HTTP 400) and on the edge (defence
in depth, returns `invalid_value`). Unit-tested by
`util::ptp_config::tests::validate_rejects_*` (5 cases).

**2. Shell injection via `iface` to the privileged script.** The
helper builds argv with `Command::args(OsString)`, which doesn't
shell-split — but the script then passes `$iface` to `systemctl
restart "ptp4l@$iface"`. Same iface validator from (1) blocks any
metachar (`;`, `$`, `` ` ``, `&`, etc.) that could be relevant if a
future code path ever does invoke a shell.

**3. Path traversal via `BILBYCAST_PTP_SCRIPT` env override.** The
helper supports the env override for tests. In production the systemd
unit (`bilbycast-ptp.service`) sets a clean environment without that
variable, so the compiled-in default `/opt/bilbycast/bin/bilbycast-ptp-gm.sh`
is what gets exec'd. The script + helper binary are both root-owned
mode 0755 (installed by `install-edge.sh`); an unprivileged attacker
on the box cannot replace either. The `bilbycast` user cannot write
to `/opt/bilbycast/bin/`.

**4. Torn writes / TOCTOU between edge and helper.** Edge uses
atomic `write(.tmp)` + `rename(2)`, so the helper's `read_to_string`
can never see a half-written file. The helper's 1 Hz mtime poll
re-reads on every observed change; subsequent edits within the same
1 s window collapse to one re-apply.

**5. Privilege escalation by replacing the script with a symlink.**
`/opt/bilbycast/bin/` is owned by root mode 0755; the `bilbycast`
user (which owns the helper process and the edge process) cannot
modify it. The helper does NOT follow symlinks before exec — but
even if it did, the directory isn't writable to the unprivileged
account, so a swap requires root already.

### Residual capabilities held by the helper

The helper holds three ambient capabilities even when idle:
`CAP_NET_RAW` (raw sockets), `CAP_NET_ADMIN` (NIC config), and
`CAP_SYS_TIME` (clock skew). These are exactly what `ptp4l` and
`phc2sys` need to run; they are not granted to the main edge
process. If the helper itself were compromised, an attacker would
inherit only those three caps — `CAP_SETUID`, `CAP_SYS_ADMIN`,
and root file write are NOT in the set. The systemd unit also sets
`ProtectSystem=strict` + `ReadWritePaths=` to the four paths
ptp4l/phc2sys actually need.

### Not yet defended (operator awareness)

- **Operator with `Operate` role can take the time source offline.**
  Flipping to `Off` stops `ptp4l` / `phc2sys`. This is by design — the
  same operator can already do worse (stop flows, force `master_clock
  = wallclock` on an ST 2110 flow, etc.) — but worth knowing for
  group permission design.
- **Per-tenant scoping of PTP changes.** Today the WS command treats
  the PTP file as node-wide. In a multi-tenant deployment where
  one node is shared, Group A's operator can change the PTP role,
  affecting Group B's flows. Tracked as a follow-up — the file
  doesn't know about tenants and the helper is a single process.

### Audit trail

Every successful `set_ptp_mode` is logged on the manager (audit:
`node.command`, action `set_ptp_mode`, args carry the requested
mode + iface) and on the edge's structured log
(`tracing::info!("set_ptp_mode applied: …")`). Failed validation is
logged at `warn` on both sides with the rejecting rule.

## See also

- [`clocking.md`](clocking.md) — flow-level master clock + PTP-disciplined wallclock
- [`st2110.md`](st2110.md) — ST 2110 essences require PTP to be locked
- [`installation.md`](installation.md) — the helper + script are installed by `install-edge.sh`
