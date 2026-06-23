# Starlink dish telemetry

Read-only link-state monitoring for a bond leg (or any interface) that egresses
over a Starlink terminal. The edge polls the dish's **local gRPC API**, attaches
the live link state to the network interface, the manager renders it on the
**Network Interfaces** card (node detail) and as a signal strip on **bond legs**,
and the edge emits a few debounced events. **No writes to the dish, no sidecar,
nothing installed on the Starlink hardware.**

This is the satellite-link sibling of [`cellular.md`](cellular.md): same poller +
cache + health-tick join shape, same UI surfaces, same capability-gating — but
the source is the Starlink dish gRPC rather than a modem / RutOS router, and the
metrics are link-quality figures (obstruction / throughput / latency / drop-rate)
rather than radio signal.

> Scope: read-only telemetry. Reboot / stow / config writes are explicitly out of
> scope — the request frame is hard-coded to the `get_status` read.

## Source

| | Starlink dish |
|---|---|
| Read via | dish **gRPC** `SpaceX.API.Device.Device/Handle` `get_status`, cleartext HTTP/2 (h2c) |
| Default endpoint | `192.168.100.1:9200` |
| Config | opt-in `starlink_uplinks` entry (interface + address) |
| Credential | **none** — the dish gRPC is unauthenticated on the LAN |
| Platform | any (pure `reqwest`, `http2` feature) |

The dish gRPC is decoded with a **hand-rolled protobuf reader** (`util::starlink::grpc`)
— no `tonic` / `prost` dependency. The request is a fixed 8-byte gRPC frame
(`Request{ get_status: {} }`, field 1004); the response decoder is a tolerant
field-walker that extracts the fields below from `dish_get_status` (response
field 2004) and skips anything it does not recognise, so newer dish firmware that
adds fields degrades cleanly. `reqwest` is built with the `http2` feature for the
cleartext-HTTP/2 prior-knowledge transport.

The poller produces a `StarlinkMetrics` block on
`HealthPayload.network_interfaces[].starlink`:

```jsonc
"starlink": {
  "state": "connected",            // searching | booting | offline | unknown
  "obstruction_fraction": 0.0012,  // fraction of the window obstructed (0..1)
  "currently_obstructed": false,
  "downlink_bps": 138000000.0,
  "uplink_bps": 14500000.0,
  "pop_ping_latency_ms": 31.5,
  "pop_ping_drop_rate": 0.004,     // 0..1
  "seconds_to_first_nonempty_slot": 0.0,
  "snr": null,                     // legacy — absent on recent firmware
  "uptime_s": 987654,
  "alerts": [],                    // e.g. ["thermal_throttle"]
  "device_id": "ut01000000-…",
  "hardware_version": "rev3_proto2",
  "software_version": "2025.10.03.mr61821",
  "country_code": "AU",
  "bars": 5,                       // derived 0..=5 quality
  "sampled_at_unix_ms": 1750000000000
}
```

All fields are additive (`Option` / `skip_serializing_if`), the `state` enum has a
`#[serde(other)]` catch-all — **no `WS_PROTOCOL_VERSION` bump**. Older managers
ignore the block; older edges omit it. The edge advertises the `"starlink"`
capability on `HealthPayload.capabilities` whenever the poller has at least one
configured dish; the manager UI gates the signal strips on that bit.

## Architecture

A single background **poller** task (`util::starlink::spawn_starlink_poller`,
under the app cancellation tree) samples every dish on a slow cadence (10 s), each
sample time-bounded (6 s). It writes the latest snapshot into a lock-free
`DashMap` cache keyed by kernel netdev. The ~15 s health tick joins that cache
onto each interface — no gRPC on the tick, **never on the data path**. Stale
snapshots (>60 s) age out so the UI shows "no data / acquiring" rather than a
frozen value.

The poller reads `config.starlink_uplinks` from the live `AppConfig` each cycle,
so a config change is picked up within one interval with **no flow restart** and
no special `UpdateConfig` hook.

```
util::starlink::spawn_starlink_poller
  └── grpc::StarlinkSource::sample ── reqwest (h2c) ─▶ http://<dish>:9200/SpaceX.API.Device.Device/Handle
        └─▶ DashMap<iface, StarlinkMetrics>  ◀── NetworkSampler::sample() (health tick)
```

### Quality bars

