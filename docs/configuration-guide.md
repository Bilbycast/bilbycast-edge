# BilbyCast Edge Configuration Guide

Complete reference for the bilbycast-edge JSON configuration file. This guide covers every field, validation rule, and common configuration patterns.

---

## Table of Contents

- [Configuration File Basics](#configuration-file-basics)
- [Full Annotated Example](#full-annotated-example)
- [Top-Level Structure (AppConfig)](#top-level-structure-appconfig)
- [Server Configuration](#server-configuration)
- [TLS Configuration](#tls-configuration)
- [Auth Configuration](#auth-configuration)
- [Monitor Configuration](#monitor-configuration)
- [Manager Configuration](#manager-configuration)
- [Resource Limits](#resource-limits)
- [Structured JSON Logging](#structured-json-logging-logging)
- [Node Tuning](#node-tuning)
- [Tunnel Configuration](#tunnel-configuration)
- [Flow Configuration](#flow-configuration)
- [Input Types](#input-types)
  - [RTP Input](#rtp-input)
  - [UDP Input](#udp-input)
  - [RIST Input](#rist-input)
  - [SRT Input](#srt-input)
  - [RTMP Input](#rtmp-input)
  - [RTSP Input](#rtsp-input)
  - [WebRTC/WHIP Input](#webrtcwhip-input)
  - [WHEP Input](#whep-input)
  - [Media Player Input](#media-player-input)
  - [TestPattern Input](#testpattern-input)
  - [Bonded Input](#bonded-input)
  - [Mosaic Input (multiviewer wall)](#mosaic-input-multiviewer-wall-multiviewer-feature)
  - [SDI Input (Blackmagic DeckLink)](#sdi-input-blackmagic-decklink)
- [Output Types](#output-types)
  - [RTP Output](#rtp-output)
  - [UDP Output](#udp-output)
  - [RIST Output](#rist-output)
  - [SRT Output](#srt-output)
  - [RTMP Output](#rtmp-output)
  - [HLS Output](#hls-output)
  - [WebRTC Output](#webrtc-output)
  - [Bonded Output](#bonded-output)
  - [SDI Output (Blackmagic DeckLink playout)](#sdi-output-blackmagic-decklink-playout)
- **SMPTE ST 2110 audio + ANC** — see the dedicated section near the
  end of this guide and the deep-dive in
  [`audio-gateway.md`](audio-gateway.md). Covers ST 2110-30/-31 audio,
  ST 2110-40 ANC, the per-output `transcode` block (sample rate / bit
  depth / channel routing), the `rtp_audio` no-PTP variant, and SMPTE
  302M LPCM-in-MPEG-TS over SRT / UDP / RTP-MP2T (`transport_mode:
  "audio_302m"`).
- **Transcoding (`audio_encode` + `video_encode`)** — see
  [`transcoding.md`](transcoding.md) for the per-output support matrix,
  the licence-gated `video-encoder-*` Cargo features (x264, x265,
  NVENC), Linux build instructions, and the running list of Phase 4
  deferred items.
- **Multi-path bonding (`bonded` input / output type)** — see
  [`bonding.md`](bonding.md) for the full config schema (paths, scheduler,
  per-transport options for UDP / QUIC / RIST), worked edge-to-edge
  examples, stats / Prometheus reference, and tuning guidance. This is
  the Peplink-class aggregation path for N heterogeneous links; protocol-
  native bonding (SRT socket groups, RIST 2022-7) remains the right
  choice for homogeneous two-leg setups.
- **Multiviewer mosaic (`mosaic` input type)** — see
  [`multiviewer.md`](multiviewer.md) for the canvas/tile model, tile
  liveness badges, telemetry counters and `mosaic_*` events. Behind the
  off-by-default `multiviewer` Cargo feature, which also **requires** a
  `video-encoder-*` feature at runtime; all three published release
  artefacts carry both. Config schema:
  [Mosaic Input](#mosaic-input-multiviewer-wall-multiviewer-feature).
- [MPTS → SPTS filtering](#mpts--spts-filtering)
- [SMPTE 2022-1 FEC Configuration](#smpte-2022-1-fec-configuration)
- [SMPTE 2022-7 SRT Redundancy](#smpte-2022-7-srt-redundancy)
- [Native libsrt SRT Bonding (Socket Groups)](#native-libsrt-srt-bonding-socket-groups)
- [SRT Connection Modes](#srt-connection-modes)
- [CLI Argument Overrides](#cli-argument-overrides)
- [Config Persistence Behavior](#config-persistence-behavior)
- [Common Configuration Scenarios](#common-configuration-scenarios)

---

## Configuration File Basics

bilbycast-edge reads its configuration from two JSON files:

- **`config.json`** — Operational configuration (specified by `--config`, default: `./config.json`). Contains server settings, flow definitions (including user-configured parameters like SRT passphrases, RTSP credentials, RTMP stream keys, bearer tokens, HLS auth tokens), and tunnel routing.
- **`secrets.json`** — Infrastructure credentials (auto-derived: same directory as `config.json`). Contains manager auth secrets, tunnel encryption keys, API auth config (JWT secret, client credentials), TLS cert/key paths. Written with `0600` permissions on Unix.

If neither file exists at startup, an empty default configuration is used. Both files are loaded and merged into a single in-memory config, then validated at startup. Changes made through the API or manager commands are automatically persisted — flow configs and operational fields to `config.json`, infrastructure secrets to `secrets.json` — using atomic writes (write to temp file, then rename).

**Migration**: If upgrading from a version that used a single `config.json` with secrets, the node automatically splits them on first startup.

---

## Full Annotated Example

```json
{
  "version": 2,
  "device_name": "Studio-A Encoder",
  "setup_enabled": true,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080,
    "tls": {
      "cert_path": "/etc/bilbycast/cert.pem",
      "key_path": "/etc/bilbycast/key.pem"
    },
    "auth": {
      "enabled": true,
      "jwt_secret": "a-cryptographically-random-string-of-at-least-32-characters",
      "token_lifetime_secs": 3600,
      "public_metrics": true,
      "token_rate_limit_per_minute": 10,
      "clients": [
        {
          "client_id": "admin",
          "client_secret": "admin-secret-here",
          "role": "admin"
        },
        {
          "client_id": "grafana",
          "client_secret": "grafana-secret-here",
          "role": "monitor"
        }
      ]
    }
  },
  "monitor": {
    "listen_addr": "0.0.0.0",
    "listen_port": 9090
  },
  "inputs": [
    {
      "id": "rtp-in",
      "name": "Main RTP Input",
      "type": "rtp",
      "bind_addr": "239.1.1.1:5000",
      "interface_addr": "192.168.1.100",
      "fec_decode": {
        "columns": 10,
        "rows": 10
      },
      "allowed_sources": ["10.0.0.1", "10.0.0.2"],
      "allowed_payload_types": [33],
      "max_bitrate_mbps": 100.0,
      "tr07_mode": true
    }
  ],
  "outputs": [
    {
      "type": "rtp",
      "id": "rtp-local",
      "name": "Local Playout",
      "dest_addr": "192.168.1.50:5004",
      "interface_addr": "192.168.1.100",
      "fec_encode": {
        "columns": 10,
        "rows": 10
      },
      "dscp": 46
    },
    {
      "type": "srt",
      "id": "srt-remote",
      "name": "Remote Site via SRT",
      "mode": "caller",
      "local_addr": "0.0.0.0:0",
      "remote_addr": "203.0.113.10:9000",
      "latency_ms": 500,
      "passphrase": "my-encryption-passphrase",
      "aes_key_len": 32
    },
    {
      "type": "rtmp",
      "id": "twitch-out",
      "name": "Twitch Stream",
      "dest_url": "rtmp://live.twitch.tv/app",
      "stream_key": "live_123456789_abcdefghijklmnop",
      "reconnect_delay_secs": 5,
      "max_reconnect_attempts": 10
    }
  ],
  "flows": [
    {
      "id": "main-feed",
      "name": "Main Program Feed",
      "enabled": true,
      "input_ids": ["rtp-in"],
      "output_ids": ["rtp-local", "srt-remote", "twitch-out"]
    }
  ]
}
```

> **`"listen_addr": "0.0.0.0"` above is a deliberate widening, not the default.**
> A config the edge generates for itself binds **loopback only** — see
> [Server Configuration](#server-configuration). This example exposes the API on
> every interface, which is why it also enables `server.tls` and `server.auth`.
> Do not copy the `server` block without the other two.

---

## Top-Level Structure (AppConfig)

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `version` | integer | Yes | - | Schema version. Must be `2`. |
| `node_id` | string | No | Auto-generated | Persistent UUID v4 identifying this edge node. Auto-generated on first startup and saved to config. Used as the NMOS IS-04 Node ID. |
| `device_name` | string | No | `null` | Optional human-readable label for this edge node (e.g. "Studio-A Encoder"). Max 256 characters. |
| `setup_enabled` | boolean | No | `true` | When true, the browser-based setup wizard is accessible at `/setup`. Automatically flipped to `false` (and persisted to disk) after the node completes its first successful registration with a manager. Operators can also flip it manually. |
| `server` | object | Yes | - | API server configuration. |
| `monitor` | object | No | `null` | Web monitoring dashboard configuration. |
| `manager` | object | No | `null` | Manager WebSocket connection configuration. See [Manager Configuration](#manager-configuration). |
| `tuning` | object | No | `null` | Node-wide tuning defaults. See [Node Tuning](#node-tuning). |
| `inputs` | array | No | `[]` | Top-level input definitions. Each is an `InputDefinition` with `id`, `name`, and flattened protocol-specific fields (enum-tagged by `type`). See [Input Types](#input-types). Inputs exist independently and are referenced by flows via `input_ids`. |
| `outputs` | array | No | `[]` | Top-level output definitions. Each is an `OutputConfig` with `id`, `name`, and protocol-specific fields (enum-tagged by `type`). See [Output Types](#output-types). Outputs exist independently and are referenced by flows via `output_ids`. |
| `flows` | array | No | `[]` | List of flow configurations. Each flow references one or more inputs (one active at a time) and zero or more outputs by ID. See [Flow Configuration](#flow-configuration). |
| `tunnels` | array | No | `[]` | List of IP tunnel configurations. See [Tunnel Configuration](#tunnel-configuration). |
| `nmos_registration` | object | No | `null` | Optional NMOS IS-04 registration-client configuration. When enabled, the edge POSTs its IS-04 resources to an external NMOS registry. See [NMOS Registration Configuration](#nmos-registration-configuration). |
| `setup_token` | string | No | Auto-generated | One-shot bearer token gating `/setup` against non-loopback callers. Minted on first boot while `setup_enabled` is true, cleared on the first successful manager registration. Persisted **encrypted in `secrets.json`, never in `config.json`** — do not hand-author it. Re-print it with `--print-setup-token`. |
| `resource_limits` | object | No | `null` | System resource monitoring thresholds (CPU, RAM). See [Resource Limits](#resource-limits). |
| `logging` | object | No | `null` | Structured-JSON log shipper for SIEM / NMS pickup. See [Structured JSON Logging](#structured-json-logging-logging). |
| `flow_groups` | array | No | `[]` | SMPTE ST 2110 essence bundles — several flows that share PTP timing and NMOS activation. See [Flow groups](#flow-groups-essence-bundles). |
| `bond_uplinks` | array | No | `[]` | Per-NIC hard ceilings for the shared-leg capacity broker. Only needed on a metered link whose capacity the broker cannot infer; listing an uplink is **not** what enables the broker. See [`bonding.md`](bonding.md). |
| `shared_leg_broker` | boolean | No | unset → **on** | Explicit on/off for the shared-leg capacity broker. Unset means enabled. `false` reverts to uncoordinated per-bond contention. See [`bonding.md`](bonding.md). |
| `upgrades` | object | No | `null` | Manager-driven binary upgrades. Off unless `enabled`. See [`upgrade.md`](upgrade.md). |
| `cellular_uplinks` | array | No | `[]` | Read-only cellular telemetry sources (RutOS routers; ModemManager modems are auto-detected and need no entry). See [`cellular.md`](cellular.md). |
| `starlink_uplinks` | array | No | `[]` | Read-only Starlink dish telemetry sources. See [`starlink.md`](starlink.md). |

---

## Server Configuration

The `server` object controls the API server listener.

```json
{
  "server": {
    "listen_addr": "127.0.0.1",
    "listen_addrs": ["127.0.0.1", "[::1]"],
    "listen_port": 8080,
    "tls": { ... },
    "auth": { ... }
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `listen_addr` | string | Yes | `"127.0.0.1"` | Legacy single-address bind. **Ignored on bind whenever `listen_addrs` is set.** Kept for backward compatibility with pre-dual-stack configs. |
| `listen_addrs` | array of strings | No | `["127.0.0.1", "[::1]"]` | Dual-stack listener addresses — one listener bound per entry, e.g. `["0.0.0.0", "[::]"]`. IPv6 entries get `IPV6_V6ONLY=1` so they coexist with an IPv4 listener on the same port. When set and non-empty it **wins over `listen_addr`**; unset falls back to `[listen_addr]`. CLI override: `--bind-addrs 0.0.0.0,[::]`. |
| `listen_port` | integer | Yes | `8080` | TCP port for the API server. Shared by every entry in `listen_addrs`. |
| `tls` | object | No | `null` | TLS configuration for HTTPS (`tls` feature enabled by default). |
| `auth` | object | No | `null` | OAuth 2.0 / JWT authentication configuration. When absent or `enabled: false`, all endpoints are open. |
| `nmos_browser_control` | array of strings | No | `null` | Browser origins permitted to drive **NMOS connection management** (IS-05 `PATCH .../staged` and `.../activate`, IS-08 `POST /map/*`) from a web page. Absent or empty — the default — refuses every browser-issued NMOS state change, because NMOS writes are unauthenticated by specification and a foreign page could otherwise re-point a live sender persistently. Entries are exact scheme + authority, no path, no wildcard (`["https://nmos-js.example.tv"]`); at most 16, `http` or `https` only. Native controllers (Sony, Riedel, Lawo, the AMWA testing tool) send no `Origin` and are unaffected either way. Read once when the router is built at node start, so a pushed change lands on the next restart. See [`nmos.md`](nmos.md#browser-hosted-controllers-servernmos_browser_control). |

**A fresh node listens on loopback only.** `ServerConfig::default()` — what you
get when no `config.json` exists yet — binds `127.0.0.1` and `[::1]`, not
`0.0.0.0`. That is defence in depth, not an oversight: the local HTTP API and
the setup wizard ship with auth disabled, and the edge's control plane is the
**outbound** manager WebSocket, which needs no inbound listener at all. To reach
the API from the LAN, set `server.listen_addrs` (or pass
`--bind-addrs 0.0.0.0,[::]`) and enable `server.auth` at the same time.

---

## TLS Configuration

Optional sub-object of `server`. The `tls` feature is enabled by default.

```json
{
  "tls": {
    "cert_path": "/etc/bilbycast/cert.pem",
    "key_path": "/etc/bilbycast/key.pem"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cert_path` | string | Yes | Path to PEM-encoded TLS certificate file (or fullchain). Cannot be empty. |
| `key_path` | string | Yes | Path to PEM-encoded TLS private key file. Cannot be empty. |

If TLS is configured but the binary was built without the `tls` feature, a warning is logged and the server starts without TLS.

---

## Auth Configuration

Optional sub-object of `server`. See the [Security Guide](api-security.md) for detailed usage.

```json
{
  "auth": {
    "enabled": true,
    "jwt_secret": "at-least-32-characters-of-random-data",
    "token_lifetime_secs": 3600,
    "public_metrics": true,
    "token_rate_limit_per_minute": 10,
    "clients": [
      {
        "client_id": "admin",
        "client_secret": "strong-secret",
        "role": "admin"
      }
    ]
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled` | boolean | Yes | - | Master switch. When `false`, all endpoints are open. |
| `jwt_secret` | string | Yes (if enabled) | - | HMAC-SHA256 signing secret. Must be >= 32 characters. |
| `token_lifetime_secs` | integer | No | `3600` | JWT token lifetime in seconds. |
| `public_metrics` | boolean | No | `true` | Whether `/metrics` and `/health` are accessible without auth. |
| `nmos_require_auth` | boolean (optional) | No | *unset* → `true` when `enabled: true`, else `false` | Overrides the default. When unset and `enabled: true`, NMOS IS-04/IS-05/IS-08 require JWT Bearer auth. Set to `false` to explicitly leave NMOS public even when auth is enabled (a `SECURITY:` warning is logged). |
| `token_rate_limit_per_minute` | integer | No | `10` | Max OAuth token requests per minute per IP. Set to `0` to disable. |
| `clients` | array | Yes (if enabled) | - | Registered OAuth clients. At least one required. |

**Client fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `client_id` | string | Yes | Unique client identifier. Cannot be empty. |
| `client_secret` | string | Yes | Client authentication secret. Cannot be empty. |
| `role` | string | Yes | Must be `"admin"` or `"monitor"`. |

---

## Monitor Configuration

Optional top-level object. When present, bilbycast-edge starts a second HTTP server serving a self-contained HTML monitoring dashboard.

```json
{
  "monitor": {
    "listen_addr": "0.0.0.0",
    "listen_port": 9090
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `listen_addr` | string | Yes | IP address for the dashboard server. Legacy single-address field — **ignored on bind when `listen_addrs` is set.** |
| `listen_addrs` | array of strings | No | Dual-stack listener addresses for the dashboard, one listener per entry. Same semantics as [`server.listen_addrs`](#server-configuration), including `IPV6_V6ONLY=1` on v6 entries. `MonitorConfig` has no built-in default, so unset means "fall back to `[listen_addr]`". |
| `listen_port` | integer | Yes | TCP port for the dashboard. Must differ from `server.listen_port` if the same `listen_addr` is used. |

**Validation:** The monitor address must differ from the API server address (same IP + same port is rejected).

---

## NMOS Registration Configuration

Optional top-level object. When `enabled`, the edge spawns a background task
that POSTs its IS-04 resources (node + device + sources + flows + senders +
receivers) to an external NMOS registry and heartbeats the node so
registry-driven controllers (Celebrum, Riedel MediorNet Control, Lawo VSM,
EVS Cerebrum, …) discover the edge automatically. Full behavioural reference:
[`docs/nmos.md`](nmos.md) ("Registration Client").

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

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled` | boolean | No | `false` | Set to `true` to spawn the registration client. |
| `registry_url` | string | Yes (if enabled) | - | Base URL of the NMOS registry. **Do not** include `/x-nmos/...` — the path is appended internally. `http://` and `https://` are both accepted; max 2048 chars. |
| `api_version` | string | No | `"v1.3"` | IS-04 registration API version. Only `v1.3` is supported. |
| `heartbeat_interval_secs` | integer | No | `5` | 1–60 s. AMWA recommends 5 s; the registry treats nodes as expired after roughly 12 s of missed heartbeats. |
| `request_timeout_secs` | integer | No | `10` | 1–30 s. Request timeout for registration / heartbeat / delete. |
| `bearer_token` | string | No | `null` | Optional static `Authorization: Bearer …` value attached to every registry request. **Stored in `secrets.json`** (envelope-encrypted) and stripped before sending the config to the manager. Max 4096 chars. |

**Validation rules:** `registry_url` must start with `http://` or `https://`
and must not contain `/x-nmos`; `api_version` must equal `"v1.3"`;
`heartbeat_interval_secs` ∈ `[1, 60]`; `request_timeout_secs` ∈ `[1, 30]`.

**Disabled blocks are still validated**, so re-enabling at runtime via the
manager UI does not surface a deferred error.

---

## Manager Configuration

Optional connection to a bilbycast-manager instance for centralized monitoring and remote control. All communication uses an outbound WebSocket connection from the edge to the manager — no inbound connections are required, making this work behind NAT and firewalls.

```json
{
  "manager": {
    "enabled": true,
    "url": "wss://manager-host:8443/ws/node",
    "accept_self_signed_cert": false,
    "cert_fingerprint": "ab:cd:ef:01:23:45:67:89:..."
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `enabled` | boolean | No | `false` | Enable the manager connection. |
| `url` | string | Yes (if enabled) | - | Manager WebSocket URL. Must use `wss://` (TLS required). Example: `"wss://manager-host:8443/ws/node"`. Max 2048 characters. |
| `accept_self_signed_cert` | boolean | No | `false` | Accept self-signed TLS certificates from the manager. **Dev/testing only** — disables all TLS validation. Requires `BILBYCAST_ALLOW_INSECURE=1` environment variable as a safety guard. |
| `cert_fingerprint` | string | No | `null` | SHA-256 fingerprint of the manager's TLS certificate for certificate pinning. Format: hex with colons, e.g. `"ab:cd:ef:01:23:..."`. When set, connections to servers presenting a different certificate are rejected, even if the certificate is CA-signed. Protects against compromised CAs. The server's fingerprint is logged on first connection. |
| `registration_token` | string | No | `null` | One-time registration token from the manager. Used on first connection only. After successful registration, the token is cleared and replaced by `node_id` + `node_secret`. **Stored in `secrets.json`.** |
| `node_id` | string | No | `null` | Persistent node ID assigned by the manager during registration. Saved automatically. |
| `node_secret` | string | No | `null` | Persistent node secret assigned by the manager during registration. **Stored in `secrets.json`** (encrypted at rest). |

### Registration Flow

1. Create a node in the manager UI — you receive a one-time registration token.
2. Provide the token via the setup wizard (`http://<edge-ip>:8080/setup`) or in `secrets.json`.
3. Start the edge. It connects to the manager, sends the token, and receives `node_id` + `node_secret`.
4. Credentials are saved automatically: `node_id` to `config.json`, `node_secret` to `secrets.json`.
5. The registration token is cleared. Future connections use `node_id` + `node_secret`.
6. If the connection drops, the edge auto-reconnects with exponential backoff (1s to 60s).

### Validation Rules

- `url` must start with `wss://` (plaintext `ws://` is rejected).
- `url` max 2048 characters.
- `registration_token` max 4096 characters.
- `accept_self_signed_cert: true` is rejected unless `BILBYCAST_ALLOW_INSECURE=1` is set.

---

## Resource Limits

Optional top-level `resource_limits` block. When set, the edge
samples CPU and RAM usage on a periodic tick and emits Warning /
Critical events under category `system_resources` when thresholds
are exceeded. Optionally gates new flow creation when resources
are critical.

```json
{
  "version": 2,
  "resource_limits": {
    "cpu_warning_percent": 80,
    "cpu_critical_percent": 95,
    "ram_warning_percent": 80,
    "ram_critical_percent": 95,
    "critical_action": "alarm",
    "grace_period_secs": 10
  },
  "inputs": [],
  "outputs": [],
  "flows": []
}
```

| Field | Default | Notes |
|-------|---------|-------|
| `cpu_warning_percent` | `80` | CPU usage warning threshold. Range `[0, 100]`. |
| `cpu_critical_percent` | `95` | CPU usage critical threshold. |
| `ram_warning_percent` | `80` | RAM usage warning threshold. Range `[0, 100]`. |
| `ram_critical_percent` | `95` | RAM usage critical threshold. |
| `critical_action` | `"alarm"` | Behaviour on critical state. `"alarm"` — events only, flows continue. `"gate_flows"` — additionally reject new flow creation while any metric is critical. |
| `grace_period_secs` | `10` | Seconds the metric must continuously exceed the threshold before the event fires (debounce). |

Omit the block to disable system-resource alarms entirely. The
edge's resource-budget probe (advertised on
`HealthPayload.resource_budget`) is independent — that's a
one-shot hardware-capability snapshot at startup, not a runtime
metric. Both surfaces feed the manager UI's per-node Resources
card; this block is the operator-tunable side, the budget probe
is the static side.

Events emitted (category `system_resources`):
`system_resources_cpu_warning`, `system_resources_cpu_critical`,
`system_resources_ram_warning`, `system_resources_ram_critical`,
`system_resources_recovered`. With `critical_action: "gate_flows"`,
new-flow rejections additionally surface as a save-time error on
the manager UI.

---

## Structured JSON Logging (`logging`)

Optional top-level `logging` block. When a `json_target` is configured, **every
operational event the edge emits** — the same events that ride the manager
WebSocket `event` channel and are catalogued in
[`events-and-alarms.md`](events-and-alarms.md) — is additionally written as one
JSON line to the chosen sink. This is how a Splunk / Skyline DataMiner / Loki /
generic syslog stack picks the edge up without polling the manager. It is
purely additive: the manager push and the Prometheus `/metrics` surface are
unaffected.

```json
{
  "logging": {
    "json_target": {
      "kind": "file",
      "path": "/var/log/bilbycast/events.jsonl",
      "format": "splunk",
      "max_size_mb": 64,
      "max_backups": 5
    }
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `json_target` | object | No | `null` | The sink. Absent disables the shipper. |
| `json_target.kind` | string | Yes | - | Selects the variant: `"stdout"`, `"file"` or `"syslog"`. |
| `json_target.format` | string | No | `"raw"` | Envelope shape, on all three variants. `"raw"` — generic single-line JSON. `"splunk"` — wraps the envelope in a top-level `{"event": ...}` object so a Splunk HTTP Event Collector forwarder ingests the same line. `"dataminer"` — Skyline DataMiner field renames (`error_code` → `parameter_id`). |
| `json_target.path` | string | `file` only | - | Absolute path to the active log file. 1–4096 characters, no NUL bytes. The edge creates the parent directory **best-effort** when the shipper starts (`observability::log_shipper`), then opens the file create/append — so a fresh `"/tmp/events.jsonl"` needs no `mkdir -p`. Startup fails with `opening log_shipper file at <path>` only if the open itself fails, which on a running host means a permissions problem (the service user cannot write there), not a missing directory. |
| `json_target.max_size_mb` | integer | No | `64` | `file` only. Rotate when the active file exceeds this size. Range 1–4096. Backups are `<path>.1` (most recent) … `<path>.N`. |
| `json_target.max_backups` | integer | No | `5` | `file` only. Rotated backups retained; the oldest beyond this is dropped. Range 0–100. `0` truncates on rotate and keeps no backups. |
| `json_target.addr` | string | `syslog` only | - | Syslog destination as `host:port`, e.g. `"127.0.0.1:514"`. RFC 5424 over **UDP**, fire-and-forget — a black-holed collector never blocks the edge. |

**Validation** rejects an empty or over-long `path`, a `path` containing a NUL
byte, a `max_size_mb` outside 1–4096, a `max_backups` above 100, and a syslog
`addr` that does not parse as a socket address. `stdout` takes no further
fields — use it in a container where the runtime already forwards stdout.

**Over the manager, an `update_config` push that omits `logging` preserves what
the node already holds** (same treatment as `monitor`, `upgrades`,
`resource_limits`, `nmos_registration` and `tuning`), so a push from a manager
that does not manage this block cannot silently switch a SIEM feed off.

---

## Node Tuning

Optional top-level `tuning` block holding node-wide defaults. Every
field here was previously reachable **only** through an environment
variable, which meant the manager could neither show nor set it, an
operator had to edit a systemd unit and restart per node, and nothing
was audited. They are ordinary config fields now, so they arrive over
the same validated `UpdateConfig` path as everything else.

Every field is optional; in a hand-edited `config.json`, omitting one (or
omitting the whole block) uses the built-in default.

**Over the manager that last sentence inverts, and the difference bites.** An
`UpdateConfig` push that omits `tuning` entirely does **not** reset the block —
it *preserves* whatever the node already holds. That is deliberate:
`tuning.ingress_dejitter_ms` both enables and sizes the ingress de-jitter
buffer, so treating an absent key as "clear it" would let any unrelated push —
a device rename, a visual deploy, a config restore, a reconcile retry —
silently switch a live buffer off on every raw UDP/RTP input on the node.
**Clearing the block therefore needs an explicit `"tuning": {}`**, which
deserialises to all-`None` and resolves to the built-in defaults; the manager's
Tuning tab sends exactly that. The same preserve-when-absent rule covers
`monitor`, `upgrades`, `resource_limits`, `logging`, `nmos_registration` and
`device_name`.

```json
{
  "version": 2,
  "tuning": {
    "ingress_dejitter_ms": 80,       // node default de-jitter setpoint (UDP/RTP)
    "ingress_residence_ms": 320,     // hard-shed cap for that buffer
    "probe_session_limits": true,    // startup HW session-capacity probe
    "probe_4k": false,               // skip the second-tier 4K pass
    "media_player_controller": true, // media-player operator transport control
    "media_player_pcr_deadlines": true // PCR-anchored TS playout pacing
  },
  "inputs": [],
  "outputs": [],
  "flows": []
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `ingress_dejitter_ms` | integer | No | `60` | Node-wide default ingress de-jitter setpoint, in ms of content. Range 20–2000. Applies to raw **UDP and RTP** inputs that do not carry their own `ingress_dejitter_ms`, and it both **switches the buffer on** and sets its depth — so setting it here de-jitters every such input on the node. It does **not** apply to SRT (TSBPD de-jitters at the transport layer), RTSP, RTMP or `bonded` inputs, which run ingress passthrough by design. |
| `ingress_residence_ms` | integer | No | `max(4 × setpoint, 250)` ms | Node-wide default hard-shed residence cap for that buffer. A packet older than this is shed rather than released late, which is what bounds ingress latency when a burst or a source-rate offset exceeds the servo's ±5 % authority. Range `ingress_dejitter_ms + 40` .. `5000`; node-wide the floor is checked against `tuning.ingress_dejitter_ms`, or the built-in 60 ms when that is unset. A per-input `ingress_residence_ms` overrides it. |
| `probe_session_limits` | boolean | No | `true` | Run the startup hardware encoder/decoder session-capacity probe. `false` trades the manager's "sessions used **of** max" denominator for a faster boot, and disables **both** tiers. See [Capacity & resource budget](#capacity--resource-budget). |
| `probe_4k` | boolean | No | `true` | Run the second-tier 4K session-capacity probe. Ignored when `probe_session_limits` is `false` — that disables both tiers. |
| `media_player_controller` | boolean | No | `true` | Node-wide default for the media-player operator-control (transition) path — the state machine the manager's **Next** button drives. `false` selects the legacy sequential playout loop **and** withdraws the `media-player-control-v1` capability, so Next disappears from every media-player flow on the node rather than being offered and refused. A per-input `operator_control` always wins. |
| `media_player_pcr_deadlines` | boolean | No | `true` | Node-wide default for PCR-anchored TS playout pacing. `false` selects the legacy byte-rate estimate, whose error integrates without bound on variable-bitrate assets. A per-input `pcr_deadlines` always wins. |

**When a pushed change lands.** The two probe switches are read once at
node start, so an edit to either takes effect at the node's next
restart; the push says so — it raises a Warning `tuning_requires_restart`
event naming both fields, rather than leaving the operator to infer it
from an unchanged Resources card. The two ingress knobs and the two
media-player knobs are re-installed on the push and re-read on every
input spawn, so a flow restart or a hot input swap picks them up; an
input already running keeps the values it spawned with. No restart
warning is raised for those four, because none is needed.

**Per-input overrides.** UDP and RTP inputs carry their own
`ingress_dejitter_ms` and `ingress_residence_ms` (see
[RTP Input](#rtp-input)), and `media_player` inputs carry
`operator_control` and `pcr_deadlines`, so one input can be tuned
without moving the node default. Precedence, highest first: **per-input field → `tuning`
→ the legacy environment variable, where one is still read → the
built-in default.**

The environment variable sits **below** the config field deliberately.
Env-above-config would reintroduce exactly the trap this block exists
to close: an operator sets the field in the UI, sees it saved, and it
never applies because a unit file outranks it. Setting one of the
legacy variables raises a Warning `deprecated_env_var` event once at
startup (category `config`, with `details.env_var` / `.replacement` /
`.status`) and logs to the journal, so a stale unit file is visible on
the manager's Events page rather than silently steering the node.

| Config field | Legacy environment variable | Status |
|---|---|---|
| `tuning.ingress_dejitter_ms` | `BILBYCAST_INGRESS_BUFFER_MS` | **Removed** — it never had any effect, in any release: the node-wide setpoint was only consulted after the per-input setpoint had already answered, so no value it held could change behaviour. Still reported at startup so a unit file that sets it cannot state an intent that isn't being applied. |
| `tuning.ingress_residence_ms` | `BILBYCAST_INGRESS_RESIDENCE_MS` | Deprecated — still read for one release, below the config field. |
| `tuning.probe_session_limits` | `BILBYCAST_PROBE_SESSION_LIMITS` | Deprecated — still read for one release, below the config field. |
| `tuning.probe_4k` | `BILBYCAST_PROBE_4K` | Deprecated — still read for one release, below the config field. |
| `tuning.media_player_controller` | `BILBYCAST_MEDIA_PLAYER_CONTROLLER` | Deprecated — still read for one release, below the config field. |
| `tuning.media_player_pcr_deadlines` | `BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES` | Deprecated — still read for one release, below the config field. |
| *(none — deliberately)* | `BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4` | **Removed.** The bounded incremental MP4/MOV reader is unconditional in release builds. This selected the whole-file demux, which holds an entire asset resident — the out-of-memory the bounded reader was written to fix. A control whose "off" position is a known OOM does not belong on an operator's screen, so unlike its two siblings it was not given a config field; it survives in debug builds only. |

**Manager UI.** Manager → node → **Configure** → **Tuning**. The tab is
gated on the `node_tuning` capability advertised on
`HealthPayload.capabilities` — an edge without the bit accepts the
block and ignores it, which looks exactly like success.

The **Media Player** section of that tab is gated separately, on
`media_player_tuning`. Those two fields landed after `node_tuning`
shipped, so an edge from that release advertises `node_tuning`, accepts
them and ignores them — the very accept-and-ignore failure `node_tuning`
exists to prevent, recreated one release later by reusing the bit. The
rest of the tab still renders on such an edge.

---

## Tunnel Configuration

IP tunnels create encrypted point-to-point links between edge nodes, either through a bilbycast-relay server (for NAT traversal) or directly via QUIC (when one edge has a public IP).

### Relay Mode

Both edges connect outbound to a bilbycast-relay server. The relay pairs them by tunnel UUID and forwards traffic. End-to-end encryption ensures the relay cannot read payloads.

`relay_addrs` is an ordered list: index 0 is the primary, and an optional second entry is the backup. When the primary becomes unreachable, the edge automatically fails over to the backup; when the primary recovers, an RTT-gated probe fails back (see [Redundant Relay Failover](#redundant-relay-failover)).

```json
{
  "tunnels": [
    {
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "name": "Stadium to Studio",
      "protocol": "udp",
      "mode": "relay",
      "direction": "egress",
      "local_addr": "0.0.0.0:9000",
      "relay_addrs": [
        "relay-primary.example.com:4433",
        "relay-backup.example.com:4433"
      ],
      "tunnel_encryption_key": "0123456789abcdef...",
      "tunnel_bind_secret": "fedcba9876543210..."
    }
  ]
}
```

The legacy single-field `"relay_addr": "host:port"` form is still accepted on load and migrated into `relay_addrs[0]` automatically, but new configs should use `relay_addrs`.

### Direct Mode

One edge has a public IP. Direct QUIC connection between edges — no relay needed.

```json
{
  "tunnels": [
    {
      "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
      "name": "Direct Link",
      "protocol": "tcp",
      "mode": "direct",
      "direction": "ingress",
      "local_addr": "127.0.0.1:9000",
      "direct_listen_addr": "0.0.0.0:4433",
      "tunnel_psk": "abcdef0123456789..."
    }
  ]
}
```

### Tunnel Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `id` | string | Yes | - | Unique tunnel identifier. Must be a valid UUID. Both edges in a tunnel pair must use the same ID. |
| `name` | string | Yes | - | Human-readable name. |
| `enabled` | boolean | No | `true` | Whether the tunnel is active. |
| `protocol` | string | Yes | - | `"tcp"` (reliable, ordered — QUIC streams) or `"udp"` (unreliable — QUIC datagrams, best for SRT and media). |
| `transport` | string | No | `"quic"` | Outer carrier. `"quic"` (default): TCP rides QUIC streams, UDP rides QUIC datagrams. `"udp"`: native plain-UDP carrier (no QUIC) for native SRT/RIST over relay/direct — avoids QUIC overhead + a second congestion controller. Only valid with `protocol = "udp"`. See [Native SRT/RIST over relay](#native-srtrist-over-relay). |
| `mode` | string | Yes | - | `"relay"` (via relay server) or `"direct"` (QUIC peer-to-peer). |
| `direction` | string | Yes | - | `"ingress"` (receives tunnel traffic, forwards to `local_addr`) or `"egress"` (listens on `local_addr`, sends into tunnel). |
| `local_addr` | string | Yes | - | For **egress**: listen address for local traffic to tunnel (e.g. `"0.0.0.0:9000"`). For **ingress**: forward destination for received traffic (e.g. `"127.0.0.1:9000"`). |
| `relay_addrs` | string[] | Relay mode | `[]` | Ordered list of relay server QUIC addresses (e.g. `["relay1:4433", "relay2:4433"]`). Index 0 is the primary; a second entry enables automatic primary↔backup failover. Max 2 entries. Required for relay mode. |
| `relay_addr` | string | No | `null` | **Legacy.** Single relay address. Accepted on load for backward compatibility and migrated into `relay_addrs[0]`. Prefer `relay_addrs` in new configs. |
| `max_rtt_failback_increase_ms` | integer | No | `50` | When the active backup is in use and the primary recovers, failback is refused if the primary's measured QUIC RTT exceeds the backup's by more than this many ms. Prevents flapping back to a degraded primary. |
| `tunnel_encryption_key` | string | Relay mode | `null` | End-to-end ChaCha20-Poly1305 encryption key. Hex-encoded, exactly 64 chars (32 bytes). Required for relay mode. Both edges must share the same key. **Stored in `secrets.json`.** |
| `tunnel_bind_secret` | string | No | `null` | HMAC-SHA256 bind authentication secret. Hex-encoded, exactly 64 chars. Proves authorization to bind on the relay. **Stored in `secrets.json`.** |
| `peer_addr` | string | Direct egress | `null` | Remote peer QUIC address (e.g. `"203.0.113.50:4433"`). Required for direct mode, egress direction. |
| `direct_listen_addr` | string | Direct ingress | `null` | QUIC listen address (e.g. `"0.0.0.0:4433"`). Required for direct mode, ingress direction. |
| `tunnel_psk` | string | No | `null` | Pre-shared key for direct mode authentication. Hex-encoded, 64 chars. Both edges must share the same PSK. **Stored in `secrets.json`.** |
| `tls_cert_pem` | string | No | Auto-generated | TLS certificate PEM for direct mode listener. Auto-generated if absent. **Stored in `secrets.json`.** |
| `tls_key_pem` | string | No | Auto-generated | TLS private key PEM for direct mode listener. **Stored in `secrets.json`.** |
| `interface` | string | No | `null` | **Per-tunnel uplink pin** (1–15 chars, e.g. `"wwan0"`). Only on `transport = "udp"`. Pins the socket to a NIC via `SO_BINDTODEVICE`, falling back to the unprivileged `IP_UNICAST_IF` hint when the edge lacks `CAP_NET_RAW` (the normal case). See [Per-tunnel uplink pinning](#per-tunnel-uplink-pinning). |
| `source` | string | No | `null` | Source address (`ip` or `ip/prefix`) the tunnel's UDP socket binds to. Pins the egress source IP; in gateway mode it also keys the policy rule. Only on `transport = "udp"`. |
| `gateway` | string | No | `null` | **Gateway-mode** next-hop the tunnel egresses through (single-NIC / dumb-switch topology). The edge programs a `from <source> → default via <gateway>` policy route via netlink (`CAP_NET_ADMIN`). Requires `source` + `interface`. Only on `transport = "udp"`. See [`bonding-gateway-routing.md`](bonding-gateway-routing.md). |

### Tunnel Validation Rules

- `id` must be a valid UUID.
- `relay_addrs` (or legacy `relay_addr`) required when `mode` is `"relay"`; at least one, at most two entries; each 1–256 chars; duplicates rejected.
- `tunnel_encryption_key` required for relay mode; must be exactly 64 hex characters.
- `tunnel_bind_secret` must be exactly 64 hex characters if present.
- `peer_addr` required for direct mode egress.
- `direct_listen_addr` required for direct mode ingress.
- `tunnel_psk` must be exactly 64 hex characters if present.
- `transport = "udp"` requires `protocol = "udp"`.
- `interface` / `source` / `gateway` only accepted on `transport = "udp"`, and never on a direct **ingress** (listener) tunnel. `interface` is 1–15 chars; gateway-mode requires `source` + `interface`, and `gateway` must be a valid IP inside the `source` subnet (same address family).
- All address fields must be valid socket addresses.

### Native SRT/RIST over relay

Setting `transport: "udp"` (with `protocol: "udp"`) carries one SRT or RIST stream over a **native plain-UDP** tunnel rather than QUIC. Both edges still dial the relay outbound (so both ends can be behind NAT), and the relay forwards the datagrams verbatim — no QUIC framing, no second congestion controller fighting the inner SRT/RIST ARQ, which owns recovery end-to-end.

RIST uses an even RTP port plus the next odd RTCP port, so a native-RIST service is provisioned as a **pair** of single-port plain-UDP tunnels (one per port) so RTCP/NACK retransmission traverses correctly. The manager creates the pair automatically.

### Per-tunnel uplink pinning

On the plain-UDP carrier a tunnel can be pinned to a specific uplink so several tunnels on one box each egress out their own NIC (5G vs Starlink vs ISP) — the same mechanism a bonded UDP leg uses:

- **Interface mode** — set `interface` (e.g. `"wwan0"`). The socket pins to that NIC via `SO_BINDTODEVICE`, falling back to the unprivileged `IP_UNICAST_IF` egress hint when the edge has no `CAP_NET_RAW` (so it works without the capability). Add `source` to also pin the egress source IP.
- **Gateway mode** — for a single-NIC / dumb-switch topology, set `source` + `interface` + `gateway`; the edge programs a `from <source> → default via <gateway>` policy route (needs `CAP_NET_ADMIN`). See [`bonding-gateway-routing.md`](bonding-gateway-routing.md).

The pin is honoured for relay mode (both directions) and direct **egress**; a direct ingress (listener) tunnel uses the host default route and rejects the fields. This is also the foundation for [relayed bond legs](bonding.md#relayed-and-nic-pinned-legs): each relayed leg is its own native-UDP tunnel, loopback-bridged to the bond leg.

### Redundant Relay Failover

When `relay_addrs` contains a second entry, the edge provides automatic primary↔backup failover:

- **Detection.** The QUIC transport uses a 5 s keep-alive interval and a 25 s max-idle timeout, so a dead relay is detected after ~25 s of silence. This tolerates typical Starlink satellite handovers and mobile cell-handoffs without flapping.
- **Failover.** Once the primary is detected down, the edge reconnects and walks to the next relay in `relay_addrs`. Each reconnect attempt is bounded to 6 s so a dead primary cannot stall the loop behind the transport timeout. Expected end-to-end failover budget is **~30–40 s** on WAN links (both edges detect independently; the slower side sets total latency).
- **Waiting convergence.** If the two edges initially land on different relays, the first-to-bind sees `Waiting`; after 10 s it steps forward to the next relay so the pair converges on the same one.
- **Failback.** A background probe (every 60 s) measures the primary's QUIC RTT. When the primary's RTT is within `max_rtt_failback_increase_ms` (default 50 ms) of the currently-active backup, traffic fails back to the primary. This RTT gate prevents returning to a degraded primary that is reachable but slow.
- **Event visibility.** Each failover emits a Warning event to the manager with `from_relay_addr`, `to_relay_addr`, `from_idx`, `to_idx` details.

Failover is only engaged for tunnels with two relays configured. A tunnel with a single `relay_addrs` entry will simply reconnect to that same address until it returns.

---

## Flow Configuration

Flows connect one or more inputs to zero or more outputs by reference. A flow may have multiple inputs but at most one is active (publishing to the broadcast channel) at a time. Inputs and outputs are defined as independent top-level entities in the `inputs` and `outputs` arrays; a flow references them by ID via `input_ids` and `output_ids`. An input or output can only be assigned to one flow at a time. Unassigned inputs and outputs are configured but not running.

At startup (or on create/update), `AppConfig::resolve_flow()` dereferences the IDs into a `ResolvedFlow` containing `Vec<InputDefinition>` and `Vec<OutputConfig>`. The engine only ever sees `ResolvedFlow`.

```json
{
  "id": "main-feed",
  "name": "Main Program Feed",
  "enabled": true,
  "input_ids": ["rtp-in"],
  "output_ids": ["rtp-local", "srt-remote", "twitch-out"]
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `id` | string | Yes | - | Unique identifier. Cannot be empty. Must be unique across all flows. |
| `name` | string | Yes | - | Human-readable display name. Cannot be empty. |
| `enabled` | boolean | No | `true` | Whether to auto-start this flow on startup or creation. |
| `media_analysis` | boolean | No | `true` | Enable media content analysis (codec, resolution, frame rate detection). |
| `thumbnail` | boolean | No | `true` | Enable thumbnail generation (in-process via libavcodec; no external ffmpeg required). |
| `thumbnail_program_number` | integer | No | `null` | When the input is an MPTS, render the thumbnail from this MPEG-TS program only. `null` uses the first program found. Must be `> 0` if set. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `bandwidth_limit` | object | No | `null` | Per-flow bandwidth monitoring (RP 2129). See [Bandwidth Limit](#bandwidth-limit). |
| `input_ids` | array of strings | No | `[]` | IDs of inputs from the top-level `inputs` array. Each referenced input must exist and must not already be assigned to another flow. At most one input may be active at a time. Can be empty (output-only flow). |
| `output_ids` | array of strings | No | `[]` | IDs of outputs from the top-level `outputs` array. Each referenced output must exist and must not already be assigned to another flow. Can be empty (input-only flow). |
| `assembly` | object | No | `null` | Optional PID-bus assembly block. `null` (or `"kind": "passthrough"`) = legacy passthrough. Set `"kind": "spts"` / `"mpts"` to build a fresh TS from elementary streams pulled off any of the flow's inputs. See [Flow Assembly (PID bus)](#flow-assembly-pid-bus--spts--mpts-from-n-inputs). |
| `bandwidth_profile` | string | No | auto | Per-flow broadcast-channel capacity tier. `"standard"` (16 384 slots / ~21 MB) handles TS-contribution up to ~500 Mbps; `"high_bitrate"` (32 768 / ~43 MB) for 500 Mbps – 3 Gbps compressed; `"uncompressed"` (65 536 / ~86 MB) for ST 2110-20/-23 + MXL video. Omit to auto-derive from inputs (ST 2110-20/-23 / MXL video → uncompressed, everything else → standard). Operators override only when auto picks too low a tier for an unusually-high-bitrate compressed source. |

### Multi-Input Flows and Seamless Switching

A flow can reference multiple inputs via `input_ids`. All inputs run simultaneously ("warm passive") — they maintain their connections and stats even while not active. At most one input is active (publishing to the broadcast channel) at a time. Switch the active input via `POST /api/v1/flows/{flow_id}/activate-input` with `{ "input_id": "..." }`.

**TS continuity fixer:** When switching between inputs, bilbycast automatically ensures clean MPEG-TS transitions for downstream receivers:

- **CC state reset** — Output-side continuity counter tracking is cleared on switch. The new input's original CC values pass through, creating a natural CC jump that receivers detect as "packet loss" — they flush PES buffers and resync on the next PES start (PUSI=1).
- **Per-input PSI caching** — Each input maintains its own PAT/PMT cache independently. On switch, the *new* input's cached PSI is injected immediately so receivers can re-acquire the stream structure without waiting for the next natural PAT/PMT cycle.
- **PSI version bump (monotonic counter)** — Injected PAT/PMT packets have their `version_number` rewritten in place (with CRC32 recalculated) from a per-fixer monotonic counter that advances on every switch — not from `cached_version + 1`. This is essential for the common `A → B → A` case: every ffmpeg / srt-live-transmit / camera-SDK-generated stream carries natural `version_number = 0`, so a naive `+1` bumps *every* input's phantom to `version = 1`, and after a round-trip the second phantom looks identical to the first. Receivers (notably ffplay) then treat it as "already seen, don't re-parse" and keep their audio decoder pointed at the wrong input's format. The monotonic counter guarantees consecutive switches always produce a strictly-different version, forcing re-parse every time; it advances even on switches to inputs with no cached PSI, so the next real switch still gets a fresh stamp. Wraps at 32 — consecutive switches remain distinct.
- **Force IDR on ingress re-encoder** — When the target input has `video_encode` (ingress transcoding), the forwarder asks its libx264 / libx265 / NVENC encoder to emit an IDR on the first post-switch frame. This keeps switch-visible latency at one to two frames even when the ingress pipeline re-encodes — without it the receiver would wait up to a full GOP (default 2 s at 30 fps) for the next natural keyframe. Passthrough inputs have no encoder to signal and are unaffected.
- **NULL-PID keepalive during dead-input periods** — When the active input has no packets for 250 ms (typical when the operator has switched to an RTP bind with nothing feeding it, an SRT caller to an unreachable host, etc.), the forwarder emits a single 1316-byte UDP datagram of seven NULL-PID (0x1FFF) TS packets. Receivers are required by spec to drop NULL packets; the keepalive exists purely to keep UDP sockets and decoder state alive across the gap — a 3 s+ silence on the output can otherwise push downstream receivers into EOF / timeout state that real data resumption cannot recover.
- **Immediate forwarding** — All packets (video, audio, data) are forwarded immediately after a switch. **Fully format-agnostic**: inputs can use any codec, container, or transport — H.264, H.265/HEVC, JPEG XS, JPEG 2000, uncompressed video, SMPTE ST 2110-30/-31/-40 (PCM, AES3, ancillary), AAC, HE-AAC, Opus, MP2, AC-3, LPCM, SMPTE 302M, or any future format. Inputs do not need to match each other in codec, resolution, frame rate, sample rate, channel count, or stream structure. For non-TS transports (e.g., raw ST 2110 RTP), the switch mechanism works identically — the fixer is transparent. Receivers see the new feed within one to two frames for both passthrough and ingress-transcoded inputs.

This is fully automatic — no configuration required. The fixer has zero overhead when a flow uses only a single input (it does not activate until the first switch occurs).

**Example: dual-input flow with primary/backup**

```json
{
  "inputs": [
    { "id": "primary-srt", "name": "Primary SRT Feed", "type": "srt", "mode": "listener", "local_addr": "0.0.0.0:9000", "active": true },
    { "id": "backup-srt", "name": "Backup SRT Feed", "type": "srt", "mode": "listener", "local_addr": "0.0.0.0:9001", "active": false }
  ],
  "outputs": [
    { "id": "rtp-out", "name": "RTP Distribution", "type": "rtp", "dest_addr": "239.1.1.1:5000" }
  ],
  "flows": [
    { "id": "main-feed", "name": "Main Program", "input_ids": ["primary-srt", "backup-srt"], "output_ids": ["rtp-out"] }
  ]
}
```

Switch to the backup input:
```bash
curl -X POST http://localhost:8080/api/v1/flows/main-feed/activate-input \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "input_id": "backup-srt" }'
```

### Bandwidth Limit

Optional per-flow bandwidth monitoring for SMPTE RP 2129 trust boundary enforcement. Monitors the flow's input bitrate and takes action when it exceeds the configured limit for the grace period. Works with all input types (RTP, UDP, SRT, RTMP, RTSP, WebRTC).

```json
{
  "bandwidth_limit": {
    "max_bitrate_mbps": 25.0,
    "action": "alarm",
    "grace_period_secs": 5
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `max_bitrate_mbps` | float | Yes | - | Expected maximum bitrate in Mbps. Must be positive and at most 10000 (10 Gbps). |
| `action` | string | Yes | - | `"alarm"`: raise warning event + flag on dashboard. `"block"`: drop all packets until bandwidth normalizes. |
| `grace_period_secs` | integer | No | `5` | Seconds the bitrate must continuously exceed the limit before triggering (1-60). |

**Alarm action:** Emits a warning event and flags the flow on the dashboard. The flow continues operating. An info event is emitted when bitrate returns to normal.

**Block action:** Gates the flow — drops all incoming packets while bandwidth exceeds the limit. The flow stays alive and automatically resumes when bandwidth normalizes via a probe-and-check mechanism. Blocked packets are counted in `packets_filtered`.

> **Note:** This is distinct from `max_bitrate_mbps` on RTP input, which is a hard token-bucket rate limiter that drops excess packets immediately. `bandwidth_limit` monitors aggregate flow bitrate over time with a grace period and configurable response actions.

---

## Per-NIC Interface Binding

Every UDP-based input and output (UDP, RTP, SRT, RIST, ST 2110-20/-23/-30/-31/-40, RTP-audio) — plus the 2022-7 redundancy legs and SRT bonding endpoints — accepts an optional `interface_binding` block that pins the underlying socket to a specific physical NIC.

```json
"interface_binding": { "name": "eno4", "strict": false }
```

Two release tiers, picked by `strict`:

| Tier | What it does | When to use | Cap requirement |
|---|---|---|---|
| **Loose** (`strict: false`, default) | Resolves `name` to the NIC's primary IPv4 and uses it as the socket source IP — same effect as setting `interface_addr` today, but operator-friendly NIC-name input. The kernel still consults the routing table; if the table says the destination should leave a different NIC, it does. | Most operators. Multi-homed hosts where a stable source IP per stream is enough. | None. |
| **Strict** (`strict: true`) | Additionally calls `setsockopt(SO_BINDTODEVICE, name)`. The kernel refuses to send the packet on any other NIC, regardless of routing-table preference. Inputs only deliver packets that arrived on the named NIC. | Genuine link-level isolation. 2022-7 over diverse paths. Multi-tenant NIC isolation. Compliance ("this stream MUST egress on the broadcast LAN"). | `CAP_NET_RAW` — opt in via `packaging/strict-binding.conf`. |

`name` must match an interface on the host (validated against `HealthPayload.network_interfaces`; manager UI populates the picker dropdown from the same list). The Linux `IFNAMSIZ` constraint applies: 1–15 bytes, alphanumeric + `._-`.

Coexists with the legacy `interface_addr` / `bind_addr` / `local_addr` fields. Precedence: `interface_binding` wins when both are set; the legacy fields stay for backward compatibility (Validation logs a Warning event `interface_binding_legacy_addr_ignored` when both are set on the same struct).

**Capability advertisement:**

- `interface-binding` — present from this release on. Signals the new field is honoured.
- `interface-binding-strict` — present only when the startup `SO_BINDTODEVICE` probe succeeds. Edges without `CAP_NET_RAW` reject `strict: true` at bind time with a Critical `interface_binding_strict_denied` event. Manager UI hides the strict toggle until the capability appears.

**Per-leg + per-endpoint:**

```json
{
  "type": "rtp",
  "bind_addr": "239.1.1.1:5000",
  "interface_binding": { "name": "eno3" },           // Red leg → NIC 1
  "redundancy": {
    "bind_addr": "239.2.2.2:5000",
    "interface_binding": { "name": "eno4" }          // Blue leg → NIC 2
  }
}
```

Same shape on `RtpOutputConfig.redundancy`, `SrtRedundancyConfig`, `RistInputRedundancyConfig`, `RistOutputRedundancyConfig`, and per-`SrtBondingEndpoint`. SRT bonding members can pin different NICs:

```json
"bonding": {
  "mode": "broadcast",
  "endpoints": [
    { "addr": "203.0.113.10:9000", "interface_binding": { "name": "eno3" } },
    { "addr": "203.0.113.20:9000", "interface_binding": { "name": "eno4" } }
  ]
}
```

**Phase 1 scope:** UDP-based protocols only. SRT/RIST/SRT-bonding currently support **loose** mode only (the binding's NIC IP is injected as `local_addr` when unset); strict mode on those surfaces is rejected at validation with `srt_strict_binding_unsupported` until `bilbycast-libsrt-rs` and `librist` plumb `SRTO_BINDTODEVICE`. UDP/RTP/ST 2110 outputs and inputs honour both tiers today.

**Granting strict mode:**

```bash
sudo install -m 0644 /opt/bilbycast/edge/current/packaging/strict-binding.conf \
    /etc/systemd/system/bilbycast-edge.service.d/strict-binding.conf
sudo systemctl daemon-reload && sudo systemctl restart bilbycast-edge
```

`CAP_NET_RAW` also enables raw socket creation and packet forging — see [`docs/security.md`](security.md) for the threat-model rationale. Plants that don't need kernel-enforced NIC pinning should leave the default zero-cap unit alone.

**Failure-mode events:**

| `error_code` | Severity | Trigger |
|---|---|---|
| `interface_not_found` | Critical | `name` doesn't match any enumerated NIC at bind time. |
| `interface_binding_strict_denied` | Critical | `setsockopt(SO_BINDTODEVICE)` returned `EPERM` (cap not granted). |
| `interface_binding_legacy_addr_ignored` | Warning | Both `interface_binding` and a legacy `interface_addr` set on the same struct. |
| `srt_strict_binding_unsupported` | Critical | `strict: true` on an SRT/RIST surface (Phase 1 limitation). |

## Input Types

Each entry in the top-level `inputs` array is an `InputDefinition` with `id`, `name`, and the protocol-specific fields flattened in (enum-tagged by `type`). Inputs are independent top-level entities that exist whether or not they are assigned to a flow. They are managed via REST at `/api/v1/inputs` (CRUD) and via manager WebSocket commands.

The `type` discriminator field selects the input variant. The full set is
`rtp`, `udp`, `srt`, `rist`, `rtmp`, `rtsp`, `webrtc`, `whep`, `bonded`,
`test_pattern`, `media_player`, `replay`, `rtp_audio`, `st2110_20`,
`st2110_23`, `st2110_30`, `st2110_31`, `st2110_40`, `sdi`, `mosaic`, and
`mxl_video` / `mxl_audio` / `mxl_anc`.

Almost all of them **parse on every build**, including `sdi` and the three
`mxl_*` types, so a config round-trips unchanged on a binary that cannot run
it — the refusal comes at input start, with an event naming the missing
feature. `mosaic` is the one exception: it is compiled out of the schema
entirely without the `multiviewer` Cargo feature, so such a build rejects the
type at parse time. See each type's own section for its build requirements.

### RTP Input

Receives RTP-wrapped MPEG-TS packets (SMPTE ST 2022-2). Requires valid RTP v2 headers. Supports unicast, multicast, IPv4, and IPv6. For raw TS without RTP headers, use the UDP input type.

```json
{
  "type": "rtp",
  "bind_addr": "239.1.1.1:5000",
  "interface_addr": "192.168.1.100",
  "source_addr": "10.0.0.5",
  "fec_decode": {
    "columns": 10,
    "rows": 10
  },
  "allowed_sources": ["10.0.0.1"],
  "allowed_payload_types": [33],
  "max_bitrate_mbps": 100.0,
  "tr07_mode": true
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"rtp"`. |
| `bind_addr` | string | Yes | - | Local socket address to bind (`ip:port`). For multicast, use the group address (e.g., `"239.1.1.1:5000"`). For unicast, use `"0.0.0.0:5000"`. IPv6: `"[::]:5000"` or `"[ff7e::1]:5000"`. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast group join. Required for multicast on multi-homed hosts. Must be the same address family as `bind_addr`. |
| `source_addr` | string | No | `null` | Source-specific multicast (SSM, RFC 3678) source address. When set, the kernel uses an `(S,G)` join instead of an `(*,G)` join — only packets from this exact source reach the socket. See [SSM vs ASM](#source-specific-multicast-ssm-vs-any-source-multicast-asm) below. |
| `fec_decode` | object | No | `null` | SMPTE 2022-1 FEC decode parameters. See [FEC Configuration](#smpte-2022-1-fec-configuration). |
| `tr07_mode` | boolean | No | `null` | Enable VSF TR-07 mode to detect and report JPEG XS streams in the transport stream. |
| `allowed_sources` | array of strings | No | `null` | Source IP allow-list (RP 2129 C5). Only RTP packets from these source IPs are accepted. Each entry must be a valid IP address. When `null`, all sources are allowed. |
| `allowed_payload_types` | array of integers | No | `null` | RTP payload type allow-list (RP 2129 U4). Only packets with these PT values (0-127) are accepted. When `null`, all payload types are allowed. |
| `max_bitrate_mbps` | float | No | `null` | Maximum ingress bitrate in megabits per second (RP 2129 C7). Excess packets are dropped. Must be positive. When `null`, no rate limiting is applied. |
| `ingress_dejitter_ms` | integer | No | node `tuning.ingress_dejitter_ms`, else `60` | Ingress **de-jitter** buffer setpoint, in ms of content. Packets are buffered and released paced at the recovered source rate (a leaky bucket trimmed ±5 % by the buffer-fill error, with a hard residence-cap shed), so every downstream consumer sees a smooth cadence regardless of network packet-delay variation. Range 20–2000. On a SMPTE 2022-7 dual-leg input it runs *after* the hitless merger, re-pacing the merger's bursty seq-ordered drain. Supersedes `ingress_delay_ms`, which is a pure delay line and *preserves* jitter. |
| `ingress_residence_ms` | integer | No | node `tuning.ingress_residence_ms`, else `max(4 × setpoint, 250)` ms | Hard-shed residence cap for this input's de-jitter buffer. A packet older than this is shed rather than released late, which is what bounds ingress latency when a burst or a source-rate offset exceeds the servo's ±5 % authority. Range `ingress_dejitter_ms + 40` .. `5000`. **Refused without `ingress_dejitter_ms` on the same input** — see the validation rules below. |
| `passthrough_clock` | boolean | No | `false` | Opt **out** of muxer-mode PCR + PES PTS/DTS regeneration. The default (`false`) regenerates PCR and PES PTS/DTS against the flow's master clock — the industry-standard remux model (Sencore RMX, Cobalt 9970-MX, Cisco D9036 mux mode). `true` emits the source's PCR/PTS bytes unchanged: relay / transparent-forwarder behaviour, which also inherits the source's clock jitter and discontinuities at the receiver. Carried by every TS-bearing input type — `rtp`, `udp`, `srt`, `rist`, `rtmp`, `rtsp`, `media_player` and `replay`. **Required (`true`, or a `bonded` input) on every input of a flow using [epoch-locked egress](#epoch-locked-egress-cross-node-alignment)**, so alignment and PCR/PTS regeneration are mutually exclusive. Full rationale in [`clocking.md`](clocking.md). |

**Validation rules:**
- `bind_addr` must be a valid `ip:port` socket address.
- `interface_addr` must be a valid IP address (no port) in the same address family as `bind_addr`.
- `source_addr` is only valid when `bind_addr` is multicast, must be a unicast IP, and must share the address family of `bind_addr`.
- `allowed_payload_types` values must be 0-127.
- `max_bitrate_mbps` must be positive.
- `ingress_dejitter_ms` must be 20–2000 ms.
- `ingress_residence_ms` is **rejected unless the same input also sets `ingress_dejitter_ms`** — it caps how long a packet may sit in the de-jitter buffer, and without a buffer there is nothing to cap, so accepting it would be a silent no-op.
- `ingress_residence_ms` must be within `ingress_dejitter_ms + 40` .. `5000` ms. The floor is the setpoint plus 40 ms because a cap at or below the setpoint would shed the very buffer the setpoint asks the servo to hold.

### UDP Input

Receives raw UDP datagrams without requiring RTP headers. Suitable for raw MPEG-TS over UDP from OBS, ffmpeg (`-f mpegts udp://`), srt-live-transmit, VLC, or any source that sends plain TS.

```json
{
  "type": "udp",
  "bind_addr": "0.0.0.0:5000",
  "interface_addr": "192.168.1.100",
  "source_addr": "10.0.0.5"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"udp"`. |
| `bind_addr` | string | Yes | - | Local socket address to bind (`ip:port`). For multicast, use the group address. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast group join. Must be the same address family as `bind_addr`. |
| `source_addr` | string | No | `null` | SSM source address — see [RTP Input](#rtp-input) above. |
| `ingress_dejitter_ms` | integer | No | node `tuning.ingress_dejitter_ms`, else `60` | Ingress de-jitter buffer setpoint, in ms of content (20–2000). Same servo as the RTP input — see [RTP Input](#rtp-input) above. |
| `ingress_residence_ms` | integer | No | node `tuning.ingress_residence_ms`, else `max(4 × setpoint, 250)` ms | Hard-shed residence cap for this input's de-jitter buffer. Range `ingress_dejitter_ms + 40` .. `5000`. **Refused without `ingress_dejitter_ms` on the same input.** See [RTP Input](#rtp-input) above. |
| `passthrough_clock` | boolean | No | `false` | Opt out of muxer-mode PCR + PES PTS/DTS regeneration — see [RTP Input](#rtp-input) above. |

**Validation rules:**
- `bind_addr` must be a valid `ip:port` socket address.
- `interface_addr` must be a valid IP address in the same address family as `bind_addr`.
- `source_addr` rules: see [RTP Input](#rtp-input) above.
- `ingress_dejitter_ms` must be 20–2000 ms.
- `ingress_residence_ms` is **rejected unless the same input also sets `ingress_dejitter_ms`** (there is no de-jitter buffer to cap, so it would be a silent no-op), and must be within `ingress_dejitter_ms + 40` .. `5000` ms.

### Source-Specific Multicast (SSM) vs Any-Source Multicast (ASM)

Multicast inputs default to **ASM** (`(*,G)`) — the kernel joins the group and accepts traffic from any source on that group address. This requires PIM-RP (Rendezvous Point) routing infrastructure and offers no per-source filtering.

Setting `source_addr` switches the join to **SSM** (`(S,G)`, RFC 3678). Benefits:

- **Skips PIM-RP** — SSM joins flow directly toward the source via PIM-SSM (no rendezvous tree).
- **Per-source filtering at the kernel** — packets from any other source on the same group are dropped before reaching userspace.
- **Required by many ST 2110 / ST 2059 broadcast plants** — production routers commonly require `(S,G)` joins for guaranteed traffic isolation.

SSM works on any multicast group; the kernel doesn't enforce the IANA SSM ranges (232.0.0.0/8 for IPv4, ff3x::/32 for IPv6). The IPv6 SSM join uses `MCAST_JOIN_SOURCE_GROUP` (Linux + macOS only — other targets fail with a clear error).

For SMPTE 2022-7 dual-leg inputs, each leg has its own `source_addr` field — real Red/Blue plants typically have different source IPs per network. Set `source_addr` on the parent input for the primary (Red) leg, and `redundancy.source_addr` for the secondary (Blue) leg.

### RIST Input

Receives a RIST Simple Profile stream (VSF TR-06-1:2020) — reliable RTP with
NACK-driven retransmission, wire-verified against librist 0.2.11. Always
compiled in; there is no feature flag and no C dependency.

```json
{
  "type": "rist",
  "id": "rist-in",
  "name": "Contribution (RIST)",
  "bind_addr": "0.0.0.0:6000",
  "buffer_ms": 1000,
  "max_nack_retries": 10,
  "rtcp_interval_ms": 100,
  "cname": "studio-a",
  "redundancy": { "bind_addr": "0.0.0.0:6002" }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"rist"`. |
| `bind_addr` | string | Yes | - | Local `ip:port` to bind. **The port must be even** — RIST binds RTCP on port + 1, so an odd port is rejected at save time. |
| `external_address` | string | No | `null` | Public `host:port` this receiver is reachable on from outside (a firewall port-forward). Hint to the manager UI only — the edge binds `bind_addr` and does not consume this field. Port must be even. |
| `buffer_ms` | integer | No | `1000` | Receiver jitter / retransmit buffer depth. Range **50–30000**. This is the whole latency budget the NACK loop has to work in — a WAN path needs several RTTs' worth. |
| `max_nack_retries` | integer | No | `10` | Retransmission attempts per lost packet. Must be **≤ 50**. |
| `cname` | string | No | auto-generated | CNAME emitted in RTCP SDES packets. Max 256 characters. |
| `rtcp_interval_ms` | integer | No | `100` | RTCP emission interval. Range **1–1000**; TR-06-1 requires ≤ 100, which is the default. |
| `redundancy` | object | No | `null` | SMPTE 2022-7 second leg: `{ "bind_addr": "...", "external_address": ..., "interface_binding": ... }`. The parent `bind_addr` is leg 1. Leg 2's `bind_addr` must differ from leg 1's and use the same address family; its port must also be even. |
| `program_number` | integer | No | `null` | Ingress MPTS → SPTS filter. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `pid_map` | object | No | `null` | Mechanical ingress PID remap. Keys and values in `0x0010..=0x1FFE`. See [TS output PID remapping](#ts-output-pid-remapping-pid_map) for the shape. |
| `pid_overrides` | object | No | `null` | Per-program PID pinning for the transcoded elementary streams. |
| `audio_encode` / `transcode` / `video_encode` | object | No | `null` | Optional ingress re-encode. Same blocks as the RTP input — see [the `audio_encode` block](#the-audio_encode-block-phase-b) and [`transcoding.md`](transcoding.md). |
| `interface_binding` | object | No | `null` | Pin to a physical NIC. **Loose mode only** on RIST — `strict: true` is rejected with `srt_strict_binding_unsupported` until `librist` plumbs `SO_BINDTODEVICE`. See [Per-NIC Interface Binding](#per-nic-interface-binding). |
| `passthrough_clock` | boolean | No | `false` | Opt out of muxer-mode PCR + PES PTS/DTS regeneration — see [RTP Input](#rtp-input) above. |

**Validation rules:**
- `bind_addr` (and `external_address`, and each redundancy leg) must be a valid `ip:port` **with an even port**. Port `0` is the one value that skips the even-port check, so validation accepts it — but **do not use it**: the RIST channel derives the RTCP port from the *requested* port, so a `bind_addr` of `0.0.0.0:0` binds RTP on an OS-assigned port and then tries to bind RTCP on port **1**, which a non-root edge cannot do. The input fails to start with a `bind_failed` event. Pin a real even port.
- `buffer_ms` 50–30000; `max_nack_retries` ≤ 50; `rtcp_interval_ms` 1–1000; `cname` ≤ 256 chars.
- Redundancy leg 2 must differ from leg 1 and share its address family.

> A bond leg written as `"type": "rist"` inside a `bonded` input's `paths` array
> is a **different** structure with different fields — see
> [`bonding.md`](bonding.md). This section is the standalone RIST input.

### SRT Input

Receives RTP encapsulated in SRT. Supports caller, listener, and rendezvous modes with optional encryption and SMPTE 2022-7 redundancy.

```json
{
  "type": "srt",
  "mode": "listener",
  "local_addr": "0.0.0.0:9000",
  "remote_addr": null,
  "latency_ms": 500,
  "passphrase": "my-encryption-key",
  "aes_key_len": 32,
  "crypto_mode": "aes-gcm",
  "redundancy": {
    "mode": "listener",
    "local_addr": "0.0.0.0:9001",
    "latency_ms": 500,
    "passphrase": "my-encryption-key",
    "aes_key_len": 32,
    "crypto_mode": "aes-gcm"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"srt"`. |
| `mode` | string | Yes | - | SRT connection mode: `"caller"`, `"listener"`, or `"rendezvous"`. See [SRT Connection Modes](#srt-connection-modes). |
| `local_addr` | string | Conditional | `0.0.0.0:0` | Local socket address to bind (`ip:port`). For **listener/rendezvous** this is the **listen address** (required). For **caller** it is the **source socket** (bind-then-connect), *not* the destination — leave unset / `0.0.0.0:0` (ephemeral) unless you must pin a source interface/port. |
| `remote_addr` | string | Conditional | `null` | Remote address to connect to. Required for `caller` and `rendezvous` modes. |
| `latency_ms` | integer | No | `120` | SRT receive latency buffer in milliseconds. Higher values provide more resilience to network jitter at the cost of increased delay. |
| `passphrase` | string | No | `null` | AES encryption passphrase. Must be 10-79 characters. When `null`, encryption is disabled. |
| `aes_key_len` | integer | No | `16` | AES key length in bytes: `16` (AES-128), `24` (AES-192), or `32` (AES-256). Only meaningful if `passphrase` is set. |
| `crypto_mode` | string | No | `null` | Cipher mode: `"aes-ctr"` (default) or `"aes-gcm"` (authenticated encryption). AES-GCM requires libsrt >= 1.5.2 on the peer and only supports AES-128/256 (not AES-192). |
| `redundancy` | object | No | `null` | SMPTE 2022-7 redundancy configuration for a second SRT leg. See [SRT Redundancy](#smpte-2022-7-srt-redundancy). |
| `passthrough_clock` | boolean | No | `false` | Opt out of muxer-mode PCR + PES PTS/DTS regeneration — see [RTP Input](#rtp-input) above. |

Beyond these, an SRT input or output accepts the full libsrt socket-tuning set
— see [SRT advanced socket parameters](#srt-advanced-socket-parameters) below.

**Validation rules:**
- `local_addr` must be a valid socket address.
- `remote_addr` is required for `caller` and `rendezvous` modes and must be a valid socket address.
- In `caller` mode, `local_addr` must **not** equal `remote_addr` — a caller binds `local_addr` as its source socket and connects to `remote_addr`, so identical values make it dial itself and produce no output. A pinned `local_addr` port is also checked against every other bind on the node (inputs, outputs, **co-located tunnels**) and rejected with `port_conflict` on collision.
- `passphrase` must be 10-79 characters.
- `aes_key_len` must be 16, 24, or 32.
- `crypto_mode` must be `"aes-ctr"` or `"aes-gcm"`. AES-GCM with `aes_key_len` 24 is rejected.

#### SRT advanced socket parameters

Every field below is optional and sits on **both** `srt` inputs and `srt`
outputs, with identical names and identical bounds — the validator
(`validate_srt_common`, `src/config/validation.rs`) is one function called from
both sides. Unset means "leave libsrt's own default alone"; the Default column below is libsrt's
value, not something the edge writes. Under [native SRT bonding](#native-libsrt-srt-bonding-socket-groups)
these are parent-level settings and apply to **all** members uniformly.

| Field | Type | Default | Bounds / notes |
|-------|------|---------|----------------|
| `recv_latency_ms` | integer | `latency_ms` | Receiver-side latency override — how long the receiver buffers before delivering. Overrides `latency_ms` for the receive direction only. |
| `peer_latency_ms` | integer | `latency_ms` | Sender-side latency override — the minimum latency the sender asks the receiver to hold. |
| `peer_idle_timeout_secs` | integer | `30` | Drop the connection after this long with no data. 30 s suits broadcast; lower it only if you want faster failover than SRT's own recovery. |
| `stream_id` | string | unset | Max **512** characters (SRT spec). Callers send it in the handshake for identification; a listener that sets it accepts only matching connections. Plain strings and the structured `#!::key=value,…` form both work. |
| `packet_filter` | string | unset | SRT FEC, e.g. `"fec,cols:10,rows:5,layout:staircase,arq:onreq"`. Max 512 chars; `cols` and `rows` each 1–256. Negotiated in the handshake, so **both peers must agree**. **Rejected in `rendezvous` mode** — libsrt 1.5.5 cannot negotiate the filter extension when both sides induct simultaneously, and the handshake silently loops on retry. |
| `max_bw` | integer | unset (libsrt default) | Total send-rate cap in **bytes/sec**. Must be `>= 0` when set; `0` means unlimited. |
| `input_bw` | integer | `0` (auto) | Estimated input rate in bytes/sec, feeding congestion control. `0` auto-detects from the data rate. |
| `overhead_bw` | integer | `25` | Retransmission headroom as a **percentage** over the input rate. Range **5–100**. |
| `max_rexmit_bw` | integer | `-1` | Retransmission bandwidth cap in bytes/sec (token-bucket shaper). `-1` unlimited, `0` disables retransmission entirely, `> 0` caps it. Values below `-1` are rejected. |
| `retransmit_algo` | string | `"default"` | `"default"` or `"reduced"` (libsrt 1.5.5's efficient algorithm). |
| `send_drop_delay` | integer | `-1` | Extra delay in ms before the sender drops a packet. `-1` = off. Must be `>= -1`. |
| `loss_max_ttl` | integer | `0` | Reorder tolerance in packets. `0` = adaptive. Must be `>= 0`. |
| `tlpkt_drop` | boolean | `true` in live mode | Too-late packet drop: discard packets that arrive after their TSBPD deadline. Turn **off** for recording / archival paths where completeness beats timeliness. |
| `flight_flag_size` | integer | `25600` | Flow-control window, in packets. Must be `>= 32`. |
| `send_buffer_size` | integer | `8192` | Send buffer, in packets. Must be `>= 32`. |
| `recv_buffer_size` | integer | `8192` | Receive buffer, in packets. Must be `>= 32`. |
| `payload_size` | integer | `1316` | Bytes of payload per SRT packet. Range **188–1456**. `1316 = 7 × 188` is the MPEG-TS-aligned default. |
| `mss` | integer | `1500` | Maximum Segment Size in bytes, including the SRT header. Range **76–9000**. Lower it for VPN / tunnel paths that fragment; raise it for jumbo frames. |
| `ip_tos` | integer | `0` | `IP_TOS` byte (DSCP × 4 + ECN). Range **0–255**. |
| `ip_ttl` | integer | `64` | IP Time To Live. Range **1–255**. |
| `connect_timeout_secs` | integer | `3` | Caller/rendezvous connect timeout. |
| `enforced_encryption` | boolean | `true` | Reject peers that do not present matching encryption. |
| `km_refresh_rate` | integer | ~16 M | Key-material refresh period, in packets. Must be `> 0`. |
| `km_pre_announce` | integer | `4096` | Packets of advance notice before a key refresh. Must be `> 0`. |
| `external_address` | string | unset | Public `host:port` this listener is reachable on from outside (a firewall port-forward). **Hint to the manager UI only** — the edge binds `local_addr` and ignores this semantically; the manager's topology matcher uses it so cross-NAT links draw correctly. |

> **`packet_filter` + `passphrase` on the pure-Rust SRT backend is rejected.**
> Parity is computed in a different order than libsrt 1.5.5 expects, so C++
> peers fail to recover. The libsrt backend — the one every published binary
> uses — is interop-safe and unaffected.

### RTMP Input

Accepts incoming RTMP publish connections from OBS, ffmpeg, Wirecast, etc.

```json
{
  "type": "rtmp",
  "listen_addr": "0.0.0.0:1935",
  "app": "live",
  "stream_key": "my_secret_key"
}
```

### RTSP Input

Pulls H.264 or H.265/HEVC video and AAC audio from RTSP sources (IP cameras, media servers). Uses the `retina` pure-Rust RTSP client with automatic reconnection. Produces MPEG-TS with proper PAT/PMT program tables. Audio-only streams are supported (PAT/PMT are emitted even without video).

```json
{
  "type": "rtsp",
  "rtsp_url": "rtsp://camera.local:554/stream1",
  "username": "admin",
  "password": "secret",
  "transport": "tcp"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"rtsp"`. |
| `rtsp_url` | string | Yes | - | RTSP source URL. Must start with `rtsp://` or `rtsps://`. |
| `username` | string | No | `null` | RTSP authentication username (Digest or Basic). |
| `password` | string | No | `null` | RTSP authentication password. |
| `transport` | string | No | `"tcp"` | `"tcp"` (interleaved, reliable) or `"udp"` (lower latency). |
| `timeout_secs` | integer | No | `10` | Connection timeout in seconds. |
| `reconnect_delay_secs` | integer | No | `5` | Delay between reconnection attempts on failure. |
| `passthrough_clock` | boolean | No | `false` | Opt out of muxer-mode PCR + PES PTS/DTS regeneration — see [RTP Input](#rtp-input) above. |

### WebRTC/WHIP Input

Accepts WebRTC contributions from publishers (OBS, browsers) via the WHIP protocol (RFC 9725). The `webrtc` feature is enabled by default.

```json
{
  "type": "webrtc",
  "bearer_token": "my-auth-token"
}
```

Publishers POST an SDP offer to `/api/v1/flows/{flow_id}/whip` and receive an SDP answer. The Bearer token (if configured) must be included in the `Authorization` header.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"webrtc"`. |
| `bearer_token` | string | No | `null` | Required from WHIP publishers for authentication. |
| `video_only` | boolean | No | `false` | Ignore audio tracks from publisher. |
| `public_ip` | string | No | `null` | Public IP to advertise in ICE candidates (for NAT traversal). |
| `stun_server` | string | No | `null` | STUN server URL for ICE candidate gathering. |

### WHEP Input

Pulls media from an external WHEP server. The edge acts as a WHEP client. The `webrtc` feature is enabled by default.

```json
{
  "type": "whep",
  "whep_url": "https://server.example.com/whep/stream",
  "bearer_token": "optional-token"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"whep"`. |
| `whep_url` | string | Yes | - | WHEP endpoint URL to pull from. |
| `bearer_token` | string | No | `null` | Bearer token for WHEP authentication. |
| `video_only` | boolean | No | `false` | Receive only video (ignore audio). |

### Media Player Input

Replays one or more local files (MPEG-TS, MP4 / MOV, or still images)
as a paced fresh MPEG-TS feed. The synthesized TS publishes onto the flow's
broadcast channel exactly like any other TS-bearing input, so every output
type works unchanged. The marquee use case is a **slate / standby
fallback** on a PID-bus Hitless leg of an Assembled flow — the live primary
takes precedence; if it stalls past the 200 ms hitless threshold, playback
of the local file kicks in transparently.

```json
{
  "type": "media_player",
  "id": "slate-1",
  "name": "Standby slate",
  "sources": [
    { "kind": "ts",    "name": "loop.ts" },
    { "kind": "mp4",   "name": "promo.mp4" },
    { "kind": "image", "name": "slate.png", "fps": 5, "bitrate_kbps": 250, "audio_silence": true }
  ],
  "loop_playback": true,
  "shuffle": false,
  "paced_bitrate_bps": null
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"media_player"`. |
| `sources` | array | Yes | - | 1–256 entries. Each entry is a `MediaPlayerSource` (see below). Files are referenced by name within the edge's media library; upload them via the manager UI before starting the flow. |
| `loop_playback` | boolean | No | `true` | Restart at the head of the playlist when the last source ends. Leave on for fallback duty. |
| `shuffle` | boolean | No | `false` | Randomise source order each time the playlist starts — a fresh permutation is drawn at flow start and again on every loop wrap. |
| `operator_control` | boolean | No | unset | Transport control (the manager's **Next** button) and the transition state machine. Unset resolves to the node default, which is **on**; `false` pins this input to the legacy sequential loop and makes the edge answer `media_player_control_unavailable` for a `Next`. Resolution order: this field → `tuning.media_player_controller` → the deprecated `BILBYCAST_MEDIA_PLAYER_CONTROLLER` → on. |
| `pcr_deadlines` | boolean | No | unset | TS playout pacing. Unset resolves to the node default, which is **on** — deadlines anchored on the asset's own PCR. `false` pins **this** input to the legacy byte-rate estimate, whose error integrates without bound on a variable-bitrate asset. Per-input because the failure it guards against is asset-dependent: a spliced file whose PCR steps mid-asset paces badly while every other asset on the node is fine. Resolution order: this field → `tuning.media_player_pcr_deadlines` → the deprecated `BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES` → on. |
| `paced_bitrate_bps` | integer | No | `null` | TS-only override for the egress pacer when the source has no usable PCR. Range 100 000 – 200 000 000 (100 kbps – 200 Mbps). Leave `null` to pace from PCR (default for any healthy TS asset). |
| `ts_packets_per_datagram` | integer | No | `7` | How many 188-byte MPEG-TS packets the player bundles into each UDP datagram on the flow broadcast channel and the QUIC/UDP tunnel path (both forward each datagram unchanged). Applies to every source kind (`ts` / `mp4` / `image`). `7 × 188 = 1316 B` is the standard / SRT datagram size. Range `[1, 348]` (`348 × 188 = 65 424 B`, the largest that fits one UDP datagram; `0` is rejected). **Lower** it (e.g. `4`–`5`) for constrained / low-MTU internet or cellular paths where a big datagram IP-fragments and drops; **raise** it (`8`+) for jumbo datagrams on a LAN. Independent of any downstream UDP/RTP/SRT output, which re-chunks to its own fixed 1316 B wire size. |
| `program_number`, `pid_map`, `pid_overrides`, `audio_encode`, `transcode`, `video_encode`, `passthrough_clock` | — | No | `null` / `false` | A `media_player` input also carries the standard TS-ingress blocks, with the same semantics as on the [RTP Input](#rtp-input): MPTS filtering, PID remapping and pinning, ingress re-encode, and the muxer-mode PCR/PTS regeneration opt-out. |

**Source variants** (tagged by `kind`):

| Field | Type | Required | Default | Applies to | Description |
|-------|------|----------|---------|------------|-------------|
| `kind` | string | Yes | - | all | `"ts"`, `"mp4"`, or `"image"`. |
| `name` | string | Yes | - | all | Filename within the media library. ASCII alphanumeric plus `._- ` only, 1–255 chars, no leading dot, no path separators. |

> **`mp4` sources must be plain (unfragmented) H.264 + AAC.** Fragmented MP4 (fMP4 — `moof`/`traf` boxes, as produced by `ffmpeg -movflags frag_keyframe+empty_moov`, browser MediaRecorder / MSE, and DASH/HLS/CMAF packagers) is **rejected** with a Critical `media_player_source_unsupported` event, because the pure-Rust demuxer cannot address samples inside movie-fragment boxes and would otherwise emit an undecodable stream. Re-mux to a plain MP4 first — `ffmpeg -i in.mp4 -c copy -movflags +faststart out.mp4` — or transcode to MPEG-TS and use a `"ts"` source (the lowest-CPU path). HEVC-in-MP4 is likewise out of scope; transcode to `.ts`.
| `fps` | integer | No | `5` | `image` | Frames per second to render. Range 1–60. A still needs no more than a few fps — the picture never changes, so `fps` only sets how often an identical frame is re-encoded (and, with `gop_size = fps`, how often an IDR lands for fast receiver join). Raising it costs CPU and bitrate for no visible benefit. |
| `bitrate_kbps` | integer | No | `250` | `image` | Encoded video bitrate. Range 50–50 000 (50 kbps – 50 Mbps). A static picture compresses to far less than this in practice; the value is an upper bound, not a target the encoder pads to. |
| `duration_secs` | integer | No | `null` | `image` | How long the still stays on air before the playlist advances. Range 1–86 400 (1 s – 24 h). **Omit it (`null`) and the still plays forever** until an operator Next, a transition, or a flow stop — the intended behaviour for a fallback/emergency slate. Set it for a timed playlist item. |
| `audio_silence` | boolean | No | `true` | `image` | Pair the rendered video with silent stereo AAC so downstream demuxers don't complain about a missing audio PID. |

> **Still images are encoded, not looped bytes.** The `image` kind decodes
> the file once to YUV420p and runs a real H.264 encoder at `fps`, so it
> needs one of the `video-encoder-*` Cargo features (a default AGPL-only
> build has no software encoder and the source fails to start). The encoder
> is fed **90 kHz PTS** and declares that timebase via `set_pts_90k()` — see
> [the 90 kHz PTS contract](sdi.md#the-90-khz-pts-contract-load-bearing);
> omitting the declaration makes rate control over-allocate by orders of
> magnitude rather than fail loudly.

**Media library directory** — the on-disk location where uploaded files
are stored on the edge. Resolution order:

1. `BILBYCAST_MEDIA_DIR` env var (recommended for production — pin a specific directory).
2. `$XDG_DATA_HOME/bilbycast/media/`.
3. `$HOME/.bilbycast/media/`.
4. `./media/` (cwd fallback).

> **Packaged installs** pin `BILBYCAST_MEDIA_DIR` (and `BILBYCAST_REPLAY_DIR`)
> to `<data-root>/{media,replay}` (default `/var/lib/bilbycast/edge/…`) in
> `/etc/bilbycast/edge.env` and create those dirs owned by the `bilbycast`
> service account. This is deliberate: the `bilbycast` system user typically
> has no writable home, so the `$HOME`-based fallbacks (steps 2–3) resolve to
> a path the service can't create — every upload then fails with
> `Permission denied (os error 13)`. If you create the service user by hand
> (outside `install-edge.sh`), set `BILBYCAST_MEDIA_DIR` yourself.

**Media-player rollback levers.** Three behaviours are **on by default** and
can be turned off so one node can be reverted without rolling back a release.
These are rollback levers, not feature switches — turning them *on* does
nothing, because on is already the default. Two are now **config fields**;
the third was withdrawn.

| Setting | Scope | Effect when set to `false` |
|---------|-------|----------------------------|
| `operator_control` | per media-player input | Pins **this** input to the legacy sequential playout loop. The edge answers `media_player_control_unavailable` for a `Next` issued against it. |
| `tuning.media_player_controller` | node-wide | Same, for every media-player input that sets no `operator_control` of its own — **and** the edge stops advertising the `media-player-control-v1` capability. Because the manager gates its playlist **Next** button on that capability, the button disappears node-wide while the edge looks otherwise healthy. That is the honest behaviour (the button would refuse anyway), but it is worth knowing before you flip it. |
| `pcr_deadlines` | per media-player input | Paces **this** input's TS playout from the legacy byte-rate estimate instead of deadlines anchored on the asset's own PCR. Use where one asset paces badly — a spliced file whose PCR steps mid-asset — without moving the rest of the node. |
| `tuning.media_player_pcr_deadlines` | node-wide | Same, for every media-player input that sets no `pcr_deadlines` of its own. Use where the *host* is the problem — a stalling disk, a clock step. |

Resolution order for both pairs: **per-input field → `tuning` → the deprecated
environment variable → on.** `BILBYCAST_MEDIA_PLAYER_CONTROLLER` and
`BILBYCAST_MEDIA_PLAYER_PCR_DEADLINES` are still read for one release, *below*
the config field, and a node that sets either raises a Warning
`deprecated_env_var` event naming the replacement. Set the config field
instead: Manager → node → Configure → **Tuning → Media Player**, gated on the
`media_player_tuning` capability.

Unlike the environment variables they replace, these are re-applied on the
config push and read when an input next starts, so **restarting the flow**
applies them — no node restart, and no `systemd-run` / `nohup` relaunch to get
wrong.

> **`BILBYCAST_MEDIA_PLAYER_INCREMENTAL_MP4` has been removed** and has no
> config field, deliberately. It selected the whole-file MP4/MOV demux, which
> holds an entire asset in memory — a 4 GiB asset is a 4 GiB resident spike,
> and that was the principal driver of the media-player OOM the bounded reader
> was written to fix. A control whose "off" position is a known out-of-memory
> does not belong on an operator's screen, so it was withdrawn rather than
> migrated with its two siblings. It survives in debug builds only, for
> diagnosing a suspected incremental-reader defect; a release binary ignores it
> and reports it as removed at startup.

Files are written `0644`. Per-asset cap: **4 GiB** (`MAX_FILE_BYTES`).
Library cap: **16 GiB** total (`MAX_TOTAL_BYTES`). Partial uploads stage
under `<media_dir>/.tmp/<name>.<session_id>` and are reaped after 1 hour
of inactivity.

**Asset manifests (content-based kind detection).** After each upload the
edge probes the new file and writes a small JSON *manifest* describing what
it actually is — the source **kind is detected from the bytes, not the
filename extension**. A `.mov` that is really an MP4 resolves to kind `mp4`;
a file that matches none of the three supported kinds (MP4/MOV, TS File,
Still Image) is reported as `unsupported` rather than mis-played. The manifest
also carries container facts (format, duration, TS packet stride), per-stream
codec / resolution / frame-rate / audio layout, and a `compatibility` block
the manager renders as a kind badge and (from Phase 2) the playlist planner
consumes.

Manifests are cached as sidecar files under `<media_dir>/.manifests/<name>.json`.
That directory is a dotfile, so it never appears in the operator library
listing and never counts against the 16 GiB quota. A sidecar is invalidated
and the file re-probed automatically whenever the file's size or mtime
changes (the edge's atomic-rename upload always changes at least one), or when
the manifest schema version moves. Probing is bounded, runs off the real-time
path, and parses the MP4 `moov` header or image header only — it never decodes
video and never blocks the upload's completion ACK.

WS commands (manager → edge): `inspect_media { name }` returns the cached-or-
freshly-probed manifest (`{ "manifest": … }`, or `manifest: null` when the file
is absent); `reprobe_media { name }` forces a fresh probe, replacing the
sidecar. Probe outcomes carry stable `error_code`s: `media_asset_kind_unsupported`
(matched no supported kind), `media_asset_mp4_unusable` (recognised MP4 but
fragmented or no H.264/AAC track), `media_asset_probe_io` (unreadable).

**Uploading files** — the manager UI's input modal exposes a *Manage Files
(this node)* panel that hosts the chosen file, splits it into 1 MiB
chunks, and POSTs each chunk to
`POST /api/v1/nodes/{id}/media/upload`. The manager forwards each chunk
to the edge as the `upload_media_chunk` WebSocket command; the edge
streams chunks into a staging file and atomically renames on the final
chunk (which also `fsync`s for durability). The manager allows up to 60 s
per chunk ACK (longer than the default 10 s command budget) so that the
final-chunk fsync doesn't trip on slow disks. List and delete are also
exposed as `GET /api/v1/nodes/{id}/media` and
`DELETE /api/v1/nodes/{id}/media/{name}` (idempotent — `{deleted: false}`
when the file was already absent).

**Behaviour when a referenced file is missing** — the engine emits a
Critical `flow`-category event with the open error, sleeps 2 seconds,
and advances to the next source. There is no automatic in-input
fallback; pair the media player with a live primary on a PID-bus
Hitless leg if you need automatic cutover.

### TestPattern Input

Generates a synthetic colour-bars-and-tone test pattern as an
MPEG-TS stream with H.264 video and AAC audio. Useful for
end-to-end pipeline tests, smoke-testing newly-deployed flows, and
exercising downstream gear without a real source.

```json
{
  "type": "test_pattern",
  "id": "in-test",
  "name": "Test pattern",
  "width": 1920,
  "height": 1080,
  "fps": 50,
  "video_bitrate_kbps": 8000,
  "audio_enabled": true,
  "tone_hz": 1000.0,
  "tone_dbfs": -20.0,
  "av_sync_marker": false
}
```

**The JSON above overrides the defaults.** The built-in defaults are
deliberately cheap — **720p25 at 2 Mbps** — so a smoke test costs almost
nothing on a node that is also carrying real feeds. Set `width` / `height` /
`fps` / `video_bitrate_kbps` explicitly whenever the resolution of the test
matters: the values shown above give 1080p50 at 8 Mbps, which is what you want
when the pattern is standing in for a 3G-SDI contribution feed. `3840` × `2160`
works too for UHD.

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `type` | string | — | Always `"test_pattern"`. |
| `width` | u16 | `1280` | Video width in pixels. Must be an even number in 64–7680. |
| `height` | u16 | `720` | Video height in pixels. Must be an even number in 64–4320. |
| `fps` | u16 | `25` | Frame rate. Range `[1, 60]`. |
| `video_bitrate_kbps` | u32 | `2000` | Target video bitrate. |
| `audio_enabled` | bool | `true` | When `false`, emits a video-only TS. |
| `audio_channels` | u8 | `2` | Channels to synthesise: `1` (mono), `2` (stereo), `6` (5.1), `8` (7.1). AAC-native configurations only — `7` has no AAC channel mode and is rejected at save time rather than failing at encoder open. Ignored when `audio_enabled = false`. Not to be confused with the SDI input's `audio_channels`, whose legal set is 0/2/8/16. |
| `audio_content` | string | `"tone"` | What each channel carries. `"tone"` — the same sine on every channel (classic line-up). `"channel_ident"` — each channel announces its own 1-based number so channels can be told apart by ear: a spoken digit where a voice clip is present, otherwise N counted beeps per cycle. See [`testgen-voice.md`](testgen-voice.md), which also covers the companion `channel_ident_layout` (`"sequential"`, the default, one channel per second so the numbers survive a downmix; `"simultaneous"`, every channel at once, better when soloing). |
| `tone_hz` | f32 | `1000.0` | Audio tone frequency. Range `[50, 8000]`. |
| `tone_dbfs` | f32 | `-20.0` | Audio level in dBFS. Range `[-60, 0]`. `-20 dBFS` is the broadcast reference level. |
| `screen_id` | string | unset | Identifier burned in large near the top of the frame so several generators are told apart on a multiviewer. Rendered uppercase; characters outside `A–Z`, `0–9`, space, `-`, `.` and `:` are dropped. Max 32 characters. Unset means no label — the timecode and bouncing box still prove liveness. |
| `av_sync_marker` | bool | `false` | A/V-sync test mode (EBU R 49 / SMPTE 2-pop style). When `true`, the tone gates into a ~80 ms burst on the timecode second boundary and a luma flash patch appears next to the timecode on the same frames. Offset between audible pip and visible flash reads off directly as A/V skew. Requires `audio_enabled = true` — a silent flash has no audio reference to align against, and the combination is rejected. Overrides `audio_content` while it is on. |
| `av_sync_style` | string | `"flash"` | Visual style of that marker. `"flash"` — corner luma patch flashing on the beep (EBU R 49 / 2-pop). `"sweep"` — a dot orbits a ring once per second and the beep fires as it crosses 12 o'clock, so skew is read off the dot's position when you hear the pip. |
| `ts_packets_per_datagram` | u16 | `7` | How many 188-byte MPEG-TS packets the generator bundles into each UDP datagram on the flow broadcast channel and the QUIC/UDP tunnel path (both forward each datagram unchanged). `7 × 188 = 1316 B` is the standard / SRT datagram size. Range `[1, 348]` (`348 × 188 = 65 424 B`, the largest that fits one UDP datagram; `0` is rejected). **Lower** it (e.g. `4`–`5`) for constrained / low-MTU internet or cellular paths where a big datagram IP-fragments and drops; **raise** it (`8`+) to test jumbo datagrams on a LAN. Independent of any downstream UDP/RTP/SRT output, which re-chunks to its own fixed 1316 B wire size. |

Requires the edge build to include the `media-codecs` and
`fdk-aac` features (both on by default). Software-encoded — counts
against the resource budget like any other transcoding flow.

### Bonded Input

Receives a media flow over the bilbycast multi-path bonding stack
— the Rust replacement for appliances like Peplink/SpeedFusion in
broadcast contribution paths. Multiple network paths are aggregated
for throughput and failover; the bonded receiver reorders into a
single ordered TS stream.

```json
{
  "type": "bonded",
  "id": "in-bonded",
  "name": "Bonded receive",
  "local_addr": "0.0.0.0:5500",
  "psk": "<32-byte hex>"
}
```

The full Bonded protocol — path adapters, link selection, latency
budget, FEC — is covered in [`bilbycast-bonding/CLAUDE.md`](../../bilbycast-bonding/CLAUDE.md)
and [`docs/bonding.md`](bonding.md). The bonded sender at the
other end uses the matching [Bonded Output](#bonded-output).

### Mosaic Input (multiviewer wall, `multiviewer` feature)

Composites N **node-local** inputs into one canvas and publishes the result as a
fresh MPEG-TS feed, so a multiviewer wall is an ordinary flow source: it
restreams over SRT/RTP/UDP/WebRTC/CMAF, records, nests inside another wall, and
produces thumbnails, with no new output code. It is the first input type in the
tree that consumes other inputs.

**A wall carries pictures only.** The composited TS has a video PID and nothing
else — the PMT declares no audio, deliberately, because an announced audio PID
that never carries a packet makes receivers wait for audio that is not coming
and makes every downstream A/V check report a fault that is not one. Tile
ingest drops non-video access units for the same reason. Embedded audio and
rasterised audio metering are phase 2.

```json
{
  "id": "wall-1",
  "name": "Gallery wall",
  "type": "mosaic",
  "width": 1920,
  "height": 1080,
  "fps": 25,
  "video_bitrate_kbps": 8000,
  "codec": "h264_auto",
  "tiles": [
    { "id": "t1", "source_input_id": "cam-1", "x": 0,   "y": 0,   "width": 960, "height": 540, "label": "CAM 1" },
    { "id": "t2", "source_input_id": "cam-2", "x": 960, "y": 0,   "width": 960, "height": 540, "label": "CAM 2" },
    { "id": "t3", "source_input_id": "cam-3", "x": 0,   "y": 540, "width": 960, "height": 540, "label": "CAM 3" },
    { "id": "t4", "source_input_id": null,    "x": 960, "y": 540, "width": 960, "height": 540, "label": "SPARE" }
  ]
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"mosaic"`. |
| `width` / `height` | u32 | No | `1920` / `1080` | Canvas size. Both must be **even** (every 4:2:0 encoder needs it) and **capped at 1920×1080** — the phase-1 ceiling, mirroring the display output's CPU-blit limit. Nobody has measured the stream-head shape at UHD, so raising it is gated on that measurement rather than on an argument. |
| `fps` | u16 | No | `25` | The **canvas's own** cadence, deliberately independent of any source's rate: the compositor samples each tile's newest frame at every tick, so a slower tile repeats and a faster one is decimated. Neither is an error and neither may stall the canvas. Range 1–60. |
| `video_bitrate_kbps` | u32 | No | `8000` | Target bitrate for the composited stream. Range 100–200000. |
| `codec` | string | No | `"h264_auto"` | Accepts the same names as an output's `video_encode.codec`. **Not honoured today** — the compositor opens the first backend `select_video_backend()` resolves, and the configured name appears only in the refusal message when no encoder is compiled in. Max 64 characters. |
| `tiles` | array | Yes | - | 1–64 tiles. The ceiling is not tidiness: each tile is an independent decode + scale task with its own buffer, so cost is linear in tile count and paid on a node that is also carrying the feeds being watched. |
| `tiles[].id` | string | **Yes** | - | Stable identity, 1–64 characters. Routing keys on it, so renaming a tile cannot silently re-point a signal. |
| `tiles[].source_input_id` | string / null | No | `null` | The node-local input id feeding this tile. `null` renders the tile as `UNASSIGNED` rather than leaving a hole. 1–64 characters when set. |
| `tiles[].x` / `.y` / `.width` / `.height` | u32 | **Yes** | - | Tile rectangle in canvas pixels. Zero width or height is rejected, and the rectangle must fit inside the canvas. No even alignment is required — the canvas is packed BGRA8, which has no chroma sub-sampling. |
| `tiles[].z` | i32 | No | `0` | Paint order; higher is drawn later, therefore on top. Overlap is legal and reported, never refused — it is how a picture-in-picture is built. |
| `tiles[].label` | string | No | `""` | Operator-facing label burned into the tile. Max 64 characters; empty means no label. |

> **`tiles[].id`, `x`, `y`, `width` and `height` have no serde default.** Omit
> one and the *whole config file* fails to deserialise, before mosaic
> validation ever runs — the error will name the JSON path, not the tile.

**Build requirements, and they are two.** The `multiviewer` Cargo feature is
**off by default**, and it is not sufficient on its own: the flow bus carries
MPEG-TS, so a composite reaches an output only by being encoded and muxed, and
a default build has no video encoder at all. A binary with `multiviewer` but no
`video-encoder-*` compiles and then refuses at flow start with a message naming
the rebuild.

```bash
cargo build --release --features "multiviewer,video-encoder-x264"    # GPL v2+
cargo build --release --features "multiviewer,video-encoder-nvenc"   # LGPL-clean
```

**All three published release artefacts** (`*-x86_64-linux-full`,
`*-aarch64-linux-full`, `*-aarch64-linux-rockchip`) are built with
`multiviewer` alongside a full encoder bundle, and the release workflow asserts
both halves on every artefact, so a node running any published binary always
advertises the `mv-compositor` capability. That bit is a property of the
binary, not of the host — it is gated on an encoder resolving at **build**
time, not on any runtime probe.

Full reference — telemetry counters, the `mosaic_*` events, tile liveness
badges, and why the canvas is BGRA rather than YUV: [`multiviewer.md`](multiviewer.md).

### SDI Input (Blackmagic DeckLink)

Captures SDI directly off a Blackmagic **DeckLink** card — video plus
embedded audio — encodes in-process and publishes a standard A+V
MPEG-TS flow, with no external SDI→IP converter in the path. Gated on
the `sdi-decklink` Cargo feature (default **off**); the schema is
always present so configs round-trip on builds without it.

```json
{
  "type": "sdi",
  "id": "sdi1",
  "name": "SDI 1",
  "device": "DeckLink Quad (1)",
  "format": "auto",
  "pixel_format": "uyvy422",
  "audio_channels": 2,
  "video_encode": {
    "codec": "h264_nvenc", "chroma": "yuv420p",
    "tune": "", "preset": "fast", "rate_control": "cbr",
    "bitrate_kbps": 10000, "gop_size": 50
  }
}
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `type` | string | — | Always `"sdi"`. |
| `device` | string | — | **Required**, non-empty. DeckLink display name as listed by the boot probe and [`HealthPayload.sdi_devices[]`](metrics.md#sdi-telemetry-sdi_stats--sdi_devices), e.g. `"DeckLink Quad (1)"`. |
| `format` | string | `"auto"` | `"auto"` (card input-format detection) or a DeckLink mode FourCC (`"Hi50"`, `"Hp25"`, …). **Use `auto`.** A forced mode that mismatches the source makes the card report no signal and emit bars — a self-inflicted outage that looks like a cable fault. |
| `pixel_format` | string | `"uyvy422"` | `"uyvy422"` (8-bit) is the only implemented format. `"v210"` parses but is **rejected at config load** — there is no 10-bit unpacker. |
| `audio_channels` | u8 | `2` | Embedded-audio channels to capture. Must be `0`, `2`, `8` or `16`; `0` = video-only. 8/16 are implemented but not hardware-verified. |
| `video_encode` | object | — | **Mandatory** (as for MXL video / ST 2110-20). Standard [`video_encode`](transcoding.md) schema. `chroma` must be `yuv420p` or `yuv422p` and `bit_depth` must be `8` — a constraint of the *capture format* (8-bit 4:2:2), not of any encoder: every backend is reachable, including `h264_auto` / `hevc_auto`. |
| `audio_encode` | object | *(AAC-LC)* | Optional. **AAC family only** — `aac_lc` / `he_aac_v1` / `he_aac_v2`. Unset gives an AAC-LC default so the flow carries sound. Any other codec warns and the flow continues **video-only** rather than dying. |
| `pid_overrides` | map | — | Accepted and validated, but **not applied on the SDI path today** — `sdi_io` runs no ingress post-process stage. Set PIDs on the output instead (`pid_map`). |

Validation rejects at config load — not mid-show — `v210`, `bit_depth: 10`,
`chroma: yuv444p`, an empty `device`, and channel counts outside
{0, 2, 8, 16}.

**Clocking.** SDI needs **no PTP** and the flow's master clock is never
consulted on this path: every timestamp comes from the card. The flow
resolves to the `wallclock` master-clock kind. Note that two SDI ports
are *not* co-clocked unless the card is genlocked — they cannot be
assembled into one output program. See
[`sdi.md`](sdi.md#clocking-the-card-is-the-clock).

**Signal loss does not stop the stream** — the card keeps delivering
frames (bars/black) with the cable out, and the edge keeps encoding
them deliberately, because holding the transport stream up is what
downstream wants. This means bitrate and `state` read healthy on a dead
feed; `InputStats.sdi_stats.signal_present` is the only honest
indicator. That distinction is the entire reason this path talks to the
Blackmagic SDK rather than FFmpeg's `decklink` avdevice.

Build prereqs, the per-port health payload, event catalogue, hardware
gotchas (connector↔device interleaving on Quad cards) and verification
recipes: [`docs/sdi.md`](sdi.md).

### MXL Inputs (`mxl_video` / `mxl_audio` / `mxl_anc`)

Consume an EBU / Linux Foundation **Media eXchange Layer** (MXL) flow
off the same-host shared-memory bus and republish it onto the flow's
broadcast channel. Three variants mirror the ST 2110-20/-30/-40 pattern.
Gated on the `mxl` Cargo feature (default **off**, heavy build prereqs —
see [`../../bilbycast-mxl-rs/CLAUDE.md`](../../bilbycast-mxl-rs/CLAUDE.md)); the
schema is always present so configs round-trip on builds without it.
**PTP is mandatory** — validation rejects `master_clock: "wallclock"` on
any MXL flow, and the flow resolves to the `ptp` master-clock kind. The
boot probe `dlopen`s `libmxl.so` and only advertises the `mxl-video` /
`mxl-audio` / `mxl-anc` capability bits on success.

> **Implementation status.** The **video** bridge (V210 ↔ H.264/HEVC) is
> implemented in both directions. The **audio** codec bridge is a known
> TODO (see [`mxl-integration-plan.md`](mxl-integration-plan.md)) — the
> config and schema exist, but the essence path is not complete.

```json
{
  "type": "mxl_video",
  "id": "mxl-in1",
  "name": "MXL camera 1",
  "domain_path": "/dev/shm/mxl-domain",
  "flow_name": "cam1-video",
  "width": 1920,
  "height": 1080,
  "frame_rate_num": 30000,
  "frame_rate_den": 1001,
  "clock_domain": 0,
  "video_encode": {
    "codec": "h264_nvenc", "chroma": "yuv422p", "bit_depth": 10,
    "preset": "fast", "rate_control": "cbr", "bitrate_kbps": 20000, "gop_size": 30
  }
}
```

All three variants share the MXL domain reference:

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `type` | string | — | `"mxl_video"`, `"mxl_audio"` or `"mxl_anc"`. |
| `domain_path` | string | — | **Required.** MXL domain directory, typically `/dev/shm/<name>`. Must live on tmpfs/ramfs — otherwise a `mxl_domain_not_tmpfs` Warning fires. |
| `flow_name` | string | — | **Required.** libmxl flow name to consume. |
| `clock_domain` | u8 | *(inherits flow)* | PTP clock domain `0..=127`. |

`mxl_video` adds `width`, `height`, `frame_rate_num`, `frame_rate_den`,
and a **mandatory** [`video_encode`](transcoding.md) block (a
feature-gated backend must be compiled in) — it decodes V210 grains to
planar 4:2:2 10-bit and encodes to MPEG-TS, exactly like ST 2110-20.
`mxl_audio` adds `channels` (1/2/4/8/16, default 2), `packet_time_us`
(default 1000, ST 2110-30 PM-compatible), and optional
`transcode` / `audio_encode` blocks (same shape as ST 2110-30).
`mxl_anc` carries only the domain reference + `clock_domain`.

Architecture rationale + integration plan:
[`mxl-integration-plan.md`](mxl-integration-plan.md); handoff notes:
[`mxl-handoff.md`](mxl-handoff.md).

---

## Output Types

Each entry in the top-level `outputs` array is an `OutputConfig` with `id`, `name`, and protocol-specific fields (enum-tagged by `type`). Outputs are independent top-level entities that exist whether or not they are assigned to a flow. They are managed via REST at `/api/v1/outputs` (CRUD) and via manager WebSocket commands.

All outputs share `id` and `name` fields.

### RTP Output

Sends RTP-wrapped MPEG-TS packets to a unicast or multicast destination. Supports SMPTE 2022-1 FEC encoding.

```json
{
  "type": "rtp",
  "id": "rtp-out-1",
  "name": "Local Playout",
  "dest_addr": "192.168.1.50:5004",
  "bind_addr": "192.168.1.100:0",
  "interface_addr": "192.168.1.100",
  "fec_encode": {
    "columns": 10,
    "rows": 10
  },
  "dscp": 46
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"rtp"`. |
| `id` | string | Yes | - | Unique output ID. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `dest_addr` | string | Yes | - | Destination socket address (`ip:port`). For multicast, use the group address (e.g., `"239.1.2.1:5004"`). IPv6: `"[::1]:5004"`. |
| `bind_addr` | string | No | `"0.0.0.0:0"` | Source bind address. Use to control the source IP/port of outgoing packets. Must be same address family as `dest_addr`. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast send. Must be same address family as `dest_addr`. |
| `fec_encode` | object | No | `null` | SMPTE 2022-1 FEC encode parameters. See [FEC Configuration](#smpte-2022-1-fec-configuration). |
| `dscp` | integer | No | `46` | DSCP value for QoS marking (RP 2129 C10). Range 0-63. Default 46 = Expedited Forwarding (RFC 4594). |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = full MPTS passthrough; `Some(N)` = forward only program N as a rewritten single-program TS. Applied before FEC, so the receiver's FEC protects the filtered SPTS. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `delay` | object | No | `null` | Output delay for stream synchronization. Modes: `{"mode":"fixed","ms":N}` adds constant delay; `{"mode":"target_ms","ms":N}` targets end-to-end latency (self-adjusting); `{"mode":"target_frames","frames":N,"fallback_ms":M}` targets latency in video frames (auto-detected fps). |
| `egress_pacing` | string | No | auto | Egress pacing model for the wire emitter: `"forward"` (emit at input cadence, no re-pacing; lowest latency, recommended for clean upstreams), `"pcr"` (open-loop re-pacing at PCR-implied instants, for SMPTE 2022-7 dual-leg coherence / strict-T-STD receivers / bond-reassembled cadence), `"servo"` (closed-loop release-rate servo for a genuinely bursty unpaced ingress). **Unset = auto**: resolves to `"pcr"` when the flow has a `bonded` input, else `"forward"` (see [Egress pacing auto-resolution](#egress-pacing-auto-resolution)). A bounded residence cap guards against latency runaway in every mode. Manager-configurable; UI gated on the `egress_pacing` capability. |
| `cbr_pad_to_kbps` | integer | No | `null` | Pad the output to a constant wire bitrate (kbps) by injecting PID `0x1FFF` NULL packets between the transcoder pipeline and the wire emitter, so the rate is stable regardless of the encoder's natural VBR output — for downstream multiplexers and legacy receivers that expect CBR. Range **1000–1000000**. When the output declares `audio_encode.bitrate_kbps` and/or `video_encode.bitrate_kbps`, the target must exceed their sum by at least **5 %**: a target at or below the encoder budget would never inject a single NULL, so it is rejected at save time rather than silently doing nothing. **Incompatible with `epoch_lock`** — see [Epoch-locked egress](#epoch-locked-egress-cross-node-alignment). |
| `egress_buffer_ms` | integer | No | `null` | Servo de-jitter cushion (ms of content). **Only valid with `egress_pacing: "servo"`** — rejected otherwise. Seeds and holds ~this much content in the egress queue, absorbing arrival jitter at the cost of that latency. Range 20-2000. `null` = no cushion (servo rate-trims only). |
| `epoch_lock` | object | No | `null` | Epoch-locked egress — release this output's datagrams on a **group-shared timeline** so independent nodes forwarding the same feed emit the same content at the same wall instant. See [Epoch-locked egress](#epoch-locked-egress-cross-node-alignment). |

**Validation rules:**
- `id` cannot be empty.
- `dest_addr`, `bind_addr`, and `interface_addr` must all use the same address family.
- `dscp` must be 0-63.
- `program_number` must be `> 0` if set (program_number 0 is reserved for the NIT).
- `delay`: `fixed` ms 0-10000; `target_ms` ms 1-10000; `target_frames` frames 0.01-300, fallback_ms 0-10000.
- `egress_buffer_ms` requires `egress_pacing: "servo"` and must be 20-2000 ms.
- `epoch_lock` requires an explicit `egress_pacing: "pcr"` and a flow that satisfies every condition in [Epoch-locked egress](#epoch-locked-egress-cross-node-alignment).

#### Egress pacing auto-resolution

An **unset** `egress_pacing` on a UDP or RTP output is *auto*, resolved at
output spawn: it becomes `"pcr"` when the flow's input set contains a
`bonded` input — the bond reassembly buffer releases recovered packets in
hold-time bursts, so forwarding at input cadence would put that burst
structure straight onto the wire — and `"forward"` otherwise (exactly the
pre-auto default, so non-bonded configs behave identically). **Upgrade
note:** an existing bonded flow whose UDP/RTP outputs never set
`egress_pacing` changes from `forward` to `pcr` on the release introducing
auto-resolution — pin `"forward"` explicitly before upgrading to keep the
old wire behaviour. The resolution is logged at `info` when it picks
`"pcr"`, surfaced per output on `OutputStats.egress_pacing_effective`
(`"auto (pcr)"` / `"auto (forward)"` vs a bare explicit mode), and is never
written back to the config; an explicit `"forward"` / `"pcr"` / `"servo"`
always wins, bonded or not.

Because the resolution happens at output spawn, a **hot** input-set edit
(`add_input` / `remove_input` / `update_flow` input delta) that flips the
flow's bonded-input membership does NOT re-pace running outputs — they keep
their spawn-time resolution and the edge emits a Warning
(`egress_pacing_auto_stale`) listing the affected outputs; restart them (or
the flow) to re-resolve.

### UDP Output

Sends raw MPEG-TS over UDP without RTP headers. Datagrams are TS-aligned (7×188 = 1316 bytes). If the input is RTP-wrapped, RTP headers are automatically stripped. Compatible with ffplay, VLC, and standard IP/TS multicast receivers.

```json
{
  "type": "udp",
  "id": "udp-out-1",
  "name": "Local Playout (raw TS)",
  "dest_addr": "192.168.1.50:5004",
  "dscp": 46
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"udp"`. |
| `id` | string | Yes | - | Unique output ID. |
| `name` | string | Yes | - | Human-readable display name. |
| `dest_addr` | string | Yes | - | Destination socket address (`ip:port`). For multicast, use the group address. |
| `bind_addr` | string | No | `"0.0.0.0:0"` | Source bind address. Must be same address family as `dest_addr`. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast send. |
| `dscp` | integer | No | `46` | DSCP value for QoS marking. Range 0-63. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = full MPTS passthrough; `Some(N)` = forward only program N as a rewritten single-program TS. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `delay` | object | No | `null` | Output delay for stream synchronization (same modes as RTP output). Incompatible with `transport_mode: "audio_302m"`. |
| `egress_pacing` | string | No | auto | Egress pacing model for the wire emitter: `"forward"` (emit at input cadence, no re-pacing; lowest latency, recommended for clean upstreams), `"pcr"` (open-loop re-pacing at PCR-implied instants, for SMPTE 2022-7 dual-leg coherence / strict-T-STD receivers / bond-reassembled cadence), `"servo"` (closed-loop release-rate servo for a genuinely bursty unpaced ingress). **Unset = auto**: resolves to `"pcr"` when the flow has a `bonded` input, else `"forward"` (see [Egress pacing auto-resolution](#egress-pacing-auto-resolution)). A bounded residence cap guards against latency runaway in every mode. Manager-configurable; UI gated on the `egress_pacing` capability. |
| `cbr_pad_to_kbps` | integer | No | `null` | Pad the output to a constant wire bitrate (kbps) by injecting PID `0x1FFF` NULL packets between the transcoder pipeline and the wire emitter, so the rate is stable regardless of the encoder's natural VBR output — for downstream multiplexers and legacy receivers that expect CBR. Range **1000–1000000**. When the output declares `audio_encode.bitrate_kbps` and/or `video_encode.bitrate_kbps`, the target must exceed their sum by at least **5 %**: a target at or below the encoder budget would never inject a single NULL, so it is rejected at save time rather than silently doing nothing. **Incompatible with `epoch_lock`** — see [Epoch-locked egress](#epoch-locked-egress-cross-node-alignment). |
| `egress_buffer_ms` | integer | No | `null` | Servo de-jitter cushion (ms of content). **Only valid with `egress_pacing: "servo"`** — rejected otherwise. Seeds and holds ~this much content in the egress queue, absorbing arrival jitter at the cost of that latency. Range 20-2000. `null` = no cushion (servo rate-trims only). |
| `epoch_lock` | object | No | `null` | Epoch-locked egress — release this output's datagrams on a **group-shared timeline** so independent nodes forwarding the same feed emit the same content at the same wall instant. See [Epoch-locked egress](#epoch-locked-egress-cross-node-alignment). |

**Validation rules:**
- `id` cannot be empty.
- `dest_addr` must be a valid socket address.
- `dscp` must be 0-63.
- `program_number` must be `> 0` if set.
- `delay`: same validation as RTP output. Incompatible with `transport_mode: "audio_302m"`.
- `egress_buffer_ms` requires `egress_pacing: "servo"` and must be 20-2000 ms.
- `epoch_lock` requires an explicit `egress_pacing: "pcr"` and a flow that satisfies every condition in [Epoch-locked egress](#epoch-locked-egress-cross-node-alignment).

#### Epoch-locked egress (cross-node alignment)

Two edges fed the same contribution feed normally emit it at whatever instant
each one's own ingest path delivers it — they differ by their path-latency
difference. `epoch_lock` puts both on a **shared timeline** so a downstream
switcher can cut between them without a timing discontinuity. It gives a clean
**cut**, not a seamless SMPTE 2022-7 merge (RTP sequence numbers stay per-node
counters).

Available on **UDP and RTP outputs only**. Advertised as the `epoch_lock`
capability — an edge without it ignores the block silently, which looks exactly
like success, so the manager UI gates the field on the bit.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `egress_offset_ms` | integer | Yes | - | Headroom between the instant the group's shared timeline assigns to a PCR and the instant its datagram is released. Range 150-800. **Must be identical on every member** — a mismatch misaligns the group by exactly the difference while every node reports healthy. |
| `group_label` | string | No | `null` | Operator label surfaced on telemetry so the manager UI can group members visually. Carries **no behaviour**. Max 64 chars. |
| `source_anchor` | object | No | `null` | The group-shared source-PCR → wall-instant anchor: `{ "pcr_27mhz": N, "unix_ns": N, "generation": N, "effective_from_pcr": N \| null }`. **Minted and pushed by the manager** (`set_epoch_anchor`) — not hand-authored. Absent until the group is armed. |
| `pcr_pid` | integer | No | `null` | PCR PID to anchor on, for a source carrying more than one program. Must be `< 0x1FFF`. Required unless the source is single-program: the emitter otherwise latches "the first PCR-bearing PID I ever saw", and two members joining an MPTS at different byte offsets can latch *different programs* — misaligning by seconds while every plausibility check passes. |

**`egress_offset_ms` is a budget for the inter-node latency _spread_, not for
absolute end-to-end latency.** The manager mints the anchor from the slowest
member's arrival, so a member's required dwell is its lead over the slowest plus
this margin — absolute path latency cancels out. Sizing it against absolute
latency instead puts a WAN contribution feed straight into the wire-emit
residence cap.

**Scope — validation rejects everything outside it:**

- Explicit `egress_pacing: "pcr"` on the output (never `auto`, `forward` or `servo`).
- Exactly **one input** on the flow, and the flow is **not** assembled (no PID bus).
- The output is **not** transcoded (no `audio_encode` / `video_encode`) and has an unambiguous PCR PID.
- The output does **not** set `cbr_pad_to_kbps`. The NULL padder seeds its byte budget from *this node's* own clock, so two members' inter-PCR byte streams diverge even when every PCR-bearing datagram lands together — aligned release timing cannot make the two streams interchangeable. Padding is not transcoding and does not touch PCR, which is exactly why this refusal surprises people.
- Every input on the flow is `bonded` **or** sets `passthrough_clock: true`.

That last one is load-bearing. The PCR reaching the wire emitter must be a
function of the **content**, not of this node. In default muxer mode
`ts_pts_rewriter` anchors output PCR to *this node's* wall clock at the first PCR
it saw, so the value carries the node's own ingest instant and inverting it
recovers nothing shared. **Consequence the operator feels: alignment and PCR/PTS
regeneration are mutually exclusive** — turning this on gives up muxer-mode
rewriting on those inputs.

A flow-level violation does not fail the config. The edge **strips `epoch_lock`
from the flow's outputs and keeps running**, logging why; the absent
`epoch_lock` telemetry block is what tells the manager the group never armed.

Per-output telemetry rides `OutputStats.epoch_lock`: `engaged`, `disengaged`,
`egress_offset_us`, `deficit_us` / `deficit_max_us` (released **late** — raise
the offset), `clamped` (released **early** — lower it; a **lifetime** counter
that never resets), `implausible`, `anchor_generation`, `group_label`.

Arming a group is a manager operation — a single node cannot mint its own
anchor. See the manager's [Alignment Groups](../../bilbycast-manager/docs/alignment-groups.md)
reference and the full design rationale in [`clocking.md`](clocking.md#cross-node-egress-alignment-epoch_lock).

### RIST Output

Sends a RIST Simple Profile stream (VSF TR-06-1:2020) to a peer. Binds a local
dual-port UDP channel and transmits reliable RTP to the peer's even RTP port;
the peer's RTCP traffic is learned dynamically, so there is no `P+1` assumption
on the far side. Always compiled in — no feature flag, no C dependency.

```json
{
  "type": "rist",
  "id": "rist-out-1",
  "name": "Remote Site (RIST)",
  "remote_addr": "203.0.113.10:6000",
  "buffer_ms": 1000,
  "retransmit_buffer_capacity": 2048,
  "rtcp_interval_ms": 100
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"rist"`. |
| `id` | string | Yes | - | Unique output ID. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `active` | boolean | No | `true` | Whether this output runs. |
| `group` | string | No | `null` | Free-form group tag, max 64 characters. |
| `remote_addr` | string | Yes | - | Peer `ip:port`. **The port must be even** — RIST pairs RTP on P and RTCP on P+1. |
| `local_addr` | string | No | unset | Source bind for the sender's RTP socket. **Leave it unset unless you need a pinned source port.** Unset does not mean "bind port 0": the sender picks its own random *even* port in the IANA dynamic range 49152–65534, on the address family of `remote_addr` (`0.0.0.0` or `[::]`), retrying up to 32 attempts on collision and failing with `exhausted 32 bind attempts` if every try is taken. When you do set it the port must be even, and it must **not** be `0` — validation lets `0` through (it is the one value exempt from the even-port check), but RIST derives the RTCP port from the *requested* port, so port `0` puts RTCP on privileged port **1** and the bind fails. That hazard is exactly why the unset path picks a port itself. |
| `buffer_ms` | integer | No | `1000` | Sender retransmit buffer depth, in ms. Range **50–30000**. |
| `retransmit_buffer_capacity` | integer | No | `2048` | Retransmit buffer capacity in packets. Range **64–65536**. |
| `cname` | string | No | auto-generated | CNAME emitted in RTCP SDES packets. Max 256 characters. |
| `rtcp_interval_ms` | integer | No | `100` | RTCP emission interval. Range **1–1000**; TR-06-1 requires ≤ 100. |
| `redundancy` | object | No | `null` | SMPTE 2022-7 second leg (`remote_addr`, optional `local_addr`, optional `interface_binding`). Leg 2's `remote_addr` must differ from leg 1's and share its address family. **`audio_encode` is not supported alongside redundancy** and the combination is rejected. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `pid_map` | object | No | `null` | Output PID remap. See [TS output PID remapping](#ts-output-pid-remapping-pid_map). |
| `pid_overrides` | object | No | `null` | Per-program PID pinning for transcoded elementary streams. |
| `delay` | object | No | `null` | Output delay for stream synchronisation. Same modes as the RTP output. |
| `audio_encode` / `transcode` / `video_encode` | object | No | `null` | Optional per-output re-encode. Allowed audio codecs: `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`. See [the `audio_encode` block](#the-audio_encode-block-phase-b) and [`transcoding.md`](transcoding.md). |
| `interface_binding` | object | No | `null` | Pin to a physical NIC. **Loose mode only** on RIST — `strict: true` is rejected with `srt_strict_binding_unsupported`. See [Per-NIC Interface Binding](#per-nic-interface-binding). |

**Validation rules:**
- `remote_addr` and `local_addr` must be valid `ip:port` addresses **with even ports** (port `0` is the sole exemption from that check, and is a broken value on RIST — see the `local_addr` row above).
- `buffer_ms` 50–30000; `retransmit_buffer_capacity` 64–65536; `rtcp_interval_ms` 1–1000; `cname` ≤ 256 chars.
- Redundancy leg 2 must differ from leg 1 and share its address family; `audio_encode` with redundancy is refused.

### SRT Output

Sends RTP encapsulated in SRT.

```json
{
  "type": "srt",
  "id": "srt-out-1",
  "name": "Remote Site",
  "mode": "caller",
  "local_addr": "0.0.0.0:0",
  "remote_addr": "203.0.113.10:9000",
  "latency_ms": 500,
  "passphrase": "encryption-key-here",
  "aes_key_len": 32,
  "redundancy": {
    "mode": "caller",
    "local_addr": "0.0.0.0:0",
    "remote_addr": "203.0.113.11:9000",
    "latency_ms": 500,
    "passphrase": "encryption-key-here",
    "aes_key_len": 32
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"srt"`. |
| `id` | string | Yes | - | Unique output ID. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `mode` | string | Yes | - | SRT connection mode: `"caller"`, `"listener"`, or `"rendezvous"`. |
| `local_addr` | string | Conditional | `0.0.0.0:0` | Local socket address to bind. For **caller** this is the **source socket** (bind-then-connect), not the destination — use `"0.0.0.0:0"` (ephemeral) unless pinning a source interface/port, and **never** set it equal to `remote_addr` or to a co-located egress tunnel's port. Required as the listen address for **listener/rendezvous**. A pinned port is checked against every other bind on the node (including tunnels) and rejected with `port_conflict` on collision. |
| `remote_addr` | string | Conditional | `null` | Remote address. Required for `caller` and `rendezvous`. |
| `latency_ms` | integer | No | `120` | SRT send latency in milliseconds. |
| `passphrase` | string | No | `null` | AES encryption passphrase (10-79 characters). |
| `aes_key_len` | integer | No | `16` | AES key length: 16, 24, or 32. |
| `crypto_mode` | string | No | `null` | Cipher mode: `"aes-ctr"` (default) or `"aes-gcm"`. |
| `redundancy` | object | No | `null` | SMPTE 2022-7 redundancy for a second SRT output leg. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = full MPTS passthrough; `Some(N)` = forward only program N as a rewritten single-program TS. Applied once and mirrored to both legs when 2022-7 is enabled. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `delay_ms` | integer | No | `null` | Output delay in milliseconds (0–10000). When set and > 0, packets are buffered and released after this delay. Used for synchronizing parallel outputs with different processing latencies. Incompatible with `transport_mode: "audio_302m"`. |
| `cbr_pad_to_kbps` | integer | No | `null` | Pad the output to a constant wire bitrate (kbps) by injecting PID `0x1FFF` NULL packets between the transcoder pipeline and the wire emitter, so the rate is stable regardless of the encoder's natural VBR output — for downstream multiplexers and legacy receivers that expect CBR. Range **1000–1000000**. When the output declares `audio_encode.bitrate_kbps` and/or `video_encode.bitrate_kbps`, the target must exceed their sum by at least **5 %**: a target at or below the encoder budget would never inject a single NULL, so it is rejected at save time rather than silently doing nothing. SRT carries the padded TS opaquely, so a receiver measuring wire rate sees the inflated stream. |

### RTMP Output

Publishes to an RTMP/RTMPS server (e.g., Twitch, YouTube Live, Facebook Live). Demuxes H.264 and AAC from MPEG-2 TS and muxes into FLV.

```json
{
  "type": "rtmp",
  "id": "twitch",
  "name": "Twitch Stream",
  "dest_url": "rtmp://live.twitch.tv/app",
  "stream_key": "live_123456789_abcdefghijklmnop",
  "reconnect_delay_secs": 5,
  "max_reconnect_attempts": 10,
  "audio_encode": {
    "codec": "aac_lc",
    "bitrate_kbps": 96
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"rtmp"`. |
| `id` | string | Yes | - | Unique output ID. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `dest_url` | string | Yes | - | RTMP server URL. Must start with `rtmp://` or `rtmps://`. RTMPS requires the `tls` feature (enabled by default). |
| `stream_key` | string | Yes | - | Stream key for authentication with the RTMP server. Cannot be empty. |
| `reconnect_delay_secs` | integer | No | `5` | Seconds to wait before reconnecting after a failure. Must be > 0. |
| `max_reconnect_attempts` | integer | No | `null` (unlimited) | Maximum reconnection attempts. When `null`, reconnects indefinitely. |
| `program_number` | integer | No | `null` | MPTS program selector. `null` = lock onto the lowest program_number in the PAT (deterministic default); `Some(N)` = extract elementary streams from program N only. RTMP is single-program by spec, so this only changes *which* program is published. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `audio_encode` | object | No | `null` | Optional ffmpeg-sidecar audio encoder. Enables PCM → compressed re-encode so the operator can normalise bitrate / sample rate / channel count or upgrade to HE-AAC v1/v2. Allowed `codec`: `aac_lc`, `he_aac_v1`, `he_aac_v2`. Same-codec passthrough fast path applies on AAC-LC source with no field overrides. Requires ffmpeg in PATH. See the [`audio_encode` block](#the-audio_encode-block-phase-b) below and [`audio-gateway.md`](audio-gateway.md#the-audio_encode-block--compressed-audio-egress-rtmp--hls--webrtc). |

**Limitations:**
- Output only. RTMP input is not supported.
- Only H.264 video and AAC audio are supported (no HEVC/VP9).

### HLS Output

Segments MPEG-2 TS data and uploads via HTTP for HLS ingest (e.g., YouTube HLS).

```json
{
  "type": "hls",
  "id": "youtube-hls",
  "name": "YouTube HLS",
  "ingest_url": "https://a.upload.youtube.com/http_upload_hls?cid=xxxx&copy=0&file=index.m3u8",
  "segment_duration_secs": 2.0,
  "auth_token": "ya29.a0ARrdaM...",
  "max_segments": 5
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"hls"`. |
| `id` | string | Yes | - | Unique output ID. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `ingest_url` | string | Yes | - | HLS ingest base URL. Must start with `http://` or `https://`. |
| `segment_duration_secs` | float | No | `2.0` | Target segment duration in seconds. Range: 0.5-10.0. |
| `auth_token` | string | No | `null` | Bearer token sent with each HTTP upload request. |
| `max_segments` | integer | No | `5` | Maximum segments in the rolling playlist. Range: 1-30. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = each segment carries the full MPTS; `Some(N)` = each segment carries only program N as a rewritten single-program TS. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `audio_encode` | object | No | `null` | Optional per-segment ffmpeg remuxer. Each segment is piped through `ffmpeg -i pipe:0 -c:v copy -c:a {codec} -f mpegts pipe:1` before HTTP PUT. Allowed `codec`: `aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`. Requires ffmpeg in PATH; the output refuses to start if ffmpeg is missing. See the [`audio_encode` block](#the-audio_encode-block-phase-b) below. |

**Limitations:**
- Output only. Segment-based transport inherently adds 1-4 seconds of latency.

### CMAF / CMAF-LL Output

Publishes fragmented-MP4 (CMAF per ISO/IEC 23000-19) segments with HLS m3u8
and/or DASH .mpd manifests to an operator-supplied HTTP push ingest. Sibling
to the HLS output but with fMP4 segments — suitable for AWS MediaStore,
Fastly, Akamai MSL, and any CDN that accepts CMAF HTTP PUT ingest. Supports
H.264 / HEVC video passthrough or on-the-fly re-encoding, AAC audio
passthrough or re-encoding, Low-Latency CMAF via chunked transfer, and
ClearKey Common Encryption (`cenc` / `cbcs`) with optional Widevine /
PlayReady PSSH passthrough.

```json
{
  "type": "cmaf",
  "id": "cmaf-cdn",
  "name": "CMAF to CDN",
  "ingest_url": "https://ingest.cdn.example.com/live",
  "segment_duration_secs": 2.0,
  "max_segments": 5,
  "manifests": ["hls", "dash"],
  "low_latency": false
}
```

LL-CMAF example with DRM:

```json
{
  "type": "cmaf",
  "id": "cmaf-ll-drm",
  "name": "LL-CMAF with ClearKey",
  "ingest_url": "https://ingest.cdn.example.com/live",
  "segment_duration_secs": 2.0,
  "chunk_duration_ms": 500,
  "low_latency": true,
  "manifests": ["hls", "dash"],
  "encryption": {
    "scheme": "cenc",
    "key_id": "0123456789abcdef0123456789abcdef",
    "key": "fedcba9876543210fedcba9876543210",
    "pssh_boxes": []
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"cmaf"`. |
| `id` | string | Yes | - | Unique output ID. Max 64 chars. |
| `name` | string | Yes | - | Human-readable display name. |
| `ingest_url` | string | Yes | - | CMAF ingest base URL. Must start with `http://` or `https://`. Artifacts are PUT to `{ingest_url}/init.mp4`, `{ingest_url}/seg-{00001}.m4s`, `{ingest_url}/manifest.m3u8`, `{ingest_url}/manifest.mpd`. |
| `segment_duration_secs` | float | No | `2.0` | Target segment duration in seconds. Range: 1.0-10.0. Segments cut on IDR — source must emit an IDR at least every `segment_duration_secs` unless `video_encode` is set (which forces GoP alignment). |
| `max_segments` | integer | No | `5` | Rolling playlist window. Range: 1-30. |
| `manifests` | array | No | `["hls","dash"]` | Subset of `{"hls", "dash"}`, non-empty. Both manifests reference the same fMP4 segments — enable either or both. |
| `low_latency` | bool | No | `false` | Enable LL-CMAF: emits a moof+mdat chunk every `chunk_duration_ms` inside a single chunked-transfer PUT per segment, advertises parts via `#EXT-X-PART` (HLS) and `availabilityTimeOffset` (DASH). Target end-to-end latency <3 s with 500 ms chunks. |
| `chunk_duration_ms` | integer | No | `500` | LL-CMAF chunk duration in ms. Range: 100-2000. Ignored when `low_latency = false`. |
| `encryption` | object | No | `null` | Common Encryption configuration. See [`encryption`](#the-cmaf-encryption-block) below. |
| `audio_encode` | object | No | `null` | Optional AAC re-encode. Allowed `codec`: `aac_lc`, `he_aac_v1`, `he_aac_v2`. Source must already be AAC (TsDemuxer decodes via fdk-aac). When omitted, the source AAC passes through unchanged. |
| `video_encode` | object | No | `null` | Optional H.264 / HEVC re-encode with explicit GoP alignment to `segment_duration_secs`. See the [`video_encode` block](#the-video_encode-block) for backends and fields. H.264 → H.264 or HEVC → H.264 conversion is supported when the matching `video-encoder-*` Cargo feature is enabled. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `auth_token` | string | No | `null` | Bearer token sent with every HTTP PUT / chunked PUT. |

#### The CMAF `encryption` block

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `scheme` | string | Yes | - | Either `"cenc"` (AES-128 CTR, the CENC default) or `"cbcs"` (AES-128 CBC with 1:9 block pattern; required for Apple FairPlay). |
| `key_id` | string | Yes | - | Key ID — exactly 32 hex characters (16 bytes). Embedded in `tenc.default_KID` and the ClearKey `pssh`. |
| `key` | string | Yes | - | AES-128 content key — exactly 32 hex characters. This is a secret; clients receive the key via the ClearKey license flow or via your commercial DRM system's PSSH. |
| `pssh_boxes` | array | No | `[]` | Operator-supplied pre-built `pssh` box payloads (hex-encoded), one per additional DRM system (Widevine, PlayReady, FairPlay). Each entry is the complete `pssh` box starting at the 4-byte size field; fourcc is sanity-checked at validation. The edge wraps them verbatim into `moov`. |

When `encryption` is set, the edge:

1. Emits `encv` / `enca` sample entries that wrap `avc1` / `hvc1` / `mp4a` via a `sinf/frma/schm/schi/tenc` chain (ISO/IEC 23001-7 §8).
2. Subsample-encrypts each H.264 / HEVC sample — NAL length prefix + NAL header + 32 bytes of slice header are left clear; the rest of the VCL NAL payload is encrypted. Parameter-set NALs (SPS / PPS / VPS / SEI / AUD) stay fully clear. For `cbcs` the encrypted span is rounded down to a multiple of 16 bytes.
3. AAC samples are whole-encrypted with no subsample split.
4. Writes `senc` / `saio` / `saiz` into every `traf` with correctly back-patched offsets.
5. Emits a ClearKey `pssh` (system ID `1077efec-c0b2-4d02-ace3-3c1e52e2fb4b`, version 1) into `moov`, plus any operator-supplied `pssh_boxes` verbatim.

**Limitations:**

- Output only. Standard-mode segment-based transport adds 1-4 s latency; LL-CMAF with 500 ms chunks targets <3 s glass-to-glass.
- Source must emit an IDR at least every `segment_duration_secs` unless `video_encode` is set.
- `video_encode` requires the `media-codecs` feature plus a matching `video-encoder-x264` / `-x265` / `-nvenc` / `-qsv` backend compiled in.
- Whip-style signaling is not needed — CMAF is stateless HTTP push.

See [`docs/cmaf.md`](cmaf.md) for the full reference, ingest compatibility notes, and performance tuning.

### WebRTC Output

Supports two modes: WHIP client (push to external endpoint) and WHEP server (serve viewers). The `webrtc` feature is enabled by default.

**WHIP Client mode** — push to an external WHIP endpoint:

```json
{
  "type": "webrtc",
  "id": "whip-push",
  "name": "Push to CDN",
  "mode": "whip_client",
  "whip_url": "https://whip.example.com/ingest/stream1",
  "bearer_token": "my-auth-token"
}
```

**WHEP Server mode** — serve browser viewers:

```json
{
  "type": "webrtc",
  "id": "whep-serve",
  "name": "Browser Viewers",
  "mode": "whep_server",
  "max_viewers": 20,
  "bearer_token": "viewer-auth-token"
}
```

Viewers POST an SDP offer to `/api/v1/flows/{flow_id}/whep` and receive an SDP answer.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"webrtc"`. |
| `id` | string | Yes | - | Unique output ID. |
| `name` | string | Yes | - | Human-readable display name. |
| `mode` | string | No | `"whip_client"` | `"whip_client"` (push to endpoint) or `"whep_server"` (serve viewers). |
| `whip_url` | string | WHIP only | - | WHIP endpoint URL. Required for `whip_client` mode. |
| `bearer_token` | string | No | `null` | Bearer token for authentication. |
| `max_viewers` | integer | No | `10` | Max concurrent viewers (WHEP server mode only, 1-100). |
| `public_ip` | string | No | `null` | Public IP for ICE candidates (NAT traversal). |
| `video_only` | boolean | No | `false` | Only send video (audio omitted). Mutually exclusive with `audio_encode` — validation rejects the combination because an audio MID must be negotiated in SDP for the encoder to write to. |
| `program_number` | integer | No | `null` | MPTS program selector. `null` = lock onto the lowest program_number in the PAT (deterministic default); `Some(N)` = extract elementary streams from program N only. WebRTC is single-program by spec, so this only changes *which* program is sent. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `audio_encode` | object | No | `null` | Optional ffmpeg-sidecar audio encoder. The only realistic codec for WebRTC is `opus`, and validation rejects anything else. When set, input AAC-LC is decoded in-process via the Phase A `AacDecoder`, encoded to Opus via the Phase B ffmpeg sidecar, and written to the WebRTC audio MID via str0m. This is the marquee Phase A+B "AAC contribution → Opus distribution" path. Requires `video_only=false` and ffmpeg in PATH. The encoder builds lazily on the first AAC frame after a viewer connects. See the [`audio_encode` block](#the-audio_encode-block-phase-b) below. |

**Audio:** Without `audio_encode`, the WebRTC output is video-only when
the source carries AAC (Opus passthrough only — Opus flows natively on
WebRTC paths). Setting an `audio_encode` block (codec: `opus`) enables
the marquee Phase A+B chain: AAC decoded in-process via Phase A's
`AacDecoder`, re-encoded as Opus via Phase B's ffmpeg sidecar
`AudioEncoder`, written to the str0m audio MID. See
[`audio-gateway.md`](audio-gateway.md#the-audio_encode-block--compressed-audio-egress-rtmp--hls--webrtc).

### The `audio_encode` block (Phase B)

Used by RTMP, HLS, WebRTC, and — since the Phase B extension — SRT,
UDP, RTP, and RIST TS outputs. Validation enforces a strict
codec×output matrix at config load time: RTMP allows AAC-LC / HE-AAC
v1 / HE-AAC v2; HLS and the TS outputs allow the same plus MP2 / AC-3;
WebRTC allows Opus only.

Every output that accepts `audio_encode` also accepts an optional
companion `transcode` block (channel shuffle / sample-rate /
bit-depth conversion applied to the decoded PCM *before* re-encoding).
When both blocks set the same field, `transcode` wins; `audio_encode`
fields are used as fallbacks for what `transcode` leaves unset. See
[`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion)
for the resolution rules, per-output coverage matrix, and worked
examples.

```json
{
  "codec": "opus",
  "bitrate_kbps": 96,
  "sample_rate": 48000,
  "channels": 2
}
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `codec` | string | Yes | - | One of: `aac_lc`, `he_aac_v1`, `he_aac_v2`, `opus`, `mp2`, `ac3`. Must be valid for the parent output type per the matrix above. |
| `bitrate_kbps` | integer | No | per-codec default (AAC-LC=128, HE-AAC-v1=64, HE-AAC-v2=32, Opus=96, MP2=192, AC-3=192) | Output bitrate in kbps. Range 16..=512. |
| `sample_rate` | integer | No | input sample rate | Output sample rate (Hz). Allowed: 8000, 16000, 22050, 24000, 32000, 44100, 48000. **Opus is always carried at 48 kHz on the wire** regardless of this field. |
| `channels` | integer | No | input channel count | Output channel count, 1 or 2. |

**Failure modes:** the encoder is opt-in. When ffmpeg is missing in
PATH, the input is non-AAC-LC, the flow input cannot carry TS audio
(PCM-only sources), or the encoder spawn / restart cap is exhausted,
the output emits a Critical `audio_encode` event to the manager and
audio is dropped silently for the rest of the output's lifetime
(video continues). HLS refuses to start outright when ffmpeg is
missing because it can't degrade gracefully. See
[`events-and-alarms.md`](events-and-alarms.md#audio-encoder-audio_encode)
for the full event reference.

**HE-AAC v2 caveat:** `aac_he_v2` requires an ffmpeg build with
`libfdk_aac`. If the host's ffmpeg doesn't have it, the encoder
fails fast on the first frame and emits the failure event.

### Display Output (HDMI / DisplayPort + ALSA)

Decode the flow's video + audio and render to a **physical Linux
display connector** (HDMI / DisplayPort) plus an ALSA audio device.
Useful for stadium / OB-truck / MCR / gallery confidence monitors that
the operator wants the manager to control alongside every other
output.

```json
{
  "type": "display",
  "id": "disp-1",
  "name": "Green-room HDMI",
  "device": "HDMI-A-1",
  "audio_device": "hw:0,3",
  "program_number": null,
  "audio_track_index": 0,
  "audio_channel_pair": [0, 1],
  "scaling_mode": "match_source",
  "sync_mode": "vsync_to_display",
  "show_audio_bars": true
}
```

- **`device`** (required) — KMS connector name from
  [`HealthPayload.display_devices`](events-and-alarms.md#display-output-events-display).
  Validated against the canonical KMS pattern `^[A-Z][A-Z0-9-]{0,63}$`
  (e.g. `HDMI-A-1`, `DP-2`, `DVI-D-1`).
- **`audio_device`** — ALSA device id (`hw:N,M` / `plughw:N,M` /
  `default` / `sysdefault` / `pulse`). `null` mutes audio playback —
  the output renders video only. The manager UI populates this from
  the connector's enumerated `alsa_device` field; operators can
  override. Audio sample rate and channel count adapt per-frame to
  whatever the source decoder reports — AAC / MP2 / AC-3 / E-AC-3 at
  any rate the device's plug layer supports works without further
  configuration. Use `plughw:N,M` (not raw `hw:`) when your sink only
  speaks 48 kHz but the source might carry 44.1 / 32 / 96 kHz audio
  so ALSA's automatic resampler engages.
- **`program_number`** — MPTS program filter (1-based). `null` selects
  the lowest program in the active input's PAT.
- **`audio_track_index`** — 0-based index into the program's audio
  elementary streams (`0..=15`). Lets the operator pick `EN` over `ES`
  on dual-language broadcasts.
- **`audio_channel_pair`** — which two channels of the decoded
  multichannel audio drive the stereo ALSA sink. Default `[0, 1]` =
  L/R. Both indices must be `< 8` and not equal.
- **`scaling_mode`** — `"match_source"` (default) or `"monitor_native"`.
  `match_source` re-modesets the panel to the smallest connector mode
  that covers the source's `(width, height)` on the first decoded frame
  (and again on any source-shape change) — no scaling cost, tightest
  A/V timing. `monitor_native` holds the panel at its EDID-preferred
  mode and upscales the source via libswscale — the right pick for
  fixed-mode panels (HDCP-locked displays, signage) and most desktop
  monitors that handle their native mode best.
- **`sync_mode`** — only `"vsync_to_display"` in v1: the renderer
  paces to monitor vsync and dup/drops video to track the audio
  master clock. PTP-genlocked / PCR-master modes land in v2.
- **`show_audio_bars`** — `true` to render a per-PID, per-channel
  audio level meter strip across the bottom 12 % of the picture (peak
  + RMS bar with a 1.5 s peak-hold tick; green ≤ -18 dBFS, yellow
  -18 → -3 dBFS, red > -3 dBFS). Independent of `audio_track_index` —
  every audio PID in the active program is decoded and metered, even
  ones not routed to ALSA. Adds an independent multi-PID audio
  decoder pool (~15 resource-budget units per output, ~1-2 % CPU per
  PID at typical broadcast bitrates). Defaults `false`.

  **How the bars reach the panel matters on a zero-copy host.** The output
  first tries to program a dedicated KMS overlay plane, composed in hardware at
  no per-frame cost; `display_stats.bars_overlay_enabled` reports whether it
  got one. On a host with no drivable overlay plane the edge falls back to
  baking the bars on the CPU, which forces **every** zero-copy (VAAPI / RKMPP)
  surface through a per-frame GPU→CPU download so the primary dumb buffer can
  be blitted. `download_count` then climbs on every frame and `blit_us_avg`
  rises — exactly the reading that otherwise means "this host cannot do
  zero-copy", so check `bars_overlay_enabled` before drawing that conclusion.
  Sources above 1920×1080 skip the bake entirely (it would exceed the CPU-blit
  ceiling), so on a 4K zero-copy source bars are simply **absent** while the
  fallback is engaged. The output retries the overlay periodically and restores
  hardware composition on its own when one becomes available.

- **`hw_decode`** — which decoder this output opens. `"auto"` (the default,
  same as unset) picks the best backend this build has compiled in and this
  host probed, in the order **VAAPI ≻ NVDEC ≻ QSV ≻ RKMPP ≻ CPU**. `"cpu"`
  forces software libavcodec, which leaves hardware sessions free for
  transcode flows elsewhere on the node. `"nvdec"` / `"qsv"` / `"vaapi"` /
  `"rkmpp"` force one backend.

  **A forced backend the host cannot satisfy does not stop the output.** The
  broadcast invariant here is that a display output never goes dark for a
  hardware-availability reason: the edge emits the Warning
  `display_hw_decode_unavailable_falling_back` — with `reason` one of
  `feature_disabled` (not in this build), `driver_missing` (build has it, the
  probe found no usable driver) or `probe_unavailable` (the probe did not run)
  — and runs CPU so the picture stays on screen. Read `decoder_kind` on
  `display_stats` to tell the two CPU cases apart: it says `"cpu (hw
  unavailable)"` after a fallback and plain `"cpu"` when CPU is what you asked
  for or what `auto` resolved to.

  VAAPI and RKMPP additionally enable zero-copy DMA-BUF scanout, reported as
  `decoder_kind` `"vaapi-zerocopy"` / `"rkmpp-zerocopy"`. That label is
  **static**: it says which path was chosen, not whether the copy is actually
  being skipped this second. Watch `download_count` for that — and read
  `bars_overlay_enabled` first (see `show_audio_bars` above), because a
  CPU-baked audio meter forces a download on every frame on a host that is
  otherwise perfectly capable of zero-copy.

- **`mpeg2_cpu_decode`** — override the fleet-wide "MPEG-2 decodes on
  CPU" policy for this output. Unset (default) keeps it; `false` opts
  out so MPEG-2 uses the hardware backend; `true` forces CPU even on a
  backend that is otherwise exempt (NVDEC).

  MPEG-2 is pinned to CPU on every hardware backend except NVDEC. On
  **RKMPP** that is a capability fact — the vendored fork registers no
  MPEG-2 decoder at all. On **VAAPI** it is a conservative default
  rather than a measurement: a matched A/B on Intel iHD put CPU at
  27–29 fps / 2.3 drops-per-second against VAAPI's 27–30 / 2.0 —
  parity within noise — at a cost of roughly 15 points of process CPU.
  The pin is source-agnostic, so it also applies to an MPEG-2
  *contribution* feed on a monitoring wall, not just a media player.

  Set `false` only if you have measured this host. A backend that then
  fails MPEG-2 at run time is pinned back to CPU automatically and this
  field does **not** override that runtime-learned pin — so opting out
  can cost frames but cannot wedge the output.

- **`present_lead_ms`** — how far ahead of the panel the wall-clock pacer
  buffers, in milliseconds. Range `0..=1000`. Unset (default) resolves
  per **live decoder**: `200` on RKMPP, `0` — bit-for-bit unchanged — on
  every other backend.

  The pacer sleeps until each frame's due time, which absorbs arrival
  jitter perfectly right up until a frame arrives *after* its due time.
  Past that there is nothing left to sleep: the frame is presented
  immediately on top of the previous one and the next waits its full
  slot. That short/long pair is what an operator sees as stutter, and
  because every frame is still presented, no loss counter moves — which
  is why it went undiagnosed until `present_bucket` was added (issue
  #104). RKMPP's `receive_frame` blocks up to 17 ms and delivers in
  bursts; VAAPI's returns in ~1 µs and never triggers it.

  Lead time moves every target later, which in practice keeps
  `lead / frame_period` frames buffered ahead of the panel so a burst is
  served from that backlog. Measured on RK3568 at 25 fps, presents
  landing on target: 70 % at `0`, 68 % at `40`, 81 % at `80`, 98 % at
  `120`, 99.8 % at `200`. One frame period buys nothing — the elbow is
  ~120 ms and 200 ms saturates.

  The cost is exactly that much added display latency, and nothing else:
  the setting lives on the muted / video-only pacing branch, so there is
  no audio for the picture to fall out of sync with. Set `0` where a
  confidence monitor's latency matters more than its smoothness.

  Two limits worth knowing. It is **clamped at run time** to what the
  decode queue can hold (`MPSC_VIDEO_DEPTH / 3` frames) — a lead deeper
  than the queue does not buffer, it spills onto
  `frames_dropped_mpsc_full`. And it **does not apply to an output with
  an `audio_device`**: that path paces against the measured ALSA playout
  position, which has no anchor to seed, so an audio-enabled display
  output on RKMPP still has the fault.

- **`present_vblank_cadence`** — schedule frames onto whole vblanks
  instead of onto wall-clock instants. Boolean, **default off**.

  **Read once, when the display output starts.** Nothing re-reads it at run
  time, and the config diff *replaces* a changed output rather than patching it
  — `remove_output` followed by `add_output` — so pushing this field tears the
  display output down (DRM master released, ALSA closed) and rebuilds it. On a
  display output that is a visible modeset and decoder re-open, not a
  next-frame change. Toggle it in a maintenance window, not on air.

  `present_lead_ms` above fixes frames arriving late. This fixes a
  different fault that survives it: the panel and the source run on
  independent crystals — this fleet's panels sit between −54.69 and
  +64.32 ppm of nominal — so even a perfectly-paced target slides against
  the scanout raster. When it drifts near a vblank boundary,
  sub-millisecond scheduling noise decides which of two adjacent vblanks
  catches the flip, and the panel shows one frame for one vblank and the
  next for three. Nothing is late and nothing is dropped, so **every loss
  counter reads clean while the picture judders** (issue #112).

  With this on, each frame is held for the whole number of vblanks its
  cadence allocates — a flat 2,2,2 at 25p on a 50 Hz panel, a generated
  3,2,3,2 pulldown at 24p on 60 Hz. Crystal drift is *absorbed* rather
  than corrected, surfacing as one longer or shorter hold per beat
  period (roughly every ten minutes at 33 ppm) instead of a continuous
  sub-frame slide.

  **It declines to engage more often than it engages, by design.** All of
  the following must hold, and any one of them keeps the existing
  wall-clock path with no loss of behaviour:

  - The driver's flip clock has earned trust — a full measurement window
    completed and agreed with the mode's advertised refresh. A driver
    reporting a fabricated or stalled timebase is never scheduled against.
  - The rate pair is schedulable: panel ÷ source must land in `1.0..=16.0`.
    A source *faster* than the panel needs frame dropping, which is a
    different algorithm and is not implemented — 60p on a 50 Hz panel is
    declined.
  - **No audio clock is master.** The gate is the live pacing reference, not
    the config field: the resolver tests whether an ALSA playout position is
    currently driving the pacer, and disengages an already-engaged cadence the
    moment one comes up. So an output that has an `audio_device` configured
    *can* engage when audio never opened (`display_audio_open_failed`) or the
    source carries none. Audio is master on that path and holding frames on
    the vblank raster would fight it; locking video to the panel is only sound
    once audio is resampled to the same clock, which this does not do.
  - There is **headroom**: the per-frame **download + blit-and-present** cost
    must sit under 60 % of the frame period. Decode time is *not* part of this
    gate — that is the separate `decode_us_avg` counter. Where the budget is
    exceeded, holding a frame pushes the next decode past its slot and the
    queue sheds. Measured per 40 ms frame: RK3568 3 749 µs (9.4 %), bilby-bite
    10 750 µs (26.9 %), RK3588 13 891 µs (34.7 %) all engage; an Intel
    Gen9 NUC at 32 093 µs (80.2 %) is refused. Sampled over a rolling 40-frame
    window and **only while the cadence is disengaged**, because the blit
    timer spans the hold loop and an engaged frame's cost therefore includes
    its own hold — the same RK3588 reads 13 891 µs off and 39 536 µs on, so
    gating on an engaged sample would refuse the node this works best on.

  A **runaway guard** backs all of that at run time. A correct cadence
  sheds exactly zero frames, so any shedding that persists across two
  consecutive 2-second samples disengages the feature and reverts to
  wall-clock pacing, with an escalating cooldown that gives up entirely
  after three failures on one output. Watch `cadence_disengaged` — any
  non-zero value means this host could not hold frames and said so.

  Measured on RK3588 at 25p on a 50 Hz panel: presents on target 94.0 %
  off, 100.0 % on (71 of 72 five-second windows exact). On the Gen9 NUC
  the headroom precondition refuses and the node is untouched; forced on
  before that precondition existed it fell to 49.5 % on-target and
  24.999 → 19.988 fps with 441 dropped frames, which is what the guard
  and the precondition exist to prevent.

  Off by default pending fleet validation. Turn it on where you can see
  a panel and confirm the result.

**Build prerequisites.** `display` is Linux-only and gated on the
`display` Cargo feature (off by default). Schema is unconditional —
configs round-trip on every platform. On non-Linux / non-feature
builds the spawner refuses with `display_device_invalid` so the
manager UI can highlight the offending field. Install:

```sh
sudo apt install libasound2-dev
cargo build --release --features display
```

**Mode selection.** Driven by `scaling_mode`:

- **`match_source`** (default). The display task opens the connector
  at its preferred mode, then re-modesets to the smallest connector
  mode whose dimensions are both ≥ source on the first decoded frame.
  A 1080p source on a 4K panel lands at 1080p (no CPU upscale spike),
  a 720p source lands at 720p. Refresh rate stays at the panel's
  preferred / native rate — desktop monitors that advertise low-rate
  EDID modes (24 / 25 / 30 Hz) typically can't drive them without
  flicker or sync loss, so we don't switch into them. The audio-master
  dup/drop logic in the display loop handles the source-fps-vs-panel-
  refresh cadence (every consumer media player on a 60 Hz monitor does
  it the same way). Mid-stream resolution changes and operator input
  switches re-arm the autodetect.
- **`monitor_native`**. The display task opens the connector at its
  preferred (panel-native) mode and **holds it** for the lifetime of
  the output. Source frames are upscaled to fill the panel via
  libswscale. Pick this on fixed-mode panels (HDCP-locked, signage)
  and most desktop monitors — modern panels handle their native mode
  significantly better than non-native ones (no scaler ghosting, no
  EDID-mode flicker). Cost: one libswscale upscale per frame in the
  display task; negligible at typical 1080p → 4K ratios.

The deprecated config fields `resolution` / `refresh_hz` are accepted
by the deserializer for backward-compat round-trip but ignored at
runtime — re-save the output via the manager UI to drop them.

**Codec coverage.** Video: H.264 + HEVC (software decode via
libavcodec — same backend as content-analysis). Audio: AAC family
(in-process via fdk-aac), MP2 / AC-3 / E-AC-3 (in-process via the new
`video-engine::AudioDecoder` libavcodec wrapper) — every common
broadcast audio codec works out of the box.

**A/V sync = audio is master.** The audio child task's blocking ALSA
write *is* the wall clock; the display task reads a lock-free
`AudioClock` per-frame and dup/drops video to keep the offset within
±1 frame period. ALSA xrun (`EPIPE`) → `prepare()` and continue
without nudging the anchor (clock pauses for the recovery window —
psychoacoustically less harmful than a fast-forward).

**Capacity.** Each running display output consumes 275 resource-budget
units (1080p30 baseline — 250 for the SW video decode, 5 for ALSA, 20
for the KMS render). 4K60 outputs scale to ≈1025 — comparable to a
4K60 SW transcode. See [Capacity & resource budget](#capacity--resource-budget).

**Limitations (v1, called out explicitly):**

- Software video decode only. Hardware decode (VAAPI / NVDEC) is
  scheduled for v2 behind the `display-vaapi` / `display-nvdec`
  Cargo features (placeholders today).
- HDMI hotplug rediscovery is startup-only. New connectors require
  an edge restart to appear in `display_devices`.
- Multichannel passthrough (5.1/7.1 LPCM over HDMI) is downmixed to
  the configured stereo `audio_channel_pair`. Full passthrough is
  v2.
- No HDR / colour-management / SCTE-104 / closed-caption rendering.
- One display output renders to a given connector at a time. **Multiple
  flows may configure outputs that target the same `(device,
  audio_device)` pair** — KMS / ALSA only let one writer hold the
  device at a time, so the per-edge `DisplayClaimRegistry` serialises
  them. **First to start wins** the slot; the others register as FCFS
  waiters and emit `display_output_waiting` (Info, `details` carry
  `holder_*` and `queue_position`). When the holder stops (manual
  stop, output removal, fatal error), the next live waiter is
  promoted automatically — `display_output_acquired` fires, and the
  modeset proceeds as a fresh `display_started`. Take-over is a few
  hundred milliseconds (KMS modeset + ALSA open). Useful for hot
  primary/backup confidence-monitor swaps where the operator just
  stops one flow and the next one already configured for the same
  HDMI takes over without manual reconfiguration. The static-config
  validator no longer rejects duplicates — it logs a `warn!` and
  lets the runtime queue handle it.

**Host prerequisites (NVIDIA / desktop systems).** Because the display
output programs the connector's CRTC directly, the host must satisfy a
few KMS/DRM conditions before a picture appears — most commonly
`nvidia-drm.modeset=1` on NVIDIA GPUs, and **no desktop compositor**
holding the DRM master (stop GDM / run headless). A compositor-held
master surfaces as a Warning `display_output_waiting` event carrying
`kms_error_code: display_master_busy`; a driver that stops posting
page-flip completions surfaces as the Warning `display_flip_timeout` event.
The full setup recipe (NVIDIA modeset, driver currency, stopping/
restoring the desktop, console/VT noise) lives in
[`docs/installation.md`](installation.md#local-display-output-display-feature-linux-only)
under *Host prerequisites (NVIDIA GPUs + desktop systems)*.

See [`docs/events-and-alarms.md`](events-and-alarms.md#display-output-events-display)
for the full event catalogue, including `display_device_unavailable`,
`display_mode_set_failed`, `display_master_busy`, `display_flip_timeout`,
`display_audio_open_failed`, `display_frame_loss_sustained` /
`display_frame_loss_recovered`, `display_subscriber_lagged`.
(`display_decoder_overload` and `display_av_drift` appear in older
revisions of this list but were never implemented — see the catalogue.)

### SDI Output (Blackmagic DeckLink playout)

Decodes the flow and plays it out of a Blackmagic **DeckLink** SDI
connector — video plus embedded audio, scheduled on the card's own
clock. Same Cargo feature gate as the SDI input (`sdi-decklink`,
default off).

```json
{
  "type": "sdi",
  "id": "sdi-out1",
  "name": "SDI monitor",
  "device": "DeckLink Quad (2)",
  "mode": "Hi50",
  "pixel_format": "uyvy422",
  "audio_channels": 2,
  "audio_offset_ms": 0,
  "program_number": null
}
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `type` | string | — | Always `"sdi"`. |
| `device` | string | — | **Required**, non-empty. Same namespace as the input's. On 8-port Quad cards mind the connector-pair routing — a sub-device playing out while its own connector carries an input emits on its *pair partner's* connector ([`sdi.md`](sdi.md)). |
| `mode` | string | — | **Required**, and explicit: a 4-character DeckLink mode FourCC (`"Hi50"`, `"Hp25"`, …). `"auto"` is **rejected at validation** — playout has nothing to auto-detect from. Must match the decoded video's raster; mismatched frames are dropped with `sdi_playout_raster_mismatch` rather than displayed garbled. |
| `pixel_format` | string | `"uyvy422"` | Only `"uyvy422"` (8-bit); anything else is rejected at config load. 10-bit playout is not implemented. |
| `audio_channels` | u8 | `2` | `0` / `2` / `8` / `16`; `0` = video-only. The flow's audio is decoded (AAC / MP2 / AC-3 / E-AC-3), interleaved to this channel count and lip-synced to video on the shared playout clock. Fixed **48 kHz** — a non-48 kHz track is dropped with an alarm (no resampler yet). Opus and AC-4 are not handled; those flows play out video-only. |
| `program_number` | u16 | *(lowest in PAT)* | MPTS down-select, like every other output. `0` is rejected (reserved for the NIT). |
| `audio_offset_ms` | i32 | `0` | Operator A/V-sync trim, validated to `-1000..=1000`. **Positive delays audio** (plays later — corrects audio-early); **negative advances it**. Applied as a constant shift to each scheduled audio block's card time; the drift-free sample counter is untouched, so the trim never accumulates. Use it to null a residual lip-sync offset measured on a real reference monitor. |

Only **8-bit 4:2:0/4:2:2** decoded video can be packed to UYVY422; a
4:4:4 or 10-bit source drops frames with
`sdi_playout_chroma_unsupported` rather than displaying corrupted
colour. An unsupported `mode`/`device` is **fatal** for the output (a
retry cannot fix a config problem); a device that vanishes mid-run
re-opens with backoff.

Cost model: **275 units** (CPU decode, 1080p-class) — the same weight
as a display output. Full pipeline, telemetry, loopback-verification
notes and the genlock caveat: [`docs/sdi.md`](sdi.md#sdi-output-playout).

### MXL Outputs (`mxl_video` / `mxl_audio` / `mxl_anc`)

Publish the flow onto the same-host MXL shared-memory bus — the egress
mirror of the [MXL inputs](#mxl-inputs-mxl_video--mxl_audio--mxl_anc).
Same `mxl` Cargo feature gate (default off) and the same **PTP-mandatory**
constraint. As with the inputs, the **video** bridge is implemented and
the **audio** bridge is a known TODO.

```json
{
  "type": "mxl_video",
  "id": "mxl-out1",
  "name": "MXL program out",
  "domain_path": "/dev/shm/mxl-domain",
  "flow_name": "program-video",
  "width": 1920,
  "height": 1080,
  "frame_rate_num": 30000,
  "frame_rate_den": 1001,
  "clock_domain": 0
}
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `type` | string | — | `"mxl_video"`, `"mxl_audio"` or `"mxl_anc"`. |
| `id` / `name` | string | — | Standard output identity. |
| `active` | bool | `true` | Whether the engine spawns it. |
| `domain_path` | string | — | **Required.** MXL domain directory on tmpfs/ramfs. |
| `flow_name` | string | — | **Required.** libmxl flow name to publish onto. |
| `clock_domain` | u8 | *(inherits flow)* | PTP clock domain `0..=127`. |

`mxl_video` adds `width` / `height` / `frame_rate_num` /
`frame_rate_den` (it decodes the flow's TS and produces V210 grains);
`mxl_audio` adds `channels` + `packet_time_us`; `mxl_anc` carries only
the domain reference + `clock_domain`. See
[`mxl-integration-plan.md`](mxl-integration-plan.md) for the essence path
detail.

### Bonded Output

Sends a media flow over the bilbycast multi-path bonding stack —
the Rust replacement for appliances like Peplink/SpeedFusion in
broadcast contribution paths. Multiple network paths are
aggregated for throughput and failover.

```json
{
  "type": "bonded",
  "id": "out-bonded",
  "name": "Bonded send",
  "remote_addr": "203.0.113.10:5500",
  "psk": "<32-byte hex>"
}
```

The full Bonded protocol — path adapters, link selection, latency
budget, FEC — is covered in [`bilbycast-bonding/CLAUDE.md`](../../bilbycast-bonding/CLAUDE.md)
and [`docs/bonding.md`](bonding.md). The bonded receiver at the
other end uses the matching [Bonded Input](#bonded-input).

---

## Flow Assembly (PID bus — SPTS / MPTS from N inputs)

A flow can optionally carry an `assembly` block that tells the runtime to stop forwarding one input verbatim and instead **build a fresh MPEG-TS from elementary streams pulled off any of the flow's inputs**. The same broadcast channel that a passthrough flow uses then carries the assembled TS, so every existing output type consumes it unchanged — UDP, RTP (with or without 2022-1 FEC / 2022-7), SRT (incl. bonded / 2022-7), RIST (incl. ARQ), RTMP/RTMPS, HLS, CMAF / CMAF-LL (incl. ClearKey CENC), WebRTC WHIP/WHEP. RTMP and WebRTC demux one program out by default (lowest `program_number` in the PAT, or the output-level `program_number` override).

Flows without an `assembly` block — or with `assembly.kind = passthrough` — run exactly as before. Existing configs are unaffected.

### Three operating modes (pick one per flow)

Flows can run in one of three modes — all coexist in the same edge build, all use the same outputs:

| Mode | When to pick it | Output PIDs on input switch | Input-switcher (`ActivateInput`) behaviour |
|---|---|---|---|
| **Passthrough** (`assembly = null` or `kind = "passthrough"`) | One input is "live" at a time; you want the receiver to see whatever PIDs that input declared, byte-for-byte. | **Change** — receivers see new PMT versions and re-tune. `TsContinuityFixer` cushions CC + PMT version + DI to keep the cutover seamless. | Flips which input's bytes are forwarded to outputs. Classic switcher behaviour. |
| **Assembly without Switch slots** (`kind = "spts" \| "mpts"`, slots use `pid` / `essence` / `hitless`) | You want a fresh PMT layout — unified output PIDs (e.g. always video on `0x100`) — built from one input or from a 2022-7 redundant pair. Every referenced input runs concurrently and contributes ES simultaneously. | **Stay unified** — every slot's `out_pid` is fixed by the assembly. | No-op for the data path. Every input contributes ES simultaneously regardless of which is "active". |
| **Assembly with Switch slots** (`kind = "spts" \| "mpts"`, one or more slots use `switch`) | You want operator-driven N-input switching with **unified output PIDs** — receivers stay locked across switches. All N legs subscribe concurrently (warm) so cutover is instant. | **Stay unified** — the slot's `out_pid` is fixed; only the source leg flips. PMT version bumps mod 32 + DI=1 on the next PCR for the affected `out_pid` so receivers re-anchor STC without re-tuning. | Flips the active leg of every Switch slot whose leg list contains the named input. Slots without that input as a leg are silent. |

The three modes can be mixed within an MPTS — one program can run as passthrough-style explicit `pid` slots, another can use `hitless` for redundancy, a third can use `switch` for operator-driven multi-cam. PIDs always behave per the slot source type.

### `assembly.kind`

| Kind | What it builds | Program count | PCR requirement |
|------|----------------|---------------|-----------------|
| `passthrough` | No assembly. Forwards the active input's bytes. Runtime-equivalent to `"assembly": null`. | must be empty (`programs = []`) | must be absent |
| `spts` | Single-program TS synthesised from selected ES slots. | exactly one program | flow-level *or* program-level `pcr_source` required (program-level wins) |
| `mpts` | Multi-program TS with fresh PAT listing every program and one synthesised PMT per program. | one or more programs, unique `program_number` per program | every program needs an effective `pcr_source` (its own, or the flow-level fallback) |

### Minimal SPTS example — mixing video from input A with audio from input B

```json
{
  "id": "mixed-feed",
  "name": "Mixed Feed",
  "input_ids": ["cam-a", "mic-b"],
  "output_ids": ["udp-out", "srt-out"],
  "assembly": {
    "kind": "spts",
    "pcr_source": { "input_id": "cam-a", "pid": 256 },
    "programs": [
      {
        "program_number": 1,
        "service_name": "Mixed",
        "pmt_pid": 4096,
        "streams": [
          { "source": { "type": "pid", "input_id": "cam-a", "source_pid": 256 }, "out_pid": 256, "stream_type": 27,  "label": "Video (cam A)" },
          { "source": { "type": "pid", "input_id": "mic-b", "source_pid": 257 }, "out_pid": 257, "stream_type": 15,  "label": "Audio (mic B)" }
        ]
      }
    ]
  }
}
```

### Minimal MPTS example — two programs from three inputs

```json
"assembly": {
  "kind": "mpts",
  "pcr_source": { "input_id": "cam-a", "pid": 256 },
  "programs": [
    {
      "program_number": 1,
      "service_name": "Studio 1",
      "pmt_pid": 4096,
      "pcr_source": { "input_id": "cam-a", "pid": 256 },
      "streams": [
        { "source": { "type": "pid", "input_id": "cam-a", "source_pid": 256 }, "out_pid": 256, "stream_type": 27 },
        { "source": { "type": "essence", "input_id": "mic-a", "kind": "audio" }, "out_pid": 257, "stream_type": 15 }
      ]
    },
    {
      "program_number": 2,
      "service_name": "Studio 2",
      "pmt_pid": 4112,
      "pcr_source": { "input_id": "cam-b", "pid": 256 },
      "streams": [
        { "source": { "type": "pid", "input_id": "cam-b", "source_pid": 256 }, "out_pid": 272, "stream_type": 36 },
        { "source": { "type": "pid", "input_id": "cam-b", "source_pid": 257 }, "out_pid": 273, "stream_type": 15 }
      ]
    }
  ]
}
```

### Slot `source` — where the bytes come from

Every slot in a program's `streams[]` has a `source` picked from four variants:

- **`"pid"`** — explicit PID off a named input: `{ "type": "pid", "input_id": "...", "source_pid": 256 }`. Use when the operator knows the exact upstream PID (picked from the input's live PSI catalogue, or published in a written spec).
- **`"essence"`** — first ES of a given kind off a named input: `{ "type": "essence", "input_id": "...", "kind": "video" | "audio" | "subtitle" | "data" }`. Useful when the upstream input is single-program and the operator just wants "its video" / "its audio" without binding to a specific PID. Resolves at flow start against the input's PSI catalogue (Phase 2); re-resolves on `UpdateFlowAssembly`.
- **`"hitless"`** — primary-preference pre-bus merger: `{ "type": "hitless", "primary": { <slot source> }, "backup": { <slot source> } }`. Both legs publish onto the bus independently; a merger task forwards primary verbatim and flips to backup if no primary packet arrives for 200 ms. Primary traffic resumption brings the merger back after a short hold-off. **Not SMPTE 2022-7 sequence-aware dedup** — the bus today doesn't carry upstream RTP sequence numbers. Either nested leg must itself be `pid` or `essence`; a Hitless nested inside another Hitless is rejected.
- **`"switch"`** — operator-driven N-input switch (1..=64 legs): `{ "type": "switch", "legs": [ { "type": "pid"|"essence", "input_id": "...", ... } ], "initial_input_id": "..." }`. All legs subscribe concurrently (warm) so cutover is instant; the assembler forwards bytes only from the leg whose `input_id` matches the flow's currently-active input. The Switcher's `ActivateInput` (PGM/PVW/Take) flips every Switch slot whose leg list contains the named input — slots without that input as a leg are silent. **Output PIDs stay unified across switches** (the slot's fixed `out_pid`); PMT version bumps mod 32 and DI=1 fires on the next PCR for that `out_pid` so receivers stay locked without re-tuning. CC remains monotonic on `out_pid` because only the active leg writes packets through the rewriter. Legs are flat (`{ "type": "pid", ... }` or `{ "type": "essence", ... }`); Switch nested inside Hitless and Switch nested inside another Switch are both type-system rejected. The active leg survives flow restart via `flow.active_input_id`; if the saved active input is no longer in the leg list, the slot silently falls back to `initial_input_id`.

Example Switch slot — three-camera multicam bus on a single video PID:

```json
{
  "out_pid": 256,
  "stream_type": 27,
  "source": {
    "type": "switch",
    "legs": [
      { "type": "essence", "input_id": "cam-a", "kind": "video" },
      { "type": "essence", "input_id": "cam-b", "kind": "video" },
      { "type": "essence", "input_id": "cam-c", "kind": "video" }
    ],
    "initial_input_id": "cam-a"
  }
}
```

To drive it: build a Switcher preset for each camera (single `activate_input` action targeting this flow + that camera's input id) and Take. The Switcher integration is unchanged — preset shape `(node_id, flow_id, input_id)` is reused verbatim; only the edge-side semantics widen to flip Switch slot active legs in addition to the legacy passthrough behaviour.

### PCR rules

- **SPTS** — needs exactly one PCR reference. Either set `FlowAssembly.pcr_source` at the top of the assembly, or set `pcr_source` on the one program. If both are set, the program-level wins. The referenced `(input_id, pid)` must resolve to a concrete slot (or an Essence slot's input) in the program, otherwise flow bring-up emits `pid_bus_pcr_source_unresolved` and the flow fails to start.
- **MPTS** — every program needs an effective PCR — its own `pcr_source`, or the flow-level fallback. Validation emits an error at config-save time if any program has neither. Per-program PCR enforces the H.222.0 rule that a program's `PCR_PID` must be one of its own ES PIDs.
- The chosen PCR rides byte-for-byte onto the assembled TS; the synthesised PMT's `PCR_PID` field points at that slot's `out_pid`.

### Input requirements — what can feed the bus

Every input referenced by any slot must either already produce MPEG-TS on the broadcast channel, or be configured so the runtime can wrap it into TS before publishing to the bus.

**Inputs that produce TS natively (always eligible):**
SRT, UDP, RTP (with `is_raw_ts: true`), RIST, RTMP (after the built-in FLV→TS muxer), RTSP, WebRTC WHIP/WHEP, ST 2110-20, ST 2110-23, Bonded, TestPattern.

**PCM / AES3 inputs that become TS when `audio_encode` is set on the input:**
- ST 2110-30 (L16/L24 PCM) — set `audio_encode.codec` to `aac_lc` / `he_aac_v1` / `he_aac_v2` or `s302m`.
- `rtp_audio` — same codecs as ST 2110-30.
- ST 2110-31 (AES3 transparent) — **only** `s302m` is valid (validator enforces; the 337M sub-frames ride through the 302M wrap bit-for-bit).

Without `audio_encode` set, an assembly referencing one of these inputs fails bring-up with `pid_bus_spts_input_needs_audio_encode`.

**Inputs that have no current path to TS:**
- ST 2110-40 (ancillary data) — wrapping ANC into TS is deferred; referencing one emits `pid_bus_spts_non_ts_input`.

**Codec support on the decoded-ES cache** (what `audio_encode.codec` actually does at runtime): `aac_lc`, `he_aac_v1`, `he_aac_v2`, `s302m`. `mp2` and `ac3` parse and validate successfully but fail loudly at flow bring-up with `pid_bus_audio_encode_codec_not_supported_on_input` until the matching TsMuxer wrappers land.

### Validation rules (config-save + WS command time)

All rejected at `AppConfig::validate()` → `validate_flow_assembly()` with a clear `context:` error. None of these conditions can slip past to runtime:

- **Passthrough** must have empty `programs` and no `pcr_source`.
- **SPTS** must have exactly one program.
- **MPTS** must have at least one program, all with unique `program_number` and unique `pmt_pid`.
- Every referenced `input_id` must be in `flow.input_ids`.
- `program_number` must be `> 0` (0 is reserved for the NIT).
- `pmt_pid` and `out_pid` must be in `0x0010..=0x1FFE` (reserved PIDs and the NULL PID are refused).
- Within a program, every `out_pid` must be unique and must not equal that program's `pmt_pid`.
- `service_name` ≤ 128 chars; slot `label` ≤ 256 chars.
- SPTS: flow-level `pcr_source` or the one program's `pcr_source` must be set.
- MPTS: every program's effective `pcr_source` (own or flow-level fallback) must be set.
- When `pcr_source` resolves concretely, it must hit one of that program's slots (Pid match) or one of its Essence-slot inputs.
- **Hitless** nested inside a Hitless is rejected.
- **Switch** slot rules:
  - `legs` length 1..=64 (`pid_bus_switch_empty_legs` / `pid_bus_switch_too_many_legs`).
  - Every leg's `input_id` must be in `flow.input_ids`.
  - No two legs may share the same identity — `(input_id, source_pid)` for `pid` legs, `(input_id, kind)` for `essence` legs (`pid_bus_switch_duplicate_leg`).
  - `initial_input_id` must equal exactly one leg's `input_id` (`pid_bus_switch_initial_leg_unknown`).
  - When every leg is `essence`-typed, all `kind` values must agree (`pid_bus_switch_legs_kind_mismatch`).
  - Switch nested inside Hitless is rejected (`pid_bus_switch_nested_in_hitless`); Switch nested inside Switch is type-system impossible.
- Non-TS inputs without `audio_encode` (or with a non-runtime `audio_encode.codec`) are rejected at flow bring-up with a specific `pid_bus_*` error code (see Event reference).

### Runtime behaviour

- The assembler subscribes to the per-ES bus (`(input_id, source_pid) → EsPacket`), rewrites each 188-byte TS packet's PID to the configured `out_pid`, stamps a per-out-PID monotonic continuity counter, bundles 7 TS packets into MTU-safe 1316-byte RTP packets, and publishes them onto the flow's existing broadcast channel — exactly where a passthrough forwarder would.
- PAT and PMT are **synthesised** on a 100 ms cadence. When the PAT set changes, `PAT.version_number` bumps mod 32. When a program's slot composition or `pcr_source` changes, that program's `PMT.version_number` bumps mod 32 — both counters advance monotonically across swaps to avoid the phantom-version collision bug `TsContinuityFixer` already handles for passthrough switching.
- PCR rides onto the TS byte-for-byte from the referenced slot's source packets.
- A 10 ms safety-net flush keeps partially-filled bundles shipping during sparse periods (audio-only idle, keyframe gaps, etc.) so downstream sockets never see multi-second silence.
- Backpressure: slot fan-ins are `broadcast::Receiver<EsPacket>`. Slow consumers get `RecvError::Lagged(n)` and drop — the demuxer side never blocks.

### Runtime swaps (`UpdateFlowAssembly`)

The assembly plan is **hot-swappable**. A manager `UpdateFlowAssembly` WS command, or a direct `PUT /api/v1/flows/{flow_id}/assembly` REST call on the edge, replaces the running plan without tearing the flow down:

- Slots that are unchanged keep their existing bus fan-in tasks — no packet gap.
- Slots whose source, `out_pid`, `stream_type`, or `label` changed have their fan-ins re-spawned; fan-ins for removed slots are cancelled.
- Per-program `PMT.version_number` bumps for any program whose composition or PCR source changed.
- `PAT.version_number` bumps only when the set of programs changed (added / removed / renumbered).
- PSI is re-emitted immediately on swap so receivers see the new PMT before any packet lands on a new `out_pid` — prevents ffprobe from briefly seeing TS bytes on an unknown PID.
- Persists to `config.json` only after the swap succeeds. A no-op swap (new assembly deserialises byte-equal to current) is a silent short-circuit.
- Transitions across the passthrough boundary (passthrough ↔ spts/mpts) are **not hot-swappable** — those require a full `UpdateFlow` round-trip because the plumbing on the flow changes (bus + assembler spawn vs. direct broadcast).

### Interaction with output-level PID remap (`pid_map`)

Assembly owns the PID layout of the TS it produces (`out_pid` per slot, `pmt_pid` per program, whatever the PCR slot got assigned). An output's `pid_map` (see [TS output PID remapping](#ts-output-pid-remapping-pid_map)) applies **after** the assembly on the way out, so you can publish one assembled PID layout and then re-label it per output if an external downstream has hard-coded PID expectations. Not recommended as the default path — pick `out_pid` values that already match downstream expectations in the assembly.

### Monitoring

Once running, an assembled flow exposes:

- **Flow-card badge** — `SPTS ASSEMBLED` / `MPTS ASSEMBLED` (cyan) in the manager UI.
- **Assembled Output section** on the flow card — one sub-table per program listing each slot's `out_pid`, `stream_type`, resolved `kind`, source label (or Hitless(A/B)), live bitrate, packets, CC errors, PCR discontinuity counters from `FlowStats.per_es[]`.
- **Per-output PCR trust** — `p50 / p99` columns on the Outputs table, fed by `OutputStats.pcr_trust`.
- **Flow-rollup PCR trust** — `FlowStats.pcr_trust_flow` (Samples, p50 / p95 / p99 / Max / Window p95) rendered at the bottom of the flow card.
- **Events** — every `pid_bus_*` error code (see [Event reference](events-and-alarms.md)) rides as a Critical event with structured `details` (`error_code`, `input_id`, `input_type`, `program_number`, ...) so the manager UI can highlight the offending form field on Create/Update modals without parsing the error string.

---

## MPTS → SPTS filtering

All outputs — and the thumbnail generator — accept an optional `program_number` selector for down-selecting an MPTS (Multi-Program Transport Stream) input to a single program. Whether the filter rewrites TS bytes or just picks which elementary streams to extract depends on the output type.

### Behaviour matrix

| Output | `program_number = null` (default) | `program_number = N` |
|--------|-----------------------------------|----------------------|
| **UDP / RTP / SRT / HLS** (TS-native) | full MPTS passthrough (current behaviour) | PAT rewritten to a single-program form; only program N's PMT, ES, and PCR PIDs survive. FEC (2022-1) and hitless redundancy (2022-7) operate on the filtered bytes. |
| **RTMP / WebRTC** (re-muxing) | lock onto the lowest `program_number` in the PAT (deterministic — replaces the old "first PMT seen" race) | extract elementary streams from program N's PMT only |
| **Thumbnail generator** (`thumbnail_program_number` on `FlowConfig`) | ffmpeg picks the first program it finds | TS is pre-filtered so ffmpeg only sees program N |

### Rules

- **`program_number` is per-output.** One flow can run three outputs in parallel — one forwarding full MPTS to an archive, one filtered to program 1, and another to program 2 — all sharing the same broadcast channel.
- **`program_number = 0` is rejected** at config load and on manager commands. Program number 0 is reserved for the NIT in the MPEG-TS specification and never identifies a real program.
- **Disappearing programs** (selected program not in the PAT, or a PAT version bump removes it): the output emits nothing until the program reappears. The filter automatically recovers on the next PAT that re-advertises the target.
- **SPTS inputs** are unaffected — there's only one program, so `program_number = 1` (or whatever it is) filters to the same stream that was already there.

### Example — 2-program MPTS fanning out to three destinations

```json
{
  "inputs": [
    {
      "id": "mpts-in", "name": "MPTS Source",
      "type": "udp", "bind_addr": "0.0.0.0:5020"
    }
  ],
  "outputs": [
    {
      "type": "udp", "id": "archive", "name": "Archive full MPTS",
      "dest_addr": "10.0.0.5:6000"
    },
    {
      "type": "udp", "id": "prog1-viewer", "name": "Program 1 → ffplay",
      "dest_addr": "127.0.0.1:6001",
      "program_number": 1
    },
    {
      "type": "rtmp", "id": "prog2-rtmp", "name": "Program 2 → CDN",
      "dest_url": "rtmp://live.example.com/app",
      "stream_key": "my-key",
      "program_number": 2
    }
  ],
  "flows": [
    {
      "id": "mpts-flow", "name": "Dual-program feed",
      "thumbnail_program_number": 1,
      "input_ids": ["mpts-in"],
      "output_ids": ["archive", "prog1-viewer", "prog2-rtmp"]
    }
  ]
}
```

The archive receives the full MPTS. The `prog1-viewer` UDP output sends only program 1 as a rewritten SPTS (PAT lists one entry, program 1's PMT + ES PIDs). The RTMP output publishes program 2's elementary streams. The manager UI thumbnail shows a frame from program 1.

---

## TS output PID remapping (`pid_map`)

Every TS-carrying output (`udp`, `rtp`, `srt`, `rist`, `hls`, `bonded`) accepts an optional `pid_map` that rewrites PIDs on the way out. Each entry is `source → target`; the PAT / PMT CRCs are recomputed so downstream decoders see a consistent stream. Use when a downstream system has hard-coded PID expectations that don't match the upstream layout (or the assembly's `out_pid` values).

```json
{
  "id": "srt-to-legacy",
  "type": "srt",
  "mode": "caller",
  "remote_addr": "203.0.113.10:9000",
  "pid_map": { "256": 2064, "257": 2068, "4096": 1001 }
}
```

### Rules (validated at config load and on WS command)

- Maximum 256 entries.
- Source and target PIDs must be in `0x0010..=0x1FFE` (reserved PIDs 0x0000–0x000F, PAT/CAT, and the NULL PID 0x1FFF are refused).
- A source PID cannot map to itself (no-ops are rejected to keep intent explicit).
- Source PIDs must be unique; target PIDs must be unique — so two different sources can never collide on the wire.
- Applies to the whole TS stream on that output, including PSI PIDs. If you remap a PMT PID, set the `pmt_pid` on the corresponding assembly program to the *source* value — the `pid_map` rewrites it on egress.
- Works equally on passthrough flows (rewrites upstream PIDs) and assembled flows (rewrites the assembly's `out_pid` values). For assembled flows, prefer picking the right `out_pid` in the assembly — `pid_map` is an escape hatch for downstream constraints you can't change.

---

## SMPTE 2022-1 FEC Configuration

Forward Error Correction parameters used by `fec_decode` (on RTP inputs) and `fec_encode` (on RTP outputs).

```json
{
  "columns": 10,
  "rows": 10
}
```

| Field | Type | Required | Range | Description |
|-------|------|----------|-------|-------------|
| `columns` | integer | Yes | 1-20 | L parameter: number of columns in the FEC matrix. |
| `rows` | integer | Yes | 4-20 | D parameter: number of rows in the FEC matrix. |

The FEC matrix protects `columns x rows` media packets with `columns + rows` parity packets. Larger matrices provide better protection at the cost of higher latency and bandwidth overhead.

Common configurations:
- `5 x 5` -- Low overhead, moderate protection
- `10 x 10` -- Good balance of overhead and protection
- `20 x 20` -- Maximum protection, higher latency

---

## SMPTE 2022-7 SRT Redundancy

Both SRT input and SRT output support SMPTE 2022-7 hitless redundancy via a second SRT leg. The parent SRT config defines leg 1; the `redundancy` block defines leg 2.

For input: packets from both legs are merged using RTP sequence numbers, providing seamless failover if one path fails.

For output: packets are duplicated and sent on both legs simultaneously.

```json
{
  "redundancy": {
    "mode": "listener",
    "local_addr": "0.0.0.0:9001",
    "remote_addr": null,
    "latency_ms": 500,
    "passphrase": "encryption-key",
    "aes_key_len": 32
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `mode` | string | Yes | - | SRT mode for leg 2: `"caller"`, `"listener"`, or `"rendezvous"`. |
| `local_addr` | string | Yes | - | Local bind address for leg 2. |
| `remote_addr` | string | Conditional | `null` | Remote address for leg 2 (required for caller/rendezvous). |
| `latency_ms` | integer | No | `120` | SRT latency for leg 2. |
| `passphrase` | string | No | `null` | AES encryption passphrase for leg 2 (10-79 characters). |
| `aes_key_len` | integer | No | `16` | AES key length for leg 2 (16, 24, or 32). |
| `crypto_mode` | string | No | `null` | Cipher mode for leg 2: `"aes-ctr"` or `"aes-gcm"`. |

Legs can use different SRT modes, different ports, different latency values, and even different encryption settings (though using the same settings is recommended for simplicity).

### Sender pacing (`max_bw`) under 2022-7

When the edge is the **SRT sender** on a 2022-7 pair, both legs share the same process, the same `srt-io` I/O thread, and the same upstream packet stream — so any sender-side drop correlates across legs and the hitless merger has no other copy to recover from. libsrt's live-mode *send pacer* (enabled when `max_bw = 0`) uses an internal input-bandwidth estimator that is conservative during the first ~1 second of a new session. A bursty upstream (`ffmpeg -re` file read, a camera emptying a kernel buffer on session start, an RTSP source after reconnection) can outrun that estimator and cause libsrt to drop packets from its own send buffer past `send_drop_delay` — the receiver never sees them, logs `RCV-DROPPED N packet(s). Packet seqno %X delayed for ~700 ms`, and with FEC on top the gap can exceed the FEC matrix and trip libsrt's `SRT.pf: FEC: IPE` internal program error on the receiver.

Current bilbycast-edge defaults `max_bw = -1` (unlimited send pacing) in the libsrt wrapper (`bilbycast-libsrt-rs`). This is the right setting for a forwarding gateway, where the upstream already paces correctly and libsrt adding its own pacing on top only creates warm-up drops. If you are operating on a shared WAN link and need an explicit per-link bitrate cap, set `max_bw` on the SRT endpoint config (both legs individually). Do **not** leave legs on the old libsrt default of `0` under 2022-7 — it is the single most common cause of correlated startup loss on a dual-leg bonded/redundant SRT pair.

### Raw TS 2022-7: dedup is ordinal, not content-based

When the upstream payload is **raw MPEG-TS** (no RTP header), the 2022-7 merger cannot key on an RTP sequence number and falls back to a per-leg synthetic counter. This is fine in the common case — both SRT legs deliver packets to the Tokio side in the same order (TSBPD enforces that), so the two counters stay aligned and dedup works. The counters only drift if **one leg permanently loses a packet** (past `latency + send_drop_delay`) that the other leg delivers. From that point on, the two legs' Nth-packet-ever are different content, so dedup fails and duplicate TS packets reach the downstream muxer (visible as `non monotonically increasing dts` in a downstream ffmpeg and macroblock decode errors with zero upstream loss).

Under FEC this is usually a non-issue — FEC recovers single-packet losses on each leg before TSBPD, so legs don't diverge. Under heavy asymmetric per-link loss that exceeds the FEC matrix on one leg only, the counter drift can surface. If you are running raw-TS 2022-7 over lossy asymmetric links and want deterministic dedup, wrap the upstream in RTP/TS so the merger can key on the real RTP sequence number.

---

## Native libsrt SRT Bonding (Socket Groups)

SRT inputs and outputs also support **native libsrt socket-group bonding** via an inline `bonding` block. Unlike `redundancy` (two independent SRT sessions merged at the app layer), bonding is the libsrt wire protocol — the peer sees a single bonded session and speaks the group handshake, making it interoperable with `srt-live-transmit grp:BROADCAST://` / `grp:BACKUP://`, Haivision socket groups, and any other libsrt peer.

```json
{
  "type": "srt",
  "mode": "caller",
  "latency_ms": 200,
  "bonding": {
    "mode": "broadcast",
    "endpoints": [
      { "addr": "203.0.113.10:9000", "local_addr": "192.168.1.2:0" },
      { "addr": "203.0.113.11:9000", "local_addr": "192.168.2.2:0" }
    ]
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `mode` | string | Yes | `"broadcast"` (all members active, libsrt dedups) or `"backup"` (primary/failover). |
| `endpoints` | array | Yes | 2–8 member entries. |
| `endpoints[].addr` | string | Yes | Caller mode: remote peer address. Listener mode: the list is advisory — the parent `local_addr` is the single bind (libsrt group handshake is multiplexed on one listener). |
| `endpoints[].local_addr` | string | No | Caller-only: source bind for this member (use to pin each leg to a different NIC). |
| `endpoints[].weight` | integer | No | Backup-mode priority; **lower is preferred**. Ignored in broadcast. Default 0 = equal. |

**Rules:**
- `bonding` and `redundancy` are **mutually exclusive** on the same SRT input/output — bonding replaces the app-layer 2022-7 with the wire-native group handshake.
- Bonding is **only supported on the libsrt backend**. The pure-Rust `bilbycast-srt` backend does not expose socket groups.
- Bonded outputs compose with every other per-output stage: `program_number`, `audio_encode` (with optional companion `transcode`), `video_encode`, output `delay`, and `transport_mode: "audio_302m"`. The bonded path reuses the same forward-loop pipeline as the single-socket path — the only change is that packets land on `SrtGroup::send()` (caller mode) or on the accepted group-aware listener socket.
- Every SRT option on the parent (latency, encryption, stream_id, packet_filter, MSS, payload_size, retransmit algo, etc.) applies to **all** members uniformly.
- `rendezvous` mode does not support bonding (libsrt's group handshake has no rendezvous variant). Validation rejects the combination.

**Per-leg stats** are surfaced via `srt_bonding_stats` on the input/output snapshot:
```json
{
  "srt_bonding_stats": {
    "mode": "broadcast",
    "aggregate": { "state": "connected", "rtt_ms": 18.2, "recv_rate_mbps": 9.7, ... },
    "members": [
      { "endpoint": "203.0.113.10:9000", "socket_status": "connected",
        "member_status": "running", "weight": 0, "stats": { ... } },
      { "endpoint": "203.0.113.11:9000", "socket_status": "connected",
        "member_status": "running", "weight": 0, "stats": { ... } }
    ]
  }
}
```

`member_status` values: `running` (active), `idle` (standby backup), `pending` (negotiating), `broken`.

**Testing interop:** see `testbed/flow-groups/srt-bonding.sh` (`./testbed/flows.sh srt-bonding`). Drives `srt-live-transmit` group callers against bonded edge listeners and vice versa.

---

## SRT Connection Modes

| Mode | Initiator | `remote_addr` required | Use case |
|------|-----------|----------------------|----------|
| `caller` | This endpoint connects to a remote listener | Yes | Sending to a known destination. Most common for outputs. |
| `listener` | This endpoint waits for incoming connections | No | Accepting streams from remote callers. Most common for inputs (ingest servers). |
| `rendezvous` | Both sides connect simultaneously | Yes | NAT traversal. Both sides must use rendezvous mode and know each other's address. |

### `local_addr` semantics by mode

`local_addr` means different things depending on `mode`:

- **listener / rendezvous** — the **listen address** the endpoint binds (required).
- **caller** — the **source socket** the endpoint binds *before* connecting to `remote_addr` (bind-then-connect). It is **not** the destination. Leave it unset / `"0.0.0.0:0"` so the OS picks an ephemeral source port; pin it only when you must source from a specific interface/port (a firewall pinhole, multi-homed host). `interface_binding` resolves to a pinned source IP with an ephemeral port automatically.

**Co-locating a caller with an egress tunnel.** A common topology sends an SRT *output* (caller) into a co-located IP tunnel: the tunnel's egress leg binds e.g. `0.0.0.0:9001` and the SRT output's `remote_addr` is `127.0.0.1:9001`. In that setup leave the SRT output's `local_addr` ephemeral — do **not** set it to `127.0.0.1:9001`. Pinning the caller's *source* port to the tunnel's port makes the caller bind a port the tunnel already owns (and, if also equal to `remote_addr`, dial itself), so the stream never establishes — the edge logs `SRT caller connecting 127.0.0.1:9001 -> 127.0.0.1:9001` and the output sits at `bitrate_bps: 0`. Validation now rejects `local_addr == remote_addr` for callers and flags a pinned `local_addr` that collides with a tunnel (or any other bind) as `port_conflict`.

---

## CLI Argument Overrides

Command-line arguments override values from the config file. This is useful for deployment automation and containerization.

```
bilbycast-edge [OPTIONS]

Options:
  -c, --config <PATH>          Path to configuration file [default: ./config.json]
  -p, --port <PORT>            Override API listen port
  -b, --bind <ADDRESS>         Override API listen address (legacy single-address)
      --bind-addrs <ADDRS>     Override API dual-stack listeners, comma-separated
                               (e.g. 0.0.0.0,[::]); outranks --bind
      --monitor-port <PORT>    Override monitor dashboard port
  -l, --log-level <LEVEL>      Log level: trace, debug, info, warn, error [default: info]
      --print-setup-token      Print the one-shot /setup bearer token and exit
      --print-capabilities     Print compiled-in features + advertised capabilities and exit
  -h, --help                   Print help
  -V, --version                Print version
```

| Argument | Config field overridden | Example |
|----------|----------------------|---------|
| `--port` | `server.listen_port` | `--port 9443` |
| `--bind` | `server.listen_addr` (legacy single address) | `--bind 127.0.0.1` |
| `--bind-addrs` | `server.listen_addrs` — **takes precedence over `--bind` and over both config fields** | `--bind-addrs 0.0.0.0,[::]` |
| `--monitor-port` | `monitor.listen_port` | `--monitor-port 9091` |
| `--log-level` | (runtime only, not in config) | `--log-level debug` |
| `--print-setup-token` | (none — reads `secrets.json`, prints, exits) | `--print-setup-token` |
| `--print-capabilities` | (none — loads no config, opens no socket, runs no probe) | `--print-capabilities` |

**`--print-capabilities`** answers "what is actually in this binary?". It
prints one token per line and exits before config load, socket bind and every
boot probe, so it is safe on a build runner or a headless host — the release
workflow's `Verify binary` step depends on exactly that. It prints two
different truths:

- `feature <name>` — a Cargo feature **compiled in**. This is the question a
  release-artefact assertion has to ask, and the reason the flag exists: every
  `*-full` binary before v0.103.0 shipped without SDI while its notes claimed
  otherwise, and every one of those builds was green.
- `capability <name>` — the capability list evaluated **cold**. Probe-gated
  bits (`display`, `sdi-decklink`, the `video-decoder-*` set) are absent here
  even when compiled in, because their boot probe has not run. The live list a
  node advertises is on its health tick.

The log level can also be set via the `RUST_LOG` environment variable, which takes precedence over the `--log-level` argument when set. Supports fine-grained filtering (e.g., `RUST_LOG=bilbycast_edge=debug,tower_http=info`).

**Examples:**

```bash
# Use a specific config file
bilbycast-edge --config /etc/bilbycast/production.json

# Override port for containerized deployment
bilbycast-edge --config config.json --port 443 --bind 0.0.0.0

# Debug logging
bilbycast-edge --config config.json --log-level debug

# Fine-grained logging via environment
RUST_LOG=bilbycast_edge=debug,tower_http=info bilbycast-edge --config config.json
```

---

## Config Persistence Behavior

bilbycast-edge automatically persists configuration changes to disk when inputs, outputs, or flows are modified through the API. Operational config (including user parameters like SRT passphrases, RTSP credentials, RTMP keys) goes to `config.json`, infrastructure secrets go to `secrets.json`:

- **Create/Update/Delete input** (`POST/PUT/DELETE /api/v1/inputs[/{id}]`) -- Modifies the top-level `inputs` array and saves.
- **Create/Update/Delete output** (`POST/PUT/DELETE /api/v1/outputs[/{id}]`) -- Modifies the top-level `outputs` array and saves.
- **Create flow** (`POST /api/v1/flows`) -- Appends the new flow and saves.
- **Update flow** (`PUT /api/v1/flows/{id}`) -- Replaces the flow in-place and saves.
- **Delete flow** (`DELETE /api/v1/flows/{id}`) -- Removes the flow and saves.
- **Add output** (`POST /api/v1/flows/{id}/outputs`) -- Assigns an existing output to the flow by ID and saves.
- **Remove output** (`DELETE /api/v1/flows/{id}/outputs/{oid}`) -- Unassigns the output from the flow and saves.
- **Replace config** (`PUT /api/v1/config`) -- Replaces the entire config and saves.
- **Get config** (`GET /api/v1/config`) -- Returns the config with infrastructure secrets stripped. User parameters (passphrases, credentials, keys) are included in the response.

### Atomic writes

All config saves use an atomic write strategy: both `config.json` and `secrets.json` are written to temporary files (`.json.tmp`), then atomically renamed to the target paths. This prevents corruption if the process is interrupted during a write. `secrets.json` is written with `0600` permissions (owner-only) on Unix.

### Default config

If the config file does not exist when bilbycast-edge starts, an empty default configuration is used:

```json
{
  "version": 2,
  "server": {
    "listen_addr": "127.0.0.1",
    "listen_addrs": ["127.0.0.1", "[::1]"],
    "listen_port": 8080
  },
  "inputs": [],
  "outputs": [],
  "flows": []
}
```

A fresh node is therefore reachable **only from the host itself** until you set
`server.listen_addrs` (or start it with `--bind-addrs 0.0.0.0,[::]`). That is
deliberate — see [Server Configuration](#server-configuration). It does not
affect managing the node: the manager link is an outbound WebSocket from the
edge.

### Reloading from disk

Use `POST /api/v1/config/reload` to re-read both `config.json` and `secrets.json` from disk. This is useful after manual edits or after deploying new config files via external tooling (e.g., Ansible, Chef).

---

## Common Configuration Scenarios

### Minimal: RTP receive and forward (no auth)

```json
{
  "version": 2,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "inputs": [
    {
      "id": "rtp-in",
      "name": "RTP Receive",
      "type": "rtp",
      "bind_addr": "0.0.0.0:5000"
    }
  ],
  "outputs": [
    {
      "type": "rtp",
      "id": "out-1",
      "name": "Forwarded Output",
      "dest_addr": "192.168.1.50:5004"
    }
  ],
  "flows": [
    {
      "id": "passthrough",
      "name": "RTP Passthrough",
      "enabled": true,
      "input_ids": ["rtp-in"],
      "output_ids": ["out-1"]
    }
  ]
}
```

### Multicast receive with FEC and trust boundary filters

```json
{
  "version": 2,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "inputs": [
    {
      "id": "mcast-in",
      "name": "Multicast with FEC and Trust Boundary",
      "type": "rtp",
      "bind_addr": "239.1.1.1:5000",
      "interface_addr": "10.0.0.100",
      "fec_decode": {
        "columns": 10,
        "rows": 10
      },
      "allowed_sources": ["10.0.0.1"],
      "allowed_payload_types": [33],
      "max_bitrate_mbps": 50.0,
      "tr07_mode": true
    }
  ],
  "outputs": [
    {
      "type": "rtp",
      "id": "local-out",
      "name": "Local Multicast Output",
      "dest_addr": "239.1.2.1:5004",
      "interface_addr": "10.0.0.100",
      "fec_encode": {
        "columns": 10,
        "rows": 10
      },
      "dscp": 46
    }
  ],
  "flows": [
    {
      "id": "multicast-feed",
      "name": "Multicast with FEC and Trust Boundary",
      "enabled": true,
      "input_ids": ["mcast-in"],
      "output_ids": ["local-out"]
    }
  ]
}
```

### SRT bidirectional with 2022-7 redundancy

```json
{
  "version": 2,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "inputs": [
    {
      "id": "srt-in",
      "name": "SRT Redundant Input",
      "type": "srt",
      "mode": "listener",
      "local_addr": "0.0.0.0:9000",
      "latency_ms": 500,
      "passphrase": "my-secure-passphrase-1234",
      "aes_key_len": 32,
      "redundancy": {
        "mode": "listener",
        "local_addr": "0.0.0.0:9001",
        "latency_ms": 500,
        "passphrase": "my-secure-passphrase-1234",
        "aes_key_len": 32
      }
    }
  ],
  "outputs": [
    {
      "type": "srt",
      "id": "srt-out",
      "name": "SRT Redundant Output",
      "mode": "caller",
      "local_addr": "0.0.0.0:0",
      "remote_addr": "203.0.113.10:9000",
      "latency_ms": 500,
      "passphrase": "output-passphrase-1234567",
      "aes_key_len": 32,
      "redundancy": {
        "mode": "caller",
        "local_addr": "0.0.0.0:0",
        "remote_addr": "203.0.113.11:9000",
        "latency_ms": 500,
        "passphrase": "output-passphrase-1234567",
        "aes_key_len": 32
      }
    }
  ],
  "flows": [
    {
      "id": "srt-redundant",
      "name": "SRT with Hitless Redundancy",
      "enabled": true,
      "input_ids": ["srt-in"],
      "output_ids": ["srt-out"]
    }
  ]
}
```

### Multi-output: RTP to SRT, RTMP, and HLS simultaneously

```json
{
  "version": 2,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "inputs": [
    {
      "id": "mcast-in",
      "name": "Multicast Source",
      "type": "rtp",
      "bind_addr": "239.1.1.1:5000",
      "interface_addr": "192.168.1.100"
    }
  ],
  "outputs": [
    {
      "type": "rtp",
      "id": "local",
      "name": "Local Playout",
      "dest_addr": "192.168.1.50:5004"
    },
    {
      "type": "srt",
      "id": "remote-srt",
      "name": "Remote Site SRT",
      "mode": "caller",
      "local_addr": "0.0.0.0:0",
      "remote_addr": "203.0.113.10:9000",
      "latency_ms": 300
    },
    {
      "type": "rtmp",
      "id": "twitch",
      "name": "Twitch",
      "dest_url": "rtmp://live.twitch.tv/app",
      "stream_key": "live_xxxxxxxxxxxx"
    },
    {
      "type": "hls",
      "id": "youtube-hls",
      "name": "YouTube HLS",
      "ingest_url": "https://a.upload.youtube.com/http_upload_hls?cid=xxxx",
      "segment_duration_secs": 2.0
    }
  ],
  "flows": [
    {
      "id": "multi-output",
      "name": "Multi-Output Fan-Out",
      "enabled": true,
      "input_ids": ["mcast-in"],
      "output_ids": ["local", "remote-srt", "twitch", "youtube-hls"]
    }
  ]
}
```

### Full production config with TLS + auth + monitoring

```json
{
  "version": 2,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8443,
    "tls": {
      "cert_path": "/etc/bilbycast/cert.pem",
      "key_path": "/etc/bilbycast/key.pem"
    },
    "auth": {
      "enabled": true,
      "jwt_secret": "K7nXp2qR8vF3mBwYd0hL5jZ1tA6gCeHsN9uIoP4xWkQrJfMaVbDcEiGyTlUwSzO",
      "token_lifetime_secs": 3600,
      "public_metrics": true,
      "nmos_require_auth": true,
      "token_rate_limit_per_minute": 10,
      "clients": [
        {
          "client_id": "ops-admin",
          "client_secret": "admin-secret-change-me",
          "role": "admin"
        },
        {
          "client_id": "grafana",
          "client_secret": "grafana-read-secret",
          "role": "monitor"
        }
      ]
    }
  },
  "monitor": {
    "listen_addr": "0.0.0.0",
    "listen_port": 9090
  },
  "inputs": [
    {
      "id": "rtp-in",
      "name": "Main RTP Input",
      "type": "rtp",
      "bind_addr": "239.1.1.1:5000",
      "interface_addr": "10.0.0.100",
      "fec_decode": {
        "columns": 10,
        "rows": 10
      }
    }
  ],
  "outputs": [
    {
      "type": "rtp",
      "id": "local-playout",
      "name": "Local Playout",
      "dest_addr": "10.0.0.50:5004",
      "dscp": 46
    },
    {
      "type": "srt",
      "id": "remote-site",
      "name": "Remote Site",
      "mode": "caller",
      "local_addr": "0.0.0.0:0",
      "remote_addr": "203.0.113.10:9000",
      "latency_ms": 500,
      "passphrase": "secure-transport-key-1234",
      "aes_key_len": 32
    }
  ],
  "flows": [
    {
      "id": "main-feed",
      "name": "Main Program Feed",
      "enabled": true,
      "input_ids": ["rtp-in"],
      "output_ids": ["local-playout", "remote-site"]
    }
  ]
}
```

### IPv6 multicast configuration

```json
{
  "version": 2,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "inputs": [
    {
      "id": "ipv6-in",
      "name": "IPv6 Multicast Input",
      "type": "rtp",
      "bind_addr": "[ff7e::1]:5000",
      "interface_addr": "::1"
    }
  ],
  "outputs": [
    {
      "type": "rtp",
      "id": "ipv6-out",
      "name": "IPv6 Output",
      "dest_addr": "[ff7e::2]:5004",
      "interface_addr": "::1"
    }
  ],
  "flows": [
    {
      "id": "ipv6-mcast",
      "name": "IPv6 Multicast Flow",
      "enabled": true,
      "input_ids": ["ipv6-in"],
      "output_ids": ["ipv6-out"]
    }
  ]
}
```

## SMPTE ST 2110

bilbycast-edge supports the broadcast-audio, broadcast-data, and
uncompressed-video subsets of SMPTE ST 2110:

- **Phase 1** (audio / data):
  - **ST 2110-30** — linear PCM L16/L24.
  - **ST 2110-31** — AES3 transparent for Dolby E and similar.
  - **ST 2110-40** — RFC 8331 ancillary data including SCTE-104,
    SMPTE 12M timecode, CEA-608/708 captions.
- **Phase 2** (uncompressed video):
  - **ST 2110-20** — RFC 4175 uncompressed video. Inputs decode from
    the wire and encode into H.264/HEVC MPEG-TS via an in-process
    encoder (`x264`/`x265`/`h264_nvenc`/`hevc_nvenc`/`h264_qsv`/`hevc_qsv`); outputs decode
    the flow's source TS and RFC 4175-packetize onto the wire. Pixel
    formats: **YCbCr-4:2:2 at 8-bit and 10-bit** (other formats are
    rejected by validation). Requires a `video-encoder-*` feature at
    build time for inputs; the `media-codecs` feature (default on)
    is enough for outputs.
  - **ST 2110-23** — a single uncompressed video essence split across
    N ST 2110-20 sub-streams. Partition modes: `two_sample_interleave`
    (2SI) and `sample_row`.
- **Deferred**: ST 2110-22 (JPEG XS) is not yet supported; integration
  is tracked under Phase 2 follow-up work.

PTP integration is best-effort and reads from an external `ptp4l`
daemon's management Unix socket — no PTP daemon ships in the edge. SMPTE
2022-7 Red/Blue dual-network operation is opt-in via the `redundancy`
block on each ST 2110 input/output. The full architecture and NIC list
live in [`docs/st2110.md`](st2110.md); the NMOS surface area
(IS-04/IS-05/IS-08, BCP-004, mDNS-SD) is documented in
[`docs/nmos.md`](nmos.md).

### Flow-level fields

| Field | Type | Required | Purpose |
|-------|------|----------|---------|
| `clock_domain` | u8 | No | IEEE 1588 PTP domain (0–127). Setting this on a flow makes the edge spawn a `PtpStateReporter` and surface lock state through `FlowStats.ptp_state`. |
| `flow_group_id` | string | No | Logical bundle id; multiple essence flows on a single edge can share a group so the manager treats them as one unit. |

Both fields are optional and backward-compatible — existing configs
deserialize unchanged.

### ST 2110-30 / -31 audio input

```json
{
  "id": "st2110-30-in",
  "name": "Studio A — stereo",
  "type": "st2110_30",
  "bind_addr": "239.0.0.10:5000",
  "interface_addr": "10.0.0.5",
  "sample_rate": 48000,
  "bit_depth": 24,
  "channels": 2,
  "packet_time_us": 1000,
  "payload_type": 97,
  "redundancy": {
    "addr": "239.1.0.10:5000",
    "interface_addr": "10.1.0.5"
  }
}
```

A flow referencing this input would set `clock_domain` and `flow_group_id` at the flow level:

```json
{
  "id": "studio-a-stereo",
  "name": "Studio A — stereo",
  "enabled": true,
  "clock_domain": 0,
  "input_ids": ["st2110-30-in"],
  "output_ids": []
}
```

`type: "st2110_31"` uses an identical struct — only the depacketizer
label changes. AES3 transparency preserves user bits, channel status,
validity, and parity.

### ST 2110-30 / -31 audio output

```json
{
  "type": "st2110_30",
  "id": "monitor-out",
  "name": "Loopback to monitor",
  "dest_addr": "239.2.0.10:5000",
  "interface_addr": "10.0.0.5",
  "dscp": 46,
  "sample_rate": 48000,
  "bit_depth": 24,
  "channels": 2,
  "packet_time_us": 1000,
  "payload_type": 97,
  "redundancy": {
    "addr": "239.3.0.10:5000",
    "interface_addr": "10.1.0.5"
  }
}
```

#### Optional `transcode` block

Every audio output (`st2110_30`, `st2110_31`, `rtp_audio`) accepts an
optional `transcode` field for per-output sample-rate / bit-depth /
channel-routing conversion. The transcoder runs lock-free between the
broadcast subscriber and the RTP send loop, and is invisible when
omitted (the existing byte-identical passthrough path runs unchanged).

```json
{
  "type": "st2110_30",
  "id": "monitoring-stereo",
  "name": "Surround → stereo monitor",
  "dest_addr": "239.2.0.10:5000",
  "sample_rate": 48000,
  "bit_depth": 24,
  "channels": 2,
  "packet_time_us": 1000,
  "payload_type": 97,
  "transcode": {
    "channels": 2,
    "channel_map_preset": "5_1_to_stereo_bs775"
  }
}
```

The full feature set, all six channel-routing presets, IS-08 hot
reload, validation rules, and worked use cases (monitoring downmix,
WAN contribution, talkback, third-party SRT decoder interop) live in
**[`audio-gateway.md`](audio-gateway.md)** — read that for the deep
dive.

### `rtp_audio` input/output (no PTP, generic PCM/RTP)

`rtp_audio` is wire-identical to ST 2110-30 but with no PTP requirement,
no NMOS `clock_domain` advertising, and a relaxed sample-rate set
(32 / 44.1 / 48 / 88.2 / 96 kHz). Use it for radio contribution feeds
over the public internet, talkback between studios that don't share a
PTP fabric, and ffmpeg / OBS / GStreamer interop.

Input definition in the top-level `inputs` array:

```json
{
  "id": "perth-receive-in",
  "name": "Sydney → Perth contribution receiver",
  "type": "rtp_audio",
  "bind_addr": "0.0.0.0:5004",
  "sample_rate": 48000,
  "bit_depth": 24,
  "channels": 2,
  "packet_time_us": 4000,
  "payload_type": 97
}
```

Output definition in the top-level `outputs` array:

```json
{
  "type": "st2110_30",
  "id": "perth-monitor",
  "name": "Perth monitor multicast",
  "dest_addr": "239.20.0.10:5004",
  "sample_rate": 44100,
  "bit_depth": 16,
  "channels": 2,
  "packet_time_us": 1000,
  "payload_type": 97,
  "transcode": {
    "sample_rate": 44100,
    "bit_depth": 16,
    "channels": 2
  }
}
```

Flow wiring them together:

```json
{
  "id": "perth-receive",
  "name": "Sydney → Perth contribution",
  "input_ids": ["perth-receive-in"],
  "output_ids": ["perth-monitor"]
}
```

`rtp_audio` outputs share the `transcode` block exactly with ST 2110-30
outputs and additionally support `transport_mode: "audio_302m"` to
emit SMPTE 302M LPCM in MPEG-TS wrapped as RFC 2250 RTP/MP2T
(payload type 33).

### SMPTE 302M LPCM in MPEG-TS over SRT / UDP / RTP-MP2T

SRT, UDP, and `rtp_audio` outputs accept an optional
`transport_mode: "audio_302m"` field. When set (and the upstream input
is an audio essence), Bilbycast runs the per-output transcode + 302M
packetizer + TsMuxer pipeline and emits 7×188-byte MPEG-TS chunks
over the chosen transport. This is the standard broadcast contribution
format for lossless audio, interoperable with `ffmpeg -c:a s302m`,
`srt-live-transmit`, and broadcast hardware decoders that consume
SMPTE 302M LPCM in MPEG-TS.

Example: send a stereo AES67 feed to a third-party SRT decoder as 302M:

```json
{
  "type": "srt",
  "id": "playout-feed",
  "name": "Studio → playout decoder",
  "mode": "caller",
  "local_addr": "0.0.0.0:0",
  "remote_addr": "playout-decoder.example.com:9000",
  "latency_ms": 200,
  "transport_mode": "audio_302m"
}
```

The `audio_302m` mode is mutually exclusive with `packet_filter` (FEC),
`program_number`, and SMPTE 2022-7 redundancy on SRT — the validator
rejects all three combinations at config load time. See
[`audio-gateway.md`](audio-gateway.md) for the full pipeline,
constraint rationale, and the four runnable interop test scripts in
`testbed/audio-tests/302m-interop/`.

### ST 2110-40 ancillary input/output

Input in the top-level `inputs` array:

```json
{
  "id": "anc-in",
  "name": "ANC input (timecode + SCTE-104)",
  "type": "st2110_40",
  "bind_addr": "239.0.0.20:5000",
  "interface_addr": "10.0.0.5",
  "payload_type": 100
}
```

Output in the top-level `outputs` array:

```json
{
  "type": "st2110_40",
  "id": "anc-out",
  "name": "ANC loopback",
  "dest_addr": "239.2.0.20:5000",
  "dscp": 46,
  "payload_type": 100
}
```

Flow:

```json
{
  "id": "anc-flow",
  "name": "ANC (timecode + SCTE-104)",
  "enabled": true,
  "clock_domain": 0,
  "input_ids": ["anc-in"],
  "output_ids": ["anc-out"]
}
```

### ST 2110-20 uncompressed video input

Mandatory `video_encode` block — the ingress pipeline depacketizes RFC
4175, pushes raw frames into a blocking worker that feeds the encoder,
then muxes the H.264/HEVC output into MPEG-TS for the flow.

```json
{
  "id": "stadium-cam-in",
  "name": "Stadium camera (uncompressed 1080p60)",
  "type": "st2110_20",
  "bind_addr": "239.0.0.30:5000",
  "interface_addr": "10.0.0.5",
  "width": 1920,
  "height": 1080,
  "frame_rate_num": 60,
  "frame_rate_den": 1,
  "pixel_format": "yuv422_10bit",
  "payload_type": 96,
  "video_encode": {
    "codec": "x264",
    "bitrate_kbps": 15000,
    "preset": "veryfast",
    "profile": "high"
  },
  "redundancy": {
    "addr": "239.1.0.30:5000",
    "interface_addr": "10.1.0.5"
  }
}
```

### ST 2110-20 uncompressed video output

```json
{
  "type": "st2110_20",
  "id": "monitor-uncompressed",
  "name": "Uncompressed monitor feed",
  "dest_addr": "239.2.0.30:5000",
  "interface_addr": "10.0.0.5",
  "width": 1920,
  "height": 1080,
  "frame_rate_num": 60,
  "frame_rate_den": 1,
  "pixel_format": "yuv422_10bit",
  "payload_type": 96,
  "dscp": 46,
  "payload_budget": 1428
}
```

The output decodes the flow's source H.264/HEVC TS in an in-process
blocking worker (`VideoDecoder` → `VideoScaler` → pgroup pack), then
RFC 4175-packetizes onto the wire (Red + optional Blue). `payload_budget`
is the per-datagram byte budget for RTP payload (defaults to `1428`
which is safe for 1500-byte MTU; raise for jumbo frames). No
`video_encode` block is allowed on the output — the decode step is
implicit.

### ST 2110-23 multi-stream video

ST 2110-23 binds N ST 2110-20 receivers/senders that carry partitions
of one video essence. The reassembler / partitioner lives in
[`src/engine/st2110/video.rs`](../src/engine/st2110/video.rs).

Input:

```json
{
  "id": "uhd-multi-in",
  "name": "UHDTV1 (4 sub-streams, 2SI)",
  "type": "st2110_23",
  "partition_mode": "two_sample_interleave",
  "width": 3840,
  "height": 2160,
  "frame_rate_num": 60,
  "frame_rate_den": 1,
  "pixel_format": "yuv422_10bit",
  "sub_streams": [
    { "bind_addr": "239.0.0.40:5000", "payload_type": 96 },
    { "bind_addr": "239.0.0.41:5000", "payload_type": 96 },
    { "bind_addr": "239.0.0.42:5000", "payload_type": 96 },
    { "bind_addr": "239.0.0.43:5000", "payload_type": 96 }
  ],
  "video_encode": {
    "codec": "hevc_nvenc",
    "bitrate_kbps": 40000
  }
}
```

Output:

```json
{
  "type": "st2110_23",
  "id": "uhd-multi-out",
  "name": "UHDTV1 sender (4 × 2SI)",
  "partition_mode": "two_sample_interleave",
  "width": 3840,
  "height": 2160,
  "frame_rate_num": 60,
  "frame_rate_den": 1,
  "pixel_format": "yuv422_10bit",
  "sub_streams": [
    { "dest_addr": "239.2.0.40:5000" },
    { "dest_addr": "239.2.0.41:5000" },
    { "dest_addr": "239.2.0.42:5000" },
    { "dest_addr": "239.2.0.43:5000" }
  ],
  "dscp": 46,
  "payload_budget": 1428
}
```

Each sub-stream accepts its own `redundancy` block for independent
2022-7 Red/Blue duplication.

### Validation limits

| Field | Allowed values |
|-------|----------------|
| `sample_rate` (ST 2110-30/-31 wire) | `48000`, `96000` |
| `bit_depth` (ST 2110-30 wire) | `16`, `24` |
| `bit_depth` (ST 2110-31 wire, AES3) | `24` |
| `channels` | `1`, `2`, `4`, `8`, `16` |
| `packet_time_us` | `125`, `250`, `333`, `500`, `1000`, `4000` |
| `payload_type` | `96`–`127` |
| `clock_domain` | `0`–`127` |
| `dscp` | `0`–`63` (default `46` / EF) |
| ST 2110-20/-23 `pixel_format` | `"yuv422_8bit"`, `"yuv422_10bit"` |
| ST 2110-20/-23 `width` / `height` | `64`–`8192`, even |
| ST 2110-20/-23 `frame_rate` | `1`–`240` fps (num/den) |
| ST 2110-20/-23 `payload_budget` | `512`–`8952` bytes |
| ST 2110-23 `sub_streams` length | `2`–`16` |
| ST 2110-23 `partition_mode` | `"two_sample_interleave"`, `"sample_row"` |
| `rtp_audio.sample_rate` | `32000`, `44100`, `48000`, `88200`, `96000` |
| `rtp_audio.bit_depth` | `16`, `24` |
| `transcode.sample_rate` | `32000`, `44100`, `48000`, `88200`, `96000` |
| `transcode.bit_depth` | `16`, `20`, `24` |
| `transcode.channels` | `1`..=`16` |
| SRT/UDP/`rtp_audio` `transport_mode` | `"ts"` (default — UDP/SRT), `"rtp"` (default — `rtp_audio`), `"audio_302m"` |

ST 2110-20/-23 inputs **require** a `video_encode` block — the config
is rejected otherwise — because ingress is always a pixel-to-compressed
conversion. ST 2110-20/-23 outputs reject `video_encode` entirely
because the decode step is implicit. Encoder backends obey the same
Cargo-feature opt-in gate as existing `video_encode` outputs: the
default `cargo build` has no software video encoders, so it accepts
the config but the encode worker logs an error and drops frames at
runtime. The `*-linux-full` release binary (built with the
`video-encoders-full` composite) has all three encoders compiled in.

When `transport_mode == "audio_302m"`:

- SRT output rejects `packet_filter`, `program_number`, and `redundancy`
- UDP output rejects `program_number`
- The upstream input must be an audio essence (`st2110_30`, `st2110_31`,
  or `rtp_audio`); other input types fall back to passthrough TS with a
  warning

Combining `allowed_sources` with `redundancy` is rejected by validation
because the merger path doesn't expose per-packet `src` and we won't
silently bypass the source filter on the dual-leg path.

### Flow groups (essence bundles)

A flow group binds multiple per-essence flows into a single logical unit
sharing a PTP `clock_domain`. The schema lives at the top level of the
config:

```json
{
  "version": 2,
  "server": { "...": "..." },
  "inputs": [ "..." ],
  "outputs": [ "..." ],
  "flow_groups": [
    {
      "id": "studio-a-program",
      "name": "Studio A program",
      "clock_domain": 0,
      "flow_ids": ["studio-a-stereo", "anc-flow"]
    }
  ],
  "flows": [ "..." ]
}
```

Each flow named in `flow_ids` must exist elsewhere in the same config.
The manager UI renders flow groups as visual containers in the topology
view (deferred — see `docs/st2110.md`).

### WAN bridge (ST 2110 → SRT)

A flow with a ST 2110-30/-31/-40 input and an SRT output passes RTP
packets through the broadcast channel unchanged, so the WAN bridge mode
is just a normal flow definition — no special configuration needed.
Receivers on the far side rebuild the multicast group from the RTP
stream.


## Content Analysis (in-depth)

Per-flow opt-in to richer transport / audio / video analysis on top
of the always-on TR-101290 + media_analysis + thumbnails layer. Three
independent tiers:

- **Lite** — compressed-domain only (<1 % CPU / core). GOP cadence
  (IDR count + interval), full SPS/VUI signalling decode (aspect
  ratio with SAR→DAR math, colour primaries / transfer / matrix /
  range, HDR family, MaxFALL / MaxCLL, AFD), SMPTE timecode via
  `pic_timing` SEI, CEA-608 / 708 caption presence, SCTE-35 cue
  decode (splice_insert + time_signal PTS), Media Delivery Index
  (RFC 4445 — NDF from IAT spread, MLR from TS CC discontinuities).
- **Audio Full** — decoded-audio EBU R128 loudness (M / S / I +
  LRA), true peak (dBTP), hard-mute, clipping, silence per PID
  (~5–10 % CPU / core). Three ingress paths:
  - **MPEG-TS / AAC** (`stream_type = 0x0F` / `0x11`): ADTS framing
    + fdk-aac decode → R128.
  - **PCM-RTP** (ST 2110-30 PM/AM L16/L24, generic RtpAudio):
    direct interleaved-sample unpack → R128. No decoder needed; the
    cleanest signal path because the wire is already linear PCM.
  - **AES3 over RTP** (ST 2110-31): 32-bit subframe extraction
    (24-bit audio bits 27..4) → R128.

  All three paths share the same per-flow state machine + event
  emission and produce the same wire shape (`audio_pids[]`). The
  snapshot's top-level `ingress` field reports `"ts"` / `"pcm"` /
  `"aes3"` so the manager UI can label the ingest mode. MP2 / AC-3 /
  E-AC-3 PIDs inside MPEG-TS now decode in-process via the FFmpeg
  audio bridge (`media-codecs` feature, default on) and run the
  full R128 / true-peak / mute / clip pipeline alongside AAC; their
  snapshot reports `codec_decoded: true`.
- **Video Full** — YUV pixel-domain metrics on decoded frames
  (~10–25 % CPU / core): YUV-SAD freeze against the previous frame,
  3×3 Laplacian-variance blur, 8×8 boundary-gradient blockiness
  (Wang / Sheikh style), letterbox / pillarbox row / column
  detection, SMPTE-bars column-uniformity heuristic, and a
  freeze + mid-brightness slate flag. Decode runs under
  `tokio::task::block_in_place` via
  [`video_engine::VideoDecoder`] (H.264 / H.265 only).

All tiers are broadcast subscribers — **they cannot add jitter or
backpressure to the data path.** Dropping a tier off mid-flight
cancels its task within one packet.

### Schema

```json
{
  "flows": [
    {
      "id": "remote-monitor-1",
      "name": "Remote monitor flow",
      "input_ids": ["srt-ingest"],
      "output_ids": [],
      "content_analysis": {
        "lite": true,
        "audio_full": false,
        "video_full": false,
        "video_full_hz": null
      }
    }
  ]
}
```

| Field | Default | Notes |
|---|---|---|
| `lite` | `true` | Cheap compressed-domain checks. TS-input only — no-op on PCM / ANC / WebRTC inputs. |
| `audio_full` | `false` | Opt-in, moderate CPU. PMT-announced audio PIDs only. |
| `video_full` | `false` | Opt-in, heavy CPU. PMT-announced video PIDs only. |
| `video_full_hz` | `null` (= 1.0) | Override the Video Full sample rate. Clamped to `(0.0, 30.0]`. Values above 5 Hz scale CPU proportionally. |

Omitting `content_analysis` is equivalent to `{ "lite": true,
"audio_full": false, "video_full": false }` — legacy flows get Lite
analysis automatically when the active input is TS-carrying.

### Monitor-only / triage deployment

A flow with `input_ids: [...]`, `output_ids: []`, and the
content-analysis tiers you care about on is a **monitor-only flow**
— the edge ingests, runs TR-101290 + content analysis, publishes
stats / events / thumbnails to the manager, and produces zero egress
traffic. This is the recommended shape when deploying a remote-site
edge purely for broadcast-engineer triage.

### Events

See [`events-and-alarms.md`](events-and-alarms.md#content-analysis-events)
for the full list of categories (`content_analysis_scte35_pid`,
`content_analysis_scte35_cue`, `content_analysis_caption_lost`,
`content_analysis_mdi_above_threshold`,
`content_analysis_audio_silent`, `content_analysis_video_freeze`).
Each event carries a structured `details.error_code` matching its
category.

### Metrics / wire shape

See [`metrics.md`](metrics.md#content-analysis-metrics-phase-1-3)
for the full `FlowStats.content_analysis` JSON shape the manager
dashboards consume.


## Replay (recording + playback)

Phase 1 of the in-edge replay server. Gated by the `replay` Cargo
feature (default on). Two surfaces: a `recording` flow attribute that
captures the flow's broadcast channel to disk continuously, and a
`replay` input type that pumps a previously-recorded clip back onto a
flow's broadcast channel paced by PCR.

### Storage root

Resolved at runtime, in order:

1. `BILBYCAST_REPLAY_DIR` env var (operator override)
2. `$XDG_DATA_HOME/bilbycast/replay/`
3. `$HOME/.bilbycast/replay/`
4. `./replay/`

Each recording lives at `<replay_root>/<recording_id>/` (defaults to
the flow id; override via `RecordingConfig.storage_id`):

```
000000.ts  000001.ts  ...  NNNNNN.ts
recording.json   ← created_at, segment_seconds, schema_version
index.bin        ← timecode → byte-offset (24 B / IDR)
clips.json       ← named (in_pts, out_pts) ranges
.tmp/            ← in-flight segment writes; atomic rename on roll
```

### Recording (flow attribute)

```json
"flows": [{
  "id": "record-flow",
  "name": "Record live SRT to disk",
  "enabled": true,
  "input_ids": ["live-srt-in"],
  "output_ids": [],
  "recording": {
    "enabled": true,
    "storage_id": "record-flow",
    "segment_seconds": 10,
    "retention_seconds": 86400,
    "max_bytes": 53687091200,
    "pre_buffer_seconds": null
  }
}]
```

| Field | Default | Notes |
|---|---|---|
| `enabled` | `true` | When `false`, the writer is built but doesn't subscribe — useful for cron-armed recording via routines |
| `storage_id` | `null` (= flow id) | Subdirectory under the replay root. Same character set as media filenames (alphanumeric + `._-`, ≤ 64 chars) |
| `segment_seconds` | `10` | Wall-clock segment roll cadence. Range `[2, 60]` |
| `retention_seconds` | `86400` (24h) | Oldest-first prune by mtime. `0` = unlimited |
| `max_bytes` | `53687091200` (50 GiB) | Oldest-first prune by total size. `0` = unlimited (still subject to disk) |
| `pre_buffer_seconds` | `null` | When set, the writer auto-arms in `PreBuffer` mode and rolls segments to disk with retention pinned at this value, so an operator pressing Start later picks up the last `N` seconds of pre-roll. `null` = no pre-buffer (writer starts in `Armed` mode the moment it spawns). Range `[1, 300]` when set. `RecordingStats.armed` stays `false` while in pre-buffer so the manager UI distinguishes pre-roll from a live recording session |

The writer is a sibling subscriber on the flow's broadcast channel —
drop-on-lag with a Critical `replay_writer_lagged` event mirrors the
slow-consumer pattern in `engine::output_udp`. Disk I/O lives behind
a bounded mpsc to a dedicated writer task; the broadcast subscriber
never blocks on `write_all`.

### Replay (input type)

```json
"inputs": [{
  "id": "replay-in",
  "name": "Replay (clip playback)",
  "type": "replay",
  "recording_id": "record-flow",
  "clip_id": null,
  "start_paused": true,
  "loop_playback": false
}]
```

| Field | Default | Notes |
|---|---|---|
| `recording_id` | (required) | The on-disk recording to read from |
| `clip_id` | `null` | Optional — when set, only that clip's `[in_pts, out_pts]` range plays. Otherwise the whole recording is available |
| `start_paused` | `true` | When `true`, the input idles on flow start until a `play_clip` / `cue_clip` command activates playback |
| `loop_playback` | `false` | When `true`, restart at the beginning on EOF |
| `passthrough_clock` | `false` | Opt out of muxer-mode PCR + PES PTS/DTS regeneration on the replayed TS — see [RTP Input](#rtp-input) |

Phase 1 supports 1.0× forward playback only — no reverse, no
slow-mo. Mark / cue / play / scrub / stop commands flow via the WS
`mark_in` / `mark_out` / `cue_clip` / `play_clip` / `scrub_playback`
/ `stop_playback` actions and route through the per-flow replay
command channel.

### Events

See [`events-and-alarms.md`](events-and-alarms.md#replay-server-events)
for the full list of `replay_event` values and `command_ack.error_code`
codes (`replay_recording_not_active`, `replay_no_playback_input`,
`replay_clip_not_found`, `replay_writer_lagged`, `replay_disk_full`,
`replay_index_corrupt`, `replay_invalid_field`, `replay_invalid_range`,
`replay_invalid_tag`).

### `recording_status` response shape

```jsonc
{
  "armed": false,
  "mode": "pre_buffer",         // Phase 2 / 1.5 — "armed" / "pre_buffer" / "idle"
  "recording_id": "record-flow",
  "current_pts_90khz": 8100000,
  "segments_written": 3,
  "bytes_written": 31457280,
  "segments_pruned": 0,
  "packets_dropped": 0,
  "index_entries": 6,
  "max_bytes": 53687091200,
  "replay_root_free_bytes": 102400000000,
  "replay_root_total_bytes": 256000000000
}
```

The `mode` field is additive — older edges omit it and the manager
falls back to `armed`-derived `Recording / Idle` state.

### Clip mutation: `update_clip` (Phase 2 / 1.5)

`update_clip` is the unified clip-mutation command — superset of the
legacy `rename_clip`, with `tags` and `in_pts_90khz` / `out_pts_90khz`
added. See [`replay.md`](replay.md#phase-2--15--clip-tags--update_clip)
for the wire shape and SMPTE-on-trim semantics.

### Metrics

Per-recording counters surfaced on the WS stats path:
`segments_written`, `bytes_written`, `segments_pruned`,
`packets_dropped`, `index_entries`, `current_pts_90khz`, `armed`,
`mode` (Phase 2 / 1.5). See
[`metrics.md`](metrics.md#replay-server-metrics).

## Capacity & resource budget

Every edge probes its hardware once at startup and advertises a
`resource_budget` block on `HealthPayload` so the manager UI can
render a per-node Resources card and a per-flow "Resource impact"
preview. The numbers are a deterministic *planning* score —
operators read live `system_resources.cpu_percent` for ground truth.

### What gets probed

- **Hardware encoders / decoders** — for each of `h264_nvenc`,
  `hevc_nvenc`, `h264_qsv`, `hevc_qsv`, `h264_videotoolbox`,
  `hevc_videotoolbox`, `h264_amf`, `hevc_amf` (and the corresponding
  hardware decoders), the edge calls FFmpeg's
  `avcodec_find_encoder_by_name` / `avcodec_find_decoder_by_name`. A
  non-NULL pointer means the codec was compiled into the vendored
  FFmpeg build. **It does NOT prove a session opens** — driver /
  hardware / runtime dependencies are still resolved at
  `avcodec_open2` time. Treat the result as "advertised, not
  guaranteed."
- **CPU info** — brand string + physical / logical core count from
  the existing `sysinfo` dependency, AVX class via
  `std::is_x86_feature_detected!` (`avx512f` → `avx2` → `sse4.2` →
  `none`; aarch64 reports `other`).
- **Software capacity estimate** — a rough mapping of `(physical
  cores, AVX class) → 720p30 x264 streams at broadcast crf28`.
  Conservative: `cores / 2 × avx_mult` where AVX-512 = ×1.3,
  AVX2 / Other = ×1.0, SSE4.2 = ×0.6, None = ×0.4. HEVC (x265) is
  half. AAC encode is ~200 streams per core (effectively unbounded).
  ±50 % accuracy.

### Live NVIDIA utilisation

Builds with the `hardware-monitor-nvml` Cargo feature additionally
poll NVIDIA NVML every 5 s for live NVENC engine %, NVDEC engine %,
and active session count on device 0. The dep is target-conditional
on Linux + Windows; macOS / BSD builds skip it cleanly. NVML init
failure (no driver / no GPU) is silent — the manager UI just hides
the live block.

There is no equivalent for QSV / VideoToolbox / AMF in the v1
surface. Static presence still reports correctly for those backends.

### Per-flow cost units

Each running flow contributes a deterministic unit count to the
node's budget. Numbers are anchored to the documented content_analysis
and transcode cost notes in this file and the root `CLAUDE.md`:

| Flow shape                              | Units |
|---|---|
| Passthrough flow (base)                 | 1     |
| Each `video_encode` output (HW backend) | 100   |
| Each `video_encode` output (SW)         | 500   |
| Each `audio_encode` output              | 5     |
| `content_analysis = lite`               | 2     |
| `content_analysis = audio_full`         | 20    |
| `content_analysis = video_full`         | 50    |
| `recording` (replay) enabled            | 5     |
| `thumbnail` (per generator @ 5 s)       | 3     |

ST 2110-20 / -23 outputs always incur the SW video-encode cost — the
RFC 4175 packetiser feeds an internal x264 / x265 pass.

Thumbnail cost scales two ways: with cadence (`thumbnail_interval_secs`,
work ∝ 1/interval → 15 units at 1 s, 3 at the 5 s default, 1 at 30 s) and
with the number of concurrent generators a flow runs — one flow-level
preview plus one per input. A 2-input flow at the default adds
`(1 + 2) × 3 = 9` units; a flow with thumbnails off or no inputs adds 0.

### Total budget capacity

`units_total = 1000 + 200 × physical_cores`. A 4-core box gets 1800
units; a 32-core EPYC gets 7400. The manager UI surfaces a
percentage utilisation against this total, amber at 80 %, red at
100 %. **No hard gate** — saving a flow that would push the node
over budget is a warning, not a refusal. Operators read the live
CPU% / NVML utilisation when the warning matters.

### Capability gate

The edge advertises `"resources"` in `HealthPayload.capabilities`
when the budget block is populated. The manager UI keys both the
per-node Resources card and the flow create/edit modal's
"Resource impact" tile off this string — older edges or builds
without the probe see no UI at all.
