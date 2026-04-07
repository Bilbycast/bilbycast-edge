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
- [Tunnel Configuration](#tunnel-configuration)
- [Flow Configuration](#flow-configuration)
- [Input Types](#input-types)
  - [RTP Input](#rtp-input)
  - [SRT Input](#srt-input)
  - [RTMP Input](#rtmp-input)
  - [RTSP Input](#rtsp-input)
  - [WebRTC/WHIP Input](#webrtcwhip-input)
  - [WHEP Input](#whep-input)
- [Output Types](#output-types)
  - [RTP Output](#rtp-output)
  - [SRT Output](#srt-output)
  - [RTMP Output](#rtmp-output)
  - [HLS Output](#hls-output)
  - [WebRTC Output](#webrtc-output)
- **SMPTE ST 2110 audio + ANC** — see the dedicated section near the
  end of this guide and the deep-dive in
  [`audio-gateway.md`](audio-gateway.md). Covers ST 2110-30/-31 audio,
  ST 2110-40 ANC, the per-output `transcode` block (sample rate / bit
  depth / channel routing), the `rtp_audio` no-PTP variant, and SMPTE
  302M LPCM-in-MPEG-TS over SRT / UDP / RTP-MP2T (`transport_mode:
  "audio_302m"`).
- [MPTS → SPTS filtering](#mpts--spts-filtering)
- [SMPTE 2022-1 FEC Configuration](#smpte-2022-1-fec-configuration)
- [SMPTE 2022-7 SRT Redundancy](#smpte-2022-7-srt-redundancy)
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
  "version": 1,
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
| `node_id` | string | No | Auto-generated | Persistent UUID v4 identifying this edge node. Auto-generated on first startup and saved to config. Used as the NMOS IS-04 Node ID. |
| `device_name` | string | No | `null` | Optional human-readable label for this edge node (e.g. "Studio-A Encoder"). Max 256 characters. |
| `setup_enabled` | boolean | No | `true` | When true, the browser-based setup wizard is accessible at `/setup`. Set to false to disable after provisioning. |
| `server` | object | Yes | - | API server configuration. |
| `monitor` | object | No | `null` | Web monitoring dashboard configuration. |
| `manager` | object | No | `null` | Manager WebSocket connection configuration. See [Manager Configuration](#manager-configuration). |
| `flows` | array | No | `[]` | List of flow configurations. See [Flow Configuration](#flow-configuration). |
| `tunnels` | array | No | `[]` | List of IP tunnel configurations. See [Tunnel Configuration](#tunnel-configuration). |

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
| `tls` | object | No | `null` | TLS configuration for HTTPS (`tls` feature enabled by default). |
| `auth` | object | No | `null` | OAuth 2.0 / JWT authentication configuration. When absent or `enabled: false`, all endpoints are open. |

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

## Tunnel Configuration

IP tunnels create encrypted point-to-point links between edge nodes, either through a bilbycast-relay server (for NAT traversal) or directly via QUIC (when one edge has a public IP).

### Relay Mode

Both edges connect outbound to a bilbycast-relay server. The relay pairs them by tunnel UUID and forwards traffic. End-to-end encryption ensures the relay cannot read payloads.

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
      "relay_addr": "relay.example.com:4433",
      "tunnel_encryption_key": "0123456789abcdef...",
      "tunnel_bind_secret": "fedcba9876543210..."
    }
  ]
}
```

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
| `mode` | string | Yes | - | `"relay"` (via relay server) or `"direct"` (QUIC peer-to-peer). |
| `direction` | string | Yes | - | `"ingress"` (receives tunnel traffic, forwards to `local_addr`) or `"egress"` (listens on `local_addr`, sends into tunnel). |
| `local_addr` | string | Yes | - | For **egress**: listen address for local traffic to tunnel (e.g. `"0.0.0.0:9000"`). For **ingress**: forward destination for received traffic (e.g. `"127.0.0.1:9000"`). |
| `relay_addr` | string | Relay mode | `null` | Relay server QUIC address (e.g. `"relay.example.com:4433"`). Required for relay mode. |
| `tunnel_encryption_key` | string | Relay mode | `null` | End-to-end ChaCha20-Poly1305 encryption key. Hex-encoded, exactly 64 chars (32 bytes). Required for relay mode. Both edges must share the same key. **Stored in `secrets.json`.** |
| `tunnel_bind_secret` | string | No | `null` | HMAC-SHA256 bind authentication secret. Hex-encoded, exactly 64 chars. Proves authorization to bind on the relay. **Stored in `secrets.json`.** |
| `peer_addr` | string | Direct egress | `null` | Remote peer QUIC address (e.g. `"203.0.113.50:4433"`). Required for direct mode, egress direction. |
| `direct_listen_addr` | string | Direct ingress | `null` | QUIC listen address (e.g. `"0.0.0.0:4433"`). Required for direct mode, ingress direction. |
| `tunnel_psk` | string | No | `null` | Pre-shared key for direct mode authentication. Hex-encoded, 64 chars. Both edges must share the same PSK. **Stored in `secrets.json`.** |
| `tls_cert_pem` | string | No | Auto-generated | TLS certificate PEM for direct mode listener. Auto-generated if absent. **Stored in `secrets.json`.** |
| `tls_key_pem` | string | No | Auto-generated | TLS private key PEM for direct mode listener. **Stored in `secrets.json`.** |

### Tunnel Validation Rules

- `id` must be a valid UUID.
- `relay_addr` required when `mode` is `"relay"`.
- `tunnel_encryption_key` required for relay mode; must be exactly 64 hex characters.
- `tunnel_bind_secret` must be exactly 64 hex characters if present.
- `peer_addr` required for direct mode egress.
- `direct_listen_addr` required for direct mode ingress.
- `tunnel_psk` must be exactly 64 hex characters if present.
- All address fields must be valid socket addresses.

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
| `media_analysis` | boolean | No | `true` | Enable media content analysis (codec, resolution, frame rate detection). |
| `thumbnail` | boolean | No | `true` | Enable thumbnail generation (requires ffmpeg). |
| `thumbnail_program_number` | integer | No | `null` | When the input is an MPTS, render the thumbnail from this MPEG-TS program only. `null` lets ffmpeg pick the first program it finds. Must be `> 0` if set. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `bandwidth_limit` | object | No | `null` | Per-flow bandwidth monitoring (RP 2129). See [Bandwidth Limit](#bandwidth-limit). |
| `input` | object | Yes | - | Input source configuration (RTP, UDP, SRT, RTMP, RTSP, WebRTC, or WHEP). |
| `outputs` | array | Yes | - | Output destination configurations. Can be empty. Output IDs must be unique within the flow. |

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

## Input Types

The `input` object uses a `type` discriminator field to determine which input variant is used: `rtp`, `udp`, `srt`, `rtmp`, `rtsp`, `webrtc`, or `whep`.

### RTP Input

Receives RTP-wrapped MPEG-TS packets (SMPTE ST 2022-2). Requires valid RTP v2 headers. Supports unicast, multicast, IPv4, and IPv6. For raw TS without RTP headers, use the UDP input type.

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

### UDP Input

Receives raw UDP datagrams without requiring RTP headers. Suitable for raw MPEG-TS over UDP from OBS, ffmpeg (`-f mpegts udp://`), srt-live-transmit, VLC, or any source that sends plain TS.

```json
{
  "type": "udp",
  "bind_addr": "0.0.0.0:5000",
  "interface_addr": "192.168.1.100"
}
```

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `type` | string | Yes | - | Must be `"udp"`. |
| `bind_addr` | string | Yes | - | Local socket address to bind (`ip:port`). For multicast, use the group address. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast group join. Must be the same address family as `bind_addr`. |

**Validation rules:**
- `bind_addr` must be a valid `ip:port` socket address.
- `interface_addr` must be a valid IP address in the same address family as `bind_addr`.

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
| `local_addr` | string | Yes | - | Local socket address to bind (`ip:port`). |
| `remote_addr` | string | Conditional | `null` | Remote address to connect to. Required for `caller` and `rendezvous` modes. |
| `latency_ms` | integer | No | `120` | SRT receive latency buffer in milliseconds. Higher values provide more resilience to network jitter at the cost of increased delay. |
| `passphrase` | string | No | `null` | AES encryption passphrase. Must be 10-79 characters. When `null`, encryption is disabled. |
| `aes_key_len` | integer | No | `16` | AES key length in bytes: `16` (AES-128), `24` (AES-192), or `32` (AES-256). Only meaningful if `passphrase` is set. |
| `crypto_mode` | string | No | `null` | Cipher mode: `"aes-ctr"` (default) or `"aes-gcm"` (authenticated encryption). AES-GCM requires libsrt >= 1.5.2 on the peer and only supports AES-128/256 (not AES-192). |
| `redundancy` | object | No | `null` | SMPTE 2022-7 redundancy configuration for a second SRT leg. See [SRT Redundancy](#smpte-2022-7-srt-redundancy). |

**Validation rules:**
- `local_addr` must be a valid socket address.
- `remote_addr` is required for `caller` and `rendezvous` modes and must be a valid socket address.
- `passphrase` must be 10-79 characters.
- `aes_key_len` must be 16, 24, or 32.
- `crypto_mode` must be `"aes-ctr"` or `"aes-gcm"`. AES-GCM with `aes_key_len` 24 is rejected.

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

---

## Output Types

Each output has a `type` discriminator. All outputs share `id` and `name` fields.

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
| `id` | string | Yes | - | Unique output ID within the flow. Cannot be empty. |
| `name` | string | Yes | - | Human-readable display name. |
| `dest_addr` | string | Yes | - | Destination socket address (`ip:port`). For multicast, use the group address (e.g., `"239.1.2.1:5004"`). IPv6: `"[::1]:5004"`. |
| `bind_addr` | string | No | `"0.0.0.0:0"` | Source bind address. Use to control the source IP/port of outgoing packets. Must be same address family as `dest_addr`. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast send. Must be same address family as `dest_addr`. |
| `fec_encode` | object | No | `null` | SMPTE 2022-1 FEC encode parameters. See [FEC Configuration](#smpte-2022-1-fec-configuration). |
| `dscp` | integer | No | `46` | DSCP value for QoS marking (RP 2129 C10). Range 0-63. Default 46 = Expedited Forwarding (RFC 4594). |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = full MPTS passthrough; `Some(N)` = forward only program N as a rewritten single-program TS. Applied before FEC, so the receiver's FEC protects the filtered SPTS. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |

**Validation rules:**
- `id` cannot be empty.
- `dest_addr`, `bind_addr`, and `interface_addr` must all use the same address family.
- `dscp` must be 0-63.
- `program_number` must be `> 0` if set (program_number 0 is reserved for the NIT).

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
| `id` | string | Yes | - | Unique output ID within the flow. |
| `name` | string | Yes | - | Human-readable display name. |
| `dest_addr` | string | Yes | - | Destination socket address (`ip:port`). For multicast, use the group address. |
| `bind_addr` | string | No | `"0.0.0.0:0"` | Source bind address. Must be same address family as `dest_addr`. |
| `interface_addr` | string | No | `null` | Network interface IP for multicast send. |
| `dscp` | integer | No | `46` | DSCP value for QoS marking. Range 0-63. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = full MPTS passthrough; `Some(N)` = forward only program N as a rewritten single-program TS. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |

**Validation rules:**
- `id` cannot be empty.
- `dest_addr` must be a valid socket address.
- `dscp` must be 0-63.
- `program_number` must be `> 0` if set.

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
| `crypto_mode` | string | No | `null` | Cipher mode: `"aes-ctr"` (default) or `"aes-gcm"`. |
| `redundancy` | object | No | `null` | SMPTE 2022-7 redundancy for a second SRT output leg. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = full MPTS passthrough; `Some(N)` = forward only program N as a rewritten single-program TS. Applied once and mirrored to both legs when 2022-7 is enabled. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |

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
| `dest_url` | string | Yes | - | RTMP server URL. Must start with `rtmp://` or `rtmps://`. RTMPS requires the `tls` feature (enabled by default). |
| `stream_key` | string | Yes | - | Stream key for authentication with the RTMP server. Cannot be empty. |
| `reconnect_delay_secs` | integer | No | `5` | Seconds to wait before reconnecting after a failure. Must be > 0. |
| `max_reconnect_attempts` | integer | No | `null` (unlimited) | Maximum reconnection attempts. When `null`, reconnects indefinitely. |
| `program_number` | integer | No | `null` | MPTS program selector. `null` = lock onto the lowest program_number in the PAT (deterministic default); `Some(N)` = extract elementary streams from program N only. RTMP is single-program by spec, so this only changes *which* program is published. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |

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

**Limitations:**
- Output only. Segment-based transport inherently adds 1-4 seconds of latency.

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
| `video_only` | boolean | No | `false` | Only send video (audio omitted). AAC sources automatically fall back to video-only. |
| `program_number` | integer | No | `null` | MPTS program selector. `null` = lock onto the lowest program_number in the PAT (deterministic default); `Some(N)` = extract elementary streams from program N only. WebRTC is single-program by spec, so this only changes *which* program is sent. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |

**Audio:** Opus passthrough only. Opus flows natively on WebRTC paths. AAC sources fall back to video-only automatically (no C-library transcoding available).

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
  "id": "mpts-flow",
  "name": "Dual-program feed",
  "thumbnail_program_number": 1,
  "input": { "type": "udp", "bind_addr": "0.0.0.0:5020" },
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
  ]
}
```

The archive receives the full MPTS. The `prog1-viewer` UDP output sends only program 1 as a rewritten SPTS (PAT lists one entry, program 1's PMT + ES PIDs). The RTMP output publishes program 2's elementary streams. The manager UI thumbnail shows a frame from program 1.

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

bilbycast-edge automatically persists configuration changes to disk when flows are modified through the API. Flow configs (including user parameters like SRT passphrases, RTSP credentials, RTMP keys) go to `config.json`, infrastructure secrets go to `secrets.json`:

- **Create flow** (`POST /api/v1/flows`) -- Appends the new flow and saves (flow parameters stay in `config.json`).
- **Update flow** (`PUT /api/v1/flows/{id}`) -- Replaces the flow in-place and saves.
- **Delete flow** (`DELETE /api/v1/flows/{id}`) -- Removes the flow and saves.
- **Add output** (`POST /api/v1/flows/{id}/outputs`) -- Appends the output and saves.
- **Remove output** (`DELETE /api/v1/flows/{id}/outputs/{oid}`) -- Removes the output and saves.
- **Replace config** (`PUT /api/v1/config`) -- Replaces the entire config and saves.
- **Get config** (`GET /api/v1/config`) -- Returns the config with infrastructure secrets stripped. Flow parameters (passphrases, credentials, keys) are included in the response.

### Atomic writes

All config saves use an atomic write strategy: both `config.json` and `secrets.json` are written to temporary files (`.json.tmp`), then atomically renamed to the target paths. This prevents corruption if the process is interrupted during a write. `secrets.json` is written with `0600` permissions (owner-only) on Unix.

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

Use `POST /api/v1/config/reload` to re-read both `config.json` and `secrets.json` from disk. This is useful after manual edits or after deploying new config files via external tooling (e.g., Ansible, Chef).

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

## SMPTE ST 2110 (Phase 1)

bilbycast-edge supports the broadcast-audio and broadcast-data subset of
SMPTE ST 2110: **ST 2110-30** (linear PCM L16/L24), **ST 2110-31** (AES3
transparent for Dolby E and similar), and **ST 2110-40** (RFC 8331
ancillary data including SCTE-104, SMPTE 12M timecode, CEA-608/708
captions). Video essences (ST 2110-22 JPEG XS, ST 2110-20 uncompressed)
are reserved for Phase 2 and Phase 3.

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
  "id": "studio-a-stereo",
  "name": "Studio A — stereo",
  "enabled": true,
  "clock_domain": 0,
  "input": {
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
  },
  "outputs": []
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

```json
{
  "id": "perth-receive",
  "name": "Sydney → Perth contribution receiver",
  "input": {
    "type": "rtp_audio",
    "bind_addr": "0.0.0.0:5004",
    "sample_rate": 48000,
    "bit_depth": 24,
    "channels": 2,
    "packet_time_us": 4000,
    "payload_type": 97
  },
  "outputs": [
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
  ]
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

```json
{
  "id": "anc-flow",
  "name": "ANC (timecode + SCTE-104)",
  "enabled": true,
  "clock_domain": 0,
  "input": {
    "type": "st2110_40",
    "bind_addr": "239.0.0.20:5000",
    "interface_addr": "10.0.0.5",
    "payload_type": 100
  },
  "outputs": [
    {
      "type": "st2110_40",
      "id": "anc-out",
      "name": "ANC loopback",
      "dest_addr": "239.2.0.20:5000",
      "dscp": 46,
      "payload_type": 100
    }
  ]
}
```

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
| `rtp_audio.sample_rate` | `32000`, `44100`, `48000`, `88200`, `96000` |
| `rtp_audio.bit_depth` | `16`, `24` |
| `transcode.sample_rate` | `32000`, `44100`, `48000`, `88200`, `96000` |
| `transcode.bit_depth` | `16`, `20`, `24` |
| `transcode.channels` | `1`..=`16` |
| SRT/UDP/`rtp_audio` `transport_mode` | `"ts"` (default — UDP/SRT), `"rtp"` (default — `rtp_audio`), `"audio_302m"` |

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
  "version": 1,
  "server": { "...": "..." },
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

