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
- [Flow Configuration](#flow-configuration)
- [Input Types](#input-types)
  - [RTP Input](#rtp-input)
  - [SRT Input](#srt-input)
- [Output Types](#output-types)
  - [RTP Output](#rtp-output)
  - [SRT Output](#srt-output)
  - [RTMP Output](#rtmp-output)
  - [HLS Output](#hls-output)
  - [WebRTC Output](#webrtc-output)
- [SMPTE 2022-1 FEC Configuration](#smpte-2022-1-fec-configuration)
- [SMPTE 2022-7 SRT Redundancy](#smpte-2022-7-srt-redundancy)
- [SRT Connection Modes](#srt-connection-modes)
- [CLI Argument Overrides](#cli-argument-overrides)
- [Config Persistence Behavior](#config-persistence-behavior)
- [Common Configuration Scenarios](#common-configuration-scenarios)

---

## Configuration File Basics

bilbycast-edge reads its configuration from a JSON file specified by the `--config` CLI argument (default: `./config.json`). If the file does not exist at startup, an empty default configuration is used.

The configuration is loaded and validated at startup. Changes made through the API (creating flows, updating config) are automatically persisted back to the same file using atomic writes (write to temp file, then rename).

---

## Full Annotated Example

```json
{
  "version": 1,
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
  "flows": [
    {
      "id": "main-feed",
      "name": "Main Program Feed",
      "enabled": true,
      "input": {
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
      },
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
      ]
    }
  ]
}
```

---

## Top-Level Structure (AppConfig)

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `version` | integer | Yes | - | Schema version. Currently must be `1`. |
| `server` | object | Yes | - | API server configuration. |
| `monitor` | object | No | `null` | Web monitoring dashboard configuration. |
| `flows` | array | No | `[]` | List of flow configurations. |

---

## Server Configuration

The `server` object controls the API server listener.

```json
{
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080,
    "tls": { ... },
    "auth": { ... }
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `listen_addr` | string | Yes | `"0.0.0.0"` | IP address to bind the API server to. Use `"0.0.0.0"` for all interfaces or a specific IP. |
| `listen_port` | integer | Yes | `8080` | TCP port for the API server. |
| `tls` | object | No | `null` | TLS configuration for HTTPS. Requires the `tls` Cargo feature. |
| `auth` | object | No | `null` | OAuth 2.0 / JWT authentication configuration. When absent or `enabled: false`, all endpoints are open. |

---

## TLS Configuration

Optional sub-object of `server`. Requires building with `--features tls`.

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
| `listen_addr` | string | Yes | IP address for the dashboard server. |
| `listen_port` | integer | Yes | TCP port for the dashboard. Must differ from `server.listen_port` if the same `listen_addr` is used. |

**Validation:** The monitor address must differ from the API server address (same IP + same port is rejected).

---

## Flow Configuration

Each flow defines one input source fanning out to one or more output destinations.

```json
{
  "id": "main-feed",
  "name": "Main Program Feed",
  "enabled": true,
  "input": { ... },
  "outputs": [ ... ]
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `id` | string | Yes | - | Unique identifier. Cannot be empty. Must be unique across all flows. |
| `name` | string | Yes | - | Human-readable display name. Cannot be empty. |
| `enabled` | boolean | No | `true` | Whether to auto-start this flow on startup or creation. |
| `input` | object | Yes | - | Input source configuration (RTP or SRT). |
| `outputs` | array | Yes | - | Output destination configurations. Can be empty. Output IDs must be unique within the flow. |

---

## Input Types

The `input` object uses a `type` discriminator field to determine which input variant is used.

### RTP Input

Receives RTP/UDP packets (SMPTE ST 2022-2). Supports unicast, multicast, IPv4, and IPv6.

```json
{
  "type": "rtp",
  "bind_addr": "239.1.1.1:5000",
  "interface_addr": "192.168.1.100",
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
| `fec_decode` | object | No | `null` | SMPTE 2022-1 FEC decode parameters. See [FEC Configuration](#smpte-2022-1-fec-configuration). |
| `tr07_mode` | boolean | No | `null` | Enable VSF TR-07 mode to detect and report JPEG XS streams in the transport stream. |
| `allowed_sources` | array of strings | No | `null` | Source IP allow-list (RP 2129 C5). Only RTP packets from these source IPs are accepted. Each entry must be a valid IP address. When `null`, all sources are allowed. |
| `allowed_payload_types` | array of integers | No | `null` | RTP payload type allow-list (RP 2129 U4). Only packets with these PT values (0-127) are accepted. When `null`, all payload types are allowed. |
| `max_bitrate_mbps` | float | No | `null` | Maximum ingress bitrate in megabits per second (RP 2129 C7). Excess packets are dropped. Must be positive. When `null`, no rate limiting is applied. |

**Validation rules:**
- `bind_addr` must be a valid `ip:port` socket address.
- `interface_addr` must be a valid IP address (no port) in the same address family as `bind_addr`.
- `allowed_payload_types` values must be 0-127.
- `max_bitrate_mbps` must be positive.

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
  "redundancy": {
    "mode": "listener",
    "local_addr": "0.0.0.0:9001",
    "latency_ms": 500,
    "passphrase": "my-encryption-key",
    "aes_key_len": 32
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"srt"`. |
| `mode` | string | Yes | - | SRT connection mode: `"caller"`, `"listener"`, or `"rendezvous"`. See [SRT Connection Modes](#srt-connection-modes). |
| `local_addr` | string | Yes | - | Local socket address to bind (`ip:port`). |
| `remote_addr` | string | Conditional | `null` | Remote address to connect to. Required for `caller` and `rendezvous` modes. |
| `latency_ms` | integer | No | `120` | SRT receive latency buffer in milliseconds. Higher values provide more resilience to network jitter at the cost of increased delay. |
| `passphrase` | string | No | `null` | AES encryption passphrase. Must be 10-79 characters. When `null`, encryption is disabled. |
| `aes_key_len` | integer | No | `16` | AES key length in bytes: `16` (AES-128), `24` (AES-192), or `32` (AES-256). Only meaningful if `passphrase` is set. |
| `redundancy` | object | No | `null` | SMPTE 2022-7 redundancy configuration for a second SRT leg. See [SRT Redundancy](#smpte-2022-7-srt-redundancy). |

**Validation rules:**
- `local_addr` must be a valid socket address.
- `remote_addr` is required for `caller` and `rendezvous` modes and must be a valid socket address.
- `passphrase` must be 10-79 characters.
- `aes_key_len` must be 16, 24, or 32.

---

## Output Types

Each output has a `type` discriminator. All outputs share `id` and `name` fields.

### RTP Output

Sends RTP/UDP packets to a unicast or multicast destination.

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
| `id` | string | Yes | - | Unique output ID within the flow. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `dest_addr` | string | Yes | - | Destination socket address (`ip:port`). For multicast, use the group address (e.g., `"239.1.2.1:5004"`). IPv6: `"[::1]:5004"`. |
| `bind_addr` | string | No | `"0.0.0.0:0"` | Source bind address. Use to control the source IP/port of outgoing packets. Must be same address family as `dest_addr`. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast send. Must be same address family as `dest_addr`. |
| `fec_encode` | object | No | `null` | SMPTE 2022-1 FEC encode parameters. See [FEC Configuration](#smpte-2022-1-fec-configuration). |
| `dscp` | integer | No | `46` | DSCP value for QoS marking (RP 2129 C10). Range 0-63. Default 46 = Expedited Forwarding (RFC 4594). |

**Validation rules:**
- `id` cannot be empty.
- `dest_addr`, `bind_addr`, and `interface_addr` must all use the same address family.
- `dscp` must be 0-63.

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
| `id` | string | Yes | - | Unique output ID within the flow. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `mode` | string | Yes | - | SRT connection mode: `"caller"`, `"listener"`, or `"rendezvous"`. |
| `local_addr` | string | Yes | - | Local socket address to bind. Use `"0.0.0.0:0"` for caller mode (ephemeral port). |
| `remote_addr` | string | Conditional | `null` | Remote address. Required for `caller` and `rendezvous`. |
| `latency_ms` | integer | No | `120` | SRT send latency in milliseconds. |
| `passphrase` | string | No | `null` | AES encryption passphrase (10-79 characters). |
| `aes_key_len` | integer | No | `16` | AES key length: 16, 24, or 32. |
| `redundancy` | object | No | `null` | SMPTE 2022-7 redundancy for a second SRT output leg. |

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
  "max_reconnect_attempts": 10
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"rtmp"`. |
| `id` | string | Yes | - | Unique output ID. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `dest_url` | string | Yes | - | RTMP server URL. Must start with `rtmp://` or `rtmps://`. RTMPS requires the `tls` Cargo feature. |
| `stream_key` | string | Yes | - | Stream key for authentication with the RTMP server. Cannot be empty. |
| `reconnect_delay_secs` | integer | No | `5` | Seconds to wait before reconnecting after a failure. Must be > 0. |
| `max_reconnect_attempts` | integer | No | `null` (unlimited) | Maximum reconnection attempts. When `null`, reconnects indefinitely. |

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

**Limitations:**
- Output only. Segment-based transport inherently adds 1-4 seconds of latency.

### WebRTC Output

Sends via WebRTC using WHIP (WebRTC-HTTP Ingestion Protocol) signaling. Extracts H.264 NALUs from the TS stream and repacketizes as RFC 6184 RTP.

```json
{
  "type": "webrtc",
  "id": "webrtc-out",
  "name": "WebRTC Output",
  "whip_url": "https://whip.example.com/ingest/stream1",
  "bearer_token": "my-auth-token",
  "video_only": true
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"webrtc"`. |
| `id` | string | Yes | - | Unique output ID. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `whip_url` | string | Yes | - | WHIP signaling endpoint URL. Must start with `http://` or `https://`. |
| `bearer_token` | string | No | `null` | Bearer token for WHIP endpoint authentication. |
| `video_only` | boolean | No | `false` | When `true`, only video is sent (audio track omitted). Set to `true` when the source carries AAC audio, since AAC-to-Opus transcoding is not available in the pure-Rust build. |

**Limitations:**
- Requires the `webrtc` Cargo feature.
- Audio: Only Opus passthrough. AAC-to-Opus transcoding requires C libraries not available in the pure-Rust build.
- Requires a WHIP-compatible endpoint.

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

Legs can use different SRT modes, different ports, different latency values, and even different encryption settings (though using the same settings is recommended for simplicity).

---

## SRT Connection Modes

| Mode | Initiator | `remote_addr` required | Use case |
|------|-----------|----------------------|----------|
| `caller` | This endpoint connects to a remote listener | Yes | Sending to a known destination. Most common for outputs. |
| `listener` | This endpoint waits for incoming connections | No | Accepting streams from remote callers. Most common for inputs (ingest servers). |
| `rendezvous` | Both sides connect simultaneously | Yes | NAT traversal. Both sides must use rendezvous mode and know each other's address. |

---

## CLI Argument Overrides

Command-line arguments override values from the config file. This is useful for deployment automation and containerization.

```
bilbycast-edge [OPTIONS]

Options:
  -c, --config <PATH>          Path to configuration file [default: ./config.json]
  -p, --port <PORT>            Override API listen port
  -b, --bind <ADDRESS>         Override API listen address
      --monitor-port <PORT>    Override monitor dashboard port
  -l, --log-level <LEVEL>      Log level: trace, debug, info, warn, error [default: info]
  -h, --help                   Print help
  -V, --version                Print version
```

| Argument | Config field overridden | Example |
|----------|----------------------|---------|
| `--port` | `server.listen_port` | `--port 9443` |
| `--bind` | `server.listen_addr` | `--bind 127.0.0.1` |
| `--monitor-port` | `monitor.listen_port` | `--monitor-port 9091` |
| `--log-level` | (runtime only, not in config) | `--log-level debug` |

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

bilbycast-edge automatically persists configuration changes to disk when flows are modified through the API:

- **Create flow** (`POST /api/v1/flows`) -- Appends the new flow and saves.
- **Update flow** (`PUT /api/v1/flows/{id}`) -- Replaces the flow in-place and saves.
- **Delete flow** (`DELETE /api/v1/flows/{id}`) -- Removes the flow and saves.
- **Add output** (`POST /api/v1/flows/{id}/outputs`) -- Appends the output and saves.
- **Remove output** (`DELETE /api/v1/flows/{id}/outputs/{oid}`) -- Removes the output and saves.
- **Replace config** (`PUT /api/v1/config`) -- Replaces the entire config and saves.

### Atomic writes

All config saves use an atomic write strategy: the configuration is written to a temporary file (`config.json.tmp`), then atomically renamed to the target path. This prevents corruption if the process is interrupted during a write.

### Default config

If the config file does not exist when bilbycast-edge starts, an empty default configuration is used:

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "flows": []
}
```

### Reloading from disk

Use `POST /api/v1/config/reload` to re-read the config file from disk. This is useful after manual edits or after deploying a new config file via external tooling (e.g., Ansible, Chef).

---

## Common Configuration Scenarios

### Minimal: RTP receive and forward (no auth)

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "flows": [
    {
      "id": "passthrough",
      "name": "RTP Passthrough",
      "enabled": true,
      "input": {
        "type": "rtp",
        "bind_addr": "0.0.0.0:5000"
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "out-1",
          "name": "Forwarded Output",
          "dest_addr": "192.168.1.50:5004"
        }
      ]
    }
  ]
}
```

### Multicast receive with FEC and trust boundary filters

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "flows": [
    {
      "id": "multicast-feed",
      "name": "Multicast with FEC and Trust Boundary",
      "enabled": true,
      "input": {
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
      },
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
      ]
    }
  ]
}
```

### SRT bidirectional with 2022-7 redundancy

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "flows": [
    {
      "id": "srt-redundant",
      "name": "SRT with Hitless Redundancy",
      "enabled": true,
      "input": {
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
      },
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
      ]
    }
  ]
}
```

### Multi-output: RTP to SRT, RTMP, and HLS simultaneously

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "flows": [
    {
      "id": "multi-output",
      "name": "Multi-Output Fan-Out",
      "enabled": true,
      "input": {
        "type": "rtp",
        "bind_addr": "239.1.1.1:5000",
        "interface_addr": "192.168.1.100"
      },
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
      ]
    }
  ]
}
```

### Full production config with TLS + auth + monitoring

```json
{
  "version": 1,
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
  "flows": [
    {
      "id": "main-feed",
      "name": "Main Program Feed",
      "enabled": true,
      "input": {
        "type": "rtp",
        "bind_addr": "239.1.1.1:5000",
        "interface_addr": "10.0.0.100",
        "fec_decode": {
          "columns": 10,
          "rows": 10
        }
      },
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
      ]
    }
  ]
}
```

### IPv6 multicast configuration

```json
{
  "version": 1,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "flows": [
    {
      "id": "ipv6-mcast",
      "name": "IPv6 Multicast Flow",
      "enabled": true,
      "input": {
        "type": "rtp",
        "bind_addr": "[ff7e::1]:5000",
        "interface_addr": "::1"
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "ipv6-out",
          "name": "IPv6 Output",
          "dest_addr": "[ff7e::2]:5004",
          "interface_addr": "::1"
        }
      ]
    }
  ]
}
```
