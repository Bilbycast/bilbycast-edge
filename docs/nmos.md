# NMOS support

bilbycast-edge implements the broadcast-audio, broadcast-data, and
uncompressed-video subsets of the AMWA NMOS specifications:

| Spec | Endpoint | Status |
|------|----------|--------|
| IS-04 v1.3 | `/x-nmos/node/v1.3/` | self, devices, sources, flows, senders, receivers |
| IS-04 v1.3 registration client | (outbound to a registry's `/x-nmos/registration/v1.3/`) | optional, opt-in via `nmos_registration` config block — see below |
| IS-05 v1.1 | `/x-nmos/connection/v1.1/` | single sender/receiver staged + active + transporttype + constraints |
| IS-08 v1.0 | `/x-nmos/channelmapping/v1.0/` | io, map/active, map/staged, map/activate (active map persists to disk) |
| BCP-004 | embedded in IS-04 receiver caps | constraint_sets for ST 2110 audio, data, and video inputs |
| mDNS-SD | `_nmos-node._tcp` | best-effort registration via `mdns-sd` (pure Rust) |

**Known gap — sender transport files.** Every IS-04 sender is serialised with
an empty `manifest_href` and an empty `interface_bindings` list, at all three
construction sites in `api::nmos` (list, single-get, and the registration-client
snapshot), and nothing anywhere fills either in. IS-05 matches: both `active`
responses hard-code `transport_file: null`, and the PATCH body type has no
`transport_file` field at all, so an SDP a controller stages against a receiver
is silently dropped by serde. Activating *this* node's sender still works — the
controller supplies `destination_ip` / port directly — but there is no transport
file to hand a downstream receiver and no NIC binding for a controller to pick
an interface from. `engine::st2110::sdp` can already generate and parse the SDP
that would go there, but it has no caller.

## Format detection

Each flow's input is classified at IS-04 list time:

- `InputConfig::St2110_30` / `St2110_31` → `urn:x-nmos:format:audio`
- `InputConfig::St2110_40` → `urn:x-nmos:format:data`
- `InputConfig::St2110_20` / `St2110_23` → `urn:x-nmos:format:video`
- everything else → `urn:x-nmos:format:mux`

The same classifier drives the receiver `caps` block:

- **Audio receivers** advertise BCP-004 constraint sets
  (`urn:x-nmos:cap:format:sample_rate`, `channel_count`, `sample_depth`).
- **Data receivers** advertise `media_types: ["video/smpte291"]`.
- **Video receivers** (ST 2110-20 / -23) advertise
  `media_types: ["video/raw"]` with BCP-004 constraints:
  - `urn:x-nmos:cap:format:color_sampling` = `YCbCr-4:2:2`
  - `urn:x-nmos:cap:format:component_depth` ∈ {8, 10}
  - `urn:x-nmos:cap:format:frame_width`, `frame_height` — the
    configured resolution
  - `urn:x-nmos:cap:format:grain_rate` — the configured
    `frame_rate_num` / `frame_rate_den`

NMOS controllers use these constraint sets to reject incompatible
senders before activation.

## Clocks

When any flow on the node sets `clock_domain`, the IS-04 `self` resource
includes a single PTP clock entry (`name: "clk0"`, `ref_type: "ptp"`).
Sources whose flow has `clock_domain` set reference this clock by name.
The `locked` field is reported as `false` until live PTP integration
lands; the manager UI uses `FlowStats.ptp_state.lock_state` for the real
view.

## IS-08 channel mapping

The IS-08 endpoints expose every ST 2110-30/-31 audio input and output
under `/io`. The active map is persisted to
`<config_dir>/nmos_channel_map.json` (next to `config.json`) and reloaded
on startup. The staged map is `GET` + `POST`, the active map is read-only
`GET`, and activation is a `POST` to `/map/activate` — there is no `PUT` on
either. Bilbycast does not currently re-route channels internally — the
map is a passthrough — but the endpoints exist so
external NMOS controllers can stage and activate maps and the manager UI
can render the channel layout.

Bounds: at most 1024 outputs per map, at most 64 channels per output. A
controller exceeding these returns 413 PAYLOAD_TOO_LARGE.

## mDNS-SD registration

On startup the edge calls
`api::nmos_mdns::spawn_nmos_node_advertisement` to register
`_nmos-node._tcp` on the local link, naming the addresses the API
server listens on (`server.listen_addrs`, else `server.listen_addr`,
after any `--bind` / `--bind-addrs` override), so that a controller
following the record reaches the API:

| API bind address | Addresses the record names |
|---|---|
| `0.0.0.0` | every IPv4 address of every interface that is up |
| `[::]` | every IPv6 address of every interface that is up, except link-local (`fe80::/10`) addresses (the edge binds `[::]` IPv6-only) |
| any other address, loopback included | that address |

`mdns-sd` looks the addresses up on the host's interfaces at
registration and follows them as they come and go; an address on a
point-to-point link is never named. The record is announced, and
queries for it are answered, only on an interface holding a named
address, with the named addresses in that interface's subnets: `mdns-sd`
keeps one address set for the record and decides which addresses belong
on an interface by prefix alone. Every interface's IPv6 link-local
address is in `fe80::/64`, so a link-local address named would be
announced on every IPv6 interface, where only its own link can reach it;
that is why `[::]` leaves them out. An interface whose only IPv6 address
is link-local therefore announces no IPv6 address: with an IPv4 bind it
announces the record with its IPv4 addresses alone, and without one not
at all. A `0.0.0.0` bind does name IPv4 link-local addresses
(`169.254.0.0/16`), so on a host where more than one interface holds
one, each of those interfaces announces them all.

The default bind, `127.0.0.1` and `[::1]`, keeps the record on
loopback: only this host can reach the API, and only this host sees the
record. The record is kept there rather than not registered, so a
controller on the same host still finds the node. The startup log line
`NMOS mDNS-SD: registered ... on host ... naming ...` gives the record's
host and lists what the record names.

Up to v0.117.0 the record named `127.0.0.1` whatever the bind, so it was
seen on loopback alone and no controller on the network could discover
a node through it.

The record's host is `<name>-bilbycast.local.`, where `<name>` is the
node's name, which the IS-04 node resource carries as its `label` and
`hostname`: `$HOSTNAME`, else `$HOST`, else the system host name, cut to
its first dot-separated part and made a valid DNS label (any character
but an ASCII letter, digit or hyphen becomes a hyphen, at most 63
bytes), else `bilbycast-edge`. Where `<name>-bilbycast` would pass the
63 bytes a label holds, `<name>` is cut short in the host. systemd
passes neither variable to a service unless its unit sets one, and the
packaged unit does not, so a packaged edge goes by its system host
name. Up to v0.117.0 it fell straight back to `bilbycast-edge`, and the
record's host was `bilbycast-edge.local.` on every packaged edge.

The host is not `<name>.local.` because the machine's own mDNS
responder, where one runs (avahi-daemon, or systemd-resolved with
MulticastDNS on), already answers for that name with the addresses of
its interfaces, not all of which the record names (it never names an
IPv6 link-local address). The record would lose its conflict probe to
that responder at every start and go out as `<name>-2.local.`. A rename
can still happen between records for the same host that name different
addresses: two edges on one host whose APIs listen on different
addresses, or edges on two hosts of the same name on one link. The one
that probes second, which depends on the order they start, loses:
`mdns-sd` renames its host (`<name>-bilbycast-2.local.`) and the edge
logs a warning naming the new name, once per name. Two edges on one
host whose APIs both listen on `0.0.0.0` name the same addresses and
keep the one host name. Discovery works either way, since the record's
SRV follows the rename; the IS-04 `hostname` stays `<name>`.

Failures (no multicast on the selected interface, or `mdns-sd` daemon
errors) are logged once and swallowed; flow startup is never blocked.

On a graceful shutdown (SIGTERM / SIGINT) the record is withdrawn once
the API server has stopped, before the flows stop: the daemon sends its
goodbye packets (TTL 0, so browsers drop the record) and exits, and the
edge waits up to 1 s for it to confirm. A process that is killed, or
that fails before reaching that point, can leave the record cached by
browsers until its TTLs run out: 2 minutes for the address and SRV
records, 75 minutes for the PTR and TXT records (`mdns-sd`'s defaults).

## Registration client (push to an NMOS registry)

mDNS-SD covers LAN-only deployments; registry-driven controllers
(Celebrum, Riedel MediorNet Control, Lawo VSM, EVS Cerebrum, …) usually
discover nodes through an NMOS **registry** instead. When
`nmos_registration.enabled: true`, the edge spawns a background task
that POSTs its IS-04 resources (node + device + sources + flows +
senders + receivers) to a configured registry and heartbeats the node
so the registry's query API surfaces the edge to controllers.

Configuration (in `config.json`):

```json
{
  "nmos_registration": {
    "enabled": true,
    "registry_url": "https://registry.example.com:8235",
    "api_version": "v1.3",
    "heartbeat_interval_secs": 5,
    "request_timeout_secs": 10,
    "bearer_token": "optional-static-bearer-token"
  }
}
```

| Field | Default | Notes |
|-------|---------|-------|
| `enabled` | `false` | Set to `true` to spawn the task. |
| `registry_url` | — | Base URL of the registry. **Do not** include `/x-nmos/...` — the path is appended internally. `http://` and `https://` are both accepted; `https://` is recommended for any non-loopback registry. |
| `api_version` | `"v1.3"` | Only `v1.3` is supported in this release. Older registries that speak only v1.2 / v1.1 are out of scope. |
| `heartbeat_interval_secs` | `5` | 1–60 s. AMWA recommends 5 s; the registry treats nodes as expired after roughly 12 s of missed heartbeats. |
| `request_timeout_secs` | `10` | 1–30 s. |
| `bearer_token` | `null` | Optional static `Authorization: Bearer …` header attached to every registry request. Persisted in `secrets.json` (envelope-encrypted) and stripped before sending the config to the manager. IS-10 OAuth2 client-credentials against the registry is **not** included in this release — operators that need it can bridge via a static token from a long-lived OAuth2 client. |

### Behaviour

- **Resource UUIDs are deterministic** — UUID v5 values rooted at the
  persisted `node_id`. Re-POSTing the same set updates the registry
  record in place; restarts do not orphan resources.
- **Change detection** — the task hashes the (versionless) resource set
  on every heartbeat tick. When the hash differs from the
  last-registered hash, every resource is re-POSTed with a fresh
  `version` stamp; otherwise only the heartbeat is sent. A typical
  busy edge re-POSTs only when an operator adds / removes / edits a
  flow.
- **Heartbeat** — POSTs to
  `<registry>/x-nmos/registration/v1.3/health/nodes/{node_id}` every
  `heartbeat_interval_secs`. A non-2xx response (typically `404`
  because the registry expired the node) drops the client back to the
  registration phase, which re-POSTs the whole set.
- **Backoff** — on network or 5xx errors the task retries with
  exponential backoff capped at 30 s. The IS-04 GET endpoints on this
  node continue to serve normally regardless.
- **Shutdown** — on `CTRL+C` / SIGTERM the task issues a single
  `DELETE /resource/nodes/{id}` (1 s timeout) so the registry stops
  listing the node immediately. Best-effort: a registry that is also
  going down is silently tolerated.

### Events

The task surfaces lifecycle on the manager's event feed under category
`nmos_registry` (see [`docs/events-and-alarms.md`](events-and-alarms.md)):

| Severity | error_code | Meaning |
|----------|-----------|---------|
| Info | `nmos_registered` | First successful POST of the node resource. |
| Warning | `nmos_heartbeat_lost` | Heartbeat returned non-2xx; client falls back to re-registering. |
| Critical | `nmos_registration_failed` | A registration POST returned 4xx/5xx. Client retries with backoff. |
| Warning | `nmos_registry_unreachable` | Network / DNS / TLS error reaching the registry. Client retries with backoff. |

### Compatibility with mDNS-SD

The registration client is purely additive — it does not turn off the
existing `_nmos-node._tcp` mDNS-SD advertisement. Operators on a
mixed network (some controllers using mDNS, some using a registry)
get both at no extra cost.

### What's not included

- mDNS-SD browsing of `_nmos-register._tcp` for **registry
  autodiscovery** (operators supply the URL manually for now).
- **Registry failover / multi-registry priority** — only a single
  registry URL is supported. Most production deployments front an HA
  registry behind a load balancer or VRRP and point the edge at the
  load-balancer URL.
- **IS-10 OAuth2 client-credentials grant** against the registry — the
  static bearer token covers most lab and private deployments.

## Authentication

NMOS auth follows a secure-by-default policy tied to the main `auth.enabled` switch:

- **`auth.enabled: true` and `nmos_require_auth` unset** — NMOS IS-04/IS-05/IS-08 require JWT Bearer auth. A startup info line confirms the policy.
- **`auth.enabled: true` and `nmos_require_auth: false`** — NMOS stays public to preserve compatibility with a controller that can't authenticate; a loud `SECURITY:` warning is logged at startup.
- **`auth.enabled: false`** — NMOS stays public regardless of `nmos_require_auth` (nothing to auth against). A warning is logged if `nmos_require_auth: true` is set, because it has no effect.

```json
{
  "server": {
    "auth": {
      "enabled": true,
      "jwt_secret": "...",
      "clients": [...]
    }
  }
}
```

When NMOS auth is active, controllers must obtain a token via `/oauth/token` first, then include `Authorization: Bearer <token>` in all NMOS requests. Both `admin` and `monitor` roles have full access.

## Browser-hosted controllers (`server.nmos_browser_control`)

Auth is a separate axis from **browser** reachability, and the two are often
confused. `/x-nmos/**` keeps its wildcard `Access-Control-Allow-Origin` so a
browser-hosted controller can discover the node, and `Accept`, `Content-Type`
and `Authorization` are always in `Access-Control-Allow-Headers` — without
`authorization` a browser could not attach a Bearer token and an auth-enabled
node would be unreachable from one even for a `GET`.

**Connection management from a browser is refused by default.** An IS-05
`PATCH .../staged` with `activate_immediate` re-points a live sender and
persists the change, and on the shipped default (`auth.enabled: false`) it is
unauthenticated — so any web page the operator happened to load could do it.
`api::server::guard_cross_origin_write` refuses a state-changing request that
carries browser fetch metadata (`Sec-Fetch-Site` / `-Mode` / `-Dest`, which
every current browser sends on every request and no native controller sends).
This is keyed on "a browser issued this" rather than on `Origin` disagreeing
with the request authority, because under DNS rebinding those two agree.

Native controllers (Sony, Riedel, Lawo, the AMWA testing tool) and `curl` send
neither header and are unaffected.

To run a browser-hosted controller such as `sony/nmos-js`, list its exact
origin:

```json
{
  "server": {
    "nmos_browser_control": ["https://nmos-js.example.tv"]
  }
}
```

Listed origins get `PATCH` / `POST` advertised in the NMOS preflight and pass
the guard. Scheme + authority only — no trailing slash, no path, no wildcard
(validation rejects all three, because a browser sends only
`scheme://host[:port]` in `Origin` and anything else would silently never
match). The list applies to `/x-nmos/**` alone; it never widens `/api/v1`.

Residual risk: this is *mitigated-partially*. IS-05 / IS-08 writes remain
unauthenticated by specification on auth-off nodes, so anything on the LAN that
is not a browser can still drive them; and when auth is on, a `monitor`-role
token is sufficient to re-point a live sender. The AMWA NMOS Testing Tool has
**not** been run against the method-restricted default policy — its generic
CORS check expects an `OPTIONS` to advertise the methods a resource supports,
which the default policy does not do for `.../staged` PATCH.
`nmos_browser_control` is the supported way to restore that.

## Backward compatibility

Multi-essence audio + data + video resources are additive — old NMOS
controllers that only consumed `format:mux` continue to work because
mux flows are still classified the same way. ST 2110-20 / -23 flows
advertise `format:video` with `video/raw` media type; controllers that
don't recognise `video/raw` will see the flow but ignore the constraint
set (graceful degrade). The IS-08 router is mounted under a fresh URL
prefix and is invisible to controllers that don't speak it. The mDNS-SD
registration is supplementary to manual NMOS registry configuration.

## IS-05 activation refusal

A `PATCH .../single/senders/{id}/staged` (or the receiver equivalent) carrying
`activation.mode: "activate_immediate"` is validated **before** the runtime is
touched. (Any other mode returns 501; a patch with no activation at all is
stored in the staged map unvalidated, because nothing has been applied yet.)
The controller's `destination_ip` / `source_port` arrive unchecked — applying
the patch only round-trips through serde, which type-checks the shape and
happily accepts `"not-an-ip:5004"` as a string — so the patched entity goes
through `validate_output` / `validate_input_definition` and then through a
cross-entity port check (`validate_port_conflicts_with_output` /
`..._with_input`, because a rewritten `bind_addr` moves a *local* port and can
newly collide with another input, output or tunnel on this node). Any failure
returns **HTTP 400** and the flow is left exactly as it was.

A sender activation changes transport parameters and never activation. The
output's stored `active` flag stands, so a passive sender takes the new parameters
with no task, raises Info `action: sender_params_saved_passive` rather than
`sender_activated`, and starts on them when it is switched on. `GET
.../single/senders/{id}/active` reports `master_enable` from that flag, and an
IS-04 sender resource its `subscription.active`. A `master_enable` in the `PATCH`
body is echoed back and not applied.

The handler's error type is a bare status code, so there is nowhere to put the
reason for the controller. It surfaces only as a Warning event on category
`nmos` with `error_code: "invalid_transport_params"` and details
`{ subsystem: "is-05", action: "sender_activation_refused" |
"receiver_activation_refused", resource_id, entity_id, error }` — so when a
controller reports a bare 400, the Events page is where the reason is.

## IS-05 and ST 2110-23

IS-05 `/transportparams` reports the **primary sub-stream leg** for
ST 2110-23 receivers (the first entry in `sub_streams`). Multi-leg
transport params for the full -23 sub-stream set are a pending
enhancement — IS-05 clients can still stage and activate the receiver
from the primary leg.

## Pending external validation

The following are documented as deferred until matching tooling becomes
available in the test lab:

- **AMWA NMOS Testing Tool** runs against IS-04, IS-05, IS-08, BCP-004.
  Expected pass matrix:
  - IS-04: pass on `test_01` (resources have valid UUIDs / formats /
    transports) through `test_19` (clocks).
  - IS-05: pass on staged/active round-trip for sender + receiver.
    Transport-file SDP advertisement for ST 2110 senders is **not** expected
    to pass — see the sender-transport-file gap above.
  - IS-08: pass on `io`, `map/active`, `map/staged`, `map/activate`
    happy paths.
  - BCP-004: pass on receiver caps containing `media_types` plus a
    `constraint_sets` block matching the configured sample rate /
    channel count / bit depth.
- **Sony NMOS Commissioning Tool** end-to-end smoke against a real
  Lawo or Riedel device.
