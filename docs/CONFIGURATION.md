# Configuration Reference

bilbycast-edge is configured via two JSON files:

- **`config.json`** (default: `./config.json`) — Operational configuration: server addresses, flow definitions (including user-configured parameters like SRT passphrases, RTSP credentials, RTMP keys, bearer tokens), tunnel routing, monitor settings. When the manager requests the node's config (`GetConfig`), this file's content is returned with infrastructure secrets stripped.
- **`secrets.json`** (auto-derived: same directory as `config.json`) — Infrastructure credentials only: manager auth secrets, tunnel encryption keys, API auth config (JWT secret, client secrets), TLS cert/key paths. This file is written with `0600` permissions (owner-only) on Unix and **never leaves the node**. Flow-level user parameters (SRT passphrases, RTSP credentials, RTMP stream keys, WebRTC bearer tokens, HLS auth tokens) stay in `config.json` so they are visible in the manager UI and survive round-trip config updates.

At runtime, both files are merged into a single `AppConfig` in memory. The split is purely a persistence and serialization boundary — existing code that reads config works unchanged.

**Migration from single file**: If you upgrade from a version that used a single `config.json` containing secrets, the node automatically detects this on first startup, extracts secrets into `secrets.json`, and rewrites `config.json` without secrets. No manual action is required.

---

## Config Persistence

The edge node's config files are the **source of truth** for its configuration. The manager does not store node configs in its database — it only sends commands, and the edge is responsible for persisting its own state.

### When config is saved to disk

Both `config.json` and `secrets.json` are written to disk immediately after any of these events:

- **First startup** — auto-generated `node_id` is saved to `config.json`
- **Manager registration** — `node_id` saved to `config.json`, `node_secret` saved to `secrets.json` (registration token cleared)
- **REST API flow operations** — creating, updating, or deleting flows via the local API (flow parameters including SRT passphrases, RTSP credentials, RTMP keys stay in `config.json`)
- **Manager `update_config` command** — operational fields update `config.json`, existing secrets are preserved in `secrets.json`
- **Manager `create_tunnel` command** — tunnel routing goes to `config.json`, tunnel encryption keys go to `secrets.json`
- **Setup wizard** — operational settings go to `config.json`, registration token goes to `secrets.json`

### Atomic writes

Config saves use an atomic two-phase write: the JSON is first written to a temporary file (`.json.tmp`), then renamed to the actual config path. This prevents partial or corrupted config files if the node crashes or loses power mid-write. Both `config.json` and `secrets.json` use this approach.

### Manager config update behavior

Both `update_config` (full config replacement) and `update_flow` (single flow update) use **diff-based** logic to minimize disruption to running flows. Only changed components are restarted; unchanged flows, outputs, and tunnels continue running uninterrupted.

When the manager sends an `update_config` or `update_flow` command:

1. The manager sends the full operational config (not a diff, and without secrets — since `GetConfig` strips them) — the edge computes the diff internally
2. The edge **merges existing secrets** from its local `secrets.json` into the incoming config, then **validates** the merged config before applying it
3. **Flows** are compared by ID between old and new config:
   - Removed flows → destroyed
   - Added flows → created
   - Unchanged flows → **not touched** (input and all outputs keep running)
   - Changed flows: if only outputs changed, outputs are surgically hot-added/removed; if the input or metadata changed, the flow is fully restarted
4. **Tunnels** are compared similarly — only changed tunnels are restarted
5. **Outputs** within a flow are diffed by ID:
   - Removed outputs → hot-removed (other outputs unaffected)
   - Added outputs → hot-added (subscribes to existing broadcast channel)
   - Changed outputs (different config) → removed and re-added
   - Unchanged outputs → **not touched** (SRT/RTP connections survive)
6. The in-memory config is updated and **saved to disk** (operational fields to `config.json`, secrets to `secrets.json`)
7. A `command_ack` is sent back to the manager

**Fields replaced** by UpdateConfig: `flows`, `tunnels`, `server`, `monitor`

**Fields preserved** (not overwritten): `version`, `node_id`, `device_name`, `setup_enabled`, `manager` (connection credentials)

### Reboot behavior

On startup, the edge loads its config from both `config.json` and `secrets.json`, merging them into a single in-memory `AppConfig`. Since all changes are persisted immediately, the node will always start with the latest config — including any changes made remotely through the manager.

---

## Full Config Structure

```json
{
  "version": 1,
  "node_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "device_name": "Studio-A Encoder",
  "setup_enabled": true,
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8080,
    "tls": null,
    "auth": null
  },
  "monitor": null,
  "manager": null,
  "flows": [],
  "tunnels": []
}
```

## Node Identity & Setup

| Field     | Type      | Default | Description |
|-----------|-----------|---------|-------------|
| `node_id` | `string?` | Auto-generated | Persistent UUID v4 identifying this edge node. Auto-generated on first startup and saved to config. Used as the NMOS IS-04 Node ID. All NMOS resource UUIDs are derived from this value (UUID v5), so they remain stable across restarts. |
| `device_name` | `string?` | `null` | Optional human-readable label for this edge node (e.g. "Studio-A Encoder"). Max 256 characters. |
| `setup_enabled` | `bool` | `true` | When true, the browser-based setup wizard is accessible at `/setup`. Automatically flipped to `false` (and persisted) after the node's first successful manager registration; operators can also flip it manually. |

---

## Server Section

API server configuration.

| Field         | Type          | Default      | Description                          |
|---------------|---------------|--------------|--------------------------------------|
| `listen_addr` | `string`      | `"0.0.0.0"`  | API listen address                   |
| `listen_port` | `u16`         | `8080`       | API listen port                      |
| `tls`         | `TlsConfig?`  | `null`       | Optional TLS for HTTPS (see below)   |
| `auth`        | `AuthConfig?`  | `null`       | Optional API authentication (see below) |

### TLS Configuration

> **Note:** The `tls` section is stored in `secrets.json`, not `config.json`, since it contains paths to sensitive key material.

Enables HTTPS on the API server. The `tls` feature is enabled by default.

| Field       | Type     | Description                          |
|-------------|----------|--------------------------------------|
| `cert_path` | `string` | Path to PEM-encoded TLS certificate  |
| `key_path`  | `string` | Path to PEM-encoded TLS private key  |

```json
{
  "server": {
    "listen_addr": "0.0.0.0",
    "listen_port": 8443,
    "tls": {
      "cert_path": "certs/server.crt",
      "key_path": "certs/server.key"
    }
  }
}
```

### Authentication Configuration

> **Note:** The `auth` section is stored in `secrets.json`, not `config.json`, since it contains the JWT signing secret and client credentials.

OAuth 2.0 client credentials authentication with JWT (HMAC-SHA256). When absent or `enabled: false`, all API endpoints are unauthenticated.

| Field                  | Type           | Default | Description                                    |
|------------------------|----------------|---------|------------------------------------------------|
| `enabled`              | `bool`         | --      | Master switch for authentication               |
| `jwt_secret`           | `string`       | --      | HMAC-SHA256 secret for signing JWTs            |
| `token_lifetime_secs`  | `u64`          | `3600`  | Token validity period in seconds               |
| `clients`              | `AuthClient[]` | --      | List of registered OAuth clients               |
| `public_metrics`       | `bool`         | `true`  | Allow unauthenticated access to `/metrics` and `/health` |
| `nmos_require_auth`    | `Option<bool>` | unset → `true` when `enabled: true` | Require JWT Bearer auth on NMOS IS-04/IS-05/IS-08 endpoints. Secure-by-default: when `enabled: true` and the field is unset, NMOS is protected. Set to `false` to explicitly opt out (logs a `SECURITY:` warning). |
| `token_rate_limit_per_minute` | `u32`   | `10`    | Max OAuth token requests per minute per IP (0 = disabled) |

Each client entry:

| Field           | Type     | Description                                    |
|-----------------|----------|------------------------------------------------|
| `client_id`     | `string` | Client identifier                              |
| `client_secret` | `string` | Client secret                                  |
| `role`          | `string` | `"admin"` (full access) or `"monitor"` (read-only) |

To obtain a token, POST to `/oauth/token` with `grant_type=client_credentials`, `client_id`, and `client_secret`.

---

## Monitor Section

Optional web monitoring dashboard. When present, bilbycast-edge starts a second HTTP server serving a self-contained HTML dashboard for browser-based status monitoring.

