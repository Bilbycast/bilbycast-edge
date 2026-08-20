# Cellular uplink telemetry

Read-only radio-state monitoring for bond legs (and any interface) that egress
over a mobile uplink — a USB/PCIe modem or a Teltonika RutOS router. The edge
attaches live signal / operator / access-tech / registration state to the
network interface, the manager renders it on the **Network Interfaces** card
(node detail) and as a compact signal strip on the bond legs that report an
egress netdev (a narrow subset — see [Where the bond-leg strip
appears](#where-the-bond-leg-strip-appears)), and the edge emits a few debounced
events. **No writes to the devices, no sidecar, nothing installed on the
Teltonika hardware.**

> Scope: read-only telemetry. APN / band-lock / reboot / SIM-switch and
> data-cap *enforcement* are explicitly out of scope.

## Two sources, one shape

| | USB / PCIe modem | RutOS router (RUT / RUTX / OTD) |
|---|---|---|
| Read via | **ModemManager** D-Bus (`org.freedesktop.ModemManager1`) | RutOS HTTP API (ubus JSON-RPC, or REST on 7.x) |
| Config | **none** — auto-detected | opt-in `cellular_uplinks` entry + read-only credential |
| Platform | Linux only | any (pure `reqwest`) |
| Credential | none (local D-Bus, unprivileged) | read-only RutOS user, stored in `secrets.json` |
| Generalises to | any modem ModemManager owns | any RutOS device |

Both produce the same `CellularMetrics` block on
`HealthPayload.network_interfaces[].cellular`:

```jsonc
"cellular": {
  "source": "modem_manager",          // | "rutos"
  "state": "registered_home",         // registered_roaming | searching | denied | sim_missing | sim_pin_required | disabled | unknown
  "access_tech": "5gnr_nsa",          // 5gnr_sa | 5gnr_nsa | lte | umts | hspa | gsm | …
  "operator": "Telstra",
  "plmn": "50501",
  "band": "n78",
  "cell_id": "0x1A2B3C",              // RutOS only — modem_manager hard-codes None
  "signal": { "rsrp_dbm": -95, "rsrq_db": -11, "sinr_db": 12, "rssi_dbm": -67, "bars": 3 },
  "roaming": false,
  "sim_slot": 1,
  "temperature_c": 41.0,              // RutOS only (best-effort)
  "data_used_bytes": 0,               // RutOS only (best-effort)
  "data_limit_bytes": null,           // RutOS only (best-effort)
  "last_error": null,                 // set ⇒ failure placeholder, not a sample (below)
  "keeper_active": true,              // modem_manager only — host keep-alive heartbeat
  "sampled_at_unix_ms": 1750000000000
}
```

Four of those fields are source-specific, not merely best-effort:
`snapshot_to_metrics` in `modem_manager.rs` hard-codes `cell_id`,
`temperature_c`, `data_used_bytes` and `data_limit_bytes` to `None`, so they are
only ever populated from a RutOS router. `roaming` and `sim_slot` come from both
sources. `last_error` is set **only** on a failure placeholder — see
[Architecture](#architecture).

All fields are additive `Option` (`skip_serializing_if`), enums have
`#[serde(other)]` catch-alls — **no `WS_PROTOCOL_VERSION` bump**. Older managers
ignore the block; older edges omit it.

### The `"cellular"` capability gates nothing today

The edge advertises `"cellular"` on `HealthPayload.capabilities` whenever the
poller has at least one source — configured uplink or auto-detected modem
(`manager/client.rs`, gated on `CellularCache::has_sources()`). **No manager UI
surface reads that bit.** Both strips render on *data presence*: `flows.js`
draws the Network Interfaces strip on `if (n.cellular)` and the bond-leg strip
on the leg's `interface` resolving into an interface that carries a `cellular`
block. The router authoring form is ungated on purpose (`cellular_uplinks.js`
says so at the top — a router is configured *before* the edge has any data).

The one cellular surface that is genuinely capability-gated is the **Wake**
button, on `"cellular-control"` (`caps.indexOf('cellular-control')` for the
Network Interfaces copy, `bondCanControl` for the per-leg copy). So `"cellular"`
is emitted and currently unconsumed — treat it as available for a future gate,
not as the reason a strip appears. (The equivalent claim *is* still written down
on the Starlink side, in the comment beside its capability push in
`manager/client.rs` — see [starlink.md](starlink.md#the-starlink-capability-gates-nothing-today).)

## Architecture

A single background **poller** task (`util::cellular`, under the app cancellation
tree) samples every source on a slow cadence (10 s), each sample time-bounded
(4 s). It writes the latest snapshot into a lock-free `DashMap` cache keyed by
kernel netdev. The ~15 s health tick joins that cache onto each interface — no
HTTP/D-Bus on the tick, **never on the data path**.

**Staleness, precisely.** Each cycle ends with
`cache.evict_older_than(now_ms − 60 s)`, so an interface the poller has *stopped
sampling* drops off the card rather than freezing on a stale value — an
unplugged modem, or one ModemManager no longer reports, takes that path. **A
configured RutOS router that goes dark does not.** Every failing cycle inserts a
fresh `CellularMetrics::unreachable(...)` placeholder stamped with the current
time, so the entry is never older than one poll interval and the eviction can
never reach it: the row renders `⚠ UNREACHABLE` with the failure cause
indefinitely. That is the intent — a misconfigured router stays diagnosable at a
glance instead of vanishing to "no data".

The poller reads `config.cellular_uplinks` from the live `AppConfig` each cycle,
so a config change is picked up within one interval with **no flow restart** and
no special `UpdateConfig` hook.

```
util::cellular::spawn_cellular_poller
  ├── modem_manager::sample_all  ── zbus ─▶ ModemManager (system D-Bus)
  └── rutos::RutosSource::sample ── reqwest ─▶ https://<router>/ubus | /api/...
        └─▶ DashMap<iface, CellularMetrics>  ◀── NetworkSampler::sample() (health tick)
```

### ModemManager (auto-detected)

One `GetManagedObjects` call enumerates modems and maps each to its kernel
netdev via the modem's `net`-type port; typed property reads fill the rest:
`State` / `StateFailedReason` / `UnlockRequired` (→ registration / SIM state),
`AccessTechnologies` (→ tech; LTE+5G bits = NSA, 5G alone = SA),
`SignalQuality`, `CurrentBands` (→ band), `Modem3gpp.{RegistrationState,
OperatorName, OperatorCode}`, and the `Modem.Signal` per-tech dicts
(`Nr5g`/`Lte`/`Umts`/`Gsm` → rsrp/rsrq/snr/rssi). Reads are unprivileged.

`Modem.Signal` is only populated when signal polling is enabled. The edge reads
whatever is published and best-effort calls `Setup(rate)` (ignoring
PermissionDenied). To guarantee the figures are published, run once on the host:

```bash
mmcli -m <N> --signal-setup=5
```

### RutOS (opt-in)

Per-uplink `reqwest` client honouring its TLS policy:
- `verify_tls: false` (default) → accept the self-signed cert.
- `cert_fingerprint` set → pin on the SHA-256 of the presented cert (stronger
  than CA validation for a self-signed router; CA chain is not required).
- `verify_tls: true`, no pin → normal CA validation.

**ubus** (default, broad compatibility): `POST /ubus` `session login` → token,
then `gsm.modem0 get_signal_query` + `info` merged. **REST** (RutOS 7.x):
`POST /api/login` → bearer → `GET /api/modems/status`.

**Reachability probe.** `test_cellular_uplink` (WS command, operator role,
`manager/client.rs`) builds a transient `CellularUplinkConfig` from the posted
fields, runs one real login + modem-status fetch through the same
`rutos::build_sources` → `sample()` path the poller uses, and returns
`{ ok: true, operator, access_tech, band, signal, state }` or
`{ ok: false, error: "<cause>" }`. It backs the router form's **Test
reachability** button (`config/cellular_uplinks.js`, `.bc-cell-test` →
`#bcCellTestResult`). On an edit with the password field left blank it falls
back to the stored secret for that interface, so an existing router can be
re-tested without re-typing the credential. It probes **RutOS only** — a
ModemManager modem has no configuration to validate.

> The exact RutOS field names vary by model + firmware. The mapper
> (`rutos::json_to_metrics`) is deliberately tolerant — it tries multiple key
> spellings and coerces string-or-number values — and is the item to confirm
> against the live device. Confirm via the device's API
> (wiki "Monitoring via JSON-RPC <model>" / developers.teltonika-networks.com)
> and adjust the alias lists if a field doesn't bind.

## Configuration

Modems need nothing. For a RutOS router, add one entry per interface to
`config.json` (operational, safe for the manager):

```jsonc
"cellular_uplinks": [
  {
    "interface": "eno4",        // kernel netdev this annotates
    "kind": "rutos",            // only "rutos" is read; modems auto-detect
    "scheme": "https",          // http | https
    "address": "192.168.1.1",   // bare host/IP, no scheme/path
    "api": "ubus",              // ubus | rest
    "username": "monitor",      // read-only RutOS user
    "verify_tls": false,        // accept self-signed (RutOS default)
    "cert_fingerprint": null    // optional SHA-256 pin
    // NO password here
  }
]
```

The **password is an infrastructure secret**: it lives only in `secrets.json`
(keyed by interface, AES-256-GCM at rest), is stripped from `GetConfig`, and is
re-merged on `UpdateConfig` — the manager never round-trips it.

```jsonc
// secrets.json (local-only, 0600)
"cellular_uplinks": { "eno4": { "password": "•••" } }
```

In the manager UI, configure routers via **Node config → Uplink Monitoring →
Cellular (mobile uplinks) → Add Router**. There is no "Cellular Monitoring" tab:
`node_config.html` carries one tab `data-tab="uplinks"` labelled **Uplink
Monitoring**, and Cellular is a sub-section inside it, alongside Starlink and
the shared-uplink capacity broker. The password field is write-only (blank keeps
the stored value); **Test reachability** runs the probe described above before
you save. Modems show up automatically with no config.

## Device-side prerequisite (RutOS)

On the router: create a **read-only** user; lock RutOS Access Control to
HTTPS-only + LAN; remote access off; optionally disable RMS (cloud). Do **not**
use Modbus (unauthenticated) for this; SNMP v3 only if ever used. The only
credential in play is read-only, so the blast radius is small.

## Waking a dormant USB modem (request/execute split)

> The telemetry module is **read-only** (above). This section adds a *control*
> surface that stays faithful to that: the edge **requests** a wake; a separate
> opt-in root daemon **executes** it. The edge never drives the modem.

A USB modem with no traffic drops to RRC-idle and the carrier may tear the
bearer down; signal sampling can also lapse on a re-enumeration. An operator who
only has the manager UI (no shell, no `sudo`) then can't start a flow over that
leg — there's no traffic to wake it and the edge has no rights to. (The edge
*can't* call `mmcli`/`Modem.Enable`/`Simple.Connect` itself: ModemManager's
`Device.Control` polkit action is `allow_inactive=no`, so a headless service is
denied — which is why the read-only `Modem.Signal.Setup` warm-up call is a
best-effort, error-swallowed write. And even a successful `Simple.Connect`
doesn't apply the lease IP/route to a raw-IP WWAN netdev, so the bond leg's
source-bind would still have nothing to bind.)

The fix is a **request/execute split**, mirroring the PTP-helper file-IPC
(`/var/lib/bilbycast/ptp.conf`):

1. **Host keeper (executes).** `packaging/bilbycast-cellular-modem.service` runs
   `setup-cellular-modem.sh --watch` as root (opt-in;
   `install-cellular-modem.sh --enable`). Every `WATCH_INTERVAL` (30 s) it runs
   `mmcli --simple-connect` (enables + registers + connects — pulling the modem
   out of idle with **zero media traffic**), applies the lease + policy route in
   table 70, re-arms `--signal-setup=5`, and writes a status **heartbeat**. It is
   ordered `Before=bilbycast-edge.service` so the leg's IP exists before the edge
   pins it. With the keeper enabled the bearer is held up 24/7 — the steady-state
   answer to dormancy.

2. **Edge (requests).** The `wake_uplink { interface, apn? }` WS command (operator
   role) writes `/var/lib/bilbycast/cellular-wake.req` (a few `key=value` lines
   with a nonce, written **in place** into an installer-seeded, edge-owned file —
   the edge can't write the root-owned dir) and polls
   `cellular-wake.status` (~8 s) for the keeper to echo its nonce with a terminal
   `state` (`connected` / `failed`), returning `{ state, detail?, addr? }`. The
   edge gains **no** modem privilege — it only touches a `bilbycast`-owned file.
   An optional `apn` rides the request so the operator can fix a wrong APN from
   the UI (the keeper's env-file APN is the fallback).

3. **Liveness + visibility (read-only).** The keeper's status-file mtime is a
   heartbeat: fresh (≤ 120 s) ⇒ the daemon is running. The edge surfaces this as
   `CellularMetrics.keeper_active` (modem sources only) and advertises the
   `cellular-control` capability only while it's fresh — so the manager shows a
   **Wake** button (Network Interfaces card + bond-leg row) that can never be
   dead. `cellular-control` is the **only** cellular capability any manager
   surface reads. The bond-leg copy of the button rides inside the compact strip,
   so it reaches only the legs that get a strip at all
   ([below](#where-the-bond-leg-strip-appears)), and only when that leg's modem
   is dormant (`disabled` / `searching` / `unknown`); the Network Interfaces copy
   has no such dormancy condition. A modem stuck `disabled`/`searching` with no
   keeper raises the debounced
   `cellular_keeper_missing` Warning so the operator knows to provision the
   keeper (one `sudo install-cellular-modem.sh --enable` at rack time).

What stays read-only: `src/util/cellular/modem_manager.rs` gains **no**
`Enable`/`Connect`/`CreateBearer`/`set_property` — the only edge-side motion is a
control *request* (a file write) plus the `cellular-control` capability bit. APN
authoring and the connect itself live in the root keeper (host provisioning),
exactly as `APN / band-lock / reboot / SIM-switch` were always out of the edge's
scope. The "No writes to the devices" line above is RutOS-specific (a RutOS
router keeps its own bearer — Wake is hidden for `rutos` sources) and unchanged.

## Events

Node-level, category `cellular`, debounced (catalogued in
[`events-and-alarms.md`](events-and-alarms.md#cellular-uplink-events-cellular)):

| `error_code` | Severity | When |
|---|---|---|
| `cellular_registration_changed` | info / warning | reg state transitions (warning on denied / no-SIM / SIM-locked) |
| `cellular_signal_degraded` | warning | bars drop to ≤ 1 (enter, hysteresis) |
| `cellular_signal_recovered` | info | bars climb to ≥ 3 (leave) |
| `cellular_uplink_unreachable` | warning | a RutOS poll fails 3 cycles running |
| `cellular_uplink_recovered` | info | a RutOS poll succeeds after being unreachable |
| `cellular_keeper_missing` | warning | a modem is `disabled`/`searching` for 3 cycles with no host keep-alive daemon running (can't be woken from the UI — provision `bilbycast-cellular-modem.service`) |

## Where the bond-leg strip appears

The Network Interfaces card draws a strip for **every** interface carrying a
`cellular` block. The bond-leg strip is far narrower, because it is a *join*:
`flows.js` builds `cellByIface` from `network_interfaces[].cellular`, then looks
each leg up by `BondPathLegStats.interface`. A leg that reports no `interface`
gets no strip — and only two leg shapes ever set it.

| Leg | `BondPathLegStats.interface` | Strip? |
|---|---|---|
| **Sender**, UDP, interface-mode (`gateway` unset) | configured netdev | yes |
| **Sender**, Relay, interface-mode (`gateway` unset) | configured netdev | yes |
| Sender, UDP or Relay, gateway-mode (`gateway` set) | `None` — steering is a policy route, not a NIC pin | no |
| Sender, QUIC | `None` — `bond_path_interface` has no `Quic` arm, even though the config variant *does* carry an `interface` field | no |
| Sender, RIST | `None` | no |
| **Receiver** (`input_bonded`), any transport | `None` — a listening leg has no meaningful egress NIC | no |

`engine::input_bonded::bond_path_interface` is the predicate and
`engine::output_bonded.rs` is its **only** caller; `stats::collector`
additionally nulls the field whenever `p.gateway_mode`. The doc comment on
`BondPathLegStats.interface` reads "interface-mode UDP legs only", which
understates its own code — interface-mode **relay** legs qualify too.

## Bars and colour

`bars` is the only thing the manager colours on — there is no per-metric
threshold table on the UI side. `util::cellular::derive_bars` bins each figure
onto its own **six**-step ladder and folds the results with `min` (worst-of);
`flows.js::cellBarsColor` then maps the count to a colour.

| bars | 5 | 4 | 3 | 2 | 1 | 0 |
|---|---|---|---|---|---|---|
| **RSRP** (dBm) | ≥ −80 | ≥ −90 | ≥ −100 | ≥ −105 | ≥ −115 | < −115 |
| **SINR** (dB) | ≥ 20 | ≥ 13 | ≥ 6 | ≥ 0 | ≥ −5 | < −5 |
| **RSSI** (dBm) | ≥ −65 | ≥ −75 | ≥ −85 | ≥ −95 | ≥ −105 | < −105 |
| **colour** | green | green | amber | amber | red | red |

Green is `#3fb950` (≥ 4 bars), amber `#d29922` (2–3), red `#f85149` (≤ 1), grey
`#8b949e` when `bars` is `null`. Six signal steps therefore collapse onto three
colours plus "no reading" — an operator cannot tell 1 bar from 0 bars by colour,
only by the glyph and the hover numbers.

RSSI is folded in **when and only when RSRP is absent**. The intent is "RSSI is
all there is on 2G/3G", but the rule is mechanical rather than
technology-aware: a 4G/5G source publishing RSSI and no RSRP is scored on the
RSSI ladder. Operators read colour first, numbers on hover.

## Code map

- Edge: `src/util/cellular/{mod.rs, modem_manager.rs, rutos.rs}` (types, cache,
  poller, sources; `mod.rs` also holds the wake file-IPC — `request_wake`,
  `keeper_heartbeat_fresh`/`wake_control_available`, `cellular_keeper_missing`;
  `modem_manager.rs` `pick_plausible_tech` is the NSA NR→LTE sentinel
  fall-through), `src/util/network_interfaces.rs` (`cellular` field + join),
  `src/config/models.rs` (`CellularUplinkConfig`), `src/config/secrets.rs`
  (`CellularUplinkSecrets` split), `src/config/validation.rs`
  (`validate_cellular_uplinks`, `validate_interface_name`), `src/manager/client.rs`
  (`"cellular"` + `"cellular-control"` capabilities, `test_cellular_uplink` +
  `wake_uplink` commands), `src/stats/{models,collector}.rs`
  (`BondPathLegStats.interface`; `collector` nulls it on gateway-mode legs),
  `src/engine/input_bonded.rs` (`bond_path_interface` — the predicate; the
  receive side sets no interface of its own) and `src/engine/output_bonded.rs`
  (its only caller).
- Host keeper (execute side): `packaging/{bilbycast-cellular-modem.service,
  setup-cellular-modem.sh, install-cellular-modem.sh, bilbycast-cellular-modem.default}`
  (root daemon: `--simple-connect` keep-alive + `--signal-setup` + wake-request
  servicing + status heartbeat). File-IPC at `/var/lib/bilbycast/cellular-wake.{req,status}`.
- Manager: `crates/manager-core/src/models/ws_protocol.rs` (mirror),
  `crates/device-edge/src/lib.rs` (`test_cellular_uplink` + `wake_uplink` in
  `EDGE_COMMANDS`), `crates/manager-server/src/ui/static/js/detail/flows.js`
  (`renderCellularStrip` + `cellBarsColor` + `cellByIface` bond-leg join +
  Network Interfaces card),
  `crates/manager-server/src/ui/static/js/config/cellular_uplinks.js` (router
  form + Test reachability), `crates/manager-server/src/ui/node_config.html`
  (**Uplink Monitoring** tab, Cellular sub-section).

## Not applicable

This change does not touch the media / transport path, so the broadcast A/V
quality gates (`testbed/BROADCAST_QUALITY_GATES.md`) do not apply — no PCR /
A-V numbers are relevant.
