# NMOS support

bilbycast-edge implements the broadcast-audio, broadcast-data, and
uncompressed-video subsets of the AMWA NMOS specifications:

| Spec | Endpoint | Status |
|------|----------|--------|
| IS-04 v1.3 | `/x-nmos/node/v1.3/` | self, devices, sources, flows, senders, receivers |
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