| Field         | Type     | Description                          |
|---------------|----------|--------------------------------------|
| `listen_addr` | `string` | Dashboard listen address             |
| `listen_port` | `u16`    | Dashboard listen port                |

Omit this section entirely to disable the dashboard.

```json
{
  "monitor": {
    "listen_addr": "0.0.0.0",
    "listen_port": 9090
  }
}
```

---

## Manager Section

Optional connection to a bilbycast-manager instance for centralized monitoring and remote control.

| Field                | Type      | Default | Description                                     |
|----------------------|-----------|---------|-------------------------------------------------|
| `enabled`            | `bool`    | `false` | Enable the manager connection                   |
| `url`                | `string`  | --      | Manager WebSocket URL, e.g. `"wss://manager-host:8443/ws/node"` (must use `wss://`) |
| `accept_self_signed_cert` | `bool` | `false` | Accept self-signed TLS certificates from the manager. **Dev/testing only** — disables cert validation. Requires `BILBYCAST_ALLOW_INSECURE=1` env var as a safety guard. |
| `cert_fingerprint`   | `string?` | `null`  | SHA-256 fingerprint of the manager's TLS certificate for certificate pinning. Format: hex-encoded with colons, e.g. `"ab:cd:ef:01:23:..."`. When set, connections to servers with a different certificate are rejected, even if CA-signed. Protects against compromised CAs and targeted MITM attacks. The server's fingerprint is logged on first connection for easy pinning. |
| `registration_token` | `string?` | `null`  | One-time registration token (first connection only). **Stored in `secrets.json`.** |
| `node_id`            | `string?` | `null`  | Assigned node ID (set automatically after registration) |
| `node_secret`        | `string?` | `null`  | Assigned node secret (set automatically after registration). **Stored in `secrets.json`.** |

On first connection, provide `registration_token`. After successful registration, the manager returns `node_id` and `node_secret`. The `node_id` is saved to `config.json`, while `node_secret` is saved to `secrets.json`. The `registration_token` is cleared from both files. Subsequent connections use `node_id` and `node_secret`.

**config.json** (before registration):
```json
{
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node"
  }
}
```

**secrets.json** (before registration, created by setup wizard):
```json
{
  "version": 1,
  "manager_registration_token": "abc123-one-time-token"
}
```

After registration, the files are updated to:

**config.json**:
```json
{
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "node_id": "assigned-uuid"
  }
}
```

**secrets.json**:
```json
{
  "version": 1,
  "manager_node_secret": "assigned-secret"
}
```

---

## Tunnels Section

An array of IP tunnel definitions. Tunnels create encrypted point-to-point links between edge nodes, either through a bilbycast-relay server (for NAT traversal) or directly via QUIC (when one edge has a public IP).

| Field                  | Type      | Default | Description |
|------------------------|-----------|---------|-------------|
| `id`                   | `string`  | --      | Unique tunnel identifier. Must be a valid UUID. |
| `name`                 | `string`  | --      | Human-readable tunnel name |
| `enabled`              | `bool`    | `true`  | Whether this tunnel is active |
| `protocol`             | `string`  | --      | Transport protocol: `"tcp"` (reliable, ordered — QUIC streams) or `"udp"` (unreliable — QUIC datagrams, best for SRT/media) |
| `mode`                 | `string`  | --      | Connectivity mode: `"relay"` (both edges behind NAT, traffic via relay) or `"direct"` (direct QUIC between edges) |
| `direction`            | `string`  | --      | This edge's role: `"ingress"` (receives tunnel traffic, forwards to `local_addr`) or `"egress"` (listens on `local_addr`, sends into tunnel) |
| `local_addr`           | `string`  | --      | Local address. For **egress**: listen address for local traffic to tunnel (e.g. `"0.0.0.0:9000"`). For **ingress**: forward address for received tunnel traffic (e.g. `"127.0.0.1:9000"`). |
| `relay_addrs`          | `string[]`| `[]`    | Ordered relay server QUIC addresses. Index 0 is the primary; a second entry enables automatic primary↔backup failover. Max 2 entries. **Required for relay mode.** |
| `relay_addr`           | `string?` | `null`  | **Legacy.** Single relay address. Accepted on load and migrated into `relay_addrs[0]`. Prefer `relay_addrs`. |
| `max_rtt_failback_increase_ms` | `u32?` | `50` | RTT gate for failback from backup to primary. Failback is refused if the primary's measured RTT exceeds the active backup's by more than this many ms. |
| `tunnel_encryption_key`| `string?` | `null`  | End-to-end encryption key (hex-encoded, exactly 64 chars = 32 bytes). Both edges must have the same key (distributed by manager). **Required for relay mode** (relay sees only ciphertext). Optional for direct mode (defense-in-depth). **Stored in `secrets.json`.** |
| `tunnel_bind_secret`   | `string?` | `null`  | Shared secret for relay bind authentication (hex-encoded, exactly 64 chars = 32 bytes). Used to compute HMAC-SHA256 bind tokens. **Stored in `secrets.json`.** |
| `peer_addr`            | `string?` | `null`  | Remote peer QUIC address, e.g. `"203.0.113.50:4433"`. **Required for direct mode, egress direction.** |
| `direct_listen_addr`   | `string?` | `null`  | QUIC listen address, e.g. `"0.0.0.0:4433"`. **Required for direct mode, ingress direction.** |
| `tunnel_psk`           | `string?` | `null`  | Pre-shared key for direct mode HMAC-SHA256 authentication (hex-encoded, 64 chars). Both edges must share the same PSK. **Stored in `secrets.json`.** |
| `tls_cert_pem`         | `string?` | `null`  | TLS certificate PEM for direct mode listener. Auto-generated if absent. **Stored in `secrets.json`.** |
| `tls_key_pem`          | `string?` | `null`  | TLS key PEM for direct mode listener. Auto-generated if absent. **Stored in `secrets.json`.** |

### Relay Mode Example

Both edges behind NAT, traffic forwarded through a bilbycast-relay server. End-to-end encrypted — the relay cannot read payloads.

**Edge A (egress — captures local SRT traffic and tunnels it):**
```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "name": "Stadium to Studio Tunnel",
  "enabled": true,
  "protocol": "udp",
  "mode": "relay",
  "direction": "egress",
  "local_addr": "0.0.0.0:9000",
  "relay_addrs": [
    "relay-primary.example.com:4433",
    "relay-backup.example.com:4433"
  ],
  "tunnel_encryption_key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "tunnel_bind_secret": "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
}
```

**Edge B (ingress — receives tunnel traffic and forwards to local address):**
```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "name": "Stadium to Studio Tunnel",
  "enabled": true,
  "protocol": "udp",
  "mode": "relay",
  "direction": "ingress",
  "local_addr": "127.0.0.1:9000",
  "relay_addrs": [
    "relay-primary.example.com:4433",
    "relay-backup.example.com:4433"
  ],
  "tunnel_encryption_key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "tunnel_bind_secret": "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
}
```

