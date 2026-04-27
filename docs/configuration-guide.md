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
  - [Media Player Input](#media-player-input)
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
| `inputs` | array | No | `[]` | Top-level input definitions. Each is an `InputDefinition` with `id`, `name`, and flattened protocol-specific fields (enum-tagged by `type`). See [Input Types](#input-types). Inputs exist independently and are referenced by flows via `input_ids`. |
| `outputs` | array | No | `[]` | Top-level output definitions. Each is an `OutputConfig` with `id`, `name`, and protocol-specific fields (enum-tagged by `type`). See [Output Types](#output-types). Outputs exist independently and are referenced by flows via `output_ids`. |
| `flows` | array | No | `[]` | List of flow configurations. Each flow references one or more inputs (one active at a time) and zero or more outputs by ID. See [Flow Configuration](#flow-configuration). |
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

### Tunnel Validation Rules

- `id` must be a valid UUID.
- `relay_addrs` (or legacy `relay_addr`) required when `mode` is `"relay"`; at least one, at most two entries; each 1–256 chars; duplicates rejected.
- `tunnel_encryption_key` required for relay mode; must be exactly 64 hex characters.
- `tunnel_bind_secret` must be exactly 64 hex characters if present.
- `peer_addr` required for direct mode egress.
- `direct_listen_addr` required for direct mode ingress.
- `tunnel_psk` must be exactly 64 hex characters if present.
- All address fields must be valid socket addresses.

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

## Input Types

Each entry in the top-level `inputs` array is an `InputDefinition` with `id`, `name`, and the protocol-specific fields flattened in (enum-tagged by `type`). Inputs are independent top-level entities that exist whether or not they are assigned to a flow. They are managed via REST at `/api/v1/inputs` (CRUD) and via manager WebSocket commands.

The `type` discriminator field determines which input variant is used: `rtp`, `udp`, `srt`, `rtmp`, `rtsp`, `webrtc`, or `whep`.

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

**Validation rules:**
- `bind_addr` must be a valid `ip:port` socket address.
- `interface_addr` must be a valid IP address (no port) in the same address family as `bind_addr`.
- `source_addr` is only valid when `bind_addr` is multicast, must be a unicast IP, and must share the address family of `bind_addr`.
- `allowed_payload_types` values must be 0-127.
- `max_bitrate_mbps` must be positive.

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

**Validation rules:**
- `bind_addr` must be a valid `ip:port` socket address.
- `interface_addr` must be a valid IP address in the same address family as `bind_addr`.
- `source_addr` rules: see [RTP Input](#rtp-input) above.

### Source-Specific Multicast (SSM) vs Any-Source Multicast (ASM)

Multicast inputs default to **ASM** (`(*,G)`) — the kernel joins the group and accepts traffic from any source on that group address. This requires PIM-RP (Rendezvous Point) routing infrastructure and offers no per-source filtering.

Setting `source_addr` switches the join to **SSM** (`(S,G)`, RFC 3678). Benefits:

- **Skips PIM-RP** — SSM joins flow directly toward the source via PIM-SSM (no rendezvous tree).
- **Per-source filtering at the kernel** — packets from any other source on the same group are dropped before reaching userspace.
- **Required by many ST 2110 / ST 2059 broadcast plants** — production routers commonly require `(S,G)` joins for guaranteed traffic isolation.

SSM works on any multicast group; the kernel doesn't enforce the IANA SSM ranges (232.0.0.0/8 for IPv4, ff3x::/32 for IPv6). The IPv6 SSM join uses `MCAST_JOIN_SOURCE_GROUP` (Linux + macOS only — other targets fail with a clear error).

For SMPTE 2022-7 dual-leg inputs, each leg has its own `source_addr` field — real Red/Blue plants typically have different source IPs per network. Set `source_addr` on the parent input for the primary (Red) leg, and `redundancy.source_addr` for the secondary (Blue) leg.

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

### Media Player Input

Replays one or more local files (MPEG-TS, MP4 / MOV / MKV, or still images)
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
| `shuffle` | boolean | No | `false` | Randomise source order each time the playlist starts. |
| `paced_bitrate_bps` | integer | No | `null` | TS-only override for the egress pacer when the source has no usable PCR. Range 100 000 – 200 000 000 (100 kbps – 200 Mbps). Leave `null` to pace from PCR (default for any healthy TS asset). |

**Source variants** (tagged by `kind`):

| Field | Type | Required | Default | Applies to | Description |
|-------|------|----------|---------|------------|-------------|
| `kind` | string | Yes | - | all | `"ts"`, `"mp4"`, or `"image"`. |
| `name` | string | Yes | - | all | Filename within the media library. ASCII alphanumeric plus `._- ` only, 1–255 chars, no leading dot, no path separators. |
| `fps` | integer | No | `5` | `image` | Frames per second to render. Range 1–60. |
| `bitrate_kbps` | integer | No | `250` | `image` | Encoded video bitrate. Range 50–50 000 (50 kbps – 50 Mbps). |
| `audio_silence` | boolean | No | `true` | `image` | Pair the rendered video with silent stereo AAC so downstream demuxers don't complain about a missing audio PID. |

**Media library directory** — the on-disk location where uploaded files
are stored on the edge. Resolution order:

1. `BILBYCAST_MEDIA_DIR` env var (recommended for production — pin a specific directory).
2. `$XDG_DATA_HOME/bilbycast/media/`.
3. `$HOME/.bilbycast/media/`.
4. `./media/` (cwd fallback).

Files are written `0644`. Per-asset cap: **4 GiB** (`MAX_FILE_BYTES`).
Library cap: **16 GiB** total (`MAX_TOTAL_BYTES`). Partial uploads stage
under `<media_dir>/.tmp/<name>.<session_id>` and are reaped after 1 hour
of inactivity.

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

**Validation rules:**
- `id` cannot be empty.
- `dest_addr`, `bind_addr`, and `interface_addr` must all use the same address family.
- `dscp` must be 0-63.
- `program_number` must be `> 0` if set (program_number 0 is reserved for the NIT).
- `delay`: `fixed` ms 0-10000; `target_ms` ms 1-10000; `target_frames` frames 0.01-300, fallback_ms 0-10000.

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

**Validation rules:**
- `id` cannot be empty.
- `dest_addr` must be a valid socket address.
- `dscp` must be 0-63.
- `program_number` must be `> 0` if set.
- `delay`: same validation as RTP output. Incompatible with `transport_mode: "audio_302m"`.

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
| `local_addr` | string | Yes | - | Local socket address to bind. Use `"0.0.0.0:0"` for caller mode (ephemeral port). |
| `remote_addr` | string | Conditional | `null` | Remote address. Required for `caller` and `rendezvous`. |
| `latency_ms` | integer | No | `120` | SRT send latency in milliseconds. |
| `passphrase` | string | No | `null` | AES encryption passphrase (10-79 characters). |
| `aes_key_len` | integer | No | `16` | AES key length: 16, 24, or 32. |
| `crypto_mode` | string | No | `null` | Cipher mode: `"aes-ctr"` (default) or `"aes-gcm"`. |
| `redundancy` | object | No | `null` | SMPTE 2022-7 redundancy for a second SRT output leg. |
| `program_number` | integer | No | `null` | MPTS → SPTS program filter. `null` = full MPTS passthrough; `Some(N)` = forward only program N as a rewritten single-program TS. Applied once and mirrored to both legs when 2022-7 is enabled. Must be `> 0`. See [MPTS → SPTS filtering](#mpts--spts-filtering). |
| `delay_ms` | integer | No | `null` | Output delay in milliseconds (0–10000). When set and > 0, packets are buffered and released after this delay. Used for synchronizing parallel outputs with different processing latencies. Incompatible with `transport_mode: "audio_302m"`. |

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
- `video_encode` requires the `video-thumbnail` feature plus a matching `video-encoder-x264` / `-x265` / `-nvenc` / `-qsv` backend compiled in.
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

---

## Flow Assembly (PID bus — SPTS / MPTS from N inputs)

A flow can optionally carry an `assembly` block that tells the runtime to stop forwarding one input verbatim and instead **build a fresh MPEG-TS from elementary streams pulled off any of the flow's inputs**. The same broadcast channel that a passthrough flow uses then carries the assembled TS, so every existing output type consumes it unchanged — UDP, RTP (with or without 2022-1 FEC / 2022-7), SRT (incl. bonded / 2022-7), RIST (incl. ARQ), RTMP/RTMPS, HLS, CMAF / CMAF-LL (incl. ClearKey CENC), WebRTC WHIP/WHEP. RTMP and WebRTC demux one program out by default (lowest `program_number` in the PAT, or the output-level `program_number` override).

Flows without an `assembly` block — or with `assembly.kind = passthrough` — run exactly as before. Existing configs are unaffected.

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

Every slot in a program's `streams[]` has a `source` picked from three variants:

- **`"pid"`** — explicit PID off a named input: `{ "type": "pid", "input_id": "...", "source_pid": 256 }`. Use when the operator knows the exact upstream PID (picked from the input's live PSI catalogue, or published in a written spec).
- **`"essence"`** — first ES of a given kind off a named input: `{ "type": "essence", "input_id": "...", "kind": "video" | "audio" | "subtitle" | "data" }`. Useful when the upstream input is single-program and the operator just wants "its video" / "its audio" without binding to a specific PID. Resolves at flow start against the input's PSI catalogue (Phase 2); re-resolves on `UpdateFlowAssembly`.
- **`"hitless"`** — primary-preference pre-bus merger: `{ "type": "hitless", "primary": { <slot source> }, "backup": { <slot source> } }`. Both legs publish onto the bus independently; a merger task forwards primary verbatim and flips to backup if no primary packet arrives for 200 ms. Primary traffic resumption brings the merger back after a short hold-off. **Not SMPTE 2022-7 sequence-aware dedup** — the bus today doesn't carry upstream RTP sequence numbers. Either nested leg must itself be `pid` or `essence`; a Hitless nested inside another Hitless is rejected.

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

Assembly owns the PID layout of the TS it produces (`out_pid` per slot, `pmt_pid` per program, whatever the PCR slot got assigned). An output's `pid_map` (see [TS output PID remapping](#ts-output-pid-remapping)) applies **after** the assembly on the way out, so you can publish one assembled PID layout and then re-label it per output if an external downstream has hard-coded PID expectations. Not recommended as the default path — pick `out_pid` values that already match downstream expectations in the assembly.

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
    "listen_addr": "0.0.0.0",
    "listen_port": 8080
  },
  "inputs": [],
  "outputs": [],
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
    build time for inputs; the `video-thumbnail` feature (default on)
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
  E-AC-3 PIDs *inside MPEG-TS* are tracked (bitrate / presence /
  silence-proxy) but their R128 decode stays deferred with a
  `codec_decoded: false` + `decode_note` in the snapshot until the
  libavcodec audio bridge lands.
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
    "max_bytes": 53687091200
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
`replay_index_corrupt`).

### Metrics

Per-recording counters surfaced on the WS stats path:
`segments_written`, `bytes_written`, `segments_pruned`,
`packets_dropped`, `index_entries`, `current_pts_90khz`. See
[`metrics.md`](metrics.md#replay-server-metrics).