`bars` (0..=5) is the worst-of the PoP-ping drop rate and the obstruction
fraction (both 0..1, lower = better), gated on the connectivity state (`0` when
not `connected`, `None` when connected but no quality figures are available). The
UI reads colour first (green ≥ 4 bars, amber 2–3, red ≤ 1), numbers on hover.

## Configuration

Add one entry per interface to `config.json` (operational, safe for the manager):

```jsonc
"starlink_uplinks": [
  {
    "interface": "wlo5",             // kernel netdev this annotates
    "address": "192.168.100.1:9200"  // dish gRPC host[:port]; port defaults to 9200
    // "source_address": "10.0.0.2"  // optional — ONLY for >1 dish on one host
  }
]
```

There are **no secrets** — the dish gRPC is unauthenticated, so nothing is split
into `secrets.json`.

In the manager UI, configure dishes via **Node config → Starlink Monitoring →
Add Dish**. Use **Test reachability** to validate the endpoint (and the host
route, below) before saving — it runs one live `get_status` and reports the
decoded state + key figures, or the exact failure cause.

## Host prerequisite — a route to the dish

The dish's link telemetry lives at the **dish** gRPC (`192.168.100.1:9200`), not
the Starlink router. On a Starlink Mini / router the dish management address sits
on its own subnet, so the edge host usually needs a route to it via the Starlink
LAN gateway. For example, with a Wi-Fi leg `wlo5` on the Starlink LAN whose
gateway is `192.168.4.1`:

```bash
sudo ip route add 192.168.100.0/24 via 192.168.4.1 dev wlo5
```

Persist it in netplan (`routes:` under the interface) so it survives a reboot.
The poll reaches the dish **by address** (it does not bind the interface), so the
interface field is only the join key for the UI strip — the route is what makes
the dish reachable. If the dish can't be reached, the row shows the failure cause
and a `starlink_uplink_unreachable` event fires (below).

> Pointing the address at the Starlink **router** endpoint (e.g. `:9000` on the
> Mini) returns `wifi_get_status`, not `dish_get_status`; the decoder reports a
> clear error telling you to use the dish endpoint `192.168.100.1:9200`.

## Multiple dishes on one host

Every Starlink dish hard-codes the **same** management endpoint
(`192.168.100.1:9200`) — SpaceX gives you no way to change it. So two dishes on
one edge, reached over two interfaces, **collide on the host route**: a single
`192.168.100.0/24` route can only point one way, and the poller reaches the dish
by address. The `address` stays `192.168.100.1:9200` on *every* dish; the
disambiguator is the **interface + a `source_address` bind + per-leg policy
routing**.

Per dish, set `source_address` to that leg's source IP. The edge binds the poll
to it (`local_address`, unprivileged), and a per-leg policy route sends a poll
from that source IP out the right interface:

```jsonc
"starlink_uplinks": [
  { "interface": "wlo5", "address": "192.168.100.1:9200", "source_address": "192.168.4.102" },
  { "interface": "wwan0", "address": "192.168.100.1:9200", "source_address": "192.168.5.102" }
]
```

Host setup (one routing table per leg — the same pattern as bonding legs):

```bash
# leg 1 — wlo5
ip rule  add from 192.168.4.102 table 80
ip route add 192.168.100.0/24 via 192.168.4.1 dev wlo5  table 80
# leg 2 — wwan0
ip rule  add from 192.168.5.102 table 81
ip route add 192.168.100.0/24 via 192.168.5.1 dev wwan0 table 81
# loosen reverse-path filtering on the legs (asymmetric multi-homing)
sysctl -w net.ipv4.conf.wlo5.rp_filter=2 net.ipv4.conf.wwan0.rp_filter=2
```

Validation enforces the rule at config time: if two uplinks share an `address`,
each **must** carry a distinct `source_address` (a single-dish entry needs none).
A missing or duplicate source bind on a shared address is rejected with a clear
error. Single-dish installs are unaffected — leave `source_address` unset and the
poll uses the main-table route exactly as before.

## Events