Both edges must use the same `id`, `relay_addrs` ordering, `tunnel_encryption_key`, and `tunnel_bind_secret`. The manager distributes these automatically when creating tunnels. With two entries in `relay_addrs`, the edges automatically fail over to the backup when the primary goes down and fail back when the primary recovers (RTT-gated). See [Redundant Relay Failover](configuration-guide.md#redundant-relay-failover) in the configuration guide for the timing budget.

If only a single relay is needed, `relay_addrs` may contain just one entry (or the legacy `"relay_addr": "host:port"` form is still accepted).

### Direct Mode Example

One edge has a public IP. Direct QUIC connection — no relay needed.

**Edge A (ingress — listens for QUIC connection):**
```json
{
  "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
  "name": "Direct Link",
  "enabled": true,
  "protocol": "tcp",
  "mode": "direct",
  "direction": "ingress",
  "local_addr": "127.0.0.1:9000",
  "direct_listen_addr": "0.0.0.0:4433",
  "tunnel_psk": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
}
```

**Edge B (egress — connects to Edge A's public address):**
```json
{
  "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
  "name": "Direct Link",
  "enabled": true,
  "protocol": "tcp",
  "mode": "direct",
  "direction": "egress",
  "local_addr": "0.0.0.0:9000",
  "peer_addr": "203.0.113.50:4433",
  "tunnel_psk": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
}
```

### Validation Rules

- `id` must be a valid UUID
- `relay_addrs` (or legacy `relay_addr`) required when `mode` is `relay`; at least one, at most two entries; duplicates rejected
- `tunnel_encryption_key` required when `mode` is `relay`, must be exactly 64 hex chars (32 bytes)
- `tunnel_bind_secret` must be exactly 64 hex chars if present
- `peer_addr` required when `mode` is `direct` and `direction` is `egress`
- `direct_listen_addr` required when `mode` is `direct` and `direction` is `ingress`
- `tunnel_psk` must be exactly 64 hex chars if present
- All address fields must be valid socket addresses

---

## Flows Section

An array of flow definitions. Each flow has one or more inputs (one active at a time) and one or more outputs.

| Field     | Type             | Default | Description                              |
|-----------|------------------|---------|------------------------------------------|
| `id`              | `string`         | --      | Unique flow identifier                   |
| `name`            | `string`         | --      | Human-readable name                      |
| `enabled`         | `bool`           | `true`  | Auto-start when the application starts   |
| `media_analysis`  | `bool`           | `true`  | Enable media content analysis (codec, resolution, frame rate detection). Set to `false` to save CPU on resource-constrained devices. |
| `thumbnail`       | `bool`           | `true`  | Enable thumbnail generation for visual flow preview (in-process via libavcodec; no external ffmpeg required). |
| `thumbnail_program_number` | `u16?`   | `null`  | When the input is an MPTS, render the thumbnail from this MPEG-TS program only. `null` uses the first program found. Must be `> 0` if set. See the **[MPTS → SPTS filtering](#mpts--spts-filtering)** section. |
| `bandwidth_limit` | `BandwidthLimitConfig?` | `null` | Per-flow bandwidth monitoring for trust boundary enforcement (RP 2129). See below. |
| `input_ids`       | `string[]`       | `[]`    | One or more input sources (one active at a time) |
| `outputs`         | `OutputConfig[]` | --      | One or more output destinations          |

### Bandwidth Limit (`bandwidth_limit`)

Optional per-flow bandwidth monitoring for SMPTE RP 2129 trust boundary enforcement. Monitors the flow's input bitrate and takes action if it exceeds the configured limit for the grace period.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_bitrate_mbps` | `f64` | -- | Expected maximum bitrate in megabits per second (0 < value ≤ 10000) |
| `action` | `string` | -- | Action when exceeded: `"alarm"` (raise warning event, flag on dashboard) or `"block"` (drop all packets until bandwidth normalizes) |
| `grace_period_secs` | `u32` | `5` | Seconds the bitrate must continuously exceed the limit before triggering (1-60). Prevents false positives from transient spikes. |

**Example:**
```json
{
  "id": "studio-feed",
  "name": "Studio Feed",
  "bandwidth_limit": {
    "max_bitrate_mbps": 25.0,
    "action": "alarm",
    "grace_period_secs": 5
  },
  "input": { "type": "srt", "mode": "listener", "local_addr": "0.0.0.0:9000" },
  "outputs": [...]
}
```

**Alarm action:** Emits a `warning` event and flags the flow on the dashboard. The flow continues operating normally. When bitrate returns to normal, an `info` recovery event is emitted.

**Block action:** Gates the flow — all incoming packets are dropped while the bitrate exceeds the limit. The flow stays alive and automatically resumes when bandwidth normalizes. A probe-and-check mechanism periodically samples real traffic to detect recovery. Blocked packets are counted in `packets_filtered`.

---

## Input Types

The input is discriminated by the `"type"` field.

### SRT Input (`"type": "srt"`)

| Field         | Type                    | Default | Description                                     |
|---------------|-------------------------|---------|-------------------------------------------------|
| `type`             | `"srt"`                 | --      | Input type discriminator                        |
| `mode`             | `SrtMode`               | --      | `"caller"`, `"listener"`, or `"rendezvous"`     |
| `local_addr`       | `string`                | --      | Local bind address, e.g. `"0.0.0.0:9000"`      |
| `remote_addr`      | `string?`               | `null`  | Remote address (required for caller/rendezvous) |
| `latency_ms`       | `u64`                   | `120`   | SRT latency in milliseconds (sets both receiver and peer/sender latency) |
| `recv_latency_ms`  | `u64?`                  | `null`  | Receiver-side latency override in ms. When set, overrides `latency_ms` for the receiver buffer only. |
| `peer_latency_ms`  | `u64?`                  | `null`  | Peer/sender-side latency override in ms. When set, overrides `latency_ms` for the sender's minimum latency request. |
| `peer_idle_timeout_secs` | `u64`              | `30`    | Connection dropped if no data received for this many seconds |
| `passphrase`       | `string?`               | `null`  | AES encryption passphrase (10-79 characters)    |
| `aes_key_len`      | `usize?`                | `null`  | AES key length: 16, 24, or 32                   |
| `crypto_mode`      | `string?`               | `null`  | Cipher mode: `"aes-ctr"` (default) or `"aes-gcm"` (authenticated encryption). AES-GCM requires libsrt >= 1.5.2 on the peer and only supports AES-128/256 (not AES-192). |
| `max_rexmit_bw`    | `i64?`                  | `null`  | Maximum retransmission bandwidth in bytes/sec. `-1` = unlimited, `0` = disable retransmissions, `> 0` = cap in bytes/sec. Uses Token Bucket shaper. |
| `stream_id`        | `string?`               | `null`  | SRT Stream ID for access control (max 512 chars). **Caller:** sent to the listener during handshake for stream identification/routing. **Listener:** if set, only callers with a matching stream_id are accepted (others are rejected). Supports plain strings or the structured `#!::r=name,m=publish,u=user` format per the SRT Access Control spec. |
| `packet_filter`    | `string?`               | `null`  | SRT FEC (Forward Error Correction) config string. Format: `"fec,cols:10,rows:5,layout:staircase,arq:onreq"`. **cols:** row group size (1-256). **rows:** column group size (1 = row-only, >1 = 2D FEC, 1-256). **layout:** `"even"` or `"staircase"` (default, spreads FEC packets evenly). **arq:** `"always"` (ARQ+FEC parallel), `"onreq"` (FEC first, then ARQ, default), `"never"` (FEC only). Negotiated with peer during handshake — both sides must agree on parameters. |
| `redundancy`       | `SrtRedundancyConfig?`  | `null`  | SMPTE 2022-7 second leg configuration           |

### RTP Input (`"type": "rtp"`)

Receives RTP-wrapped MPEG-TS packets (SMPTE ST 2022-2), VSF TR-07, or generic RTP streams. Requires valid RTP v2 headers — non-RTP packets are silently dropped. Supports SMPTE 2022-7 hitless redundancy (dual-leg merge) and SMPTE 2022-1 FEC recovery. Use the UDP input type for raw TS without RTP headers.

| Field                    | Type                    | Default | Description                                     |
|--------------------------|-------------------------|---------|-------------------------------------------------|
| `type`                   | `"rtp"`                 | --      | Input type discriminator                        |
| `bind_addr`              | `string`                | --      | Local bind address (leg 1), e.g. `"0.0.0.0:5000"` or `"239.1.1.1:5000"` for multicast |
| `interface_addr`         | `string?`               | `null`  | Network interface IP for multicast join          |
| `fec_decode`             | `FecConfig?`            | `null`  | SMPTE 2022-1 FEC decode parameters (applied per-leg before merge) |
| `tr07_mode`              | `bool?`                 | `null`  | Enable VSF TR-07 (JPEG XS) compliance reporting |
| `allowed_sources`        | `string[]?`             | `null`  | Source IP allowlist (RP 2129 C5)                 |
| `allowed_payload_types`  | `u8[]?`                 | `null`  | RTP payload type allowlist (RP 2129 U4)          |
| `max_bitrate_mbps`       | `f64?`                  | `null`  | Maximum ingress bitrate in Mbps (RP 2129 C7)     |
| `redundancy`             | `RtpRedundancyConfig?`  | `null`  | SMPTE 2022-7 second leg configuration           |

### UDP Input (`"type": "udp"`)

Receives raw UDP datagrams without requiring RTP headers. Suitable for raw MPEG-TS over UDP from OBS, ffmpeg (`-f mpegts udp://`), srt-live-transmit, VLC, or any source that sends plain TS.

| Field            | Type       | Default | Description                                     |
|------------------|------------|---------|-------------------------------------------------|
| `type`           | `"udp"`    | --      | Input type discriminator                        |
| `bind_addr`      | `string`   | --      | Local bind address, e.g. `"0.0.0.0:5000"` or `"239.1.1.1:5000"` for multicast |
| `interface_addr`  | `string?` | `null`  | Network interface IP for multicast join          |

### RTMP Input (`"type": "rtmp"`)

Accepts incoming RTMP publish connections from OBS, ffmpeg, Wirecast, or any RTMP encoder. The received H.264 video and AAC audio are remuxed into MPEG-TS and pushed through the flow pipeline.

| Field            | Type      | Default  | Description                                     |
|------------------|-----------|----------|-------------------------------------------------|
| `type`           | `"rtmp"`  | --       | Input type discriminator                        |
| `listen_addr`    | `string`  | --       | RTMP listen address, e.g. `"0.0.0.0:1935"`     |
| `app`            | `string`  | `"live"` | RTMP application name (URL path component)      |
| `stream_key`     | `string?` | `null`   | Stream key for authentication (null = accept any)|
| `max_publishers` | `u32`     | `1`      | Max simultaneous publishers                     |

**Example:**

```json
{
  "type": "rtmp",
  "listen_addr": "0.0.0.0:1935",
  "app": "live",
  "stream_key": "my_secret_key"
}
```

**Publishing from ffmpeg:**
```bash
ffmpeg -re -i input.mp4 -c:v libx264 -c:a aac -f flv rtmp://edge:1935/live/my_secret_key
```

**Publishing from OBS:**
- Server: `rtmp://edge:1935/live`
- Stream Key: `my_secret_key`

### RTSP Input (`"type": "rtsp"`)

Pulls H.264 or H.265/HEVC video and AAC audio from RTSP sources (IP cameras, media servers). Uses the `retina` pure-Rust RTSP client. Produces MPEG-TS with proper PAT/PMT program tables. Audio-only streams are supported.

| Field                 | Type     | Default  | Description                                     |
|-----------------------|----------|----------|-------------------------------------------------|
| `type`                | `"rtsp"` | --       | Input type discriminator                        |
| `rtsp_url`            | `string` | --       | RTSP source URL, e.g. `"rtsp://camera:554/stream1"` |
| `username`            | `string?`| `null`   | Username for RTSP authentication                 |
| `password`            | `string?`| `null`   | Password for RTSP authentication                 |
| `transport`           | `string` | `"tcp"`  | `"tcp"` (interleaved, default) or `"udp"`        |
| `timeout_secs`        | `u64`    | `10`     | Connection timeout in seconds                    |
| `reconnect_delay_secs`| `u64`    | `5`      | Delay between reconnection attempts              |

### WebRTC/WHIP Input (`"type": "webrtc"`)

Accepts incoming WebRTC contributions from publishers (OBS, browsers) via the WHIP protocol (RFC 9725). The WHIP endpoint is auto-generated at `/api/v1/flows/{flow_id}/whip`. The `webrtc` feature is enabled by default.

| Field          | Type      | Default | Description                                     |
|----------------|-----------|---------|-------------------------------------------------|
| `type`         | `"webrtc"`| --      | Input type discriminator                        |
| `bearer_token` | `string?` | `null`  | Bearer token required from WHIP publishers       |
| `video_only`   | `bool`    | `false` | Ignore audio tracks from publisher               |
| `public_ip`    | `string?` | `null`  | Public IP for ICE candidates (NAT traversal)     |
| `stun_server`  | `string?` | `null`  | STUN server URL (optional, ICE-lite doesn't need it) |

**Example:**
```json
{
  "type": "webrtc",
  "bearer_token": "my-secret-token"
}
```

**Publishing from OBS:**
1. Settings → Stream → Service: WHIP
2. Server: `http://edge:8080/api/v1/flows/my-flow/whip`
3. Bearer Token: `my-secret-token`

### WHEP Input (`"type": "whep"`)

Pulls media from an external WHEP server. The edge acts as a WHEP client. The `webrtc` feature is enabled by default.

| Field          | Type      | Default | Description                                     |
|----------------|-----------|---------|-------------------------------------------------|
| `type`         | `"whep"`  | --      | Input type discriminator                        |
| `whep_url`     | `string`  | --      | WHEP endpoint URL to pull media from             |
| `bearer_token` | `string?` | `null`  | Bearer token for WHEP authentication             |
| `video_only`   | `bool`    | `false` | Receive only video (ignore audio)                |

---

## Output Types

Outputs are discriminated by the `"type"` field. All output types share `id` and `name` fields.

### SRT Output (`"type": "srt"`)

| Field         | Type                    | Default | Description                                     |
|---------------|-------------------------|---------|-------------------------------------------------|
| `type`             | `"srt"`                 | --      | Output type discriminator                       |
| `id`               | `string`                | --      | Unique output ID within this flow               |
| `name`             | `string`                | --      | Human-readable name                             |
| `mode`             | `SrtMode`               | --      | `"caller"`, `"listener"`, or `"rendezvous"`     |
| `local_addr`       | `string`                | --      | Local bind address                              |
| `remote_addr`      | `string?`               | `null`  | Remote address (required for caller/rendezvous) |
| `latency_ms`       | `u64`                   | `120`   | SRT latency in milliseconds (sets both receiver and peer/sender latency) |
| `recv_latency_ms`  | `u64?`                  | `null`  | Receiver-side latency override in ms            |
| `peer_latency_ms`  | `u64?`                  | `null`  | Peer/sender-side latency override in ms         |
| `peer_idle_timeout_secs` | `u64`              | `30`    | Connection dropped if no data received for this many seconds |
| `passphrase`       | `string?`               | `null`  | AES encryption passphrase (10-79 characters)    |
| `aes_key_len`      | `usize?`                | `null`  | AES key length: 16, 24, or 32                   |
| `crypto_mode`      | `string?`               | `null`  | Cipher mode: `"aes-ctr"` (default) or `"aes-gcm"` (authenticated encryption) |
| `max_rexmit_bw`    | `i64?`                  | `null`  | Maximum retransmission bandwidth in bytes/sec (`-1` = unlimited, `0` = disable, `> 0` = cap) |
| `stream_id`        | `string?`               | `null`  | SRT Stream ID for access control (max 512 chars). Caller: sent during handshake. Listener: if set, only matching callers accepted. |
| `packet_filter`    | `string?`               | `null`  | SRT FEC config string (see SRT Input for format details). |
| `redundancy`       | `SrtRedundancyConfig?`  | `null`  | SMPTE 2022-7 second leg configuration           |
| `program_number`   | `u16?`                  | `null`  | MPTS → SPTS program filter. `null` = passthrough (full MPTS); `Some(N)` = send only program N as a rewritten single-program TS. Must be `> 0`. See the **[MPTS → SPTS filtering](#mpts--spts-filtering)** section. |
| `transport_mode`   | `string?`               | `null`  | `"ts"` (default) sends raw MPEG-TS as the existing path does. `"audio_302m"` runs the per-output transcode + SMPTE 302M packetizer + TsMuxer pipeline and ships 7×188-byte LPCM-in-MPEG-TS chunks over SRT. Mutually exclusive with `packet_filter`, `program_number`, `redundancy`, and `delay`. The upstream input must be an audio essence (`st2110_30`, `st2110_31`, or `rtp_audio`). See [`audio-gateway.md`](audio-gateway.md). |
| `delay`            | `OutputDelay?`          | `null`  | Output delay for stream synchronization. Three modes: `{"mode":"fixed","ms":N}` adds a constant delay; `{"mode":"target_ms","ms":N}` sets a target end-to-end latency (dynamically adjusts); `{"mode":"target_frames","frames":N,"fallback_ms":M}` sets target in video frames (auto-detected fps, optional ms fallback). Incompatible with `transport_mode: "audio_302m"`. |
| `audio_encode`     | `AudioEncodeConfig?`    | `null`  | Optional audio re-encode (Phase B). `codec` ∈ {`aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`}. Rewrites the TS audio ES in place via `TsAudioReplacer`. Incompatible with `transport_mode: "audio_302m"`, 2022-7 redundancy, and SRT FEC (`packet_filter`). See `AudioEncodeConfig` block below. |
| `transcode`        | `TranscodeJson?`        | `null`  | Optional channel shuffle / sample-rate conversion applied to decoded PCM **before** `audio_encode` re-encodes. Ignored when `audio_encode` is unset. See [`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion) for full schema and resolution rules. |
| `video_encode`     | `VideoEncodeConfig?`    | `null`  | Optional video re-encode (feature-gated — `video-encoder-x264` / `-x265` / `-nvenc` / `-qsv`). See [`transcoding.md`](transcoding.md). |

### RTP Output (`"type": "rtp"`)

Sends RTP-wrapped MPEG-TS packets with RTP headers. Supports SMPTE 2022-1 FEC encoding and SMPTE 2022-7 hitless redundancy (dual-leg duplication).

| Field            | Type                          | Default | Description                                     |
|------------------|-------------------------------|---------|-------------------------------------------------|
| `type`           | `"rtp"`                       | --      | Output type discriminator                       |
| `id`             | `string`                      | --      | Unique output ID within this flow               |
| `name`           | `string`                      | --      | Human-readable name                             |
| `dest_addr`      | `string`                      | --      | Destination address (leg 1), e.g. `"192.168.1.100:5004"` |
| `bind_addr`      | `string?`                     | `null`  | Source bind address (default: `"0.0.0.0:0"`)    |
| `interface_addr` | `string?`                     | `null`  | Network interface IP for multicast send          |
| `fec_encode`     | `FecConfig?`                  | `null`  | SMPTE 2022-1 FEC encode parameters (sent to both legs) |
| `dscp`           | `u8`                          | `46`    | DSCP value for QoS marking (0-63, default: 46 = Expedited Forwarding) |
| `redundancy`     | `RtpOutputRedundancyConfig?`  | `null`  | SMPTE 2022-7 second leg configuration           |
| `program_number` | `u16?`                        | `null`  | MPTS → SPTS program filter. `null` = passthrough (full MPTS); `Some(N)` = send only program N as a rewritten single-program TS. Applied before FEC and 2022-7 redundancy, so receivers see the filtered SPTS. Must be `> 0`. |
| `delay`          | `OutputDelay?`                | `null`  | Output delay for stream synchronization. Three modes: `fixed` (constant delay), `target_ms` (target end-to-end latency), `target_frames` (target latency in video frames). See SRT Output for format details. |
| `audio_encode`   | `AudioEncodeConfig?`          | `null`  | Optional audio re-encode (Phase B). `codec` ∈ {`aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`}. Rewrites the TS audio ES in place via `TsAudioReplacer`. Incompatible with 2022-7 redundancy and SMPTE 2022-1 FEC encode. See `AudioEncodeConfig` block below. |
| `transcode`      | `TranscodeJson?`              | `null`  | Optional channel shuffle / sample-rate conversion applied to decoded PCM **before** `audio_encode` re-encodes. Ignored when `audio_encode` is unset. See [`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion). |
| `video_encode`   | `VideoEncodeConfig?`          | `null`  | Optional video re-encode (feature-gated — `video-encoder-x264` / `-x265` / `-nvenc` / `-qsv`). See [`transcoding.md`](transcoding.md). |

### UDP Output (`"type": "udp"`)

Sends raw MPEG-TS over UDP without RTP headers. Datagrams are TS-aligned (7×188 = 1316 bytes). If the input is RTP-wrapped, RTP headers are stripped automatically. Compatible with ffplay, VLC, and standard IP/TS multicast receivers.

| Field            | Type       | Default | Description                                     |
|------------------|-----------|---------|-------------------------------------------------|
| `type`           | `"udp"`    | --      | Output type discriminator                       |
| `id`             | `string`   | --      | Unique output ID within this flow               |
| `name`           | `string`   | --      | Human-readable name                             |
| `dest_addr`      | `string`   | --      | Destination address, e.g. `"192.168.1.100:5004"` |
| `bind_addr`      | `string?`  | `null`  | Source bind address (default: `"0.0.0.0:0"`)    |
| `interface_addr`  | `string?` | `null`  | Network interface IP for multicast send          |
| `dscp`           | `u8`       | `46`    | DSCP value for QoS marking (0-63, default: 46 = Expedited Forwarding) |
| `program_number` | `u16?`     | `null`  | MPTS → SPTS program filter. `null` = passthrough (full MPTS); `Some(N)` = send only program N as a rewritten single-program TS. Must be `> 0`. |
| `transport_mode` | `string?`  | `null`  | `"ts"` (default) sends raw MPEG-TS as the existing path does. `"audio_302m"` runs the per-output transcode + SMPTE 302M packetizer pipeline and emits 7×188-byte LPCM-in-MPEG-TS chunks as plain UDP datagrams (useful for legacy hardware decoders that consume raw MPEG-TS over UDP). Mutually exclusive with `program_number` and `delay`. See [`audio-gateway.md`](audio-gateway.md). |
| `delay`          | `OutputDelay?` | `null` | Output delay for stream synchronization. Three modes: `fixed` (constant delay), `target_ms` (target end-to-end latency), `target_frames` (target latency in video frames). Incompatible with `transport_mode: "audio_302m"`. See SRT Output for format details. |
| `audio_encode`   | `AudioEncodeConfig?` | `null` | Optional audio re-encode (Phase B). `codec` ∈ {`aac_lc`, `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`}. Rewrites the TS audio ES in place via `TsAudioReplacer`. Incompatible with `transport_mode: "audio_302m"`. See `AudioEncodeConfig` block below. |
| `transcode`      | `TranscodeJson?` | `null` | Optional channel shuffle / sample-rate conversion applied to decoded PCM **before** `audio_encode` re-encodes. Ignored when `audio_encode` is unset. See [`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion). |
| `video_encode`   | `VideoEncodeConfig?` | `null` | Optional video re-encode (feature-gated — `video-encoder-x264` / `-x265` / `-nvenc` / `-qsv`). See [`transcoding.md`](transcoding.md). |

### RTMP Output (`"type": "rtmp"`)

H.264/AAC only. Supports RTMPS (TLS) via `rtmps://` URLs. Audio is
passthrough by default; the optional `audio_encode` block runs the
input AAC through the Phase B ffmpeg-sidecar encoder so the operator
can normalise bitrate / sample rate / channel count or upgrade to
HE-AAC v1/v2 — see [`audio-gateway.md`](audio-gateway.md#the-audio_encode-block--compressed-audio-egress-rtmp--hls--webrtc).

| Field                      | Type    | Default | Description                                     |
|----------------------------|---------|---------|-------------------------------------------------|
| `type`                     | `"rtmp"`| --      | Output type discriminator                       |
| `id`                       | `string`| --      | Unique output ID within this flow               |
| `name`                     | `string`| --      | Human-readable name                             |
| `dest_url`                 | `string`| --      | RTMP destination URL, e.g. `"rtmp://live.twitch.tv/app"` |
| `stream_key`               | `string`| --      | Stream key for authentication                   |
| `reconnect_delay_secs`     | `u64`   | `5`     | Delay between reconnection attempts             |
| `max_reconnect_attempts`   | `u32?`  | `null`  | Maximum reconnect attempts (null = unlimited)   |
| `program_number`           | `u16?`  | `null`  | MPTS program selector. `null` = lock onto the lowest program_number in the PAT (deterministic default for MPTS inputs); `Some(N)` = extract elementary streams from program N only. RTMP is single-program by spec, so this only changes *which* program is published. Must be `> 0`. |
| `audio_encode`             | `AudioEncodeConfig?` | `null`  | Optional ffmpeg-sidecar audio encoder. Allowed `codec`: `aac_lc` (default), `he_aac_v1`, `he_aac_v2`. Requires ffmpeg in PATH at runtime; outputs without `audio_encode` set keep working without ffmpeg. The same-codec passthrough fast path applies when `codec == aac_lc` with no overrides on an AAC-LC source (disabled when `transcode` is set). See `AudioEncodeConfig` shape below. |
| `transcode`                | `TranscodeJson?`     | `null`  | Optional channel shuffle / sample-rate conversion applied to decoded PCM **before** `audio_encode` re-encodes. Setting `transcode` disables the same-codec passthrough fast path. Ignored when `audio_encode` is unset. See [`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion). |

### HLS Output (`"type": "hls"`)

Segment-based HTTP ingest. Supports HEVC/HDR content.

| Field                  | Type      | Default | Description                                     |
|------------------------|-----------|---------|-------------------------------------------------|
| `type`                 | `"hls"`   | --      | Output type discriminator                       |
| `id`                   | `string`  | --      | Unique output ID within this flow               |
| `name`                 | `string`  | --      | Human-readable name                             |
| `ingest_url`           | `string`  | --      | HLS ingest base URL for segment upload           |
| `segment_duration_secs`| `f64`     | `2.0`   | Target segment duration (range: 0.5-10.0)        |
| `auth_token`           | `string?` | `null`  | Bearer token for Authorization header            |
| `max_segments`         | `usize`   | `5`     | Maximum segments in rolling playlist             |
| `program_number`       | `u16?`    | `null`  | MPTS → SPTS program filter. `null` = each segment carries the full MPTS; `Some(N)` = each segment carries only program N as a rewritten single-program TS. Must be `> 0`. |
| `audio_encode`         | `AudioEncodeConfig?` | `null` | Optional per-segment ffmpeg remuxer. Allowed `codec`: `aac_lc` (default), `he_aac_v1`, `he_aac_v2`, `mp2`, `ac3`. Each segment is piped through `ffmpeg -i pipe:0 -c:v copy -c:a {codec} -f mpegts pipe:1`. Requires ffmpeg in PATH; the output refuses to start if ffmpeg is missing and emits a Critical `audio_encode` event. |
| `transcode`            | `TranscodeJson?`     | `null` | Optional channel shuffle / sample-rate conversion applied to decoded PCM **before** `audio_encode` re-encodes. Honoured only on the in-process remux path (`video-thumbnail` feature, default); the subprocess fallback logs a warning and ignores it. Ignored when `audio_encode` is unset. See [`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion). |

### WebRTC Output (`"type": "webrtc"`)

WebRTC output supporting two modes: WHIP client (push to external
endpoint) and WHEP server (serve viewers). The `webrtc` feature is
enabled by default. Video: H.264 only.

**Audio:** by default the WebRTC output is video-only when the source
carries AAC. Setting an `audio_encode` block (codec: `opus`) enables
the Phase B chain — input AAC is decoded in-process via the Phase A
`AacDecoder` (FDK AAC by default, supporting AAC-LC/HE-AAC v1/v2/multichannel),
encoded to Opus via the `AudioEncoder` (ffmpeg subprocess for Opus),
and written to the WebRTC audio MID via str0m. This is the marquee
"AAC contribution → Opus distribution" path. Requires `video_only=false`
so SDP negotiates an audio MID.

| Field          | Type      | Default        | Description                                     |
|----------------|-----------|----------------|-------------------------------------------------|
| `type`         | `"webrtc"`| --             | Output type discriminator                       |
| `id`           | `string`  | --             | Unique output ID within this flow               |
| `name`         | `string`  | --             | Human-readable name                             |
| `mode`         | `string`  | `"whip_client"`| `"whip_client"` (push to WHIP endpoint) or `"whep_server"` (serve viewers) |
| `whip_url`     | `string?` | `null`         | WHIP endpoint URL (required for `whip_client` mode) |
| `bearer_token` | `string?` | `null`         | Bearer token for authentication                  |
| `max_viewers`  | `u32?`    | `10`           | Max concurrent viewers (WHEP server mode only, 1-100) |
| `public_ip`    | `string?` | `null`         | Public IP for ICE candidates (NAT traversal)     |
| `video_only`   | `bool`    | `false`        | Send only video (audio omitted). Mutually exclusive with `audio_encode`. |
| `program_number` | `u16?`  | `null`         | MPTS program selector. `null` = lock onto the lowest program_number in the PAT (deterministic default); `Some(N)` = extract elementary streams from program N only. WebRTC is single-program by spec, so this only changes *which* program is sent. Must be `> 0`. |
| `audio_encode` | `AudioEncodeConfig?` | `null` | Optional ffmpeg-sidecar audio encoder. Only `codec: opus` is allowed for WebRTC. Validation rejects `audio_encode` + `video_only=true` (an audio MID must be negotiated in SDP). Requires ffmpeg in PATH; the encoder builds lazily on the first AAC frame after a viewer connects. |
| `transcode`    | `TranscodeJson?`     | `null` | Optional channel shuffle / sample-rate conversion applied to decoded PCM **before** the Opus encoder. `transcode.channels` overrides the Opus encoder's channel count; unset keeps the source channel count. Opus on the wire is always 48 kHz regardless of either block. Ignored when `audio_encode` is unset. See [`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion). |

#### `AudioEncodeConfig` block (Phase B)

Used by RTMP, HLS, WebRTC, and the TS-carrying SRT / UDP / RTP / RIST
outputs. Validation enforces a strict codec×output matrix at config
load time — see [`audio-gateway.md`](audio-gateway.md#the-audio_encode-block--compressed-audio-egress-rtmp--hls--webrtc).
Every output that accepts `audio_encode` also accepts an optional
companion `transcode` block (channel shuffle / sample-rate conversion
applied to the decoded PCM before re-encoding). Full reference:
[`transcoding.md`](transcoding.md#transcode--channel-shuffle--sample-rate-conversion).

| Field | Type | Default | Description |
|---|---|---|---|
| `codec` | `string` | -- | One of: `aac_lc`, `he_aac_v1`, `he_aac_v2`, `opus`, `mp2`, `ac3`. RTMP allows AAC family only; HLS allows AAC family + `mp2` + `ac3`; WebRTC allows `opus` only. |
| `bitrate_kbps` | `u32?` | per-codec default (AAC-LC=128, HE-AAC-v1=64, HE-AAC-v2=32, Opus=96, MP2=192, AC-3=192) | Output bitrate. Range 16..=512. |
| `sample_rate` | `u32?` | input sample rate | Output sample rate (Hz). Allowed: 8000, 16000, 22050, 24000, 32000, 44100, 48000. Opus is always carried at 48 kHz on the wire regardless of this field. |
| `channels` | `u8?` | input channel count | Output channel count, 1 or 2. |

### SMPTE ST 2110-30 / -31 Audio Output (`"type": "st2110_30"` / `"st2110_31"`)

RFC 3551 PCM audio over RTP/UDP, multicast or unicast. ST 2110-30 carries
big-endian L16 or L24 PCM samples; ST 2110-31 carries 24-bit AES3 sub-frames
(transparent to Dolby E and AES3 user/channel-status bits). Both share the
same struct shape — only the depacketizer label differs. Best-effort PTP
slave operation via the external `ptp4l` daemon's management Unix socket.

| Field            | Type                | Default | Description                                     |
|------------------|---------------------|---------|-------------------------------------------------|
| `type`           | `"st2110_30"` / `"st2110_31"` | -- | Output type discriminator                       |
| `id`             | `string`            | --      | Unique output ID within this flow               |
| `name`           | `string`            | --      | Human-readable name                             |
| `dest_addr`      | `string`            | --      | Primary (Red) destination address               |
| `bind_addr`      | `string?`           | `null`  | Source bind address (default `"0.0.0.0:0"`)     |
| `interface_addr` | `string?`           | `null`  | Multicast NIC IP for the primary leg            |
| `redundancy`     | `RedBlueBindConfig?`| `null`  | Optional SMPTE 2022-7 second leg (Blue network) |
| `sample_rate`    | `u32`               | `48000` | `48000` or `96000` Hz (wire format)             |
| `bit_depth`      | `u8`                | `24`    | `16` or `24` (-30); always `24` for -31         |
| `channels`       | `u8`                | `2`     | `1`, `2`, `4`, `8`, or `16`                     |
| `packet_time_us` | `u32`               | `1000`  | `125`, `250`, `333`, `500`, `1000`, `4000` µs   |
| `payload_type`   | `u8`                | `97`    | RTP dynamic payload type, 96..=127              |
| `clock_domain`   | `u8?`               | `null`  | PTP clock domain, 0..=127. Inherits from the parent flow when omitted. |
| `dscp`           | `u8`                | `46`    | DSCP value for QoS marking (0-63, default 46 / EF) |
| `ssrc`           | `u32?`              | `null`  | Optional fixed RTP SSRC; random when omitted    |
| `transcode`      | `TranscodeJson?`    | `null`  | Optional per-output PCM transcode block (sample rate, bit depth, channel routing). When omitted, byte-identical passthrough runs. See [`audio-gateway.md`](audio-gateway.md) for the full feature set, channel-routing presets, IS-08 hot reload, and stats. |

#### `transcode` block fields

| Field | Allowed values | Default | Description |
|---|---|---|---|
| `sample_rate` | 32000, 44100, 48000, 88200, 96000 | input rate | Output sample rate; `rubato` SRC if different from input |
| `bit_depth` | 16, 20, 24 | input depth | TPDF dither on down-conversion by default |
| `channels` | 1..=16 | input count | Must agree with `channel_map` length when set |
| `channel_map` | `[[in_ch, ...], ...]` per output channel | identity / auto-promote | Manual unity-gain matrix; mutually exclusive with `channel_map_preset` |
| `channel_map_preset` | named preset (see [`audio-gateway.md`](audio-gateway.md#channel-routing-presets)) | none | `mono_to_stereo`, `stereo_to_mono_3db`, `stereo_to_mono_6db`, `5_1_to_stereo_bs775`, `7_1_to_stereo_bs775`, `4ch_to_stereo_lt_rt` |
| `packet_time_us` | 125, 250, 333, 500, 1000, 4000 | 1000 | Output RTP packet time |
| `payload_type` | 96..=127 | 97 | Output RTP dynamic payload type |
| `src_quality` | `"high"`, `"fast"` | `"high"` | High = `rubato::SincFixedIn` (broadcast quality). Fast = `rubato::FastFixedIn` (lower latency, talkback). |
| `dither` | `"tpdf"`, `"none"` | `"tpdf"` | TPDF dither on bit-depth down-conversion |

### SMPTE ST 2110-40 ANC Output (`"type": "st2110_40"`)

RFC 8331 ancillary data: SCTE-104 ad markers, SMPTE 12M timecode,
CEA-608/708 captions, AFD, CDP. Same shape as the audio output minus
the audio essence fields and minus `transcode`.

| Field            | Type                | Default | Description                                     |
|------------------|---------------------|---------|-------------------------------------------------|
| `type`           | `"st2110_40"`       | --      | Output type discriminator                       |
| `id`             | `string`            | --      | Unique output ID within this flow               |
| `name`           | `string`            | --      | Human-readable name                             |
| `dest_addr`      | `string`            | --      | Primary destination address                     |
| `bind_addr`      | `string?`           | `null`  | Source bind address                             |
| `interface_addr` | `string?`           | `null`  | Multicast NIC IP                                |
| `redundancy`     | `RedBlueBindConfig?`| `null`  | Optional SMPTE 2022-7 second leg                |
| `payload_type`   | `u8`                | `100`   | RTP dynamic payload type, 96..=127              |
| `clock_domain`   | `u8?`               | `null`  | PTP clock domain, 0..=127                       |
| `dscp`           | `u8`                | `46`    | DSCP value for QoS marking                      |
| `ssrc`           | `u32?`              | `null`  | Optional fixed RTP SSRC                         |

### Generic `rtp_audio` Output (`"type": "rtp_audio"`)

Wire-identical to ST 2110-30 (RFC 3551 RTP + big-endian L16/L24 PCM)
but with relaxed validation: sample rates 32 / 44.1 / 48 / 88.2 / 96
kHz, no PTP requirement, no NMOS `clock_domain` advertising. Use this
for radio contribution feeds over the public internet, talkback between
studios that don't share a PTP fabric, and ffmpeg / OBS / GStreamer
interop where ST 2110-30's PTP assumption is overkill.

| Field            | Type                | Default | Description                                     |
|------------------|---------------------|---------|-------------------------------------------------|
| `type`           | `"rtp_audio"`       | --      | Output type discriminator                       |
| `id`             | `string`            | --      | Unique output ID within this flow               |
| `name`           | `string`            | --      | Human-readable name                             |
| `dest_addr`      | `string`            | --      | Primary destination address                     |
| `bind_addr`      | `string?`           | `null`  | Source bind address                             |
| `interface_addr` | `string?`           | `null`  | Multicast NIC IP                                |
| `redundancy`     | `RedBlueBindConfig?`| `null`  | Optional SMPTE 2022-7 second leg                |
| `sample_rate`    | `u32`               | --      | `32000`, `44100`, `48000`, `88200`, `96000` Hz  |
| `bit_depth`      | `u8`                | --      | `16` or `24`                                    |
| `channels`       | `u8`                | --      | `1`..=`16`                                      |
| `packet_time_us` | `u32`               | `1000`  | `125`, `250`, `333`, `500`, `1000`, `4000`, `20000` µs |
| `payload_type`   | `u8`                | `97`    | RTP dynamic payload type, 96..=127              |
| `dscp`           | `u8`                | `46`    | DSCP value for QoS marking                      |
| `ssrc`           | `u32?`              | `null`  | Optional fixed RTP SSRC                         |
| `transcode`      | `TranscodeJson?`    | `null`  | Same `transcode` block as ST 2110-30 outputs    |
| `transport_mode` | `string?`           | `null`  | `"rtp"` (default) sends RFC 3551 PCM/RTP. `"audio_302m"` wraps the PCM as SMPTE 302M LPCM in MPEG-TS and ships it inside RFC 2250 RTP/MP2T (payload type 33), useful for hardware decoders that expect MPEG-TS over RTP. |

`rtp_audio` also exists as an **input type** with the same field set
minus `dest_addr`/`dscp`/`ssrc`/`transport_mode`/`transcode`. The input
variant additionally accepts `allowed_sources` (RP 2129 C5 source IP
allow-list).

> **Audio gateway deep dive.** For worked configuration examples
> covering radio contribution, monitoring downmix, talkback, and
> third-party SRT decoder interop — including the four runnable
> ffmpeg / srt-live-transmit interop test scripts — see the dedicated
> [Audio Gateway Guide](audio-gateway.md).

---

## MPTS → SPTS filtering

All outputs — and the thumbnail generator — accept an optional `program_number` selector for down-selecting an MPTS (Multi-Program Transport Stream) input to a single program. There are two behaviours depending on whether the output natively carries a full MPTS:

**TS-native outputs** (`udp`, `rtp`, `srt`, `hls`):

- `program_number = null` (default) → **pass the full MPTS through unchanged**. This is the current behaviour for every existing config.
- `program_number = N` → the edge rewrites the PAT to a single-program form and drops every TS packet that doesn't belong to program N's PMT, ES, or PCR PIDs. The receiver sees a valid SPTS. FEC (2022-1) and hitless redundancy (2022-7) operate on the filtered bytes, so the protection applies to the SPTS the receiver will decode.

**Re-muxing outputs** (`rtmp`, `webrtc`):

These outputs extract elementary streams from the TS, so they can only carry one program by spec.

- `program_number = null` (default) → **lock onto the lowest `program_number` in the PAT**. This is deterministic across restarts and across sibling outputs of the same flow. For single-program inputs it has no visible effect.
- `program_number = N` → extract elementary streams from program N's PMT only.

**Thumbnail generator** (`thumbnail_program_number` on `FlowConfig`):

- `null` (default) → ffmpeg picks the first program it finds in the buffered TS.
- `Some(N)` → the buffered TS is pre-filtered so ffmpeg sees only program N. Useful when you want the manager UI preview to show a specific program.

**Validation:** `program_number = 0` is rejected at config load and on manager commands — program_number 0 is reserved for the NIT in the MPEG-TS specification and never identifies a real program.

**Disappearing programs:** If the selected program is not present in the PAT (either because it never existed or because a PAT version bump dropped it), the output emits nothing (TS-native) or produces no frames (re-muxing) until the program reappears. The filter automatically recovers on the next PAT that re-advertises the target.

**Per-output scope:** `program_number` is set on each output independently. You can run a single MPTS flow with one UDP output forwarding the full MPTS to an archive, a second UDP output filtering to program 1 for a viewer, and an RTMP output publishing program 2 — all three sharing the same broadcast channel inside the edge.

**Example — 2-program MPTS fanning out to three different destinations:**

```json
{
  "id": "mpts-flow",
  "name": "Dual-program feed",
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

---

## Shared Types

### SRT Mode (`SrtMode`)

| Value          | Description                                              |
|----------------|----------------------------------------------------------|
| `"caller"`     | Active mode: initiates connection to a remote listener   |
| `"listener"`   | Passive mode: waits for incoming connection              |
| `"rendezvous"` | Symmetric mode: both sides connect simultaneously (NAT traversal) |

`"caller"` and `"rendezvous"` require `remote_addr` to be set.

### SRT Redundancy Config (`SrtRedundancyConfig`)

Defines the second SRT leg for SMPTE 2022-7 hitless redundancy. The primary SRT settings in the parent define leg 1; this defines leg 2.

| Field              | Type      | Default | Description                              |
|--------------------|-----------|---------|------------------------------------------|
| `mode`             | `SrtMode` | --      | SRT mode for leg 2                       |
| `local_addr`       | `string`  | --      | Bind address for leg 2                   |
| `remote_addr`      | `string?` | `null`  | Remote address for leg 2                 |
| `latency_ms`       | `u64`     | `120`   | SRT latency for leg 2                    |
| `recv_latency_ms`  | `u64?`    | `null`  | Receiver-side latency override for leg 2 |
| `peer_latency_ms`  | `u64?`    | `null`  | Peer/sender-side latency override for leg 2 |
| `peer_idle_timeout_secs` | `u64` | `30`  | Peer idle timeout for leg 2              |
| `passphrase`       | `string?` | `null`  | AES passphrase for leg 2                 |
| `aes_key_len`      | `usize?`  | `null`  | AES key length for leg 2                 |
| `crypto_mode`      | `string?` | `null`  | Cipher mode for leg 2: `"aes-ctr"` or `"aes-gcm"` |
| `max_rexmit_bw`    | `i64?`    | `null`  | Max retransmission bandwidth for leg 2   |
| `stream_id`        | `string?` | `null`  | SRT Stream ID for leg 2                 |

### RTP Redundancy Config (`RtpRedundancyConfig`)

Defines the second RTP leg for SMPTE 2022-7 hitless redundancy on input. The primary `bind_addr` in the parent RTP input defines leg 1; this defines leg 2. Ingress filters, FEC decode, and rate limiting from the parent apply to both legs.

| Field            | Type       | Default | Description                              |
|------------------|-----------|---------|------------------------------------------|
| `bind_addr`      | `string`  | --      | Bind address for leg 2                   |
| `interface_addr` | `string?` | `null`  | Network interface IP for multicast join   |

### RTP Output Redundancy Config (`RtpOutputRedundancyConfig`)

Defines the second RTP leg for SMPTE 2022-7 hitless redundancy on output. The primary `dest_addr` in the parent RTP output defines leg 1; this defines leg 2. FEC encoding from the parent applies to both legs.

| Field            | Type       | Default       | Description                              |
|------------------|-----------|---------------|------------------------------------------|
| `dest_addr`      | `string`  | --            | Destination address for leg 2            |
| `bind_addr`      | `string?` | `null`        | Source bind address for leg 2            |
| `interface_addr` | `string?` | `null`        | Network interface IP for multicast send   |
| `dscp`           | `u8?`     | parent's dscp | DSCP value for leg 2 (defaults to parent) |

### FEC Config (`FecConfig`)

SMPTE 2022-1 Forward Error Correction parameters.

| Field     | Type | Range | Description                              |
|-----------|------|-------|------------------------------------------|
| `columns` | `u8` | 1-20  | L parameter (columns in FEC matrix)      |
| `rows`    | `u8` | 4-20  | D parameter (rows in FEC matrix)         |

---

## Example Configurations

### Simple SRT to RTP Bridge

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
  "flows": [
    {
      "id": "srt-to-rtp",
      "name": "SRT to RTP Bridge",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "rtp-out",
          "name": "RTP Output",
          "dest_addr": "192.168.1.50:5004"
        }
      ]
    }
  ]
}
```

### SRT with FEC (Forward Error Correction)

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "srt-fec-flow",
      "name": "SRT with FEC protection",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200,
        "packet_filter": "fec,cols:10,rows:5,layout:staircase,arq:onreq"
      },
      "outputs": [
        {
          "type": "srt",
          "id": "srt-fec-out",
          "name": "SRT FEC Output",
          "mode": "caller",
          "local_addr": "0.0.0.0:0",
          "remote_addr": "192.168.1.100:9001",
          "latency_ms": 200,
          "packet_filter": "fec,cols:10,rows:5,layout:staircase,arq:onreq"
        }
      ]
    }
  ]
}
```

> **Note:** SRT FEC is different from SMPTE 2022-1 FEC used with RTP. SRT FEC operates at the SRT protocol level and is negotiated during the SRT handshake. The `packet_filter` config must match between caller and listener. Row-only FEC (`rows:1`) adds ~10% bandwidth overhead; 2D FEC (e.g., `rows:5`) adds ~30% but can recover from burst losses across both dimensions.

### SRT with 2022-7 Redundancy and FEC Output

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "redundant-with-fec",
      "name": "Redundant SRT to FEC-protected RTP",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 500,
        "redundancy": {
          "mode": "listener",
          "local_addr": "0.0.0.0:9001",
          "latency_ms": 500
        }
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "rtp-fec",
          "name": "RTP with FEC",
          "dest_addr": "192.168.1.50:5004",
          "fec_encode": { "columns": 10, "rows": 10 }
        }
      ]
    }
  ]
}
```

### RTP with 2022-7 Redundancy (Dual Multicast)

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "redundant-rtp",
      "name": "RTP 2022-7 Multicast to Multicast",
      "enabled": true,
      "input": {
        "type": "rtp",
        "bind_addr": "239.1.1.1:5000",
        "interface_addr": "192.168.1.10",
        "redundancy": {
          "bind_addr": "239.1.1.2:5000",
          "interface_addr": "192.168.1.10"
        },
        "fec_decode": { "columns": 10, "rows": 10 }
      },
      "outputs": [
        {
          "type": "rtp",
          "id": "rtp-out",
          "name": "Redundant RTP Output",
          "dest_addr": "239.2.1.1:5004",
          "interface_addr": "192.168.1.10",
          "redundancy": {
            "dest_addr": "239.2.1.2:5004",
            "interface_addr": "192.168.1.10"
          },
          "fec_encode": { "columns": 10, "rows": 10 }
        }
      ]
    }
  ]
}
```

### Multi-Output Fan-out with RTMP and HLS

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "flows": [
    {
      "id": "fanout",
      "name": "SRT to RTMP + HLS + RTP",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200
      },
      "outputs": [
        {
          "type": "rtmp",
          "id": "twitch",
          "name": "Twitch",
          "dest_url": "rtmp://live.twitch.tv/app",
          "stream_key": "live_xxxxx",
          "reconnect_delay_secs": 5
        },
        {
          "type": "hls",
          "id": "youtube-hls",
          "name": "YouTube HLS",
          "ingest_url": "https://a.upload.youtube.com/http_upload_hls",
          "segment_duration_secs": 2.0,
          "auth_token": "your-youtube-token"
        },
        {
          "type": "rtp",
          "id": "local-rtp",
          "name": "Local RTP",
          "dest_addr": "192.168.1.50:5004"
        }
      ]
    }
  ]
}
```

### Manager-Connected Node

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "monitor": { "listen_addr": "0.0.0.0", "listen_port": 9090 },
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "registration_token": "token-from-manager-dashboard"
  },
  "flows": [
    {
      "id": "main-feed",
      "name": "Main Feed",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200,
        "passphrase": "my-srt-encryption-key",
        "crypto_mode": "aes-gcm"
      },
      "outputs": [
        {
          "type": "srt",
          "id": "srt-relay",
          "name": "SRT Relay",
          "mode": "caller",
          "local_addr": "0.0.0.0:0",
          "remote_addr": "10.0.1.100:9000",
          "latency_ms": 200
        }
      ]
    }
  ]
}
```

### Manager-Connected Node with Relay Tunnel

```json
{
  "version": 1,
  "server": { "listen_addr": "0.0.0.0", "listen_port": 8080 },
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "cert_fingerprint": "ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89"
  },
  "flows": [
    {
      "id": "tunnel-feed",
      "name": "Feed via Tunnel",
      "enabled": true,
      "input": {
        "type": "srt",
        "mode": "listener",
        "local_addr": "0.0.0.0:9000",
        "latency_ms": 200
      },
      "outputs": [
        {
          "type": "udp",
          "id": "tunnel-out",
          "name": "To Tunnel Egress",
          "dest_addr": "127.0.0.1:9100"
        }
      ]
    }
  ],
  "tunnels": [
    {
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "name": "Stadium to Studio",
      "enabled": true,
      "protocol": "udp",
      "mode": "relay",
      "direction": "egress",
      "local_addr": "0.0.0.0:9100",
      "relay_addrs": [
        "relay-primary.example.com:4433",
        "relay-backup.example.com:4433"
      ],
      "tunnel_encryption_key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "tunnel_bind_secret": "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
    }
  ]
}
```
