# Configuration Reference

bilbycast-edge is configured via a JSON file (default: `./config.json`). All changes — whether from the REST API, manager commands, or the setup wizard — are persisted to this file immediately and survive reboots.

---

## Config Persistence

The edge node's config file is the **source of truth** for its configuration. The manager does not store node configs in its database — it only sends commands, and the edge is responsible for persisting its own state.

### When config is saved to disk

Config is written to disk immediately after any of these events:

- **First startup** — auto-generated `node_id` is saved
- **Manager registration** — `node_id` and `node_secret` credentials are saved after successful registration
- **REST API flow operations** — creating, updating, or deleting flows via the local API
- **Manager `update_config` command** — when a user updates the config through the manager UI
- **Setup wizard** — initial provisioning via the browser-based setup at `/setup`

### Atomic writes

Config saves use an atomic two-phase write: the JSON is first written to a temporary file (`.json.tmp`), then renamed to the actual config path. This prevents partial or corrupted config files if the node crashes or loses power mid-write.

### Manager config update behavior

Both `update_config` (full config replacement) and `update_flow` (single flow update) use **diff-based** logic to minimize disruption to running flows. Only changed components are restarted; unchanged flows, outputs, and tunnels continue running uninterrupted.

When the manager sends an `update_config` or `update_flow` command:

1. The manager sends the full config (not a diff) — the edge computes the diff internally
2. The edge **validates** the new config before applying it
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
6. The in-memory config is updated and **saved to disk**
7. A `command_ack` is sent back to the manager

**Fields replaced** by UpdateConfig: `flows`, `tunnels`, `server`, `monitor`

**Fields preserved** (not overwritten): `version`, `node_id`, `device_name`, `setup_enabled`, `manager` (connection credentials)

### Reboot behavior

On startup, the edge loads its config from the JSON file on disk. Since all changes are persisted immediately, the node will always start with the latest config — including any changes made remotely through the manager.

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
  "flows": []
}
```

## Node Identity & Setup

| Field     | Type      | Default | Description |
|-----------|-----------|---------|-------------|
| `node_id` | `string?` | Auto-generated | Persistent UUID v4 identifying this edge node. Auto-generated on first startup and saved to config. Used as the NMOS IS-04 Node ID. All NMOS resource UUIDs are derived from this value (UUID v5), so they remain stable across restarts. |
| `device_name` | `string?` | `null` | Optional human-readable label for this edge node (e.g. "Studio-A Encoder"). Max 256 characters. |
| `setup_enabled` | `bool` | `true` | When true, the browser-based setup wizard is accessible at `/setup`. Set to false to disable it after provisioning. |

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

Enables HTTPS on the API server. Requires building with `--features tls`.

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

OAuth 2.0 client credentials authentication with JWT (HMAC-SHA256). When absent or `enabled: false`, all API endpoints are unauthenticated.

| Field                  | Type           | Default | Description                                    |
|------------------------|----------------|---------|------------------------------------------------|
| `enabled`              | `bool`         | --      | Master switch for authentication               |
| `jwt_secret`           | `string`       | --      | HMAC-SHA256 secret for signing JWTs            |
| `token_lifetime_secs`  | `u64`          | `3600`  | Token validity period in seconds               |
| `clients`              | `AuthClient[]` | --      | List of registered OAuth clients               |
| `public_metrics`       | `bool`         | `true`  | Allow unauthenticated access to `/metrics` and `/health` |

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
| `accept_self_signed_cert` | `bool` | `false` | Accept self-signed TLS certificates from the manager. **Dev/testing only** — disables cert validation. |
| `registration_token` | `string?` | `null`  | One-time registration token (first connection only) |
| `node_id`            | `string?` | `null`  | Assigned node ID (set automatically after registration) |
| `node_secret`        | `string?` | `null`  | Assigned node secret (set automatically after registration) |

On first connection, provide `registration_token`. After successful registration, the manager returns `node_id` and `node_secret`, which are written back to the config file automatically. The `registration_token` is cleared. Subsequent connections use `node_id` and `node_secret`.

```json
{
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "registration_token": "abc123-one-time-token"
  }
}
```

After registration, the file is updated to:

```json
{
  "manager": {
    "enabled": true,
    "url": "wss://manager.example.com:8443/ws/node",
    "node_id": "assigned-uuid",
    "node_secret": "assigned-secret"
  }
}
```

---

## Flows Section

An array of flow definitions. Each flow has one input and one or more outputs.

| Field     | Type             | Default | Description                              |
|-----------|------------------|---------|------------------------------------------|
| `id`              | `string`         | --      | Unique flow identifier                   |
| `name`            | `string`         | --      | Human-readable name                      |
| `enabled`         | `bool`           | `true`  | Auto-start when the application starts   |
| `media_analysis`  | `bool`           | `true`  | Enable media content analysis (codec, resolution, frame rate detection). Set to `false` to save CPU on resource-constrained devices. |
| `input`           | `InputConfig`    | --      | Single input source                      |
| `outputs`         | `OutputConfig[]` | --      | One or more output destinations          |

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

Accepts incoming WebRTC contributions from publishers (OBS, browsers) via the WHIP protocol (RFC 9725). The WHIP endpoint is auto-generated at `/api/v1/flows/{flow_id}/whip`. Requires the `webrtc` cargo feature.

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

Pulls media from an external WHEP server. The edge acts as a WHEP client. Requires the `webrtc` cargo feature.

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

### RTMP Output (`"type": "rtmp"`)

H.264/AAC only. Supports RTMPS (TLS) via `rtmps://` URLs.

| Field                      | Type    | Default | Description                                     |
|----------------------------|---------|---------|-------------------------------------------------|
| `type`                     | `"rtmp"`| --      | Output type discriminator                       |
| `id`                       | `string`| --      | Unique output ID within this flow               |
| `name`                     | `string`| --      | Human-readable name                             |
| `dest_url`                 | `string`| --      | RTMP destination URL, e.g. `"rtmp://live.twitch.tv/app"` |
| `stream_key`               | `string`| --      | Stream key for authentication                   |
| `reconnect_delay_secs`     | `u64`   | `5`     | Delay between reconnection attempts             |
| `max_reconnect_attempts`   | `u32?`  | `null`  | Maximum reconnect attempts (null = unlimited)   |

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

### WebRTC Output (`"type": "webrtc"`)

WebRTC output supporting two modes: WHIP client (push to external endpoint) and WHEP server (serve viewers). Requires the `webrtc` cargo feature. Video: H.264 only; audio: Opus passthrough only (AAC sources fall back to video-only automatically).

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
| `video_only`   | `bool`    | `false`        | Send only video (audio omitted)                  |

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
