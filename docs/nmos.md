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
on startup. Both staged and active maps support the standard PUT/POST
+ activate workflow. Bilbycast does not currently re-route channels
internally — the map is a passthrough — but the endpoints exist so
external NMOS controllers can stage and activate maps and the manager UI
can render the channel layout.

Bounds: at most 1024 outputs per map, at most 64 channels per output. A
controller exceeding these returns 413 PAYLOAD_TOO_LARGE.

## mDNS-SD registration

On startup the edge calls
`api::nmos_mdns::spawn_nmos_node_advertisement` to register
`_nmos-node._tcp` on the local link. Failures (no multicast on the
selected interface, or `mdns-sd` daemon errors) are logged once and
swallowed; flow startup is never blocked. The handle is dropped on
process exit, which unregisters the service cleanly.

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

## Backward compatibility

Multi-essence audio + data + video resources are additive — old NMOS
controllers that only consumed `format:mux` continue to work because
mux flows are still classified the same way. ST 2110-20 / -23 flows
advertise `format:video` with `video/raw` media type; controllers that
don't recognise `video/raw` will see the flow but ignore the constraint
set (graceful degrade). The IS-08 router is mounted under a fresh URL
prefix and is invisible to controllers that don't speak it. The mDNS-SD
registration is supplementary to manual NMOS registry configuration.

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
  - IS-05: pass on staged/active round-trip for sender + receiver, with
    transport-file SDP advertisement for ST 2110 senders.
  - IS-08: pass on `io`, `map/active`, `map/staged`, `map/activate`
    happy paths.
  - BCP-004: pass on receiver caps containing `media_types` plus a
    `constraint_sets` block matching the configured sample rate /
    channel count / bit depth.
- **Sony NMOS Commissioning Tool** end-to-end smoke against a real
  Lawo or Riedel device.