Node-level, category `starlink`, debounced (catalogued in
[`events-and-alarms.md`](events-and-alarms.md#starlink-dish-events-starlink)):

| `error_code` | Severity | When |
|---|---|---|
| `starlink_state_changed` | info / warning | dish connectivity state transitions (warning on `unknown`; an unresponsive dish surfaces via `starlink_uplink_unreachable` instead) |
| `starlink_obstructed` | warning | the dish becomes obstructed (enter, hysteresis) |
| `starlink_obstruction_cleared` | info | obstruction clears and the rolling fraction is low |
| `starlink_alert` | warning | a hardware alert becomes active (thermal throttle / shutdown, motors stuck, …), once per newly-raised alert |
| `starlink_uplink_unreachable` | warning | the dish poll fails 3 cycles running |
| `starlink_uplink_recovered` | info | the dish poll succeeds after being unreachable |

## Quality thresholds (UI colour)

- **PoP-ping drop rate:** good ≤ 0.5 % · fair ≤ 5 % · poor ≤ 10 % · bad > 25 %
- **Obstruction fraction:** good ≤ 0.1 % · fair ≤ 1 % · poor ≤ 2 % · bad > 5 %

Bars are the worst-of the two. Operators read colour first; raw figures on hover.

## Code map

- Edge: `src/util/starlink/{mod.rs, grpc.rs}` (types, cache, poller, events;
  `grpc.rs` holds the hand-rolled gRPC client + protobuf decoder + golden tests),
  `src/util/network_interfaces.rs` (`starlink` field + join), `src/config/models.rs`
  (`StarlinkUplinkConfig`), `src/config/validation.rs` (`validate_starlink_uplinks`),
  `src/manager/client.rs` (`"starlink"` capability, `test_starlink_uplink` command,
  `update_config` copy), `src/manager/events.rs` (`category::STARLINK`).
- Manager: `crates/manager-core/src/models/ws_protocol.rs` (mirror),
  `crates/device-edge/src/lib.rs` (`test_starlink_uplink` in `EDGE_COMMANDS`),
  `crates/manager-server/src/ui/static/js/config/starlink_uplinks.js` (dish form),
  `crates/manager-server/src/ui/static/js/detail/flows.js`
  (`renderStarlinkStrip` + bond-leg + Network Interfaces card),
  `crates/manager-server/src/ui/node_config.html` (Starlink Monitoring section).

## Wire format reference

Verified against ewilken/starlink-rs + danopstech/starlink reverse-engineered
`.proto` trees and confirmed live against a Starlink Mini:

- Endpoint: `192.168.100.1:9200`, cleartext HTTP/2 (h2c, prior-knowledge), no TLS.
- Service `SpaceX.API.Device.Device`, method `Handle`, path
  `/SpaceX.API.Device.Device/Handle`.
- gRPC framing: `[1-byte compression flag = 0][4-byte BE length][protobuf]`;
  `te: trailers` + `content-type: application/grpc` + `grpc-encoding: identity`.
- Request: `Request{ get_status }` — field **1004** → frame `00 00 00 00 03 E2 3E 00`.
- Response: `Response{ dish_get_status }` — field **2004**. `DishGetStatusResponse`:
  `device_info`=1, `device_state`=2, `snr`=1001 (legacy), `seconds_to_first_nonempty_slot`=1002,
  `pop_ping_drop_rate`=1003, `obstruction_stats`=1004, `alerts`=1005, `state`=1006 (enum),
  `downlink_throughput_bps`=1007, `uplink_throughput_bps`=1008, `pop_ping_latency_ms`=1009.
  `DeviceInfo{ id=1, hardware_version=2, software_version=3, country_code=4 }`,
  `DeviceState{ uptime_s=1 }`,
  `DishObstructionStats{ fraction_obstructed=1, currently_obstructed=5 }`,
  `DishAlerts{ motors_stuck=1, thermal_shutdown=2, thermal_throttle=3,
  unexpected_location=4, mast_not_near_vertical=5, slow_ethernet_speeds=6 }`,
  `DishState{ UNKNOWN=0, CONNECTED=1, SEARCHING=2, BOOTING=3 }`.

> These protos are reverse-engineered, not SpaceX-published. The decoder is
> tolerant (additive — unknown fields skipped); the `DishState` enum mapping is
> the field most likely to drift across firmware and is the item to confirm
> against a live `get_status` capture if the state pill looks wrong while the
> link metrics are correct.

## Not applicable

This change does not touch the media / transport path, so the broadcast A/V
quality gates (`testbed/BROADCAST_QUALITY_GATES.md`) do not apply — no PCR /
A-V numbers are relevant.
