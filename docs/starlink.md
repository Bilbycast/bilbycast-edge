# Starlink dish telemetry

Read-only link-state monitoring for a bond leg (or any interface) that egresses
over a Starlink terminal. The edge polls the dish's **local gRPC API**, attaches
the live link state to the network interface, the manager renders it on the
**Network Interfaces** card (node detail) and as a compact strip on the bond
legs that report an egress netdev, and the edge emits a few debounced events.
**No writes to the dish, no sidecar, nothing installed on the Starlink
hardware.**

This is the satellite-link sibling of [`cellular.md`](cellular.md): same poller +
cache + health-tick join shape, same UI surfaces, same (absent) capability
gating — but the source is the Starlink dish gRPC rather than a modem / RutOS
router, and the metrics are link-quality figures (obstruction / throughput /
latency / drop-rate) rather than radio signal.

The bond-leg join is *byte-for-byte* the cellular one — `flows.js` builds
`slByIface` from `network_interfaces[].starlink` and resolves each leg by
`BondPathLegStats.interface` — so the coverage limits are identical and are
tabulated once, in
[cellular.md → Where the bond-leg strip appears](cellular.md#where-the-bond-leg-strip-appears).
In short: sender-side interface-mode UDP and Relay legs only; gateway-mode,
QUIC, RIST and every receiver-side leg report no interface and get no strip.

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

(`last_error` is absent on a healthy sample; when present the whole block is a
failure placeholder rather than a reading — see [Architecture](#architecture).)

All fields are additive (`Option` / `skip_serializing_if`), the `state` enum has a
`#[serde(other)]` catch-all — **no `WS_PROTOCOL_VERSION` bump**. Older managers
ignore the block; older edges omit it.

### The `"starlink"` capability gates nothing today

The edge advertises `"starlink"` on `HealthPayload.capabilities` whenever the
poller has at least one configured dish (`manager/client.rs`, gated on
`StarlinkCache::has_sources()`). **No manager UI surface reads that bit.** Both
strips render on *data presence* — `flows.js` draws the Network Interfaces strip
on `if (n.starlink)` and the bond-leg strip on `slByIface[p.interface]` — and
the Starlink sub-section under Uplink Monitoring is shown unconditionally. The
only place the string `'starlink'` appears anywhere in the manager UI is the
Events page category filter (`pages/events.js`: one label, one icon).

There is no `starlink-control` sibling to the cellular Wake gate either —
nothing in this feature is capability-gated. The comment beside the capability
push in `manager/client.rs` still claims the UI gates the strips on it; it does
not.

## Architecture

A single background **poller** task (`util::starlink::spawn_starlink_poller`,
under the app cancellation tree) samples every dish on a slow cadence (10 s), each
sample time-bounded (6 s). It writes the latest snapshot into a lock-free
`DashMap` cache keyed by kernel netdev. The ~15 s health tick joins that cache
onto each interface — no gRPC on the tick, **never on the data path**.

**Staleness, precisely.** Each cycle ends with
`cache.evict_older_than(now_ms − 60 s)`, but that cutoff can only ever reach an
interface the poller has **stopped polling** — the dish was removed from
`starlink_uplinks`, its `interface` was left blank, or `grpc::build_sources`
could not build an HTTP/2 client for it. **A configured dish that goes dark
never ages out.** Every failing cycle inserts a fresh
`StarlinkMetrics::unreachable(reason, now_ms)` placeholder (`state: offline`,
`bars: Some(0)`, `last_error: Some(cause)`) stamped with the current time, so
the entry is never older than one poll interval. `renderStarlinkStrip` sees
`last_error` and draws `⚠ UNREACHABLE` with the cause — indefinitely, not "no
data". That is the intent: an unroutable or misconfigured dish stays diagnosable
at a glance. Its "Last attempt: Ns ago" rides the badge tooltip; the `⟳ Ns` age
counter (whose own tooltip mentions the 60 s eviction) is drawn on the
successful-sample branch only.

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
not `connected`, `None` when connected but no quality figures are available).
Full ladders + colour mapping: [Bars and colour](#bars-and-colour).

## Configuration

Add one entry per interface to `config.json` (operational, safe for the manager):

```jsonc
"starlink_uplinks": [
  {
    "interface": "wlo5",             // kernel netdev this annotates (required)
    "address": "192.168.100.1:9200"  // dish gRPC host[:port]; port defaults to 9200
    // "source_address": "10.0.0.2"  // optional — ONLY for >1 dish on one host
    // "gateway": "192.168.4.1"      // optional — config.json ONLY, no UI control
  }
]
```

There are **no secrets** — the dish gRPC is unauthenticated, so nothing is split
into `secrets.json`.

`gateway` is a real field on `StarlinkUplinkConfig` and takes **first
precedence** in the route resolution below, but it is reachable only by editing
`config.json` (or by a direct config PUT). The manager's Add/Edit Dish form has
exactly three controls — Interface, Dish gRPC address, Source IP
(`starlink_uplinks.js::formHtml`) — and no gateway input. An edit merges with
`Object.assign({}, list[idx], entry)`, so a hand-written `gateway` **survives**
being edited in the UI; it simply cannot be created or changed there.

In the manager UI, configure dishes via **Node config → Uplink Monitoring →
Starlink (dish uplinks) → Add Dish**. There is no "Starlink Monitoring" tab:
`node_config.html` carries a single tab `data-tab="uplinks"` labelled **Uplink
Monitoring**, and Starlink is a sub-section inside it, alongside Cellular and
the shared-uplink capacity broker.

Use **Test reachability** (`.bc-sl-test` → `#bcSlTestResult`) to validate the
endpoint before saving. It issues the `test_starlink_uplink` WS command
(operator role), which builds a throwaway `StarlinkUplinkConfig` — honouring
`source_address`, always `gateway: None` — and runs one live `get_status`
through the same `grpc::build_sources` → `sample()` path the poller uses. It
returns `{ ok: true, state, software_version, hardware_version, device_id,
obstruction_fraction, pop_ping_latency_ms, pop_ping_drop_rate, downlink_bps,
uplink_bps, bars, alerts }`, or `{ ok: false, error: "<cause>" }`. Because the
probe passes no gateway it programs **no route** — it tests reachability over
whatever route the host already has, so a first-ever dish may need the poller's
own route pass (up to one 10 s cycle) before the probe succeeds.

## Route to the dish (maintained automatically)

The dish's link telemetry lives at the **dish** gRPC (`192.168.100.1:9200`), not
the Starlink router. On a Starlink Mini / router the dish management address sits
on its own `/24`, off the box's default route, so the edge host needs a route to
it via the Starlink LAN gateway.

**The edge programs and maintains this route itself — you do not add it by
hand.** Once a Starlink uplink is configured, the poller re-asserts
`192.168.100.0/24` (the dish address's `/24`) via the leg's gateway, in the
**main** routing table, on **every poll cycle** (10 s), with `replace` semantics
so it is idempotent and survives a Wi-Fi re-associate or DHCP lease change with no
operator action. The gateway is resolved in this order:

1. an explicit `gateway` on the uplink config, if set (config.json only — the
   Add/Edit Dish form has no control for it, see
   [Configuration](#configuration));
2. otherwise the interface's **kernel default-route next-hop** (read from
   `/proc/net/route`, so it works on any subnet — no assumption about your
   addressing);
3. as a last resort, `.1` of the leg's `/24`.

The `interface` field is both the UI join key **and** the `dev` the route is
programmed against, so it must name the leg the dish is on. Route programming uses
`CAP_NET_ADMIN`, which the packaged **systemd unit grants** (ambient) — so on a
normal install there is nothing to set up.

> **Manual fallback.** If you run the binary outside systemd without
> `CAP_NET_ADMIN` (e.g. a bare `cargo run` as an unprivileged user), the netlink
> route-add fails, the edge logs it at debug and carries on, and the dish stays
> unreachable until a route exists. Add the equivalent by hand — for a Wi-Fi leg
> `wlo5` on the Starlink LAN whose gateway is `192.168.4.1`:
>
> ```bash
> sudo ip route add 192.168.100.0/24 via 192.168.4.1 dev wlo5
> ```
>
> Persist it in netplan (`routes:` under the interface) so it survives a reboot.
> Because the edge uses `replace`, a stale hand-added route is harmless once the
> capability is restored.

By default the poll reaches the dish **by address**, over the main-table route
above: `interface` is the UI join key and the route's `dev`, never a socket
bind. **The exception is `source_address`** — when it is set,
`grpc::build_client` calls `reqwest`'s `local_address(ip)`, so the poll *is*
bound, to that local IP (not to the netdev). That bind is the whole multi-dish
mechanism described in the next section; with `source_address` unset there is no
bind of any kind. If the dish still can't be reached, the row shows the failure
cause and a `starlink_uplink_unreachable` event fires (below).

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

Host setup — **this part is manual.** The automatic main-table route above covers
the *single*-dish case; a single `192.168.100.0/24` route can only point one way,
so it can't disambiguate two dishes that share `192.168.100.1`. The edge binds
each poll to its `source_address` but does **not** install per-leg policy routes,
so you own these (one routing table per leg — the same pattern as bonding legs):

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

## Bars and colour

`bars` is the only thing the manager colours on — there is no per-metric
threshold table on the UI side. `util::starlink::derive_bars` short-circuits to
`Some(0)` on any state other than `Connected`, then bins the two quality figures
onto their own **six**-step ladders (clamped to `0.0..=1.0` first) and folds
them with `min` (worst-of). The strip reuses the cellular colour function
(`flows.js::cellBarsColor`), so the mapping is identical to
[cellular.md](cellular.md#bars-and-colour).

| bars | 5 | 4 | 3 | 2 | 1 | 0 |
|---|---|---|---|---|---|---|
| **PoP-ping drop rate** | ≤ 0.005 | ≤ 0.02 | ≤ 0.05 | ≤ 0.10 | ≤ 0.25 | > 0.25 |
| **Obstruction fraction** | ≤ 0.001 | ≤ 0.005 | ≤ 0.01 | ≤ 0.02 | ≤ 0.05 | > 0.05 |
| **colour** | green | green | amber | amber | red | red |

Green is `#3fb950` (≥ 4 bars), amber `#d29922` (2–3), red `#f85149` (≤ 1), grey
`#8b949e` when `bars` is `null`. Six quality steps therefore collapse onto three
colours plus "no reading" — an operator cannot tell 1 bar from 0 by colour
alone. `bars` is `null` only when the dish reports `Connected` and neither
figure is present; a non-`Connected` dish is `0`, not `null`. Operators read
colour first; raw figures on hover.

## Code map

- Edge: `src/util/starlink/{mod.rs, grpc.rs}` — `mod.rs` holds the types, cache,
  poller and events plus the host-route maintenance (`ensure_dish_route`,
  `derive_gateway`, `kernel_gateway_for`, driving
  `engine::bond_routing::BondRouteManager::program_dish_route`) and the
  `derive_bars` ladders; `grpc.rs` holds the hand-rolled gRPC client
  (`build_client`, where `source_address` becomes `local_address`),
  `build_sources`, the protobuf decoder and the golden tests.
  `src/util/network_interfaces.rs` (`starlink` field + join),
  `src/config/models.rs` (`StarlinkUplinkConfig`, `DEFAULT_DISH_ADDR`),
  `src/config/validation.rs` (`validate_starlink_uplinks`),
  `src/manager/client.rs` (`"starlink"` capability — advertised but unconsumed,
  `test_starlink_uplink` command, `update_config` copy), `src/manager/events.rs`
  (`category::STARLINK`).
- Manager: `crates/manager-core/src/models/ws_protocol.rs` (mirror),
  `crates/device-edge/src/lib.rs` (`test_starlink_uplink` in `EDGE_COMMANDS`),
  `crates/manager-server/src/ui/static/js/config/starlink_uplinks.js` (dish form
  + Test reachability; no gateway control),
  `crates/manager-server/src/ui/static/js/detail/flows.js`
  (`renderStarlinkStrip` — which reuses `cellBarsColor` — plus the `slByIface`
  bond-leg join and the Network Interfaces card),
  `crates/manager-server/src/ui/node_config.html` (**Uplink Monitoring** tab,
  Starlink sub-section).

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
